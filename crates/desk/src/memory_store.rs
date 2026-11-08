//! Session memory in Postgres: the exchanges harvested from each record,
//! written in one transaction with the ledger's rows and both of the
//! record's offsets; what is never to be read again; the person's switches;
//! and the keyword search over what is kept.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::engine::Engine;
use crate::ledger_store::{Cursor, Kept};
use crate::memory::passage::{collection_of, Purge};
use crate::memory::record::{Asker, Exchange, Step};
use crate::store::DeskStore;

/// Whether forgetting sessions forgets what Desk remembers of them: an
/// explicit forget or removal does; clearing finished sessions away only
/// tidies their cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    Keep,
    Forget,
}

/// One step of a record, as [`DeskStore::record_write`] keeps it.
pub struct RecordWrite<'a> {
    pub session: Uuid,
    pub engine: Engine,
    pub source: &'a str,
    pub cursor: &'a Cursor,
    pub rows: &'a [Kept],
    /// Exchanges the step closed, in order, and the one still open.
    pub closed: &'a [Exchange],
    pub open: Option<&'a Exchange>,
    /// The session's project and folder, stamped on its exchanges.
    pub project: &'a str,
    pub folder: &'a str,
    /// What the session is called, and its conversation's id, kept with its
    /// memory.
    pub name: &'a str,
    pub cli_session_id: Option<&'a str>,
}

/// What the harvest checks a session against, from the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// `sessions.memory` is on; a session the registry no longer has isn't.
    pub memory_on: bool,
    /// The refs of its engine tombstoned among those asked about.
    pub forgotten: HashSet<String>,
    /// `memory.paused`.
    pub paused: bool,
    /// Its group's `memory.group.<id>` says not to harvest.
    pub group_off: bool,
}

/// A search of session memory, as the person asks it.
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    pub q: String,
    /// Only one group's (`Some(None)`: only ungrouped sessions').
    pub group: Option<Option<String>>,
    pub session: Option<Uuid>,
    /// Only these kinds of exchanges and notes; all when none.
    pub kinds: Option<Vec<crate::memory::passage::PassageKind>>,
    pub limit: i64,
}

/// An exchange or a note a search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHit {
    pub session: Uuid,
    pub group: Option<String>,
    pub project: String,
    pub at: DateTime<Utc>,
    pub asker: Asker,
    /// A note's is empty.
    pub ask: String,
    /// A note's text.
    pub answer: String,
    pub compaction: bool,
    /// `exchange`, `compaction`, `decision`, `fact` or `todo`.
    pub kind: &'static str,
    /// A note Desk's model wrote, which may be wrong.
    pub noted_by_desk: bool,
    /// A note the person confirmed or wrote.
    pub confirmed: bool,
    /// A note another replaced.
    pub superseded: bool,
}

fn exchange_kind() -> &'static str {
    "exchange"
}

/// An exchange as a session's memory lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptExchange {
    pub seq: i32,
    pub at: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
    pub asker: Asker,
    pub ask: String,
    pub answer: String,
    pub steps: Vec<Step>,
    pub files: Vec<String>,
    pub commands: Vec<String>,
    pub errors: Vec<String>,
    pub compaction: bool,
    pub redacted: i32,
}

/// What a session's memory holds in all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCounts {
    pub exchanges: i64,
    /// Exchanges still being written.
    pub open: i64,
    pub compactions: i64,
    /// Secrets hidden in what was kept.
    pub secrets_hidden: i64,
    /// Older exchanges past the session's cap: kept, but found by keyword
    /// only.
    pub past_cap: i64,
}

/// What session memory holds of one group's sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupTotals {
    /// None: ungrouped sessions'.
    pub group: Option<String>,
    pub sessions: i64,
    pub exchanges: i64,
    pub secrets_hidden: i64,
    /// Notes still held.
    pub notes: i64,
}

fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The setting key of a group's memory switches.
pub fn group_key(group: &str) -> String {
    format!("memory.group.{group}")
}

/// The most exchanges of one session the search index holds: older ones
/// stay in Postgres, found by keyword.
pub const SESSION_CAP: usize = 20_000;

/// What a sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// Exchanges past their keeping, deleted.
    pub gone: usize,
    /// Exchanges past their session's cap, taken out of the index.
    pub capped: usize,
    /// Index deletions queued.
    pub purges: usize,
}

/// The setting key of the pause.
pub const PAUSED: &str = "memory.paused";

impl DeskStore {
    /// Keeps one step of a record in one transaction: the ledger's rows, the
    /// exchanges it closed, the one still open, and both of the record's
    /// offsets, so a restart between read and write loses nothing and keeps
    /// nothing twice. An exchange's key is kept by the first session that
    /// read it (a fork's copied history stays its source's); that session's
    /// later reading of it (an open exchange closing) completes it. A
    /// session forgotten meanwhile gets no exchanges: its row is held while
    /// they are written, and forgetting it waits. How many ledger rows were
    /// added or completed.
    pub async fn record_write(&self, w: &RecordWrite<'_>) -> Result<u64> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let changed =
            crate::ledger_store::ledger_write_in(&tx, w.session, w.engine, w.rows).await?;
        let held = tx
            .query_opt(
                "SELECT 1 FROM sessions WHERE id = $1 FOR SHARE",
                &[&w.session],
            )
            .await?
            .is_some();
        if held && (!w.closed.is_empty() || w.open.is_some()) {
            tx.execute(
                "INSERT INTO memory_sessions (session, engine, name, cli_session_id, sources, \
                   updated_at) VALUES ($1, $2, $3, $4, ARRAY[$5], now()) \
                 ON CONFLICT (session) DO UPDATE SET name = $3, \
                   cli_session_id = COALESCE($4, memory_sessions.cli_session_id), \
                   sources = CASE WHEN $5 = ANY(memory_sessions.sources) \
                     THEN memory_sessions.sources ELSE memory_sessions.sources || $5 END, \
                   updated_at = now()",
                &[
                    &w.session,
                    &w.engine.as_str(),
                    &w.name,
                    &w.cli_session_id,
                    &w.source,
                ],
            )
            .await
            .context("naming a session's memory")?;
        }
        if held {
            // What this step rewrites is locked in id order, as every write
            // of many exchanges locks them.
            let keys: Vec<&str> = w
                .closed
                .iter()
                .chain(w.open)
                .map(|x| x.key.as_str())
                .collect();
            tx.execute(
                "SELECT 1 FROM memory_exchanges WHERE engine = $1 AND key = ANY($2) \
                 ORDER BY id FOR UPDATE",
                &[&w.engine.as_str(), &keys],
            )
            .await?;
            let base: i32 = tx
                .query_one(
                    "SELECT COALESCE(MAX(seq), 0) FROM memory_exchanges WHERE session = $1",
                    &[&w.session],
                )
                .await?
                .try_get(0)?;
            // Each exchange is the group's its session was in when it began,
            // so one read after a move stays where it was said.
            let insert = format!(
                "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, \
                   folder, seq, at, ended, asker, ask, answer, steps, files, commands, \
                   errors, compaction, redacted, flagged) \
                 SELECT gen_random_uuid(), $1, $2, $3, {group}, $4, $5, $6, $7, $8, $9, \
                   $10, $11, $12, $13, $14, $15, $16, $17, $18 \
                 WHERE NOT EXISTS (SELECT 1 FROM memory_forgotten f \
                   WHERE f.engine = $1 AND f.ref = 'key:' || $2) \
                 ON CONFLICT (engine, key) DO UPDATE SET ended = EXCLUDED.ended, \
                   ask = EXCLUDED.ask, answer = EXCLUDED.answer, steps = EXCLUDED.steps, \
                   files = EXCLUDED.files, commands = EXCLUDED.commands, \
                   errors = EXCLUDED.errors, redacted = EXCLUDED.redacted, \
                   flagged = EXCLUDED.flagged, \
                   indexed_at = CASE WHEN (memory_exchanges.ask, memory_exchanges.answer, \
                     memory_exchanges.steps) IS DISTINCT FROM (EXCLUDED.ask, EXCLUDED.answer, \
                     EXCLUDED.steps) THEN NULL ELSE memory_exchanges.indexed_at END, \
                   index_tries = CASE WHEN (memory_exchanges.ask, memory_exchanges.answer, \
                     memory_exchanges.steps) IS DISTINCT FROM (EXCLUDED.ask, EXCLUDED.answer, \
                     EXCLUDED.steps) THEN 0 ELSE memory_exchanges.index_tries END \
                 WHERE memory_exchanges.session = EXCLUDED.session",
                group = crate::store::group_at_sql("$3::uuid", "$7::timestamptz"),
            );
            for (i, x) in w.closed.iter().chain(w.open).enumerate() {
                let seq = base + i32::try_from(i).unwrap_or(i32::MAX - base) + 1;
                let steps = serde_json::to_value(&x.steps)?;
                let files: Vec<&str> = x.files.iter().map(String::as_str).collect();
                let flagged: Vec<&str> = x.flagged.iter().map(String::as_str).collect();
                tx.execute(
                    insert.as_str(),
                    &[
                        &w.engine.as_str(),
                        &x.key,
                        &w.session,
                        &w.project,
                        &w.folder,
                        &seq,
                        &x.at,
                        &x.ended,
                        &x.asker.as_str(),
                        &x.ask,
                        &x.answer,
                        &steps,
                        &files,
                        &x.commands,
                        &x.errors,
                        &x.compaction,
                        &i32::try_from(x.redacted).unwrap_or(i32::MAX),
                        &flagged,
                    ],
                )
                .await
                .context("keeping an exchange")?;
            }
            // Exchanges that closed are to be read for notes: due as the
            // turn ends, at most every ten minutes.
            if !w.closed.is_empty() {
                tx.execute(&crate::memory_derive_store::arm_sql(), &[&w.session])
                    .await
                    .context("queueing a session to note decisions from")?;
            }
        }
        // A session forgotten meanwhile keeps no cursor: its harvest's state
        // holds what it said.
        if held {
            let c = w.cursor;
            tx.execute(
                "INSERT INTO ledger_cursors (session, source, read_to, seen, harvested_to, harvest, \
                   updated_at) VALUES ($1, $2, $3, $4, $5, $6, now()) \
                 ON CONFLICT (session, source) DO UPDATE SET read_to = $3, seen = $4, \
                   harvested_to = $5, harvest = $6, updated_at = now()",
                &[
                    &w.session,
                    &w.source,
                    &count(c.read_to),
                    &c.seen,
                    &count(c.harvested_to),
                    &c.harvest,
                ],
            )
            .await
            .context("moving a record's cursor")?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Forgets what Desk remembers of `session`, whether or not the registry
    /// still has it: as forgetting the session does, with the same
    /// tombstones. Whether there was anything of it to forget.
    pub async fn forget_memory(&self, session: Uuid) -> Result<bool> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let known = tx
            .query_opt(
                "SELECT 1 FROM sessions WHERE id = $1 \
                 UNION ALL SELECT 1 FROM memory_sessions WHERE session = $1 \
                 UNION ALL SELECT 1 FROM memory_exchanges WHERE session = $1 LIMIT 1",
                &[&session],
            )
            .await?
            .is_some();
        forget_memory_in(&tx, &[session]).await?;
        tx.commit().await?;
        Ok(known)
    }

    /// What a session whose memory is kept was called, and its engine.
    pub async fn memory_names(&self, ids: &[Uuid]) -> Result<Vec<(Uuid, String, Engine)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT session, name, engine FROM memory_sessions WHERE session = ANY($1)",
                &[&ids],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?, Engine::parse(r.try_get(2)?)?)))
            .collect()
    }

    /// What the harvest of `session` is checked against: its switch, which
    /// of `refs` (its conversation's id, its records' paths) are tombstoned
    /// for `engine`, the pause, and `group`'s switch.
    pub async fn harvest_facts(
        &self,
        session: Uuid,
        engine: Engine,
        refs: &[String],
        group: Option<&str>,
    ) -> Result<Facts> {
        let client = self.pool().get().await?;
        let memory_on = client
            .query_opt("SELECT memory FROM sessions WHERE id = $1", &[&session])
            .await?
            .map(|r| r.try_get::<_, String>(0))
            .transpose()?
            .is_some_and(|m| m == "on");
        let forgotten = client
            .query(
                "SELECT ref FROM memory_forgotten WHERE engine = $1 AND ref = ANY($2)",
                &[&engine.as_str(), &refs],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<Result<HashSet<String>, _>>()?;
        let keys: Vec<String> = [Some(PAUSED.to_string()), group.map(group_key)]
            .into_iter()
            .flatten()
            .collect();
        let mut facts = Facts {
            memory_on,
            forgotten,
            ..Facts::default()
        };
        for row in client
            .query(
                "SELECT key, value FROM settings WHERE key = ANY($1)",
                &[&keys],
            )
            .await?
        {
            let (key, value): (String, Value) = (row.try_get(0)?, row.try_get(1)?);
            if key == PAUSED {
                facts.paused = value.as_bool().unwrap_or(false);
            } else {
                facts.group_off = value.get("harvest").and_then(Value::as_bool) == Some(false);
            }
        }
        Ok(facts)
    }

    /// The person's setting `key`.
    pub async fn setting(&self, key: &str) -> Result<Option<Value>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt("SELECT value FROM settings WHERE key = $1", &[&key])
            .await?
            .map(|r| r.try_get(0))
            .transpose()?)
    }

    pub async fn put_setting(&self, key: &str, value: &Value) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, now()) \
                 ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()",
                &[&key, value],
            )
            .await?;
        Ok(())
    }

    /// Merges `fields` into the object setting `key`, in one statement: two
    /// changes at once each keep the other's fields. What it now is.
    pub async fn merge_setting(
        &self,
        key: &str,
        fields: &serde_json::Map<String, Value>,
    ) -> Result<Value> {
        let client = self.pool().get().await?;
        let fields = Value::Object(fields.clone());
        Ok(client
            .query_one(
                "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, now()) \
                 ON CONFLICT (key) DO UPDATE SET value = CASE \
                   WHEN jsonb_typeof(settings.value) = 'object' THEN settings.value || $2 \
                   ELSE $2 END, updated_at = now() \
                 RETURNING value",
                &[&key, &fields],
            )
            .await?
            .try_get(0)?)
    }

    /// Sets a session's own memory switch; false when there is no such
    /// session.
    pub async fn set_session_memory(&self, session: Uuid, on: bool) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE sessions SET memory = $2 WHERE id = $1",
                &[&session, &if on { "on" } else { "off" }],
            )
            .await?
            > 0)
    }

    /// A session's own memory switch: on unless the person turned it off;
    /// none when there is no such session.
    pub async fn session_memory_on(&self, session: Uuid) -> Result<Option<bool>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt("SELECT memory FROM sessions WHERE id = $1", &[&session])
            .await?
            .map(|r| r.try_get::<_, String>(0))
            .transpose()?
            .map(|m| m != "off"))
    }

    /// When Desk last kept something of `session`'s conversation.
    pub async fn memory_last_kept(&self, session: Uuid) -> Result<Option<DateTime<Utc>>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT updated_at FROM memory_sessions WHERE session = $1",
                &[&session],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()?)
    }

    /// Every `memory.*` setting the person has made, by key.
    pub async fn memory_settings(&self) -> Result<HashMap<String, Value>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT key, value FROM settings WHERE key LIKE 'memory.%'",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// What session memory holds, by group (none: ungrouped).
    pub async fn memory_totals(&self) -> Result<Vec<GroupTotals>> {
        let client = self.pool().get().await?;
        let mut by: HashMap<Option<String>, GroupTotals> = HashMap::new();
        for r in client
            .query(
                "SELECT group_id, COUNT(DISTINCT session), COUNT(*), \
                   COALESCE(SUM(redacted), 0)::int8 \
                 FROM memory_exchanges GROUP BY group_id",
                &[],
            )
            .await?
        {
            let group: Option<String> = r.try_get(0)?;
            by.insert(
                group.clone(),
                GroupTotals {
                    group,
                    sessions: r.try_get(1)?,
                    exchanges: r.try_get(2)?,
                    secrets_hidden: r.try_get(3)?,
                    notes: 0,
                },
            );
        }
        for r in client
            .query(
                "SELECT group_id, COUNT(*) FROM memory_items \
                 WHERE valid_to IS NULL GROUP BY group_id",
                &[],
            )
            .await?
        {
            let group: Option<String> = r.try_get(0)?;
            by.entry(group.clone())
                .or_insert_with(|| GroupTotals {
                    group,
                    ..GroupTotals::default()
                })
                .notes = r.try_get(1)?;
        }
        let mut totals: Vec<GroupTotals> = by.into_values().collect();
        totals.sort_by(|a, b| a.group.cmp(&b.group));
        Ok(totals)
    }

    /// Deletes conversations that ended more than `retain_days` ago, and
    /// reads older than 90 days, and takes each session's exchanges past
    /// the newest [`SESSION_CAP`] out of the search index. What the index
    /// held of them is queued to be deleted from it in the same
    /// transaction.
    pub async fn sweep_memory(&self, retain_days: u32) -> Result<Swept> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let days = i32::try_from(retain_days).unwrap_or(i32::MAX);
        let cap = i64::try_from(SESSION_CAP).unwrap_or(i64::MAX);
        // What goes, and what is past its session's cap, are found first,
        // with nothing locked; then only those rows are locked, all in one
        // id order, as every write of many exchanges locks them. One never
        // ended (its session stopped mid-turn) goes by when it began.
        let old: Vec<Uuid> = tx
            .query(
                "SELECT id FROM memory_exchanges \
                 WHERE COALESCE(ended, at) < now() - make_interval(days => $1)",
                &[&days],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<Result<_, _>>()?;
        let over: Vec<Uuid> = tx
            .query(
                "SELECT id FROM ( \
                   SELECT id, row_number() OVER (PARTITION BY session ORDER BY seq DESC) AS newer \
                   FROM memory_exchanges WHERE NOT past_cap AND session IN ( \
                     SELECT session FROM memory_exchanges WHERE NOT past_cap \
                     GROUP BY session HAVING COUNT(*) > $1)) x \
                 WHERE newer > $1",
                &[&cap],
            )
            .await
            .context("finding sessions' exchanges past their cap")?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<Result<_, _>>()?;
        let both: Vec<Uuid> = old.iter().chain(over.iter()).copied().collect();
        tx.execute(crate::memory_index_store::LOCKED_IDS, &[&both])
            .await?;
        let gone = tx
            .query(
                "DELETE FROM memory_exchanges WHERE id = ANY($1) \
                   AND COALESCE(ended, at) < now() - make_interval(days => $2) \
                 RETURNING id, group_id",
                &[&old, &days],
            )
            .await?;
        let gone = refs_by_group(&gone)?;
        let capped = tx
            .query(
                "UPDATE memory_exchanges SET past_cap = true WHERE id = ANY($1) AND NOT past_cap \
                 RETURNING id, group_id",
                &[&over],
            )
            .await
            .context("capping sessions' indexed exchanges")?;
        let capped = refs_by_group(&capped)?;
        let mut purged = 0;
        for (group, refs) in gone.iter().chain(capped.iter()) {
            for purge in crate::memory::passage::ref_purges(refs) {
                queue_purge(&tx, group.as_deref(), &purge).await?;
                purged += 1;
            }
        }
        tx.execute(
            "DELETE FROM memory_reads WHERE at < now() - interval '90 days'",
            &[],
        )
        .await?;
        tx.execute(
            "DELETE FROM memory_purges WHERE done_at < now() - interval '7 days'",
            &[],
        )
        .await?;
        tx.commit().await?;
        let count = |m: &HashMap<Option<String>, Vec<Uuid>>| m.values().map(Vec::len).sum();
        Ok(Swept {
            gone: count(&gone),
            capped: count(&capped),
            purges: purged,
        })
    }

    /// The exchanges a keyword search of `q` finds, the best first: Postgres's
    /// own text search, until the search index is ready.
    pub async fn memory_search(&self, q: &MemoryQuery) -> Result<Vec<MemoryHit>> {
        let client = self.pool().get().await?;
        let (one_group, group) = match &q.group {
            None => (false, None),
            Some(g) => (true, g.clone()),
        };
        let rows = client
            .query(
                "SELECT session, group_id, project, at, asker, ask, answer, compaction \
                 FROM memory_exchanges, websearch_to_tsquery('simple', $1) query \
                 WHERE tsv @@ query \
                   AND (NOT $2::bool OR group_id IS NOT DISTINCT FROM $3::text) \
                   AND ($4::uuid IS NULL OR session = $4) \
                 ORDER BY ts_rank(tsv, query) DESC, at DESC LIMIT $5",
                &[&q.q, &one_group, &group, &q.session, &q.limit],
            )
            .await
            .context("searching session memory")?;
        rows.iter()
            .map(|r| {
                Ok(MemoryHit {
                    session: r.try_get(0)?,
                    group: r.try_get(1)?,
                    project: r.try_get(2)?,
                    at: r.try_get(3)?,
                    asker: Asker::parse(r.try_get(4)?)?,
                    ask: r.try_get(5)?,
                    answer: r.try_get(6)?,
                    compaction: r.try_get(7)?,
                    kind: exchange_kind(),
                    noted_by_desk: false,
                    confirmed: false,
                    superseded: false,
                })
            })
            .collect()
    }

    /// A session's exchanges, the latest `limit` in order, and its counts.
    pub async fn session_memory(
        &self,
        session: Uuid,
        limit: i64,
    ) -> Result<(Vec<KeptExchange>, MemoryCounts)> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM (SELECT seq, at, ended, asker, ask, answer, steps, files, \
                   commands, errors, compaction, redacted FROM memory_exchanges \
                   WHERE session = $1 ORDER BY seq DESC LIMIT $2) x ORDER BY seq",
                &[&session, &limit],
            )
            .await?;
        let exchanges = rows
            .iter()
            .map(|r| {
                Ok(KeptExchange {
                    seq: r.try_get(0)?,
                    at: r.try_get(1)?,
                    ended: r.try_get(2)?,
                    asker: Asker::parse(r.try_get(3)?)?,
                    ask: r.try_get(4)?,
                    answer: r.try_get(5)?,
                    steps: serde_json::from_value(r.try_get(6)?).unwrap_or_default(),
                    files: r.try_get(7)?,
                    commands: r.try_get(8)?,
                    errors: r.try_get(9)?,
                    compaction: r.try_get(10)?,
                    redacted: r.try_get(11)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let c = client
            .query_one(
                "SELECT COUNT(*), COUNT(*) FILTER (WHERE ended IS NULL), \
                   COUNT(*) FILTER (WHERE compaction), COALESCE(SUM(redacted), 0)::int8, \
                   COUNT(*) FILTER (WHERE past_cap) \
                 FROM memory_exchanges WHERE session = $1",
                &[&session],
            )
            .await?;
        Ok((
            exchanges,
            MemoryCounts {
                exchanges: c.try_get(0)?,
                open: c.try_get(1)?,
                compactions: c.try_get(2)?,
                secrets_hidden: c.try_get(3)?,
                past_cap: c.try_get(4)?,
            },
        ))
    }
}

/// Queues `purge` of `group`'s collection, to run once `tx` commits.
pub(crate) async fn queue_purge(
    tx: &tokio_postgres::Transaction<'_>,
    group: Option<&str>,
    purge: &Purge,
) -> Result<()> {
    queue_purge_of(tx, &collection_of(group), purge).await
}

/// Queues `purge` of `collection`, to run once `tx` commits.
pub(crate) async fn queue_purge_of(
    tx: &tokio_postgres::Transaction<'_>,
    collection: &str,
    purge: &Purge,
) -> Result<()> {
    tx.execute(
        "INSERT INTO memory_purges (id, collection, filter, created_at) \
         VALUES (gen_random_uuid(), $1, $2, clock_timestamp())",
        &[&collection, &serde_json::to_value(purge)?],
    )
    .await
    .context("queueing an index deletion")?;
    Ok(())
}

/// Rows' ids (column 0) by their group (column 1).
fn refs_by_group(rows: &[tokio_postgres::Row]) -> Result<HashMap<Option<String>, Vec<Uuid>>> {
    let mut by: HashMap<Option<String>, Vec<Uuid>> = HashMap::new();
    for r in rows {
        by.entry(r.try_get(1)?).or_default().push(r.try_get(0)?);
    }
    Ok(by)
}

/// Forgets what session memory holds of `ids` within `tx`, which forgets
/// the sessions themselves: their exchanges, items, derivation queue and
/// reads, and a tombstone for each conversation and record they had, so
/// they are never harvested again. Their rows are locked first, so a step
/// of theirs being written finishes before, or writes nothing after.
/// Ledger rows stay: they are numbers only.
pub(crate) async fn forget_memory_in(
    tx: &tokio_postgres::Transaction<'_>,
    ids: &[Uuid],
) -> Result<()> {
    tx.execute(
        "SELECT 1 FROM sessions WHERE id = ANY($1) FOR UPDATE",
        &[&ids],
    )
    .await?;
    // Then their derivation queue, as writing what was noted locks it
    // before their notes.
    tx.execute(
        "SELECT 1 FROM memory_derive WHERE session = ANY($1) ORDER BY session FOR UPDATE",
        &[&ids],
    )
    .await?;
    // Their exchanges are locked in id order, as every write of many
    // exchanges locks them.
    tx.execute(
        "SELECT 1 FROM memory_exchanges WHERE session = ANY($1) ORDER BY id FOR UPDATE",
        &[&ids],
    )
    .await?;
    // And their notes, in id order, as every write of many notes locks
    // them.
    tx.execute(
        "SELECT 1 FROM memory_items WHERE session = ANY($1) ORDER BY id FOR UPDATE",
        &[&ids],
    )
    .await?;
    // The exchanges' own keys too: a fork that copied them can't keep them
    // again.
    tx.execute(
        "INSERT INTO memory_forgotten (engine, ref, at) \
         SELECT engine, 'key:' || key, now() FROM memory_exchanges WHERE session = ANY($1) \
         ON CONFLICT DO NOTHING",
        &[&ids],
    )
    .await
    .context("tombstoning forgotten exchanges")?;
    tx.execute(
        "INSERT INTO memory_forgotten (engine, ref, at) \
         SELECT engine, ref, now() FROM ( \
           SELECT engine, cli_session_id AS ref FROM sessions WHERE id = ANY($1) \
           UNION SELECT engine, transcript FROM sessions WHERE id = ANY($1) \
           UNION SELECT s.engine, c.source FROM ledger_cursors c \
             JOIN sessions s ON s.id = c.session WHERE c.session = ANY($1) \
           UNION SELECT engine, cli_session_id FROM memory_sessions WHERE session = ANY($1) \
           UNION SELECT engine, unnest(sources) FROM memory_sessions WHERE session = ANY($1)) r \
         WHERE ref IS NOT NULL AND ref <> '' ON CONFLICT DO NOTHING",
        &[&ids],
    )
    .await
    .context("tombstoning forgotten records")?;
    // What the index holds of them goes too: one purge per session for
    // each collection its rows are in (a session moved between groups has
    // rows in more than one).
    for row in tx
        .query(
            "SELECT DISTINCT session, group_id FROM memory_exchanges WHERE session = ANY($1) \
             UNION SELECT DISTINCT session, group_id FROM memory_items WHERE session = ANY($1)",
            &[&ids],
        )
        .await?
    {
        let (session, group): (Uuid, Option<String>) = (row.try_get(0)?, row.try_get(1)?);
        queue_purge(tx, group.as_deref(), &Purge::Session { session }).await?;
    }
    let readers: Vec<String> = ids.iter().map(|id| format!("session:{id}")).collect();
    tx.execute(
        "DELETE FROM memory_reads WHERE reader = ANY($1)",
        &[&readers],
    )
    .await?;
    // A session that stays keeps reading its records for the ledger, but
    // no harvest text of it is kept.
    tx.execute(
        "UPDATE ledger_cursors SET harvest = '{\"decided\": true}', harvested_to = read_to \
         WHERE session = ANY($1)",
        &[&ids],
    )
    .await?;
    for table in [
        "memory_exchanges",
        "memory_items",
        "memory_derive",
        "memory_derive_reads",
        "memory_sessions",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE session = ANY($1)"),
            &[&ids],
        )
        .await
        .with_context(|| format!("forgetting {table}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{build_pool, tests::record_fixture};
    use workforce_postgres::test_support::with_test_pool;

    fn exchange(key: &str, ask: &str, ended: Option<DateTime<Utc>>) -> Exchange {
        Exchange {
            key: key.into(),
            at: ended.unwrap_or_else(Utc::now) - chrono::Duration::minutes(1),
            ended,
            asker: Asker::Person,
            ask: ask.into(),
            answer: "done".into(),
            steps: Vec::new(),
            files: Default::default(),
            commands: Vec::new(),
            errors: Vec::new(),
            compaction: false,
            redacted: 0,
            flagged: ["instruction_override".to_string()].into(),
        }
    }

    async fn write(store: &DeskStore, session: Uuid, closed: &[Exchange], open: Option<&Exchange>) {
        let cursor = Cursor {
            read_to: 10,
            harvested_to: 10,
            harvest: serde_json::json!({"open": {"ask": "what it said"}}),
            ..Cursor::default()
        };
        store
            .record_write(&RecordWrite {
                session,
                engine: Engine::Claude,
                source: "/records/t.jsonl",
                cursor: &cursor,
                rows: &[],
                closed,
                open,
                project: "/dev/acme-web",
                folder: "/dev/acme-web",
                name: "acme-web",
                cli_session_id: Some("5b1f0c2e"),
            })
            .await
            .unwrap();
    }

    async fn n(store: &DeskStore, sql: &str, id: Uuid) -> i64 {
        let client = store.pool().get().await.unwrap();
        client.query_one(sql, &[&id]).await.unwrap().get(0)
    }

    #[tokio::test]
    async fn the_sweep_deletes_what_is_past_its_keeping_ended_or_not() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let s = record_fixture("acme-web");
            store.put(&s).await.unwrap();
            let old = Utc::now() - chrono::Duration::days(200);
            let mut never = exchange("x-never", "stopped mid-turn", None);
            never.at = old;
            write(&store, s.id, &[exchange("x-old", "long ago", Some(old)), exchange("x-new", "today", Some(Utc::now()))], Some(&never)).await;
            assert_eq!(store.sweep_memory(180).await.unwrap().gone, 2);
            let (kept, _) = store.session_memory(s.id, 10).await.unwrap();
            assert_eq!(kept.iter().map(|x| x.ask.as_str()).collect::<Vec<_>>(), ["today"]);
            assert_eq!(n(&store, "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1 AND cardinality(flagged) = 1", s.id).await, 1, "what was flagged is kept with it");
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_forgotten_while_its_step_was_read_keeps_no_cursor_and_no_text() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let gone = Uuid::new_v4();
            write(
                &store,
                gone,
                &[exchange("x-1", "fix it", Some(Utc::now()))],
                None,
            )
            .await;
            assert_eq!(
                n(
                    &store,
                    "SELECT COUNT(*) FROM ledger_cursors WHERE session = $1",
                    gone
                )
                .await,
                0
            );
            assert_eq!(
                n(
                    &store,
                    "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1",
                    gone
                )
                .await,
                0
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clearing_keeps_memory_and_forgetting_tombstones_even_a_forks_copy() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let (cleared, forgotten, fork) = (
                record_fixture("acme-old"),
                record_fixture("acme-web"),
                record_fixture("acme-fork"),
            );
            for s in [&cleared, &forgotten, &fork] {
                store.put(s).await.unwrap();
            }
            write(
                &store,
                cleared.id,
                &[exchange("x-c", "the cookie flag", Some(Utc::now()))],
                None,
            )
            .await;
            write(
                &store,
                forgotten.id,
                &[exchange("x-f", "the vault path", Some(Utc::now()))],
                None,
            )
            .await;
            store
                .remove_sessions(&[cleared.id], &[], "you", Memory::Keep)
                .await
                .unwrap();
            store
                .remove_sessions(&[forgotten.id], &[], "you", Memory::Forget)
                .await
                .unwrap();

            let found = store
                .memory_search(&MemoryQuery {
                    q: "cookie".into(),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(
                found.len(),
                1,
                "a cleared session's memory is still searched"
            );
            assert_eq!(found[0].session, cleared.id);
            assert_eq!(
                n(
                    &store,
                    "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1",
                    forgotten.id
                )
                .await,
                0
            );

            // A cleared session's memory is still named, and can be forgotten.
            assert_eq!(
                store.memory_names(&[cleared.id]).await.unwrap(),
                [(cleared.id, "acme-web".to_string(), Engine::Claude)]
            );
            assert!(store.forget_memory(cleared.id).await.unwrap());
            assert_eq!(n(&store, "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1", cleared.id).await, 0);
            assert_eq!(n(&store, "SELECT COUNT(*) FROM memory_sessions WHERE session = $1", cleared.id).await, 0);
            assert_eq!(
                n(&store, "SELECT COUNT(*) FROM memory_forgotten WHERE ref IN ('5b1f0c2e', '/records/t.jsonl', 'key:x-c') AND $1::uuid IS NOT NULL", cleared.id).await,
                3,
                "its conversation, record and exchange are tombstoned"
            );
            assert!(!store.forget_memory(Uuid::new_v4()).await.unwrap(), "nothing to forget");

            // A fork that copied the forgotten lines can't keep them again.
            write(
                &store,
                fork.id,
                &[exchange("x-f", "the vault path", Some(Utc::now()))],
                None,
            )
            .await;
            assert_eq!(
                n(
                    &store,
                    "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1",
                    fork.id
                )
                .await,
                0
            );
        })
        .await;
    }
}

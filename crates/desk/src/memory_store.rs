//! Session memory in Postgres: the exchanges harvested from each record,
//! written in one transaction with the ledger's rows and both of the
//! record's offsets; what is never to be read again; the person's switches;
//! and the keyword search over what is kept.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::engine::Engine;
use crate::ledger_store::{Cursor, Kept};
use crate::memory::record::{Asker, Exchange, Step};
use crate::store::DeskStore;

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
    pub limit: i64,
}

/// An exchange a search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHit {
    pub session: Uuid,
    pub group: Option<String>,
    pub project: String,
    pub at: DateTime<Utc>,
    pub asker: Asker,
    pub ask: String,
    pub answer: String,
    pub compaction: bool,
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
}

fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The setting key of a group's memory switches.
pub fn group_key(group: &str) -> String {
    format!("memory.group.{group}")
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
        if held {
            let base: i32 = tx
                .query_one(
                    "SELECT COALESCE(MAX(seq), 0) FROM memory_exchanges WHERE session = $1",
                    &[&w.session],
                )
                .await?
                .try_get(0)?;
            for (i, x) in w.closed.iter().chain(w.open).enumerate() {
                let seq = base + i32::try_from(i).unwrap_or(i32::MAX - base) + 1;
                let steps = serde_json::to_value(&x.steps)?;
                let files: Vec<&str> = x.files.iter().map(String::as_str).collect();
                tx.execute(
                    "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, \
                       folder, seq, at, ended, asker, ask, answer, steps, files, commands, \
                       errors, compaction, redacted) \
                     VALUES (gen_random_uuid(), $1, $2, $3, ( \
                       SELECT NULLIF(g.group_id, 'ungrouped') FROM group_members g \
                       WHERE g.item = 'session:' || $3::uuid::text), $4, $5, $6, $7, $8, $9, \
                       $10, $11, $12, $13, $14, $15, $16, $17) \
                     ON CONFLICT (engine, key) DO UPDATE SET ended = EXCLUDED.ended, \
                       ask = EXCLUDED.ask, answer = EXCLUDED.answer, steps = EXCLUDED.steps, \
                       files = EXCLUDED.files, commands = EXCLUDED.commands, \
                       errors = EXCLUDED.errors, redacted = EXCLUDED.redacted \
                     WHERE memory_exchanges.session = EXCLUDED.session",
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
                    ],
                )
                .await
                .context("keeping an exchange")?;
            }
        }
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
        tx.commit().await?;
        Ok(changed)
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

    /// Sets a session's own memory switch.
    pub async fn set_session_memory(&self, session: Uuid, on: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE sessions SET memory = $2 WHERE id = $1",
                &[&session, &if on { "on" } else { "off" }],
            )
            .await?;
        Ok(())
    }

    /// Deletes conversations that ended more than `retain_days` ago, and
    /// reads older than 90 days. How many exchanges went.
    pub async fn sweep_memory(&self, retain_days: u32) -> Result<u64> {
        let client = self.pool().get().await?;
        let days = f64::from(retain_days);
        let gone = client
            .execute(
                "DELETE FROM memory_exchanges WHERE ended < now() - make_interval(days => $1)",
                &[&days],
            )
            .await?;
        client
            .execute(
                "DELETE FROM memory_reads WHERE at < now() - interval '90 days'",
                &[],
            )
            .await?;
        Ok(gone)
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
                   COUNT(*) FILTER (WHERE compaction), COALESCE(SUM(redacted), 0)::int8 \
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
            },
        ))
    }
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
    tx.execute(
        "INSERT INTO memory_forgotten (engine, ref, at) \
         SELECT engine, ref, now() FROM ( \
           SELECT engine, cli_session_id AS ref FROM sessions WHERE id = ANY($1) \
           UNION SELECT engine, transcript FROM sessions WHERE id = ANY($1) \
           UNION SELECT s.engine, c.source FROM ledger_cursors c \
             JOIN sessions s ON s.id = c.session WHERE c.session = ANY($1)) r \
         WHERE ref IS NOT NULL AND ref <> '' ON CONFLICT DO NOTHING",
        &[&ids],
    )
    .await
    .context("tombstoning forgotten records")?;
    let readers: Vec<String> = ids.iter().map(|id| format!("session:{id}")).collect();
    tx.execute(
        "DELETE FROM memory_reads WHERE reader = ANY($1)",
        &[&readers],
    )
    .await?;
    for table in ["memory_exchanges", "memory_items", "memory_derive"] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE session = ANY($1)"),
            &[&ids],
        )
        .await
        .with_context(|| format!("forgetting {table}"))?;
    }
    Ok(())
}

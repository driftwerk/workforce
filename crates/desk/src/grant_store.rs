//! Grants, the engine's own switches for bosses, and the audit of what was
//! asked of it about them, in Postgres ([`crate::grant`]).

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::grant::{BossRef, Floor, Grant, Level, Sees, Stopped, StoppedFrom};
use crate::service::{Layout, UNGROUPED};
use crate::store::DeskStore;

/// The engine's switches for bosses, as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BossEngine {
    pub paused: bool,
    pub paused_at: Option<DateTime<Utc>>,
    pub paused_from: Option<String>,
    /// When a machine began taking its sessions' groups from its grants.
    pub grants_since: Option<DateTime<Utc>>,
}

/// One row of the boss audit.
#[derive(Debug, Clone)]
pub struct CallRow {
    /// `desk`, `machine`, or `boss:<hub>/<agent>`.
    pub caller: String,
    pub boss: Option<String>,
    pub group: Option<String>,
    pub tool: &'static str,
    /// What was asked, with nothing secret in it.
    pub input: String,
    pub input_hash: String,
    /// `ok` or `refused`.
    pub outcome: &'static str,
    pub said: Option<String>,
    pub result: serde_json::Value,
}

/// What a machine's grant write also does: the sidebar its grants make,
/// and the sessions the written grants name, with their groups, to restamp
/// the first time each is named.
#[derive(Debug, Clone)]
pub struct Projection<'a> {
    pub layout: &'a Layout,
    pub named: Vec<(Uuid, String)>,
    /// Every session the grants name after the write, with its group: where
    /// an old group's budgets go.
    pub all: Vec<(Uuid, String)>,
    /// Every grant's group after the write: rows already in one are never
    /// restamped.
    pub granted: Vec<String>,
}

/// What a write did.
#[derive(Debug, Clone, Default)]
pub struct Written {
    /// Boards in a group the machine's sidebar no longer has.
    pub ungrouped_boards: Vec<Uuid>,
    /// The sessions restamped by it.
    pub restamped: Vec<Uuid>,
}

const COLUMNS: &str = "group_id, name, revision, grant_version, boss, level, sees, recall, \
     folders, repos, members, born, act, stopped_at, stopped_from, issued_at, expires_at";

fn grant_of(r: &Row) -> Result<Grant> {
    let boss: Option<String> = r.try_get("boss")?;
    let level: String = r.try_get("level")?;
    let sees: String = r.try_get("sees")?;
    let stopped_at: Option<DateTime<Utc>> = r.try_get("stopped_at")?;
    let stopped_from: Option<String> = r.try_get("stopped_from")?;
    let grant_version: i32 = r.try_get("grant_version")?;
    let members: Vec<Uuid> = r.try_get("members")?;
    let born: Vec<Uuid> = r.try_get("born")?;
    Ok(Grant {
        group: r.try_get("group_id")?,
        name: r.try_get("name")?,
        revision: r.try_get("revision")?,
        // A version this engine can't say is one it doesn't read.
        grant_version: u32::try_from(grant_version).unwrap_or(u32::MAX),
        boss: boss
            .as_deref()
            .map(BossRef::parse)
            .transpose()
            .map_err(|e| eyre::eyre!(e))?,
        // A level or sight this engine can't read is the narrowest.
        level: Level::parse(&level).unwrap_or_default(),
        sees: Sees::parse(&sees).unwrap_or_default(),
        recall: r.try_get("recall")?,
        folders: r.try_get("folders")?,
        repos: r.try_get("repos")?,
        members: members.into_iter().collect(),
        born: born.into_iter().collect(),
        act: r.try_get("act")?,
        stopped: stopped_at.map(|at| Stopped {
            at,
            // A stop from somewhere unknown is still a stop.
            from: stopped_from
                .as_deref()
                .and_then(StoppedFrom::parse)
                .unwrap_or(StoppedFrom::Machine),
        }),
        issued_at: r.try_get("issued_at")?,
        expires_at: r.try_get("expires_at")?,
    })
}

async fn upsert(tx: &tokio_postgres::Transaction<'_>, g: &Grant) -> Result<()> {
    let members: Vec<Uuid> = g.members.iter().copied().collect();
    let born: Vec<Uuid> = g.born.iter().copied().collect();
    let version = i32::try_from(g.grant_version).unwrap_or(i32::MAX);
    let boss = g.boss.as_ref().map(BossRef::key);
    let stopped_at = g.stopped.map(|s| s.at);
    let stopped_from = g.stopped.map(|s| s.from.as_str());
    tx.execute(
        &format!(
            "INSERT INTO boss_grants ({COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
             ON CONFLICT (group_id) DO UPDATE SET name = EXCLUDED.name, \
               revision = EXCLUDED.revision, grant_version = EXCLUDED.grant_version, \
               boss = EXCLUDED.boss, level = EXCLUDED.level, sees = EXCLUDED.sees, \
               recall = EXCLUDED.recall, folders = EXCLUDED.folders, repos = EXCLUDED.repos, \
               members = EXCLUDED.members, born = EXCLUDED.born, act = EXCLUDED.act, \
               stopped_at = EXCLUDED.stopped_at, stopped_from = EXCLUDED.stopped_from, \
               issued_at = EXCLUDED.issued_at, expires_at = EXCLUDED.expires_at, \
               deleted_at = NULL"
        ),
        &[
            &g.group,
            &g.name,
            &g.revision,
            &version,
            &boss,
            &g.level.as_str(),
            &g.sees.as_str(),
            &g.recall,
            &g.folders,
            &g.repos,
            &members,
            &born,
            &g.act,
            &stopped_at,
            &stopped_from,
            &g.issued_at,
            &g.expires_at,
        ],
    )
    .await
    .with_context(|| format!("keeping {}'s grant", g.name))?;
    Ok(())
}

async fn audit(tx: &tokio_postgres::Transaction<'_>, c: &CallRow) -> Result<()> {
    tx.execute(
        "INSERT INTO boss_calls (id, at, caller, boss, group_id, tool, input, input_hash, \
           outcome, said, result) \
         VALUES (gen_random_uuid(), now(), $1, $2, $3, $4, $5, $6, $7, $8, $9)",
        &[
            &c.caller,
            &c.boss,
            &c.group,
            &c.tool,
            &c.input,
            &c.input_hash,
            &c.outcome,
            &c.said,
            &c.result,
        ],
    )
    .await
    .context("auditing a boss call")?;
    Ok(())
}

/// What a boss's call recorded before it was acted on finds: a new row to
/// finish, or the one an earlier sending of the same call left.
#[derive(Debug, Clone, PartialEq)]
pub enum Begun {
    New(Uuid),
    /// The same `(link_id, call_id)` came before: its outcome and what was
    /// said, never acted on again.
    Again {
        /// The boss that sent it first (its key).
        boss: Option<String>,
        outcome: String,
        said: Option<String>,
    },
}

impl DeskStore {
    /// Records a boss's call before it is acted on, `pending`; one whose
    /// `(link_id, call_id)` came already is found instead.
    pub async fn boss_call_begin(
        &self,
        link_id: Option<&str>,
        call_id: Option<&str>,
        c: &CallRow,
    ) -> Result<Begun> {
        let client = self.pool().get().await?;
        let id = Uuid::new_v4();
        let made = client
            .query_opt(
                "INSERT INTO boss_calls (id, at, link_id, call_id, caller, boss, group_id, tool, \
                   input, input_hash, outcome, result) \
                 VALUES ($1, now(), $2, $3, $4, $5, $6, $7, $8, $9, 'pending', '{}') \
                 ON CONFLICT (link_id, call_id) DO NOTHING RETURNING id",
                &[
                    &id,
                    &link_id,
                    &call_id,
                    &c.caller,
                    &c.boss,
                    &c.group,
                    &c.tool,
                    &c.input,
                    &c.input_hash,
                ],
            )
            .await
            .context("recording a boss's call")?;
        if made.is_some() {
            return Ok(Begun::New(id));
        }
        let r = client
            .query_one(
                "SELECT outcome, said, boss FROM boss_calls WHERE link_id = $1 AND call_id = $2",
                &[&link_id, &call_id],
            )
            .await
            .context("reading a boss's call sent again")?;
        Ok(Begun::Again {
            outcome: r.try_get(0)?,
            said: r.try_get(1)?,
            boss: r.try_get(2)?,
        })
    }

    /// How many calls of `tool` that came to one of `outcomes` link
    /// `link_id` made here since `since`.
    pub async fn boss_calls_count(
        &self,
        link_id: &str,
        tool: &str,
        outcomes: &[&str],
        since: DateTime<Utc>,
    ) -> Result<i64> {
        let client = self.pool().get().await?;
        let outcomes: Vec<String> = outcomes.iter().map(|o| o.to_string()).collect();
        Ok(client
            .query_one(
                "SELECT COUNT(*) FROM boss_calls \
                 WHERE link_id = $1 AND tool = $2 AND outcome = ANY($3) AND at > $4",
                &[&link_id, &tool, &outcomes, &since],
            )
            .await
            .context("counting a boss link's calls")?
            .try_get(0)?)
    }

    /// What became of the call `id` began.
    pub async fn boss_call_end(
        &self,
        id: Uuid,
        outcome: &str,
        said: Option<&str>,
        result: &serde_json::Value,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_calls SET outcome = $2, said = $3, result = $4 WHERE id = $1",
                &[&id, &outcome, &said, result],
            )
            .await
            .context("recording what became of a boss's call")?;
        Ok(())
    }

    /// Every grant this engine keeps.
    pub async fn boss_grants(&self) -> Result<Vec<Grant>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_grants WHERE deleted_at IS NULL ORDER BY group_id"
                ),
                &[],
            )
            .await
            .context("reading the grants")?
            .iter()
            .map(grant_of)
            .collect()
    }

    /// What deleted grants (and stops of groups with no grant) left: each
    /// group's revision and stop.
    pub async fn boss_grant_floors(&self) -> Result<Vec<(String, Floor)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT group_id, revision, stopped_at, stopped_from FROM boss_grants \
                 WHERE deleted_at IS NOT NULL",
                &[],
            )
            .await
            .context("reading the deleted grants")?
            .iter()
            .map(|r| {
                let at: Option<DateTime<Utc>> = r.try_get(2)?;
                let from: Option<String> = r.try_get(3)?;
                Ok((
                    r.try_get(0)?,
                    Floor {
                        revision: r.try_get(1)?,
                        stopped: at.map(|at| Stopped {
                            at,
                            from: from
                                .as_deref()
                                .and_then(StoppedFrom::parse)
                                .unwrap_or(StoppedFrom::Machine),
                        }),
                    },
                ))
            })
            .collect()
    }

    /// Stops each group's grant, or, for a group with none, leaves a stop a
    /// later grant of it keeps; a grant stopped already keeps its first
    /// stop. With `call`'s audit when given, even when nothing changed.
    pub async fn stop_grants(
        &self,
        stops: &[(String, Stopped)],
        call: Option<&CallRow>,
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for (group, s) in stops {
            tx.execute(
                "INSERT INTO boss_grants (group_id, name, revision, grant_version, level, sees, \
                   stopped_at, stopped_from, issued_at, expires_at, deleted_at) \
                 VALUES ($1, $1, 0, 1, 'watch', 'status', $2, $3, $2, $2, $2) \
                 ON CONFLICT (group_id) DO UPDATE SET \
                   stopped_from = CASE WHEN boss_grants.stopped_at IS NULL \
                     THEN EXCLUDED.stopped_from ELSE boss_grants.stopped_from END, \
                   stopped_at = COALESCE(boss_grants.stopped_at, EXCLUDED.stopped_at)",
                &[group, &s.at, &s.from.as_str()],
            )
            .await
            .context("keeping a stop")?;
        }
        if let Some(call) = call {
            audit(&tx, call).await?;
        }
        tx.commit().await.context("keeping the stops")?;
        Ok(())
    }

    /// The engine's switches for bosses.
    pub async fn boss_engine(&self) -> Result<BossEngine> {
        let client = self.pool().get().await?;
        let r = client
            .query_one(
                "SELECT paused, paused_at, paused_from, grants_since FROM boss_engine",
                &[],
            )
            .await
            .context("reading the engine's switches for bosses")?;
        Ok(BossEngine {
            paused: r.try_get(0)?,
            paused_at: r.try_get(1)?,
            paused_from: r.try_get(2)?,
            grants_since: r.try_get(3)?,
        })
    }

    /// Writes `put` and deletes `delete`'s grant, in one transaction with
    /// `call`'s audit; on a machine, with `projection`'s sidebar and the
    /// first restamp of each session it names.
    pub async fn write_grants(
        &self,
        put: &[Grant],
        delete: Option<(&str, Option<Stopped>)>,
        projection: Option<Projection<'_>>,
        call: &CallRow,
    ) -> Result<Written> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for g in put {
            upsert(&tx, g).await?;
        }
        if let Some((group, stopped)) = delete {
            // What is left keeps its revision and its stop.
            let at = stopped.map(|s| s.at);
            let from = stopped.map(|s| s.from.as_str());
            tx.execute(
                "UPDATE boss_grants SET deleted_at = now(), members = '{}', born = '{}', \
                   stopped_from = CASE WHEN stopped_at IS NULL THEN $3 ELSE stopped_from END, \
                   stopped_at = COALESCE(stopped_at, $2) \
                 WHERE group_id = $1",
                &[&group, &at, &from],
            )
            .await
            .context("deleting a grant")?;
        }
        let mut written = Written::default();
        if let Some(p) = projection {
            written = project(&tx, &p).await?;
        }
        audit(&tx, call).await?;
        tx.commit().await.context("keeping the grants")?;
        Ok(written)
    }

    /// Sets or clears the engine-wide pause, with its audit.
    pub async fn put_bosses_paused(
        &self,
        paused: bool,
        from: Option<&str>,
        call: &CallRow,
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "UPDATE boss_engine SET paused = $1, \
               paused_at = CASE WHEN $1 THEN COALESCE(paused_at, now()) END, \
               paused_from = CASE WHEN $1 THEN COALESCE(paused_from, $2) END",
            &[&paused, &from],
        )
        .await
        .context("keeping the bosses' pause")?;
        audit(&tx, call).await?;
        tx.commit().await.context("keeping the bosses' pause")?;
        Ok(())
    }

    /// Keeps `call` alone: what was refused.
    pub async fn boss_call(&self, call: &CallRow) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        audit(&tx, call).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The latest boss audit rows, newest first, for tests and the status
    /// route: (tool, outcome, caller, group).
    pub async fn boss_calls_latest(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, String, String, Option<String>)>> {
        let client = self.pool().get().await?;
        Ok(client
            .query(
                "SELECT tool, outcome, caller, group_id FROM boss_calls \
                 ORDER BY at DESC, id LIMIT $1",
                &[&limit],
            )
            .await?
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
            .collect())
    }
}

/// A machine's grants made its sidebar, within `tx`; each session they
/// name for the first time is restamped from its old layout's group to its
/// grant's (decision 8): its ledger rows and session memory written before
/// the machine took its groups from grants, and its old group's budgets
/// copied to its new one. Only into the session's own grant's group, only
/// rows in no grant's group, and once per session ever: a later move
/// between grants keeps what was written where it was written, as a move
/// on the Mac does. Activity and lines keep their stamps.
async fn project(tx: &tokio_postgres::Transaction<'_>, p: &Projection<'_>) -> Result<Written> {
    // The first time, the machine's own old layout is kept as it was: each
    // session's first restamp reads it, whenever that comes.
    let r = tx
        .query_one(
            "UPDATE boss_engine SET grants_since = COALESCE(grants_since, now()), \
               old_groups = COALESCE(old_groups, (SELECT COALESCE(jsonb_object_agg(item, group_id), \
                 '{}'::jsonb) FROM group_members \
                 WHERE item LIKE 'session:%' AND group_id <> 'ungrouped')) \
             RETURNING grants_since, old_groups",
            &[],
        )
        .await
        .context("noting when grants began")?;
    let since: DateTime<Utc> = r.try_get(0)?;
    let was: HashMap<String, String> =
        serde_json::from_value(r.try_get::<_, serde_json::Value>(1)?).unwrap_or_default();
    let ungrouped_boards = crate::service::put_layout_in(tx, p.layout, true).await?;
    // Only into a group: a session the home put in Ungrouped keeps its
    // rows where they are, since a restamp only ever narrows.
    let (sessions, groups): (Vec<Uuid>, Vec<String>) = p
        .named
        .iter()
        .filter(|(_, g)| g != UNGROUPED)
        .cloned()
        .unzip();
    let fresh: Vec<(Uuid, String)> = tx
        .query(
            "INSERT INTO boss_restamped (session, group_id, at) \
             SELECT s, g, now() FROM UNNEST($1::uuid[], $2::text[]) AS r(s, g) \
             ON CONFLICT (session) DO NOTHING RETURNING session, group_id",
            &[&sessions, &groups],
        )
        .await
        .context("noting which sessions are restamped")?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    if fresh.is_empty() {
        return Ok(Written {
            ungrouped_boards,
            restamped: Vec::new(),
        });
    }
    let (ids, to): (Vec<Uuid>, Vec<String>) = fresh.iter().cloned().unzip();
    let items: Vec<String> = ids
        .iter()
        .map(|s| crate::service::session_item(*s))
        .collect();
    let granted = &p.granted;
    // The old layout's moves of these sessions are of groups no one means:
    // a row read late is stamped from the grant, never from them.
    tx.execute("DELETE FROM group_moves WHERE item = ANY($1)", &[&items])
        .await?;
    tx.execute(
        "UPDATE ledger l SET group_id = r.g \
         FROM UNNEST($1::uuid[], $2::text[]) AS r(s, g) \
         WHERE l.session = r.s AND l.at < $3 \
           AND (l.group_id IS NULL OR NOT l.group_id = ANY($4))",
        &[&ids, &to, &since, granted],
    )
    .await
    .context("restamping the ledger")?;
    restamp_memory(tx, &ids, &to, since, granted).await?;
    // Each old group's budgets go along to the groups its sessions are in
    // now, split between them, the stricter kept where one is there
    // already: a limit is never lost or doubled on the way. A session the
    // home put in Ungrouped counts toward Desk's own budgets there.
    let olds: BTreeSet<&String> = fresh
        .iter()
        .filter_map(|(s, _)| was.get(&crate::service::session_item(*s)))
        .filter(|x| !granted.contains(*x))
        .collect();
    let (mut xs, mut gs, mut ns) = (Vec::new(), Vec::new(), Vec::new());
    for x in olds {
        let to: BTreeSet<&String> = p
            .all
            .iter()
            .filter(|(s, g)| {
                g != UNGROUPED && g != x && was.get(&crate::service::session_item(*s)) == Some(x)
            })
            .map(|(_, g)| g)
            .collect();
        let n = to.len() as f64;
        for g in to {
            xs.push(x.clone());
            gs.push(g.clone());
            ns.push(n);
        }
    }
    if !xs.is_empty() {
        tx.execute(
            "INSERT INTO budgets (id, scope, group_id, period, unit, amount, warn_at, \
               on_exceed, updated_at) \
             SELECT gen_random_uuid(), b.scope, p.g, b.period, b.unit, MIN(b.amount / p.n), \
               MIN(b.warn_at), \
               CASE WHEN bool_or(b.on_exceed = 'pause') THEN 'pause' ELSE MIN(b.on_exceed) END, \
               now() \
             FROM budgets b JOIN UNNEST($1::text[], $2::text[], $3::float8[]) AS p(x, g, n) \
               ON b.group_id = p.x \
             GROUP BY p.g, b.scope, b.period, b.unit \
             ON CONFLICT ((COALESCE(group_id, '')), scope, period, unit) DO UPDATE SET \
               amount = LEAST(budgets.amount, EXCLUDED.amount), \
               warn_at = LEAST(budgets.warn_at, EXCLUDED.warn_at), \
               on_exceed = CASE WHEN 'pause' IN (budgets.on_exceed, EXCLUDED.on_exceed) \
                 THEN 'pause' ELSE budgets.on_exceed END, \
               updated_at = now()",
            &[&xs, &gs, &ns],
        )
        .await
        .context("taking the old groups' budgets along")?;
    }
    Ok(Written {
        ungrouped_boards,
        restamped: ids,
    })
}

/// The session memory of `ids` written before `since` and in no grant's
/// group, moved to each one's group in `to`: re-indexed there, and taken
/// out of its old collection.
async fn restamp_memory(
    tx: &tokio_postgres::Transaction<'_>,
    ids: &[Uuid],
    to: &[String],
    since: DateTime<Utc>,
    granted: &[String],
) -> Result<()> {
    let pick = |table: &str, at: &str| {
        format!(
            "SELECT m.id FROM {table} m JOIN UNNEST($1::uuid[], $4::text[]) AS r(s, g) \
               ON m.session = r.s \
             WHERE m.{at} < $2 AND (m.group_id IS NULL OR NOT m.group_id = ANY($3))"
        )
    };
    let moving = |table: &str| {
        format!(
            "WITH old AS (SELECT m.id, m.group_id AS was, r.g FROM {table} m \
               JOIN UNNEST($1::uuid[], $2::text[]) AS r(s, g) ON m.session = r.s \
               WHERE m.id = ANY($3)) \
             UPDATE {table} m SET group_id = old.g, indexed_at = NULL \
             FROM old \
             WHERE m.id = old.id RETURNING m.id, old.was"
        )
    };
    let mut moved: HashMap<Option<String>, Vec<Uuid>> = HashMap::new();
    for (table, at, locked) in [
        (
            "memory_exchanges",
            "at",
            crate::memory_index_store::LOCKED_IDS,
        ),
        (
            "memory_items",
            "written_at",
            crate::memory_derive_store::ITEMS_LOCKED,
        ),
    ] {
        let rows: Vec<Uuid> = tx
            .query(&pick(table, at), &[&ids, &since, &granted, &to])
            .await
            .with_context(|| format!("finding {table} to restamp"))?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if rows.is_empty() {
            continue;
        }
        tx.execute(locked, &[&rows]).await?;
        for r in tx
            .query(&moving(table), &[&ids, &to, &rows])
            .await
            .with_context(|| format!("restamping {table}"))?
        {
            moved.entry(r.get(1)).or_default().push(r.get(0));
        }
    }
    for (group, refs) in &moved {
        for purge in crate::memory::passage::ref_purges(refs) {
            crate::memory_store::queue_purge(tx, group.as_deref(), &purge).await?;
        }
    }
    // What was read for notes goes along: the new group starts where the
    // session's least read old one was, so nothing is noted twice there.
    tx.execute(
        "INSERT INTO memory_derive_reads (session, group_key, derived_to) \
         SELECT r.s, r.k, MIN(d.derived_to) FROM (SELECT s, \
             CASE WHEN g = 'ungrouped' THEN '' ELSE g END AS k \
             FROM UNNEST($1::uuid[], $2::text[]) AS u(s, g)) r \
         JOIN memory_derive_reads d ON d.session = r.s AND d.group_key <> r.k \
         GROUP BY r.s, r.k \
         ON CONFLICT (session, group_key) DO UPDATE \
           SET derived_to = GREATEST(memory_derive_reads.derived_to, EXCLUDED.derived_to)",
        &[&ids, &to],
    )
    .await
    .context("moving how far restamped sessions were read for notes")?;
    Ok(())
}

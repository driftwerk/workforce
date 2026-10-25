//! Where shadow mode's comparisons are kept (DW-676): the tables of
//! `V028__shadow_comparisons.sql`, in the `telemetry` schema, beside the
//! artifact tables whose pool runs the same migrations.
//!
//! [`PostgresShadowSink`] takes each [`Comparison`] without ever waiting:
//! `record` hands it to a bounded channel, and a sample that does not fit
//! is dropped and counted. One writer task per sink writes them in batches,
//! each insert under a timeout, so a Postgres that hangs or fails costs
//! samples, counted, and nothing else. Every sample recorded ends as
//! written, dropped or failed ([`SinkCounts`]) while the process runs: a
//! sample still in the channel or in a batch being written when the
//! process stops is lost and counted nowhere.
//!
//! Every statement runs in a transaction of its own that Postgres gives up
//! after a limit, the statement and any wait for a lock alike
//! ([`bounded`]). A batch counted as failed is therefore not written later,
//! and a hung table costs the hub's retention task one pass, not all of
//! them.
//!
//! [`ShadowStore`] also keeps the tables to their windows, through the
//! hub's retention task: a store's day is rolled up into `shadow_daily`
//! before its comparisons go, a whole UTC day at a time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};
use tokio::sync::mpsc;

use crate::postgres::PostgresPool;
use crate::shadow::{Comparison, Outcome, ShadowSink};

/// Days a compared question is kept.
pub const COMPARISONS_RETENTION_DAYS: u32 = 14;
/// Days a store's daily rollup is kept.
pub const DAILY_RETENTION_DAYS: u32 = 180;
/// Days a reconcile's record is kept.
pub const RECONCILES_RETENTION_DAYS: u32 = 180;
/// Fixes kept by name in one reconcile's record; the rest are counted.
pub const FIXES_KEPT: usize = 200;
/// How long recording one reconcile may run, or wait for a lock.
pub const RECORD_LIMIT: Duration = Duration::from_secs(10);
/// Most comparisons written in one insert.
pub const SINK_BATCH: usize = 100;
/// How long one of retention's statements may run, or wait for a lock.
pub const RETENTION_STATEMENT_LIMIT: Duration = Duration::from_secs(60);
/// Connections the shadow tables' own pool holds: the sink's writer and
/// retention. Kept small: production's Postgres holds two hubs of about 58
/// connections each, and a third while one is deployed, within 200.
pub const POOL_SIZE: usize = 2;

/// The shadow tables of one Postgres.
pub struct ShadowStore {
    pool: PostgresPool,
    retention_limit: Duration,
}

impl ShadowStore {
    /// Make sure the tables exist (the telemetry migrations, on a pool
    /// whose `search_path` includes `telemetry`), then a ready store.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        Self::with_limit(pool, RETENTION_STATEMENT_LIMIT).await
    }

    /// As [`Self::with_pool`], retention's statements given up after
    /// `retention_limit`.
    pub async fn with_limit(pool: PostgresPool, retention_limit: Duration) -> Result<Arc<Self>> {
        crate::telemetry_postgres::run_telemetry_migrations(&pool)
            .await
            .context("ensuring the shadow tables exist")?;
        Ok(Arc::new(Self {
            pool,
            retention_limit,
        }))
    }

    /// A sink holding at most `capacity` samples not yet written, each
    /// batch's insert given up by Postgres after `insert_timeout`
    /// ([`bounded`]). Its writer runs on the current Tokio runtime until
    /// the sink is dropped.
    pub fn sink(&self, capacity: usize, insert_timeout: Duration) -> Arc<PostgresShadowSink> {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let counts = Arc::new(Counts::default());
        let writer = tokio::spawn(write(
            self.pool.clone(),
            rx,
            Arc::clone(&counts),
            insert_timeout,
        ));
        Arc::new(PostgresShadowSink {
            tx,
            counts,
            writer: writer.abort_handle(),
        })
    }

    /// The comparisons' retention pass, as of `before` (now less
    /// [`COMPARISONS_RETENTION_DAYS`]): every whole UTC day before today
    /// rolled up into `shadow_daily`, then the comparisons of the days
    /// before `before`'s own deleted, in bounded batches. Returns how many
    /// were deleted. A day before the cutoff already rolled up is not
    /// rolled up again: a pass that stopped part way through deleting it
    /// leaves its total as it was.
    pub fn roll_up_and_prune_comparisons(&self, before: DateTime<Utc>) -> Result<usize> {
        let cutoff_day = before.date_naive();
        let today = Utc::now().date_naive();
        block_on(self.roll_up(cutoff_day, today)).context("rolling up shadow comparisons")?;
        let cutoff = midnight(cutoff_day);
        let batch = crate::telemetry::PRUNE_BATCH as i64;
        crate::telemetry::prune_in_batches(|| {
            block_on(async {
                Ok(bounded(
                    &self.pool,
                    self.retention_limit,
                    "DELETE FROM shadow_comparisons WHERE id IN (
                         SELECT id FROM shadow_comparisons WHERE at < $1
                         ORDER BY at LIMIT $2 FOR UPDATE SKIP LOCKED)",
                    &[&cutoff, &batch],
                )
                .await? as usize)
            })
        })
    }

    /// The daily rollup's retention pass: the days before `before`'s own
    /// deleted. Returns how many.
    pub fn prune_daily(&self, before: DateTime<Utc>) -> Result<usize> {
        let cutoff_day = before.date_naive();
        let batch = crate::telemetry::PRUNE_BATCH as i64;
        crate::telemetry::prune_in_batches(|| {
            block_on(async {
                Ok(bounded(
                    &self.pool,
                    self.retention_limit,
                    "DELETE FROM shadow_daily WHERE (store, day) IN (
                         SELECT store, day FROM shadow_daily WHERE day < $1
                         ORDER BY day LIMIT $2 FOR UPDATE SKIP LOCKED)",
                    &[&cutoff_day, &batch],
                )
                .await? as usize)
            })
        })
    }

    /// Record one reconcile: its counts, and the first [`FIXES_KEPT`] of its
    /// fixes by name, the rest counted.
    pub async fn record_reconcile(&self, run: &ReconcileRun) -> Result<()> {
        let kept: Vec<serde_json::Value> = run
            .fixed
            .iter()
            .take(FIXES_KEPT)
            .map(|fix| {
                serde_json::json!({
                    "kind": fix.kind, "object_id": fix.object_id, "memory_id": fix.memory_id,
                })
            })
            .collect();
        let left_out = i32::try_from(run.fixed.len().saturating_sub(FIXES_KEPT))?;
        let count = |n: usize| i32::try_from(n);
        bounded(
            &self.pool,
            RECORD_LIMIT,
            "INSERT INTO shadow_reconciles (store, started_at, outcome, error, copied, updated,
                 dropped, unchanged, fixed, fixed_left_out)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &run.store,
                &run.started_at,
                &run.outcome.as_str(),
                &run.error,
                &count(run.copied)?,
                &count(run.updated)?,
                &count(run.dropped)?,
                &count(run.unchanged)?,
                &serde_json::Value::Array(kept),
                &left_out,
            ],
        )
        .await
        .context("recording a shadow reconcile")?;
        Ok(())
    }

    /// The reconciles' retention pass: those that finished before `before`
    /// deleted. Returns how many.
    pub fn prune_reconciles(&self, before: DateTime<Utc>) -> Result<usize> {
        let batch = crate::telemetry::PRUNE_BATCH as i64;
        crate::telemetry::prune_in_batches(|| {
            block_on(async {
                Ok(bounded(
                    &self.pool,
                    self.retention_limit,
                    "DELETE FROM shadow_reconciles WHERE id IN (
                         SELECT id FROM shadow_reconciles WHERE finished_at < $1
                         ORDER BY finished_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
                    &[&before, &batch],
                )
                .await? as usize)
            })
        })
    }

    /// Roll every store's days before `today` up into `shadow_daily`:
    /// inserted when new, updated only from `cutoff_day` on.
    ///
    /// Two sets of latency percentiles. Over the answered samples, both
    /// sides on the same questions. And for the exit gate (L1, design r2
    /// section 8), over the answered and the timed out, each timeout at its
    /// `shadow_ms`: about the deadline, a lower bound, so that a slow tail
    /// is counted rather than left out.
    async fn roll_up(&self, cutoff_day: NaiveDate, today: NaiveDate) -> Result<()> {
        let today = midnight(today);
        let mut latency = Vec::new();
        for (of, suffix) in [
            ("outcome = 'answered'", ""),
            ("outcome IN ('answered', 'timed_out')", "_with_timeouts"),
        ] {
            for side in ["primary", "shadow"] {
                for (p, name) in [("0.5", "p50"), ("0.95", "p95"), ("0.99", "p99")] {
                    latency.push((
                        format!("{side}_{name}_ms{suffix}"),
                        format!(
                            "percentile_cont({p}) WITHIN GROUP (ORDER BY {side}_ms) FILTER (WHERE {of})"
                        ),
                    ));
                }
            }
        }
        let columns: Vec<&str> = latency.iter().map(|(c, _)| c.as_str()).collect();
        let values: Vec<&str> = latency.iter().map(|(_, v)| v.as_str()).collect();
        let updates: Vec<String> = columns
            .iter()
            .map(|c| format!("{c} = EXCLUDED.{c}"))
            .collect();
        let sql = format!(
            "INSERT INTO shadow_daily AS d (
                 store, day, samples, answered, failed, panicked, timed_out, probes,
                 mean_overlap, mean_jaccard, mean_rbo, top1_rate,
                 empty_primary_only, empty_shadow_only, {columns}, rolled_up_at)
             SELECT store, (at AT TIME ZONE 'UTC')::date,
                 count(*),
                 count(*) FILTER (WHERE outcome = 'answered'),
                 count(*) FILTER (WHERE outcome = 'failed'),
                 count(*) FILTER (WHERE outcome = 'panicked'),
                 count(*) FILTER (WHERE outcome = 'timed_out'),
                 count(*) FILTER (WHERE probe),
                 avg(overlap) FILTER (WHERE outcome = 'answered'),
                 avg(jaccard) FILTER (WHERE outcome = 'answered'),
                 avg(rbo) FILTER (WHERE outcome = 'answered'),
                 avg(CASE WHEN top1 THEN 1.0 ELSE 0.0 END) FILTER (WHERE outcome = 'answered'),
                 count(*) FILTER (WHERE outcome = 'answered' AND empty_primary_only),
                 count(*) FILTER (WHERE outcome = 'answered' AND empty_shadow_only),
                 {values},
                 now()
             FROM shadow_comparisons
             WHERE at < $2
             GROUP BY 1, 2
             ON CONFLICT (store, day) DO UPDATE SET
                 samples = EXCLUDED.samples,
                 answered = EXCLUDED.answered,
                 failed = EXCLUDED.failed,
                 panicked = EXCLUDED.panicked,
                 timed_out = EXCLUDED.timed_out,
                 probes = EXCLUDED.probes,
                 mean_overlap = EXCLUDED.mean_overlap,
                 mean_jaccard = EXCLUDED.mean_jaccard,
                 mean_rbo = EXCLUDED.mean_rbo,
                 top1_rate = EXCLUDED.top1_rate,
                 empty_primary_only = EXCLUDED.empty_primary_only,
                 empty_shadow_only = EXCLUDED.empty_shadow_only,
                 {updates},
                 rolled_up_at = EXCLUDED.rolled_up_at
             WHERE d.day >= $1",
            columns = columns.join(", "),
            values = values.join(", "),
            updates = updates.join(", "),
        );
        bounded(
            &self.pool,
            self.retention_limit,
            &sql,
            &[&cutoff_day, &today],
        )
        .await?;
        Ok(())
    }
}

/// One reconcile of a shadowed store's replica, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRun {
    pub store: String,
    pub started_at: DateTime<Utc>,
    pub outcome: ReconcileOutcome,
    pub error: Option<String>,
    pub copied: usize,
    pub updated: usize,
    pub dropped: usize,
    pub unchanged: usize,
    /// Every object written to or dropped from the replica.
    pub fixed: Vec<ReconcileFix>,
}

/// One object a reconcile wrote to or dropped from the replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileFix {
    /// `copied`, `updated` or `dropped`.
    pub kind: &'static str,
    pub object_id: String,
    pub memory_id: Option<String>,
}

/// How a reconcile ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Ok,
    Failed,
    /// The replica has been promoted: it is written to now.
    RefusedPromoted,
    /// The store Weaviate serves is not yet the one the replica copies.
    RefusedUnswitched,
    /// The replica's tenant holds what it must not.
    RefusedTenant,
}

impl ReconcileOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::RefusedPromoted => "refused_promoted",
            Self::RefusedUnswitched => "refused_unswitched",
            Self::RefusedTenant => "refused_tenant",
        }
    }
}

/// The advisory lock over the memories' vellev replica (DW-676). Every
/// writer that copies from Weaviate into it (the reconcile, a page at a
/// time; the mirror's applier, a memory at a time) holds it shared, and an
/// erasure or a promotion holds it alone: an erasure then neither overlaps
/// a write decided on what was read before it, nor is undone by one after.
pub const MEMORY_REPLICA_LOCK: i64 = 0x0676_4d45_4d52_4550;

/// [`MEMORY_REPLICA_LOCK`], taken on connections of its own.
pub struct ReplicaLock {
    pool: PostgresPool,
}

impl ReplicaLock {
    /// The lock on `pool`'s Postgres, whose `search_path` holds the
    /// telemetry schema (the promotion record is there).
    pub fn new(pool: PostgresPool) -> Self {
        Self { pool }
    }

    /// Hold the lock shared: waiting at most `wait` for it, and given up by
    /// Postgres if the hold sits idle for `hold`. The hold says whether
    /// Postgres records memories as promoted.
    pub async fn shared(&self, wait: Duration, hold: Duration) -> Result<ReplicaHold> {
        self.take("pg_advisory_xact_lock_shared", wait, hold).await
    }

    /// Hold the lock alone, as [`Self::shared`].
    pub async fn exclusive(&self, wait: Duration, hold: Duration) -> Result<ReplicaHold> {
        self.take("pg_advisory_xact_lock", wait, hold).await
    }

    async fn take(&self, how: &str, wait: Duration, hold: Duration) -> Result<ReplicaHold> {
        let taking = async {
            // Held from the first: a hold given up part way, by the timeout
            // below or by its caller, closes its connection rather than
            // hand it back to the pool inside a transaction.
            let mut taken = ReplicaHold {
                conn: Some(self.pool.get().await?),
                promoted: false,
            };
            let conn = taken.conn.as_ref().expect("just taken");
            conn.batch_execute(&format!(
                "BEGIN; SET LOCAL lock_timeout = {}; SET LOCAL statement_timeout = {}; \
                 SET LOCAL idle_in_transaction_session_timeout = {}",
                wait.as_millis().max(1),
                wait.as_millis().max(1),
                hold.as_millis().max(1),
            ))
            .await?;
            conn.execute(&format!("SELECT {how}($1)"), &[&MEMORY_REPLICA_LOCK])
                .await
                .context("waiting for the memory replica's lock")?;
            taken.promoted = conn
                .query_one("SELECT EXISTS (SELECT 1 FROM shadow_memory_promotion)", &[])
                .await?
                .get(0);
            eyre::Ok(taken)
        };
        tokio::time::timeout(wait * 2 + Duration::from_secs(1), taking)
            .await
            .map_err(|_| eyre::eyre!("Postgres did not answer within {:?}", wait * 2))?
    }

    /// Record that memories have been promoted to `collection`; idempotent.
    pub async fn record_promotion(&self, collection: &str) -> Result<()> {
        bounded(
            &self.pool,
            RECORD_LIMIT,
            "INSERT INTO shadow_memory_promotion (collection) VALUES ($1) ON CONFLICT DO NOTHING",
            &[&collection],
        )
        .await?;
        Ok(())
    }
}

/// [`MEMORY_REPLICA_LOCK`] held, until [`Self::release`]. Dropped without
/// it, the connection is closed, and Postgres ends the transaction and
/// lets the lock go.
pub struct ReplicaHold {
    conn: Option<deadpool_postgres::Object>,
    /// Postgres records memories as promoted.
    pub promoted: bool,
}

impl ReplicaHold {
    /// Let the lock go. `Err` if Postgres gave it up before (the hold sat
    /// idle too long, or the connection failed): what was done under it
    /// may not have been kept apart.
    pub async fn release(mut self) -> Result<()> {
        let conn = self.conn.take().expect("held until released");
        match conn.batch_execute("COMMIT").await {
            Ok(()) => Ok(()),
            Err(e) => {
                drop(deadpool_postgres::Object::take(conn));
                Err(eyre::eyre!(
                    "the memory replica's lock was lost while held: {e}"
                ))
            }
        }
    }
}

impl Drop for ReplicaHold {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            drop(deadpool_postgres::Object::take(conn));
        }
    }
}

/// How long one of the memory mirror's statements may run, or wait for a
/// lock.
pub const MIRROR_NOTE_LIMIT: Duration = Duration::from_secs(10);
/// How much of why a memory could not be applied is kept with its note.
pub const MIRROR_ERROR_KEPT: usize = 500;

/// The memories noted for vellev's replica to take (DW-676, PR 7b): the
/// `memory_mirror_pending` list of `V031__memory_mirror_pending.sql`. Every
/// statement runs bounded ([`bounded`]), so a hung table costs a batch of
/// notes or a round of the applier, never a caller.
pub struct MirrorNotes {
    pool: PostgresPool,
}

/// A memory's note as the applier reads it before applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Noted {
    pub seq: i64,
    pub tries: i32,
}

/// A note due, as [`MirrorNotes::due`] gives it: also the cursor a round
/// passes back to go on from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueNote {
    pub id: String,
    pub next_try_at: Option<DateTime<Utc>>,
}

/// The pending list's size, for the gauges and promotion's guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Backlog {
    pub pending: i64,
    pub oldest: Option<DateTime<Utc>>,
}

impl MirrorNotes {
    /// Make sure the list exists (the telemetry migrations, on a pool whose
    /// `search_path` includes `telemetry`), then a ready list.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        crate::telemetry_postgres::run_telemetry_migrations(&pool)
            .await
            .context("ensuring the memory mirror's pending list exists")?;
        Ok(Arc::new(Self { pool }))
    }

    /// Note `ids`, each afresh: a new number, its tries forgotten, due now.
    pub async fn note(&self, ids: &[String]) -> Result<u64> {
        bounded(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "INSERT INTO memory_mirror_pending (memory_id)
             SELECT DISTINCT unnest($1::text[])
             ON CONFLICT (memory_id) DO UPDATE SET
                 noted_seq = nextval('memory_mirror_noted_seq'), noted_at = now(), tries = 0,
                 last_error = NULL, first_failed_at = NULL, next_try_at = NULL",
            &[&ids],
        )
        .await
        .context("noting memories for vellev's replica")
    }

    /// Up to `limit` memories due by `now`, in the list's order: those
    /// never yet failed first (a note's `next_try_at` is NULL until a try
    /// of it fails), then those due longest, each order broken by the id.
    /// `after` is where the caller has reached: only notes past it are
    /// given, so a round's statement stays the same size however far it
    /// has gone.
    pub async fn due(
        &self,
        now: DateTime<Utc>,
        after: Option<&DueNote>,
        limit: i64,
    ) -> Result<Vec<DueNote>> {
        let (from, at, id) = match after {
            None => (false, None, String::new()),
            Some(note) => (true, note.next_try_at, note.id.clone()),
        };
        let rows = bounded_query(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "SELECT memory_id, next_try_at FROM memory_mirror_pending
             WHERE (next_try_at IS NULL OR next_try_at <= $1)
               AND (NOT $2
                    OR ($3::timestamptz IS NULL
                        AND (next_try_at IS NOT NULL OR memory_id > $4))
                    OR ($3::timestamptz IS NOT NULL AND next_try_at IS NOT NULL
                        AND (next_try_at, memory_id) > ($3, $4)))
             ORDER BY next_try_at NULLS FIRST, memory_id LIMIT $5",
            &[&now, &from, &at, &id, &limit],
        )
        .await?;
        Ok(rows
            .iter()
            .map(|row| DueNote {
                id: row.get(0),
                next_try_at: row.get(1),
            })
            .collect())
    }

    /// `id`'s note, if it is noted.
    pub async fn seen(&self, id: &str) -> Result<Option<Noted>> {
        let rows = bounded_query(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "SELECT noted_seq, tries FROM memory_mirror_pending WHERE memory_id = $1",
            &[&id],
        )
        .await?;
        Ok(rows.first().map(|row| Noted {
            seq: row.get(0),
            tries: row.get(1),
        }))
    }

    /// Take `id`'s note away if it is still the one numbered `seq`: one
    /// made since is a later write's, and stays. Returns whether it went.
    pub async fn taken(&self, id: &str, seq: i64) -> Result<bool> {
        Ok(bounded(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "DELETE FROM memory_mirror_pending WHERE memory_id = $1 AND noted_seq = $2",
            &[&id, &seq],
        )
        .await?
            == 1)
    }

    /// Note `id` afresh after a try that failed, under a new number: what
    /// was sent may land late, over what another applier wrote since, and
    /// only a note still there puts that right. If the note was still the
    /// one numbered `seq`, the try is counted, with `error`, and the next
    /// put off to `next`; a noting made meanwhile is left due at once.
    pub async fn renoted(
        &self,
        id: &str,
        seq: i64,
        error: &str,
        now: DateTime<Utc>,
        next: DateTime<Utc>,
    ) -> Result<()> {
        let error: String = error.chars().take(MIRROR_ERROR_KEPT).collect();
        bounded(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "INSERT INTO memory_mirror_pending AS p (memory_id) VALUES ($1)
             ON CONFLICT (memory_id) DO UPDATE SET
                 noted_seq = nextval('memory_mirror_noted_seq'),
                 tries = CASE WHEN p.noted_seq = $2 THEN p.tries + 1 ELSE p.tries END,
                 last_error = CASE WHEN p.noted_seq = $2 THEN $3 ELSE p.last_error END,
                 first_failed_at = CASE WHEN p.noted_seq = $2
                     THEN coalesce(p.first_failed_at, $4) ELSE p.first_failed_at END,
                 next_try_at = CASE WHEN p.noted_seq = $2 THEN $5 ELSE p.next_try_at END",
            &[&id, &seq, &error, &now, &next],
        )
        .await
        .context("noting a memory afresh for vellev's replica")?;
        Ok(())
    }

    /// How many memories are noted, and since when the oldest.
    pub async fn backlog(&self) -> Result<Backlog> {
        let rows = bounded_query(
            &self.pool,
            MIRROR_NOTE_LIMIT,
            "SELECT count(*), min(noted_at) FROM memory_mirror_pending",
            &[],
        )
        .await?;
        let row = rows
            .first()
            .ok_or_else(|| eyre::eyre!("count gave no row"))?;
        Ok(Backlog {
            pending: row.get(0),
            oldest: row.get(1),
        })
    }
}

fn midnight(day: NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc()
}

/// Run `sql` in a transaction of its own that Postgres gives up after
/// `limit` (`statement_timeout` and `lock_timeout`, both `SET LOCAL`), so
/// that a statement given up is never committed later. The client gives up
/// too, after twice `limit` and a second, for a server that does not answer
/// at all. Returns the rows the statement touched.
async fn bounded(
    pool: &PostgresPool,
    limit: Duration,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<u64> {
    let ms = limit.as_millis().max(1);
    let run = async {
        let mut conn = pool.get().await?;
        let tx = conn.transaction().await?;
        tx.batch_execute(&format!(
            "SET LOCAL statement_timeout = {ms}; SET LOCAL lock_timeout = {ms}"
        ))
        .await?;
        let touched = tx.execute(sql, params).await?;
        tx.commit().await?;
        eyre::Ok(touched)
    };
    tokio::time::timeout(limit * 2 + Duration::from_secs(1), run)
        .await
        .map_err(|_| eyre::eyre!("Postgres did not answer within {:?}", limit * 2))?
}

/// [`bounded`] for a statement whose rows are wanted.
async fn bounded_query(
    pool: &PostgresPool,
    limit: Duration,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<Vec<tokio_postgres::Row>> {
    let ms = limit.as_millis().max(1);
    let run = async {
        let mut conn = pool.get().await?;
        let tx = conn.transaction().await?;
        tx.batch_execute(&format!(
            "SET LOCAL statement_timeout = {ms}; SET LOCAL lock_timeout = {ms}"
        ))
        .await?;
        let rows = tx.query(sql, params).await?;
        tx.commit().await?;
        eyre::Ok(rows)
    };
    tokio::time::timeout(limit * 2 + Duration::from_secs(1), run)
        .await
        .map_err(|_| eyre::eyre!("Postgres did not answer within {:?}", limit * 2))?
}

/// Runs `fut` to its end from synchronous code on a runtime worker, as the
/// Postgres telemetry store does for the retention task.
fn block_on<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

#[derive(Debug, Default)]
struct Counts {
    recorded: AtomicU64,
    dropped: AtomicU64,
    written: AtomicU64,
    failed: AtomicU64,
}

/// What a sink has done: every sample `recorded` is in time `written`,
/// `dropped` (no room, or the writer gone) or `failed` (its insert failed
/// or was given up).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkCounts {
    pub recorded: u64,
    pub dropped: u64,
    pub written: u64,
    pub failed: u64,
}

/// A [`ShadowSink`] writing to `shadow_comparisons`, never waiting.
pub struct PostgresShadowSink {
    tx: mpsc::Sender<Comparison>,
    counts: Arc<Counts>,
    /// The writer, stopped with the sink.
    writer: tokio::task::AbortHandle,
}

impl Drop for PostgresShadowSink {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

impl PostgresShadowSink {
    pub fn counts(&self) -> SinkCounts {
        let c = &*self.counts;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SinkCounts {
            recorded: get(&c.recorded),
            dropped: get(&c.dropped),
            written: get(&c.written),
            failed: get(&c.failed),
        }
    }
}

impl ShadowSink for PostgresShadowSink {
    fn record(&self, comparison: Comparison) {
        self.counts.recorded.fetch_add(1, Ordering::Relaxed);
        // No room, or the writer gone: dropped either way, never waited for.
        if self.tx.try_send(comparison).is_err() {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The writer: a batch of what is waiting, up to [`SINK_BATCH`], in one
/// insert under `insert_timeout`, until the sink is dropped.
async fn write(
    pool: PostgresPool,
    mut rx: mpsc::Receiver<Comparison>,
    counts: Arc<Counts>,
    insert_timeout: Duration,
) {
    let mut batch = Vec::with_capacity(SINK_BATCH);
    while rx.recv_many(&mut batch, SINK_BATCH).await > 0 {
        let n = batch.len() as u64;
        match insert(&pool, &batch, insert_timeout).await {
            Ok(()) => {
                counts.written.fetch_add(n, Ordering::Relaxed);
            }
            Err(e) => {
                counts.failed.fetch_add(n, Ordering::Relaxed);
                tracing::warn!(samples = n, error = %format!("{e:#}"), "writing shadow comparisons failed");
            }
        }
        batch.clear();
    }
}

/// One row's values, in the order of [`COLUMNS`].
struct Row {
    store: &'static str,
    outcome: &'static str,
    error: Option<String>,
    probe: bool,
    query_hash: Option<String>,
    labels: serde_json::Value,
    primary_ms: f64,
    primary_embed_ms: Option<f64>,
    primary_count: i32,
    primary_top_score: Option<f32>,
    shadow_ms: f64,
    shadow_embed_ms: Option<f64>,
    shadow_count: Option<i32>,
    shadow_top_score: Option<f32>,
    k: Option<i32>,
    overlap: Option<f64>,
    jaccard: Option<f64>,
    rbo: Option<f64>,
    top1: Option<bool>,
    empty_primary_only: Option<bool>,
    empty_shadow_only: Option<bool>,
}

const COLUMNS: &str = "store, outcome, error, probe, query_hash, labels, \
     primary_ms, primary_embed_ms, primary_count, primary_top_score, \
     shadow_ms, shadow_embed_ms, shadow_count, shadow_top_score, \
     k, overlap, jaccard, rbo, top1, empty_primary_only, empty_shadow_only";
const WIDTH: usize = 21;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

impl Row {
    fn of(c: &Comparison) -> Self {
        let mut labels: serde_json::Map<String, serde_json::Value> = c
            .primary
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone())))
            .collect();
        // Where the shadow's time went (DW-676, PR 9b), kept in `labels`
        // rather than columns of their own: numbers, not strings. An index
        // time is left out, never written as 0, when there is none.
        labels.insert("shadow_queue_ms".into(), ms(c.queued).into());
        labels.insert("concurrent".into(), c.concurrent.into());
        if let Some(index) = c.index {
            labels.insert("shadow_index_ms".into(), ms(index).into());
        }
        let labels = serde_json::Value::Object(labels);
        let mut row = Self {
            store: c.store,
            outcome: "answered",
            error: None,
            probe: c.probe,
            query_hash: c.primary.query_hash.clone(),
            labels,
            primary_ms: ms(c.primary.latency),
            primary_embed_ms: c.primary.ranked.embed.map(ms),
            primary_count: count(c.primary.ranked.ids.len()),
            primary_top_score: c.primary.ranked.top_score,
            shadow_ms: ms(c.shadow_latency),
            shadow_embed_ms: None,
            shadow_count: None,
            shadow_top_score: None,
            k: None,
            overlap: None,
            jaccard: None,
            rbo: None,
            top1: None,
            empty_primary_only: None,
            empty_shadow_only: None,
        };
        match &c.outcome {
            Outcome::Answered { ranked, metrics } => {
                row.shadow_embed_ms = ranked.embed.map(ms);
                row.shadow_count = Some(count(ranked.ids.len()));
                row.shadow_top_score = ranked.top_score;
                row.k = Some(count(metrics.k));
                row.overlap = Some(metrics.overlap);
                row.jaccard = Some(metrics.jaccard);
                row.rbo = Some(metrics.rbo);
                row.top1 = Some(metrics.top1);
                row.empty_primary_only = Some(metrics.empty_primary_only);
                row.empty_shadow_only = Some(metrics.empty_shadow_only);
            }
            Outcome::Failed(error) => {
                row.outcome = "failed";
                row.error = Some(error.clone());
            }
            Outcome::Panicked => row.outcome = "panicked",
            Outcome::TimedOut => row.outcome = "timed_out",
        }
        row
    }

    fn values(&self) -> [&(dyn tokio_postgres::types::ToSql + Sync); WIDTH] {
        [
            &self.store,
            &self.outcome,
            &self.error,
            &self.probe,
            &self.query_hash,
            &self.labels,
            &self.primary_ms,
            &self.primary_embed_ms,
            &self.primary_count,
            &self.primary_top_score,
            &self.shadow_ms,
            &self.shadow_embed_ms,
            &self.shadow_count,
            &self.shadow_top_score,
            &self.k,
            &self.overlap,
            &self.jaccard,
            &self.rbo,
            &self.top1,
            &self.empty_primary_only,
            &self.empty_shadow_only,
        ]
    }
}

/// Write `batch` in one insert, given up after `limit` ([`bounded`]).
async fn insert(pool: &PostgresPool, batch: &[Comparison], limit: Duration) -> Result<()> {
    let rows: Vec<Row> = batch.iter().map(Row::of).collect();
    let placeholders: Vec<String> = (0..rows.len())
        .map(|r| {
            let cells: Vec<String> = (1..=WIDTH).map(|c| format!("${}", r * WIDTH + c)).collect();
            format!("({})", cells.join(", "))
        })
        .collect();
    let sql = format!(
        "INSERT INTO shadow_comparisons ({COLUMNS}) VALUES {}",
        placeholders.join(", ")
    );
    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        rows.iter().flat_map(|row| row.values()).collect();
    bounded(pool, limit, &sql, &params).await?;
    Ok(())
}

mod known_items;
pub use known_items::*;

#[cfg(test)]
mod tests;

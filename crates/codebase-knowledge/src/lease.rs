//! Persistent indexing lease.
//!
//! Replaces the in-process `DashSet` "currently indexing" flag with a
//! SQLite-backed record that survives restarts. Each lease carries:
//!
//! - the **worker id** of the process that owns it (UUID per hub start),
//! - the **target commit** the run is indexing toward,
//! - the **phase** the run is currently in,
//! - **progress counters** so observers can chart the long phases,
//! - a **heartbeat timestamp** refreshed on a timer.
//!
//! A lease is *fresh* if its heartbeat is within
//! [`STALE_THRESHOLD`]; a *stale* lease is the signature of a worker that
//! crashed or was killed, and the next acquisition steals it. Because
//! every phase of the pipeline is idempotent — entity upserts, vector
//! upserts, and summary generation that skips entries with up-to-date
//! `content_hash` — resuming after a steal converges to the right
//! state without manual intervention.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::graph::CodeGraph;

/// How often the heartbeat task refreshes `last_heartbeat_at_ms`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Heartbeats older than this mark a lease as stale and stealable.
/// Three intervals gives genuine work two missed beats of slack before
/// another acquirer concludes the original worker is gone.
pub const STALE_THRESHOLD: Duration = Duration::from_secs(90);

/// Human-readable phase names persisted on the lease row. Treated as
/// opaque strings by the schema; the strongly-typed enum lives in the
/// pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    Crawling,
    Parsing,
    Committing,
    VectorIndexing,
    GeneratingSummaries,
}

impl IndexPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crawling => "crawling",
            Self::Parsing => "parsing",
            Self::Committing => "committing",
            Self::VectorIndexing => "vector_indexing",
            Self::GeneratingSummaries => "generating_summaries",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "crawling" => Some(Self::Crawling),
            "parsing" => Some(Self::Parsing),
            "committing" => Some(Self::Committing),
            "vector_indexing" => Some(Self::VectorIndexing),
            "generating_summaries" => Some(Self::GeneratingSummaries),
            _ => None,
        }
    }
}

/// Snapshot of the persisted lease row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexLease {
    pub repo_id: String,
    pub worker_id: String,
    pub target_commit: Option<String>,
    pub phase: IndexPhase,
    pub acquired_at_ms: i64,
    pub last_heartbeat_at_ms: i64,
    pub entities_total: Option<u64>,
    pub entities_processed: Option<u64>,
    pub summaries_total: Option<u64>,
    pub summaries_processed: Option<u64>,
}

impl IndexLease {
    /// Whether the heartbeat is recent enough that another worker should
    /// treat this lease as live (rather than stealing it).
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        let age_ms = now_ms.saturating_sub(self.last_heartbeat_at_ms);
        age_ms >= 0 && (age_ms as u128) < STALE_THRESHOLD.as_millis()
    }
}

/// Outcome of an `acquire` attempt.
#[derive(Debug)]
pub enum AcquireOutcome {
    /// The caller now owns the lease.
    Acquired,
    /// Another worker holds a fresh lease. The caller should not start.
    Held(IndexLease),
}

/// Convenience: "now" in unix milliseconds, the unit the schema uses.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Mutable progress counters reported through a [`LeaseContext`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LeaseProgress {
    pub entities_total: Option<u64>,
    pub entities_processed: Option<u64>,
    pub summaries_total: Option<u64>,
    pub summaries_processed: Option<u64>,
}

/// Bridge between the indexing pipeline and the lease row in storage.
///
/// The pipeline calls `set_phase` at phase boundaries and `set_progress`
/// during long phases; both calls update the live lease row immediately.
/// In parallel, [`spawn_heartbeat`] runs a background task that refreshes
/// the heartbeat timestamp on a fixed cadence — so a phase that takes
/// hours (e.g. summary generation against a huge repo) doesn't go stale.
pub struct LeaseContext {
    graph: Arc<CodeGraph>,
    repo_id: String,
    worker_id: String,
    phase: RwLock<IndexPhase>,
    progress: RwLock<LeaseProgress>,
}

impl LeaseContext {
    pub fn new(
        graph: Arc<CodeGraph>,
        repo_id: String,
        worker_id: String,
        initial_phase: IndexPhase,
    ) -> Arc<Self> {
        Arc::new(Self {
            graph,
            repo_id,
            worker_id,
            phase: RwLock::new(initial_phase),
            progress: RwLock::new(LeaseProgress::default()),
        })
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn repo_id(&self) -> &str {
        &self.repo_id
    }

    /// Transition to a new phase and persist immediately.
    pub async fn set_phase(&self, phase: IndexPhase) {
        *self.phase.write().await = phase;
        self.flush().await;
    }

    /// Update progress counters and persist immediately. Callers can
    /// pass `None` for fields they don't want to overwrite.
    pub async fn set_progress(&self, update: LeaseProgress) {
        {
            let mut p = self.progress.write().await;
            p.entities_total = update.entities_total.or(p.entities_total);
            p.entities_processed = update.entities_processed.or(p.entities_processed);
            p.summaries_total = update.summaries_total.or(p.summaries_total);
            p.summaries_processed = update.summaries_processed.or(p.summaries_processed);
        }
        self.flush().await;
    }

    /// Push the current phase + progress to storage as a heartbeat.
    /// Logs (without surfacing) lease-write failures so a transient DB
    /// glitch doesn't tear down the indexing run.
    async fn flush(&self) {
        let phase = *self.phase.read().await;
        let p = *self.progress.read().await;
        if let Err(e) = self
            .graph
            .heartbeat_lease(
                &self.repo_id,
                &self.worker_id,
                phase,
                p.entities_total,
                p.entities_processed,
                p.summaries_total,
                p.summaries_processed,
            )
            .await
        {
            warn!(
                repo_id = %self.repo_id,
                error = %e,
                "Lease heartbeat failed"
            );
        }
    }

    /// Spawn the periodic heartbeat task. The returned [`HeartbeatGuard`]
    /// aborts the task on drop, so a panic or cancellation in the
    /// caller cleanly stops heartbeats — the lease will then become
    /// stale within `STALE_THRESHOLD` and another worker can steal it.
    pub fn spawn_heartbeat(self: &Arc<Self>) -> HeartbeatGuard {
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                this.flush().await;
            }
        });
        HeartbeatGuard(handle)
    }
}

/// Drop-safe wrapper around the heartbeat task's [`JoinHandle`]. Aborts
/// the task when dropped — required for the panic / cancellation
/// recovery story to work, since neither path runs explicit cleanup
/// code in the caller.
pub struct HeartbeatGuard(JoinHandle<()>);

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

//! The write mirror of memories while they are shadowed (DW-676, PR 7b):
//! each memory written through the hub is noted, and the leader's applier
//! brings vellev's replica in line with Weaviate about it soon after,
//! rather than at the next reconcile.
//!
//! - **The noter.** After Weaviate took a write, [`Noter::note`] hands the
//!   memory's id to a bounded channel with `try_send`. A full channel is
//!   counted ([`NoteCounts::dropped`]) and nothing waits: the user's write
//!   never waits on the shadow. One task drains the channel and writes the
//!   ids, deduped and in batches, to `memory_mirror_pending` (V031).
//! - **The applier.** On the leader alone it works the noted memories:
//!   [`sync_memory`] of each, repeated until a pass finds nothing to write
//!   (two reads agree, as the artifact keeper's `settle`); the note taken
//!   away only at the number read before, so a noting made meanwhile stays;
//!   one that could not be applied noted afresh, under a new number, its
//!   next try put off. When leadership moves and two appliers meet, the
//!   newer state wins: a late write of the older lands before the other's
//!   last read, which then writes the newer again, or after its note was
//!   renewed, which is then applied again.
//! - **The replica's lock.** Each pass holds it shared ([`WriteTurns`]), so
//!   an erasure, which holds it alone, never overlaps one. A pass is never
//!   cut short by a timeout of its own: dropped while held, it would let
//!   the lock go while a write it sent could still land, after an erasure
//!   that took the lock meanwhile, and so bring an erased memory back. Each
//!   call a pass makes is bounded by its client's own timeouts, and the
//!   hold by Postgres.
//! - **The kill switch** (`[vellev.shadow] paused`) stops the noter and the
//!   applier; never an erasure. Unlike the artifact mirror, whose writes
//!   are noted while paused because nothing else would find them, memories
//!   are reconciled whole once unpaused, so notes taken while paused would
//!   add nothing.
//!
//! Convergence rests on the reconcile (`memory_shadow`), not on this: a
//! note dropped, lost with a hub that stopped before writing it, or never
//! made (a CLI process, which notes nothing; a memory remembered under an
//! identity between a reassign's resolving its memories and the reassign
//! itself) is found by the next run. This is for lag.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use workforce_common::shadow_postgres::{DueNote, MirrorNotes, Noted};
use workforce_memory::admin::{sync_memory, Promoted, SyncSource, SyncTarget, WriteTurns};
use workforce_memory::types::MemoryId;

/// Notes the channel holds before one is dropped.
pub const NOTE_CAPACITY: usize = 4096;
/// Most memories noted in one statement.
pub const NOTE_BATCH: usize = 256;
/// How long a noter is given to make its pending list ready (a pool, the
/// telemetry migrations) before it gives up for good.
pub const NOTE_SETUP_LIMIT: Duration = Duration::from_secs(30);
/// Connections a pending list's own pool holds (the noter's, and the
/// applier's): its statements take one at a time, and making it ready (the
/// migrations) takes two.
pub const NOTES_POOL_SIZE: usize = 2;
/// How often the applier looks for noted memories.
pub const APPLY_WATCH: Duration = Duration::from_secs(2);
/// Passes of one memory before it is given up as changing too fast, and
/// noted afresh.
pub const SETTLE_PASSES: usize = 5;
/// Memories read from the list at a time.
pub const APPLY_PAGE: i64 = 100;
/// Failures in a row that end a round, the rest left as they are.
pub const ROUND_ENDS_AFTER: usize = 3;
/// The wait after a memory's first failed try, doubled each try after.
pub const BACKOFF: Duration = Duration::from_secs(2);
/// The longest wait between two tries of a memory.
pub const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// What is noted: a memory, or every memory a bulk change touched, which
/// takes one place in the channel however many it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    One(MemoryId),
    Many(Vec<MemoryId>),
}

impl Note {
    fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(ids) => ids.len(),
        }
    }
}

/// What a noter has done with the memories it was given, while the
/// process runs.
#[derive(Debug, Default)]
pub struct NoteCounts {
    /// Not noted: no room in the channel, or none resolved for a bulk
    /// change. The reconcile finds them.
    pub dropped: AtomicU64,
    /// Written to the list.
    pub written: AtomicU64,
    /// Given to the list and not written: Postgres failed or hung.
    pub lost: AtomicU64,
    /// 1 once the noter's pending list could not be made ready: it is not
    /// tried again while the process runs, and every note is dropped.
    pub setup_failed: AtomicU64,
}

/// Where the noter writes.
#[async_trait]
pub trait NoteSink: Send + Sync {
    async fn note(&self, ids: &[String]) -> Result<()>;
}

#[async_trait]
impl NoteSink for MirrorNotes {
    async fn note(&self, ids: &[String]) -> Result<()> {
        MirrorNotes::note(self, ids).await.map(|_| ())
    }
}

/// Hands memories written to the noter's task; never waits.
#[derive(Clone)]
pub struct Noter {
    tx: mpsc::Sender<Note>,
    counts: Arc<NoteCounts>,
}

impl Noter {
    /// Note `note` if there is room; count it dropped if not.
    pub fn note(&self, note: Note) {
        let n = note.len();
        if n == 0 {
            return;
        }
        if let Err(e) = self.tx.try_send(note) {
            self.dropped(n);
            let why = match e {
                mpsc::error::TrySendError::Full(_) => "the channel is full",
                mpsc::error::TrySendError::Closed(_) => "the noter has stopped",
            };
            tracing::warn!(target: "shadow", dropped = n, why, "memories written not noted for vellev's replica; the reconcile will find them");
        }
    }

    /// Count `n` memories written and never noted.
    pub fn dropped(&self, n: usize) {
        self.counts.dropped.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn counts(&self) -> Arc<NoteCounts> {
        Arc::clone(&self.counts)
    }
}

/// A noter writing to `sink`, `capacity` notes held, at most `batch`
/// memories a statement. Its task runs on the current runtime until every
/// [`Noter`] made from it is dropped.
pub fn spawn_noter(sink: Arc<dyn NoteSink>, capacity: usize, batch: usize) -> Noter {
    spawn_noter_after(
        std::future::ready(Ok(sink)),
        NOTE_SETUP_LIMIT,
        capacity,
        batch,
    )
}

/// A noter whose sink is made by `setup`, in its own task: whoever asks
/// for the noter never waits on it. Notes given meanwhile wait in the
/// channel (or are dropped when it is full). A `setup` that fails, or does
/// not finish within `limit`, is final: said once, counted
/// ([`NoteCounts::setup_failed`]), never tried again, and every note after
/// is dropped for the reconcile to find.
pub fn spawn_noter_after<F>(setup: F, limit: Duration, capacity: usize, batch: usize) -> Noter
where
    F: std::future::Future<Output = Result<Arc<dyn NoteSink>>> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Note>(capacity);
    let counts = Arc::new(NoteCounts::default());
    let counting = Arc::clone(&counts);
    tokio::spawn(async move {
        let failed = match tokio::time::timeout(limit, setup).await {
            Ok(Ok(sink)) => Ok(sink),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err(format!("not ready within {limit:?}")),
        };
        let sink = match failed {
            Ok(sink) => sink,
            Err(why) => {
                counting.setup_failed.store(1, Ordering::Relaxed);
                tracing::error!(target: "shadow", error = %why, "memories written are not noted for vellev's replica while this process runs: the reconcile alone keeps it");
                while let Some(note) = rx.recv().await {
                    counting
                        .dropped
                        .fetch_add(note.len() as u64, Ordering::Relaxed);
                }
                return;
            }
        };
        while let Some(first) = rx.recv().await {
            let mut ids = BTreeSet::new();
            let take = |ids: &mut BTreeSet<String>, note: Note| match note {
                Note::One(id) => {
                    ids.insert(id.0);
                }
                Note::Many(many) => ids.extend(many.into_iter().map(|id| id.0)),
            };
            take(&mut ids, first);
            while ids.len() < batch {
                match rx.try_recv() {
                    Ok(note) => take(&mut ids, note),
                    Err(_) => break,
                }
            }
            let ids: Vec<String> = ids.into_iter().collect();
            for chunk in ids.chunks(batch.max(1)) {
                match sink.note(chunk).await {
                    Ok(()) => counting
                        .written
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed),
                    Err(e) => {
                        tracing::error!(target: "shadow", lost = chunk.len(), error = %format!("{e:#}"), "memories written could not be noted for vellev's replica; the reconcile will find them");
                        counting
                            .lost
                            .fetch_add(chunk.len() as u64, Ordering::Relaxed)
                    }
                };
            }
        }
    });
    Noter { tx, counts }
}

/// The noted memories as the applier works them.
#[async_trait]
pub trait PendingNotes: Send + Sync {
    /// Up to `limit` notes due by `now`, in the list's order, after the
    /// note `after` (a cursor: what a round asks never grows).
    async fn due(
        &self,
        now: DateTime<Utc>,
        after: Option<&DueNote>,
        limit: i64,
    ) -> Result<Vec<DueNote>>;
    async fn seen(&self, id: &str) -> Result<Option<Noted>>;
    async fn taken(&self, id: &str, seq: i64) -> Result<bool>;
    async fn renoted(
        &self,
        id: &str,
        seq: i64,
        error: &str,
        now: DateTime<Utc>,
        next: DateTime<Utc>,
    ) -> Result<()>;
}

#[async_trait]
impl PendingNotes for MirrorNotes {
    async fn due(
        &self,
        now: DateTime<Utc>,
        after: Option<&DueNote>,
        limit: i64,
    ) -> Result<Vec<DueNote>> {
        MirrorNotes::due(self, now, after, limit).await
    }
    async fn seen(&self, id: &str) -> Result<Option<Noted>> {
        MirrorNotes::seen(self, id).await
    }
    async fn taken(&self, id: &str, seq: i64) -> Result<bool> {
        MirrorNotes::taken(self, id, seq).await
    }
    async fn renoted(
        &self,
        id: &str,
        seq: i64,
        error: &str,
        now: DateTime<Utc>,
        next: DateTime<Utc>,
    ) -> Result<()> {
        MirrorNotes::renoted(self, id, seq, error, now, next).await
    }
}

/// The pending list on `[postgres]`, on a pool of one of its own (the
/// shadow tables' pool is the sink's and retention's). Owns what it needs
/// of `config`, so that it can be made in a task of its own.
pub fn notes(
    config: &workforce_common::settings::WorkforceConfig,
) -> impl std::future::Future<Output = Result<Arc<MirrorNotes>>> + Send + 'static {
    use eyre::Context;
    use workforce_common::postgres::PostgresConfig;
    use workforce_common::telemetry_postgres::PostgresTelemetryStore;
    let pg = config.postgres.clone().filter(|p| p.enabled);
    async move {
        let pg = pg.ok_or_else(|| {
            eyre::eyre!("[memory] backend = \"shadow\" needs [postgres] enabled = true")
        })?;
        // Two: making the list ready runs the telemetry migrations, which
        // hold one connection for their lock and ask for another; with one,
        // that would wait for ever.
        let pool = PostgresTelemetryStore::build_pool(&PostgresConfig {
            pool_size: NOTES_POOL_SIZE,
            ..pg
        })
        .context("building the memory mirror's pool")?;
        MirrorNotes::with_pool(pool).await
    }
}

/// The applier's turns of the replica's lock: held shared, a memory at a
/// time, one after another with no pace between them.
pub fn applier_turns(
    lock: Arc<workforce_common::shadow_postgres::ReplicaLock>,
) -> Arc<dyn WriteTurns> {
    Arc::new(crate::memory_shadow::ReplicaTurns::new(
        lock,
        Duration::ZERO,
        crate::memory_shadow::LOCK_WAIT,
        crate::memory_shadow::LOCK_HOLD,
    ))
}

/// When it is, for the applier's backoff: given, so that a test can say.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// How one memory's try came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tried {
    /// The replica agrees with Weaviate about it.
    Applied,
    /// Not applied; noted afresh.
    Failed,
    /// No longer noted: another applier took it.
    NotPending,
    /// The replica is promoted: nothing is written into it, and the note
    /// stays for promotion's guard to see.
    Refused,
}

/// What a round did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Worked {
    pub applied: usize,
    pub failed: usize,
    pub refused: bool,
}

/// The leader's applier of noted memories.
pub struct Applier {
    pub replica: Arc<dyn SyncTarget>,
    pub serving: Arc<dyn SyncSource>,
    pub notes: Arc<dyn PendingNotes>,
    pub turns: Arc<dyn WriteTurns>,
    pub clock: Clock,
}

impl Applier {
    /// Bring the replica in line with Weaviate about `id`: synced until a
    /// pass finds nothing to write, so that a write landing between one
    /// pass's read and its own (another applier's, late) is put right by
    /// the next. `Err` if it never settles within [`SETTLE_PASSES`].
    async fn settle(&self, id: &str) -> Result<()> {
        for _ in 0..SETTLE_PASSES {
            let report = sync_memory(
                self.replica.as_ref(),
                self.serving.as_ref(),
                id,
                self.turns.as_ref(),
            )
            .await?;
            if report.copied + report.updated + report.dropped == 0 {
                return Ok(());
            }
        }
        Err(eyre::eyre!(
            "memory '{id}' kept changing while vellev's replica took it"
        ))
    }

    /// One try at one noted memory: the only way the applier writes.
    pub async fn try_one(&self, id: &str) -> Result<Tried> {
        let Some(Noted { seq, tries }) = self.notes.seen(id).await? else {
            return Ok(Tried::NotPending);
        };
        let failed = match self.settle(id).await {
            Ok(()) => {
                self.notes.taken(id, seq).await?;
                return Ok(Tried::Applied);
            }
            Err(e) if e.downcast_ref::<Promoted>().is_some() => return Ok(Tried::Refused),
            Err(e) => e,
        };
        tracing::warn!(target: "shadow", id, tries = tries + 1, error = %format!("{failed:#}"), "memory not applied to vellev's replica; it is tried again");
        let now = (self.clock)();
        let wait = workforce_common::backoff::jittered(BACKOFF, BACKOFF_CAP, id, tries + 1);
        let next = now + chrono::Duration::from_std(wait)?;
        self.notes
            .renoted(id, seq, &format!("{failed:#}"), now, next)
            .await?;
        Ok(Tried::Failed)
    }

    /// Work the noted memories that are due, each tried once. After
    /// [`ROUND_ENDS_AFTER`] failures in a row, or on a promoted replica,
    /// the round ends; and between two memories once `drain` is cancelled
    /// (never during one: see the module doc).
    pub async fn work_due(&self, drain: &CancellationToken) -> Result<Worked> {
        // Where the round has reached in the list, and what is due in it,
        // fixed at its start: each memory is tried at most once a round. A
        // try that fails puts its next one after `now`, and a note renewed
        // meanwhile sorts before the cursor; either waits for the next round.
        let now = (self.clock)();
        let mut after: Option<DueNote> = None;
        let mut worked = Worked::default();
        let mut in_a_row = 0;
        loop {
            let page = self.notes.due(now, after.as_ref(), APPLY_PAGE).await?;
            if page.is_empty() {
                return Ok(worked);
            }
            for due in page {
                let id = due.id.clone();
                if drain.is_cancelled() {
                    return Ok(worked);
                }
                match self.try_one(&id).await? {
                    Tried::Applied => {
                        worked.applied += 1;
                        in_a_row = 0;
                    }
                    Tried::Failed => {
                        worked.failed += 1;
                        in_a_row += 1;
                    }
                    Tried::NotPending => {}
                    Tried::Refused => {
                        worked.refused = true;
                        tracing::error!(target: "shadow", "vellev's replica of memories is promoted: noted memories are not applied");
                        return Ok(worked);
                    }
                }
                after = Some(due);
                if in_a_row == ROUND_ENDS_AFTER {
                    return Ok(worked);
                }
            }
        }
    }
}

/// Apply noted memories every `every`, on the leader only (`leader`:
/// `None` for a hub alone), never while `paused`, until `drain`.
pub fn spawn_applier(
    applier: Arc<Applier>,
    every: Duration,
    paused: Arc<AtomicBool>,
    leader: Option<Arc<AtomicBool>>,
    drain: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = drain.cancelled() => return,
                _ = ticks.tick() => {}
            }
            if paused.load(Ordering::SeqCst) {
                continue;
            }
            if !leader.as_ref().is_none_or(|l| l.load(Ordering::SeqCst)) {
                continue;
            }
            // Not raced against `drain`: a round dropped part way would
            // drop a pass holding the replica's lock (see the module doc).
            if let Err(e) = applier.work_due(&drain).await {
                tracing::error!(target: "shadow", error = %format!("{e:#}"), "a round of applying memories to vellev's replica failed");
            }
        }
    })
}

#[cfg(test)]
#[path = "memory_mirror_tests.rs"]
mod tests;

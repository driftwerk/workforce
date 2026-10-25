//! Shadow mode for memories (DW-676): vellev kept a replica of the
//! Weaviate collection that serves them, by a reconcile the fleet's leader
//! runs at a fixed interval (`[vellev.shadow] memory_reconcile_secs`).
//!
//! The reconcile is what makes the replica converge: whatever the write
//! mirror misses (a full channel, a crash, a CLI or MCP write that does not
//! go through it, a decay job), the next run copies, updates or drops. Each
//! run records what it had to fix, memory by memory, so that every fix can
//! be traced to how it was missed (`shadow_reconciles`).
//!
//! It never runs while the shadow is paused, and never once vellev has
//! been promoted, by vellev's marker or by Postgres's record of it. Each
//! page it reads and writes is a turn of the replica's lock, held shared,
//! so that an erasure, which holds it alone, neither overlaps a write
//! decided on what was read before it nor is undone by one after.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{eyre, Context, Result};
use tokio_util::sync::CancellationToken;
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::{WorkforceConfig, SHADOWED};
use workforce_common::shadow_postgres::{
    ReconcileFix, ReconcileOutcome, ReconcileRun, ReplicaHold, ReplicaLock, ShadowStore,
};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;
use workforce_memory::admin::{
    sync_recorded, MemoryAdmin, Promoted, Switchable, SyncMode, SyncTarget, Synced, Turn,
    WriteTurns,
};
use workforce_memory::vellev::VellevStore;

/// The store a run is recorded under.
pub const STORE: &str = "memories";
/// Time for the leader election to be wired before the first run.
const LEADER_WIRING: Duration = Duration::from_secs(20);
/// The pause between two pages of a run: Weaviate serves users.
pub const PAGE_PACE: Duration = Duration::from_millis(100);
/// How long a page waits for the replica's lock (an erasure holding it).
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
/// How long a page may hold the lock before Postgres gives it up: past
/// every call a page makes, each bounded by its client's own timeouts.
pub const LOCK_HOLD: Duration = Duration::from_secs(30 * 60);
/// Connections the replica's lock holds: a page of the reconcile, a memory
/// of the mirror's applier, and an erasure.
const LOCK_POOL_SIZE: usize = 3;

/// Why a run was not made.
#[derive(Debug)]
enum Refused {
    /// The store Weaviate serves is not yet the collection the replica
    /// copies: memories have not switched to the hub's embeddings.
    Unswitched(String),
    /// The replica's tenant holds the collection an import from Weaviate
    /// brings, which nothing here keeps or erases.
    Tenant(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unswitched(collection) => write!(
                f,
                "Weaviate does not yet serve memories from {collection}, the collection the replica copies: memories have not switched to the hub's embeddings"
            ),
            Self::Tenant(collection) => write!(
                f,
                "the replica's vellev tenant holds {collection}, as an import from Weaviate makes it: nothing keeps it in step or erases from it. Drop it from the tenant; the replica is seeded by the reconcile alone"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// Where runs are recorded.
#[async_trait]
pub trait Recorder: Send + Sync {
    async fn record(&self, run: &ReconcileRun) -> Result<()>;
}

#[async_trait]
impl Recorder for ShadowStore {
    async fn record(&self, run: &ReconcileRun) -> Result<()> {
        self.record_reconcile(run).await
    }
}

/// One reconcile of memories' replica, ready to run.
pub struct Reconcile {
    /// vellev's replica.
    pub replica: Arc<dyn SyncTarget>,
    /// The Weaviate collection that serves, which the replica copies.
    pub serving: Arc<dyn SyncTarget>,
    /// Its name, for what is said of it.
    pub serving_name: String,
    pub ready: Arc<dyn ReplicaReady>,
    pub turns: Arc<dyn WriteTurns>,
    pub recorder: Arc<dyn Recorder>,
}

/// Made ready before each run: the replica's collection there, and its
/// tenant holding nothing it must not.
#[async_trait]
pub trait ReplicaReady: Send + Sync {
    async fn ready(&self) -> Result<()>;
}

/// [`ReplicaReady`] for vellev's replica.
struct VellevReady {
    replica: Arc<dyn Switchable>,
    /// vellev's store of the collection an import from Weaviate brings.
    imported: VellevStore,
}

#[async_trait]
impl ReplicaReady for VellevReady {
    async fn ready(&self) -> Result<()> {
        if self.imported.exists().await? {
            return Err(Refused::Tenant(self.imported.collection().to_string()).into());
        }
        self.replica
            .ensure_collection()
            .await
            .context("making the replica's collection ready")
    }
}

impl Reconcile {
    /// Run once, and record the run whatever its outcome. Returns what was
    /// recorded.
    pub async fn once(&self) -> ReconcileRun {
        let started_at = Utc::now();
        let mut synced = Synced::default();
        let attempted = self.attempt(&mut synced).await;
        let (outcome, error) = match &attempted {
            Ok(()) => (ReconcileOutcome::Ok, None),
            Err(e) if e.downcast_ref::<Promoted>().is_some() => {
                (ReconcileOutcome::RefusedPromoted, Some(format!("{e:#}")))
            }
            Err(e) => match e.downcast_ref::<Refused>() {
                Some(Refused::Unswitched(_)) => {
                    (ReconcileOutcome::RefusedUnswitched, Some(format!("{e:#}")))
                }
                Some(Refused::Tenant(_)) => {
                    (ReconcileOutcome::RefusedTenant, Some(format!("{e:#}")))
                }
                None => (ReconcileOutcome::Failed, Some(format!("{e:#}"))),
            },
        };
        let report = synced.report;
        let run = ReconcileRun {
            store: STORE.to_string(),
            started_at,
            outcome,
            error,
            copied: report.copied,
            updated: report.updated,
            dropped: report.dropped,
            unchanged: report.unchanged,
            fixed: synced
                .fixed
                .into_iter()
                .map(|fix| ReconcileFix {
                    kind: fix.kind.as_str(),
                    object_id: fix.object_id,
                    memory_id: fix.memory_id,
                })
                .collect(),
        };
        match outcome {
            ReconcileOutcome::Ok => tracing::info!(
                target: "shadow",
                copied = run.copied, updated = run.updated, dropped = run.dropped,
                "memories' replica reconciled"
            ),
            _ => tracing::error!(
                target: "shadow",
                outcome = outcome.as_str(),
                error = run.error.as_deref().unwrap_or_default(),
                copied = run.copied, updated = run.updated, dropped = run.dropped,
                "memories' replica not reconciled"
            ),
        }
        if let Err(e) = self.recorder.record(&run).await {
            tracing::error!(target: "shadow", error = %format!("{e:#}"), "a reconcile of memories' replica could not be recorded");
        }
        run
    }

    async fn attempt(&self, synced: &mut Synced) -> Result<()> {
        if !self.serving.switched().await? {
            return Err(Refused::Unswitched(self.serving_name.clone()).into());
        }
        self.ready.ready().await?;
        sync_recorded(
            self.replica.as_ref(),
            self.serving.as_ref(),
            SyncMode::Replica,
            self.turns.as_ref(),
            synced,
        )
        .await
    }
}

/// Run `reconcile` every `every`, on the leader only (`leader`: `None` for
/// a hub alone), never while `paused`, until `drain`.
pub fn spawn(
    reconcile: Arc<Reconcile>,
    every: Duration,
    paused: Arc<AtomicBool>,
    leader: Option<Arc<AtomicBool>>,
    drain: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            _ = drain.cancelled() => return,
            _ = tokio::time::sleep(LEADER_WIRING) => {}
        }
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
            tokio::select! {
                _ = drain.cancelled() => return,
                _ = reconcile.once() => {}
            }
        }
    })
}

/// Turns of the replica's lock, held shared, one a page, a page started
/// no sooner than `pace` after the one before.
pub struct ReplicaTurns {
    lock: Arc<ReplicaLock>,
    pace: Duration,
    wait: Duration,
    hold: Duration,
    last: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl ReplicaTurns {
    pub fn new(lock: Arc<ReplicaLock>, pace: Duration, wait: Duration, hold: Duration) -> Self {
        Self {
            lock,
            pace,
            wait,
            hold,
            last: tokio::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl WriteTurns for ReplicaTurns {
    async fn take(&self) -> Result<Box<dyn Turn>> {
        {
            let mut last = self.last.lock().await;
            if let Some(at) = *last {
                tokio::time::sleep_until(at + self.pace).await;
            }
            *last = Some(tokio::time::Instant::now());
        }
        let held = self.lock.shared(self.wait, self.hold).await?;
        if held.promoted {
            held.release().await?;
            return Err(eyre::Report::new(Promoted));
        }
        Ok(Box::new(ReplicaTurn(held)))
    }
}

struct ReplicaTurn(ReplicaHold);

#[async_trait]
impl Turn for ReplicaTurn {
    async fn done(self: Box<Self>) -> Result<()> {
        self.0.release().await
    }
}

/// The replica's lock on `[postgres]`, on a pool of its own: a page that
/// waits on an erasure ties up none of the other stores' connections.
pub fn replica_lock(config: &WorkforceConfig) -> Result<Arc<ReplicaLock>> {
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre!("[memory] backend = \"shadow\" needs [postgres] enabled = true"))?;
    let pool = PostgresTelemetryStore::build_pool(&PostgresConfig {
        pool_size: LOCK_POOL_SIZE,
        ..pg.clone()
    })
    .context("building the memory replica lock's pool")?;
    Ok(Arc::new(ReplicaLock::new(pool)))
}

/// A reconcile of memories' replica as the hub runs it, ready to run, and
/// what it is made of.
pub struct Reconciling {
    pub reconcile: Reconcile,
    /// The replica's lock, on a pool of its own.
    pub lock: Arc<ReplicaLock>,
    /// vellev's replica.
    pub replica: Arc<dyn Switchable>,
    /// The Weaviate collection that serves.
    pub serving: Arc<dyn Switchable>,
}

/// The reconcile `[memory] backend = "shadow"` runs, recorded in `shadow`:
/// what the hub's leader runs at its interval, and promotion runs to its
/// end (DW-676, PR 11).
pub async fn reconciling(
    config: &WorkforceConfig,
    shadow: Arc<dyn Recorder>,
) -> Result<Reconciling> {
    let (replica, serving) = crate::embeddings::shadow_stores(config).await?;
    let ready = VellevReady {
        replica: Arc::clone(&replica),
        imported: crate::embeddings::shadow_v1_store(config)?,
    };
    let lock = replica_lock(config)?;
    let turns = ReplicaTurns::new(Arc::clone(&lock), PAGE_PACE, LOCK_WAIT, LOCK_HOLD);
    let reconcile = Reconcile {
        serving_name: serving.collection().to_string(),
        replica: Arc::clone(&replica) as Arc<dyn SyncTarget>,
        serving: Arc::clone(&serving) as Arc<dyn SyncTarget>,
        ready: Arc::new(ready),
        turns: Arc::new(turns),
        recorder: shadow,
    };
    Ok(Reconciling {
        reconcile,
        lock,
        replica,
        serving,
    })
}

/// Start reconciling memories' replica, and applying the memories noted
/// for it ([`crate::memory_mirror`]), on the leader, when `[memory]
/// backend` shadows them; `shadow` is the hub's shadow tables. Both hold
/// the replica's lock shared, on one pool. Failing to start costs the
/// shadow, never the hub.
pub async fn start(
    config: &WorkforceConfig,
    shadow: Option<Arc<ShadowStore>>,
    leader: Option<Arc<AtomicBool>>,
    drain: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    if config.memory.backend != SHADOWED {
        return Vec::new();
    }
    let paused = Arc::new(AtomicBool::new(config.vellev.shadow.paused));
    let started = async {
        let shadow = shadow.ok_or_else(|| eyre!("the shadow tables are not open"))?;
        let tables = Arc::clone(&shadow);
        let every = config.vellev.shadow.memory_reconcile()?;
        let Reconciling {
            reconcile,
            lock,
            replica,
            serving,
        } = reconciling(config, shadow).await?;
        let known_items = known_items(
            config,
            Arc::clone(&replica),
            Arc::clone(&serving),
            tables,
            every,
        );
        let reconciling = spawn(
            Arc::new(reconcile),
            every,
            Arc::clone(&paused),
            leader.clone(),
            drain.clone(),
        );
        eyre::Ok((reconciling, lock, replica, serving, known_items))
    };
    let (reconciling, lock, replica, serving, known_items) = match started.await {
        Ok(started) => started,
        Err(e) => {
            tracing::error!(target: "shadow", error = %format!("{e:#}"), "memories' replica is not reconciled, nor are memories written applied to it");
            return Vec::new();
        }
    };
    let known_items = match known_items.await {
        Ok(Some(runner)) => Some(crate::known_items::spawn(
            Arc::new(runner) as Arc<dyn crate::known_items::Nightly>,
            Arc::clone(&paused),
            leader.clone(),
            drain.clone(),
        )),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(target: "shadow", error = %format!("{e:#}"), "memories' known-item runs are not made: the memory verdict has none to rest on");
            None
        }
    };
    let applying = async {
        let notes = crate::memory_mirror::notes(config).await?;
        let applier = crate::memory_mirror::Applier {
            replica,
            serving,
            notes,
            // The turns the lock test (and its NoTurns plant) exercises.
            turns: crate::memory_mirror::applier_turns(lock),
            clock: Arc::new(Utc::now),
        };
        eyre::Ok(crate::memory_mirror::spawn_applier(
            Arc::new(applier),
            crate::memory_mirror::APPLY_WATCH,
            paused,
            leader,
            drain,
        ))
    };
    let mut tasks = vec![reconciling];
    match applying.await {
        Ok(applying) => tasks.push(applying),
        Err(e) => {
            tracing::error!(target: "shadow", error = %format!("{e:#}"), "memories written are not applied to vellev's replica: the reconcile alone keeps it");
        }
    }
    tasks.extend(known_items);
    tasks
}

/// The nightly known-item runs of memories ([`crate::known_items`]), or
/// `None` when `[vellev.shadow.known_items] items` is 0. They share the
/// read shadow's kernel and embedder, and leave out memories written
/// within two reconciles (`every`), which the replica may not hold yet.
pub(crate) async fn known_items(
    config: &WorkforceConfig,
    replica: Arc<dyn Switchable>,
    serving: Arc<dyn Switchable>,
    tables: Arc<ShadowStore>,
    every: Duration,
) -> Result<Option<crate::known_items::Runner>> {
    use workforce_common::settings::ShadowedStore;
    let settings = config.vellev.shadow.known_items()?;
    if settings.items == 0 {
        tracing::info!(target: "shadow", "memories' known-item runs are off: [vellev.shadow.known_items] items = 0");
        return Ok(None);
    }
    let memories = config.vellev.shadow.settings(ShadowedStore::Memories)?;
    let reads = crate::memory_erasure::reads(
        config,
        Arc::clone(&replica) as Arc<dyn workforce_memory::store::MemoryStore>,
    )
    .await?;
    Ok(Some(crate::known_items::Runner {
        source: Arc::clone(&serving) as Arc<dyn workforce_memory::admin::SyncSource>,
        serving: serving as Arc<dyn workforce_memory::store::MemoryStore>,
        replica: replica as Arc<dyn workforce_memory::store::MemoryStore>,
        embedder: reads.embedder,
        kernel: reads.shadow,
        recorder: tables,
        settings,
        k: memories.k,
        rbo_p: memories.rbo_p,
        settle: every * 2,
        permit_wait: crate::known_items::PERMIT_WAIT,
        page_pace: crate::known_items::DRAW_PAGE_PACE,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use workforce_memory::admin::testing::Held;
    use workforce_memory::admin::NoTurns;
    use workforce_memory::embedding::EmbeddingBackend;

    /// Embeds as a hub model named `hub`.
    struct Hub;

    #[async_trait]
    impl EmbeddingBackend for Hub {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            Ok(vec![text.len() as f32, 1.0])
        }

        fn model_id(&self) -> &str {
            "hub"
        }
    }

    /// A local address that counts the connections made to it.
    async fn counted() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = Arc::clone(&reached);
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                counting.fetch_add(1, Ordering::SeqCst);
                drop(conn);
            }
        });
        (url, reached)
    }

    /// Under `"shadow"` every builder that reads `[memory] backend` builds
    /// on Weaviate. The replica is built on vellev all the same: it
    /// reaches the vellev node and never Weaviate, and the store it copies
    /// reaches Weaviate. Built on Weaviate, the replica would be Weaviate
    /// compared with itself (design r2 section 4.7).
    #[tokio::test]
    async fn the_replica_is_built_on_vellev_and_never_reaches_weaviate() {
        let (vellev, at_vellev) = counted().await;
        let (weaviate, at_weaviate) = counted().await;
        let mut config = WorkforceConfig::default();
        config.memory.backend = SHADOWED.into();
        config.vellev.url = vellev;
        config.vellev.api_key = Some("k".into());
        config.vellev.tenant = "acme".into();
        config.weaviate.enabled = true;
        config.weaviate.url = weaviate;
        let (replica, serving) = crate::embeddings::paired(&config, Arc::new(Hub), None).unwrap();
        assert_eq!(replica.embedding_model(), Some("hub"));
        assert_eq!(
            serving.embedding_model(),
            Some("hub"),
            "same model: vectors copied"
        );

        let _ = tokio::time::timeout(Duration::from_secs(2), replica.switched()).await;
        assert!(
            at_vellev.load(Ordering::SeqCst) > 0,
            "the replica asks vellev"
        );
        assert_eq!(at_weaviate.load(Ordering::SeqCst), 0, "and never Weaviate");
        let _ = tokio::time::timeout(Duration::from_secs(2), serving.switched()).await;
        assert!(
            at_weaviate.load(Ordering::SeqCst) > 0,
            "what it copies is Weaviate"
        );
    }

    #[derive(Default)]
    struct Recorded(Mutex<Vec<ReconcileRun>>);

    #[async_trait]
    impl Recorder for Recorded {
        async fn record(&self, run: &ReconcileRun) -> Result<()> {
            self.0.lock().unwrap().push(run.clone());
            Ok(())
        }
    }

    struct Ready(Option<Refused>);

    #[async_trait]
    impl ReplicaReady for Ready {
        async fn ready(&self) -> Result<()> {
            match &self.0 {
                None => Ok(()),
                Some(Refused::Tenant(c)) => Err(Refused::Tenant(c.clone()).into()),
                Some(Refused::Unswitched(c)) => Err(Refused::Unswitched(c.clone()).into()),
            }
        }
    }

    fn memory(n: u64, memory: &str) -> serde_json::Value {
        serde_json::json!({
            "id": format!("00000000-0000-0000-0000-{n:012}"),
            "class": "Memories",
            "properties": { "content": format!("note {n}"), "memoryId": memory, "deleted": false },
            "vector": [n as f32, 1.0],
        })
    }

    struct Case {
        replica: Arc<Held>,
        serving: Arc<Held>,
        recorded: Arc<Recorded>,
        reconcile: Arc<Reconcile>,
    }

    fn case(ready: Option<Refused>) -> Case {
        let replica = Arc::new(Held::holding(Some("hub"), vec![]));
        let serving = Arc::new(Held::holding(Some("hub"), vec![memory(1, "m1")]));
        *serving.switched.lock().unwrap() = true;
        let recorded = Arc::new(Recorded::default());
        let reconcile = Arc::new(Reconcile {
            replica: Arc::clone(&replica) as Arc<dyn SyncTarget>,
            serving: Arc::clone(&serving) as Arc<dyn SyncTarget>,
            serving_name: "WorkforceMemory_hub".into(),
            ready: Arc::new(Ready(ready)),
            turns: Arc::new(NoTurns),
            recorder: Arc::clone(&recorded) as Arc<dyn Recorder>,
        });
        Case {
            replica,
            serving,
            recorded,
            reconcile,
        }
    }

    const EVERY: Duration = Duration::from_secs(900);

    /// Let the reconcile task run up to `at` past its start.
    async fn run_until(at: Duration) {
        let step = Duration::from_secs(1);
        let mut gone = Duration::ZERO;
        while gone < at {
            tokio::time::advance(step).await;
            gone += step;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
    }

    /// A memory written to Weaviate by a path that never told the mirror
    /// is in the replica within one interval, and the run that copied it
    /// records it, by memory: what the reconcile fixes is the measure of
    /// what the mirror missed.
    #[tokio::test(start_paused = true)]
    async fn an_unwrapped_write_is_in_the_replica_within_one_interval_and_recorded() {
        let c = case(None);
        let task = spawn(
            Arc::clone(&c.reconcile),
            EVERY,
            Arc::new(AtomicBool::new(false)),
            None,
            CancellationToken::new(),
        );
        run_until(LEADER_WIRING + Duration::from_secs(1)).await;
        assert!(c
            .replica
            .get(memory(1, "m1")["id"].as_str().unwrap())
            .is_some());

        c.serving.objects.lock().unwrap().push(memory(2, "m2"));
        run_until(EVERY).await;
        let id = memory(2, "m2")["id"].as_str().unwrap().to_string();
        assert!(c.replica.get(&id).is_some(), "copied within one interval");
        let runs = c.recorded.0.lock().unwrap().clone();
        assert_eq!(runs.len(), 2, "{runs:?}");
        let last = &runs[1];
        assert_eq!(last.outcome, ReconcileOutcome::Ok);
        assert_eq!((last.copied, last.unchanged), (1, 1));
        assert_eq!(
            last.fixed,
            [ReconcileFix {
                kind: "copied",
                object_id: id,
                memory_id: Some("m2".into()),
            }]
        );

        // Purged in Weaviate, it goes from the replica too, though the
        // replica holds the switch marker an import brings.
        *c.replica.switched.lock().unwrap() = true;
        let first = memory(1, "m1")["id"].as_str().unwrap().to_string();
        c.serving
            .objects
            .lock()
            .unwrap()
            .retain(|o| o["id"] != first.as_str());
        run_until(EVERY).await;
        assert!(
            c.replica.get(&first).is_none(),
            "dropped within one interval"
        );
        let runs = c.recorded.0.lock().unwrap().clone();
        assert_eq!(runs[2].dropped, 1, "{:?}", runs[2]);
        task.abort();
    }

    /// Only the leader reconciles, and nothing runs while the shadow is
    /// paused.
    #[tokio::test(start_paused = true)]
    async fn a_follower_or_a_paused_shadow_never_reconciles() {
        for (paused, leader) in [(false, Some(false)), (true, None), (true, Some(true))] {
            let c = case(None);
            let task = spawn(
                Arc::clone(&c.reconcile),
                EVERY,
                Arc::new(AtomicBool::new(paused)),
                leader.map(|l| Arc::new(AtomicBool::new(l))),
                CancellationToken::new(),
            );
            run_until(LEADER_WIRING + EVERY * 2).await;
            assert!(
                c.recorded.0.lock().unwrap().is_empty(),
                "{paused} {leader:?}"
            );
            assert!(c.replica.objects.lock().unwrap().is_empty());
            task.abort();
        }
    }

    /// A run against a promoted replica, an unswitched Weaviate, or a
    /// tenant holding the imported collection writes nothing, and is
    /// recorded as refused, saying why.
    #[tokio::test]
    async fn a_run_that_must_not_write_is_recorded_as_refused() {
        let promoted = case(None);
        *promoted.replica.promoted.lock().unwrap() = true;
        let unswitched = case(None);
        *unswitched.serving.switched.lock().unwrap() = false;
        let tenant = case(Some(Refused::Tenant("WorkforceMemory".into())));
        for (c, outcome, says) in [
            (promoted, ReconcileOutcome::RefusedPromoted, "promoted"),
            (
                unswitched,
                ReconcileOutcome::RefusedUnswitched,
                "WorkforceMemory_hub",
            ),
            (tenant, ReconcileOutcome::RefusedTenant, "WorkforceMemory"),
        ] {
            let run = c.reconcile.once().await;
            assert_eq!(run.outcome, outcome, "{run:?}");
            assert!(
                run.error.as_deref().unwrap_or_default().contains(says),
                "{run:?}"
            );
            assert!(c.replica.objects.lock().unwrap().is_empty(), "{outcome:?}");
            assert_eq!(c.recorded.0.lock().unwrap().len(), 1);
        }
    }

    /// The replica's tenant must not hold the collection an import from
    /// Weaviate brings: nothing keeps it, and no erasure reaches it.
    #[tokio::test]
    async fn a_tenant_holding_the_imported_collection_is_refused() {
        use workforce_memory::vellev::DEFAULT_COLLECTION;
        let Some(node) = workforce_vellev::testing::Node::fresh().await else {
            return;
        };
        let store = |collection: &str| {
            VellevStore::new(&node.url, &node.key, &node.tenant, collection)
                .unwrap()
                .with_embeddings(Arc::new(Hub), None, 0.75, 30)
        };
        let replica: Arc<dyn Switchable> = Arc::new(store("WorkforceMemory_hub"));
        let ready = VellevReady {
            replica: Arc::clone(&replica),
            imported: store(DEFAULT_COLLECTION),
        };
        ready.ready().await.unwrap();
        assert!(
            store("WorkforceMemory_hub").exists().await.unwrap(),
            "made ready"
        );
        store(DEFAULT_COLLECTION).ensure_collection().await.unwrap();
        let refused = ready.ready().await.unwrap_err();
        assert!(
            matches!(refused.downcast_ref::<Refused>(), Some(Refused::Tenant(_))),
            "{refused}"
        );
    }

    /// The replica's lock and the record of runs, in Postgres: a page
    /// waits for an erasure holding the lock alone, and gives up past its
    /// wait; pages hold it together; a hold dropped without release lets it
    /// go; once Postgres records the promotion, a page is refused. A run is
    /// recorded with its first fixes by name and the rest counted.
    #[tokio::test(flavor = "multi_thread")]
    async fn pages_wait_for_an_erasure_and_stop_once_promoted() {
        let ran = Arc::new(AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresTelemetryStore::build_pool,
            |pool| async move {
                let shadow = ShadowStore::with_pool(pool.clone()).await.unwrap();
                let lock = Arc::new(ReplicaLock::new(pool.clone()));
                let turns = ReplicaTurns::new(
                    Arc::clone(&lock),
                    Duration::ZERO,
                    Duration::from_millis(300),
                    Duration::from_secs(60),
                );

                let erasing = lock.exclusive(Duration::from_secs(1), Duration::from_secs(60)).await.unwrap();
                let waited = turns.take().await.err().expect("a page waits, then gives up");
                assert!(format!("{waited:#}").contains("lock"), "{waited:#}");
                erasing.release().await.unwrap();

                let one = turns.take().await.unwrap();
                let two = turns.take().await.unwrap();
                assert!(
                    lock.exclusive(Duration::from_millis(300), Duration::from_secs(60)).await.is_err(),
                    "an erasure waits for the pages in hand"
                );
                one.done().await.unwrap();
                drop(two);
                lock.exclusive(Duration::from_secs(5), Duration::from_secs(60))
                    .await
                    .expect("a dropped hold lets the lock go")
                    .release()
                    .await
                    .unwrap();

                lock.record_promotion("WorkforceMemory_hub").await.unwrap();
                lock.record_promotion("WorkforceMemory_hub").await.unwrap();
                let refused = turns.take().await.err().expect("refused once promoted");
                assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused:#}");

                let run = ReconcileRun {
                    store: STORE.into(),
                    started_at: Utc::now(),
                    outcome: ReconcileOutcome::Ok,
                    error: None,
                    copied: 250,
                    updated: 0,
                    dropped: 0,
                    unchanged: 3,
                    fixed: (0..250)
                        .map(|n| ReconcileFix {
                            kind: "copied",
                            object_id: format!("o{n}"),
                            memory_id: Some(format!("m{n}")),
                        })
                        .collect(),
                };
                shadow.record_reconcile(&run).await.unwrap();
                let conn = pool.get().await.unwrap();
                let row = conn
                    .query_one(
                        "SELECT outcome, copied, unchanged, jsonb_array_length(fixed), fixed_left_out, fixed->0
                         FROM shadow_reconciles",
                        &[],
                    )
                    .await
                    .unwrap();
                assert_eq!(row.get::<_, String>(0), "ok");
                assert_eq!((row.get::<_, i32>(1), row.get::<_, i32>(2)), (250, 3));
                assert_eq!((row.get::<_, i32>(3), row.get::<_, i32>(4)), (200, 50));
                assert_eq!(
                    row.get::<_, serde_json::Value>(5),
                    serde_json::json!({ "kind": "copied", "object_id": "o0", "memory_id": "m0" })
                );
                reached.store(true, Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }
}

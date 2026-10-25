//! Promoting vellev to serve memories and the artifact index, after shadow
//! mode has compared it with Weaviate (DW-676, PR 11), and rolling back.
//!
//! **Start-up guards.** A process that opens memories in `vellev` refuses
//! to when memories were shadowed on its database and are not promoted, or
//! are promoted with a backlog of notes, an unclean last check, or a
//! collection not marked as holding every memory. One that opens them in
//! `weaviate` or `shadow` refuses once memories are promoted: Weaviate no
//! longer has what was written since, and `workforce shadow rollback`
//! carries it back first. A hub refuses to serve the artifact index from
//! vellev while a shadow of it is still noted for and not promoted. Each
//! guard decides on facts read from Postgres and vellev
//! ([`memories_may_open`], [`artifacts_may_open`]), and refuses when it
//! cannot read them: a guard that let a process start on facts it could
//! not read would be no guard.
//!
//! **Promotion** (`workforce shadow promote`) runs with every hub stopped.
//! A hub still on `shadow` would keep writing memories to Weaviate, and
//! none of those writes would reach a promoted vellev. It proves no hub
//! runs, takes the recorded verdict, brings the replica in line one last
//! time, takes away the notes that covered, and checks with a second whole
//! pass that nothing is left to fix. Only then does it write the promotion:
//! Postgres's record first, then vellev's marker, under the replica's lock
//! held alone. From the record on, every write into the replica is refused.
//!
//! **Rollback** (`workforce shadow rollback`) carries every change made on
//! vellev since back into Weaviate ([`SyncMode::Rollback`]), checks with a
//! second pass, and only then takes the marker and the record away.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Context, Result};
use workforce_common::postgres::{PostgresConfig, PostgresPool};
use workforce_common::settings::{WorkforceConfig, SHADOWED};
use workforce_common::shadow_postgres::{
    self, ArtifactMirrorFacts, MemoryShadowFacts, MirrorNotes, ReconcileOutcome, ReplicaLock,
};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;
use workforce_memory::admin::{
    sync_recorded, MemoryAdmin, NoTurns, Switchable, SyncMode, SyncSource, SyncTarget, Synced,
};
use workforce_memory::vellev::VellevStore;

use crate::memory_shadow::{Reconcile, ReplicaReady, LOCK_HOLD, LOCK_WAIT};

/// The store memories are promoted as, and their runs recorded under.
pub const MEMORIES: &str = crate::memory_shadow::STORE;
/// The store the artifact index is promoted as.
pub const ARTIFACTS: &str = "artifacts";

/// Why a guard or a step of promotion refused: what to do is in the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(why: impl Into<String>) -> eyre::Report {
    eyre::Report::new(Refused(why.into()))
}

// ---------------------------------------------------------------- verdicts

/// A store's verdict for its shadow week, as recorded (design r2 §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Too few samples to judge (A14): never a pass by itself.
    TooFewToJudge,
    Fail(String),
    /// No verdict is recorded, and why.
    Missing(String),
}

/// Where the weekly verdicts are read from.
#[async_trait]
pub trait VerdictSource: Send + Sync {
    async fn verdict(&self, store: &str) -> Result<Verdict>;
}

/// The verdicts as this release can read them: none. The weekly verdicts
/// are recorded by the operator report (DW-676 PR 9c, `shadow_verdicts`),
/// which is not built yet, so no promotion can pass this guard until it is
/// read here. Promotion fails closed rather than on a guessed table.
pub struct Unrecorded;

#[async_trait]
impl VerdictSource for Unrecorded {
    async fn verdict(&self, _store: &str) -> Result<Verdict> {
        Ok(Verdict::Missing(
            "weekly verdicts are not recorded where this release can read them: the operator report (DW-676 PR 9c, shadow_verdicts) is needed first".into(),
        ))
    }
}

/// Whether `verdict` lets `store` be promoted. "Too few to judge" lets the
/// artifact index be promoted only when the owner said so
/// (`accept_too_few`, A14: the criterion is then a clean check, which
/// promotion makes itself); memories never.
pub fn verdict_admits(store: &str, verdict: &Verdict, accept_too_few: bool) -> Result<(), Refused> {
    match verdict {
        Verdict::Pass => Ok(()),
        Verdict::TooFewToJudge if store == ARTIFACTS && accept_too_few => Ok(()),
        Verdict::TooFewToJudge if store == ARTIFACTS => Err(Refused(
            "the artifact index's verdict is \"too few to judge\": promote it only with the owner's sign-off under A14, by --accept-too-few".into(),
        )),
        Verdict::TooFewToJudge => Err(Refused(format!(
            "{store}' verdict is \"too few to judge\", which is never a pass"
        ))),
        Verdict::Fail(why) => Err(Refused(format!("{store}' verdict failed: {why}"))),
        Verdict::Missing(why) => Err(Refused(format!("{store} have no verdict: {why}"))),
    }
}

// --------------------------------------------------------- start-up guards

/// What a guard on memories decides on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryFacts {
    /// What Postgres says.
    pub shadow: MemoryShadowFacts,
    /// vellev's promotion marker on the collection, when it was read.
    pub marked: Option<bool>,
    /// vellev's switch marker on the collection, when it was read.
    pub switched: Option<bool>,
    /// `[embeddings] keep_previous`.
    pub keep_previous: bool,
}

impl MemoryFacts {
    /// Memories were shadowed on this database.
    fn shadowed(&self) -> bool {
        self.shadow.reconciled || self.shadow.backlog > 0 || self.shadow.promoted
    }
}

/// Whether memories may be opened from `backend`, on `facts`.
pub fn memories_may_open(backend: &str, facts: &MemoryFacts) -> Result<(), Refused> {
    match backend {
        "vellev" => {
            if !facts.shadowed() {
                return Ok(());
            }
            if !facts.shadow.promoted || facts.marked != Some(true) {
                return Err(Refused(
                    "[memory] backend = \"vellev\", but memories were shadowed here and are not promoted: vellev's replica may be behind Weaviate. Stop every hub and run `workforce shadow promote memories` on the shadow config, or set the backend back to \"shadow\"".into(),
                ));
            }
            if facts.shadow.backlog > 0 {
                return Err(Refused(format!(
                    "[memory] backend = \"vellev\", but {} memories are still noted for vellev's replica: they may not be in it. Set the backend back to \"shadow\" and roll back (`workforce shadow rollback memories`), then promote again",
                    facts.shadow.backlog
                )));
            }
            if facts.switched != Some(true) {
                return Err(Refused(
                    "[memory] backend = \"vellev\", but vellev's collection is not marked as holding every memory: opening it would copy into it from vellev's own first collection. Run `workforce shadow promote memories`, which marks it once it is checked".into(),
                ));
            }
            // The promotion was recorded only after a clean pass, so with no
            // reconcile since (the rows pruned, or none run) it stands for
            // the last check; one run since must be clean.
            match &facts.shadow.last_run {
                None => Ok(()),
                Some((outcome, 0)) if outcome == ReconcileOutcome::Ok.as_str() => Ok(()),
                Some((outcome, fixed)) => Err(Refused(format!(
                    "[memory] backend = \"vellev\", but a check of vellev's replica against Weaviate since the promotion was not clean ({outcome}, {fixed} objects fixed): set the backend back to \"shadow\", roll back, and promote again"
                ))),
            }
        }
        "weaviate" | SHADOWED if facts.shadow.previous_frozen && facts.keep_previous => {
            Err(Refused(format!(
                "[memory] backend = \"{backend}\" with [embeddings] keep_previous on, but the store memories switched from (WorkforceMemory) has missed erasures: memories were promoted, or a hub left it alone, here. Keeping it again would catch an erased memory back up. Set [embeddings] keep_previous = false"
            )))
        }
        "weaviate" | SHADOWED if facts.shadow.promoted => Err(Refused(format!(
            "[memory] backend = \"{backend}\", but memories were promoted to vellev: Weaviate does not have what was written since. Run `workforce shadow rollback memories` with every hub stopped, or set the backend to \"vellev\""
        ))),
        _ => Ok(()),
    }
}

/// What a guard on the artifact index decides on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactFacts {
    pub mirror: ArtifactMirrorFacts,
    /// vellev's promotion marker on the artifact index, when it was read.
    pub marked: Option<bool>,
}

/// Whether the artifact index may be served from `backend`, on `facts`.
pub fn artifacts_may_open(backend: &str, facts: &ArtifactFacts) -> Result<(), Refused> {
    if backend != "vellev" {
        return Ok(());
    }
    if facts.mirror.lease_held {
        return Err(Refused(
            "[artifacts] index_backend = \"vellev\", but a hub is still keeping a shadow of the artifact index (its lease holds): stop every hub and run `workforce shadow promote artifacts` on the shadow config".into(),
        ));
    }
    if facts.mirror.pending > 0 {
        return Err(Refused(format!(
            "[artifacts] index_backend = \"vellev\", but {} artifacts are still noted for vellev's mirror and the artifact index is not promoted: run `workforce shadow promote artifacts` on the shadow config",
            facts.mirror.pending
        )));
    }
    // Shadowed here, the lease ever set: served from vellev only once
    // promoted, by Postgres's end of the lease and vellev's marker both.
    if facts.mirror.lease_set && !(facts.mirror.promoted && facts.marked == Some(true)) {
        return Err(Refused(
            "[artifacts] index_backend = \"vellev\", but the artifact index was shadowed here and is not promoted: run `workforce shadow promote artifacts` on the shadow config".into(),
        ));
    }
    Ok(())
}

/// A pool for promotion's reads and writes on `[postgres]`, or `None`
/// without it: then nothing can have been shadowed (shadow mode needs it).
/// One pool of two connections per database a process names, made on
/// first use and kept, so that the guard every opening of memories runs
/// builds none after the first.
fn pool(config: &WorkforceConfig) -> Result<Option<PostgresPool>> {
    static POOLS: std::sync::Mutex<Vec<(String, PostgresPool)>> = std::sync::Mutex::new(Vec::new());
    let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) else {
        return Ok(None);
    };
    let key = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        pg.url, pg.host, pg.port, pg.database, pg.user
    );
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, pool)) = pools.iter().find(|(k, _)| *k == key) {
        return Ok(Some(pool.clone()));
    }
    let pool = PostgresTelemetryStore::build_pool(&PostgresConfig {
        pool_size: 2,
        ..pg.clone()
    })
    .context("building promotion's Postgres pool")?;
    pools.push((key, pool.clone()));
    Ok(Some(pool))
}

/// vellev's store of the collection memories are kept in under
/// `[embeddings]`, for its markers.
fn embedded_collection(config: &WorkforceConfig) -> Result<VellevStore> {
    let v = &config.vellev;
    let key = v
        .api_key
        .as_deref()
        .ok_or_else(|| eyre!("[vellev] api_key is not set"))?;
    VellevStore::new(&v.url, key, &v.tenant, &config.embeddings.collection_name())
}

/// The facts a guard on memories needs, or `None` without `[postgres]`.
async fn memory_facts(config: &WorkforceConfig) -> Result<Option<MemoryFacts>> {
    let Some(pool) = pool(config)? else {
        return Ok(None);
    };
    let mut facts = MemoryFacts {
        shadow: shadow_postgres::memory_shadow_facts(&pool, MEMORIES).await?,
        keep_previous: config.embeddings.keep_previous,
        ..MemoryFacts::default()
    };
    // Left alone from here on: recorded before the store is opened, so no
    // hub can keep it again unseen.
    if !config.embeddings.keep_previous && !config.memory.is_vellev() {
        shadow_postgres::record_previous_left(&pool).await?;
        facts.shadow.previous_frozen = true;
    }
    if config.memory.is_vellev() && facts.shadowed() {
        let replica = embedded_collection(config)?;
        facts.marked = Some(replica.promoted().await?);
        facts.switched = Some(replica.switched().await?);
    }
    Ok(Some(facts))
}

/// Refuse to open memories as `[memory] backend` says, if
/// [`memories_may_open`] refuses or its facts cannot be read. Called by
/// every process before it opens memories.
pub async fn guard_memories(config: &WorkforceConfig) -> Result<()> {
    // As the hub opens them: "sqlite" with [weaviate] enabled is Weaviate.
    let backend = match config.memory.backend.as_str() {
        "sqlite" if config.weaviate.enabled => "weaviate",
        backend => backend,
    };
    if !matches!(backend, "vellev" | "weaviate" | SHADOWED) {
        return Ok(());
    }
    let facts = memory_facts(config).await.map_err(|e| {
        refused(format!(
            "whether memories were promoted to vellev cannot be read, so they are not opened from \"{backend}\": {e:#}"
        ))
    })?;
    match facts {
        Some(facts) => Ok(memories_may_open(backend, &facts)?),
        None => Ok(()),
    }
}

/// Refuse to serve the artifact index from vellev, if
/// [`artifacts_may_open`] refuses or its facts cannot be read. Called by
/// the hub at start-up.
pub async fn guard_artifacts(config: &WorkforceConfig) -> Result<()> {
    let backend = config.artifacts.index_backend.as_str();
    if backend != "vellev" {
        return Ok(());
    }
    let read = async {
        let Some(pool) = pool(config)? else {
            return eyre::Ok(None);
        };
        let mirror = shadow_postgres::artifact_mirror_facts(&pool).await?;
        let marked = match mirror.lease_set {
            true => Some(vellev_artifacts(config)?.promoted().await?),
            false => None,
        };
        eyre::Ok(Some(ArtifactFacts { mirror, marked }))
    };
    let facts = read.await.map_err(|e| {
        refused(format!(
            "whether the artifact index was promoted to vellev cannot be read, so it is not served from vellev: {e:#}"
        ))
    })?;
    match facts {
        Some(facts) => Ok(artifacts_may_open(backend, &facts)?),
        None => Ok(()),
    }
}

fn vellev_artifacts(config: &WorkforceConfig) -> Result<crate::artifact_index::VellevArtifacts> {
    let v = &config.vellev;
    let key = v
        .api_key
        .as_deref()
        .ok_or_else(|| eyre!("[vellev] api_key is not set"))?;
    crate::artifact_index::VellevArtifacts::new(&v.url, key, &v.tenant)
}

// ------------------------------------------------------------- the fleet

/// Whether any hub runs against the database.
#[async_trait]
pub trait Fleet: Send + Sync {
    /// `Err` ([`Refused`]) while one does.
    async fn idle(&self) -> Result<()>;
}

/// [`Fleet`] as Postgres tells it: no artifact index served (each hub's
/// keeper holds its serving lock, shared, for its whole life), and the
/// artifact mirror's lease lapsed (a hub keeping a mirror renews it every
/// 30 s). The serving locks of `backends` are taken alone at the first
/// look and held until the fleet is dropped, at the end of the command,
/// so that a hub started meanwhile serves no index; its keeper says so.
pub struct PostgresFleet {
    pool: PostgresPool,
    backends: Vec<&'static str>,
    held: tokio::sync::Mutex<Vec<workforce_orchestrator::artifact_store::HeldLock>>,
}

impl PostgresFleet {
    /// Every kind of artifact index: what promotion of memories holds.
    pub fn new(pool: PostgresPool) -> Self {
        Self::holding(pool, &["weaviate", "vellev"])
    }

    /// Only `backends`' serving locks.
    pub fn holding(pool: PostgresPool, backends: &[&'static str]) -> Self {
        Self {
            pool,
            backends: backends.to_vec(),
            held: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Fleet for PostgresFleet {
    async fn idle(&self) -> Result<()> {
        let mut held = self.held.lock().await;
        if held.is_empty() {
            let (taken, served) =
                workforce_orchestrator::artifact_store::hold_unserved(&self.pool, &self.backends)
                    .await
                    .context("asking Postgres whether a hub serves")?;
            if !served.is_empty() {
                return Err(refused(format!(
                    "a hub is running: it serves the {} artifact index. Stop every hub (and any other workforce process that writes memories) first",
                    served.join(" and ")
                )));
            }
            *held = taken;
        } else {
            // Held since the first look: the connections must still be
            // there, or the locks went with them.
            for conn in held.iter() {
                conn.query_one("SELECT 1", &[])
                    .await
                    .context("the serving locks promotion holds were lost")?;
            }
        }
        if shadow_postgres::artifact_mirror_facts(&self.pool)
            .await?
            .lease_held
        {
            return Err(refused(
                "a hub kept a shadow of the artifact index within the last 90 s (its lease holds): stop every hub and wait for the lease to lapse",
            ));
        }
        Ok(())
    }
}

// ------------------------------------------------- memories: promotion

/// The memories noted for vellev's replica.
#[async_trait]
pub trait Notes: Send + Sync {
    async fn last_seq(&self) -> Result<Option<i64>>;
    async fn drain_through(&self, seq: i64) -> Result<u64>;
    async fn backlog(&self) -> Result<i64>;
}

#[async_trait]
impl Notes for MirrorNotes {
    async fn last_seq(&self) -> Result<Option<i64>> {
        MirrorNotes::last_seq(self).await
    }
    async fn drain_through(&self, seq: i64) -> Result<u64> {
        MirrorNotes::drain_through(self, seq).await
    }
    async fn backlog(&self) -> Result<i64> {
        Ok(MirrorNotes::backlog(self).await?.pending)
    }
}

/// The record and the marker of memories' promotion.
#[async_trait]
pub trait Marks: Send + Sync {
    /// Postgres's record, then vellev's marker.
    async fn promote(&self) -> Result<()>;
    /// Whether (Postgres records, vellev marks) the promotion.
    async fn state(&self) -> Result<(bool, bool)>;
    /// vellev's marker away, then Postgres's record.
    async fn withdraw(&self) -> Result<()>;
}

/// [`Marks`] in Postgres and on vellev's replica, each written under the
/// replica's lock held alone: no page of a reconcile, note of the applier
/// or erasure is between its read and its write.
pub struct ReplicaMarks {
    pub lock: Arc<ReplicaLock>,
    pub replica: VellevStore,
}

#[async_trait]
impl Marks for ReplicaMarks {
    async fn promote(&self) -> Result<()> {
        let held = self.lock.exclusive(LOCK_WAIT, LOCK_HOLD).await?;
        // The record first: from it on, every write into the replica is
        // refused, whatever becomes of the marker.
        let done = async {
            self.lock
                .record_promotion(self.replica.collection())
                .await
                .context("recording the promotion in Postgres")?;
            self.replica
                .mark_promoted()
                .await
                .context("marking the promotion on vellev")
        }
        .await;
        let released = held.release().await;
        done?;
        released
    }

    async fn state(&self) -> Result<(bool, bool)> {
        Ok((
            self.lock.promotion_recorded().await?,
            self.replica.promoted().await?,
        ))
    }

    async fn withdraw(&self) -> Result<()> {
        let held = self.lock.exclusive(LOCK_WAIT, LOCK_HOLD).await?;
        // The marker first: a rollback stopped between the two leaves
        // Postgres still refusing every write into the replica.
        let done = async {
            self.replica
                .unmark_promoted()
                .await
                .context("taking the promotion's marker away on vellev")?;
            self.lock
                .withdraw_promotion()
                .await
                .context("taking the promotion's record away in Postgres")
        }
        .await;
        let released = held.release().await;
        done?;
        released
    }
}

/// `workforce shadow promote memories`, made of its parts.
pub struct MemoryPromotion {
    pub verdicts: Arc<dyn VerdictSource>,
    pub fleet: Arc<dyn Fleet>,
    pub notes: Arc<dyn Notes>,
    /// The reconcile the hub's leader runs, recorded as its runs are.
    pub reconcile: Arc<Reconcile>,
    /// vellev's replica, for its switch marker.
    pub replica: Arc<dyn SyncTarget>,
    pub marks: Arc<dyn Marks>,
}

impl MemoryPromotion {
    /// Promote, saying each step done to `say`; `Err` ([`Refused`] where a
    /// guard refused) at the first step that does not pass, having written
    /// no promotion.
    pub async fn run(&self, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        self.fleet.idle().await?;
        say("no hub is running");
        match self.marks.state().await? {
            (false, false) => {}
            (true, true) => {
                return Err(refused(
                    "memories are promoted to vellev already: set [memory] backend = \"vellev\"",
                ))
            }
            // Stopped between the record and the marker (or a restore of
            // the tenant took the marker away): every replica write is
            // refused, so a reconcile cannot run. A rollback carries back
            // what vellev took and clears both.
            _ => {
                return Err(refused(
                    "a promotion of memories stopped part way (Postgres's record and vellev's marker disagree): run `workforce shadow rollback memories`, then promote again",
                ))
            }
        }
        verdict_admits(MEMORIES, &self.verdicts.verdict(MEMORIES).await?, false)?;
        say("the week's verdict is a pass");
        let seen = self.notes.last_seq().await?;
        let last = self.reconcile.once().await;
        if last.outcome != ReconcileOutcome::Ok {
            return Err(refused(format!(
                "the last reconcile before promotion ended {}: {}",
                last.outcome.as_str(),
                last.error.unwrap_or_default()
            )));
        }
        say(&format!(
            "the replica is in line with Weaviate ({} copied, {} updated, {} dropped)",
            last.copied, last.updated, last.dropped
        ));
        if let Some(seq) = seen {
            self.notes.drain_through(seq).await?;
        }
        let left = self.notes.backlog().await?;
        if left > 0 {
            return Err(refused(format!(
                "{left} memories were noted for the replica while it was brought in line: something still writes memories. Stop it and promote again"
            )));
        }
        say("no memory is noted for the replica");
        let check = self.reconcile.once().await;
        let fixed = check.copied + check.updated + check.dropped;
        if check.outcome != ReconcileOutcome::Ok || fixed > 0 {
            return Err(refused(format!(
                "a second pass over the replica was not clean ({}, {fixed} objects fixed): something still writes memories, or the stores do not settle. Find it and promote again",
                check.outcome.as_str()
            )));
        }
        say("a second pass found nothing to fix");
        if !self.replica.switched().await? {
            self.replica.mark_switched().await?;
            say("the replica's collection is marked as holding every memory");
        }
        self.fleet.idle().await?;
        self.marks.promote().await?;
        say("memories are promoted to vellev");
        Ok(())
    }
}

// --------------------------------------------------- memories: rollback

/// A collection to be dropped only if it holds nothing.
#[async_trait]
pub trait Leftover: Send + Sync {
    /// The collection's name.
    fn name(&self) -> String;
    /// Whether it holds nothing, or is not there.
    async fn empty(&self) -> Result<bool>;
    /// Drop it if it holds nothing; `Ok(false)` if it holds anything.
    async fn drop_if_empty(&self) -> Result<bool>;
}

#[async_trait]
impl Leftover for VellevStore {
    fn name(&self) -> String {
        self.collection().to_string()
    }
    async fn empty(&self) -> Result<bool> {
        self.holds_nothing().await
    }
    async fn drop_if_empty(&self) -> Result<bool> {
        VellevStore::drop_if_empty(self).await
    }
}

/// `workforce shadow rollback memories`, made of its parts.
pub struct MemoryRollback {
    pub fleet: Arc<dyn Fleet>,
    pub marks: Arc<dyn Marks>,
    /// vellev's collection memories were promoted to.
    pub promoted: Arc<dyn SyncSource>,
    /// Weaviate's collection they were promoted from.
    pub weaviate: Arc<dyn SyncTarget>,
    /// Makes Weaviate's collection ready, as it is dropped once memories
    /// are promoted.
    pub ready: Arc<dyn ReplicaReady>,
    /// vellev's first collection, which a hub serving from vellev makes
    /// and the shadow refuses a tenant to hold.
    pub first: Arc<dyn Leftover>,
}

impl MemoryRollback {
    pub async fn run(&self, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        self.fleet.idle().await?;
        say("no hub is running");
        let (recorded, marked) = self.marks.state().await?;
        if !recorded && !marked {
            return Err(refused(
                "memories are not promoted to vellev: there is nothing to roll back",
            ));
        }
        if !self.first.empty().await? {
            return Err(refused(format!(
                "vellev's {} holds memories, which a hub on \"shadow\" refuses and a rollback never drops: find out what wrote them first",
                self.first.name()
            )));
        }
        self.ready
            .ready()
            .await
            .context("making Weaviate's collection ready")?;
        let carried = rolled(self.weaviate.as_ref(), self.promoted.as_ref()).await?;
        say(&format!(
            "Weaviate holds what vellev holds ({} copied, {} updated, {} dropped)",
            carried.copied, carried.updated, carried.dropped
        ));
        let check = rolled(self.weaviate.as_ref(), self.promoted.as_ref()).await?;
        let fixed = check.copied + check.updated + check.dropped;
        if fixed > 0 {
            return Err(refused(format!(
                "a second pass found {fixed} objects to fix in Weaviate: something still writes memories to vellev. Stop it and roll back again"
            )));
        }
        say("a second pass found nothing to fix");
        if !self.first.drop_if_empty().await? {
            return Err(refused(format!(
                "vellev's {} came to hold memories during the rollback: something still writes. Stop it and roll back again",
                self.first.name()
            )));
        }
        self.fleet.idle().await?;
        self.marks.withdraw().await?;
        say("memories are no longer promoted: set [memory] backend to \"shadow\" (or \"weaviate\") and start the hubs");
        Ok(())
    }
}

/// A store made ready by making its collection.
struct Made(Arc<dyn Switchable>);

#[async_trait]
impl ReplicaReady for Made {
    async fn ready(&self) -> Result<()> {
        self.0.ensure_collection().await
    }
}

/// One whole [`SyncMode::Rollback`] of `weaviate` from `promoted`.
async fn rolled(
    weaviate: &dyn SyncTarget,
    promoted: &dyn SyncSource,
) -> Result<workforce_memory::admin::SyncReport> {
    let mut synced = Synced::default();
    sync_recorded(
        weaviate,
        promoted,
        SyncMode::Rollback,
        &NoTurns,
        &mut synced,
    )
    .await
    .context("carrying memories back from vellev into Weaviate")?;
    Ok(synced.report)
}

// --------------------------------------------- wiring, from the config

/// `workforce shadow promote memories`, on the shadow config.
pub async fn promote_memories(
    config: &WorkforceConfig,
    verdicts: Arc<dyn VerdictSource>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    if config.memory.backend != SHADOWED {
        return Err(refused(format!(
            "memories are promoted on the config they were shadowed on: [memory] backend is \"{}\", not \"shadow\"",
            config.memory.backend
        )));
    }
    if config.embeddings.keep_previous {
        return Err(refused(
            "[embeddings] keep_previous is on: the hub keeps the collection memories switched from (Weaviate's WorkforceMemory) beside the one it serves, catching up from it, and nothing erases from it once memories are promoted, so a memory erased on vellev would come back after a rollback. Set [embeddings] keep_previous = false, restart the hubs on it, then stop them and promote",
        ));
    }
    let pool = pool(config)?
        .ok_or_else(|| refused("[memory] backend = \"shadow\" needs [postgres] enabled"))?;
    // Its own store of the shadow tables, not the hub's shared one: this
    // runs once, apart from any hub.
    let shadow = workforce_common::shadow_postgres::ShadowStore::with_pool(pool.clone()).await?;
    let reconciling = crate::memory_shadow::reconciling(config, shadow).await?;
    let notes = MirrorNotes::with_pool(pool.clone()).await?;
    let promotion = MemoryPromotion {
        verdicts,
        fleet: Arc::new(PostgresFleet::new(pool)),
        notes,
        reconcile: Arc::new(reconciling.reconcile),
        replica: reconciling.replica.clone(),
        marks: Arc::new(ReplicaMarks {
            lock: reconciling.lock,
            replica: embedded_collection(config)?,
        }),
    };
    promotion.run(say).await
}

/// `workforce shadow rollback memories`, on the shadow config the hubs
/// will start on again.
pub async fn rollback_memories(
    config: &WorkforceConfig,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    if config.memory.backend != SHADOWED {
        return Err(refused(format!(
            "memories are rolled back on the shadow config the hubs start on again: [memory] backend is \"{}\", not \"shadow\"",
            config.memory.backend
        )));
    }
    if config.embeddings.keep_previous {
        return Err(refused(
            "[embeddings] keep_previous is on: hubs started on this config after the rollback would keep the store memories switched from (WorkforceMemory), which missed every erasure made on vellev, and catch erased memories back up. Set [embeddings] keep_previous = false",
        ));
    }
    let pool = pool(config)?
        .ok_or_else(|| refused("[memory] backend = \"shadow\" needs [postgres] enabled"))?;
    let (promoted, weaviate) = crate::embeddings::shadow_stores(config).await?;
    let rollback = MemoryRollback {
        ready: Arc::new(Made(Arc::clone(&weaviate))),
        fleet: Arc::new(PostgresFleet::new(pool)),
        marks: Arc::new(ReplicaMarks {
            lock: crate::memory_shadow::replica_lock(config)?,
            replica: embedded_collection(config)?,
        }),
        promoted,
        weaviate,
        first: Arc::new(crate::embeddings::shadow_v1_store(config)?),
    };
    rollback.run(say).await
}

/// `workforce shadow promote artifacts`, on the shadow config.
pub async fn promote_artifacts(
    config: &WorkforceConfig,
    verdicts: Arc<dyn VerdictSource>,
    accept_too_few: bool,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    if config.artifacts.index_backend != SHADOWED {
        return Err(refused(format!(
            "the artifact index is promoted on the config it was shadowed on: [artifacts] index_backend is \"{}\", not \"shadow\"",
            config.artifacts.index_backend
        )));
    }
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| {
            refused("[artifacts] index_backend = \"shadow\" needs [postgres] enabled")
        })?;
    let pool = workforce_orchestrator::artifact_store::KnowledgeStore::build_pool(pg)
        .map_err(|e| eyre!("Postgres cannot be reached: {e}"))?;
    // Weaviate's serving lock held from here to the end; vellev's is
    // taken by the fills below on sessions of their own (refused while a
    // hub's mirror keeper holds it), and held from after them.
    let fleet = PostgresFleet::holding(pool.clone(), &["weaviate"]);
    fleet.idle().await?;
    say("no hub is running");
    verdict_admits(
        ARTIFACTS,
        &verdicts.verdict(ARTIFACTS).await?,
        accept_too_few,
    )?;
    say("the week's verdict admits promotion");
    let none = |_: &workforce_vellev::seeding::Equivalence| {};
    let filled = crate::serve::reindex_artifacts(config, true, None, &none, |_, _| {}).await?;
    if filled.failed > 0 {
        return Err(refused(format!(
            "{} of {} artifacts could not be put in vellev's mirror: promote again once it takes them",
            filled.failed, filled.total
        )));
    }
    say(&format!(
        "vellev's mirror is in line with Postgres ({} indexed, {} removed)",
        filled.indexed, filled.removed
    ));
    let check = crate::serve::reindex_artifacts(config, true, None, &none, |_, _| {}).await?;
    if check.indexed + check.removed + check.failed > 0 {
        return Err(refused(format!(
            "a second pass over vellev's mirror was not clean ({} indexed, {} removed, {} failed): something still writes artifacts. Stop it and promote again",
            check.indexed, check.removed, check.failed
        )));
    }
    let artifacts = vellev_artifacts(config)?;
    {
        use workforce_orchestrator::artifact_store::ArtifactIndexer;
        let index = crate::artifact_index::ArtifactIndex::new(vellev_artifacts(config)?);
        if !index.complete().await? {
            return Err(refused(
                "vellev's mirror is not marked complete after a clean fill: promote again",
            ));
        }
    }
    say("a second pass found nothing to fix, and the index is marked complete");
    fleet.idle().await?;
    let after = PostgresFleet::holding(pool.clone(), &["vellev"]);
    after.idle().await?;
    let dropped = shadow_postgres::end_artifact_mirror(&pool).await?;
    say(&format!(
        "the mirror's noting is ended ({dropped} leftover notes dropped)"
    ));
    artifacts.mark_promoted().await?;
    say("the artifact index is promoted to vellev");
    Ok(())
}

/// `workforce shadow rollback artifacts`: Postgres holds every artifact,
/// so only the marker goes; Weaviate's keeper puts its index right on its
/// first compare once it serves again.
pub async fn rollback_artifacts(
    config: &WorkforceConfig,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| refused("a rollback needs [postgres] enabled"))?;
    let pool = workforce_orchestrator::artifact_store::KnowledgeStore::build_pool(pg)
        .map_err(|e| eyre!("Postgres cannot be reached: {e}"))?;
    PostgresFleet::new(pool.clone()).idle().await?;
    say("no hub is running");
    vellev_artifacts(config)?.unmark_promoted().await?;
    shadow_postgres::unend_artifact_mirror(&pool).await?;
    say("the artifact index is no longer promoted: set [artifacts] index_backend to \"shadow\" (or \"weaviate\") and start the hubs");
    Ok(())
}

#[cfg(test)]
#[path = "promotion_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "promotion_e2e_tests.rs"]
mod e2e;

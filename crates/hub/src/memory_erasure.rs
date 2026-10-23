//! Erasure while memories are shadowed (DW-676, owner decision D1 (a)): a
//! purge reaches vellev's replica in the same call, or fails.
//!
//! The rule is `WeaviateStore::purge_identity`'s for its rollback copy:
//! erasure that left a copy behind would not be erasure. vellev's replica
//! is such a copy while memories are shadowed. So a purge erases from the
//! replica first, then from Weaviate, all while holding the replica's lock
//! alone, so that no write into the replica decided on what was read
//! before the erasure lands after it.
//!
//! Neither the kill switch, the breaker nor sampling stops it: the shadow
//! paused, a purge still reaches vellev; vellev unreachable, the purge
//! fails, naming vellev, to be retried. Both purges are idempotent.
//!
//! The replica first: if Weaviate's purge came first and vellev's then
//! failed, a retry could no longer find in Weaviate a memory moved to the
//! erased identity there but not yet in vellev, which still files it
//! under its old identity; the copy would outlive the erasure. Erased
//! from the replica first, a failure leaves Weaviate as it was, and the
//! retry resolves the same memories again.
//!
//! Every other write Weaviate takes is noted for the replica after it
//! succeeds ([`crate::memory_mirror`]), never waiting on the shadow; a
//! reassign's memories are resolved from Weaviate before it, as after it
//! they are no longer filed under the identity they came from.
//!
//! What erasure does not reach (the runbook says so): vellev's backups
//! and its point-in-time window keep an erased memory for their window,
//! as Weaviate's backups do, and the importer's ledger keeps each
//! imported object's id and a hash of it, not its content.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Context, Result};
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow_postgres::{ReplicaHold, ReplicaLock};
use workforce_core::types::AgentId;
use workforce_memory::store::{MemoryStore, MemoryStoreCacheStats};
use workforce_memory::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};

use workforce_common::shadow::{Observed, Ranked, Shadow};
use workforce_memory::embedding::EmbeddingBackend;

use crate::memory_mirror::{Note, Noter};

/// How long a purge waits for the replica's lock: for the page of the
/// reconcile in hand to be written.
pub const ERASURE_LOCK_WAIT: Duration = Duration::from_secs(60);
/// How long a purge may hold the lock before Postgres gives it up.
pub const ERASURE_LOCK_HOLD: Duration = Duration::from_secs(30 * 60);

/// What a purge erases from besides the store that serves.
#[async_trait]
pub trait Erases: Send + Sync {
    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize>;
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize>;
}

/// The replica's lock, held alone for an erasure.
#[async_trait]
pub trait ErasureLock: Send + Sync {
    async fn hold(&self) -> Result<Box<dyn Held>>;
}

/// A hold of [`ErasureLock`], until released.
#[async_trait]
pub trait Held: Send {
    async fn release(self: Box<Self>) -> Result<()>;
}

/// [`ReplicaLock`] held alone, waiting at most `wait` for it.
pub struct Exclusive {
    pub lock: Arc<ReplicaLock>,
    pub wait: Duration,
    pub hold: Duration,
}

#[async_trait]
impl ErasureLock for Exclusive {
    async fn hold(&self) -> Result<Box<dyn Held>> {
        let held = self
            .lock
            .exclusive(self.wait, self.hold)
            .await
            .context("taking the memory replica's lock for an erasure: a reconcile page holds it, or Postgres is unreachable. Nothing was erased; retry the purge")?;
        Ok(Box::new(held))
    }
}

#[async_trait]
impl Held for ReplicaHold {
    async fn release(self: Box<Self>) -> Result<()> {
        ReplicaHold::release(*self).await
    }
}

/// The memory store while memories are shadowed: Weaviate's, which
/// serves everything, with each purge also erasing from vellev's replica
/// in the same call.
pub struct ShadowedMemory {
    serving: Arc<dyn MemoryStore>,
    replica: Arc<dyn Erases>,
    lock: Arc<dyn ErasureLock>,
    /// Where each write Weaviate took is noted for the replica; `None`
    /// while paused, or in a process that notes nothing.
    noter: Option<Noter>,
    /// Shadow mode's reads: each recall asked of the replica too; `None`
    /// in a process that shadows none.
    reads: Option<Reads>,
}

/// Shadow mode's reads of memories (DW-676, design r2 section 4.7): each
/// recall is embedded once, by the hub, and that one vector is used by
/// Weaviate for the user's answer and by vellev's replica for the
/// kernel's background comparison, so that the two indexes are compared,
/// not two embedding calls (A5). The replica's answer never reaches the
/// user, and a recall never waits on it.
#[derive(Clone)]
pub struct Reads {
    /// The hub's embedder: of the model both stores hold vectors of.
    pub embedder: Arc<dyn EmbeddingBackend>,
    pub replica: Arc<dyn MemoryStore>,
    pub shadow: Arc<Shadow>,
}

/// A recall's answer as the kernel compares it: memories, not the objects
/// holding them, in the order given, each once.
fn ranked(hits: &[MemorySearchResult], embed: Option<Duration>) -> Ranked {
    let mut seen = std::collections::HashSet::new();
    Ranked {
        ids: hits
            .iter()
            .map(|hit| hit.memory.id.0.clone())
            .filter(|id| seen.insert(id.clone()))
            .collect(),
        top_score: hits.first().map(|hit| hit.score),
        embed,
    }
}

impl ShadowedMemory {
    pub fn new(
        serving: Arc<dyn MemoryStore>,
        replica: Arc<dyn Erases>,
        lock: Arc<dyn ErasureLock>,
    ) -> Self {
        Self {
            serving,
            replica,
            lock,
            noter: None,
            reads: None,
        }
    }

    /// Shadow each recall through `reads`.
    pub fn reading(mut self, reads: Reads) -> Self {
        self.reads = Some(reads);
        self
    }

    /// A recall shadowed: embedded once, answered by Weaviate with that
    /// vector, and asked of the replica with it in the background. Not
    /// shadowed (Weaviate recalling as it always does) while either store
    /// takes no vector of the hub's model, as while Weaviate still serves
    /// the collection memories are moved from: no vector is handed across
    /// models. A query that cannot be embedded is Weaviate's alone, as it
    /// answers none. When embedding fails both are asked by keywords alone,
    /// as Weaviate is then.
    async fn recall_shadowed(
        &self,
        reads: &Reads,
        query: &str,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        use workforce_common::vectorizable::VectorizableText;
        let model = reads.embedder.model_id();
        let same = self.serving.query_model().as_deref() == Some(model)
            && reads.replica.query_model().as_deref() == Some(model);
        if !same || VectorizableText::try_new(query).is_err() {
            return self.serving.recall(query, filter).await;
        }
        let began = std::time::Instant::now();
        let vector = match reads.embedder.embed_query(query).await {
            Ok(vector) => Some(vector),
            Err(e) => {
                tracing::warn!(target: "embeddings", error = %format!("{e:#}"), "query embedding failed; recalling by keywords only");
                None
            }
        };
        let embed = began.elapsed();
        let began = std::time::Instant::now();
        let hits = self
            .serving
            .recall_with_vector(query, vector.clone(), filter.clone())
            .await?;
        let latency = began.elapsed();
        let mut hashed = blake3::Hasher::new();
        hashed.update(query.as_bytes());
        hashed.update(&[0]);
        hashed.update(filter.identity_id.as_deref().unwrap_or_default().as_bytes());
        let primary = Observed {
            ranked: ranked(&hits, Some(embed)),
            latency,
            query_hash: Some(hashed.finalize().to_hex().to_string()),
            labels: vec![
                ("same_model", "true".to_string()),
                (
                    "vector",
                    match vector {
                        Some(_) => "given",
                        None => "none",
                    }
                    .to_string(),
                ),
            ],
        };
        let replica = Arc::clone(&reads.replica);
        let query = query.to_string();
        reads.shadow.observe(primary, move || async move {
            let hits = workforce_memory::access_audit::as_shadow(
                replica.recall_with_vector(&query, vector, filter),
            )
            .await?;
            Ok(ranked(&hits, None))
        });
        Ok(hits)
    }

    /// Note each write Weaviate takes through `noter`.
    pub fn noting(mut self, noter: Noter) -> Self {
        self.noter = Some(noter);
        self
    }

    /// Note `id` once Weaviate took a write of it: `written` handed back
    /// as it came, noted only when `Ok`.
    fn noted<T>(&self, id: &MemoryId, written: Result<T>) -> Result<T> {
        if let (Ok(_), Some(noter)) = (&written, &self.noter) {
            noter.note(Note::One(id.clone()));
        }
        written
    }

    /// Run `erase` holding the replica's lock alone, letting it go after,
    /// whatever `erase` came to.
    async fn erasing<T>(&self, erase: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let held = self.lock.hold().await?;
        let erased = erase.await;
        let released = held.release().await;
        let erased = erased?;
        released
            .context("the memory replica's lock was lost during an erasure; retry the purge")?;
        Ok(erased)
    }
}

/// Context for a replica purge that failed.
const REPLICA_FAILED: &str = "erasing from vellev's replica of memories failed, so nothing was erased from Weaviate either: retry the purge once vellev answers";

#[async_trait]
impl MemoryStore for ShadowedMemory {
    fn cache_stats(&self) -> MemoryStoreCacheStats {
        self.serving.cache_stats()
    }
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        let id = self.serving.remember(memory).await?;
        if let Some(noter) = &self.noter {
            noter.note(Note::One(id.clone()));
        }
        Ok(id)
    }
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        match &self.reads {
            Some(reads) => self.recall_shadowed(reads, query, filter).await,
            None => self.serving.recall(query, filter).await,
        }
    }
    fn query_model(&self) -> Option<String> {
        self.serving.query_model()
    }
    fn recall_settings(&self) -> Option<workforce_memory::store::RecallSettings> {
        self.serving.recall_settings()
    }
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        self.serving.recall_with_vector(query, vector, filter).await
    }
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.serving.get(id).await
    }
    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.noted(id, self.serving.forget(id).await)
    }
    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.noted(id, self.serving.share(id, visibility).await)
    }
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        self.serving.count(filter).await
    }
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        self.serving.list(filter).await
    }
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        self.noted(id, self.serving.update_tags(id, tags).await)
    }
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.noted(id, self.serving.adjust_importance(id, delta).await)
    }
    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        self.serving.identity_memories(owner, identity).await
    }
    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
        self.serving.identity_counts(owner).await
    }
    async fn identities(&self, owner: &AgentId) -> Result<Vec<String>> {
        self.serving.identities(owner).await
    }

    /// The memories moved are resolved from Weaviate before the move (after
    /// it, they are filed under `to`, among others), and noted once it is
    /// made. Resolving them failing does not fail the move: the shadow never
    /// fails a write; it is counted as dropped, for the reconcile to find.
    /// A memory filed under `from` between the two is moved and not noted,
    /// and the reconcile finds it too.
    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        let Some(noter) = &self.noter else {
            return self.serving.reassign_identity(owner, from, to).await;
        };
        let resolved = self.serving.identity_memories(owner, from).await;
        let moved = self.serving.reassign_identity(owner, from, to).await?;
        match resolved {
            Ok(ids) => noter.note(Note::Many(ids)),
            Err(e) => {
                noter.dropped(moved.max(1));
                tracing::warn!(target: "shadow", error = %format!("{e:#}"), "the memories of a reassigned identity could not be resolved, so are not noted for vellev's replica; the reconcile will find them");
            }
        }
        Ok(moved)
    }

    /// The replica first, by identity and by every memory Weaviate files
    /// under it, then Weaviate; all under the replica's lock.
    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        self.erasing(async {
            let ids = self
                .serving
                .identity_memories(owner, identity)
                .await
                .context("finding the memories to erase in Weaviate")?;
            self.replica
                .purge_identity(owner, identity)
                .await
                .context(REPLICA_FAILED)?;
            self.replica
                .purge_memories(&ids)
                .await
                .context(REPLICA_FAILED)?;
            self.serving.purge_identity(owner, identity).await
        })
        .await
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.erasing(async {
            self.replica
                .purge_memories(ids)
                .await
                .context(REPLICA_FAILED)?;
            self.serving.purge_memories(ids).await
        })
        .await
    }

    async fn find_near_duplicate(
        &self,
        content: &str,
        filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        self.serving
            .find_near_duplicate(content, filter, min_similarity)
            .await
    }
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        self.noted(
            id,
            self.serving
                .record_recurrence(id, additional_message_ids)
                .await,
        )
    }
}

/// Whether a process notes the writes it makes for vellev's replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Noting {
    /// A hub: noted, unless the shadow is paused.
    Hub,
    /// A one-shot CLI process: never, for a noter could die with it before
    /// writing; the reconcile finds its writes.
    Off,
}

/// Whether a store notes its writes: a hub's, unless the shadow is paused.
pub fn wants_noter(noting: Noting, paused: bool) -> bool {
    noting == Noting::Hub && !paused
}

/// A process's noter: made once, at the first store that notes (a hub
/// resolves memories once per agent, and each must not have a task and a
/// pool of its own), and never waited on. Its pending list is made ready
/// in its own task, bounded; if that fails the noter says so once and
/// drops every note, and is not made again ([`spawn_noter_after`]).
///
/// [`spawn_noter_after`]: crate::memory_mirror::spawn_noter_after
pub struct Noters {
    noter: std::sync::OnceLock<Noter>,
    made: std::sync::atomic::AtomicUsize,
}

impl Noters {
    pub const fn new() -> Self {
        Self {
            noter: std::sync::OnceLock::new(),
            made: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The noter a store opened as `noting` under `config` notes through:
    /// none for a CLI process, or while the shadow is paused; otherwise the
    /// process's one, made now if it is the first. Never waits.
    pub fn for_store(&self, config: &WorkforceConfig, noting: Noting) -> Option<Noter> {
        if !wants_noter(noting, config.vellev.shadow.paused) {
            return None;
        }
        let noter = self.noter.get_or_init(|| {
            self.made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let notes = crate::memory_mirror::notes(config);
            crate::memory_mirror::spawn_noter_after(
                async move {
                    let notes: Arc<dyn crate::memory_mirror::NoteSink> = notes.await?;
                    eyre::Ok(notes)
                },
                crate::memory_mirror::NOTE_SETUP_LIMIT,
                crate::memory_mirror::NOTE_CAPACITY,
                crate::memory_mirror::NOTE_BATCH,
            )
        });
        Some(noter.clone())
    }

    /// How many noters were made: one at most.
    pub fn made(&self) -> usize {
        self.made.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// What the noter has done, once there is one.
    pub fn counts(&self) -> Option<Arc<crate::memory_mirror::NoteCounts>> {
        self.noter.get().map(Noter::counts)
    }
}

impl Default for Noters {
    fn default() -> Self {
        Self::new()
    }
}

/// This process's noter. Its task runs on the runtime of the first store
/// that notes: a store opened on a later runtime, once that one is gone,
/// gets a noter that drops every note (one runtime a process in a hub).
static NOTERS: Noters = Noters::new();

/// What this process's noter has done, once it has one.
pub fn noter_counts() -> Option<Arc<crate::memory_mirror::NoteCounts>> {
    NOTERS.counts()
}

/// The process's one lock for erasures, on a pool of its own.
static ERASURE_LOCK: std::sync::OnceLock<Arc<ReplicaLock>> = std::sync::OnceLock::new();

/// `serving` as the hub uses it while memories are shadowed: each purge
/// erasing from vellev's replica too, in the same call, under the
/// replica's lock. Nothing about the shadow's reads (paused, sampled, the
/// breaker) is consulted: erasure is not a shadow read.
///
/// With [`Noting::Hub`] and the shadow not paused, each other write
/// Weaviate takes is noted for the replica ([`Noters`]). Making the store
/// never waits on Postgres: a noter that cannot be made ready costs the
/// shadow its lag, never the hub, and is left to the reconcile.
pub async fn shadowed(
    config: &WorkforceConfig,
    serving: Arc<dyn MemoryStore>,
    noting: Noting,
) -> Result<Arc<dyn MemoryStore>> {
    let (replica, _) = crate::embeddings::shadow_stores(config).await?;
    let replica: Arc<dyn MemoryStore> = replica;
    let lock = match ERASURE_LOCK.get() {
        Some(lock) => Arc::clone(lock),
        None => {
            // Two first callers at once each build a pool; one is kept, and
            // the other, never connected (pools connect when asked), goes.
            let built = crate::memory_shadow::replica_lock(config)?;
            Arc::clone(ERASURE_LOCK.get_or_init(|| built))
        }
    };
    let lock = Exclusive {
        lock,
        wait: ERASURE_LOCK_WAIT,
        hold: ERASURE_LOCK_HOLD,
    };
    let store = ShadowedMemory::new(
        serving,
        Arc::new(Replica(Arc::clone(&replica))),
        Arc::new(lock),
    );
    let store = match noting {
        Noting::Hub => match reads(config, replica).await {
            Ok(reads) => store.reading(reads),
            Err(e) => {
                tracing::error!(target: "shadow", error = %format!("{e:#}"), "memories' recalls are not shadowed");
                store
            }
        },
        Noting::Off => store,
    };
    let store = match NOTERS.for_store(config, noting) {
        Some(noter) => store.noting(noter),
        None => store,
    };
    Ok(Arc::new(store))
}

/// The process's one read shadow of memories: one kernel, so that its
/// permits bound the shadow reads of the whole hub, not of each agent.
static READS: tokio::sync::OnceCell<Reads> = tokio::sync::OnceCell::const_new();

/// The read shadow of memories, with the hub's embedder, `replica` (when
/// first made), and the kernel at `[vellev.shadow.memories]`'s settings,
/// asking nothing while `[vellev.shadow] paused`. Making it never waits on
/// Postgres: the comparisons go to the hub's one sink once it is open
/// ([`Deferred`]), and until then are dropped and counted.
async fn reads(config: &WorkforceConfig, replica: Arc<dyn MemoryStore>) -> Result<Reads> {
    let reads = READS
        .get_or_try_init(|| made_reads(config, replica, crate::shadow::sink_for(config)))
        .await?;
    Ok(reads.clone())
}

/// [`reads`] made afresh, its sink opened by `opening` in the background:
/// never waited for here, so that a Postgres slow or down at boot holds up
/// no hub.
async fn made_reads<F>(
    config: &WorkforceConfig,
    replica: Arc<dyn MemoryStore>,
    opening: F,
) -> Result<Reads>
where
    F: std::future::Future<Output = Result<Arc<dyn workforce_common::shadow::ShadowSink>>>
        + Send
        + 'static,
{
    let (embedder, _) = crate::embeddings::from_config(config)
        .await?
        .ok_or_else(|| eyre::eyre!("[memory] backend = \"shadow\" needs [embeddings]"))?;
    let sink = Deferred::opening(opening, crate::memory_mirror::NOTE_SETUP_LIMIT);
    let shadow = memory_shadow_kernel(config, sink)?;
    Ok(Reads {
        embedder,
        replica,
        shadow: Arc::new(shadow),
    })
}

/// The kernel that shadows recalls of memories: `[vellev.shadow.memories]`'s
/// settings, paused as `[vellev.shadow] paused` says, writing to `sink`.
pub(crate) fn memory_shadow_kernel(
    config: &WorkforceConfig,
    sink: Arc<dyn workforce_common::shadow::ShadowSink>,
) -> Result<Shadow> {
    use workforce_common::settings::ShadowedStore;
    let settings = config.vellev.shadow.settings(ShadowedStore::Memories)?;
    let shadow = Shadow::new("memories", settings, sink)?;
    shadow.set_enabled(!config.vellev.shadow.paused);
    Ok(shadow)
}

/// A sink that is opened in the background: each comparison made before it
/// is open, or after it failed to open, is dropped and counted, never
/// waited for.
pub struct Deferred {
    sink: Arc<std::sync::OnceLock<Arc<dyn workforce_common::shadow::ShadowSink>>>,
    dropped: std::sync::atomic::AtomicU64,
}

impl Deferred {
    /// A sink that `open` makes, within `limit`; said once if it cannot.
    pub fn opening<F>(open: F, limit: Duration) -> Arc<Self>
    where
        F: std::future::Future<Output = Result<Arc<dyn workforce_common::shadow::ShadowSink>>>
            + Send
            + 'static,
    {
        let deferred = Arc::new(Self {
            sink: Arc::default(),
            dropped: std::sync::atomic::AtomicU64::new(0),
        });
        let sink = Arc::clone(&deferred.sink);
        tokio::spawn(async move {
            match tokio::time::timeout(limit, open).await {
                Ok(Ok(opened)) => {
                    let _ = sink.set(opened);
                }
                Ok(Err(e)) => {
                    tracing::error!(target: "shadow", error = %format!("{e:#}"), "memories' recalls are compared but not kept: the shadow tables cannot be opened")
                }
                Err(_) => {
                    tracing::error!(target: "shadow", ?limit, "memories' recalls are compared but not kept: the shadow tables were not open in time")
                }
            }
        });
        deferred
    }

    /// Comparisons dropped for want of an open sink.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl workforce_common::shadow::ShadowSink for Deferred {
    fn record(&self, comparison: workforce_common::shadow::Comparison) {
        match self.sink.get() {
            Some(sink) => sink.record(comparison),
            None => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// vellev's replica as a purge erases from it.
struct Replica(Arc<dyn MemoryStore>);

#[async_trait]
impl Erases for Replica {
    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        self.0.purge_identity(owner, identity).await
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.0.purge_memories(ids).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::MemoryLayer;

    /// vellev's replica, as far as a purge sees it: each memory filed by
    /// owner and identity as vellev last had it, and failing while told to.
    #[derive(Default)]
    struct Copies {
        held: Mutex<Vec<(String, String, MemoryId)>>,
        down: AtomicBool,
        /// What was asked of it, in order.
        asked: Mutex<Vec<&'static str>>,
    }

    impl Copies {
        fn holds(&self, id: &MemoryId) -> bool {
            self.held.lock().unwrap().iter().any(|(_, _, m)| m == id)
        }

        fn up(&self) -> Result<()> {
            match self.down.load(Ordering::SeqCst) {
                true => Err(eyre::eyre!("vellev answered 503")),
                false => Ok(()),
            }
        }
    }

    #[async_trait]
    impl Erases for Copies {
        async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
            self.asked.lock().unwrap().push("replica");
            self.up()?;
            let mut held = self.held.lock().unwrap();
            let before = held.len();
            held.retain(|(o, i, _)| !(o == owner.as_str() && i == identity));
            Ok(before - held.len())
        }

        async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
            self.asked.lock().unwrap().push("replica");
            self.up()?;
            let mut held = self.held.lock().unwrap();
            let before = held.len();
            held.retain(|(_, _, m)| !ids.contains(m));
            Ok(before - held.len())
        }
    }

    /// A lock always free, counting its holds and releases.
    #[derive(Default)]
    struct Free {
        held: AtomicUsize,
        released: Arc<AtomicUsize>,
    }

    struct Release(Arc<AtomicUsize>);

    #[async_trait]
    impl Held for Release {
        async fn release(self: Box<Self>) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl ErasureLock for Free {
        async fn hold(&self) -> Result<Box<dyn Held>> {
            self.held.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Release(Arc::clone(&self.released))))
        }
    }

    fn agent() -> AgentId {
        AgentId::new("agent")
    }

    /// Whether the lock is held, and what each purge saw of it.
    #[derive(Default)]
    struct Watch {
        held: AtomicBool,
        seen: Mutex<Vec<(&'static str, bool)>>,
    }

    impl Watch {
        fn saw(&self, what: &'static str) {
            let held = self.held.load(Ordering::SeqCst);
            self.seen.lock().unwrap().push((what, held));
        }
    }

    /// A lock that says, while held, that it is.
    struct Watched(Arc<Watch>);

    struct Unwatch(Arc<Watch>);

    #[async_trait]
    impl Held for Unwatch {
        async fn release(self: Box<Self>) -> Result<()> {
            self.0.held.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl ErasureLock for Watched {
        async fn hold(&self) -> Result<Box<dyn Held>> {
            self.0.held.store(true, Ordering::SeqCst);
            Ok(Box::new(Unwatch(Arc::clone(&self.0))))
        }
    }

    #[async_trait]
    impl Erases for Watched {
        async fn purge_identity(&self, _: &AgentId, _: &str) -> Result<usize> {
            self.0.saw("replica by identity");
            Ok(0)
        }

        async fn purge_memories(&self, _: &[MemoryId]) -> Result<usize> {
            self.0.saw("replica by memory");
            Ok(0)
        }
    }

    /// Weaviate's store, noting whether the lock was held at each purge.
    struct Serving(InMemoryStore, Arc<Watch>);

    #[async_trait]
    impl MemoryStore for Serving {
        async fn remember(&self, memory: Memory) -> Result<MemoryId> {
            self.0.remember(memory).await
        }
        async fn recall(
            &self,
            query: &str,
            filter: MemoryFilter,
        ) -> Result<Vec<MemorySearchResult>> {
            self.0.recall(query, filter).await
        }
        async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
            self.0.get(id).await
        }
        async fn forget(&self, id: &MemoryId) -> Result<()> {
            self.0.forget(id).await
        }
        async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
            self.0.share(id, visibility).await
        }
        async fn count(&self, filter: MemoryFilter) -> Result<usize> {
            self.0.count(filter).await
        }
        async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
            self.0.list(filter).await
        }
        async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
            self.0.update_tags(id, tags).await
        }
        async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
            self.0.adjust_importance(id, delta).await
        }
        async fn identity_memories(
            &self,
            owner: &AgentId,
            identity: &str,
        ) -> Result<Vec<MemoryId>> {
            self.0.identity_memories(owner, identity).await
        }
        async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
            self.0.identity_counts(owner).await
        }
        async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
            self.0.reassign_identity(owner, from, to).await
        }
        async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
            self.1.saw("serving by identity");
            self.0.purge_identity(owner, identity).await
        }
        async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
            self.1.saw("serving by memory");
            self.0.purge_memories(ids).await
        }
        async fn find_near_duplicate(
            &self,
            content: &str,
            filter: MemoryFilter,
            min_similarity: f32,
        ) -> Result<Option<MemorySearchResult>> {
            self.0
                .find_near_duplicate(content, filter, min_similarity)
                .await
        }
        async fn record_recurrence(
            &self,
            id: &MemoryId,
            additional_message_ids: Vec<String>,
        ) -> Result<u32> {
            self.0.record_recurrence(id, additional_message_ids).await
        }
    }

    /// Both purges, of the replica and of Weaviate, run while the lock is
    /// held: let go before either, a reconcile page could read Weaviate
    /// before the erasure and write the memory back into the replica
    /// after it.
    #[tokio::test]
    async fn the_lock_is_held_through_both_purges() {
        let watch = Arc::new(Watch::default());
        let serving = Arc::new(Serving(InMemoryStore::new(), Arc::clone(&watch)));
        let erased = serving
            .remember(
                Memory::new("about bob", MemoryLayer::Episodic, agent()).with_identity_id("bob"),
            )
            .await
            .unwrap();
        let store = ShadowedMemory::new(
            serving as Arc<dyn MemoryStore>,
            Arc::new(Watched(Arc::clone(&watch))) as Arc<dyn Erases>,
            Arc::new(Watched(Arc::clone(&watch))) as Arc<dyn ErasureLock>,
        );

        store.purge_identity(&agent(), "bob").await.unwrap();
        store.purge_memories(&[erased]).await.unwrap();

        assert_eq!(
            watch.seen.lock().unwrap().clone(),
            vec![
                ("replica by identity", true),
                ("replica by memory", true),
                ("serving by identity", true),
                ("replica by memory", true),
                ("serving by memory", true),
            ]
        );
        assert!(!watch.held.load(Ordering::SeqCst), "let go after");
    }

    /// A memory moved to bob in Weaviate but not yet in vellev, which still
    /// files it under alice. Erasing bob with vellev down fails, naming
    /// vellev, and leaves Weaviate as it was; the retry then finds the
    /// memory in Weaviate again and erases vellev's copy, though vellev
    /// files it under another identity. Erased from Weaviate first, the
    /// retry would find nothing there, and the copy would outlive the
    /// erasure.
    #[tokio::test]
    async fn a_purge_erases_the_replica_first_so_a_retry_still_finds_what_moved() {
        let serving = Arc::new(InMemoryStore::new());
        let moved = serving
            .remember(
                Memory::new("moved to bob", MemoryLayer::Episodic, agent()).with_identity_id("bob"),
            )
            .await
            .unwrap();
        let copies = Arc::new(Copies::default());
        copies
            .held
            .lock()
            .unwrap()
            .push(("agent".into(), "alice".into(), moved.clone()));
        let lock = Arc::new(Free::default());
        let store = ShadowedMemory::new(
            Arc::clone(&serving) as Arc<dyn MemoryStore>,
            Arc::clone(&copies) as Arc<dyn Erases>,
            Arc::clone(&lock) as Arc<dyn ErasureLock>,
        );

        copies.down.store(true, Ordering::SeqCst);
        let failed = store.purge_identity(&agent(), "bob").await.unwrap_err();
        let said = format!("{failed:#}");
        assert!(said.contains("vellev") && said.contains("retry"), "{said}");
        assert_eq!(
            serving.identity_memories(&agent(), "bob").await.unwrap(),
            std::slice::from_ref(&moved),
            "Weaviate untouched while the replica could not be erased"
        );
        assert!(copies.holds(&moved));

        copies.down.store(false, Ordering::SeqCst);
        store.purge_identity(&agent(), "bob").await.unwrap();
        assert!(
            !copies.holds(&moved),
            "vellev's copy erased, under whatever identity"
        );
        assert!(serving
            .identity_memories(&agent(), "bob")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(lock.held.load(Ordering::SeqCst), 2);
        assert_eq!(
            lock.released.load(Ordering::SeqCst),
            2,
            "let go, failed or not"
        );
    }

    /// Purging memories by id erases them from the replica, then Weaviate;
    /// with vellev down, from neither.
    #[tokio::test]
    async fn purged_memories_go_from_the_replica_then_weaviate_or_from_neither() {
        let serving = Arc::new(InMemoryStore::new());
        let id = serving
            .remember(Memory::new("a chunk", MemoryLayer::Episodic, agent()))
            .await
            .unwrap();
        let copies = Arc::new(Copies::default());
        copies
            .held
            .lock()
            .unwrap()
            .push(("agent".into(), "".into(), id.clone()));
        let store = ShadowedMemory::new(
            Arc::clone(&serving) as Arc<dyn MemoryStore>,
            Arc::clone(&copies) as Arc<dyn Erases>,
            Arc::new(Free::default()),
        );
        copies.down.store(true, Ordering::SeqCst);
        assert!(store
            .purge_memories(std::slice::from_ref(&id))
            .await
            .is_err());
        assert!(serving.get(&id).await.unwrap().is_some());
        copies.down.store(false, Ordering::SeqCst);
        assert_eq!(
            store
                .purge_memories(std::slice::from_ref(&id))
                .await
                .unwrap(),
            1
        );
        assert!(!copies.holds(&id));
        assert!(serving.get(&id).await.unwrap().is_none());
    }

    /// A purge waits for a reconcile page holding the replica's lock, and
    /// past its wait fails, naming the lock, having erased nothing; once
    /// the page is done it goes ahead.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_waits_for_a_page_in_hand_and_erases_nothing_past_its_wait() {
        use workforce_common::telemetry_postgres::PostgresTelemetryStore;
        let ran = Arc::new(AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresTelemetryStore::build_pool,
            |pool| async move {
                workforce_common::shadow_postgres::ShadowStore::with_pool(pool.clone())
                    .await
                    .unwrap();
                let lock = Arc::new(ReplicaLock::new(pool));
                let serving = Arc::new(InMemoryStore::new());
                let id = serving
                    .remember(
                        Memory::new("a note", MemoryLayer::Episodic, agent())
                            .with_identity_id("bob"),
                    )
                    .await
                    .unwrap();
                let copies = Arc::new(Copies::default());
                copies
                    .held
                    .lock()
                    .unwrap()
                    .push(("agent".into(), "bob".into(), id.clone()));
                let store = ShadowedMemory::new(
                    Arc::clone(&serving) as Arc<dyn MemoryStore>,
                    Arc::clone(&copies) as Arc<dyn Erases>,
                    Arc::new(Exclusive {
                        lock: Arc::clone(&lock),
                        wait: Duration::from_millis(300),
                        hold: Duration::from_secs(60),
                    }),
                );

                let page = lock
                    .shared(Duration::from_secs(1), Duration::from_secs(60))
                    .await
                    .unwrap();
                let waited = store.purge_identity(&agent(), "bob").await.unwrap_err();
                assert!(
                    format!("{waited:#}").contains("replica's lock"),
                    "{waited:#}"
                );
                assert!(
                    copies.asked.lock().unwrap().is_empty(),
                    "nothing asked of vellev"
                );
                assert!(serving.get(&id).await.unwrap().is_some(), "nor of Weaviate");

                page.release().await.unwrap();
                store.purge_identity(&agent(), "bob").await.unwrap();
                assert!(!copies.holds(&id));
                assert!(serving.get(&id).await.unwrap().is_none());
                lock.shared(Duration::from_secs(1), Duration::from_secs(60))
                    .await
                    .expect("the erasure let the lock go")
                    .release()
                    .await
                    .unwrap();
                reached.store(true, Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }

    /// Everything but a purge is Weaviate's alone: the replica is never
    /// asked, and so never slows or fails it.
    #[tokio::test]
    async fn only_a_purge_reaches_the_replica() {
        let copies = Arc::new(Copies::default());
        copies.down.store(true, Ordering::SeqCst);
        let lock = Arc::new(Free::default());
        let store = ShadowedMemory::new(
            Arc::new(InMemoryStore::new()),
            Arc::clone(&copies) as Arc<dyn Erases>,
            Arc::clone(&lock) as Arc<dyn ErasureLock>,
        );
        let id = store
            .remember(Memory::new("kept", MemoryLayer::Episodic, agent()).with_identity_id("bob"))
            .await
            .unwrap();
        store.forget(&id).await.unwrap();
        store
            .reassign_identity(&agent(), "bob", "carol")
            .await
            .unwrap();
        store.identity_counts(&agent()).await.unwrap();
        assert!(copies.asked.lock().unwrap().is_empty());
        assert_eq!(lock.held.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
#[path = "memory_reads_tests.rs"]
mod reads_tests;

#[cfg(test)]
#[path = "memory_shadow_e2e_tests.rs"]
mod e2e_tests;

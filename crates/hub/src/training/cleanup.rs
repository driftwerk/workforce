//! Startup orphan sweep for training corpora.
//!
//! Cross-checks `training_chunks` (SQL) against the memory store
//! (Weaviate / InMemoryStore). Two passes:
//!
//! 1. SQL chunks whose Memory backing is missing → drop the chunk row.
//! 2. Memory rows tagged `training` not referenced by any SQL chunk
//!    → forget the memory.
//!
//! Both passes are idempotent and partial-failure tolerant — a hung
//! Weaviate or a stale row never blocks startup. The sweep fires
//! exactly once per hub process via a module-level `OnceCell`, so
//! the multiple `AgentService::build` calls (one per agent) don't
//! re-trigger it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use once_cell::sync::OnceCell;
use tokio::task::JoinHandle;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{MemoryFilter, MemoryId};
use workforce_training::TrainingStore;

use super::TRAINING_TAG;

/// Summary of one sweep. Surfaced separately from `spawn_sweep` so
/// tests can assert behaviour without observing the spawned task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub chunks_examined: usize,
    pub chunks_dropped: usize,
    pub memories_examined: usize,
    pub memories_forgotten: usize,
}

/// Knobs passed to `sweep_orphans`. Mirrors the
/// `[training.cleanup]` config block from `WorkforceConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepOptions {
    pub verify_only: bool,
}

static SWEEP_FIRED: OnceCell<()> = OnceCell::new();

/// Spawn the orphan sweep on a tokio task with a hard timeout. The
/// first caller in the process wins; subsequent calls are no-ops, so
/// every per-agent `AgentService::build` can call this freely. In a
/// clustered deployment the sweep additionally waits for the election
/// to settle and runs only on the leader — it deletes from shared
/// training/memory stores, so one sweeper per fleet.
pub fn spawn_sweep(
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    options: SweepOptions,
    timeout: Duration,
    leader_gate: Arc<crate::leader_gate::LeaderGate>,
) -> Option<JoinHandle<()>> {
    if SWEEP_FIRED.set(()).is_err() {
        return None;
    }
    Some(tokio::spawn(async move {
        if !leader_gate.wait_boot_singleton().await {
            tracing::info!("training: orphan sweep skipped — not the leader");
            return;
        }
        match tokio::time::timeout(timeout, sweep_orphans(training, memory, options)).await {
            Ok(Ok(report)) => tracing::info!(
                chunks_examined = report.chunks_examined,
                chunks_dropped = report.chunks_dropped,
                memories_examined = report.memories_examined,
                memories_forgotten = report.memories_forgotten,
                verify_only = options.verify_only,
                "training: orphan sweep complete",
            ),
            Ok(Err(e)) => tracing::warn!(error = %e, "training: orphan sweep failed"),
            Err(_) => tracing::warn!(
                timeout_secs = timeout.as_secs(),
                "training: orphan sweep timed out",
            ),
        }
    }))
}

/// Run the sweep synchronously. Public so the test suite can drive
/// it without going through `tokio::spawn`.
pub async fn sweep_orphans(
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    options: SweepOptions,
) -> Result<CleanupReport> {
    if !memory_backend_ready(&*memory, READINESS_PROBES, READINESS_PROBE_PAUSE).await {
        tracing::warn!(
            "training cleanup: memory backend unreachable after readiness probes; skipping the \
             orphan sweep this boot rather than acting on a recovering store",
        );
        return Ok(CleanupReport::default());
    }
    let chunks_report = drop_chunks_missing_memory(&*training, &*memory, options).await?;
    let memories_report = forget_memories_missing_chunk(&*training, &*memory, options).await?;
    Ok(CleanupReport {
        chunks_examined: chunks_report.chunks_examined,
        chunks_dropped: chunks_report.chunks_dropped,
        memories_examined: memories_report.memories_examined,
        memories_forgotten: memories_report.memories_forgotten,
    })
}

/// Fraction of a sweep's examined items that, if they all appear
/// orphaned at once, marks the result as a probable memory-store desync
/// rather than a genuine set of orphans. An empty or freshly-recreated
/// Weaviate collection answers every `memory.get` with `Ok(None)` (not an
/// `Err`), so without this guard a single transient Weaviate outage drives
/// the sweep to delete *every* training chunk — permanently destroying the
/// canonical rows (seen in production). The asymmetry is
/// deliberate: a wrongly-dropped chunk is unrecoverable, whereas a
/// wrongly-kept orphan is harmless (it renders nothing and the next sweep,
/// once the store is healthy again, removes it).
const BULK_LOSS_ABORT_FRACTION: f64 = 0.5;

/// Minimum sample before the *fractional* bulk-loss check engages. A total
/// wipe (every examined item orphaned) is always treated as a store-wide
/// outage regardless of size — this floor only governs the partial-majority
/// case, where a genuine handful of orphans in a tiny store should still be
/// cleaned rather than mistaken for an outage.
const BULK_LOSS_MIN_SAMPLE: usize = 5;

/// True when the orphan set looks like a store-wide memory outage rather
/// than genuine orphans: either *every* examined item is orphaned (a total
/// wipe, at any size — an empty or recreated Weaviate answers every
/// `memory.get` with `Ok(None)`, so even a single-agent corpus of one chunk
/// is protected), or a majority of a non-trivial sample is. An empty sweep
/// is never bulk loss.
fn is_bulk_loss(missing: usize, examined: usize) -> bool {
    if examined == 0 {
        return false;
    }
    if missing == examined {
        return true;
    }
    examined >= BULK_LOSS_MIN_SAMPLE && missing as f64 >= examined as f64 * BULK_LOSS_ABORT_FRACTION
}

/// Readiness probes attempted before the destructive passes, and the pause
/// between them. A hub that boots while its Weaviate is still restarting
/// (the 2026-06-24 incident) must not sweep against a backend that is
/// unreachable or mid-recovery: probing a trivial query gives a just-
/// restarted store time to load its shards, and a backend that never
/// answers across the window skips the sweep rather than risk acting on a
/// partially-loaded view.
const READINESS_PROBES: usize = 5;
const READINESS_PROBE_PAUSE: Duration = Duration::from_secs(2);

/// Probe the memory backend until a trivial query succeeds. Returns `true`
/// once a probe answers; `false` only if every attempt across the window
/// errors (the backend is down). An empty store answers `Ok(0)` and is
/// therefore ready — the empty/desync case is handled downstream by
/// [`is_bulk_loss`], not here.
async fn memory_backend_ready(memory: &dyn MemoryStore, probes: usize, pause: Duration) -> bool {
    for attempt in 1..=probes {
        match memory.count(orphan_scan_filter()).await {
            Ok(_) => return true,
            Err(e) => {
                tracing::warn!(
                    attempt,
                    probes,
                    error = %e,
                    "training cleanup: memory backend not ready; deferring orphan sweep",
                );
                if attempt < probes {
                    tokio::time::sleep(pause).await;
                }
            }
        }
    }
    false
}

async fn drop_chunks_missing_memory(
    training: &dyn TrainingStore,
    memory: &dyn MemoryStore,
    options: SweepOptions,
) -> Result<CleanupReport> {
    let corpora = training.list_all_corpora()?;
    let mut examined = 0usize;
    let mut orphaned: Vec<(uuid::Uuid, String)> = Vec::new();
    for corpus in &corpora {
        let chunks = training.list_chunks_for_corpus(corpus.id)?;
        examined += chunks.len();
        for chunk in chunks {
            let mem_id = MemoryId(chunk.memory_id.clone());
            let still_present = match memory.get(&mem_id).await {
                Ok(Some(m)) => !m.deleted,
                Ok(None) => false,
                Err(e) => {
                    tracing::warn!(
                        memory_id = %chunk.memory_id,
                        error = %e,
                        "training cleanup: memory.get failed; leaving chunk in place",
                    );
                    true
                }
            };
            if !still_present {
                orphaned.push((chunk.id, chunk.memory_id));
            }
        }
    }

    let mut report = CleanupReport {
        chunks_examined: examined,
        ..Default::default()
    };

    if is_bulk_loss(orphaned.len(), examined) {
        tracing::warn!(
            missing = orphaned.len(),
            examined,
            verify_only = options.verify_only,
            "training cleanup: {}/{} chunks reference missing memories — suspected memory-store \
             desync (e.g. an empty or recreated Weaviate collection), not real orphans; skipping \
             the destructive pass and leaving chunk rows in place",
            orphaned.len(),
            examined,
        );
        return Ok(report);
    }

    for (chunk_id, memory_id) in orphaned {
        tracing::info!(
            chunk_id = %chunk_id,
            memory_id = %memory_id,
            verify_only = options.verify_only,
            "training cleanup: chunk references missing memory",
        );
        if options.verify_only {
            report.chunks_dropped += 1;
            continue;
        }
        match training.delete_chunk(chunk_id) {
            Ok(_) => report.chunks_dropped += 1,
            Err(e) => tracing::warn!(
                chunk_id = %chunk_id,
                error = %e,
                "training cleanup: failed to delete orphan chunk",
            ),
        }
    }
    Ok(report)
}

async fn forget_memories_missing_chunk(
    training: &dyn TrainingStore,
    memory: &dyn MemoryStore,
    options: SweepOptions,
) -> Result<CleanupReport> {
    let referenced: HashSet<String> = training.list_all_chunk_memory_ids()?.into_iter().collect();
    let candidates = memory.list(orphan_scan_filter()).await?;
    let examined = candidates.len();
    let unbacked: Vec<(MemoryId, String)> = candidates
        .into_iter()
        .filter(|mem| !referenced.contains(&mem.id.0))
        .map(|mem| (mem.id, mem.owner.0))
        .collect();

    let mut report = CleanupReport {
        memories_examined: examined,
        ..Default::default()
    };

    if is_bulk_loss(unbacked.len(), examined) {
        tracing::warn!(
            missing = unbacked.len(),
            examined,
            verify_only = options.verify_only,
            "training cleanup: {}/{} training memories have no backing chunk — suspected SQL \
             chunk-table desync, not real orphans; skipping the forget pass and leaving memories \
             in place",
            unbacked.len(),
            examined,
        );
        return Ok(report);
    }

    for (mem_id, owner) in unbacked {
        tracing::info!(
            memory_id = %mem_id.0,
            owner = %owner,
            verify_only = options.verify_only,
            "training cleanup: memory has no backing chunk",
        );
        if options.verify_only {
            report.memories_forgotten += 1;
            continue;
        }
        match memory.forget(&mem_id).await {
            Ok(()) => report.memories_forgotten += 1,
            Err(e) => tracing::warn!(
                memory_id = %mem_id.0,
                error = %e,
                "training cleanup: failed to forget orphan memory",
            ),
        }
    }
    Ok(report)
}

/// Filter that selects every training-tagged Memory row across every
/// agent. `system_unscoped` opts past the DW-518 isolation gate the
/// same way `training_recall_filter` does — we have no per-user
/// identity here and need a cross-agent view.
fn orphan_scan_filter() -> MemoryFilter {
    MemoryFilter {
        owner: None,
        tags: Some(vec![TRAINING_TAG.to_string()]),
        system_unscoped: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::types::AgentId as _AgentId;
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::{Memory, MemoryLayer, MemorySource, Visibility};
    use workforce_training::sqlite::SqliteTrainingStore;
    use workforce_training::{
        Chunk, ChunkIdentity, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind,
    };

    fn opts() -> SweepOptions {
        SweepOptions { verify_only: false }
    }

    fn dry_run() -> SweepOptions {
        SweepOptions { verify_only: true }
    }

    fn corpus(agent: &str, name: &str) -> Corpus {
        Corpus {
            id: uuid::Uuid::new_v4(),
            agent_id: agent.into(),
            name: name.into(),
            scope: CorpusScope::Agent,
            created_at: chrono::Utc::now(),
            created_by: "test".into(),
        }
    }

    fn document(corpus_id: uuid::Uuid, filename: &str) -> Document {
        Document {
            id: uuid::Uuid::new_v4(),
            corpus_id,
            original_filename: filename.into(),
            mime_type: "application/json".into(),
            size_bytes: 100,
            storage_uri: format!("file://tmp/{filename}"),
            source_kind: SourceKind::Cli,
            source_ref: None,
            processing_status: ProcessingStatus::Ready,
            processing_error: None,
            uploaded_at: chrono::Utc::now(),
            uploaded_by: "test".into(),
            page_count: None,
        }
    }

    fn training_memory(agent: &str, body: &str) -> Memory {
        let mut m = Memory::new(
            body.to_string(),
            MemoryLayer::Organisation,
            _AgentId(agent.to_string()),
        );
        m.visibility = Visibility::Global;
        m.importance = 0.9;
        m.source = MemorySource::Curated;
        m.tags = vec![TRAINING_TAG.to_string()];
        m
    }

    async fn build() -> (Arc<dyn TrainingStore>, Arc<dyn MemoryStore>) {
        (
            Arc::new(SqliteTrainingStore::in_memory().unwrap()),
            Arc::new(InMemoryStore::new()),
        )
    }

    async fn write_chunk(
        training: &dyn TrainingStore,
        memory: &dyn MemoryStore,
        corpus: &Corpus,
        document_id: uuid::Uuid,
        body: &str,
    ) -> Chunk {
        let mem = training_memory(&corpus.agent_id, body);
        let mem_id = memory.remember(mem).await.unwrap();
        let chunk = Chunk {
            id: uuid::Uuid::new_v4(),
            document_id,
            chunk_index: 0,
            content_hash: ChunkIdentity::hash(corpus.id, body),
            memory_id: mem_id.0,
            locator: "x".into(),
            char_start: 0,
            char_end: body.len() as i32,
        };
        training
            .create_chunks(std::slice::from_ref(&chunk))
            .unwrap();
        chunk
    }

    #[tokio::test]
    async fn sweep_no_op_when_stores_aligned() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        let d = document(c.id, "x.json");
        t.create_document(&d).unwrap();
        write_chunk(&*t, &*m, &c, d.id, "hello").await;

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.chunks_dropped, 0);
        assert_eq!(r.memories_forgotten, 0);
        assert_eq!(r.chunks_examined, 1);
        assert_eq!(r.memories_examined, 1);
    }

    #[tokio::test]
    async fn sweep_drops_chunks_with_missing_memory() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        // A healthy chunk alongside the orphan keeps this a partial (real)
        // orphan rather than a total wipe the bulk-loss guard would refuse.
        let d_keep = document(c.id, "keep.json");
        t.create_document(&d_keep).unwrap();
        write_chunk(&*t, &*m, &c, d_keep.id, "keep").await;
        let d_drop = document(c.id, "drop.json");
        t.create_document(&d_drop).unwrap();
        let orphan = write_chunk(&*t, &*m, &c, d_drop.id, "drop").await;
        m.forget(&MemoryId(orphan.memory_id.clone())).await.unwrap();

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.chunks_dropped, 1);
        assert_eq!(t.list_chunks_for_corpus(c.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sweep_forgets_orphan_training_memories() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        let d = document(c.id, "x.json");
        t.create_document(&d).unwrap();
        let chunk = write_chunk(&*t, &*m, &c, d.id, "kept").await;

        let orphan_id = m.remember(training_memory("mark", "stray")).await.unwrap();

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.memories_forgotten, 1);
        assert!(is_forgotten(&*m, &orphan_id).await);
        assert!(!is_forgotten(&*m, &MemoryId(chunk.memory_id.clone())).await);
    }

    #[tokio::test]
    async fn sweep_leaves_non_training_memories_alone() {
        let (t, m) = build().await;
        let mut other = Memory::new(
            "untouched".to_string(),
            MemoryLayer::Episodic,
            _AgentId("mark".to_string()),
        );
        other.tags = vec!["lessons".to_string()];
        let other_id = m.remember(other).await.unwrap();

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.memories_forgotten, 0);
        assert!(!is_forgotten(&*m, &other_id).await);
    }

    /// `MemoryStore::forget` is a soft-delete (the row stays so an
    /// audit trail is preserved); `get` returns the row regardless.
    /// The cleanup's "missing memory" check uses `Memory.deleted`.
    async fn is_forgotten(m: &dyn MemoryStore, id: &MemoryId) -> bool {
        m.get(id).await.unwrap().map(|x| x.deleted).unwrap_or(true)
    }

    #[tokio::test]
    async fn sweep_verify_only_counts_without_mutating() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        let d_keep = document(c.id, "keep.json");
        t.create_document(&d_keep).unwrap();
        write_chunk(&*t, &*m, &c, d_keep.id, "keep").await;
        let d_drop = document(c.id, "drop.json");
        t.create_document(&d_drop).unwrap();
        let orphan = write_chunk(&*t, &*m, &c, d_drop.id, "drop").await;
        m.forget(&MemoryId(orphan.memory_id.clone())).await.unwrap();

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), dry_run())
            .await
            .unwrap();
        assert_eq!(r.chunks_dropped, 1);
        assert_eq!(t.list_chunks_for_corpus(c.id).unwrap().len(), 2);
    }

    /// Create `n` documents each backed by one chunk + one memory under
    /// `corpus`, so a sweep examines a store-wide sample.
    async fn write_n_chunks(
        training: &Arc<dyn TrainingStore>,
        memory: &Arc<dyn MemoryStore>,
        corpus: &Corpus,
        n: usize,
    ) -> Vec<Chunk> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let d = document(corpus.id, &format!("doc{i}.json"));
            training.create_document(&d).unwrap();
            out.push(write_chunk(&**training, &**memory, corpus, d.id, &format!("body{i}")).await);
        }
        out
    }

    #[test]
    fn bulk_loss_guard_boundaries() {
        assert!(!is_bulk_loss(0, 0), "an empty sweep is never bulk loss");
        assert!(is_bulk_loss(1, 1), "a total wipe of a single-chunk store");
        assert!(
            is_bulk_loss(3, 3),
            "a total wipe of a small single-agent store"
        );
        assert!(
            is_bulk_loss(4, 4),
            "a total wipe, even below the fractional floor"
        );
        assert!(is_bulk_loss(5, 5), "store-wide wipe at scale");
        assert!(is_bulk_loss(3, 5), "60% of a non-trivial sample");
        assert!(!is_bulk_loss(2, 5), "40% < 50%");
        assert!(
            !is_bulk_loss(2, 3),
            "a minority orphan in a tiny store is real"
        );
        assert!(!is_bulk_loss(1, 100), "a tiny minority is a real orphan");
    }

    /// A `MemoryStore` whose every call errors — stands in for a Weaviate
    /// that is unreachable or still restarting. Only `count` is exercised
    /// by the readiness probe; nothing else runs once the gate trips.
    struct UnreadyStore;

    #[async_trait::async_trait]
    impl MemoryStore for UnreadyStore {
        async fn remember(&self, _: Memory) -> Result<MemoryId> {
            unimplemented!()
        }
        async fn recall(
            &self,
            _: &str,
            _: MemoryFilter,
        ) -> Result<Vec<workforce_memory::types::MemorySearchResult>> {
            unimplemented!()
        }
        async fn get(&self, _: &MemoryId) -> Result<Option<Memory>> {
            unimplemented!()
        }
        async fn forget(&self, _: &MemoryId) -> Result<()> {
            unimplemented!()
        }
        async fn share(&self, _: &MemoryId, _: Visibility) -> Result<()> {
            unimplemented!()
        }
        async fn count(&self, _: MemoryFilter) -> Result<usize> {
            Err(eyre::eyre!("backend unreachable"))
        }
        async fn list(&self, _: MemoryFilter) -> Result<Vec<Memory>> {
            unimplemented!()
        }
        async fn update_tags(&self, _: &MemoryId, _: Vec<String>) -> Result<()> {
            unimplemented!()
        }
        async fn adjust_importance(&self, _: &MemoryId, _: f32) -> Result<f32> {
            unimplemented!()
        }
        async fn identity_memories(
            &self,
            _: &workforce_core::types::AgentId,
            _: &str,
        ) -> Result<Vec<MemoryId>> {
            unimplemented!()
        }
        async fn identity_counts(
            &self,
            _: &workforce_core::types::AgentId,
        ) -> Result<std::collections::BTreeMap<String, usize>> {
            unimplemented!()
        }
        async fn reassign_identity(
            &self,
            _: &workforce_core::types::AgentId,
            _: &str,
            _: &str,
        ) -> Result<usize> {
            unimplemented!()
        }
        async fn purge_identity(
            &self,
            _: &workforce_core::types::AgentId,
            _: &str,
        ) -> Result<usize> {
            unimplemented!()
        }
        async fn purge_memories(&self, _: &[MemoryId]) -> Result<usize> {
            unimplemented!()
        }
        async fn record_recurrence(&self, _: &MemoryId, _: Vec<String>) -> Result<u32> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn readiness_probe_passes_for_reachable_store() {
        let m = InMemoryStore::new();
        assert!(memory_backend_ready(&m, 1, Duration::ZERO).await);
    }

    #[tokio::test]
    async fn readiness_probe_fails_for_unreachable_store() {
        let m = UnreadyStore;
        assert!(!memory_backend_ready(&m, 3, Duration::ZERO).await);
    }

    /// The incident: a Weaviate restart returns an empty collection, so
    /// every chunk's memory reads as `Ok(None)`. The sweep must recognise
    /// the store-wide signature and refuse to delete the canonical rows.
    #[tokio::test]
    async fn sweep_aborts_drop_on_store_wide_memory_loss() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        let chunks = write_n_chunks(&t, &m, &c, 5).await;
        for ch in &chunks {
            m.forget(&MemoryId(ch.memory_id.clone())).await.unwrap();
        }

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.chunks_examined, 5);
        assert_eq!(r.chunks_dropped, 0, "bulk loss must not delete chunks");
        assert_eq!(t.list_chunks_for_corpus(c.id).unwrap().len(), 5);
    }

    /// A genuine minority of orphans (one bad chunk among many healthy
    /// ones) is still cleaned up — the guard only trips at store scale.
    #[tokio::test]
    async fn sweep_drops_minority_orphans_at_scale() {
        let (t, m) = build().await;
        let c = corpus("mark", "x");
        t.create_corpus(&c).unwrap();
        let chunks = write_n_chunks(&t, &m, &c, 5).await;
        m.forget(&MemoryId(chunks[0].memory_id.clone()))
            .await
            .unwrap();

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.chunks_examined, 5);
        assert_eq!(r.chunks_dropped, 1);
        assert_eq!(t.list_chunks_for_corpus(c.id).unwrap().len(), 4);
    }

    /// The reverse desync: the SQL chunk table reads empty while Weaviate
    /// still holds the training memories. The forget pass must not wipe
    /// every memory on that signal.
    #[tokio::test]
    async fn sweep_aborts_forget_when_all_memories_unbacked() {
        let (t, m) = build().await;
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                m.remember(training_memory("mark", &format!("stray{i}")))
                    .await
                    .unwrap(),
            );
        }

        let r = sweep_orphans(Arc::clone(&t), Arc::clone(&m), opts())
            .await
            .unwrap();
        assert_eq!(r.memories_examined, 5);
        assert_eq!(r.memories_forgotten, 0, "bulk unbacked must not forget");
        for id in ids {
            assert!(!is_forgotten(&*m, &id).await);
        }
    }
}

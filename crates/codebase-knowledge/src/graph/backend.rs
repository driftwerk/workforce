//! Backend-agnostic storage trait for the code graph.
//!
//! Phase 3 of the multi-store migration roadmap (see
//! `docs/POSTGRES.md`). The concrete storage implementation lives
//! behind this trait so the [`crate::graph::CodeGraph`] wrapper can
//! transparently use either the embedded SQLite backend or the
//! opt-in Postgres backend. All methods are async because
//! `tokio_postgres` is async; the SQLite backend wraps its blocking
//! calls in `tokio::task::spawn_blocking` internally.

use async_trait::async_trait;
use eyre::Result;
use std::collections::HashMap;

use super::entities::{CodeEntity, EntityId};
use super::relationships::Relationship;
use super::vectors::{PendingFile, StoredSummary, VectorFill};
use super::{GraphStats, IndexState, RepoLineage, RepositoryRow};
use crate::lease::{AcquireOutcome, IndexLease, IndexPhase};

/// Direction to follow when looking up entity neighbours in the
/// relationship graph.
///
/// Workforce stores each relationship exactly once (`source → target`),
/// but queries sometimes want "who calls me" (incoming) and sometimes
/// "who do I call" (outgoing). The backend's [`CodeGraphBackend::neighbours`]
/// primitive takes this enum so a single method handles both patterns
/// without encoding SQL direction into the trait surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    /// Match `source_id = current_id`; return the target entities.
    /// "What does this entity point at?" — e.g. `calls` / `depends_on`
    /// from the entity's point of view.
    Outgoing,
    /// Match `target_id = current_id`; return the source entities.
    /// "What points at this entity?" — e.g. `implements` / `imports`
    /// from the target's point of view.
    Incoming,
}

/// Backend-agnostic storage trait for the code graph.
///
/// Every method mirrors one of the pre-existing `CodeGraph::*`
/// methods. The `CodeGraph` wrapper holds an `Arc<dyn CodeGraphBackend>`
/// and delegates to it directly.
///
/// ## Trait size
///
/// This is a large trait (~35 methods) because the code graph has a
/// correspondingly wide API. Splitting it into sub-traits would add
/// ceremony without improving anything — consumers nearly always want
/// read + write + queries together, and both backends implement every
/// method.
#[async_trait]
pub trait CodeGraphBackend: Send + Sync {
    // ─── Repo ──────────────────────────────────────────────────────

    /// Register a repository for tracking (no-op if already present).
    async fn register_repo(&self, id: &str, name: &str, root_path: &str) -> Result<()>;

    // ─── Entities ──────────────────────────────────────────────────

    /// Insert or update a batch of entities. Parent-child foreign
    /// keys are validated after the whole batch so children can come
    /// before their parents.
    async fn upsert_entities(&self, entities: &[CodeEntity]) -> Result<usize>;

    async fn get_entity(&self, id: &EntityId) -> Result<Option<CodeEntity>>;

    async fn find_entities_by_name(
        &self,
        repo_id: &str,
        name: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>>;

    async fn find_entities_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeEntity>>;

    async fn find_entities_by_kind(
        &self,
        repo_id: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>>;

    async fn search_entities(
        &self,
        repo_id: &str,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>>;

    async fn all_entities(&self, repo_id: &str) -> Result<Vec<CodeEntity>>;

    async fn get_entity_indexed_at(&self, entity_id: &EntityId) -> Result<Option<String>>;

    // ─── Relationships ─────────────────────────────────────────────

    /// Insert or update a batch of relationships. Entity FKs are
    /// validated after the whole batch.
    async fn upsert_relationships(&self, relationships: &[Relationship]) -> Result<usize>;

    async fn count_relationships_by_kind(&self, repo_id: &str, kind: &str) -> Result<usize>;

    /// Count distinct entities that reference `entity_id` via any of
    /// the "impact" edges: `called_by`, `imports`, `depends_on`,
    /// `implements`.
    async fn count_dependents(&self, entity_id: &EntityId) -> Result<usize>;

    /// Look up neighbour entities of `current_id` along edges of a
    /// specific `kind`. The `direction` argument controls whether
    /// `current_id` is matched on the `source` side (returning
    /// targets) or on the `target` side (returning sources). Used
    /// as the BFS primitive for call graphs, impact analysis, etc.
    async fn neighbours(
        &self,
        current_id: &EntityId,
        kind: &str,
        direction: EdgeDirection,
    ) -> Result<Vec<CodeEntity>>;

    // ─── Summaries ─────────────────────────────────────────────────

    async fn upsert_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
        text: &str,
        source_hash: &str,
        model_used: &str,
    ) -> Result<()>;

    async fn get_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
    ) -> Result<Option<(String, bool)>>;

    async fn get_stale_summaries(&self, repo_id: &str) -> Result<Vec<(EntityId, String)>>;

    /// All stored summaries for a repo as `(entity_id, granularity) ->
    /// (source_hash, is_stale)`. Lets the resumable backfill skip summaries
    /// whose input is unchanged in a single round-trip instead of N lookups.
    async fn summary_hashes(
        &self,
        repo_id: &str,
    ) -> Result<HashMap<(EntityId, String), (String, bool)>>;

    async fn count_stale_summaries(&self, repo_id: &str) -> Result<usize>;

    async fn mark_summaries_stale(&self, entity_ids: &[EntityId]) -> Result<usize>;

    // ─── File index ────────────────────────────────────────────────

    async fn update_file_index(
        &self,
        repo_id: &str,
        file_path: &str,
        content_hash: &str,
        language: &str,
        entity_count: usize,
    ) -> Result<()>;

    async fn get_file_hash(&self, repo_id: &str, file_path: &str) -> Result<Option<String>>;

    async fn get_file_indexed_at(&self, repo_id: &str, file_path: &str) -> Result<Option<String>>;

    async fn delete_file_from_index(&self, repo_id: &str, file_path: &str) -> Result<()>;

    async fn get_file_paths(&self, repo_id: &str) -> Result<Vec<String>>;

    // ─── Index state ───────────────────────────────────────────────

    async fn update_index_state_full(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()>;

    async fn update_index_state_incremental(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()>;

    async fn get_index_state(&self, repo_id: &str) -> Result<Option<IndexState>>;

    async fn set_indexing_in_progress(&self, repo_id: &str, in_progress: bool) -> Result<()>;

    /// Which code graph this is: an id made once, with the graph. What the
    /// graph keeps in a vector database is kept as this graph's.
    fn identity(&self) -> &str;

    // ─── Vectors: filling the vector store from the graph ──────────

    /// Up to `limit` of a repo's entities in id order, after the entity
    /// `after` (from the first when empty) and no further than `upto`.
    async fn entities_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeEntity>>;

    /// Up to `limit` of a repo's summaries in order of entity and
    /// granularity, after the cursor `after` (see
    /// [`StoredSummary::cursor`]; from the first when empty) and no
    /// further than `upto`.
    async fn summaries_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSummary>>;

    /// The summaries of the entities in one file.
    async fn summaries_in_file(&self, repo_id: &str, file_path: &str)
        -> Result<Vec<StoredSummary>>;

    // Fills and pending files are kept per vector database, `backend`
    // (see [`super::VectorKeys`]): what is done for one leaves another's
    // as it was.

    /// How far the fill of a repo's vectors in `backend` has got, if one
    /// ever began.
    async fn vector_fill(&self, backend: &str, repo_id: &str) -> Result<Option<VectorFill>>;

    /// Record how far a repo's fill has got, if it stands where `from`
    /// says: at `from`'s generation, stage and cursor, or, for `None`,
    /// nowhere or finished. `false`, recording nothing, if another worker
    /// has moved it meanwhile.
    async fn advance_vector_fill(
        &self,
        backend: &str,
        repo_id: &str,
        from: Option<&VectorFill>,
        to: &VectorFill,
    ) -> Result<bool>;

    /// Forget a repo's fill, so that the next starts afresh.
    async fn clear_vector_fill(&self, backend: &str, repo_id: &str) -> Result<()>;

    /// Note that the vectors of `files` may be behind the graph in each of
    /// `backends`.
    async fn note_vectors_pending(
        &self,
        backends: &[&str],
        repo_id: &str,
        files: &[String],
    ) -> Result<()>;

    /// Up to `limit` of a repo's files noted as pending, with each note's
    /// sequence, in path order.
    async fn vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingFile>>;

    /// How many of a repo's files are noted as pending.
    async fn count_vectors_pending(&self, backend: &str, repo_id: &str) -> Result<usize>;

    /// Take a file off the pending list, if its note is still the one
    /// with `seq`: one noted again since stays.
    async fn clear_vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        file_path: &str,
        seq: i64,
    ) -> Result<()>;

    /// Which making of the vector store's collection (its identity, see
    /// [`crate::vector::CodeVectorStore::identity`]) holds a vector for
    /// every entity of the repo, as the graph has them. None for a repo
    /// never fully indexed, and from each
    /// [`Self::update_index_state_full`] until it is set.
    async fn vectors_complete_in(&self, repo_id: &str) -> Result<Option<String>>;

    /// Record which making of the vector store's collection holds every
    /// entity of the repo, or that none is known to. The graph is where
    /// this lives because it is reachable whenever the graph was just
    /// written, as when a vector write failed; it names the collection
    /// because the collection may be lost while the graph is kept. A repo
    /// never fully indexed is left as it is.
    async fn set_vectors_complete_in(&self, repo_id: &str, identity: Option<&str>) -> Result<()>;

    // ─── Lease (active-run tracking) ───────────────────────────────
    //
    // The lease is the source of truth for "is something working on
    // this repo right now". It's kept in SQLite so it survives process
    // restarts; a worker that dies leaves a stale heartbeat behind, and
    // the next acquirer steals it.

    /// Try to acquire the lease for `repo_id`. Steals stale leases
    /// (heartbeat older than [`crate::lease::STALE_THRESHOLD`]),
    /// otherwise returns [`AcquireOutcome::Held`] when a fresh lease
    /// already exists.
    async fn acquire_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        target_commit: Option<&str>,
        phase: IndexPhase,
    ) -> Result<AcquireOutcome>;

    /// Refresh `last_heartbeat_at_ms` and progress counters on the lease
    /// owned by `worker_id`. A no-op if the lease was stolen since
    /// acquisition (worker_id mismatch).
    async fn heartbeat_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        phase: IndexPhase,
        entities_total: Option<u64>,
        entities_processed: Option<u64>,
        summaries_total: Option<u64>,
        summaries_processed: Option<u64>,
    ) -> Result<()>;

    /// Release the lease, but only if `worker_id` still owns it.
    async fn release_lease(&self, repo_id: &str, worker_id: &str) -> Result<()>;

    /// Read the current lease, if any. Callers should consult
    /// `IndexLease::is_fresh` before treating it as live.
    async fn get_lease(&self, repo_id: &str) -> Result<Option<IndexLease>>;

    // ─── Completeness primitives ───────────────────────────────────

    /// Count entities that should have a summary (Function, Method,
    /// File, Module) for one repo.
    async fn count_summarisable_entities(&self, repo_id: &str) -> Result<u64>;

    /// Count summaries present for one repo (regardless of staleness).
    async fn count_summaries_present(&self, repo_id: &str) -> Result<u64>;

    // ─── Bulk ops ──────────────────────────────────────────────────

    /// Delete every row for `repo_id` across entities, relationships,
    /// summaries, file_index, and index_state. Returns the number of
    /// entities deleted. Keeps the `repositories` row so a follow-up
    /// `register_repo` can reuse it for a clean re-index.
    /// The files noted pending for the backends in `keep_pending` stay
    /// noted: a standby is told of what a full index changes by what it
    /// notes, and must not lose what was noted before.
    async fn delete_repo_data(&self, repo_id: &str, keep_pending: &[&str]) -> Result<usize>;

    /// Like [`Self::delete_repo_data`] but also drops the `repositories`
    /// row itself. Use when fully removing a repo (e.g. cascade from
    /// `projects.delete purge` or operator-driven `knowledge.delete`),
    /// not for a re-index churn.
    async fn drop_repo(&self, repo_id: &str) -> Result<usize>;

    /// Return all known `repo_id`s. Used by `knowledge.delete` prune to
    /// find orphans.
    async fn list_repos(&self) -> Result<Vec<String>>;

    /// Return every row of `repositories` with its lineage classification
    /// and index timestamps. Ordered by `id` for deterministic output.
    /// Callers wanting just ids should keep using `list_repos`.
    async fn list_repositories(&self) -> Result<Vec<RepositoryRow>>;

    /// Return every row whose lineage matches `lineage`. Same ordering
    /// guarantee as `list_repositories`.
    async fn list_repositories_by_lineage(
        &self,
        lineage: RepoLineage,
    ) -> Result<Vec<RepositoryRow>>;

    /// Return every active `index_lease` row. Ordering: `acquired_at_ms`
    /// ascending so the oldest lease is first. Heartbeat-based
    /// staleness is *not* filtered server-side — callers decide via
    /// `IndexLease::is_fresh` whether to include or warn on zombies.
    async fn list_active_leases(&self) -> Result<Vec<IndexLease>>;

    /// Update a single repository's `lineage` column. No-op when the
    /// `repo_id` does not exist.
    async fn set_repo_lineage(&self, repo_id: &str, lineage: RepoLineage) -> Result<()>;

    /// Apply many `(repo_id, lineage)` updates atomically. Returns the
    /// number of rows actually touched (existing repo_ids only).
    async fn set_repo_lineage_batch(&self, updates: &[(String, RepoLineage)]) -> Result<usize>;

    /// Delete entities + their relationships + their summaries for a
    /// single file. Returns the number of entities deleted.
    async fn delete_file_entities(&self, repo_id: &str, file_path: &str) -> Result<usize>;

    /// Update the file_path for every entity + file_index row that
    /// currently points at `old_path` for `repo_id`. Returns the
    /// number of entities updated.
    async fn rename_file(&self, repo_id: &str, old_path: &str, new_path: &str) -> Result<usize>;

    async fn get_stats(&self, repo_id: &str) -> Result<GraphStats>;

    // ─── Maintenance ───────────────────────────────────────────────

    /// Flush any pending writes so data survives an abrupt shutdown.
    /// SQLite runs `PRAGMA wal_checkpoint(TRUNCATE)`; Postgres is a
    /// no-op because it always fsyncs at commit time.
    async fn checkpoint(&self) -> Result<()>;
}

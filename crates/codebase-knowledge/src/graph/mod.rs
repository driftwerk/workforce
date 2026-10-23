//! Code graph storage.
//!
//! The graph is stored through a backend-agnostic trait —
//! [`CodeGraphBackend`] — with a concrete SQLite implementation that
//! has shipped since day one and an optional Postgres implementation
//! gated on the `postgres` cargo feature (Phase 3 of the multi-store
//! migration roadmap; see `docs/POSTGRES.md`).
//!
//! [`CodeGraph`] is the public wrapper used by every caller. It holds
//! an `Arc<dyn CodeGraphBackend>` and delegates every operation; the
//! graph-traversal query methods (BFS for callers/callees/impact) live
//! in [`queries`] and are implemented in terms of the backend's
//! [`CodeGraphBackend::neighbours`] primitive.

pub mod backend;
pub mod entities;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod queries;
pub mod relationships;
pub mod schema;
pub mod sqlite;
pub mod vectors;

use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

pub use self::backend::{CodeGraphBackend, EdgeDirection};
#[cfg(feature = "postgres")]
pub use self::postgres::PostgresCodeGraphBackend;
pub use self::queries::{ContextEntity, GraphQueryResult, ReachedVia};
pub use self::sqlite::SqliteCodeGraphBackend;

use self::entities::{CodeEntity, EntityId};
use self::relationships::Relationship;

/// Classification of a `repositories` row by the on-disk layout that
/// produced it. Computed at the hub layer (the only place that knows
/// the canonical post-DW-506 paths) and persisted in the graph DB so
/// callers like `knowledge.delete prune` can filter without re-running
/// the classifier every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLineage {
    /// Path matches the current canonical layout
    /// (`{data_dir}/knowledge/checkouts/repos/{owner}/{repo}`).
    Canonical,
    /// Path matches a documented pre-DW-506 layout pattern
    /// (`agents/...`, `knowledge/checkouts/projects/...`,
    /// `knowledge/checkouts/standalone/...`).
    MigrationOrphan,
    /// Path matches no known pattern. Test fixtures, ad-hoc indexes,
    /// or rows written under an older `data_dir` that doesn't match
    /// the current process's `data_dir` all fall here — conservative
    /// default; never auto-pruned.
    Unknown,
}

impl RepoLineage {
    pub const ALL: [RepoLineage; 3] = [
        RepoLineage::Canonical,
        RepoLineage::MigrationOrphan,
        RepoLineage::Unknown,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            RepoLineage::Canonical => "canonical",
            RepoLineage::MigrationOrphan => "migration_orphan",
            RepoLineage::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for RepoLineage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RepoLineage {
    type Err = eyre::Report;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "canonical" => Ok(RepoLineage::Canonical),
            "migration_orphan" => Ok(RepoLineage::MigrationOrphan),
            "unknown" => Ok(RepoLineage::Unknown),
            other => Err(eyre!(
                "unknown lineage value '{other}'; expected canonical | migration_orphan | unknown"
            )),
        }
    }
}

/// Full row of the `repositories` table. Returned by
/// [`CodeGraph::list_repositories`] when callers need lineage / index
/// timestamps alongside the id (the older `list_repos` only returned ids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryRow {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub last_indexed_commit: Option<String>,
    pub last_indexed_at: Option<String>,
    pub created_at: String,
    pub lineage: RepoLineage,
}

/// Public facade over the code graph. Thin wrapper around
/// `Arc<dyn CodeGraphBackend>` so every call site — including all
/// pre-existing ones — can stay unchanged regardless of which
/// backend is actually holding the data.
pub struct CodeGraph {
    pub(crate) backend: Arc<dyn CodeGraphBackend>,
    vectors: VectorKeys,
}

/// The vector database code vectors are kept in, as `[knowledge]
/// vector_backend` names it, and the one kept in step with it as a
/// standby, if any. Fills and pending files are kept per database under
/// these names: what is done for one leaves another's as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorKeys {
    active: String,
    standby: Option<String>,
}

impl VectorKeys {
    /// vellev's name.
    pub const VELLEV: &'static str = "vellev";
    /// Weaviate's name.
    pub const WEAVIATE: &'static str = "weaviate";

    /// Vectors kept in `active`, with `standby` kept in step if given:
    /// each a database's name, and not the same one.
    pub fn new(active: &str, standby: Option<&str>) -> Result<Self> {
        let known = |name: &str| -> Result<String> {
            match name {
                Self::VELLEV | Self::WEAVIATE => Ok(name.to_string()),
                other => Err(eyre::eyre!(
                    "'{other}' is no vector database: say \"{}\" or \"{}\"",
                    Self::WEAVIATE,
                    Self::VELLEV
                )),
            }
        };
        let active = known(active)?;
        let standby = standby.map(known).transpose()?;
        if standby.as_ref() == Some(&active) {
            eyre::bail!("the standby vector database is the one in use ('{active}')");
        }
        Ok(Self { active, standby })
    }

    /// Where code vectors are kept, and filled.
    pub fn active(&self) -> &str {
        &self.active
    }

    /// The one kept in step with it, never filled nor read: told of every
    /// file changed, so that it can be brought up to date when it is
    /// switched to.
    pub fn standby(&self) -> Option<&str> {
        self.standby.as_deref()
    }
}

/// Before a graph is told otherwise its vector state is vellev's: only a
/// store that keeps track of its vectors' completeness fills them or
/// notes files pending, and vellev's is the only one.
impl Default for VectorKeys {
    fn default() -> Self {
        Self {
            active: Self::VELLEV.to_string(),
            standby: None,
        }
    }
}

impl CodeGraph {
    /// Build a `CodeGraph` from an existing backend. Used by
    /// integration points like `HubState` that want to pick the
    /// backend themselves (e.g. via `[codegraph] backend = "postgres"`).
    pub fn from_backend(backend: Arc<dyn CodeGraphBackend>) -> Self {
        Self {
            backend,
            vectors: VectorKeys::default(),
        }
    }

    /// The graph, keeping its vector state under `keys`.
    pub fn keyed(mut self, keys: VectorKeys) -> Self {
        self.vectors = keys;
        self
    }

    /// Which vector databases the graph keeps vector state for.
    pub fn vector_keys(&self) -> &VectorKeys {
        &self.vectors
    }

    /// Open (or create) a SQLite code graph database at the given
    /// path. Historical alias kept for backwards compatibility —
    /// `KnowledgeService::new` and friends still call this.
    pub fn open(path: &Path) -> Result<Self> {
        let backend = Arc::new(SqliteCodeGraphBackend::open(path)?);
        Ok(Self::from_backend(backend))
    }

    /// Create an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let backend = Arc::new(SqliteCodeGraphBackend::open_in_memory()?);
        Ok(Self::from_backend(backend))
    }

    pub fn backend(&self) -> &Arc<dyn CodeGraphBackend> {
        &self.backend
    }

    // ─── Delegated API ─────────────────────────────────────────────
    //
    // Every one of these used to live in this file directly, poking
    // at `self.conn.lock().await`. The bodies now forward straight
    // to the trait object; semantics are unchanged, which means the
    // existing test suite continues to cover the SQLite backend
    // end-to-end.

    pub async fn checkpoint(&self) -> Result<()> {
        self.backend.checkpoint().await
    }

    pub async fn register_repo(&self, id: &str, name: &str, root_path: &str) -> Result<()> {
        self.backend.register_repo(id, name, root_path).await
    }

    /// Insert or update a single entity. Convenience wrapper around
    /// `upsert_entities(&[entity])` so the old single-entity API
    /// keeps working.
    pub async fn upsert_entity(&self, entity: &CodeEntity) -> Result<()> {
        self.backend
            .upsert_entities(std::slice::from_ref(entity))
            .await?;
        Ok(())
    }

    pub async fn upsert_entities(&self, entities: &[CodeEntity]) -> Result<usize> {
        self.backend.upsert_entities(entities).await
    }

    pub async fn upsert_relationships(&self, relationships: &[Relationship]) -> Result<usize> {
        self.backend.upsert_relationships(relationships).await
    }

    pub async fn get_entity(&self, id: &EntityId) -> Result<Option<CodeEntity>> {
        self.backend.get_entity(id).await
    }

    pub async fn find_entities_by_name(
        &self,
        repo_id: &str,
        name: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        self.backend
            .find_entities_by_name(repo_id, name, limit)
            .await
    }

    pub async fn find_entities_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeEntity>> {
        self.backend.find_entities_in_file(repo_id, file_path).await
    }

    /// The standby's pending files stay noted: see
    /// [`backend::CodeGraphBackend::delete_repo_data`].
    pub async fn delete_repo_data(&self, repo_id: &str) -> Result<usize> {
        let kept: Vec<&str> = self.vectors.standby().into_iter().collect();
        self.backend.delete_repo_data(repo_id, &kept).await
    }

    pub async fn drop_repo(&self, repo_id: &str) -> Result<usize> {
        self.backend.drop_repo(repo_id).await
    }

    pub async fn list_repos(&self) -> Result<Vec<String>> {
        self.backend.list_repos().await
    }

    pub async fn list_repositories(&self) -> Result<Vec<RepositoryRow>> {
        self.backend.list_repositories().await
    }

    pub async fn list_repositories_by_lineage(
        &self,
        lineage: RepoLineage,
    ) -> Result<Vec<RepositoryRow>> {
        self.backend.list_repositories_by_lineage(lineage).await
    }

    pub async fn set_repo_lineage(&self, repo_id: &str, lineage: RepoLineage) -> Result<()> {
        self.backend.set_repo_lineage(repo_id, lineage).await
    }

    pub async fn set_repo_lineage_batch(&self, updates: &[(String, RepoLineage)]) -> Result<usize> {
        self.backend.set_repo_lineage_batch(updates).await
    }

    pub async fn list_active_leases(&self) -> Result<Vec<crate::lease::IndexLease>> {
        self.backend.list_active_leases().await
    }

    pub async fn delete_file_entities(&self, repo_id: &str, file_path: &str) -> Result<usize> {
        self.backend.delete_file_entities(repo_id, file_path).await
    }

    pub async fn update_file_index(
        &self,
        repo_id: &str,
        file_path: &str,
        content_hash: &str,
        language: &str,
        entity_count: usize,
    ) -> Result<()> {
        self.backend
            .update_file_index(repo_id, file_path, content_hash, language, entity_count)
            .await
    }

    pub async fn get_file_hash(&self, repo_id: &str, file_path: &str) -> Result<Option<String>> {
        self.backend.get_file_hash(repo_id, file_path).await
    }

    pub async fn update_index_state_full(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        self.backend
            .update_index_state_full(repo_id, commit, total_entities, total_relationships)
            .await
    }

    pub async fn update_index_state_incremental(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        self.backend
            .update_index_state_incremental(repo_id, commit, total_entities, total_relationships)
            .await
    }

    pub async fn delete_file_from_index(&self, repo_id: &str, file_path: &str) -> Result<()> {
        self.backend
            .delete_file_from_index(repo_id, file_path)
            .await
    }

    pub async fn get_index_state(&self, repo_id: &str) -> Result<Option<IndexState>> {
        self.backend.get_index_state(repo_id).await
    }

    /// See [`backend::CodeGraphBackend::identity`].
    pub fn identity(&self) -> &str {
        self.backend.identity()
    }

    /// See [`backend::CodeGraphBackend::entities_page`].
    pub async fn entities_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        self.backend
            .entities_page(repo_id, after, upto, limit)
            .await
    }

    /// See [`backend::CodeGraphBackend::summaries_page`].
    pub async fn summaries_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<vectors::StoredSummary>> {
        self.backend
            .summaries_page(repo_id, after, upto, limit)
            .await
    }

    /// See [`backend::CodeGraphBackend::summaries_in_file`].
    pub async fn summaries_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<vectors::StoredSummary>> {
        self.backend.summaries_in_file(repo_id, file_path).await
    }

    // Fills and pending files are those of the vector database in use
    // ([`VectorKeys::active`]), unless said otherwise.

    /// See [`backend::CodeGraphBackend::vector_fill`].
    pub async fn vector_fill(&self, repo_id: &str) -> Result<Option<vectors::VectorFill>> {
        self.backend
            .vector_fill(self.vectors.active(), repo_id)
            .await
    }

    /// See [`backend::CodeGraphBackend::advance_vector_fill`].
    pub async fn advance_vector_fill(
        &self,
        repo_id: &str,
        from: Option<&vectors::VectorFill>,
        to: &vectors::VectorFill,
    ) -> Result<bool> {
        self.backend
            .advance_vector_fill(self.vectors.active(), repo_id, from, to)
            .await
    }

    /// See [`backend::CodeGraphBackend::clear_vector_fill`].
    pub async fn clear_vector_fill(&self, repo_id: &str) -> Result<()> {
        self.backend
            .clear_vector_fill(self.vectors.active(), repo_id)
            .await
    }

    /// See [`backend::CodeGraphBackend::note_vectors_pending`].
    pub async fn note_vectors_pending(&self, repo_id: &str, files: &[String]) -> Result<()> {
        self.note_vectors_pending_in(&[self.vectors.active()], repo_id, files)
            .await
    }

    /// Note `files` pending in each of `backends`.
    pub async fn note_vectors_pending_in(
        &self,
        backends: &[&str],
        repo_id: &str,
        files: &[String],
    ) -> Result<()> {
        if backends.is_empty() || files.is_empty() {
            return Ok(());
        }
        self.backend
            .note_vectors_pending(backends, repo_id, files)
            .await
    }

    /// See [`backend::CodeGraphBackend::vectors_pending`].
    pub async fn vectors_pending(
        &self,
        repo_id: &str,
        limit: usize,
    ) -> Result<Vec<vectors::PendingFile>> {
        self.backend
            .vectors_pending(self.vectors.active(), repo_id, limit)
            .await
    }

    /// See [`backend::CodeGraphBackend::count_vectors_pending`].
    pub async fn count_vectors_pending(&self, repo_id: &str) -> Result<usize> {
        self.count_vectors_pending_in(self.vectors.active(), repo_id)
            .await
    }

    /// How many of a repo's files are noted pending in `backend`.
    pub async fn count_vectors_pending_in(&self, backend: &str, repo_id: &str) -> Result<usize> {
        self.backend.count_vectors_pending(backend, repo_id).await
    }

    /// See [`backend::CodeGraphBackend::clear_vectors_pending`].
    pub async fn clear_vectors_pending(
        &self,
        repo_id: &str,
        file_path: &str,
        seq: i64,
    ) -> Result<()> {
        self.backend
            .clear_vectors_pending(self.vectors.active(), repo_id, file_path, seq)
            .await
    }

    /// See [`backend::CodeGraphBackend::vectors_complete_in`].
    pub async fn vectors_complete_in(&self, repo_id: &str) -> Result<Option<String>> {
        self.backend.vectors_complete_in(repo_id).await
    }

    /// See [`backend::CodeGraphBackend::set_vectors_complete_in`].
    pub async fn set_vectors_complete_in(
        &self,
        repo_id: &str,
        identity: Option<&str>,
    ) -> Result<()> {
        self.backend
            .set_vectors_complete_in(repo_id, identity)
            .await
    }

    pub async fn get_entity_indexed_at(&self, entity_id: &EntityId) -> Result<Option<String>> {
        self.backend.get_entity_indexed_at(entity_id).await
    }

    pub async fn get_file_indexed_at(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Option<String>> {
        self.backend.get_file_indexed_at(repo_id, file_path).await
    }

    pub async fn count_dependents(&self, entity_id: &EntityId) -> Result<usize> {
        self.backend.count_dependents(entity_id).await
    }

    pub async fn mark_summaries_stale(&self, entity_ids: &[EntityId]) -> Result<usize> {
        self.backend.mark_summaries_stale(entity_ids).await
    }

    pub async fn upsert_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
        text: &str,
        source_hash: &str,
        model_used: &str,
    ) -> Result<()> {
        self.backend
            .upsert_summary(entity_id, granularity, text, source_hash, model_used)
            .await
    }

    pub async fn get_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
    ) -> Result<Option<(String, bool)>> {
        self.backend.get_summary(entity_id, granularity).await
    }

    pub async fn get_stale_summaries(&self, repo_id: &str) -> Result<Vec<(EntityId, String)>> {
        self.backend.get_stale_summaries(repo_id).await
    }

    pub async fn summary_hashes(
        &self,
        repo_id: &str,
    ) -> Result<std::collections::HashMap<(EntityId, String), (String, bool)>> {
        self.backend.summary_hashes(repo_id).await
    }

    pub async fn count_stale_summaries(&self, repo_id: &str) -> Result<usize> {
        self.backend.count_stale_summaries(repo_id).await
    }

    pub async fn search_entities(
        &self,
        repo_id: &str,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        self.backend.search_entities(repo_id, pattern, limit).await
    }

    pub async fn find_entities_by_kind(
        &self,
        repo_id: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        self.backend
            .find_entities_by_kind(repo_id, kind, limit)
            .await
    }

    pub async fn all_entities(&self, repo_id: &str) -> Result<Vec<CodeEntity>> {
        self.backend.all_entities(repo_id).await
    }

    pub async fn get_file_paths(&self, repo_id: &str) -> Result<Vec<String>> {
        self.backend.get_file_paths(repo_id).await
    }

    pub async fn rename_file(
        &self,
        repo_id: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<usize> {
        self.backend.rename_file(repo_id, old_path, new_path).await
    }

    pub async fn set_indexing_in_progress(&self, repo_id: &str, in_progress: bool) -> Result<()> {
        self.backend
            .set_indexing_in_progress(repo_id, in_progress)
            .await
    }

    pub async fn get_stats(&self, repo_id: &str) -> Result<GraphStats> {
        self.backend.get_stats(repo_id).await
    }

    pub async fn count_relationships_by_kind(&self, repo_id: &str, kind: &str) -> Result<usize> {
        self.backend
            .count_relationships_by_kind(repo_id, kind)
            .await
    }

    // ─── Lease delegation ──────────────────────────────────────────

    pub async fn acquire_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        target_commit: Option<&str>,
        phase: crate::lease::IndexPhase,
    ) -> Result<crate::lease::AcquireOutcome> {
        self.backend
            .acquire_lease(repo_id, worker_id, target_commit, phase)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn heartbeat_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        phase: crate::lease::IndexPhase,
        entities_total: Option<u64>,
        entities_processed: Option<u64>,
        summaries_total: Option<u64>,
        summaries_processed: Option<u64>,
    ) -> Result<()> {
        self.backend
            .heartbeat_lease(
                repo_id,
                worker_id,
                phase,
                entities_total,
                entities_processed,
                summaries_total,
                summaries_processed,
            )
            .await
    }

    pub async fn release_lease(&self, repo_id: &str, worker_id: &str) -> Result<()> {
        self.backend.release_lease(repo_id, worker_id).await
    }

    pub async fn get_lease(&self, repo_id: &str) -> Result<Option<crate::lease::IndexLease>> {
        self.backend.get_lease(repo_id).await
    }

    pub async fn count_summarisable_entities(&self, repo_id: &str) -> Result<u64> {
        self.backend.count_summarisable_entities(repo_id).await
    }

    pub async fn count_summaries_present(&self, repo_id: &str) -> Result<u64> {
        self.backend.count_summaries_present(repo_id).await
    }
}

/// Index state for a repository.
#[derive(Debug, Clone)]
pub struct IndexState {
    pub repo_id: String,
    pub last_commit: Option<String>,
    pub last_full_index_at: Option<String>,
    pub last_incremental_at: Option<String>,
    pub total_entities: usize,
    pub total_relationships: usize,
    pub indexing_in_progress: bool,
}

/// Basic statistics about the code graph.
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub entity_count: usize,
    pub relationship_count: usize,
    pub file_count: usize,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What both backends keep of where a repo's vectors are complete:
    /// nothing for a repo with no index state; nothing from each full index
    /// until set; kept through an incremental index. `repo` is registered.
    pub(crate) async fn vectors_complete_in_is_kept_per_repo(graph: &CodeGraph, repo: &str) {
        let at = |repo: &str| {
            let repo = repo.to_string();
            async move { graph.vectors_complete_in(&repo).await.unwrap() }
        };
        graph
            .register_repo("unindexed", "Unindexed", "/tmp/unindexed")
            .await
            .unwrap();
        graph
            .set_vectors_complete_in("unindexed", Some("a"))
            .await
            .unwrap();
        assert_eq!(at("unindexed").await, None);
        assert_eq!(at("no-such-repo").await, None);

        graph
            .update_index_state_full(repo, "c1", 1, 1)
            .await
            .unwrap();
        assert_eq!(at(repo).await, None);
        graph
            .set_vectors_complete_in(repo, Some("a"))
            .await
            .unwrap();
        assert_eq!(at(repo).await.as_deref(), Some("a"));
        assert_eq!(at("unindexed").await, None, "per repo");

        graph
            .update_index_state_incremental(repo, "c2", 2, 2)
            .await
            .unwrap();
        graph.set_indexing_in_progress(repo, true).await.unwrap();
        graph.set_indexing_in_progress(repo, false).await.unwrap();
        assert_eq!(
            at(repo).await.as_deref(),
            Some("a"),
            "kept through an incremental index"
        );
        graph
            .set_vectors_complete_in(repo, Some("b"))
            .await
            .unwrap();
        assert_eq!(at(repo).await.as_deref(), Some("b"));

        graph
            .update_index_state_full(repo, "c3", 3, 3)
            .await
            .unwrap();
        assert_eq!(at(repo).await, None, "unknown again after a full index");
        graph
            .set_vectors_complete_in(repo, Some("b"))
            .await
            .unwrap();
        graph.set_vectors_complete_in(repo, None).await.unwrap();
        assert_eq!(at(repo).await, None);
    }

    /// What both backends keep for filling a repo's vectors: its entities
    /// and summaries a page at a time in a fixed order, how far a fill has
    /// got, moved only from where it was found, and the files noted as
    /// pending. Takes a repo `fill-repo` of its own.
    pub(crate) async fn fills_are_kept_per_repo(graph: &CodeGraph) {
        use super::entities::{mk_entity, CodeVisibility, EntityKind, Language};
        use super::vectors::{FillStage, VectorFill};
        const REPO: &str = "fill-repo";
        graph
            .register_repo(REPO, "Fill", "/tmp/fill")
            .await
            .unwrap();
        graph
            .register_repo("other", "Other", "/tmp/other")
            .await
            .unwrap();
        let entity = |name: &str, file: &str, repo: &str| {
            let mut entity = mk_entity(
                name,
                EntityKind::Function,
                file,
                None,
                CodeVisibility::Public,
                None,
                Language::Rust,
                3,
            );
            entity.id = EntityId(format!("{repo}-{name}"));
            entity.repo_id = repo.to_string();
            entity
        };
        let mut entities: Vec<CodeEntity> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|name| {
                entity(
                    name,
                    if *name < "d" {
                        "src/one.rs"
                    } else {
                        "src/two.rs"
                    },
                    REPO,
                )
            })
            .collect();
        entities.push(entity("z", "src/one.rs", "other"));
        graph.upsert_entities(&entities).await.unwrap();
        let ids = |page: &[CodeEntity]| -> Vec<String> {
            page.iter().map(|e| e.id.as_str().to_string()).collect()
        };
        let id = |name: &str| format!("{REPO}-{name}");

        // Entities, in id order, of the one repo.
        let first = graph.entities_page(REPO, "", None, 2).await.unwrap();
        assert_eq!(ids(&first), [id("a"), id("b")]);
        let next = graph.entities_page(REPO, &id("b"), None, 2).await.unwrap();
        assert_eq!(ids(&next), [id("c"), id("d")]);
        let last = graph.entities_page(REPO, &id("d"), None, 2).await.unwrap();
        assert_eq!(ids(&last), [id("e")]);
        assert!(graph
            .entities_page(REPO, &id("e"), None, 2)
            .await
            .unwrap()
            .is_empty());
        let upto = graph
            .entities_page(REPO, &id("a"), Some(&id("c")), 100)
            .await
            .unwrap();
        assert_eq!(ids(&upto), [id("b"), id("c")]);

        // Summaries, by entity and granularity, with their entity's file.
        for (name, granularity) in [
            ("a", "function"),
            ("a", "file"),
            ("d", "function"),
            ("z", "function"),
        ] {
            let repo = if name == "z" { "other" } else { REPO };
            graph
                .upsert_summary(
                    &EntityId(format!("{repo}-{name}")),
                    granularity,
                    name,
                    "h",
                    "m",
                )
                .await
                .unwrap();
        }
        let keys = |page: &[super::vectors::StoredSummary]| -> Vec<(String, String, String)> {
            page.iter()
                .map(|s| {
                    (
                        s.entity_id.clone(),
                        s.granularity.clone(),
                        s.file_path.clone(),
                    )
                })
                .collect()
        };
        let one = "src/one.rs".to_string();
        let first = graph.summaries_page(REPO, "", None, 2).await.unwrap();
        assert_eq!(
            keys(&first),
            [
                (id("a"), "file".to_string(), one.clone()),
                (id("a"), "function".to_string(), one.clone())
            ]
        );
        assert!(!first[0].is_stale && first[0].text == "a");
        let next = graph
            .summaries_page(REPO, &first[1].cursor(), None, 2)
            .await
            .unwrap();
        assert_eq!(
            keys(&next),
            [(id("d"), "function".to_string(), "src/two.rs".to_string())]
        );
        let upto = graph
            .summaries_page(REPO, &first[0].cursor(), Some(&first[1].cursor()), 100)
            .await
            .unwrap();
        assert_eq!(
            keys(&upto),
            [(id("a"), "function".to_string(), one.clone())]
        );
        let in_file = graph.summaries_in_file(REPO, "src/one.rs").await.unwrap();
        assert_eq!(in_file.len(), 2);

        // A fill, moved only from where it was found.
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), None);
        let begun = VectorFill {
            generation: "g1".into(),
            collection: "c1".into(),
            stage: FillStage::Entities,
            cursor: String::new(),
            done: 0,
            total: 8,
        };
        assert!(graph.advance_vector_fill(REPO, None, &begun).await.unwrap());
        let rival = VectorFill {
            generation: "g2".into(),
            ..begun.clone()
        };
        assert!(
            !graph.advance_vector_fill(REPO, None, &rival).await.unwrap(),
            "one is under way"
        );
        let paged = VectorFill {
            cursor: id("b"),
            done: 2,
            ..begun.clone()
        };
        assert!(graph
            .advance_vector_fill(REPO, Some(&begun), &paged)
            .await
            .unwrap());
        assert!(
            !graph
                .advance_vector_fill(REPO, Some(&begun), &paged)
                .await
                .unwrap(),
            "moved on since it was read"
        );
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), Some(paged.clone()));
        assert_eq!(graph.vector_fill("other").await.unwrap(), None, "per repo");
        let done = VectorFill {
            stage: FillStage::Done,
            cursor: String::new(),
            done: 8,
            ..paged.clone()
        };
        assert!(graph
            .advance_vector_fill(REPO, Some(&paged), &done)
            .await
            .unwrap());
        assert!(
            graph.advance_vector_fill(REPO, None, &rival).await.unwrap(),
            "finished, another may begin"
        );
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), Some(rival));

        // A full index forgets the fill; so does forgetting it.
        graph
            .update_index_state_full(REPO, "c1", 5, 0)
            .await
            .unwrap();
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), None);
        assert!(graph.advance_vector_fill(REPO, None, &begun).await.unwrap());
        graph.clear_vector_fill(REPO).await.unwrap();
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), None);

        // Pending files: a note renewed is another note.
        let files = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        graph
            .note_vectors_pending(REPO, &files(&["src/two.rs", "src/one.rs"]))
            .await
            .unwrap();
        graph
            .note_vectors_pending("other", &files(&["src/one.rs"]))
            .await
            .unwrap();
        let pending = graph.vectors_pending(REPO, 10).await.unwrap();
        assert_eq!(
            pending,
            [("src/one.rs".to_string(), 1), ("src/two.rs".to_string(), 1)]
        );
        assert_eq!(graph.vectors_pending(REPO, 1).await.unwrap().len(), 1);
        graph
            .note_vectors_pending(REPO, &files(&["src/one.rs"]))
            .await
            .unwrap();
        graph
            .clear_vectors_pending(REPO, "src/one.rs", 1)
            .await
            .unwrap();
        graph
            .clear_vectors_pending(REPO, "src/two.rs", 1)
            .await
            .unwrap();
        assert_eq!(
            graph.vectors_pending(REPO, 10).await.unwrap(),
            [("src/one.rs".to_string(), 2)],
            "noted again since, it stays"
        );
        assert_eq!(graph.count_vectors_pending(REPO).await.unwrap(), 1);
        assert_eq!(graph.count_vectors_pending("other").await.unwrap(), 1);

        // Deleting the repo's data forgets both.
        assert!(graph.advance_vector_fill(REPO, None, &begun).await.unwrap());
        graph.delete_repo_data(REPO).await.unwrap();
        assert_eq!(graph.vector_fill(REPO).await.unwrap(), None);
        assert_eq!(graph.count_vectors_pending(REPO).await.unwrap(), 0);
        assert_eq!(graph.count_vectors_pending("other").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sqlite_keeps_fills_per_repo() {
        fills_are_kept_per_repo(&CodeGraph::open_in_memory().unwrap()).await;
    }

    /// Each vector database's fill and pending files are its own: noting,
    /// moving, taking off and forgetting what is one's leaves the other's
    /// as it was; and a full index's purge keeps the pending files of the
    /// databases it is told to.
    pub(crate) async fn vector_state_is_kept_per_backend(graph: &CodeGraph) {
        use super::vectors::{FillStage, VectorFill};
        const REPO: &str = "keyed-repo";
        let (vellev, weaviate) = (VectorKeys::VELLEV, VectorKeys::WEAVIATE);
        let store = graph.backend();
        graph
            .register_repo(REPO, "Keyed", "/tmp/keyed")
            .await
            .unwrap();
        let fill = |generation: &str| VectorFill {
            generation: generation.into(),
            collection: "making-1".into(),
            stage: FillStage::Entities,
            cursor: String::new(),
            done: 0,
            total: 3,
        };

        // A fill begun in each, neither in the other's way.
        assert!(store
            .advance_vector_fill(vellev, REPO, None, &fill("v1"))
            .await
            .unwrap());
        assert!(store
            .advance_vector_fill(weaviate, REPO, None, &fill("w1"))
            .await
            .unwrap());
        assert_eq!(
            store.vector_fill(vellev, REPO).await.unwrap(),
            Some(fill("v1"))
        );
        assert_eq!(
            store.vector_fill(weaviate, REPO).await.unwrap(),
            Some(fill("w1"))
        );
        // Moved on in one only where that one stands.
        let mut on = fill("v1");
        on.cursor = "e9".into();
        assert!(!store
            .advance_vector_fill(weaviate, REPO, Some(&fill("v1")), &on)
            .await
            .unwrap());
        assert!(store
            .advance_vector_fill(vellev, REPO, Some(&fill("v1")), &on)
            .await
            .unwrap());
        assert_eq!(
            store.vector_fill(weaviate, REPO).await.unwrap(),
            Some(fill("w1"))
        );
        store.clear_vector_fill(weaviate, REPO).await.unwrap();
        assert_eq!(store.vector_fill(weaviate, REPO).await.unwrap(), None);
        assert_eq!(store.vector_fill(vellev, REPO).await.unwrap(), Some(on));

        // Noted in both at once; taken off in one.
        let files = ["src/a.rs".to_string(), "src/b.rs".to_string()];
        store
            .note_vectors_pending(&[vellev, weaviate], REPO, &files)
            .await
            .unwrap();
        store
            .note_vectors_pending(&[weaviate], REPO, &files[..1])
            .await
            .unwrap();
        assert_eq!(
            store.vectors_pending(vellev, REPO, 10).await.unwrap(),
            [("src/a.rs".to_string(), 1), ("src/b.rs".to_string(), 1)]
        );
        assert_eq!(
            store.vectors_pending(weaviate, REPO, 10).await.unwrap(),
            [("src/a.rs".to_string(), 2), ("src/b.rs".to_string(), 1)]
        );
        store
            .clear_vectors_pending(vellev, REPO, "src/a.rs", 1)
            .await
            .unwrap();
        store
            .clear_vectors_pending(vellev, REPO, "src/b.rs", 1)
            .await
            .unwrap();
        assert_eq!(store.count_vectors_pending(vellev, REPO).await.unwrap(), 0);
        assert_eq!(
            store.count_vectors_pending(weaviate, REPO).await.unwrap(),
            2
        );
        store
            .clear_vectors_pending(weaviate, REPO, "src/b.rs", 1)
            .await
            .unwrap();
        assert_eq!(
            store.count_vectors_pending(weaviate, REPO).await.unwrap(),
            1
        );

        // The graph in use keyed: its own calls are the active one's, and
        // its purge keeps the standby's pending files, and no fill.
        let keyed = CodeGraph::from_backend(Arc::clone(store))
            .keyed(VectorKeys::new(vellev, Some(weaviate)).unwrap());
        keyed.note_vectors_pending(REPO, &files[1..]).await.unwrap();
        assert_eq!(keyed.count_vectors_pending(REPO).await.unwrap(), 1);
        assert_eq!(
            keyed
                .count_vectors_pending_in(weaviate, REPO)
                .await
                .unwrap(),
            1,
            "the standby's are not the active one's"
        );
        assert!(keyed.vector_fill(REPO).await.unwrap().is_some());
        keyed.delete_repo_data(REPO).await.unwrap();
        assert_eq!(store.vector_fill(vellev, REPO).await.unwrap(), None);
        assert_eq!(store.count_vectors_pending(vellev, REPO).await.unwrap(), 0);
        assert_eq!(
            store.vectors_pending(weaviate, REPO, 10).await.unwrap(),
            [("src/a.rs".to_string(), 2)],
            "the standby's notes are kept"
        );
        // Without a standby, a purge forgets them all.
        let alone = CodeGraph::from_backend(Arc::clone(store))
            .keyed(VectorKeys::new(weaviate, None).unwrap());
        alone.delete_repo_data(REPO).await.unwrap();
        assert_eq!(
            store.count_vectors_pending(weaviate, REPO).await.unwrap(),
            0
        );

        // A repo dropped leaves no fill nor pending file in any database,
        // a standby's included: there is nothing left to put right.
        for backend in [vellev, weaviate] {
            assert!(store
                .advance_vector_fill(backend, REPO, None, &fill("d1"))
                .await
                .unwrap());
        }
        store
            .note_vectors_pending(&[vellev, weaviate], REPO, &files)
            .await
            .unwrap();
        graph
            .register_repo("kept", "Kept", "/tmp/kept")
            .await
            .unwrap();
        store
            .note_vectors_pending(&[vellev, weaviate], "kept", &files)
            .await
            .unwrap();
        keyed.drop_repo(REPO).await.unwrap();
        for backend in [vellev, weaviate] {
            assert_eq!(store.vector_fill(backend, REPO).await.unwrap(), None);
            assert_eq!(store.count_vectors_pending(backend, REPO).await.unwrap(), 0);
            assert_eq!(
                store.count_vectors_pending(backend, "kept").await.unwrap(),
                2,
                "another repo's are its own"
            );
        }
    }

    #[tokio::test]
    async fn sqlite_keeps_vector_state_per_backend() {
        vector_state_is_kept_per_backend(&CodeGraph::open_in_memory().unwrap()).await;
    }

    /// The names fills and pending files are kept under are the ones
    /// `[knowledge] vector_backend` takes, and that the cutover's checks
    /// query by (`backend = 'vellev'`); a graph not told otherwise keeps
    /// vellev's.
    #[test]
    fn vector_databases_are_named_as_the_configuration_names_them() {
        assert_eq!(VectorKeys::VELLEV, "vellev");
        assert_eq!(VectorKeys::WEAVIATE, "weaviate");
        let keys = VectorKeys::new("weaviate", Some("vellev")).unwrap();
        assert_eq!(
            (keys.active(), keys.standby()),
            ("weaviate", Some("vellev"))
        );
        assert_eq!(
            VectorKeys::default(),
            VectorKeys::new("vellev", None).unwrap()
        );
        assert!(VectorKeys::new("Weaviate", None).is_err());
        assert!(VectorKeys::new("vellev", Some("qdrant")).is_err());
        assert!(VectorKeys::new("vellev", Some("vellev")).is_err());
        let graph = CodeGraph::open_in_memory().unwrap();
        assert_eq!(graph.vector_keys(), &VectorKeys::default());
    }

    #[tokio::test]
    async fn sqlite_keeps_where_vectors_are_complete_per_repo() {
        let graph = CodeGraph::open_in_memory().unwrap();
        graph.register_repo("r", "r", "/r").await.unwrap();
        vectors_complete_in_is_kept_per_repo(&graph, "r").await;
    }
}

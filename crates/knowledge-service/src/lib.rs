//! Shared knowledge service: owns the code graph, indexing pipeline, and
//! query engine. Consumed by both `workforce-hub` (for agent tools) and the
//! local MCP server (for Claude Code tools) so there is a single source of
//! truth for code knowledge orchestration.
//!
//! The summary-generating LLM stays out of this crate — it would drag in the
//! full `workforce-agent` dependency graph. Instead, callers implement
//! [`SummariserBuilder`] and pass it in at construction time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashSet;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use tracing::{info, warn};
use uuid::Uuid;

use workforce_codebase_knowledge::completeness::{IndexCompleteness, NextAction};
use workforce_codebase_knowledge::graph::vectors::{FillStage, VectorFill};
use workforce_codebase_knowledge::graph::{CodeGraph, RepositoryRow, VectorKeys};
use workforce_codebase_knowledge::indexer::crawler::CrawlerConfig;
use workforce_codebase_knowledge::indexer::fill::{VectorFiller, Verified, Verifying, FILL_PAGE};
use workforce_codebase_knowledge::indexer::summary_generator::{SummaryGenerator, SummaryLlm};
use workforce_codebase_knowledge::indexer::{
    IndexResult, IndexingConfig, IndexingPipeline, RunOptions,
};
use workforce_codebase_knowledge::lease::{
    now_ms, AcquireOutcome, IndexLease, IndexPhase, LeaseContext,
};
use workforce_codebase_knowledge::query::{QueryEngine, VectorWait};
use workforce_codebase_knowledge::vector::{
    CodeVectorStore, VellevCodeVectorStore, WeaviateCodeVectorStore,
};
use workforce_common::settings::{
    CodegraphConfig, KnowledgeConfig, VellevConfig, WeaviateConfig, WorkforceConfig,
};
use workforce_postgres::PostgresConfig;

pub mod project;
pub mod schedule;
pub use project::{ProjectConfig, RepoConfig, RepoSource};
pub use schedule::{EffectiveSchedule, ReindexMode, ScheduleSpec};

/// Per-repo outcome of an [`index_project`](KnowledgeService::index_project) run.
pub struct RepoIndexReport {
    pub name: String,
    pub path: PathBuf,
    pub repo_id: String,
    pub outcome: Result<IndexResult>,
}

/// Aggregated outcome of indexing every repo in a project.
pub struct ProjectIndexReport {
    pub project_name: String,
    pub repos: Vec<RepoIndexReport>,
}

/// Builds a [`SummaryLlm`] on demand. Implemented by the host (e.g. `workforce-hub`)
/// so this crate stays free of LLM-provider dependencies.
pub trait SummariserBuilder: Send + Sync {
    fn build(&self) -> Result<Box<dyn SummaryLlm>>;
}

/// What [`KnowledgeService::trigger_for`] actually did. Returned to
/// callers (e.g. the `knowledge.index trigger` agent tool) so they can
/// surface the right user-facing message.
pub enum TriggerOutcome {
    Complete,
    AlreadyIndexing(IndexLease),
    Full(IndexResult),
    Incremental(IndexResult),
    ResumeSummaries(IndexResult),
}

/// Shared service that manages the code graph and indexing pipeline
/// for all knowledge tools.
pub struct KnowledgeService {
    graph: Arc<CodeGraph>,
    query_engine: Arc<QueryEngine>,
    vector_store: Option<Arc<dyn CodeVectorStore>>,
    workspace: PathBuf,
    /// Path to the on-disk codegraph database. `None` for in-memory
    /// (test) instances. Stashed for memory observability — SQLite mmaps
    /// part of this file into RAM, so its size is a useful proxy for
    /// the codegraph share of process RSS.
    db_path: Option<PathBuf>,
    /// In-process mirror of which repos this hub is actively indexing.
    /// The persistent lease in storage is the source of truth for
    /// cross-process correctness; this set exists so the synchronous
    /// `is_indexing_repo` check (used by status endpoints on the hot
    /// path) doesn't pay a DB round trip.
    indexing_repos: Arc<DashSet<String>>,
    /// How each repo's vectors stood when they were last compared with
    /// the graph, or filled from it.
    compared: dashmap::DashMap<String, Compared>,
    /// How often a repo's vectors are compared with the graph whatever
    /// else is seen.
    reconcile: Duration,
    /// Why each repo's last fill pass left something undone.
    fill_errors: dashmap::DashMap<String, String>,
    /// How each repo's vectors stood when a pass last looked: what status
    /// reports read, so that reading them asks nothing of the vector
    /// store or the graph.
    vector_status: std::sync::Mutex<Vec<RepoVectors>>,
    fill_pace: FillPace,
    /// Whether this service keeps its vectors in line with the graph in
    /// the background.
    keeping: std::sync::atomic::AtomicBool,
    /// Per-process worker id stamped on every lease this service
    /// acquires. A UUID generated at construction; lets storage tell
    /// our heartbeats apart from another hub's that touched the same
    /// data dir.
    worker_id: String,
    parse_concurrency: usize,
    summary_concurrency: usize,
    generate_summaries: bool,
    summariser: Option<Arc<dyn SummariserBuilder>>,
    summariser_max_tokens: usize,
    exclude_patterns: Vec<String>,
    max_file_size: u64,
    index_timeout: Duration,
}

/// What a pass of [`KnowledgeService::fill_vectors`] did, by repo id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VectorBackfill {
    /// Repos whose vectors were filled from the graph, or whose pending
    /// files were put right.
    pub filled: Vec<String>,
    /// Repos left with something undone, to be gone on with: each fill
    /// keeps its place.
    pub failed: Vec<String>,
}

/// How a repo's code vectors stand, for status reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RepoVectors {
    pub repo_id: String,
    pub name: String,
    /// Whether the store holds every entity and summary of the repo, so
    /// that questions of it are answered with vectors.
    pub complete: bool,
    /// How far a fill has got, if one ever began.
    pub fill: Option<VectorFill>,
    /// Files whose vectors are behind the graph, to be put right.
    pub pending_files: usize,
    /// Why the last pass left the repo with something undone.
    pub last_error: Option<String>,
    /// How long ago the store was last compared with the graph, entity by
    /// entity, or filled from it; `None` if not since this process began.
    pub compared_secs_ago: Option<u64>,
}

/// How a repo's vectors stood when the store was last compared with the
/// graph, or filled from it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Compared {
    at: Instant,
    collection: String,
    counts: Counts,
}

/// How much of a repo the store holds and the graph has: a change in
/// either that the other does not follow is a reason to compare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    stored_entities: usize,
    stored_summaries: usize,
    entities: usize,
    summaries: u64,
}

impl Counts {
    /// Whether the store has gained or lost as many vectors since `self`
    /// as the graph has entities and summaries: what an index run that
    /// stored what it indexed leaves, and a store that lost vectors, or
    /// was put back to an earlier state with another number of them, does
    /// not.
    fn kept_pace(&self, now: &Counts) -> bool {
        let moved = |was: usize, is: usize| is as i128 - was as i128;
        moved(self.stored_entities, now.stored_entities) == moved(self.entities, now.entities)
            && moved(self.stored_summaries, now.stored_summaries)
                == moved(self.summaries as usize, now.summaries as usize)
    }
}

/// How a pass over the repos' vectors goes ([`KnowledgeService::fill`]).
#[derive(Debug, Clone, Copy)]
pub struct FillRun {
    pub pace: FillPace,
    /// Fill every repo again from the graph, whatever is recorded of its
    /// vectors: every vector is written over and what the graph has not
    /// is removed.
    pub force: bool,
    /// How many repos have a page stored at once.
    pub at_once: usize,
    /// How often the pass looks for repos indexed since it began, and
    /// again at the ones it had finished with.
    pub rescan: Duration,
}

impl FillRun {
    pub fn at(pace: FillPace) -> Self {
        Self {
            pace,
            force: false,
            at_once: 1,
            rescan: RESCAN,
        }
    }
}

/// A repo's part in a pass.
struct Job {
    repo: RepositoryRow,
    /// Whether the collection is still to be looked at afresh.
    afresh: bool,
    force: bool,
    comparing: Option<Verifying>,
    /// Compared or filled in this pass.
    compared: bool,
    did: bool,
    /// Files noted as pending when last looked.
    pending: usize,
    /// Nothing left to do, when last looked.
    over: bool,
    failed: Option<String>,
    /// When a job that failed is tried again within the pass.
    again_at: Option<Instant>,
    retry: Duration,
}

/// How often a pass looks for repos indexed since it began, and at the
/// ones it had finished with, unless told otherwise.
const RESCAN: Duration = Duration::from_secs(5);
/// How often, at most, a repo is compared because a question found none of
/// its vectors, while nothing else says they have changed.
const SUSPECT_EVERY: Duration = Duration::from_secs(60);
/// The wait between pages of a paced comparison.
const COMPARE_PAUSE: Duration = Duration::from_millis(100);

/// How a fill of code vectors is paced.
#[derive(Debug, Clone, Copy)]
pub struct FillPace {
    /// The most entities and summaries stored per second; `0` for no limit.
    pub per_sec: u32,
    /// Entities or summaries per page.
    pub page: usize,
    /// How long one page may take before it is given up, to be tried
    /// again: each page is bounded, the whole fill is not.
    pub page_timeout: Duration,
    /// How many pages of a repo are stored at a time: one, unless
    /// something outside the node embeds them and is to be kept busy
    /// while a page is written and recorded.
    pub ahead: usize,
}

impl Default for FillPace {
    fn default() -> Self {
        Self::at(5)
    }
}

/// Entities or summaries per page of a paced fill. The node runs its model
/// on one batch at a time, and a question waits for the batch under way:
/// a paced fill sends batches a question does not wait long for.
const PACED_PAGE: usize = 16;

impl FillPace {
    /// At most `per_sec` stored a second, in pages small enough that the
    /// questions the node embeds meanwhile wait for little; or, at `0`, as
    /// fast as the store takes whole pages.
    pub fn at(per_sec: u32) -> Self {
        Self {
            per_sec,
            page: if per_sec > 0 { PACED_PAGE } else { FILL_PAGE },
            page_timeout: Duration::from_secs(600),
            ahead: 1,
        }
    }
}

/// Held by the round of a fill pass under way in this process: a hub makes
/// a service for each agent's knowledge, which may share one graph and do
/// share the node, so their passes take rounds in turn, a page a repo.
static FILL_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The first wait before a vector backfill that failed is tried again; it
/// doubles each time, up to [`BACKFILL_WATCH`].
const BACKFILL_RETRY: Duration = Duration::from_secs(30);
/// How often vectors are checked once every repo's are complete.
const BACKFILL_WATCH: Duration = Duration::from_secs(600);

/// Where code vectors are kept.
#[derive(Clone)]
pub enum VectorBackend<'a> {
    /// Weaviate, when it is enabled.
    Weaviate(&'a WeaviateConfig),
    /// vellev, in its tenant.
    Vellev(&'a VellevConfig),
    /// A store made by the caller: a vellev one that is given its vectors,
    /// say (see `VellevCodeVectorStore::seeded_by`).
    Given(Arc<dyn CodeVectorStore>),
}

impl<'a> VectorBackend<'a> {
    /// vellev when `[knowledge] vector_backend` says so, otherwise
    /// Weaviate: wherever memories are kept.
    pub fn of(config: &'a WorkforceConfig) -> Self {
        if config.code_vectors_in_vellev() {
            Self::Vellev(&config.vellev)
        } else {
            Self::Weaviate(&config.weaviate)
        }
    }

    /// The database's name, as `[knowledge] vector_backend` gives it and
    /// the graph keeps its vector state under (see [`VectorKeys`]).
    fn key(&self) -> &'static str {
        match self {
            Self::Weaviate(_) => VectorKeys::WEAVIATE,
            Self::Vellev(_) => VectorKeys::VELLEV,
            // A given store says which database it is.
            Self::Given(store) => store.backend(),
        }
    }

    /// The database's name, as logs give it.
    fn label(&self) -> &'static str {
        match self {
            Self::Weaviate(_) => "Weaviate",
            Self::Vellev(_) | Self::Given(_) => "vellev",
        }
    }

    /// The store, with its collections being made in the background, or
    /// `None` when Weaviate is not enabled.
    fn store(self) -> Result<Option<Arc<dyn CodeVectorStore>>> {
        let label = self.label();
        let store: Arc<dyn CodeVectorStore> = match self {
            Self::Given(store) => store,
            Self::Weaviate(weaviate) if !weaviate.enabled => return Ok(None),
            Self::Weaviate(weaviate) => Arc::new(WeaviateCodeVectorStore::new(
                &weaviate.url,
                weaviate.api_key.as_deref(),
            )),
            Self::Vellev(vellev) => {
                let key = vellev
                    .api_key
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("[vellev] api_key is not set"))?;
                Arc::new(VellevCodeVectorStore::new(
                    &vellev.url,
                    key,
                    &vellev.tenant,
                )?)
            }
        };
        let ensuring = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = ensuring.ensure_collections().await {
                tracing::warn!("{label} collection setup failed: {}", e);
            }
        });
        info!("Vector store enabled ({})", store.base_url());
        Ok(Some(store))
    }
}

/// Snapshot of knowledge-service state for memory observability.
#[derive(Debug, Default, Clone, Copy)]
pub struct KnowledgeServiceStats {
    /// Size in bytes of the on-disk codegraph SQLite database (proxy
    /// for SQLite mmap usage). Zero for in-memory instances or when the
    /// file isn't readable.
    pub db_bytes: u64,
    /// Number of repos currently being indexed.
    pub indexing_repos: u64,
}

impl KnowledgeService {
    /// Create a new KnowledgeService, opening or creating the code
    /// graph database. Convenience wrapper that always uses the
    /// SQLite backend — kept for tests and back-compat.
    ///
    /// `root` is the directory to index (can be read-only).
    /// `data_dir` is where the SQLite DB is stored (must be writable).
    pub fn new(
        root: PathBuf,
        data_dir: PathBuf,
        config: &KnowledgeConfig,
        weaviate: &WeaviateConfig,
        summariser: Option<Arc<dyn SummariserBuilder>>,
    ) -> Result<Self> {
        Self::new_with_backend(
            root,
            data_dir,
            config,
            VectorBackend::Weaviate(weaviate),
            &CodegraphConfig::default(),
            None,
            summariser,
        )
    }

    /// Variant that honours `[codegraph] backend = "postgres"` when
    /// the `postgres` feature is enabled and a `PostgresConfig` is
    /// available. Fails closed when `"postgres"` is requested but
    /// unavailable, rather than silently downgrading to SQLite.
    ///
    /// Synchronous because the overwhelmingly common case (SQLite)
    /// is synchronous; the rare Postgres path uses
    /// `tokio::task::block_in_place` + `Handle::current().block_on`
    /// under the hood, which works inside every workforce hub startup
    /// context (all of which run under a multi-threaded tokio
    /// runtime).
    pub fn new_with_backend(
        root: PathBuf,
        data_dir: PathBuf,
        config: &KnowledgeConfig,
        vectors: VectorBackend<'_>,
        codegraph_config: &CodegraphConfig,
        postgres_config: Option<&PostgresConfig>,
        summariser: Option<Arc<dyn SummariserBuilder>>,
    ) -> Result<Self> {
        // Ensure data directory exists
        let knowledge_dir = data_dir.join(".workforce").join("knowledge");
        std::fs::create_dir_all(&knowledge_dir)?;
        let db_path = knowledge_dir.join("codegraph.db");
        // Its vector state is kept as the database in use's, with the
        // standby told of every file changed: a standby that is no
        // database, or the one in use, is refused.
        let standby = Some(config.vector_standby.trim()).filter(|standby| !standby.is_empty());
        let keys =
            VectorKeys::new(vectors.key(), standby).wrap_err("[knowledge] vector_standby")?;
        let graph = build_code_graph(&db_path, codegraph_config, postgres_config)?.keyed(keys);
        let graph = Arc::new(graph);

        // A vector store that cannot be made leaves the code graph and its
        // tools in service, without semantic search.
        let vector_store = vectors.store().unwrap_or_else(|e| {
            warn!(error = %e, "Vector store unavailable; code knowledge runs without semantic search");
            None
        });

        if let Some(vs) = &vector_store {
            vs.keep_for_graph(graph.identity());
        }

        let query_engine = if let Some(ref vs) = vector_store {
            let wait = VectorWait {
                deadline: Duration::from_millis(config.vector_search_timeout_ms),
                ..VectorWait::default()
            };
            Arc::new(QueryEngine::with_vector_store(graph.clone(), vs.clone()).waiting(wait))
        } else {
            Arc::new(QueryEngine::new(graph.clone()))
        };

        let parse_concurrency = if config.parse_concurrency == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get() * 2)
                .unwrap_or(8)
        } else {
            config.parse_concurrency
        };

        let summary_concurrency = if config.summary_concurrency == 0 {
            16
        } else {
            config.summary_concurrency
        };

        info!(
            "Knowledge service initialised (db: {}, parse: {}, summary: {}, summaries: {}, vector: {})",
            db_path.display(),
            parse_concurrency,
            summary_concurrency,
            config.generate_summaries,
            vector_store.is_some(),
        );

        Ok(Self {
            graph,
            query_engine,
            vector_store,
            workspace: root,
            db_path: Some(db_path),
            indexing_repos: Arc::new(DashSet::new()),
            compared: dashmap::DashMap::new(),
            reconcile: Duration::from_secs(config.vector_reconcile_secs.max(1)),
            fill_errors: dashmap::DashMap::new(),
            vector_status: std::sync::Mutex::new(Vec::new()),
            fill_pace: FillPace::at(config.vector_backfill_per_sec),
            keeping: std::sync::atomic::AtomicBool::new(false),
            worker_id: Uuid::new_v4().to_string(),
            parse_concurrency,
            summary_concurrency,
            generate_summaries: config.generate_summaries,
            summariser,
            summariser_max_tokens: 1024,
            exclude_patterns: config.exclude_patterns.clone(),
            max_file_size: config.max_file_size,
            index_timeout: Duration::from_secs(config.index_timeout_secs),
        })
    }

    /// Snapshot the knowledge service for memory observability.
    pub fn cache_stats(&self) -> KnowledgeServiceStats {
        let db_bytes = self
            .db_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        KnowledgeServiceStats {
            db_bytes,
            indexing_repos: self.indexing_repos.len() as u64,
        }
    }

    /// Create a KnowledgeService with an in-memory database (for testing).
    pub fn in_memory(workspace: PathBuf) -> Result<Self> {
        let graph = Arc::new(CodeGraph::open_in_memory()?);
        let query_engine = Arc::new(QueryEngine::new(graph.clone()));
        Ok(Self {
            graph,
            query_engine,
            vector_store: None,
            workspace,
            db_path: None,
            indexing_repos: Arc::new(DashSet::new()),
            compared: dashmap::DashMap::new(),
            reconcile: Duration::from_secs(21_600),
            fill_errors: dashmap::DashMap::new(),
            vector_status: std::sync::Mutex::new(Vec::new()),
            fill_pace: FillPace::default(),
            keeping: std::sync::atomic::AtomicBool::new(false),
            worker_id: Uuid::new_v4().to_string(),
            parse_concurrency: 8,
            summary_concurrency: 16,
            generate_summaries: false,
            summariser: None,
            summariser_max_tokens: 1024,
            exclude_patterns: Vec::new(),
            max_file_size: 1_048_576,
            index_timeout: Duration::from_secs(1800),
        })
    }

    pub fn graph(&self) -> &Arc<CodeGraph> {
        &self.graph
    }

    pub fn query_engine(&self) -> &Arc<QueryEngine> {
        &self.query_engine
    }

    pub fn vector_store(&self) -> &Option<Arc<dyn CodeVectorStore>> {
        &self.vector_store
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Derive a repository ID from a path.
    pub fn repo_id_for(path: &Path) -> String {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        let hash = blake3::hash(path.to_string_lossy().as_bytes()).to_hex()[..8].to_string();
        format!("{name}-{hash}")
    }

    /// Derive a repository name from a path.
    pub fn repo_name_for(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string())
    }

    /// Derive a repository ID from the default workspace path.
    pub fn repo_id(&self) -> String {
        Self::repo_id_for(&self.workspace)
    }

    /// Derive a repository name from the default workspace path.
    pub fn repo_name(&self) -> String {
        Self::repo_name_for(&self.workspace)
    }

    /// Resolve an optional relative path to a repo_id.
    ///
    /// When `path` is `Some`, resolves it relative to `agent_workspace`,
    /// canonicalises, and derives a repo_id. When `None`, falls back to the
    /// service's default workspace repo_id.
    pub fn resolve_repo_id(
        &self,
        agent_workspace: &Path,
        path: &Option<String>,
    ) -> Result<String, String> {
        match path {
            Some(rel) => {
                let resolved = agent_workspace.join(rel);
                if !resolved.is_dir() {
                    return Err(format!(
                        "Path '{}' does not exist or is not a directory (resolved to {})",
                        rel,
                        resolved.display()
                    ));
                }
                let canonical = std::fs::canonicalize(&resolved)
                    .map_err(|e| format!("Cannot resolve path '{}': {}", resolved.display(), e))?;
                Ok(Self::repo_id_for(&canonical))
            }
            None => Ok(self.repo_id()),
        }
    }

    fn build_pipeline_for(&self, root: &Path) -> IndexingPipeline {
        let crawler = CrawlerConfig {
            root: root.to_path_buf(),
            max_file_size: self.max_file_size,
            exclude_patterns: CrawlerConfig::default()
                .exclude_patterns
                .into_iter()
                .chain(self.exclude_patterns.iter().cloned())
                .collect(),
            ..CrawlerConfig::default()
        };

        let config = IndexingConfig {
            root: root.to_path_buf(),
            repo_id: Self::repo_id_for(root),
            repo_name: Self::repo_name_for(root),
            crawler,
            parse_concurrency: self.parse_concurrency,
            summary_concurrency: self.summary_concurrency,
            generate_summaries: self.generate_summaries,
        };

        let pipeline = match self.vector_store.as_ref() {
            Some(vs) => IndexingPipeline::with_vector_store(config, self.graph.clone(), vs.clone()),
            None => IndexingPipeline::new(config, self.graph.clone()),
        };

        if !self.generate_summaries {
            return pipeline;
        }

        let Some(builder) = self.summariser.as_ref() else {
            warn!(
                "generate_summaries is enabled but no SummariserBuilder was provided; \
                 proceeding without summaries"
            );
            return pipeline;
        };

        match builder.build() {
            Ok(llm) => pipeline
                .with_summary_generator(SummaryGenerator::new(llm, self.summariser_max_tokens)),
            Err(e) => {
                warn!("Summariser construction failed ({e}); proceeding without summaries");
                pipeline
            }
        }
    }

    /// What fills this service's vector store from its graph, if the
    /// store is one whose vectors of a repo may be missing or behind.
    fn filler(&self, pace: FillPace) -> Option<VectorFiller> {
        let store = self.vector_store.as_ref()?;
        store.tracks_completeness().then(|| {
            VectorFiller::new(Arc::clone(&self.graph), Arc::clone(store))
                .paged(pace.page)
                .ahead(pace.ahead)
        })
    }

    /// How every indexed repo's code vectors stood when a fill pass last
    /// looked, and how far a fill under way has got since. It asks nothing
    /// of the vector store or the graph, so a liveness probe may read it
    /// however either is doing; empty before the first pass.
    pub fn vector_status_seen(&self) -> Vec<RepoVectors> {
        let seen = self.vector_status.lock().unwrap_or_else(|e| e.into_inner());
        seen.clone()
    }

    /// Look at how every repo's vectors stand, for
    /// [`Self::vector_status_seen`].
    async fn see_vector_status(&self) {
        match self.vector_status().await {
            Ok(status) => *self.vector_status.lock().unwrap_or_else(|e| e.into_inner()) = status,
            Err(e) => warn!("code vector status could not be read: {e:#}"),
        }
    }

    /// How every indexed repo's code vectors stand, asked of the graph and
    /// the vector store now.
    pub async fn vector_status(&self) -> Result<Vec<RepoVectors>> {
        let Some(filler) = self.filler(self.fill_pace) else {
            return Ok(Vec::new());
        };
        let mut repos = Vec::new();
        for repo in self.graph.list_repositories().await? {
            if self.graph.get_index_state(&repo.id).await?.is_none() {
                continue;
            }
            repos.push(RepoVectors {
                complete: filler.complete(&repo.id, false).await.unwrap_or(false),
                fill: self.graph.vector_fill(&repo.id).await?,
                pending_files: self.graph.count_vectors_pending(&repo.id).await?,
                last_error: self.fill_errors.get(&repo.id).map(|e| e.clone()),
                compared_secs_ago: self
                    .compared
                    .get(&repo.id)
                    .map(|compared| compared.at.elapsed().as_secs()),
                name: repo.name,
                repo_id: repo.id,
            });
        }
        Ok(repos)
    }

    /// How much of the repo the store holds and the graph has.
    async fn counts(&self, store: &Arc<dyn CodeVectorStore>, repo_id: &str) -> Result<Counts> {
        Ok(Counts {
            stored_entities: store.entity_count(repo_id).await?,
            stored_summaries: store.summary_count(repo_id).await?,
            entities: self.graph.get_stats(repo_id).await?.entity_count,
            summaries: self.graph.count_summaries_present(repo_id).await?,
        })
    }

    /// Whether the repo's vectors, recorded complete, are to be compared
    /// with the graph now: not yet since this process began, or since the
    /// collection became another; not for as long as
    /// `[knowledge] vector_reconcile_secs`; when the store has gained or
    /// lost another number of them since it was last looked at than the
    /// graph has, as after the store lost some or was put back to an
    /// earlier state (an index run that stored what it indexed moves both
    /// alike, and is no reason); or when a question found none of them,
    /// though not for that alone more than once a minute.
    async fn compare_due(&self, store: &Arc<dyn CodeVectorStore>, repo_id: &str) -> Result<bool> {
        let suspected = self.query_engine.vectors_suspected();
        let suspected = suspected.lock().expect("not poisoned").remove(repo_id);
        let Some(last) = self.compared.get(repo_id).map(|last| last.clone()) else {
            return Ok(true);
        };
        // A repo with nothing to embed is found empty by every question:
        // it is not gone through for each of them.
        Ok((suspected && last.at.elapsed() >= SUSPECT_EVERY)
            || last.at.elapsed() >= self.reconcile
            || last.collection != store.identity(false).await?
            || !last.counts.kept_pace(&self.counts(store, repo_id).await?))
    }

    /// Do one thing for a repo's vectors, and say whether there may be
    /// more to do. In order: put right a page of the files noted as
    /// pending, if the vectors are complete; store one page of a fill, if
    /// they are not complete in the collection the store has now, or one
    /// was begun; compare one page of the graph with the store, if that is
    /// due (see [`Self::compare_due`]).
    async fn turn(
        &self,
        filler: &VectorFiller,
        job: &mut Job,
        pace: FillPace,
        progress: &(dyn Fn(&str, &VectorFill) + Send + Sync),
    ) -> Result<bool> {
        let Some(store) = &self.vector_store else {
            return Ok(false);
        };
        let repo_id = job.repo.id.clone();
        if self.graph.get_index_state(&repo_id).await?.is_none() {
            return Ok(false);
        }
        // An index run in progress is changing the graph, and a full one
        // begins the fill again when it ends: wait for it.
        if let Some(lease) = self.graph.get_lease(&repo_id).await? {
            if lease.is_fresh(now_ms()) {
                // Nothing of the fill is left on its way to the store
                // while the run changes the graph, or when it begins the
                // fill again at its end.
                filler.let_go(&repo_id).await;
                return Err(eyre::eyre!(
                    "'{}' is being indexed ({}); its vectors are filled after",
                    repo_id,
                    lease.phase.as_str()
                ));
            }
        }
        let timed = |what: &'static str| {
            let repo_id = repo_id.clone();
            move |_| {
                eyre::eyre!(
                    "a page of the code vectors of '{repo_id}' was not {what} within {:?}",
                    pace.page_timeout
                )
            }
        };
        let complete = filler.complete(&repo_id, job.afresh).await?;
        job.afresh = false;
        if job.force {
            // Written over from the graph, searched meanwhile if whole.
            filler.begin(&repo_id, complete).await?;
            job.force = false;
            job.did = true;
        }
        job.pending = self.graph.count_vectors_pending(&repo_id).await?;
        if complete && job.pending > 0 {
            tokio::time::timeout(pace.page_timeout, filler.settle_pending(&repo_id))
                .await
                .map_err(timed("put right"))??;
            job.did = true;
            return Ok(true);
        }
        if !complete || filler.filling(&repo_id).await? {
            let began = Instant::now();
            let before = self.graph.vector_fill(&repo_id).await?;
            let before = before.map_or(0, |fill| fill.done);
            let fill = tokio::time::timeout(pace.page_timeout, filler.step(&repo_id))
                .await
                .map_err(timed("stored"))??;
            progress(&repo_id, &fill);
            {
                let mut seen = self.vector_status.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(seen) = seen.iter_mut().find(|seen| seen.repo_id == repo_id) {
                    seen.fill = Some(fill.clone());
                }
            }
            job.did = true;
            if fill.stage == FillStage::Done {
                // What an index run noted meanwhile and did not store.
                filler.settle_pending(&repo_id).await?;
                // Then compared with the graph, whatever the fill stored:
                // a write of an earlier fill of the repo, given up and
                // still landing as this one went, would have put an
                // earlier vector over one of this fill's, which its sweep
                // then took for stale and removed.
                job.comparing = Some(Verifying::default());
                return Ok(true);
            }
            let stored = fill.done.saturating_sub(before);
            if pace.per_sec > 0 && stored > 0 {
                let due = Duration::from_secs_f64(stored as f64 / f64::from(pace.per_sec));
                tokio::time::sleep(due.saturating_sub(began.elapsed())).await;
            }
            return Ok(true);
        }
        if job.comparing.is_none() && !job.compared && self.compare_due(store, &repo_id).await? {
            job.comparing = Some(Verifying::default());
        }
        if let Some(at) = &mut job.comparing {
            let compared =
                tokio::time::timeout(pace.page_timeout, filler.verify_step(&repo_id, at))
                    .await
                    .map_err(timed("compared"))??;
            match compared {
                Verified::Going => {
                    if pace.per_sec > 0 {
                        tokio::time::sleep(COMPARE_PAUSE).await;
                    }
                }
                Verified::Done {
                    compared,
                    differing,
                    strays,
                } => {
                    job.comparing = None;
                    job.compared = true;
                    if differing + strays > 0 {
                        job.did = true;
                        warn!(
                            repo_id = %repo_id,
                            compared,
                            differing,
                            strays,
                            "the vector store did not hold the repo as the graph has it; the files concerned are put right"
                        );
                    }
                }
                Verified::Refilling => {
                    job.comparing = None;
                    job.did = true;
                }
            }
            return Ok(true);
        }
        // Complete, nothing pending, compared if that was due: how it
        // stands now is what the next pass compares with.
        let counts = self.counts(store, &repo_id).await?;
        let collection = store.identity(false).await?;
        let at = match (job.compared, self.compared.get(&repo_id)) {
            (false, Some(last)) => last.at,
            _ => Instant::now(),
        };
        let stands = Compared {
            at,
            collection,
            counts,
        };
        self.compared.insert(repo_id, stands);
        Ok(false)
    }

    /// The jobs of a pass brought up to date: one for each repo indexed
    /// (or for `only`), those finished with opened again if their repo has
    /// files pending or its vectors are no longer complete, and those that
    /// failed tried again when that is due.
    async fn jobs(
        &self,
        filler: &VectorFiller,
        jobs: &mut Vec<Job>,
        only: Option<&str>,
        run: &FillRun,
        first: bool,
    ) -> Result<()> {
        for repo in self.graph.list_repositories().await? {
            if only.is_some_and(|only| only != repo.id) {
                continue;
            }
            match jobs.iter_mut().find(|job| job.repo.id == repo.id) {
                None => jobs.push(Job {
                    pending: self.graph.count_vectors_pending(&repo.id).await?,
                    repo,
                    afresh: true,
                    // Only what was there when the pass began is filled
                    // again at an operator's word.
                    force: run.force && first,
                    comparing: None,
                    compared: false,
                    did: false,
                    over: false,
                    failed: None,
                    again_at: None,
                    retry: BACKFILL_RETRY,
                }),
                Some(job) if job.failed.is_some() => {
                    if job.again_at.is_some_and(|at| Instant::now() >= at) {
                        job.failed = None;
                        job.over = false;
                        job.afresh = true;
                    }
                }
                Some(job) if job.over => {
                    job.pending = self.graph.count_vectors_pending(&repo.id).await?;
                    let complete = filler.complete(&repo.id, false).await.unwrap_or(false);
                    let suspected = self.query_engine.vectors_suspected();
                    let suspected = suspected.lock().expect("not poisoned").contains(&repo.id);
                    if job.pending > 0 || !complete || suspected {
                        job.over = false;
                        job.compared &= !suspected;
                    }
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Bring the code vectors of every indexed repo, or of `only`, in line
    /// with the graph, as `run` says, reporting each page of a fill as
    /// `progress`. Nothing is crawled.
    ///
    /// The pass goes round the repos, each having one turn a round (see
    /// [`Self::turn`]): a page of pending files put right, a page of a
    /// fill stored, or a page compared. Repos with files pending go first
    /// in a round, so that a repo being filled for hours holds no other's
    /// repairs up for longer than one of its pages takes, nor the fill of
    /// a repo indexed meanwhile: the repos are looked at again as the pass
    /// goes. A repo that fails is tried again later in the pass while
    /// others are still at work, and named in what is returned if it had
    /// not succeeded by the end; its fill keeps its place. Only for a
    /// store that tracks completeness; others have nothing to do.
    pub async fn fill(
        &self,
        only: Option<&str>,
        run: FillRun,
        progress: &(dyn Fn(&str, &VectorFill) + Send + Sync),
    ) -> Result<VectorBackfill> {
        let mut pass = VectorBackfill::default();
        let Some(filler) = self.filler(run.pace) else {
            return self.drain(only, run.pace).await;
        };
        let mut jobs: Vec<Job> = Vec::new();
        let mut scanned: Option<Instant> = None;
        loop {
            // One round at a time in this process.
            let _turn = FILL_TURN.lock().await;
            if scanned.is_none_or(|at| at.elapsed() >= run.rescan) {
                self.jobs(&filler, &mut jobs, only, &run, scanned.is_none())
                    .await?;
                self.see_vector_status().await;
                scanned = Some(Instant::now());
            }
            let mut round: Vec<&mut Job> = jobs.iter_mut().filter(|job| !job.over).collect();
            if round.is_empty() {
                break;
            }
            // Repairs before fills: a stable sort, so otherwise as listed.
            round.sort_by_key(|job| job.pending == 0);
            let filler = &filler;
            futures::stream::iter(round)
                .for_each_concurrent(run.at_once.max(1), |job| async move {
                    match self.turn(filler, job, run.pace, progress).await {
                        Ok(more) => {
                            job.over = !more;
                            job.retry = BACKFILL_RETRY;
                            self.fill_errors.remove(&job.repo.id);
                        }
                        Err(e) => {
                            warn!(repo_id = %job.repo.id, "code vectors not complete: {e:#}");
                            self.fill_errors
                                .insert(job.repo.id.clone(), format!("{e:#}"));
                            job.failed = Some(format!("{e:#}"));
                            job.over = true;
                            job.comparing = None;
                            job.again_at = Some(Instant::now() + job.retry);
                            job.retry = (job.retry * 2).min(BACKFILL_WATCH);
                        }
                    }
                })
                .await;
        }
        self.see_vector_status().await;
        for job in jobs {
            if job.failed.is_some() {
                pass.failed.push(job.repo.id);
            } else if job.did {
                pass.filled.push(job.repo.id);
            }
        }
        Ok(pass)
    }

    /// [`Self::fill`] of every repo, or of `only`, at `pace`: nothing is
    /// filled again that is recorded complete and found so, one repo at a
    /// time.
    pub async fn fill_vectors(
        &self,
        only: Option<&str>,
        pace: FillPace,
        progress: &(dyn Fn(&str, &VectorFill) + Send + Sync),
    ) -> Result<VectorBackfill> {
        self.fill(only, FillRun::at(pace), progress).await
    }

    /// [`Self::fill_vectors`] of every repo at the configured pace.
    pub async fn backfill_vectors(&self) -> Result<VectorBackfill> {
        self.fill_vectors(None, self.fill_pace, &|_, _| {}).await
    }

    /// Keep every repo's vectors complete in the background: a pass of
    /// [`Self::backfill_vectors`] now, then again after a wait that grows
    /// from [`BACKFILL_RETRY`] while passes fail, and is [`BACKFILL_WATCH`]
    /// once none does, or less when a question finds a repo's vectors
    /// incomplete or lost: an index run leaves a repo to fill or files to
    /// put right, and a collection may be lost.
    pub fn spawn_vector_backfill(self: &Arc<Self>) {
        self.spawn_vector_backfill_paced(BACKFILL_RETRY, BACKFILL_WATCH);
    }

    /// Whether questions are asking the vector store, or leaving it alone
    /// (see [`QueryEngine::vector_breaker`]).
    pub fn vector_breaker(&self) -> Option<&'static str> {
        self.query_engine.vector_breaker()
    }

    /// Whether this service keeps its vectors in line with the graph in
    /// the background ([`Self::spawn_vector_backfill`]).
    pub fn keeps_vectors(&self) -> bool {
        self.keeping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// As [`Self::spawn_vector_backfill`], waiting `retry` at first after
    /// a pass that failed and `watch` after one that did not.
    pub fn spawn_vector_backfill_paced(self: &Arc<Self>, retry: Duration, watch: Duration) {
        // A store that keeps no track is not filled, but has the files
        // noted for it while it was the standby put right (see
        // [`Self::drain`]).
        if self.vector_store.is_none() {
            return;
        }
        self.keeping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let service = Arc::downgrade(self);
        let wanted = self.query_engine.vectors_wanted();
        tokio::spawn(async move {
            let mut next_retry = retry;
            loop {
                let Some(service) = service.upgrade() else {
                    return;
                };
                let pass = service.backfill_vectors().await;
                drop(service);
                match pass {
                    Ok(pass) if pass.failed.is_empty() => {
                        if !pass.filled.is_empty() {
                            info!(repos = ?pass.filled, "code vectors brought in line with the graph");
                        }
                        next_retry = retry;
                        tokio::select! {
                            () = tokio::time::sleep(watch) => {}
                            () = wanted.notified() => {}
                        }
                    }
                    failed => {
                        if let Err(e) = failed {
                            warn!("code vector backfill could not run: {e}");
                        }
                        tokio::time::sleep(next_retry).await;
                        next_retry = (next_retry * 2).min(watch);
                    }
                }
            }
        });
    }

    /// For a store that keeps no track of its vectors' completeness, as
    /// Weaviate's: put right, file by file, the files noted pending for it
    /// (of every repo, or of `only`), as they were noted while it was the
    /// standby ([`VectorKeys::standby`]); nothing else, for it is never
    /// filled. A repo being indexed is left for the next pass, as is one
    /// whose files are noted again as fast as they are put right; each
    /// takes [`FILL_TURN`] a page at a time, as a fill does.
    async fn drain(&self, only: Option<&str>, pace: FillPace) -> Result<VectorBackfill> {
        let mut pass = VectorBackfill::default();
        let Some(store) = self.vector_store.as_ref() else {
            return Ok(pass);
        };
        let filler = VectorFiller::new(Arc::clone(&self.graph), Arc::clone(store));
        for repo in self.graph.list_repositories().await? {
            if only.is_some_and(|only| only != repo.id) {
                continue;
            }
            let mut left = self.graph.count_vectors_pending(&repo.id).await?;
            if left == 0 {
                continue;
            }
            if let Some(lease) = self.graph.get_lease(&repo.id).await? {
                if lease.is_fresh(now_ms()) {
                    pass.failed.push(repo.id);
                    continue;
                }
            }
            let settled = loop {
                let _turn = FILL_TURN.lock().await;
                let page = tokio::time::timeout(pace.page_timeout, filler.settle_pending(&repo.id));
                match page.await {
                    Ok(Ok(0)) => break Ok(()),
                    Ok(Ok(now)) if now < left => left = now,
                    Ok(Ok(now)) => {
                        break Err(eyre::eyre!(
                            "{now} files of '{}' are noted again as fast as they are put right",
                            repo.id
                        ))
                    }
                    Ok(Err(e)) => break Err(e),
                    Err(_) => {
                        break Err(eyre::eyre!(
                            "a page of the pending files of '{}' was not put right within {:?}",
                            repo.id,
                            pace.page_timeout
                        ))
                    }
                }
            };
            match settled {
                Ok(()) => {
                    self.fill_errors.remove(&repo.id);
                    pass.filled.push(repo.id);
                }
                Err(e) => {
                    warn!(repo_id = %repo.id, "code vectors noted pending not put right: {e:#}");
                    self.fill_errors.insert(repo.id.clone(), format!("{e:#}"));
                    pass.failed.push(repo.id);
                }
            }
        }
        Ok(pass)
    }

    /// Trigger a full index of a specific path. Acquires a persistent
    /// lease so a concurrent process (or our own retry) can detect the
    /// run and decide whether to wait or steal a stale heartbeat.
    pub async fn trigger_full_index_for(&self, root: &Path) -> Result<IndexResult> {
        self.run_with_lease(root, IndexPhase::Crawling, |pipeline, opts| {
            Box::pin(async move { pipeline.run_full_index(opts).await })
        })
        .await
    }

    /// Trigger a full index of the default workspace.
    pub async fn trigger_full_index(&self) -> Result<IndexResult> {
        self.trigger_full_index_for(&self.workspace.clone()).await
    }

    /// Trigger an incremental index of a specific path.
    pub async fn trigger_incremental_index_for(&self, root: &Path) -> Result<IndexResult> {
        self.run_with_lease(root, IndexPhase::Crawling, |pipeline, opts| {
            Box::pin(async move { pipeline.run_incremental_index(opts).await })
        })
        .await
    }

    /// "Do the right thing" trigger. Reads the current index state,
    /// dispatches based on [`NextAction`], and returns a [`TriggerOutcome`]
    /// describing what happened. This is what every agent-facing
    /// `trigger` action should call — the older `trigger_full` /
    /// `trigger_incremental` methods stay for explicit overrides.
    pub async fn trigger_for(&self, root: &Path) -> Result<TriggerOutcome> {
        let repo_id = Self::repo_id_for(root);
        let head_commit = self.get_head_commit_for(root).ok();
        let completeness = self.completeness(&repo_id, head_commit.as_deref()).await?;
        match completeness.next_action(now_ms()) {
            NextAction::Complete => Ok(TriggerOutcome::Complete),
            NextAction::AlreadyIndexing(l) => Ok(TriggerOutcome::AlreadyIndexing(l)),
            NextAction::ResumeSummaries => self
                .run_with_lease(root, IndexPhase::GeneratingSummaries, |pipeline, opts| {
                    Box::pin(async move { pipeline.run_summaries_only(opts).await })
                })
                .await
                .map(TriggerOutcome::ResumeSummaries),
            NextAction::Incremental => self
                .trigger_incremental_index_for(root)
                .await
                .map(TriggerOutcome::Incremental),
            NextAction::Full => self
                .trigger_full_index_for(root)
                .await
                .map(TriggerOutcome::Full),
        }
    }

    /// Run a pipeline phase under a freshly-acquired lease. Acquires
    /// the lease, spawns the heartbeat task, drives `run` to completion
    /// (or error), then releases — even on panic. The whole thing is
    /// bounded by `self.index_timeout` to catch genuinely stuck calls
    /// (the lease alone covers correctness; the timeout is a backstop).
    async fn run_with_lease<F>(
        &self,
        root: &Path,
        initial_phase: IndexPhase,
        run: F,
    ) -> Result<IndexResult>
    where
        F: for<'a> FnOnce(
            &'a IndexingPipeline,
            RunOptions,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<IndexResult>> + Send + 'a>,
        >,
    {
        let repo_id = Self::repo_id_for(root);
        let target_commit = self.get_head_commit_for(root).ok();

        // Ensure the `repositories` row exists before we touch the
        // lease — `index_lease.repo_id` is FK-bound to it, so a
        // first-time index of a never-seen repo would otherwise fail
        // its lease INSERT before the pipeline gets to register the
        // repo itself. `register_repo` is `INSERT OR IGNORE`, so this
        // is a no-op once the row exists.
        self.graph
            .register_repo(
                &repo_id,
                &Self::repo_name_for(root),
                &root.to_string_lossy(),
            )
            .await?;

        match self
            .graph
            .acquire_lease(
                &repo_id,
                &self.worker_id,
                target_commit.as_deref(),
                initial_phase,
            )
            .await?
        {
            AcquireOutcome::Acquired => {}
            AcquireOutcome::Held(l) => {
                return Err(eyre::eyre!(
                    "Indexing already in progress for repo '{}' (worker {}, phase {})",
                    repo_id,
                    l.worker_id,
                    l.phase.as_str()
                ));
            }
        }

        // Mirror the lease into the in-process set so the synchronous
        // `is_indexing_repo` check is correct for status endpoints.
        self.indexing_repos.insert(repo_id.clone());

        let lease = LeaseContext::new(
            self.graph.clone(),
            repo_id.clone(),
            self.worker_id.clone(),
            initial_phase,
        );
        // `_heartbeat` is a `HeartbeatGuard`: its `Drop` aborts the task,
        // so a panic or cancellation between here and the explicit
        // release below still stops heartbeats. The lease then ages out
        // of freshness and another worker can steal it cleanly.
        let _heartbeat = lease.spawn_heartbeat();

        let pipeline = self.build_pipeline_for(root);
        let opts = RunOptions::default().with_lease(Arc::clone(&lease));
        let outcome = tokio::time::timeout(self.index_timeout, run(&pipeline, opts)).await;

        self.indexing_repos.remove(&repo_id);
        if let Err(e) = self.graph.release_lease(&repo_id, &self.worker_id).await {
            warn!(repo_id = %repo_id, error = %e, "Lease release failed");
        }

        match outcome {
            Ok(Ok(r)) => {
                info!(
                    root = %root.display(),
                    "Indexing run complete: {} files, {} entities, {} relationships, {} summaries in {:.1}s",
                    r.files_indexed,
                    r.entities_created,
                    r.relationships_created,
                    r.summaries_generated,
                    r.duration_ms as f64 / 1000.0
                );
                Ok(r)
            }
            Ok(Err(e)) => {
                warn!(root = %root.display(), "Indexing failed: {}", e);
                Err(e)
            }
            Err(_) => Err(eyre::eyre!(
                "Indexing timed out after {} minutes for repo '{}'",
                self.index_timeout.as_secs() / 60,
                repo_id
            )),
        }
    }

    /// Compute a fresh [`IndexCompleteness`] snapshot for a repo. The
    /// fields are derived from the current data — no flags, so no risk
    /// of staleness lying about completion. `head_commit` should be the
    /// repo's current `git HEAD`; pass `None` for non-git workspaces.
    pub async fn completeness(
        &self,
        repo_id: &str,
        head_commit: Option<&str>,
    ) -> Result<IndexCompleteness> {
        let state = self.graph.get_index_state(repo_id).await?;
        let stats = self.graph.get_stats(repo_id).await?;
        let summaries_expected = self.graph.count_summarisable_entities(repo_id).await?;
        let summaries_present = self.graph.count_summaries_present(repo_id).await?;
        let summaries_stale = self.graph.count_stale_summaries(repo_id).await? as u64;
        let vectors = match self.vector_store.as_ref() {
            Some(vs) => vs.entity_count(repo_id).await.unwrap_or(0) as u64,
            None => stats.entity_count as u64,
        };
        let lease = self.graph.get_lease(repo_id).await?;
        Ok(IndexCompleteness {
            repo_id: repo_id.to_string(),
            head_commit: head_commit.map(str::to_string),
            indexed_commit: state.as_ref().and_then(|s| s.last_commit.clone()),
            last_full_index_at: state.as_ref().and_then(|s| s.last_full_index_at.clone()),
            last_incremental_at: state.as_ref().and_then(|s| s.last_incremental_at.clone()),
            entities: stats.entity_count as u64,
            vectors,
            summaries_expected,
            summaries_present,
            summaries_stale,
            generate_summaries: self.generate_summaries,
            lease,
        })
    }

    /// Trigger an incremental index of the default workspace.
    pub async fn trigger_incremental_index(&self) -> Result<IndexResult> {
        self.trigger_incremental_index_for(&self.workspace.clone())
            .await
    }

    /// Index every repo in a project, writing them into this service's shared
    /// [`CodeGraph`]. Repos are indexed sequentially; per-repo failures are
    /// captured in the report rather than aborting the whole run. Pass
    /// `incremental = true` to only reprocess files whose content has
    /// changed since the previous index — much cheaper for routine refreshes.
    ///
    /// `data_dir` is used to resolve mirror paths for repos declared with
    /// `source` (rather than a legacy explicit `path`).
    pub async fn index_project(
        &self,
        project: &ProjectConfig,
        data_dir: &Path,
        incremental: bool,
    ) -> ProjectIndexReport {
        let mut repos = Vec::with_capacity(project.repos.len());
        for repo in &project.repos {
            let path = repo.effective_path(project, data_dir);
            let outcome = if incremental {
                self.trigger_incremental_index_for(&path).await
            } else {
                self.trigger_full_index_for(&path).await
            };
            repos.push(RepoIndexReport {
                name: repo.name.clone(),
                repo_id: Self::repo_id_for(&path),
                path,
                outcome,
            });
        }
        ProjectIndexReport {
            project_name: project.name.clone(),
            repos,
        }
    }

    /// Check if a specific repo is currently being indexed.
    pub fn is_indexing_repo(&self, repo_id: &str) -> bool {
        self.indexing_repos.contains(repo_id)
    }

    /// Check if any repo is currently being indexed.
    pub fn is_indexing_any(&self) -> bool {
        !self.indexing_repos.is_empty()
    }

    /// Get the current HEAD commit SHA for a given git repository path.
    pub fn get_head_commit_for(&self, path: &Path) -> Result<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg("HEAD")
            .output()?;

        if !output.status.success() {
            return Err(eyre::eyre!(
                "Failed to get HEAD commit for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Release this hub's hold on a repo's indexing lease and drop it
    /// from the in-process active set, allowing another worker (or our
    /// own retry) to acquire immediately. Returns `true` if we actually
    /// held it. The background task may keep running for a beat until
    /// its next phase boundary — its heartbeats stop refreshing the
    /// lease (the row is gone), so a steal-time-out backstop is no
    /// longer needed.
    pub async fn cancel_indexing(&self, repo_id: &str) -> bool {
        let in_set = self.indexing_repos.remove(repo_id).is_some();
        if let Err(e) = self.graph.release_lease(repo_id, &self.worker_id).await {
            warn!(repo_id, error = %e, "Lease release during cancel failed");
        }
        in_set
    }
}

/// Build a `CodeGraph` using the backend selected by `[codegraph] backend`.
/// Fails closed: a `"postgres"` backend that can't be used returns an error
/// rather than silently downgrading to SQLite.
fn build_code_graph(
    db_path: &Path,
    codegraph_config: &CodegraphConfig,
    postgres_config: Option<&PostgresConfig>,
) -> Result<CodeGraph> {
    if codegraph_config.backend == "postgres" {
        #[cfg(feature = "postgres")]
        {
            let pg_cfg = postgres_config.filter(|p| p.enabled).ok_or_else(|| {
                eyre::eyre!(
                    "[codegraph] backend = \"postgres\" but [postgres] is not enabled; refusing to \
                     fall back to SQLite. Enable [postgres] or set [codegraph] backend = \"sqlite\"."
                )
            })?;
            info!("Using Postgres code graph backend");
            // Synchronous surface; block on the async constructor
            // via the multi-threaded tokio runtime that the hub
            // always uses.
            let pg_cfg = pg_cfg.clone();
            let backend = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    workforce_codebase_knowledge::graph::PostgresCodeGraphBackend::new(&pg_cfg)
                        .await
                })
            })?;
            return Ok(CodeGraph::from_backend(backend));
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = postgres_config;
            eyre::bail!(
                "[codegraph] backend = \"postgres\" but workforce-knowledge-service was built \
                 without the postgres feature; rebuild with --features postgres or set [codegraph] \
                 backend = \"sqlite\""
            );
        }
    }
    info!(path = %db_path.display(), "Using SQLite code graph backend");
    CodeGraph::open(db_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Code vectors are kept where `[knowledge] vector_backend` says,
    /// wherever memories are.
    #[test]
    fn code_vectors_are_kept_where_their_own_setting_says() {
        let mut config = WorkforceConfig::default();
        assert!(matches!(
            VectorBackend::of(&config),
            VectorBackend::Weaviate(_)
        ));
        config.memory.backend = "vellev".into();
        assert!(
            matches!(VectorBackend::of(&config), VectorBackend::Weaviate(_)),
            "memories moved to vellev, code vectors stay"
        );
        config.memory.backend = "weaviate".into();
        config.knowledge.vector_backend = "vellev".into();
        assert!(matches!(
            VectorBackend::of(&config),
            VectorBackend::Vellev(_)
        ));
    }

    /// Weaviate's setup failure is logged in the words operators know.
    #[tokio::test]
    async fn a_weaviate_collection_setup_failure_is_logged_as_before() {
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let logging = tracing::subscriber::set_default(subscriber);
        let weaviate = WeaviateConfig {
            enabled: true,
            url: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        VectorBackend::Weaviate(&weaviate).store().unwrap();
        let expected = "Weaviate collection setup failed: ";
        let logged_so_far = || String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        for _ in 0..250 {
            if logged_so_far().contains(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(logging);
        let logged = logged_so_far();
        assert!(logged.contains(expected), "{logged}");
    }

    #[tokio::test]
    async fn a_vellev_store_that_cannot_be_made_leaves_the_knowledge_service_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let keyless = VellevConfig::default();
        let service = KnowledgeService::new_with_backend(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            &KnowledgeConfig::default(),
            VectorBackend::Vellev(&keyless),
            &CodegraphConfig::default(),
            None,
            None,
        )
        .expect("the code graph serves without vectors");
        assert!(service.vector_store().is_none());
    }

    #[tokio::test]
    async fn a_store_is_made_for_enabled_weaviate_and_for_vellev_with_a_key() {
        let mut config = WorkforceConfig::default();
        assert!(VectorBackend::of(&config).store().unwrap().is_none());
        config.weaviate.enabled = true;
        assert!(VectorBackend::of(&config).store().unwrap().is_some());
        config.knowledge.vector_backend = "vellev".into();
        config.vellev.url = "http://127.0.0.1:1".into();
        assert!(
            VectorBackend::of(&config).store().is_err(),
            "a key is needed"
        );
        config.vellev.api_key = Some("key".into());
        assert!(
            VectorBackend::of(&config).store().is_err(),
            "a tenant is needed"
        );
        config.vellev.tenant = "workforce".into();
        let store = VectorBackend::of(&config).store().unwrap().unwrap();
        assert_eq!(store.base_url(), "http://127.0.0.1:1");
    }

    /// A small repo in `dir`, and a service indexing it into the code
    /// vectors of `vellev`'s tenant, filling as fast as the node takes it.
    fn repo_and_service(dir: &Path, vellev: &VellevConfig) -> (PathBuf, KnowledgeService) {
        let root = dir.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/config.rs"),
            "/// Reads the TOML configuration file.\npub fn parse_config(path: &str) -> String {\n    path.to_string()\n}\n\n/// Sends a message over SMTP.\npub fn send_email(to: &str) -> bool {\n    !to.is_empty()\n}\n",
        )
        .unwrap();
        let config = KnowledgeConfig {
            vector_backfill_per_sec: 0,
            ..KnowledgeConfig::default()
        };
        let service = KnowledgeService::new_with_backend(
            root.clone(),
            dir.join("data"),
            &config,
            VectorBackend::Vellev(vellev),
            &CodegraphConfig::default(),
            None,
            None,
        )
        .unwrap();
        (root, service)
    }

    /// A store like Weaviate's, keeping no track of its vectors'
    /// completeness, that records what is put right in it, file by file.
    #[derive(Default)]
    struct Untracked {
        dropped: std::sync::Mutex<Vec<String>>,
        indexed: std::sync::atomic::AtomicUsize,
        /// Said to be another database than Weaviate.
        named: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl CodeVectorStore for Untracked {
        fn backend(&self) -> &'static str {
            self.named.unwrap_or(VectorKeys::WEAVIATE)
        }
        async fn ensure_collections(&self) -> Result<()> {
            Ok(())
        }
        async fn health_check(&self) -> Result<bool> {
            Ok(true)
        }
        fn base_url(&self) -> &str {
            "http://weaviate"
        }
        fn set_insert_error_sink(
            &self,
            _: Arc<dyn workforce_codebase_knowledge::vector::health::InsertErrorSink>,
        ) {
        }
        async fn index_entities(
            &self,
            entities: &[workforce_codebase_knowledge::vector::VectorCodeEntity],
        ) -> Result<usize> {
            self.indexed
                .fetch_add(entities.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(entities.len())
        }
        async fn search_entities(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<workforce_codebase_knowledge::graph::entities::EntityKind>,
            _: usize,
        ) -> Result<Vec<workforce_codebase_knowledge::vector::VectorSearchResult>> {
            Ok(Vec::new())
        }
        async fn delete_repo_entities(
            &self,
            _: &str,
        ) -> Result<workforce_codebase_knowledge::vector::DeleteOutcome> {
            Ok(Default::default())
        }
        async fn delete_entities_by_file(
            &self,
            _: &str,
            file: &str,
        ) -> Result<workforce_codebase_knowledge::vector::DeleteOutcome> {
            self.dropped.lock().unwrap().push(file.to_string());
            Ok(Default::default())
        }
        async fn index_summaries(
            &self,
            summaries: &[workforce_codebase_knowledge::vector::VectorCodeSummary],
        ) -> Result<usize> {
            Ok(summaries.len())
        }
        async fn delete_repo_summaries(
            &self,
            _: &str,
        ) -> Result<workforce_codebase_knowledge::vector::DeleteOutcome> {
            Ok(Default::default())
        }
        async fn delete_summaries_by_file(
            &self,
            _: &str,
            _: &str,
        ) -> Result<workforce_codebase_knowledge::vector::DeleteOutcome> {
            Ok(Default::default())
        }
        async fn entity_count(&self, _: &str) -> Result<usize> {
            Ok(0)
        }
    }

    /// A repo of one file, on a service keeping its code vectors in
    /// `store`, with `standby` as `[knowledge] vector_standby`.
    fn service_on(
        dir: &Path,
        store: Arc<dyn CodeVectorStore>,
        standby: &str,
    ) -> Result<(PathBuf, KnowledgeService)> {
        let root = dir.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/config.rs"),
            "/// Reads the configuration.\npub fn parse_config(path: &str) -> String {\n    path.to_string()\n}\n\n/// Sends a message.\npub fn send_email(to: &str) -> bool {\n    !to.is_empty()\n}\n",
        )
        .unwrap();
        let config = KnowledgeConfig {
            vector_backfill_per_sec: 0,
            vector_standby: standby.into(),
            ..KnowledgeConfig::default()
        };
        let service = KnowledgeService::new_with_backend(
            root.clone(),
            dir.join("data"),
            &config,
            VectorBackend::Given(store),
            &CodegraphConfig::default(),
            None,
            None,
        )?;
        Ok((root, service))
    }

    /// The graph keeps its vector state as the database in use's, with the
    /// standby named in `[knowledge] vector_standby`; one that is no
    /// database, or the one in use, is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_graph_keeps_vector_state_for_the_database_in_use_and_its_standby() {
        let dir = tempfile::tempdir().unwrap();
        let (_, service) =
            service_on(dir.path(), Arc::new(Untracked::default()), "vellev").unwrap();
        let keys = service.graph().vector_keys();
        assert_eq!(
            (keys.active(), keys.standby()),
            ("weaviate", Some("vellev"))
        );
        let (_, service) = service_on(dir.path(), Arc::new(Untracked::default()), " ").unwrap();
        assert_eq!(service.graph().vector_keys().standby(), None);
        for refused in ["weaviate", "qdrant"] {
            let made = service_on(dir.path(), Arc::new(Untracked::default()), refused);
            let error = format!("{:#}", made.err().expect("refused"));
            assert!(error.contains("vector_standby"), "{error}");
        }
    }

    /// A given store's database is the one it names, not one told from how
    /// it behaves: a store keeping no track that says it is vellev's is
    /// keyed as vellev's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_given_store_is_keyed_as_the_database_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let named = Untracked {
            named: Some(VectorKeys::VELLEV),
            ..Untracked::default()
        };
        assert!(!named.tracks_completeness());
        let (_, service) = service_on(dir.path(), Arc::new(named), "weaviate").unwrap();
        let keys = service.graph().vector_keys();
        assert_eq!(
            (keys.active(), keys.standby()),
            ("vellev", Some("weaviate"))
        );
    }

    /// With Weaviate in use again, the files noted for it while it was the
    /// standby are put right in it, file by file, by the backfill, and
    /// taken off; what is noted for vellev, now the standby, is left.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_files_noted_for_a_store_that_keeps_no_track_are_put_right_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Untracked::default());
        let (root, service) = service_on(
            dir.path(),
            Arc::clone(&store) as Arc<dyn CodeVectorStore>,
            "vellev",
        )
        .unwrap();
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        // As the switch back finds them: noted for Weaviate while vellev
        // was in use, and for vellev since.
        let files = ["src/config.rs".to_string()];
        service
            .graph()
            .note_vectors_pending_in(&["weaviate", "vellev"], &repo, &files)
            .await
            .unwrap();
        store.dropped.lock().unwrap().clear();
        let before = store.indexed.load(std::sync::atomic::Ordering::SeqCst);

        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert!(pass.failed.is_empty());
        assert_eq!(*store.dropped.lock().unwrap(), files);
        let entities = service
            .graph()
            .find_entities_in_file(&repo, "src/config.rs")
            .await
            .unwrap()
            .len();
        assert!(entities >= 2);
        assert_eq!(
            store.indexed.load(std::sync::atomic::Ordering::SeqCst) - before,
            entities
        );
        assert_eq!(
            service.graph().count_vectors_pending(&repo).await.unwrap(),
            0
        );
        assert_eq!(
            service
                .graph()
                .count_vectors_pending_in("vellev", &repo)
                .await
                .unwrap(),
            1,
            "the standby's are left"
        );
        // Nothing noted: nothing done.
        let pass = service.backfill_vectors().await.unwrap();
        assert!(pass.filled.is_empty() && pass.failed.is_empty());
    }

    /// With a node that does not answer, a full index still indexes the
    /// repo, a pass fails and says why, and the repo is not taken to have
    /// vectors.
    #[tokio::test]
    async fn a_repo_whose_vectors_cannot_be_stored_is_indexed_and_reported_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let unreachable = VellevConfig {
            url: "http://127.0.0.1:1".into(),
            api_key: Some("key".into()),
            tenant: "workforce".into(),
            ..VellevConfig::default()
        };
        let (root, service) = repo_and_service(dir.path(), &unreachable);
        assert!(service.vector_store().is_some());
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        assert!(service
            .graph()
            .get_index_state(&repo)
            .await
            .unwrap()
            .is_some());
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.failed, std::slice::from_ref(&repo));
        let status = service.vector_status().await.unwrap();
        assert_eq!(status.len(), 1);
        assert!(!status[0].complete);
        assert!(status[0]
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("vellev")));
    }

    const QUESTION: &str = "how is the configuration read";

    /// Whether a question of `repo` is scored with its vectors.
    async fn scored_by_vectors(service: &KnowledgeService, repo: &str) -> bool {
        let plan = workforce_codebase_knowledge::query::natural_language::QueryPlanner::plan(
            QUESTION, false,
        );
        service
            .query_engine()
            .resolve_seed_entities_multi(&[repo.to_string()], QUESTION, &plan)
            .await
            .unwrap()
            .vector_configured
    }

    /// Whether the graph records `repo`'s vectors complete in the
    /// collection the store has now.
    async fn complete(service: &KnowledgeService, repo: &str) -> bool {
        let vectors = service.vector_store().clone().unwrap();
        let held_in = vectors.identity(true).await.unwrap();
        service.graph().vectors_complete_in(repo).await.unwrap() == Some(held_in)
    }

    fn on(node: &workforce_vellev::testing::Node) -> VellevConfig {
        VellevConfig {
            url: node.url.clone(),
            api_key: Some(node.key.clone()),
            tenant: node.tenant.clone(),
            ..VellevConfig::default()
        }
    }

    /// A repo indexed on `node`, its vectors filled.
    async fn indexed_on(
        node: &workforce_vellev::testing::Node,
        dir: &Path,
    ) -> (String, KnowledgeService) {
        let (root, service) = repo_and_service(dir, &on(node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        assert!(
            !complete(&service, &repo).await,
            "the index run embeds nothing"
        );
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert!(complete(&service, &repo).await);
        (repo, service)
    }

    /// A full index leaves the repo's vectors to a fill from the graph,
    /// which stores them on the node page by page; until it ends the repo
    /// is not scored by vectors, and after it the store holds exactly what
    /// the graph says it should.
    #[tokio::test]
    async fn a_repos_vectors_are_filled_from_the_graph_after_it_is_indexed() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let vectors = service.vector_store().clone().unwrap();
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), 0);
        assert!(
            !scored_by_vectors(&service, &repo).await,
            "not while incomplete"
        );
        let before = service.vector_status().await.unwrap();
        assert!(!before[0].complete && before[0].fill.is_none());

        let pages = std::sync::Mutex::new(Vec::new());
        let pace = FillPace {
            per_sec: 0,
            page: 2,
            ..FillPace::default()
        };
        let pass = service
            .fill_vectors(None, pace, &|repo, fill| {
                pages
                    .lock()
                    .unwrap()
                    .push((repo.to_string(), fill.stage, fill.done))
            })
            .await
            .unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert!(pass.failed.is_empty());
        let pages = pages.into_inner().unwrap();
        assert!(pages.len() > 3, "page by page: {pages:?}");
        assert_eq!(pages.last().unwrap().1, FillStage::Done);

        let filler = service.filler(pace).unwrap();
        let confirmed = filler.confirm(&repo).await.unwrap();
        assert!(
            confirmed.agrees() && confirmed.entities_stored > 1,
            "{confirmed:?}"
        );
        assert!(complete(&service, &repo).await);
        assert!(scored_by_vectors(&service, &repo).await);
        let after = service.vector_status().await.unwrap();
        assert!(after[0].complete && after[0].pending_files == 0);
        assert_eq!(after[0].fill.as_ref().unwrap().stage, FillStage::Done);
        assert!(
            service.backfill_vectors().await.unwrap().filled.is_empty(),
            "nothing left to fill"
        );

        // What the node holds of the repo from before a fill, and the
        // graph no longer has, goes when the fill ends.
        let held = service.graph().all_entities(&repo).await.unwrap();
        let mut stale = workforce_codebase_knowledge::vector::VectorCodeEntity::from_code_entity(
            &held[0], &repo, None,
        );
        stale.entity_id = "gone-long-ago".into();
        vectors.index_entities(&[stale]).await.unwrap();
        assert_eq!(
            vectors.entity_count(&repo).await.unwrap(),
            confirmed.entities_stored + 1
        );
        service
            .graph()
            .set_vectors_complete_in(&repo, None)
            .await
            .unwrap();
        service.backfill_vectors().await.unwrap();
        assert!(filler.confirm(&repo).await.unwrap().agrees());
    }

    /// The collection dropped behind the hub's back, as one a restored
    /// backup left out is: what the graph records as complete was complete
    /// in a collection that is no more. The next search finds it so and is
    /// not scored by vectors, and the next pass fills the new collection.
    #[tokio::test]
    async fn a_code_collection_lost_behind_the_hub_is_found_out_and_filled_again() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let vectors = service.vector_store().clone().unwrap();
        let indexed = vectors.entity_count(&repo).await.unwrap();
        assert!(indexed > 0 && scored_by_vectors(&service, &repo).await);
        let recorded = service.graph().vectors_complete_in(&repo).await.unwrap();

        for lost in ["CodeEntity", "CodeSummary"] {
            let name = vellev_core::CollectionName::new(lost).unwrap();
            node.tenant().collection(&name).drop().await.unwrap();
            if lost == "CodeEntity" {
                assert!(
                    !scored_by_vectors(&service, &repo).await,
                    "the search that finds the collection gone is not scored by it"
                );
                assert_eq!(
                    service.graph().vectors_complete_in(&repo).await.unwrap(),
                    recorded,
                    "what the graph records is of the collection that was"
                );
            }
            assert!(!complete(&service, &repo).await, "{lost} lost");
            let pass = service.backfill_vectors().await.unwrap();
            assert_eq!(pass.filled, std::slice::from_ref(&repo), "{lost} lost");
            assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
            assert!(complete(&service, &repo).await);
            assert!(scored_by_vectors(&service, &repo).await);
        }
    }

    /// The background task fills a collection lost behind the hub without
    /// waiting out its watch: the question that finds it lost wakes it.
    #[tokio::test]
    async fn a_question_that_finds_the_code_collection_lost_wakes_the_backfill() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let service = Arc::new(service);
        service.spawn_vector_backfill_paced(Duration::from_millis(50), Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let entities = vellev_core::CollectionName::new("CodeEntity").unwrap();
        node.tenant().collection(&entities).drop().await.unwrap();
        assert!(!scored_by_vectors(&service, &repo).await);
        let begun = std::time::Instant::now();
        while !complete(&service, &repo).await {
            assert!(
                begun.elapsed() < Duration::from_secs(30),
                "not filled again"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(scored_by_vectors(&service, &repo).await);
    }

    /// A collection lost and partly filled again by another hub, or
    /// emptied with the collection kept, which this hub has not looked at
    /// since: the pass looks afresh, and fills it.
    #[tokio::test]
    async fn a_pass_finds_a_code_collection_made_again_or_emptied() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let vectors = service.vector_store().clone().unwrap();
        let indexed = vectors.entity_count(&repo).await.unwrap();
        assert!(indexed > 1);

        let entities = vellev_core::CollectionName::new("CodeEntity").unwrap();
        node.tenant().collection(&entities).drop().await.unwrap();
        let other = workforce_codebase_knowledge::vector::VellevCodeVectorStore::new(
            &node.url,
            &node.key,
            &node.tenant,
        )
        .unwrap();
        let held = service.graph().all_entities(&repo).await.unwrap();
        let one = workforce_codebase_knowledge::vector::VectorCodeEntity::from_code_entity(
            &held[0], &repo, None,
        );
        assert_eq!(other.index_entities(&[one]).await.unwrap(), 1);

        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo), "made again");
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        assert!(complete(&service, &repo).await);

        vectors.delete_repo_entities(&repo).await.unwrap();
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo), "emptied");
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
    }

    /// A page that takes longer than it may is given up, and the fill
    /// keeps its place: each page is bounded, the whole fill is not.
    #[tokio::test]
    async fn a_page_that_takes_too_long_is_given_up_and_the_fill_keeps_its_place() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let hasty = FillPace {
            per_sec: 0,
            page: 1,
            page_timeout: Duration::from_nanos(1),
            ahead: 1,
        };
        let pass = service.fill_vectors(None, hasty, &|_, _| {}).await.unwrap();
        assert_eq!(pass.failed, std::slice::from_ref(&repo));
        let status = service.vector_status().await.unwrap();
        assert!(status[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("was not stored within"));

        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert!(complete(&service, &repo).await);
        assert_eq!(service.vector_status().await.unwrap()[0].last_error, None);
    }

    /// A fill is paced: at a rate it stores no more a second than it may.
    #[tokio::test]
    async fn a_fill_stores_no_faster_than_its_pace() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let stored = service.graph().all_entities(&repo).await.unwrap().len();
        let pace = FillPace {
            per_sec: 4,
            page: 1,
            ..FillPace::default()
        };
        let began = std::time::Instant::now();
        service.fill_vectors(None, pace, &|_, _| {}).await.unwrap();
        assert!(complete(&service, &repo).await);
        let least = Duration::from_secs_f64(stored as f64 / 4.0);
        assert!(
            began.elapsed() >= least.mul_f64(0.9),
            "{stored} stored in {:?}, at 4 a second",
            began.elapsed()
        );
    }

    /// While a repo is being indexed its vectors are not filled: the pass
    /// waits for the run, which changes the graph.
    #[tokio::test]
    async fn a_repo_being_indexed_is_filled_after() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let held = service
            .graph()
            .acquire_lease(&repo, "another-hub", None, IndexPhase::Parsing)
            .await
            .unwrap();
        assert!(matches!(held, AcquireOutcome::Acquired));
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.failed, std::slice::from_ref(&repo));
        assert!(!complete(&service, &repo).await);
        service
            .graph()
            .release_lease(&repo, "another-hub")
            .await
            .unwrap();
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
    }

    /// Status reports read what the last pass saw and ask nothing of the
    /// node or the graph: what changes behind the hub shows in them only
    /// once a pass has looked.
    #[tokio::test]
    async fn status_is_what_the_last_pass_saw() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        assert!(service.vector_status_seen().is_empty(), "before any pass");
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        service.backfill_vectors().await.unwrap();
        let seen = service.vector_status_seen();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].complete && seen[0].repo_id == repo);
        assert_eq!(seen[0].fill.as_ref().unwrap().stage, FillStage::Done);

        service
            .graph()
            .set_vectors_complete_in(&repo, None)
            .await
            .unwrap();
        assert!(
            !service.vector_status().await.unwrap()[0].complete,
            "asked now"
        );
        assert_eq!(service.vector_status_seen(), seen, "not asked");
        service.backfill_vectors().await.unwrap();
        let again = service.vector_status_seen();
        assert!(again[0].complete);
        assert_ne!(
            again[0].fill.as_ref().unwrap().generation,
            seen[0].fill.as_ref().unwrap().generation,
            "seen again by the pass that filled it"
        );
    }

    /// One round of a fill runs at a time in a process, however many services
    /// it has: they share the node.
    #[tokio::test]
    async fn one_round_of_a_fill_runs_at_a_time_in_a_process() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let service = Arc::new(service);
        let turn = FILL_TURN.lock().await;
        let pass = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.backfill_vectors().await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!pass.is_finished(), "waits its turn");
        drop(turn);
        let pass = tokio::time::timeout(Duration::from_secs(120), pass).await;
        let pass = pass.expect("its turn comes").unwrap().unwrap();
        assert_eq!(pass.filled.len(), 1);
    }

    /// A repo of `functions` functions under `dir`/`name`, indexed by
    /// `service`; its id.
    async fn another_repo(
        service: &KnowledgeService,
        dir: &Path,
        name: &str,
        functions: usize,
    ) -> String {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source: String = (0..functions)
            .map(|n| {
                format!(
                    "/// Does {name} thing {n}.\npub fn {name}_{n}() -> usize {{\n    {n}\n}}\n\n"
                )
            })
            .collect();
        std::fs::write(root.join("src/lib.rs"), source).unwrap();
        service.trigger_full_index_for(&root).await.unwrap();
        KnowledgeService::repo_id_for(&root)
    }

    /// A store on `node`'s tenant that is no graph's: it writes and
    /// removes behind the service's back, as a restore or a fault would.
    fn behind(node: &workforce_vellev::testing::Node) -> VellevCodeVectorStore {
        VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap()
    }

    fn vector_of(
        service_entity: &workforce_codebase_knowledge::graph::entities::CodeEntity,
        repo: &str,
    ) -> workforce_codebase_knowledge::vector::VectorCodeEntity {
        workforce_codebase_knowledge::vector::VectorCodeEntity::from_code_entity(
            service_entity,
            repo,
            None,
        )
    }

    /// An index run that stored what it indexed is no reason to compare a
    /// repo with the graph; a store that gained or lost otherwise is.
    #[test]
    fn a_repo_is_compared_when_its_store_has_not_kept_pace_with_the_graph() {
        let was = Counts {
            stored_entities: 100,
            stored_summaries: 40,
            entities: 120,
            summaries: 40,
        };
        let now = |stored_entities, stored_summaries, entities, summaries| Counts {
            stored_entities,
            stored_summaries,
            entities,
            summaries,
        };
        assert!(was.kept_pace(&was));
        assert!(was.kept_pace(&now(103, 42, 123, 42)), "indexed and stored");
        assert!(was.kept_pace(&now(90, 40, 110, 40)), "removed from both");
        assert!(!was.kept_pace(&now(99, 40, 120, 40)), "a vector lost");
        assert!(!was.kept_pace(&now(0, 0, 120, 40)), "emptied");
        assert!(!was.kept_pace(&now(101, 40, 120, 40)), "one left behind");
        assert!(
            !was.kept_pace(&now(100, 40, 121, 40)),
            "indexed, not stored"
        );
        assert!(!was.kept_pace(&now(100, 39, 120, 40)), "a summary lost");
    }

    /// A question that finds none of a complete repo's vectors has the
    /// repo compared by the next pass, though as many are held as before
    /// and its time to be compared has not come; not if it was compared
    /// within the last minute, since a repo with nothing to embed is found
    /// empty by every question.
    #[tokio::test]
    async fn a_repo_a_question_found_empty_is_compared_by_the_next_pass() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let held = service.graph().all_entities(&repo).await.unwrap();
        let changed = held.iter().find(|e| e.name == "send_email").unwrap();
        // As many held as before, one of them of another text.
        let mut earlier = vector_of(changed, &repo);
        earlier.searchable_content = "Posts a letter by pigeon.".into();
        behind(&node).index_entities(&[earlier]).await.unwrap();
        let suspect = || {
            let suspected = service.query_engine.vectors_suspected();
            suspected.lock().unwrap().insert(repo.clone());
        };

        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
        suspect();
        let pass = service.backfill_vectors().await.unwrap();
        assert!(pass.filled.is_empty(), "compared a moment ago: not again");
        // Compared more than a minute ago.
        suspect();
        service.compared.get_mut(&repo).unwrap().at -= SUSPECT_EVERY + Duration::from_secs(1);
        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(
            pass.filled,
            std::slice::from_ref(&repo),
            "compared, and put right"
        );
        let found = behind(&node)
            .search_entities("posts a letter by pigeon", Some(&repo), None, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), held.len(), "{found:?}");
    }

    /// A vector lost from a repo recorded complete is noticed by the next
    /// pass, since the store then holds fewer than it did, and put back:
    /// by storing its file again, not the repo.
    #[tokio::test]
    async fn a_vector_lost_from_a_complete_repo_is_put_back() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let vectors = service.vector_store().clone().unwrap();
        let indexed = vectors.entity_count(&repo).await.unwrap();
        let filled = service.graph().vector_fill(&repo).await.unwrap().unwrap();
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());

        let held = service.graph().all_entities(&repo).await.unwrap();
        let lost = held.iter().find(|e| e.name == "parse_config").unwrap();
        behind(&node)
            .remove_entities(&[vector_of(lost, &repo)])
            .await
            .unwrap();
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed - 1);

        let pass = service.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        assert!(complete(&service, &repo).await);
        let found = vectors
            .search_entities("reads the TOML configuration file", Some(&repo), None, 3)
            .await
            .unwrap();
        assert!(found.iter().any(|f| f.entity_id == lost.id.as_str()));
        assert_eq!(
            service.graph().vector_fill(&repo).await.unwrap().unwrap(),
            filled,
            "no fill of the repo: only the file was stored again"
        );
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
        let seen = service.vector_status_seen();
        assert!(seen[0].compared_secs_ago.is_some(), "{seen:?}");
    }

    /// A store put back to an earlier state holds as many vectors as
    /// before, some of them wrong: one lost, one of a text the graph no
    /// longer has, one of an entity it no longer has. Counting finds
    /// nothing. The comparison made when the hub starts does, as does the
    /// one made every `[knowledge] vector_reconcile_secs`, and only what
    /// differs is stored again or removed.
    #[tokio::test]
    async fn a_store_restored_to_an_earlier_state_is_found_out_and_put_right() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let vectors = service.vector_store().clone().unwrap();
        let indexed = vectors.entity_count(&repo).await.unwrap();
        let filled = service.graph().vector_fill(&repo).await.unwrap().unwrap();
        let held = service.graph().all_entities(&repo).await.unwrap();
        let lost = held.iter().find(|e| e.name == "parse_config").unwrap();
        let changed = held.iter().find(|e| e.name == "send_email").unwrap();

        let drift = || async {
            let behind = behind(&node);
            behind
                .remove_entities(&[vector_of(lost, &repo)])
                .await
                .unwrap();
            let mut earlier = vector_of(changed, &repo);
            earlier.searchable_content = "Posts a letter by pigeon.".into();
            let mut gone = vector_of(lost, &repo);
            gone.entity_id = "gone::long_ago".into();
            gone.qualified_name = "gone::long_ago".into();
            gone.file_path = "src/gone.rs".into();
            gone.searchable_content = "An entity removed since the backup.".into();
            assert_eq!(behind.index_entities(&[earlier, gone]).await.unwrap(), 2);
            assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        };
        let right = || async {
            assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
            let found = vectors
                .search_entities("posts a letter by pigeon", Some(&repo), None, 10)
                .await
                .unwrap();
            let ids: Vec<&str> = found.iter().map(|f| f.entity_id.as_str()).collect();
            assert!(ids.contains(&lost.id.as_str()), "put back: {ids:?}");
            assert!(!ids.contains(&"gone::long_ago"), "removed: {ids:?}");
            let confirmed = service.filler(FillPace::at(0)).unwrap();
            let mut at = Verifying::default();
            let mut steps = 0;
            let ended = loop {
                steps += 1;
                assert!(steps < 1000, "the comparison did not end");
                match confirmed.verify_step(&repo, &mut at).await.unwrap() {
                    Verified::Going => {}
                    ended => break ended,
                }
            };
            assert!(
                matches!(
                    ended,
                    Verified::Done {
                        differing: 0,
                        strays: 0,
                        ..
                    }
                ),
                "{ended:?}"
            );
            assert_eq!(
                service.graph().vector_fill(&repo).await.unwrap().unwrap(),
                filled,
                "no fill of the repo"
            );
        };

        // The same counts: this service, which has compared the repo
        // lately, sees nothing amiss.
        drift().await;
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());

        // The hub started again compares every repo first.
        let config = KnowledgeConfig {
            vector_backfill_per_sec: 0,
            vector_reconcile_secs: 1,
            ..KnowledgeConfig::default()
        };
        let restarted = KnowledgeService::new_with_backend(
            dir.path().join("repo"),
            dir.path().join("data"),
            &config,
            VectorBackend::Vellev(&on(&node)),
            &CodegraphConfig::default(),
            None,
            None,
        )
        .unwrap();
        let pass = restarted.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo), "at the start");
        right().await;
        assert!(restarted
            .backfill_vectors()
            .await
            .unwrap()
            .filled
            .is_empty());

        // And again when it has not for as long as it is told to let pass.
        drift().await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let pass = restarted.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo), "in its time");
        right().await;
    }

    /// At an operator's word a repo recorded complete is filled again from
    /// the graph: every vector written over, what the graph has not
    /// removed, and its vectors relied on all the while.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repo_is_filled_again_when_forced_and_searched_meanwhile() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, service) = indexed_on(&node, dir.path()).await;
        let vectors = service.vector_store().clone().unwrap();
        let indexed = vectors.entity_count(&repo).await.unwrap();
        let first = service.graph().vector_fill(&repo).await.unwrap().unwrap();
        // Another repo, filled, which the forced fill is not of: what is
        // swept at its end is this repo's alone.
        let other = another_repo(&service, dir.path(), "other", 5).await;
        service.backfill_vectors().await.unwrap();
        let others = vectors.entity_count(&other).await.unwrap();
        let others_fill = service.graph().vector_fill(&other).await.unwrap().unwrap();
        assert!(others >= 5);

        let relied_on = std::sync::Mutex::new(Vec::new());
        let run = FillRun {
            force: true,
            ..FillRun::at(FillPace {
                page: 1,
                ..FillPace::at(0)
            })
        };
        let pass = service
            .fill(Some(&repo), run, &|repo, _| {
                let graph = Arc::clone(service.graph());
                let complete = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(graph.vectors_complete_in(repo))
                });
                relied_on.lock().unwrap().push(complete.unwrap().is_some());
            })
            .await
            .unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        let relied_on = relied_on.into_inner().unwrap();
        assert!(relied_on.len() > indexed, "a page an entity: {relied_on:?}");
        assert!(relied_on.iter().all(|relied| *relied), "{relied_on:?}");
        let again = service.graph().vector_fill(&repo).await.unwrap().unwrap();
        assert_ne!(again.generation, first.generation);
        assert_eq!(again.stage, FillStage::Done);
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        assert_eq!(vectors.entity_count(&other).await.unwrap(), others);
        assert_eq!(
            service.graph().vector_fill(&other).await.unwrap().unwrap(),
            others_fill
        );
        assert!(complete(&service, &other).await);
        // Unforced, there is nothing to do.
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
    }

    /// A tenant's code vectors are one code graph's. A second graph is
    /// refused: it would take the first's vectors of a repo of the same id
    /// for stale ones of its own and remove them. Nothing of the first's
    /// is touched, and the second is told what to do; told to take them
    /// over, it has them, and the first is refused in its turn.
    #[tokio::test]
    async fn a_second_code_graph_is_refused_a_tenants_code_vectors() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (repo, first) = indexed_on(&node, dir.path()).await;
        let vectors = behind(&node);
        let indexed = vectors.entity_count(&repo).await.unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let (root, second) = repo_and_service(other_dir.path(), &on(&node));
        assert_ne!(first.graph().identity(), second.graph().identity());
        second.trigger_full_index_for(&root).await.unwrap();
        let others = KnowledgeService::repo_id_for(&root);
        let pass = second.backfill_vectors().await.unwrap();
        assert_eq!(pass.failed, std::slice::from_ref(&others));
        let why = second.vector_status().await.unwrap()[0]
            .last_error
            .clone()
            .unwrap();
        assert!(
            why.contains("another code graph") && why.contains("--take-over"),
            "{why}"
        );
        assert!(!scored_by_vectors(&second, &others).await);
        assert_eq!(vectors.entity_count(&others).await.unwrap(), 0);
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), indexed);
        assert!(first.backfill_vectors().await.unwrap().failed.is_empty());
        assert!(scored_by_vectors(&first, &repo).await);

        // The same graph opened again is the same graph.
        let (_, again) = repo_and_service(dir.path(), &on(&node));
        assert_eq!(again.graph().identity(), first.graph().identity());
        assert!(again.backfill_vectors().await.unwrap().failed.is_empty());

        let taking = second.vector_store().clone().unwrap();
        taking.take_over().await.unwrap();
        let pass = second.backfill_vectors().await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&others));
        assert!(scored_by_vectors(&second, &others).await);
    }

    /// A long fill holds up neither another repo's repairs nor another's
    /// fill: each repo has a page a round, those with files pending first,
    /// and what is noted or indexed after the pass began is seen by it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_fill_holds_up_no_other_repo() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (small, service) = indexed_on(&node, dir.path()).await;
        let long = another_repo(&service, dir.path(), "long", 24).await;
        let short = another_repo(&service, dir.path(), "short", 6).await;
        let file = vec!["src/config.rs".to_string()];
        service
            .graph()
            .note_vectors_pending(&small, &file)
            .await
            .unwrap();

        // What happened, in order: a page of a fill, or how many files the
        // small repo had pending when the long one's page was stored.
        let events = std::sync::Mutex::new(Vec::<(String, FillStage, usize)>::new());
        let run = FillRun {
            rescan: Duration::ZERO,
            ..FillRun::at(FillPace {
                page: 3,
                ..FillPace::at(0)
            })
        };
        // Bounded: a pass that waited on one repo would not end.
        let noting = |repo: &str, fill: &VectorFill| {
            let graph = Arc::clone(service.graph());
            let (small, file) = (small.clone(), file.clone());
            let mut events = events.lock().unwrap();
            let pages = events.iter().filter(|(of, _, _)| of == repo).count();
            let pending = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let pending = graph.count_vectors_pending(&small).await.unwrap();
                    // Noted while the pass runs, as an index run would.
                    if repo == long && pages == 2 {
                        graph.note_vectors_pending(&small, &file).await.unwrap();
                    }
                    pending
                })
            });
            events.push((repo.to_string(), fill.stage, pending));
        };
        let filling = service.fill(None, run, &noting);
        let pass = tokio::time::timeout(Duration::from_secs(120), filling)
            .await
            .expect("the pass ends")
            .unwrap();
        assert!(pass.failed.is_empty(), "{pass:?}");
        let events = events.into_inner().unwrap();
        let of = |repo: &str| -> Vec<usize> {
            let all = events.iter().enumerate();
            all.filter(|(_, (of, _, _))| of == repo)
                .map(|(at, _)| at)
                .collect()
        };
        let (longs, shorts) = (of(&long), of(&short));
        assert!(longs.len() >= 8 && shorts.len() >= 3, "{events:?}");
        // Page about: the short repo's fill is over while the long one's
        // has pages to go, whichever was listed first.
        assert!(
            shorts.last().unwrap() < &longs[longs.len() - 3],
            "{events:?}"
        );
        assert!(shorts[1] < longs[3] && longs[1] < shorts[3], "{events:?}");
        // The small repo's file was put right before any page was stored,
        assert_eq!(events[0].2, 0, "{events:?}");
        // and the one noted during the pass, within a round or two.
        let noted = longs[2];
        assert_eq!(events[noted + 4].2, 0, "{events:?}");
        assert!(events[noted + 4].0 == long || events[noted + 4].0 == short);
        for repo in [&small, &long, &short] {
            assert!(complete(&service, repo).await);
        }
        assert_eq!(
            service.graph().count_vectors_pending(&small).await.unwrap(),
            0
        );
    }

    /// An embedder that takes a while, and counts the requests it has had
    /// and how many it had at once.
    #[derive(Default)]
    struct Busy {
        now: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
        asked: std::sync::atomic::AtomicUsize,
        /// A request waits here for leave to be answered, if set: one
        /// permit answers one.
        gate: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        /// Texts whose requests do not wait there.
        free: std::sync::Mutex<Vec<String>>,
        /// Requests waiting there.
        held: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl workforce_vellev::seeding::DocumentEmbedder for Busy {
        fn name(&self) -> String {
            "a busy embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            use std::sync::atomic::Ordering;
            // Counted out when the request ends, or is given up.
            struct UnderWay<'a>(&'a std::sync::atomic::AtomicUsize);
            impl Drop for UnderWay<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            self.asked.fetch_add(1, Ordering::SeqCst);
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            let _under_way = UnderWay(&self.now);
            self.most.fetch_max(now, Ordering::SeqCst);
            let gate = self.gate.lock().unwrap().clone();
            let free = self.free.lock().unwrap().contains(&texts[0]);
            if let Some(gate) = gate.filter(|_| !free) {
                self.held.fetch_add(1, Ordering::SeqCst);
                let _held = UnderWay(&self.held);
                gate.acquire().await.expect("open").forget();
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mut vector = vec![0.0f32; 768];
            vector[5] = 1.0;
            Ok(texts.iter().map(|_| vector.clone()).collect())
        }
    }

    /// With vectors made outside the node, pages are stored several at a
    /// time, so the embedder has its requests from one end of a repo to
    /// the other: by the time a page has been written to the node, read
    /// again and recorded, the embedder is already at work on the next.
    /// A page at a time, it has had only that page's requests by then, and
    /// is left with none until the next page is read.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_outside_embedder_is_kept_busy_from_page_to_page() {
        use std::sync::atomic::Ordering;
        for ahead in [4usize, 1] {
            // A tenant each.
            let Some(node) = workforce_vellev::testing::Node::embedding().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let busy = Arc::new(Busy::default());
            let seeding = workforce_vellev::seeding::Seeding::new(busy.clone(), 2, 2);
            let seeded = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
                .unwrap()
                .seeded_by(seeding.clone());
            let service = KnowledgeService::new_with_backend(
                dir.path().join("repo"),
                dir.path().join("data"),
                &KnowledgeConfig::default(),
                VectorBackend::Given(Arc::new(seeded)),
                &CodegraphConfig::default(),
                None,
                None,
            )
            .unwrap();
            let repo = another_repo(&service, dir.path(), "long", 40).await;
            let run = FillRun::at(FillPace {
                page: seeding.page(),
                ahead,
                ..FillPace::at(0)
            });
            // The requests the embedder had had when each page was recorded.
            let had = std::sync::Mutex::new(Vec::new());
            let pass = service
                .fill(Some(&repo), run, &|_, fill| {
                    if fill.stage == FillStage::Entities {
                        had.lock().unwrap().push(busy.asked.load(Ordering::SeqCst));
                    }
                })
                .await
                .unwrap();
            assert_eq!(pass.filled, std::slice::from_ref(&repo));
            assert!(complete(&service, &repo).await);
            assert_eq!(
                busy.most.load(Ordering::SeqCst),
                2,
                "never more than allowed"
            );
            let had = had.into_inner().unwrap();
            assert!(had.len() >= 10, "{had:?}");
            // A page is two requests of two.
            match ahead {
                1 => assert_eq!(had[..3], [2, 4, 6], "a page's, and no more"),
                _ => assert!(
                    had[0] > 2 && had[1] > 4 && had[2] > 6,
                    "at work on the pages after: {had:?}"
                ),
            }
        }
    }

    /// An embedder that takes a while, and tells how many repos' texts it
    /// had at once at the most, and how many requests.
    #[derive(Default)]
    struct Telling {
        /// The repos whose requests are under way, one entry a request.
        under_way: std::sync::Mutex<Vec<String>>,
        most_repos: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl workforce_vellev::seeding::DocumentEmbedder for Telling {
        fn name(&self) -> String {
            "a telling embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            use std::sync::atomic::Ordering;
            let of = ["alpha", "beta", "gamma"]
                .into_iter()
                .find(|repo| texts[0].contains(repo))
                .unwrap_or_default()
                .to_string();
            {
                let mut under_way = self.under_way.lock().unwrap();
                under_way.push(of.clone());
                let mut repos = under_way.clone();
                repos.sort();
                repos.dedup();
                self.most_repos.fetch_max(repos.len(), Ordering::SeqCst);
                self.most.fetch_max(under_way.len(), Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
            {
                let mut under_way = self.under_way.lock().unwrap();
                let at = under_way.iter().position(|repo| *repo == of).unwrap();
                under_way.remove(at);
            }
            let mut vector = vec![0.0f32; 768];
            vector[9] = 1.0;
            Ok(texts.iter().map(|_| vector.clone()).collect())
        }
    }

    /// When an index run takes a repo while its fill has pages on their
    /// way to the store, the fill gives them up at its next turn and waits
    /// until none is running: nothing of it is still being sent when the
    /// run ends and begins the fill again. The fill keeps its place.
    #[tokio::test(flavor = "multi_thread")]
    async fn pages_on_their_way_are_given_up_when_an_index_run_begins() {
        use std::sync::atomic::Ordering;
        const PAGE: usize = 2;
        const AHEAD: usize = 4;
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let busy = Arc::new(Busy::default());
        // A page is one request, and the embedder is given more at once
        // than there are pages on their way: none waits for another.
        let seeding = workforce_vellev::seeding::Seeding::new(busy.clone(), PAGE, 2 * AHEAD);
        let seeded = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .seeded_by(seeding.clone());
        let service = KnowledgeService::new_with_backend(
            dir.path().join("repo"),
            dir.path().join("data"),
            &KnowledgeConfig::default(),
            VectorBackend::Given(Arc::new(seeded)),
            &CodegraphConfig::default(),
            None,
            None,
        )
        .unwrap();
        let repo = another_repo(&service, dir.path(), "long", 60).await;
        let run = FillRun::at(FillPace {
            page: PAGE,
            ahead: AHEAD,
            ..FillPace::at(0)
        });
        // The embedder answers the first page, and holds the request of
        // every other until it is given leave.
        let first = service.graph().entities_page(&repo, "", None, PAGE);
        let first = first.await.unwrap();
        assert_eq!(first.len(), PAGE);
        *busy.free.lock().unwrap() = first
            .iter()
            .map(|entity| vector_of(entity, &repo).searchable_content)
            .collect();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *busy.gate.lock().unwrap() = Some(Arc::clone(&gate));
        // An index run takes the repo as the first page is recorded, with
        // the three after it on their way: each has its request with the
        // embedder, unanswered.
        let taking = |repo: &str, fill: &VectorFill| {
            assert_eq!(
                fill.done as usize, PAGE,
                "no page after the first is stored"
            );
            let began = Instant::now();
            while busy.held.load(Ordering::SeqCst) < AHEAD - 1 {
                assert!(began.elapsed() < Duration::from_secs(20), "none on its way");
                std::thread::sleep(Duration::from_millis(5));
            }
            let graph = Arc::clone(service.graph());
            tokio::task::block_in_place(|| {
                let taken = graph.acquire_lease(repo, "another-hub", None, IndexPhase::Parsing);
                let taken = tokio::runtime::Handle::current().block_on(taken).unwrap();
                assert!(matches!(taken, AcquireOutcome::Acquired));
            });
        };
        let pass = service.fill(Some(&repo), run, &taking);
        let pass = tokio::time::timeout(Duration::from_secs(60), pass).await;
        let pass = pass.expect("the pass ends").unwrap();
        assert_eq!(pass.failed, std::slice::from_ref(&repo));
        // Nothing is with the embedder, which had three requests held:
        // they were given up, not waited out, for it has answered none.
        assert_eq!(busy.now.load(Ordering::SeqCst), 0, "none still running");
        assert_eq!(busy.held.load(Ordering::SeqCst), 0);
        assert_eq!(busy.asked.load(Ordering::SeqCst), AHEAD, "a request a page");
        // And nothing more is sent to it when it would answer.
        gate.add_permits(10_000);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(busy.asked.load(Ordering::SeqCst), AHEAD, "and none after");
        assert_eq!(busy.now.load(Ordering::SeqCst), 0);
        let kept = service.graph().vector_fill(&repo).await.unwrap().unwrap();
        assert_eq!(kept.done as usize, PAGE, "its place kept");

        service
            .graph()
            .release_lease(&repo, "another-hub")
            .await
            .unwrap();
        let pass = service.fill(Some(&repo), run, &|_, _| {}).await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert!(complete(&service, &repo).await);
    }

    /// A repo is compared with the graph when its fill ends, by the same
    /// pass: a vector of an earlier fill that landed over one of this
    /// fill's while it went, and was swept at its end as stale, is found
    /// missing and stored again, not left for the next comparison hours
    /// later.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repo_is_compared_when_its_fill_ends() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let held = service.graph().all_entities(&repo).await.unwrap();
        let overwritten = held.iter().find(|e| e.name == "parse_config").unwrap();
        // As the fill reaches its sweep, a write of an earlier fill lands.
        let late = |repo: &str, fill: &VectorFill| {
            if fill.stage != FillStage::Sweep {
                return;
            }
            let earlier = vector_of(overwritten, repo).of_generation("an-earlier-fill");
            let behind = behind(&node);
            tokio::task::block_in_place(|| {
                let landed = behind.index_entities(std::slice::from_ref(&earlier));
                tokio::runtime::Handle::current().block_on(landed).unwrap();
            });
        };
        let run = FillRun::at(FillPace::at(0));
        let pass = service.fill(None, run, &late).await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        let vectors = service.vector_store().clone().unwrap();
        let embeddable = held.len();
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), embeddable);
        let found = vectors
            .search_entities("reads the TOML configuration file", Some(&repo), None, 3)
            .await
            .unwrap();
        assert!(found.iter().any(|f| f.entity_id == overwritten.id.as_str()));
        assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
    }

    /// So too when nothing else would have the repo compared: it was
    /// compared a moment ago, and the write of an earlier fill lands over
    /// one of this fill's after its sweep, which changes no count. Only
    /// the comparison a fill ends with finds it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repo_is_compared_when_its_fill_ends_though_nothing_else_says_to() {
        let Some(node) = workforce_vellev::testing::Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (root, service) = repo_and_service(dir.path(), &on(&node));
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let run = FillRun::at(FillPace::at(0));
        // Filled and compared: no comparison is due for hours.
        let pass = service.fill(None, run, &|_, _| {}).await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        let vectors = service.vector_store().clone().unwrap();
        assert!(!service.compare_due(&vectors, &repo).await.unwrap());

        let held = service.graph().all_entities(&repo).await.unwrap();
        let overwritten = held.iter().find(|e| e.name == "parse_config").unwrap();
        let as_graph = vector_of(overwritten, &repo);
        let earlier = || {
            let mut earlier = as_graph.clone().of_generation("an-earlier-fill");
            earlier.searchable_content = "what it said before it was changed".to_string();
            earlier
        };
        let differs = || async {
            let page = std::slice::from_ref(&as_graph);
            let differing = vectors.differing_entities(&repo, page).await.unwrap();
            !differing.places.is_empty()
        };
        // Filled again at an operator's word. As the fill ends, its sweep
        // done, a write of the earlier fill lands.
        let late = |_: &str, fill: &VectorFill| {
            if fill.stage != FillStage::Done {
                return;
            }
            let (behind, earlier) = (behind(&node), earlier());
            tokio::task::block_in_place(|| {
                let landed = behind.index_entities(std::slice::from_ref(&earlier));
                tokio::runtime::Handle::current().block_on(landed).unwrap();
            });
        };
        let again = FillRun { force: true, ..run };
        let pass = service.fill(None, again, &late).await.unwrap();
        assert_eq!(pass.filled, std::slice::from_ref(&repo));
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), held.len());
        assert!(
            !differs().await,
            "found and put right by the pass that filled"
        );

        // Such a write is one a comparison finds, and nothing else does.
        let landed = behind(&node).index_entities(&[earlier()]).await.unwrap();
        assert_eq!(landed, 1);
        assert!(differs().await);
        assert_eq!(vectors.entity_count(&repo).await.unwrap(), held.len());
        assert!(!service.compare_due(&vectors, &repo).await.unwrap());
    }

    /// Asked to, a pass fills several repos at once: their pages are with
    /// the embedder together, which still has no more requests than it is
    /// allowed, whichever repos they are for. One repo at a time, only
    /// one's are.
    #[tokio::test(flavor = "multi_thread")]
    async fn several_repos_are_filled_at_once_within_one_allowance() {
        use std::sync::atomic::Ordering;
        for at_once in [3usize, 1] {
            let Some(node) = workforce_vellev::testing::Node::embedding().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let telling = Arc::new(Telling::default());
            let seeding = workforce_vellev::seeding::Seeding::new(telling.clone(), 2, 3);
            let seeded = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
                .unwrap()
                .seeded_by(seeding.clone());
            let service = KnowledgeService::new_with_backend(
                dir.path().join("repo"),
                dir.path().join("data"),
                &KnowledgeConfig::default(),
                VectorBackend::Given(Arc::new(seeded)),
                &CodegraphConfig::default(),
                None,
                None,
            )
            .unwrap();
            let mut repos = Vec::new();
            for name in ["alpha", "beta", "gamma"] {
                repos.push(another_repo(&service, dir.path(), name, 16).await);
            }
            let run = FillRun {
                at_once,
                ..FillRun::at(FillPace {
                    page: 2,
                    ..FillPace::at(0)
                })
            };
            let pass = service.fill(None, run, &|_, _| {}).await.unwrap();
            assert!(pass.failed.is_empty(), "{pass:?}");
            for repo in &repos {
                assert!(complete(&service, repo).await);
            }
            let most_repos = telling.most_repos.load(Ordering::SeqCst);
            match at_once {
                1 => assert_eq!(most_repos, 1, "one repo's at a time"),
                _ => assert!(most_repos >= 2, "several repos' at once: {most_repos}"),
            }
            assert!(
                telling.most.load(Ordering::SeqCst) <= 3,
                "within the allowance"
            );
        }
    }
}

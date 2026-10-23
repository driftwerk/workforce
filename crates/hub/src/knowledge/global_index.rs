//! Hub-wide knowledge maintenance: enumerate every repo we should index
//! (standalone repos cloned under `{workspace}/repos/`, plus every project
//! repo across every agent), pull the latest source into the indexer's
//! private checkout area, then run the codegraph index pass against each.
//!
//! All checkouts live under the global `{data_dir}/knowledge/checkouts/`
//! tree so agents never read or write inside the indexer's working area —
//! a partially-pulled repo never trips up an agent, and an in-flight agent
//! edit never trips up the parser. The same global tree is the source for
//! every agent's knowledge queries: there is one index per hub.
//!
//! Failures are per-target, not per-pass: one bad pull or a parser error in
//! one repo never aborts the rest. Every target gets a row in the report.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use eyre::{eyre, Result};
use serde::Serialize;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, Semaphore};
use tracing::{info, warn};
use workforce_projects::{Project, ProjectRepo, ProjectStore, RepoSource};

use super::delete::drop_repo_everywhere;
use super::index_layout;
use super::pass_state::{
    LeaseSnapshot, PassRunOutcome, PassScope, PassStateSnapshot, TargetQueueEntry, TargetState,
    ALREADY_IN_FLIGHT_MESSAGE, STARTED_MESSAGE,
};
use super::ring_buffer::RingBuffer;
use super::KnowledgeService;
use crate::projects::fetcher::Fetcher;
use crate::vault::Vault;
use workforce_codebase_knowledge::graph::RepoLineage;
use workforce_codebase_knowledge::indexer::IndexResult;
use workforce_knowledge_service::TriggerOutcome;

/// How many completed `PassReport`s to retain in memory for the
/// `recent_passes` / `recent_failures` accessors. At the default 4h
/// maintenance interval that's ~5 days of history.
pub const PASS_HISTORY_CAPACITY: usize = 32;

/// One repo the indexer cares about, plus where it lives on disk.
///
/// Post-DW-506 the same upstream repo (same owner/name) referenced by
/// multiple projects collapses into one target, with every owning
/// agent listed in `agent_ids`. Standalone (filesystem-discovered)
/// repos with no project referrer have an empty `agent_ids`.
#[derive(Debug, Clone)]
pub struct IndexTarget {
    /// Stable, human-readable id (`repo:{owner}/{repo}`). One per
    /// canonical upstream regardless of how many projects reference it.
    pub id: String,
    pub source: RepoSource,
    pub default_branch: String,
    /// Shared canonical checkout path under
    /// `{data_dir}/knowledge/checkouts/repos/{owner}/{repo}`.
    pub dest: PathBuf,
    /// Every agent that owns a project including this repo, sorted for
    /// deterministic credential lookup. Empty when the repo was only
    /// discovered via the standalone filesystem walk; in that case
    /// credential resolution falls straight through to the global vault.
    pub agent_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum IndexMode {
    Incremental,
    Full,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub id: String,
    #[serde(flatten)]
    pub outcome: TargetOutcome,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TargetOutcome {
    Indexed {
        files_indexed: usize,
        entities_created: usize,
        relationships_created: usize,
        duration_ms: u64,
    },
    PullFailed {
        error: String,
    },
    IndexFailed {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct PassReport {
    pub mode: &'static str,
    pub target_count: usize,
    pub duration_ms: u64,
    pub targets: Vec<TargetReport>,
}

pub struct MaintenanceRunner {
    knowledge: Arc<KnowledgeService>,
    project_store: Arc<dyn ProjectStore>,
    /// Hub data root (`wf_config.hub.data_dir`).
    data_dir: PathBuf,
    /// Where the loose / single-repo `/cc` flow auto-clones repos. The
    /// indexer discovers these via filesystem walk and pulls into the
    /// post-DW-506 shared canonical dir at
    /// `{data_dir}/knowledge/checkouts/repos/{owner}/{repo}` rather
    /// than reading them in place — agents may be editing them.
    workspace: PathBuf,
    /// Per-agent vaults keyed by `agent_id`. Project targets resolve their
    /// PAT against the owning agent's vault first; standalone targets skip
    /// this map and go straight to the global vault. Snapshotted at startup
    /// — same constraint that `slack_manager` already accepts.
    agent_vaults: BTreeMap<String, Arc<Vault>>,
    /// Hub-wide vault used as a fallback when the agent vault lacks a
    /// matching entry, and as the only source for standalone targets.
    global_vault: Option<Arc<Vault>>,
    pull_concurrency: usize,
    /// Single-flight gate: only one `run_pass*` body at a time. Other
    /// callers receive `PassRunOutcome::AlreadyInFlight` with the
    /// snapshot of the running pass. Per-process; cross-process
    /// serialisation is still handled by the per-repo lease table.
    in_flight: Arc<Mutex<()>>,
    /// `Some(..)` while a pass is running; cleared on completion or
    /// panic via [`PassStateGuard`]. Read by `snapshot_state()` so
    /// `knowledge.status` and `knowledge.refresh_index`'s
    /// `AlreadyInFlight` shape can both surface live progress.
    state: RwLock<Option<PassStateSnapshot>>,
    /// Bounded history of completed passes. Surfaced by
    /// `recent_passes()` and filtered by `recent_failures()`.
    history: RwLock<RingBuffer<PassReport>>,
}

impl MaintenanceRunner {
    pub fn new(
        knowledge: Arc<KnowledgeService>,
        project_store: Arc<dyn ProjectStore>,
        data_dir: PathBuf,
        workspace: PathBuf,
        agent_vaults: BTreeMap<String, Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        pull_concurrency: usize,
    ) -> Self {
        Self {
            knowledge,
            project_store,
            data_dir,
            workspace,
            agent_vaults,
            global_vault,
            pull_concurrency: pull_concurrency.max(1),
            in_flight: Arc::new(Mutex::new(())),
            state: RwLock::new(None),
            history: RwLock::new(RingBuffer::new(PASS_HISTORY_CAPACITY)),
        }
    }

    /// Hub-wide pass. Single-flight: returns `AlreadyInFlight` instead
    /// of starting a second pass when one is in progress.
    pub async fn run_pass(&self, mode: IndexMode) -> Result<PassRunOutcome> {
        let Ok(guard) = Arc::clone(&self.in_flight).try_lock_owned() else {
            return Ok(self.already_in_flight_outcome().await);
        };
        let targets = self.enumerate_targets()?;
        self.run_under_guard_owned(guard, PassScope::Hub, mode, targets)
            .await
    }

    /// Start a hub-wide pass in the background and return immediately with a
    /// `Started` snapshot — the caller does NOT wait for it. The spawned task
    /// owns the single-flight guard, so the gate frees exactly when the pass
    /// ends (including on panic/drop). Returns `AlreadyInFlight` when a pass
    /// is already running. Progress and completion are observable via
    /// `knowledge.status`.
    pub async fn start_pass(self: &Arc<Self>, mode: IndexMode) -> Result<PassRunOutcome> {
        let Ok(guard) = Arc::clone(&self.in_flight).try_lock_owned() else {
            return Ok(self.already_in_flight_outcome().await);
        };
        let targets = self.enumerate_targets()?;
        Ok(self
            .spawn_started(guard, PassScope::Hub, mode, targets)
            .await)
    }

    /// Background project-scoped variant: the immediate-return twin of
    /// `run_pass_for_project`, mirroring [`Self::start_pass`]. Resolves the
    /// project (erroring before the gate when missing, so a bad name never
    /// holds up other passes), then spawns the pass and returns `Started`.
    pub async fn start_pass_for_project(
        self: &Arc<Self>,
        agent_id: &str,
        project_name: &str,
        mode: IndexMode,
    ) -> Result<PassRunOutcome> {
        let project = self.find_project(agent_id, project_name)?;
        let Ok(guard) = Arc::clone(&self.in_flight).try_lock_owned() else {
            return Ok(self.already_in_flight_outcome().await);
        };
        let targets = self.enumerate_project_targets(&project);
        let scope = PassScope::Project {
            agent_id: agent_id.to_string(),
            name: project_name.to_string(),
        };
        Ok(self.spawn_started(guard, scope, mode, targets).await)
    }

    /// Seed the in-flight state, spawn the pass owning `guard`, and return a
    /// `Started` outcome with the initial queue snapshot. Shared by
    /// [`Self::start_pass`] and [`Self::start_pass_for_project`].
    async fn spawn_started(
        self: &Arc<Self>,
        guard: OwnedMutexGuard<()>,
        scope: PassScope,
        mode: IndexMode,
        targets: Vec<IndexTarget>,
    ) -> PassRunOutcome {
        // Seed the in-flight state up front so an immediate knowledge.status
        // poll already sees the queued pass, and capture the snapshot to
        // return. Disarm the borrowed guard so it doesn't clear the state on
        // drop — the spawned task owns the full state lifecycle from here.
        let seed = PassStateGuard::install(&self.state, &scope, mode, &targets).await;
        let snapshot = self
            .snapshot_state()
            .await
            .unwrap_or_else(empty_snapshot_fallback);
        seed.disarm();

        let me = Arc::clone(self);
        tokio::spawn(async move {
            match me.run_under_guard_owned(guard, scope, mode, targets).await {
                Ok(PassRunOutcome::Completed(report)) => info!(
                    target_count = report.target_count,
                    duration_ms = report.duration_ms,
                    "background knowledge pass complete"
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "background knowledge pass errored"),
            }
        });

        PassRunOutcome::Started {
            message: STARTED_MESSAGE,
            snapshot,
        }
    }

    async fn run_under_guard_owned(
        &self,
        _guard: OwnedMutexGuard<()>,
        scope: PassScope,
        mode: IndexMode,
        targets: Vec<IndexTarget>,
    ) -> Result<PassRunOutcome> {
        let cleanup = PassStateGuard::install(&self.state, &scope, mode, &targets).await;
        let result = self.run_targets(targets, mode).await;
        // Clear state synchronously in the happy path so the next call
        // sees a clean slate; the Drop impl on `cleanup` is a defensive
        // fallback for panic / early-return paths.
        *self.state.write().await = None;
        cleanup.disarm();
        let report = result?;
        self.history.write().await.push(report.clone());
        Ok(PassRunOutcome::Completed(report))
    }

    async fn already_in_flight_outcome(&self) -> PassRunOutcome {
        let snap = self
            .snapshot_state()
            .await
            .unwrap_or_else(empty_snapshot_fallback);
        PassRunOutcome::AlreadyInFlight {
            message: ALREADY_IN_FLIGHT_MESSAGE,
            in_flight: snap,
        }
    }

    /// The knowledge service the passes index with; its code graph is the
    /// one every agent's code tools read.
    pub fn knowledge(&self) -> &Arc<KnowledgeService> {
        &self.knowledge
    }

    /// Snapshot the in-flight pass. Returns `None` when no pass is
    /// running. The lease lookup for the currently-indexing target is
    /// best-effort; a lookup error simply omits `last_lease`.
    pub async fn snapshot_state(&self) -> Option<PassStateSnapshot> {
        let mut snap = self.state.read().await.clone()?;
        let current = snap.current_target_index;
        if let Some(entry) = snap.targets.get(current) {
            if entry.state == TargetState::Indexing {
                let repo_id = workforce_knowledge_service::KnowledgeService::repo_id_for(
                    &index_layout::repo_dir(&self.data_dir, &entry.source),
                );
                if let Ok(Some(lease)) = self.knowledge.graph().get_lease(&repo_id).await {
                    if let Some(entry_mut) = snap.targets.get_mut(current) {
                        entry_mut.last_lease = Some(LeaseSnapshot::from(&lease));
                    }
                }
            }
        }
        Some(snap)
    }

    /// Bounded most-recent-first view of completed passes.
    pub async fn recent_passes(&self) -> Vec<PassReport> {
        self.history.read().await.iter().rev().cloned().collect()
    }

    /// Filter [`Self::recent_passes`] to entries whose `TargetReport`
    /// outcome is `PullFailed` or `IndexFailed`. Flattened so callers
    /// can iterate one row per failed target.
    pub async fn recent_failures(&self) -> Vec<RecentFailure> {
        self.history
            .read()
            .await
            .iter()
            .rev()
            .flat_map(|report| {
                report
                    .targets
                    .iter()
                    .filter(|t| !matches!(t.outcome, TargetOutcome::Indexed { .. }))
                    .map(|t| RecentFailure {
                        pass_mode: report.mode,
                        target_id: t.id.clone(),
                        outcome: t.outcome.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    async fn mark_target_state(&self, index: usize, state: TargetState) {
        if let Some(snap) = self.state.write().await.as_mut() {
            if let Some(entry) = snap.targets.get_mut(index) {
                entry.state = state;
                if state == TargetState::Indexing {
                    snap.current_target_index = index;
                    entry.last_lease = None;
                }
            }
        }
    }

    async fn run_targets(&self, targets: Vec<IndexTarget>, mode: IndexMode) -> Result<PassReport> {
        let start = Instant::now();
        info!(
            target_count = targets.len(),
            mode = ?mode,
            "knowledge maintenance pass starting"
        );

        let pulled = self.pull_all(&targets).await;
        let reports = self.index_all(&targets, &pulled, mode).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        info!(
            duration_ms,
            target_count = reports.len(),
            "knowledge maintenance pass complete"
        );
        Ok(PassReport {
            mode: mode_label(mode),
            target_count: reports.len(),
            duration_ms,
            targets: reports,
        })
    }

    pub fn enumerate_targets(&self) -> Result<Vec<IndexTarget>> {
        Ok(
            RepoCatalog::build(&self.workspace, &self.data_dir, self.project_store.as_ref())?
                .into_targets(),
        )
    }

    /// Drop every stored repository index that is no longer a canonical target
    /// — e.g. a former in-place root or an agent-dir clone left by an earlier
    /// layout. One index per repo: a row absent from the current target set is
    /// an orphan. Rows under a fresh lease are left to the in-flight pass.
    /// Returns the dropped repo_ids.
    pub async fn prune_orphans(&self) -> Result<Vec<String>> {
        let canonical: std::collections::HashSet<String> =
            self.enumerate_targets()?.iter().map(repo_id_of).collect();
        // Never treat "no targets" as "everything is an orphan" — that would
        // wipe every index if the catalog were momentarily empty. With zero
        // canonical repos there is nothing to prune against.
        if canonical.is_empty() {
            return Ok(Vec::new());
        }
        let now = workforce_codebase_knowledge::lease::now_ms();
        let leased: std::collections::HashSet<String> = self
            .knowledge
            .graph()
            .list_active_leases()
            .await?
            .into_iter()
            .filter(|lease| lease.is_fresh(now))
            .map(|lease| lease.repo_id)
            .collect();
        let mut dropped = Vec::new();
        for row in self.knowledge.graph().list_repositories().await? {
            if canonical.contains(&row.id) || leased.contains(&row.id) {
                continue;
            }
            drop_repo_everywhere(&self.knowledge, &row.id).await;
            dropped.push(row.id);
        }
        Ok(dropped)
    }

    /// Build the canonical IndexTarget set for one project. Same dedup
    /// invariant as the hub-wide walk: if two `ProjectRepo` aliases name
    /// the same upstream, they collapse to one target.
    pub fn enumerate_project_targets(&self, project: &Project) -> Vec<IndexTarget> {
        let mut by_slug: BTreeMap<String, IndexTarget> = BTreeMap::new();
        for repo in &project.repos {
            let mut target = project_to_target(repo, &self.data_dir);
            target.default_branch = project.effective_default_branch(&repo.alias).to_string();
            target.agent_ids = vec![project.agent_id.clone()];
            by_slug
                .entry(target.source.canonical_slug())
                .or_insert(target);
        }
        by_slug.into_values().collect()
    }

    /// Look up a project by `(agent_id, name)`. The store keys on this
    /// tuple so `list(agent_id)` + `find` is the cheapest path; no
    /// `list_all` walk needed.
    pub fn find_project(&self, agent_id: &str, name: &str) -> Result<Project> {
        self.project_store
            .list(agent_id)?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                eyre!(
                    "project '{}' not found for agent '{}'. Use projects.list to see available projects.",
                    name, agent_id
                )
            })
    }

    fn resolve_github_token_for(&self, target: &IndexTarget) -> Option<String> {
        resolve_github_token_for_target(&self.agent_vaults, self.global_vault.as_deref(), target)
    }

    async fn pull_all(&self, targets: &[IndexTarget]) -> Vec<Result<(), String>> {
        let semaphore = Arc::new(Semaphore::new(self.pull_concurrency));
        let tasks: Vec<_> = targets
            .iter()
            .map(|t| {
                let target = t.clone();
                let token = self.resolve_github_token_for(&target);
                // Blobless: indexer checkouts are crawled + diffed, never used
                // as `git clone --local` sources, so a partial clone is safe and
                // keeps large repos (e.g. HarborRails) inside the timeout.
                let fetcher = Fetcher::new_blobless(token);
                let permit = Arc::clone(&semaphore);
                tokio::spawn(async move {
                    let _permit = permit
                        .acquire_owned()
                        .await
                        .map_err(|e| format!("semaphore closed: {e}"))?;
                    fetcher
                        .pull_repo(&target.source, &target.dest, &target.default_branch)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
            })
            .collect();

        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            results.push(match task.await {
                Ok(r) => r,
                Err(e) => Err(format!("pull task panicked: {e}")),
            });
        }
        results
    }

    async fn index_all(
        &self,
        targets: &[IndexTarget],
        pulled: &[Result<(), String>],
        mode: IndexMode,
    ) -> Vec<TargetReport> {
        let mut reports = Vec::with_capacity(targets.len());
        for (i, (target, pull_result)) in targets.iter().zip(pulled.iter()).enumerate() {
            if let Err(e) = pull_result {
                warn!(target = %target.id, error = %e, "pull failed; skipping index");
                self.mark_target_state(i, TargetState::Failed).await;
                reports.push(TargetReport {
                    id: target.id.clone(),
                    outcome: TargetOutcome::PullFailed { error: e.clone() },
                });
                continue;
            }
            self.mark_target_state(i, TargetState::Indexing).await;
            let report = self.index_one(target, mode).await;
            let terminal = match report.outcome {
                TargetOutcome::Indexed { .. } => TargetState::Done,
                TargetOutcome::PullFailed { .. } | TargetOutcome::IndexFailed { .. } => {
                    TargetState::Failed
                }
            };
            self.mark_target_state(i, terminal).await;
            reports.push(report);
        }
        reports
    }

    async fn index_one(&self, target: &IndexTarget, mode: IndexMode) -> TargetReport {
        let started = Instant::now();
        // Incremental dispatches through `trigger_for` so a repo already at
        // HEAD but with a summary gap drains its backlog via
        // `NextAction::ResumeSummaries` — deterministically, no LLM in the loop.
        // `Full` forces a complete reindex.
        let result: Result<IndexResult> = match mode {
            IndexMode::Incremental => {
                self.knowledge
                    .trigger_for(&target.dest)
                    .await
                    .map(|outcome| match outcome {
                        TriggerOutcome::Full(r)
                        | TriggerOutcome::Incremental(r)
                        | TriggerOutcome::ResumeSummaries(r) => r,
                        // Already current, or a fresh lease held by another
                        // worker: nothing for this pass to do.
                        TriggerOutcome::Complete | TriggerOutcome::AlreadyIndexing(_) => {
                            IndexResult {
                                files_indexed: 0,
                                entities_created: 0,
                                relationships_created: 0,
                                summaries_generated: 0,
                                duration_ms: 0,
                            }
                        }
                    })
            }
            IndexMode::Full => self.knowledge.trigger_full_index_for(&target.dest).await,
        };
        match result {
            Ok(r) => {
                // target.dest is built from `index_layout::repo_dir`, so by
                // construction the row's lineage is Canonical. Tag it after
                // the index commits so the row exists.
                let repo_id =
                    workforce_knowledge_service::KnowledgeService::repo_id_for(&target.dest);
                if let Err(e) = self
                    .knowledge
                    .graph()
                    .set_repo_lineage(&repo_id, RepoLineage::Canonical)
                    .await
                {
                    warn!(target = %target.id, error = %e, "tag canonical lineage failed (non-fatal)");
                }
                TargetReport {
                    id: target.id.clone(),
                    outcome: TargetOutcome::Indexed {
                        files_indexed: r.files_indexed,
                        entities_created: r.entities_created,
                        relationships_created: r.relationships_created,
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                }
            }
            Err(e) => {
                warn!(target = %target.id, error = %e, "index failed");
                TargetReport {
                    id: target.id.clone(),
                    outcome: TargetOutcome::IndexFailed {
                        error: e.to_string(),
                    },
                }
            }
        }
    }
}

/// Resolve the PAT to use for cloning/fetching `target`. Walks every
/// owning agent's vault in `target.agent_ids` order (already sorted by
/// the enumerator), returning the first hit; falls through to the
/// global vault when no agent vault has a matching token. Standalone
/// targets — `agent_ids` empty — skip directly to the global vault.
///
/// Sorted-order walk is load-bearing: a flaky vault that intermittently
/// returns no PAT could otherwise cause the indexer to silently swap
/// between two valid tokens between passes, masking the underlying
/// problem and producing inconsistent rate-limit attribution upstream.
fn resolve_github_token_for_target(
    agent_vaults: &BTreeMap<String, Arc<Vault>>,
    global_vault: Option<&Vault>,
    target: &IndexTarget,
) -> Option<String> {
    let RepoSource::Github { owner, .. } = &target.source;
    for agent_id in &target.agent_ids {
        let Some(vault) = agent_vaults.get(agent_id) else {
            continue;
        };
        if let Some(token) =
            crate::user_config::resolve_github_pat(Some(vault.as_ref()), None, owner)
        {
            return Some(token);
        }
    }
    crate::user_config::resolve_github_pat(None, global_vault, owner)
}

/// Walk `{workspace}/repos/{owner}/{repo}/.git` and seed the dedup map
/// with one entry per filesystem-discovered repo. Standalone-only
/// entries never set `agent_ids` — token resolution falls through to
/// the global vault. If a project later names the same upstream, the
/// project merge upgrades the `agent_ids` list and `default_branch`.
fn merge_standalone(
    workspace: &Path,
    data_dir: &Path,
    by_slug: &mut BTreeMap<String, IndexTarget>,
) -> Result<()> {
    let repos_root = workspace.join("repos");
    if !repos_root.is_dir() {
        return Ok(());
    }
    for owner_entry in std::fs::read_dir(&repos_root)? {
        let owner_entry = owner_entry?;
        if !owner_entry.file_type()?.is_dir() {
            continue;
        }
        let owner = owner_entry.file_name().to_string_lossy().into_owned();
        for repo_entry in std::fs::read_dir(owner_entry.path())? {
            let repo_entry = repo_entry?;
            if !repo_entry.file_type()?.is_dir() {
                continue;
            }
            // Only treat it as a target if it's actually a clone, not a stray dir.
            if !repo_entry.path().join(".git").exists() {
                continue;
            }
            let repo = repo_entry.file_name().to_string_lossy().into_owned();
            let source = RepoSource::Github {
                owner: owner.clone(),
                repo: repo.clone(),
            };
            let slug = source.canonical_slug();
            by_slug.entry(slug.clone()).or_insert_with(|| IndexTarget {
                id: format!("repo:{slug}"),
                dest: index_layout::repo_dir(data_dir, &source),
                source,
                default_branch: "main".to_string(),
                agent_ids: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Build the canonical IndexTarget skeleton for one project repo
/// (no agent_ids — callers attach those in the way that fits their
/// merge or filter semantics). The only place path/id/branch are
/// derived from a `ProjectRepo`, so `merge_projects` and
/// `enumerate_project_targets` share one source of truth.
fn project_to_target(repo: &ProjectRepo, data_dir: &Path) -> IndexTarget {
    let slug = repo.source.canonical_slug();
    IndexTarget {
        id: format!("repo:{slug}"),
        dest: index_layout::repo_dir(data_dir, &repo.source),
        source: repo.source.clone(),
        default_branch: repo
            .default_branch
            .clone()
            .unwrap_or_else(|| "main".to_string()),
        agent_ids: Vec::new(),
    }
}

/// Fold every project repo into the dedup map keyed by canonical slug.
/// First-seen branch wins on conflict (with a warning logged) so the
/// outcome is deterministic even when two projects pin the same
/// upstream to different branches; ordering of `store.list_all()` is
/// the tie-break.
fn merge_projects(
    store: &dyn ProjectStore,
    data_dir: &Path,
    by_slug: &mut BTreeMap<String, IndexTarget>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut project_slugs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for project in store.list_all()? {
        for repo in &project.repos {
            let mut target = project_to_target(repo, data_dir);
            target.default_branch = project.effective_default_branch(&repo.alias).to_string();
            let slug = target.source.canonical_slug();
            project_slugs
                .entry(project.name.clone())
                .or_default()
                .push(slug.clone());
            let entry = by_slug
                .entry(slug.clone())
                .or_insert_with(|| target.clone());
            if entry.default_branch != target.default_branch {
                warn!(
                    repo = %slug,
                    existing = %entry.default_branch,
                    requested = %target.default_branch,
                    project = %format!("{}/{}", project.agent_id, project.name),
                    "index branch conflict — keeping first-seen branch; project default_branch overrides on shared repos are best-effort"
                );
            }
            if !entry.agent_ids.contains(&project.agent_id) {
                entry.agent_ids.push(project.agent_id.clone());
                entry.agent_ids.sort();
            }
        }
    }
    for slugs in project_slugs.values_mut() {
        slugs.sort();
        slugs.dedup();
    }
    Ok(project_slugs)
}

fn repo_id_of(target: &IndexTarget) -> String {
    KnowledgeService::repo_id_for(&target.dest)
}

/// Read-only view of the one-index-per-repo set the hub maintains, plus the
/// name → `repo_id` resolution the query tools and orphan cleanup share. Built
/// fresh from the project store + on-disk standalone repos so it always
/// reflects the current registration; the single source of truth for "which
/// repos exist and what is each one's canonical id".
pub struct RepoCatalog {
    targets: Vec<IndexTarget>,
    project_slugs: BTreeMap<String, Vec<String>>,
}

impl RepoCatalog {
    pub fn build(workspace: &Path, data_dir: &Path, store: &dyn ProjectStore) -> Result<Self> {
        let mut by_slug: BTreeMap<String, IndexTarget> = BTreeMap::new();
        merge_standalone(workspace, data_dir, &mut by_slug)?;
        let project_slugs = merge_projects(store, data_dir, &mut by_slug)?;
        Ok(Self {
            targets: by_slug.into_values().collect(),
            project_slugs,
        })
    }

    pub fn into_targets(self) -> Vec<IndexTarget> {
        self.targets
    }

    pub fn targets(&self) -> &[IndexTarget] {
        &self.targets
    }

    /// Canonical `repo_id`s of every indexed repo — the set against which a
    /// stored `repositories` row counts as an orphan to clean up.
    pub fn canonical_repo_ids(&self) -> std::collections::HashSet<String> {
        self.targets.iter().map(repo_id_of).collect()
    }

    /// The lone repo when exactly one is indexed — the default when a query
    /// omits its `repo` argument.
    pub fn sole_target(&self) -> Option<&IndexTarget> {
        match self.targets.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    pub fn sole(&self) -> Option<String> {
        self.sole_target().map(repo_id_of)
    }

    /// Resolve a repo/project `name` to its canonical target: an exact
    /// `owner/repo` slug, an unambiguous bare repo name, or a single-repo
    /// project. Errors (listing the indexed names) rather than guessing.
    pub fn resolve_target(&self, name: &str) -> Result<&IndexTarget, String> {
        if let Some(target) = self.find_by_slug(name) {
            return Ok(target);
        }
        let by_repo: Vec<&IndexTarget> = self
            .targets
            .iter()
            .filter(|t| matches!(&t.source, RepoSource::Github { repo, .. } if repo == name))
            .collect();
        match by_repo.as_slice() {
            [only] => return Ok(only),
            [_, ..] => {
                return Err(ambiguous(
                    name,
                    by_repo.iter().map(|t| t.source.canonical_slug()),
                ))
            }
            [] => {}
        }
        if let Some(slugs) = self.project_slugs.get(name) {
            match slugs.as_slice() {
                [slug] => {
                    if let Some(target) = self.find_by_slug(slug) {
                        return Ok(target);
                    }
                }
                [_, ..] => return Err(ambiguous(name, slugs.iter().cloned())),
                [] => {}
            }
        }
        Err(format!(
            "unknown repo or project '{name}'; indexed: {}",
            self.indexed_names().join(", ")
        ))
    }

    /// Resolve a repo/project `name` to its canonical `repo_id`.
    pub fn resolve(&self, name: &str) -> Result<String, String> {
        self.resolve_target(name).map(repo_id_of)
    }

    fn find_by_slug(&self, slug: &str) -> Option<&IndexTarget> {
        let needle = canonical_query_slug(slug);
        self.targets
            .iter()
            .find(|t| t.source.canonical_slug() == needle)
    }

    /// Every repo/project name a query may target, sorted — for "specify
    /// `repo`" / "unknown repo" error messages.
    pub fn indexed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .targets
            .iter()
            .map(|t| t.source.canonical_slug())
            .collect();
        names.extend(self.project_slugs.keys().cloned());
        names.sort();
        names.dedup();
        names
    }
}

/// Owner-case-insensitive form of an `owner/repo` slug query, folded by the
/// same `RepoSource::canonical_slug` path as catalog slugs so resolution stays
/// symmetric with dedup; a bare repo name passes through untouched.
fn canonical_query_slug(slug: &str) -> String {
    match slug.split_once('/') {
        Some((owner, repo)) => RepoSource::Github {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
        .canonical_slug(),
        None => slug.to_string(),
    }
}

fn ambiguous(name: &str, slugs: impl Iterator<Item = String>) -> String {
    let mut slugs: Vec<String> = slugs.collect();
    slugs.sort();
    slugs.dedup();
    format!("'{name}' is ambiguous; name one of: {}", slugs.join(", "))
}

/// Resolves a query's optional `repo` argument to a canonical `repo_id`,
/// building a fresh [`RepoCatalog`] each call so it always reflects current
/// registration. Held by the hub's knowledge query tools.
pub struct RepoResolver {
    store: Arc<dyn ProjectStore>,
    data_dir: PathBuf,
    workspace: PathBuf,
}

impl RepoResolver {
    pub fn new(store: Arc<dyn ProjectStore>, data_dir: PathBuf, workspace: PathBuf) -> Self {
        Self {
            store,
            data_dir,
            workspace,
        }
    }

    /// The catalog as registered now.
    pub fn catalog(&self) -> Result<RepoCatalog> {
        RepoCatalog::build(&self.workspace, &self.data_dir, self.store.as_ref())
    }

    fn resolve_with<T>(
        &self,
        repo: Option<&str>,
        pick: impl FnOnce(&IndexTarget) -> T,
    ) -> Result<T, String> {
        let catalog = self
            .catalog()
            .map_err(|e| format!("failed to read repo catalog: {e}"))?;
        let target = match repo {
            Some(name) => catalog.resolve_target(name)?,
            None => catalog.sole_target().ok_or_else(|| {
                format!(
                    "multiple repos are indexed — pass `repo` naming one of: {}",
                    catalog.indexed_names().join(", ")
                )
            })?,
        };
        Ok(pick(target))
    }

    /// Resolve the query target's canonical `repo_id`.
    pub fn resolve(&self, repo: Option<&str>) -> Result<String, String> {
        self.resolve_with(repo, repo_id_of)
    }

    /// Resolve the canonical checkout directory — the path `knowledge.index`
    /// operates on.
    pub fn resolve_dest(&self, repo: Option<&str>) -> Result<PathBuf, String> {
        self.resolve_with(repo, |target| target.dest.clone())
    }

    /// Names of every indexed repo, for discovery (`repo.list`) and for
    /// surfacing the available set in tool errors. Best-effort: an unreadable
    /// catalog yields an empty list rather than an error.
    pub fn indexed_names(&self) -> Vec<String> {
        self.catalog()
            .map(|catalog| catalog.indexed_names())
            .unwrap_or_default()
    }
}

/// Resolve a query tool's target `repo_id`: via the canonical [`RepoResolver`]
/// when the hub supplies one, otherwise the service's own root (single-agent
/// setups, which index exactly one repo).
///
/// Without a resolver `repo` is a path inside the agent's workspace. It comes
/// from the model, so one that is absolute or climbs with `..` is refused
/// before it is joined: the id of any directory on the host is derivable from
/// its path, and with it whatever the index holds for that directory.
pub fn resolve_query_repo(
    service: &KnowledgeService,
    agent_workspace: &Path,
    resolver: Option<&RepoResolver>,
    repo: &Option<String>,
) -> Result<String, String> {
    match resolver {
        Some(resolver) => resolver.resolve(repo.as_deref()),
        None => match repo.as_deref().filter(|path| !stays_inside(path)) {
            Some(outside) => Err(format!("Path '{outside}' is outside the workspace")),
            None => service.resolve_repo_id(agent_workspace, repo),
        },
    }
}

/// Whether a relative path names something at or below where it is joined.
fn stays_inside(path: &str) -> bool {
    Path::new(path).components().all(|part| {
        matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

fn mode_label(mode: IndexMode) -> &'static str {
    match mode {
        IndexMode::Incremental => "incremental",
        IndexMode::Full => "full",
    }
}

fn empty_snapshot_fallback() -> PassStateSnapshot {
    PassStateSnapshot {
        scope: PassScope::Hub,
        mode: "incremental",
        started_at: Utc::now(),
        current_target_index: 0,
        targets: Vec::new(),
    }
}

/// One flattened entry returned by `MaintenanceRunner::recent_failures`:
/// a target that did not reach `Indexed` in some recent pass.
#[derive(Debug, Clone, Serialize)]
pub struct RecentFailure {
    pub pass_mode: &'static str,
    pub target_id: String,
    #[serde(flatten)]
    pub outcome: TargetOutcome,
}

/// RAII fallback that clears the in-flight `PassStateSnapshot` if a
/// pass body panics or returns early without going through the normal
/// teardown. The happy path calls `disarm()` after a synchronous
/// clear so the Drop becomes a no-op.
struct PassStateGuard<'a> {
    state: &'a RwLock<Option<PassStateSnapshot>>,
    armed: bool,
}

impl<'a> PassStateGuard<'a> {
    async fn install(
        state: &'a RwLock<Option<PassStateSnapshot>>,
        scope: &PassScope,
        mode: IndexMode,
        targets: &[IndexTarget],
    ) -> Self {
        let entries = targets
            .iter()
            .map(|t| TargetQueueEntry {
                id: t.id.clone(),
                source: t.source.clone(),
                state: TargetState::Queued,
                last_lease: None,
            })
            .collect();
        let snap = PassStateSnapshot {
            scope: scope.clone(),
            mode: mode_label(mode),
            started_at: Utc::now(),
            current_target_index: 0,
            targets: entries,
        };
        *state.write().await = Some(snap);
        Self { state, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl<'a> Drop for PassStateGuard<'a> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut g) = self.state.try_write() {
            *g = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_path_stays_inside_only_without_a_root_or_a_climb() {
        for inside in ["services/auth", "repos/acme/api", "./services", "api"] {
            assert!(stays_inside(inside), "{inside}");
        }
        for outside in [
            "/srv/agents/other/workspace",
            "..",
            "services/../../other",
            "../x",
        ] {
            assert!(!stays_inside(outside), "{outside}");
        }
    }
    use tempfile::TempDir;
    use workforce_projects::sqlite::SqliteProjectStore;
    use workforce_projects::{Project, ProjectRepo};

    fn enumerate_to_vec(
        workspace: &Path,
        data_dir: &Path,
        store: &dyn ProjectStore,
    ) -> Vec<IndexTarget> {
        let mut by = BTreeMap::new();
        merge_standalone(workspace, data_dir, &mut by).unwrap();
        merge_projects(store, data_dir, &mut by).unwrap();
        by.into_values().collect()
    }

    fn empty_store() -> Arc<dyn ProjectStore> {
        Arc::new(SqliteProjectStore::in_memory().unwrap())
    }

    #[test]
    fn standalone_walk_yields_one_target_per_owner_repo() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("repos/acme/widgets/.git")).unwrap();
        std::fs::create_dir_all(workspace.join("repos/acme/gadgets/.git")).unwrap();
        std::fs::create_dir_all(workspace.join("repos/other/thing/.git")).unwrap();

        let store = empty_store();
        let targets = enumerate_to_vec(&workspace, tmp.path(), store.as_ref());
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["repo:acme/gadgets", "repo:acme/widgets", "repo:other/thing",]
        );
        for t in &targets {
            assert!(t.agent_ids.is_empty());
        }
    }

    #[test]
    fn standalone_walk_skips_non_git_dirs() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("repos/acme/has-git/.git")).unwrap();
        std::fs::create_dir_all(workspace.join("repos/acme/no-git/src")).unwrap();

        let store = empty_store();
        let targets = enumerate_to_vec(&workspace, tmp.path(), store.as_ref());
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["repo:acme/has-git"]);
    }

    #[test]
    fn standalone_walk_handles_missing_workspace() {
        let tmp = TempDir::new().unwrap();
        let store = empty_store();
        let targets = enumerate_to_vec(
            &tmp.path().join("does-not-exist"),
            tmp.path(),
            store.as_ref(),
        );
        assert!(targets.is_empty());
    }

    #[test]
    fn owner_case_variant_checkouts_collapse_to_canonical_repo_id() {
        // A stray capital-`O` checkout next to the canonical lowercase one must
        // not fork a second index: both dedupe to one target whose repo_id is
        // the canonical (lowercase-owner) id.
        let tmp = TempDir::new().unwrap();
        let store = empty_store();

        let mixed = tmp.path().join("mixed");
        std::fs::create_dir_all(mixed.join("repos/Harborlabs/HarborApps/.git")).unwrap();
        std::fs::create_dir_all(mixed.join("repos/harborlabs/HarborApps/.git")).unwrap();
        let mixed_targets = enumerate_to_vec(&mixed, tmp.path(), store.as_ref());
        assert_eq!(mixed_targets.len(), 1);
        assert_eq!(mixed_targets[0].id, "repo:harborlabs/HarborApps");

        let lower = tmp.path().join("lower");
        std::fs::create_dir_all(lower.join("repos/harborlabs/HarborApps/.git")).unwrap();
        let lower_targets = enumerate_to_vec(&lower, tmp.path(), store.as_ref());

        assert_eq!(repo_id_of(&mixed_targets[0]), repo_id_of(&lower_targets[0]));
    }

    #[test]
    fn projects_merge_yields_one_target_per_canonical_repo() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());

        let mut p = Project::new("allan", "harbor");
        for alias in ["rails", "frontend"] {
            p.repos.push(ProjectRepo {
                alias: alias.into(),
                source: RepoSource::Github {
                    owner: "harborlabs".into(),
                    repo: alias.into(),
                },
                default_branch: None,
                setup: None,
            });
        }
        store.put(&p).unwrap();

        let mut q = Project::new("rita", "fabrikam");
        q.defaults.default_branch = Some("develop".into());
        q.repos.push(ProjectRepo {
            alias: "backend".into(),
            source: RepoSource::Github {
                owner: "fabrikam".into(),
                repo: "backend".into(),
            },
            default_branch: None,
            setup: None,
        });
        store.put(&q).unwrap();

        let workspace = tmp.path().join("ws");
        let targets = enumerate_to_vec(&workspace, tmp.path(), store.as_ref());
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "repo:fabrikam/backend",
                "repo:harborlabs/frontend",
                "repo:harborlabs/rails",
            ]
        );

        let fabrikam = targets
            .iter()
            .find(|t| t.id == "repo:fabrikam/backend")
            .unwrap();
        assert_eq!(fabrikam.default_branch, "develop");
        assert_eq!(fabrikam.agent_ids, vec!["rita".to_string()]);
        assert!(fabrikam
            .dest
            .ends_with("knowledge/checkouts/repos/fabrikam/backend"));

        let rails = targets
            .iter()
            .find(|t| t.id == "repo:harborlabs/rails")
            .unwrap();
        assert_eq!(rails.agent_ids, vec!["allan".to_string()]);
    }

    #[test]
    fn shared_repo_collapses_to_one_target_with_both_agent_ids() {
        // Two agents' projects naming the same upstream → one IndexTarget,
        // both agents listed sorted. Confirms the dedup invariant the
        // whole DW-506 design hangs on.
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());

        let mut allan_p = Project::new("allan", "shared");
        allan_p.repos.push(ProjectRepo {
            alias: "tool".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "shared-tool".into(),
            },
            default_branch: None,
            setup: None,
        });
        store.put(&allan_p).unwrap();

        let mut rita_p = Project::new("rita", "also-shared");
        rita_p.repos.push(ProjectRepo {
            alias: "the-tool".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "shared-tool".into(),
            },
            default_branch: None,
            setup: None,
        });
        store.put(&rita_p).unwrap();

        let workspace = tmp.path().join("ws");
        let targets = enumerate_to_vec(&workspace, tmp.path(), store.as_ref());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "repo:acme/shared-tool");
        assert_eq!(
            targets[0].agent_ids,
            vec!["allan".to_string(), "rita".to_string()]
        );
    }

    #[test]
    fn standalone_and_project_referrers_share_one_target() {
        // A standalone-discovered repo named by a project should produce
        // a single target whose agent_ids include the project's agent —
        // no double-pull, no double-index.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("repos/acme/shared/.git")).unwrap();

        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let mut p = Project::new("allan", "p");
        p.repos.push(ProjectRepo {
            alias: "shared".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "shared".into(),
            },
            default_branch: None,
            setup: None,
        });
        store.put(&p).unwrap();

        let targets = enumerate_to_vec(&workspace, tmp.path(), store.as_ref());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "repo:acme/shared");
        assert_eq!(targets[0].agent_ids, vec!["allan".to_string()]);
    }

    fn make_vault(tmp: &Path, name: &str) -> Arc<Vault> {
        let path = tmp.join(format!("{name}.enc"));
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    fn shared_target(owner: &str, repo: &str, agent_ids: &[&str]) -> IndexTarget {
        IndexTarget {
            id: format!("repo:{owner}/{repo}"),
            source: RepoSource::Github {
                owner: owner.into(),
                repo: repo.into(),
            },
            default_branch: "main".into(),
            dest: PathBuf::from("/tmp"),
            agent_ids: agent_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolve_token_dispatches_to_owning_agent_vault() {
        let tmp = TempDir::new().unwrap();
        let penny = make_vault(tmp.path(), "penny");
        penny
            .set("skills/github/workspaces/contoso/token", "penny-ws")
            .unwrap();

        let mut agent_vaults = BTreeMap::new();
        agent_vaults.insert("penny".into(), penny);

        let target = shared_target("contoso", "monorepo", &["penny"]);
        let token = resolve_github_token_for_target(&agent_vaults, None, &target);
        assert_eq!(token.as_deref(), Some("penny-ws"));
    }

    #[test]
    fn resolve_token_does_not_cross_unlisted_agents() {
        // A target whose agent_ids list only `penny` must not pick up
        // allan's token even though allan's vault has one. The DW-506
        // multi-agent fallback only walks vaults the project store
        // actually links to this repo.
        let tmp = TempDir::new().unwrap();
        let penny = make_vault(tmp.path(), "penny");
        let allan = make_vault(tmp.path(), "allan");
        allan.set("skills/github/token", "allans-pat").unwrap();

        let mut agent_vaults = BTreeMap::new();
        agent_vaults.insert("penny".into(), penny);
        agent_vaults.insert("allan".into(), allan);

        let target = shared_target("contoso", "monorepo", &["penny"]);
        let token = resolve_github_token_for_target(&agent_vaults, None, &target);
        assert!(token.is_none());
    }

    #[test]
    fn resolve_token_falls_back_through_listed_agents_in_order() {
        // First agent (sorted: allan) has no PAT for this owner → walk
        // continues to the next listed agent (penny) and returns its
        // token. Documents the deterministic-order contract from the
        // resolver doc comment.
        let tmp = TempDir::new().unwrap();
        let allan = make_vault(tmp.path(), "allan");
        let penny = make_vault(tmp.path(), "penny");
        penny.set("skills/github/token", "penny-pat").unwrap();

        let mut agent_vaults = BTreeMap::new();
        agent_vaults.insert("allan".into(), allan);
        agent_vaults.insert("penny".into(), penny);

        let target = shared_target("acme", "widgets", &["allan", "penny"]);
        let token = resolve_github_token_for_target(&agent_vaults, None, &target);
        assert_eq!(token.as_deref(), Some("penny-pat"));
    }

    #[test]
    fn resolve_token_for_standalone_only_uses_global_vault() {
        // Empty agent_ids → standalone-only target → never fishes in
        // any agent vault, even one that happens to be loaded.
        let tmp = TempDir::new().unwrap();
        let penny = make_vault(tmp.path(), "penny");
        let global = make_vault(tmp.path(), "global");
        penny.set("skills/github/token", "agent-pat").unwrap();
        global.set("skills/github/token", "global-pat").unwrap();

        let mut agent_vaults = BTreeMap::new();
        agent_vaults.insert("penny".into(), penny);

        let target = shared_target("acme", "widgets", &[]);
        let token = resolve_github_token_for_target(&agent_vaults, Some(&global), &target);
        assert_eq!(token.as_deref(), Some("global-pat"));
    }

    fn make_runner(tmp: &TempDir, store: Arc<dyn ProjectStore>) -> MaintenanceRunner {
        MaintenanceRunner::new(
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).unwrap()),
            store,
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
            BTreeMap::new(),
            None,
            1,
        )
    }

    #[tokio::test]
    async fn start_pass_returns_started_then_completes_in_background() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(make_runner(&tmp, store));

        let outcome = runner.start_pass(IndexMode::Incremental).await.unwrap();
        assert!(
            matches!(outcome, PassRunOutcome::Started { .. }),
            "first call should return Started, got {outcome:?}"
        );

        // The pass runs in a spawned task. Wait for it to record into history
        // AND release the gate, both of which happen as the task winds down.
        let mut waited = 0;
        loop {
            let recorded = !runner.recent_passes().await.is_empty();
            let gate_free = Arc::clone(&runner.in_flight).try_lock_owned().is_ok();
            if recorded && gate_free {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            waited += 1;
            // Generous ceiling (~10s): a real empty pass settles in <1ms; only
            // a genuine hang should ever trip this, even under full-suite load.
            assert!(waited < 1000, "background pass never settled");
        }

        let recent = runner.recent_passes().await;
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target_count, 0);
    }

    #[tokio::test]
    async fn start_pass_returns_already_in_flight_when_locked() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(make_runner(&tmp, store));

        // Hold the gate to simulate a pass already running.
        let _held = Arc::clone(&runner.in_flight).lock_owned().await;
        let outcome = runner.start_pass(IndexMode::Incremental).await.unwrap();
        assert!(matches!(outcome, PassRunOutcome::AlreadyInFlight { .. }));
    }

    #[tokio::test]
    async fn concurrent_run_pass_returns_already_in_flight_for_loser() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(make_runner(&tmp, store));

        // Pre-acquire the in_flight mutex to simulate a long-running
        // pass without actually starting the pipeline.
        let guard = runner.in_flight.lock().await;

        let outcome = runner.run_pass(IndexMode::Incremental).await.unwrap();
        assert!(matches!(outcome, PassRunOutcome::AlreadyInFlight { .. }));

        drop(guard);

        // After releasing the gate the next call completes normally.
        let outcome = runner.run_pass(IndexMode::Incremental).await.unwrap();
        assert!(matches!(outcome, PassRunOutcome::Completed(_)));
    }

    #[tokio::test]
    async fn run_pass_records_history_for_completed_pass() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);

        let outcome = runner.run_pass(IndexMode::Incremental).await.unwrap();
        assert!(matches!(outcome, PassRunOutcome::Completed(_)));

        let recent = runner.recent_passes().await;
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].mode, "incremental");
        assert_eq!(recent[0].target_count, 0);
    }

    #[tokio::test]
    async fn snapshot_state_is_none_outside_pass() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);
        assert!(runner.snapshot_state().await.is_none());
        runner.run_pass(IndexMode::Incremental).await.unwrap();
        assert!(runner.snapshot_state().await.is_none());
    }

    #[tokio::test]
    async fn start_pass_for_project_returns_started_then_completes() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(make_runner(&tmp, store.clone()));

        let project = project_with_repos("allan", "alpha", &[]);
        store.put(&project).unwrap();

        let outcome = runner
            .start_pass_for_project("allan", "alpha", IndexMode::Incremental)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PassRunOutcome::Started { .. }),
            "expected Started, got {outcome:?}"
        );

        // Wait for the background task to record into history and free the gate.
        let mut waited = 0;
        loop {
            let recorded = !runner.recent_passes().await.is_empty();
            let gate_free = Arc::clone(&runner.in_flight).try_lock_owned().is_ok();
            if recorded && gate_free {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            waited += 1;
            // Generous ceiling (~10s): a real empty pass settles in <1ms; only
            // a genuine hang should ever trip this, even under full-suite load.
            assert!(waited < 1000, "background project pass never settled");
        }
        assert_eq!(runner.recent_passes().await.len(), 1);
    }

    #[tokio::test]
    async fn start_pass_for_project_errors_before_gate_on_missing_project() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(make_runner(&tmp, store));

        // Missing project errors out, and crucially does NOT leave the gate held.
        let err = runner
            .start_pass_for_project("allan", "nope", IndexMode::Incremental)
            .await;
        assert!(err.is_err(), "missing project should error");
        assert!(
            Arc::clone(&runner.in_flight).try_lock_owned().is_ok(),
            "gate must remain free after a pre-gate error"
        );
    }

    #[tokio::test]
    async fn recent_failures_filters_to_non_indexed_outcomes() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);

        // Seed history directly to keep the test focused on the filter
        // behaviour rather than coupling to the indexer's failure paths.
        runner.history.write().await.push(PassReport {
            mode: "incremental",
            target_count: 2,
            duration_ms: 10,
            targets: vec![
                TargetReport {
                    id: "repo:ok".into(),
                    outcome: TargetOutcome::Indexed {
                        files_indexed: 1,
                        entities_created: 2,
                        relationships_created: 3,
                        duration_ms: 4,
                    },
                },
                TargetReport {
                    id: "repo:bad".into(),
                    outcome: TargetOutcome::PullFailed {
                        error: "auth".into(),
                    },
                },
            ],
        });
        let failures = runner.recent_failures().await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].target_id, "repo:bad");
        match &failures[0].outcome {
            TargetOutcome::PullFailed { error } => assert_eq!(error, "auth"),
            _ => panic!("expected PullFailed"),
        }
    }

    #[tokio::test]
    async fn pass_state_guard_disarm_skips_drop_clear() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);

        // Install via a fake call to run_under_guard semantics: take the
        // mutex, install a state, drop the disarmed guard, manually clear.
        let lock = runner.in_flight.try_lock().expect("uncontended");
        let g = PassStateGuard::install(&runner.state, &PassScope::Hub, IndexMode::Full, &[]).await;
        assert!(runner.state.read().await.is_some());
        g.disarm();
        // Snapshot still populated because we didn't drop yet; clear
        // synchronously the same way run_under_guard does.
        *runner.state.write().await = None;
        assert!(runner.state.read().await.is_none());
        drop(lock);
    }

    fn project_with_repos(agent: &str, name: &str, repos: &[(&str, &str, &str)]) -> Project {
        let mut p = Project::new(agent, name);
        for (alias, owner, repo) in repos {
            p.repos.push(ProjectRepo {
                alias: (*alias).into(),
                source: RepoSource::Github {
                    owner: (*owner).into(),
                    repo: (*repo).into(),
                },
                default_branch: None,
                setup: None,
            });
        }
        p
    }

    #[test]
    fn enumerate_project_targets_filters_to_named_project() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos("allan", "alpha", &[("x", "acme", "x")]))
            .unwrap();
        store
            .put(&project_with_repos("allan", "beta", &[("y", "acme", "y")]))
            .unwrap();

        let project = store.get("allan", "alpha").unwrap().unwrap();
        let runner = make_runner(&tmp, Arc::clone(&store));
        let targets = runner.enumerate_project_targets(&project);
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["repo:acme/x"]);
        assert_eq!(targets[0].agent_ids, vec!["allan".to_string()]);
    }

    #[test]
    fn enumerate_project_targets_dedup_within_project() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos(
                "allan",
                "alpha",
                &[("one", "acme", "same"), ("another-name", "acme", "same")],
            ))
            .unwrap();

        let project = store.get("allan", "alpha").unwrap().unwrap();
        let runner = make_runner(&tmp, Arc::clone(&store));
        let targets = runner.enumerate_project_targets(&project);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "repo:acme/same");
    }

    #[test]
    fn find_project_returns_match_for_known_name() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store.put(&Project::new("allan", "alpha")).unwrap();

        let runner = make_runner(&tmp, store);
        let p = runner.find_project("allan", "alpha").unwrap();
        assert_eq!(p.name, "alpha");
    }

    #[test]
    fn find_project_errors_with_useful_message_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);

        let err = runner.find_project("allan", "missing").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"), "missing project name: {msg}");
        assert!(msg.contains("allan"), "agent id in message: {msg}");
        assert!(msg.contains("projects.list"), "next-step hint: {msg}");
    }

    #[test]
    fn target_outcome_serialises_with_outcome_tag() {
        let r = TargetReport {
            id: "repo:acme/widgets".into(),
            outcome: TargetOutcome::PullFailed {
                error: "auth".into(),
            },
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"], "pull_failed");
        assert_eq!(v["error"], "auth");
    }

    #[test]
    fn pass_report_serialises_round_trip() {
        let p = PassReport {
            mode: "full",
            target_count: 1,
            duration_ms: 42,
            targets: vec![TargetReport {
                id: "repo:acme/widgets".into(),
                outcome: TargetOutcome::Indexed {
                    files_indexed: 10,
                    entities_created: 50,
                    relationships_created: 100,
                    duration_ms: 30,
                },
            }],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"mode\":\"full\""));
        assert!(s.contains("\"outcome\":\"indexed\""));
    }

    #[test]
    fn catalog_resolves_by_slug_repo_name_and_single_repo_project() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos(
                "allan",
                "harbor",
                &[("rails", "harborlabs", "HarborRails")],
            ))
            .unwrap();
        let catalog =
            RepoCatalog::build(&tmp.path().join("ws"), tmp.path(), store.as_ref()).unwrap();

        let by_slug = catalog.resolve("harborlabs/HarborRails").unwrap();
        assert_eq!(catalog.resolve("HarborRails").unwrap(), by_slug);
        assert_eq!(catalog.resolve("harbor").unwrap(), by_slug);
        assert_eq!(catalog.resolve("Harborlabs/HarborRails").unwrap(), by_slug);
        assert!(catalog.canonical_repo_ids().contains(&by_slug));
    }

    #[test]
    fn catalog_resolve_errors_are_actionable() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos(
                "allan",
                "multi",
                &[("a", "acme", "a"), ("b", "acme", "b")],
            ))
            .unwrap();
        let catalog =
            RepoCatalog::build(&tmp.path().join("ws"), tmp.path(), store.as_ref()).unwrap();

        assert!(catalog.resolve("ghost").unwrap_err().contains("unknown"));
        assert!(catalog.resolve("multi").unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn catalog_sole_only_when_exactly_one_repo() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos("allan", "one", &[("x", "acme", "x")]))
            .unwrap();
        assert!(
            RepoCatalog::build(&tmp.path().join("ws"), tmp.path(), store.as_ref())
                .unwrap()
                .sole()
                .is_some()
        );

        store
            .put(&project_with_repos("allan", "two", &[("y", "acme", "y")]))
            .unwrap();
        assert!(
            RepoCatalog::build(&tmp.path().join("ws"), tmp.path(), store.as_ref())
                .unwrap()
                .sole()
                .is_none()
        );
    }

    #[test]
    fn resolver_defaults_to_sole_and_errors_when_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos("allan", "p", &[("x", "acme", "x")]))
            .unwrap();
        let resolver = RepoResolver::new(
            Arc::clone(&store),
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
        );
        assert_eq!(
            resolver.resolve(None).unwrap(),
            resolver.resolve(Some("x")).unwrap()
        );
        assert!(resolver
            .resolve_dest(Some("x"))
            .unwrap()
            .ends_with("acme/x"));

        store
            .put(&project_with_repos("allan", "q", &[("y", "acme", "y")]))
            .unwrap();
        let resolver = RepoResolver::new(
            Arc::clone(&store),
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
        );
        assert!(resolver
            .resolve(None)
            .unwrap_err()
            .contains("multiple repos"));
    }

    #[test]
    fn resolve_query_repo_prefers_resolver_then_service_root() {
        let tmp = TempDir::new().unwrap();
        let ks = KnowledgeService::in_memory(tmp.path().to_path_buf()).unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos("allan", "p", &[("x", "acme", "x")]))
            .unwrap();
        let resolver = RepoResolver::new(
            Arc::clone(&store),
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
        );

        let via_resolver =
            resolve_query_repo(&ks, tmp.path(), Some(&resolver), &Some("x".to_string())).unwrap();
        assert_eq!(via_resolver, resolver.resolve(Some("x")).unwrap());

        let via_service = resolve_query_repo(&ks, tmp.path(), None, &None).unwrap();
        assert_eq!(via_service, ks.repo_id());
    }

    #[tokio::test]
    async fn prune_orphans_drops_only_non_canonical_rows() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store
            .put(&project_with_repos("allan", "p", &[("x", "acme", "x")]))
            .unwrap();
        let runner = make_runner(&tmp, Arc::clone(&store));

        let canonical_id = repo_id_of(&runner.enumerate_targets().unwrap()[0]);
        runner
            .knowledge
            .graph()
            .register_repo(&canonical_id, "x", "/canonical")
            .await
            .unwrap();
        runner
            .knowledge
            .graph()
            .register_repo("orphan-id", "old", "/legacy/path")
            .await
            .unwrap();

        assert_eq!(
            runner.prune_orphans().await.unwrap(),
            vec!["orphan-id".to_string()]
        );

        let remaining: Vec<String> = runner
            .knowledge
            .graph()
            .list_repositories()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(remaining.contains(&canonical_id));
        assert!(!remaining.contains(&"orphan-id".to_string()));
    }

    #[tokio::test]
    async fn prune_orphans_is_noop_when_no_targets() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = make_runner(&tmp, store);
        runner
            .knowledge
            .graph()
            .register_repo("some-id", "x", "/x")
            .await
            .unwrap();

        assert!(runner.prune_orphans().await.unwrap().is_empty());
        assert_eq!(
            runner
                .knowledge
                .graph()
                .list_repositories()
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

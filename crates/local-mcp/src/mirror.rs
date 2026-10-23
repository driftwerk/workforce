//! Auto-pull mirror manager.
//!
//! Owns a pristine clone of every mirror-mode repo across every project and
//! keeps it current via `git fetch` + `git reset --hard origin/<branch>`. A
//! single background task wakes every [`TICK_INTERVAL`] and fires any
//! `(project, repo)` whose effective schedule is both active right now and
//! overdue since its last successful pull. Per-repo single-flight ensures a
//! slow pull on the previous tick doesn't queue up another one.
//!
//! State (last pulled / indexed timestamps, last error) is in-memory only —
//! on restart, every repo is "stale" and waits for the next scheduled tick
//! per the user's chosen boot policy. `project.sync` is the manual override
//! that bypasses the schedule.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Timelike, Utc};
use dashmap::DashMap;
use eyre::{eyre, Result, WrapErr};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{info, warn};
use workforce_knowledge_service::{
    EffectiveSchedule, KnowledgeService, ProjectConfig, ReindexMode, RepoConfig, RepoSource,
};

/// How often the scheduler wakes to re-evaluate every repo. 60s is fine
/// granularity for the smallest practical interval (1m); finer than 30s
/// gains nothing and burns more CPU.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Key for the per-repo state map: `<project>/<repo>`.
fn repo_key(project: &str, repo: &str) -> String {
    format!("{project}/{repo}")
}

/// In-memory per-repo state. Cheap to clone for snapshotting.
#[derive(Debug, Clone, Default)]
pub struct RepoMirrorState {
    pub last_pulled_at: Option<DateTime<Utc>>,
    pub last_indexed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

pub struct MirrorManager {
    knowledge: Arc<KnowledgeService>,
    projects_dir: PathBuf,
    data_dir: PathBuf,
    /// Per-repo state keyed by `<project>/<repo>`.
    state: DashMap<String, RepoMirrorState>,
    /// Per-repo single-flight mutex keyed by `<project>/<repo>`. Held for
    /// the duration of a sync (clone/fetch/reset/index) so a slow tick can't
    /// be triggered twice in parallel.
    locks: DashMap<String, Arc<Mutex<()>>>,
    /// Pluggable git runner for tests; production uses the real `git` CLI.
    runner: Arc<dyn GitRunner>,
}

impl MirrorManager {
    /// Production constructor: shells out to the installed `git` binary.
    pub fn new(
        knowledge: Arc<KnowledgeService>,
        projects_dir: PathBuf,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        Self::with_runner(knowledge, projects_dir, data_dir, Arc::new(SystemGitRunner))
    }

    /// Test/DI constructor.
    pub fn with_runner(
        knowledge: Arc<KnowledgeService>,
        projects_dir: PathBuf,
        data_dir: PathBuf,
        runner: Arc<dyn GitRunner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            knowledge,
            projects_dir,
            data_dir,
            state: DashMap::new(),
            locks: DashMap::new(),
            runner,
        })
    }

    /// Spawn the scheduler tick loop. Caller drops the returned handle on
    /// shutdown; for the long-lived MCP server we just leak it.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = time::interval(TICK_INTERVAL);
            // Skip the immediate-fire that `interval` does — boot policy is
            // "wait for next scheduled tick", not "pull at startup".
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = this.tick(Utc::now()).await {
                    warn!(error = %e, "mirror tick failed");
                }
            }
        })
    }

    /// One iteration of the scheduler — re-reads project configs from disk
    /// (so editing a TOML doesn't require a restart) and fires any repo
    /// whose schedule says it's due.
    async fn tick(self: &Arc<Self>, now: DateTime<Utc>) -> Result<()> {
        let projects = ProjectConfig::load_all_from_dir(&self.projects_dir)?;
        let local_now = now.with_timezone(&Local);
        let weekday_idx = local_now.weekday().num_days_from_monday();
        let hour = local_now.hour();

        for project in projects {
            for repo in project.repos.clone() {
                let Some(schedule) = repo.effective_schedule(&project)? else {
                    continue;
                };
                if matches!(schedule.reindex, ReindexMode::Off) {
                    continue;
                }
                if !schedule.is_active_at(weekday_idx, hour) {
                    continue;
                }
                if !self.is_due(&project.name, &repo.name, &schedule, now) {
                    continue;
                }
                let this = Arc::clone(self);
                let project = project.clone();
                let repo = repo.clone();
                let schedule = schedule.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.sync_repo(&project, &repo, schedule.reindex).await {
                        warn!(
                            project = %project.name,
                            repo = %repo.name,
                            error = %e,
                            "scheduled sync failed",
                        );
                    }
                    // Suppress unused warning for the rare case where the
                    // schedule only contributes the reindex mode.
                    let _ = schedule;
                });
            }
        }
        Ok(())
    }

    fn is_due(
        &self,
        project_name: &str,
        repo_name: &str,
        schedule: &EffectiveSchedule,
        now: DateTime<Utc>,
    ) -> bool {
        let key = repo_key(project_name, repo_name);
        match self.state.get(&key).and_then(|s| s.last_pulled_at) {
            None => true,
            Some(last) => {
                let elapsed = now.signed_duration_since(last);
                elapsed.to_std().unwrap_or(Duration::ZERO) >= schedule.every
            }
        }
    }

    fn lock_for(&self, project: &str, repo: &str) -> Arc<Mutex<()>> {
        let key = repo_key(project, repo);
        self.locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Public sync entry point. Used by the scheduler and the `project.sync`
    /// MCP tool. Acquires the per-repo lock so concurrent calls serialise
    /// rather than racing on the working tree.
    pub async fn sync_repo(
        self: &Arc<Self>,
        project: &ProjectConfig,
        repo: &RepoConfig,
        reindex: ReindexMode,
    ) -> Result<()> {
        if !repo.is_mirror() {
            return Err(eyre!(
                "repo \"{}\" is in legacy `path` mode and cannot be synced",
                repo.name
            ));
        }
        let lock = self.lock_for(&project.name, &repo.name);
        let _guard = lock.lock().await;

        let path = repo.effective_path(project, &self.data_dir);
        let branch = repo.effective_default_branch().to_string();
        let source = repo
            .source
            .as_ref()
            .ok_or_else(|| eyre!("mirror repo \"{}\" has no source", repo.name))?;

        let pull_result = self.pull_or_clone(&path, source, &branch).await;
        let mut indexed = false;
        let outcome: Result<()> = match pull_result {
            Ok(()) => {
                let index_result = match reindex {
                    ReindexMode::Off => Ok(()),
                    ReindexMode::Incremental => self
                        .knowledge
                        .trigger_incremental_index_for(&path)
                        .await
                        .map(|_| ()),
                    ReindexMode::Full => self
                        .knowledge
                        .trigger_full_index_for(&path)
                        .await
                        .map(|_| ()),
                };
                if index_result.is_ok() && !matches!(reindex, ReindexMode::Off) {
                    indexed = true;
                }
                index_result.wrap_err("indexing after pull")
            }
            Err(e) => Err(e.wrap_err("pulling mirror")),
        };

        let key = repo_key(&project.name, &repo.name);
        let mut state = self.state.entry(key).or_default();
        let now = Utc::now();
        match &outcome {
            Ok(()) => {
                state.last_pulled_at = Some(now);
                if indexed {
                    state.last_indexed_at = Some(now);
                }
                state.last_error = None;
            }
            Err(e) => {
                state.last_error = Some(format!("{e:#}"));
            }
        }
        outcome
    }

    /// Force-sync every mirror-mode repo in `project`, regardless of schedule.
    /// Used by `project.sync`.
    pub async fn force_sync_project(
        self: &Arc<Self>,
        project: &ProjectConfig,
    ) -> Vec<(String, Result<()>)> {
        let mut results = Vec::new();
        for repo in &project.repos {
            if !repo.is_mirror() {
                continue;
            }
            // For force-sync the user explicitly asked: always reindex
            // incremental, regardless of `reindex = "off"` in the schedule.
            let r = self
                .sync_repo(project, repo, ReindexMode::Incremental)
                .await;
            results.push((repo.name.clone(), r));
        }
        results
    }

    /// Look up a single repo's state.
    pub fn state_for(&self, project: &str, repo: &str) -> Option<RepoMirrorState> {
        self.state.get(&repo_key(project, repo)).map(|s| s.clone())
    }

    async fn pull_or_clone(&self, path: &Path, source: &RepoSource, branch: &str) -> Result<()> {
        let exists = path.join(".git").is_dir();
        if !exists {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .wrap_err_with(|| format!("creating parent of {}", path.display()))?;
            }
            let url = source.clone_url();
            info!(path = %path.display(), url = %url, "cloning mirror");
            self.runner
                .run(
                    Path::new("."),
                    &[
                        "clone",
                        "--branch",
                        branch,
                        "--single-branch",
                        &url,
                        path.to_str().ok_or_else(|| eyre!("non-utf8 mirror path"))?,
                    ],
                )
                .await
                .wrap_err("git clone")?;
            return Ok(());
        }

        // Existing clone: fetch + hard-reset to upstream + drop untracked.
        self.runner
            .run(path, &["fetch", "--prune", "origin"])
            .await
            .wrap_err("git fetch")?;
        self.runner
            .run(path, &["reset", "--hard", &format!("origin/{branch}")])
            .await
            .wrap_err("git reset")?;
        self.runner
            .run(path, &["clean", "-fdx"])
            .await
            .wrap_err("git clean")?;
        Ok(())
    }
}

/// Pluggable git runner — production uses the real CLI; tests inject a fake
/// to drive `pull_or_clone` without network or fork overhead.
#[async_trait::async_trait]
pub trait GitRunner: Send + Sync {
    async fn run(&self, cwd: &Path, args: &[&str]) -> Result<()>;
}

struct SystemGitRunner;

#[async_trait::async_trait]
impl GitRunner for SystemGitRunner {
    async fn run(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .await
            .wrap_err_with(|| format!("spawning git {}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every git invocation in order and reports per-arg counts so
    /// tests can assert without parsing log output.
    struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
        fail_on: Vec<String>,
        post_clone_init: AtomicUsize,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_on: Vec::new(),
                post_clone_init: AtomicUsize::new(0),
            }
        }

        async fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl GitRunner for RecordingRunner {
        async fn run(&self, cwd: &Path, args: &[&str]) -> Result<()> {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let argline = owned.join(" ");
            self.calls.lock().await.push(owned.clone());
            if self.fail_on.iter().any(|f| argline.contains(f)) {
                return Err(eyre!("recording runner: forced failure on {argline}"));
            }
            // Simulate "git clone <url> <dest>" by creating <dest>/.git so a
            // subsequent existence check returns true.
            if owned.first().map(|s| s.as_str()) == Some("clone") {
                let dest = owned.last().expect("clone args end with destination");
                let dest = Path::new(dest);
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                self.post_clone_init.fetch_add(1, Ordering::SeqCst);
            }
            // For non-clone calls, validate the cwd is a "repo".
            if owned.first().map(|s| s.as_str()) != Some("clone") && !cwd.join(".git").exists() {
                return Err(eyre!("recording runner: {} is not a repo", cwd.display()));
            }
            Ok(())
        }
    }

    fn project_with_one_mirror(mirror_root: &Path) -> ProjectConfig {
        ProjectConfig {
            name: "demo".into(),
            mirror_root: Some(mirror_root.to_path_buf()),
            schedule: Some(workforce_knowledge_service::ScheduleSpec {
                every: Some("15m".into()),
                ..Default::default()
            }),
            repos: vec![RepoConfig {
                name: "rails".into(),
                path: None,
                source: Some(RepoSource::Github {
                    owner: "harborlabs".into(),
                    repo: "HarborRails".into(),
                }),
                default_branch: None,
                schedule: None,
            }],
        }
    }

    fn manager_with(runner: Arc<dyn GitRunner>, data_dir: &Path) -> Arc<MirrorManager> {
        // The mirror manager uses KnowledgeService only for indexing; for
        // these unit tests we never reach that path because we set
        // `reindex = "off"`. We can construct a real KnowledgeService
        // pointing at the tempdir.
        let workspace = data_dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let ks = KnowledgeService::new(
            workspace,
            data_dir.to_path_buf(),
            &workforce_common::settings::KnowledgeConfig::default(),
            &workforce_common::settings::WeaviateConfig::default(),
            None,
        )
        .expect("KnowledgeService for tests");
        MirrorManager::with_runner(
            Arc::new(ks),
            data_dir.to_path_buf(),
            data_dir.to_path_buf(),
            runner,
        )
    }

    #[tokio::test]
    async fn sync_repo_clones_then_resets_on_subsequent_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner.clone(), tmp.path());
        let project = project_with_one_mirror(tmp.path());

        // First sync → clone path.
        mgr.sync_repo(&project, &project.repos[0], ReindexMode::Off)
            .await
            .unwrap();
        let calls = runner.calls().await;
        assert_eq!(calls[0][0], "clone");
        assert!(
            calls[0].iter().any(|s| s == "main"),
            "default branch should be \"main\""
        );

        // Second sync → fetch/reset/clean.
        mgr.sync_repo(&project, &project.repos[0], ReindexMode::Off)
            .await
            .unwrap();
        let calls = runner.calls().await;
        let later: Vec<_> = calls.iter().skip(1).map(|c| c[0].as_str()).collect();
        assert_eq!(later, vec!["fetch", "reset", "clean"]);
    }

    #[tokio::test]
    async fn sync_repo_marks_state_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner, tmp.path());
        let project = project_with_one_mirror(tmp.path());

        mgr.sync_repo(&project, &project.repos[0], ReindexMode::Off)
            .await
            .unwrap();
        let snap = mgr.state_for("demo", "rails").expect("state recorded");
        assert!(snap.last_pulled_at.is_some());
        assert!(snap.last_error.is_none());
        // reindex=off, so last_indexed_at remains None.
        assert!(snap.last_indexed_at.is_none());
    }

    #[tokio::test]
    async fn sync_repo_records_error_on_pull_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = RecordingRunner::new();
        runner.fail_on = vec!["clone".into()];
        let mgr = manager_with(Arc::new(runner), tmp.path());
        let project = project_with_one_mirror(tmp.path());

        let err = mgr
            .sync_repo(&project, &project.repos[0], ReindexMode::Off)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("forced failure"));
        let snap = mgr.state_for("demo", "rails").expect("state recorded");
        assert!(snap.last_error.is_some());
        assert!(snap.last_pulled_at.is_none());
    }

    #[tokio::test]
    async fn sync_repo_rejects_legacy_path_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner, tmp.path());
        let project = ProjectConfig {
            name: "demo".into(),
            mirror_root: None,
            schedule: None,
            repos: vec![RepoConfig {
                name: "x".into(),
                path: Some(tmp.path().join("x")),
                source: None,
                default_branch: None,
                schedule: None,
            }],
        };
        let err = mgr
            .sync_repo(&project, &project.repos[0], ReindexMode::Off)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("legacy `path` mode"));
    }

    #[tokio::test]
    async fn force_sync_project_skips_legacy_repos_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner.clone(), tmp.path());
        let project = ProjectConfig {
            name: "mixed".into(),
            mirror_root: Some(tmp.path().to_path_buf()),
            schedule: Some(workforce_knowledge_service::ScheduleSpec {
                every: Some("15m".into()),
                ..Default::default()
            }),
            repos: vec![
                RepoConfig {
                    name: "legacy".into(),
                    path: Some(tmp.path().join("legacy")),
                    source: None,
                    default_branch: None,
                    schedule: None,
                },
                RepoConfig {
                    name: "mirror".into(),
                    path: None,
                    source: Some(RepoSource::Github {
                        owner: "a".into(),
                        repo: "b".into(),
                    }),
                    default_branch: None,
                    schedule: None,
                },
            ],
        };
        let results = mgr.force_sync_project(&project).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "mirror");
        assert!(results[0].1.is_ok());
    }

    #[test]
    fn is_due_fires_when_never_pulled() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Arc<dyn GitRunner> = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner, tmp.path());
        let schedule = workforce_knowledge_service::ScheduleSpec {
            every: Some("15m".into()),
            ..Default::default()
        }
        .into_effective()
        .unwrap()
        .unwrap();
        assert!(mgr.is_due("p", "r", &schedule, Utc::now()));
    }

    #[test]
    fn is_due_respects_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Arc<dyn GitRunner> = Arc::new(RecordingRunner::new());
        let mgr = manager_with(runner, tmp.path());
        let now = Utc::now();
        mgr.state.insert(
            repo_key("p", "r"),
            RepoMirrorState {
                last_pulled_at: Some(now - chrono::Duration::seconds(60)),
                ..RepoMirrorState::default()
            },
        );
        let schedule = workforce_knowledge_service::ScheduleSpec {
            every: Some("15m".into()),
            ..Default::default()
        }
        .into_effective()
        .unwrap()
        .unwrap();
        assert!(!mgr.is_due("p", "r", &schedule, now));
        // Advance clock past the interval.
        let later = now + chrono::Duration::seconds(901);
        assert!(mgr.is_due("p", "r", &schedule, later));
    }
}

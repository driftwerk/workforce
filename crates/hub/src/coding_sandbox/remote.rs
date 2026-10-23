//! The `[exec.remote]` worker as the backend: a workspace is a scope the
//! worker prepares from its golden checkouts, and every round is a `claude`
//! dispatched into it with its hub tool calls tunnelled back over the same
//! connection and answered here.

#[cfg(test)]
use workforce_agent::tools::exec_runner::protocol::PROTOCOL_VERSION;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{bail, eyre, Result};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workforce_agent::tools::exec_runner::{
    dispatch, observe_protocol, run_or_dispatch, EnvMode, ExecOutcome, ExecRequest, Outcome,
    PrepareRequest, Prepared, ProtocolEvent, RefusalReason, RepoSpec, Request, RunnerEndpoint,
    ScopeId, ServerFrame, WorkerHealth, MAX_FRAME_LEN, MAX_OUTPUT_SIZE, SCOPE_HOME,
    SCOPE_WORK_ROOT,
};
use workforce_projects::{Project, RepoSource};

use super::{
    head_utf8, sandbox_path, AtCapacity, CodingSandbox, DirEntry, EntryKind, McpSurface,
    SandboxCommand, SandboxLauncher, WorkspaceSpec,
};
use crate::claude_code::RemotePlacement;
pub use crate::hub_core::pipelines::{ProtocolRefusal, WorkerFacts};
use crate::projects::setup::SetupReport;

/// The most a single write may carry: one request frame, less room for the
/// command around it.
pub const MAX_WRITE_BYTES: usize = MAX_FRAME_LEN - 64 * 1024;

/// Exit code the read helper uses to say "no such file", distinct from any
/// failure of `head` itself.
const READ_MISSING: i32 = 66;

pub struct RemoteLauncher {
    endpoint: RunnerEndpoint,
    ttl_secs: u64,
    mcp: McpSurface,
}

impl RemoteLauncher {
    pub fn new(endpoint: RunnerEndpoint, ttl_secs: u64, mcp: McpSurface) -> Self {
        Self {
            endpoint,
            ttl_secs,
            mcp,
        }
    }

    async fn request(&self, request: Request) -> Result<Outcome> {
        dispatch(
            &self.endpoint,
            request,
            drained_sink(),
            CancellationToken::new(),
            None,
        )
        .await
    }
}

/// The worker's name for a workspace: dataset-safe, at most 64 characters, and
/// the same for the same (agent, project, scope) every time, so a round finds
/// the scope its predecessor prepared.
pub fn remote_scope(agent_id: &str, project: &str, scope: &str) -> ScopeId {
    let mut hasher = Sha256::new();
    for part in [agent_id, project, scope] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hex::encode(hasher.finalize());
    let readable: String = scope
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(20)
        .collect();
    let id = [readable.trim_matches('-'), &digest[..24]]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    ScopeId::new(id)
}

/// What the worker checks out for `project`, with each repo's token.
fn repo_specs(project: &Project, tokens: &HashMap<String, String>) -> Vec<RepoSpec> {
    project
        .repos
        .iter()
        .map(|repo| {
            let RepoSource::Github { owner, repo: name } = &repo.source;
            RepoSpec {
                alias: repo.alias.clone(),
                slug: format!("{owner}/{name}"),
                default_branch: repo.default_branch.clone().unwrap_or_default(),
                setup: project.effective_setup(&repo.alias).map(str::to_string),
                token: tokens.get(owner).cloned(),
            }
        })
        .collect()
}

/// Where rounds start in a prepared scope: the `primary` repo's checkout when
/// one is named, else the worker's own choice.
fn start_dir(prepared: &Prepared, primary: Option<&str>) -> Result<String> {
    let Some(alias) = primary else {
        return Ok(prepared.workdir.clone());
    };
    prepared
        .repos
        .iter()
        .find(|(name, _)| name == alias)
        .map(|(_, path)| path.clone())
        .ok_or_else(|| {
            let held: Vec<&str> = prepared
                .repos
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            eyre!(
                "the coding worker's workspace has no repo `{alias}` to start in (it holds: {})",
                held.join(", ")
            )
        })
}

/// The repos `project` names that a prepared scope does not hold: a scope the
/// worker found is handed back as it was first prepared.
fn absent_repos<'a>(prepared: &Prepared, project: &'a Project) -> Vec<&'a str> {
    project
        .repos
        .iter()
        .map(|repo| repo.alias.as_str())
        .filter(|alias| !prepared.repos.iter().any(|(name, _)| name == alias))
        .collect()
}

/// A sink that keeps the connection's heartbeats flowing for a request whose
/// only interesting frame is the terminal one.
fn drained_sink() -> mpsc::Sender<ServerFrame> {
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tx
}

#[async_trait]
impl SandboxLauncher for RemoteLauncher {
    async fn ensure(&self, spec: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
        let scope = remote_scope(spec.agent_id, &spec.project.name, spec.scope);
        let request = Request::Prepare(PrepareRequest {
            scope: scope.clone(),
            repos: repo_specs(spec.project, spec.tokens),
            ttl_secs: self.ttl_secs,
            env: Vec::new(),
            home: spec.home.clone(),
        });
        let prepared = match self.request(request).await? {
            Outcome::Prepared(prepared) => prepared,
            Outcome::Refused(RefusalReason::AtCapacity) => return Err(AtCapacity.into()),
            Outcome::Refused(reason) => {
                bail!("the coding worker refused to prepare a workspace: {reason}")
            }
            other => bail!("the coding worker answered a prepare with {other:?}"),
        };
        let workdir = start_dir(&prepared, spec.primary)?;
        let absent = absent_repos(&prepared, spec.project);
        if !absent.is_empty() {
            warn!(
                scope = %scope,
                absent = %absent.join(", "),
                "the coding worker handed back a workspace prepared without repos this round names; they stay absent until it is released"
            );
        }
        info!(
            scope = %scope,
            workdir = %workdir,
            reused = prepared.reused,
            home_reused = prepared.home_reused,
            "remote coding workspace ready"
        );
        Ok(Arc::new(RemoteSandbox {
            endpoint: self.endpoint.clone(),
            scope,
            workdir: PathBuf::from(workdir),
            setup: prepared.setup,
            fresh: !prepared.reused && !prepared.home_reused,
            hub_tools: self.mcp.hub_tools(),
            jobs: prepared.jobs,
        }))
    }

    /// The worker recreates a reclaimed scope on the next prepare; a transcript
    /// lost with it surfaces as a session the round cannot resume.
    async fn exists(&self, _agent_id: &str, _project: &str, _scope: &str) -> bool {
        true
    }

    async fn close(&self, agent_id: &str, project: &str, scope: &str) -> Result<()> {
        let scope = remote_scope(agent_id, project, scope);
        match self.request(Request::Release { scope }).await? {
            Outcome::Done(_) | Outcome::Refused(RefusalReason::UnknownScope) => Ok(()),
            Outcome::Refused(reason) => {
                bail!("the coding worker refused to release the workspace: {reason}")
            }
            other => bail!("the coding worker answered a release with {other:?}"),
        }
    }

    async fn end_job(
        &self,
        agent_id: &str,
        project: &str,
        scope: &str,
        job: &workforce_agent::tools::exec_runner::JobId,
    ) {
        let scope = remote_scope(agent_id, project, scope);
        workforce_agent::tools::exec_runner::cancel_job(&self.endpoint, job, &scope).await;
    }

    /// The worker reclaims idle scopes itself once their lifetime passes.
    async fn sweep(
        &self,
        _agent_id: &str,
        _project: &str,
        _prefix: &str,
        _live: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
    ) {
    }
}

struct RemoteSandbox {
    endpoint: RunnerEndpoint,
    scope: ScopeId,
    workdir: PathBuf,
    setup: Vec<SetupReport>,
    fresh: bool,
    hub_tools: Vec<String>,
    jobs: bool,
}

impl RemoteSandbox {
    fn path(&self, path: &Path) -> Result<PathBuf> {
        sandbox_path(
            &self.workdir,
            path,
            &[Path::new(SCOPE_WORK_ROOT), Path::new(SCOPE_HOME)],
        )
    }

    /// A shell snippet run inside the scope with `$1..` bound to `args`.
    async fn shell(
        &self,
        script: &str,
        args: &[String],
        stdin: Option<String>,
    ) -> Result<ExecOutcome> {
        let args: Vec<String> = ["-c", script, "sh"]
            .into_iter()
            .map(str::to_string)
            .chain(args.iter().cloned())
            .collect();
        let out = self
            .exec(SandboxCommand {
                stdin,
                ..SandboxCommand::new("sh", args)
            })
            .await?;
        if out.summary.truncated {
            bail!("output exceeds the {MAX_OUTPUT_SIZE} byte cap of one command");
        }
        Ok(out)
    }
}

#[async_trait]
impl CodingSandbox for RemoteSandbox {
    fn workdir(&self) -> &Path {
        &self.workdir
    }

    fn setup(&self) -> &[SetupReport] {
        &self.setup
    }

    fn fresh(&self) -> bool {
        self.fresh
    }

    async fn exec(&self, command: SandboxCommand) -> Result<ExecOutcome> {
        let cwd = match &command.cwd {
            Some(cwd) => self.path(cwd)?,
            None => self.workdir.clone(),
        };
        let request = ExecRequest {
            program: command.program,
            args: command.args,
            workdir: Some(cwd.display().to_string()),
            timeout_secs: command.timeout.as_secs(),
            allow_retry: false,
            env: command.env,
            stdin: command.stdin,
            scope: Some(self.scope.clone()),
            mcp: None,
        };
        run_or_dispatch(Some(&self.endpoint), request, &[], EnvMode::Scrubbed).await
    }

    /// Fetches three bytes past `max_bytes` so a character cut at the limit
    /// arrives whole and the text is trimmed at a boundary, as locally.
    async fn read(&self, path: &Path, max_bytes: usize) -> Result<Option<String>> {
        let path = self.path(path)?;
        let out = self
            .shell(
                &format!("test -e \"$1\" || exit {READ_MISSING}; head -c \"$2\" -- \"$1\""),
                &[path.display().to_string(), (max_bytes + 3).to_string()],
                None,
            )
            .await?;
        match out.exit_code() {
            0 => Ok(Some(head_utf8(out.stdout.as_bytes(), max_bytes))),
            READ_MISSING => Ok(None),
            code => bail!(
                "reading {} failed ({code}): {}",
                path.display(),
                out.stderr.trim()
            ),
        }
    }

    async fn write(&self, path: &Path, content: &str) -> Result<()> {
        let path = self.path(path)?;
        if content.len() > MAX_WRITE_BYTES {
            bail!(
                "writing {}: {} bytes exceeds the {MAX_WRITE_BYTES} byte limit of one write",
                path.display(),
                content.len()
            );
        }
        let out = self
            .shell(
                "mkdir -p \"$(dirname \"$1\")\" && cat > \"$1\"",
                &[path.display().to_string()],
                Some(content.to_string()),
            )
            .await?;
        if out.success() {
            Ok(())
        } else {
            bail!(
                "writing {} failed ({}): {}",
                path.display(),
                out.exit_code(),
                out.stderr.trim()
            )
        }
    }

    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let path = self.path(path)?;
        let out = self
            .shell(
                concat!(
                    "[ -d \"$1\" ] || exit 1; ",
                    "for f in \"$1\"/* \"$1\"/.[!.]* \"$1\"/..?*; do ",
                    "[ -e \"$f\" ] || [ -L \"$f\" ] || continue; ",
                    "if [ -d \"$f\" ]; then k=d; elif [ -f \"$f\" ]; then k=f; else k=o; fi; ",
                    "printf '%s\\t%s\\n' \"$k\" \"${f##*/}\"; done",
                ),
                &[path.display().to_string()],
                None,
            )
            .await?;
        if !out.success() {
            bail!(
                "listing {} failed ({}): {}",
                path.display(),
                out.exit_code(),
                out.stderr.trim()
            );
        }
        let mut entries: Vec<DirEntry> = out
            .stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(kind, name)| DirEntry {
                name: name.to_string(),
                kind: match kind {
                    "f" => EntryKind::File,
                    "d" => EntryKind::Dir,
                    _ => EntryKind::Other,
                },
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The worker writes the config at a fixed path inside the scope when a
    /// job binds MCP.
    fn hub_tool_patterns(&self) -> Vec<String> {
        self.hub_tools.clone()
    }

    fn placement(&self) -> Option<RemotePlacement> {
        Some(RemotePlacement {
            endpoint: self.endpoint.clone(),
            scope: self.scope.clone(),
            jobs: self.jobs,
        })
    }
}

/// Every runner daemon this process has heard from, by endpoint kind.
#[derive(Debug, Default)]
pub struct WorkerRegistry(parking_lot::RwLock<BTreeMap<&'static str, WorkerFacts>>);

impl WorkerRegistry {
    /// Keep a worker's answer to a health check.
    pub fn healthy(&self, endpoint: &RunnerEndpoint, health: &WorkerHealth, at: DateTime<Utc>) {
        self.update(endpoint, |facts| WorkerFacts {
            version: Some(health.version.clone()),
            capacity: Some(health.capacity),
            last_health_at: Some(at),
            ..facts
        });
    }

    /// Keep what a connection heard about a worker's protocol.
    pub fn observe(&self, endpoint: &RunnerEndpoint, event: ProtocolEvent, at: DateTime<Utc>) {
        self.update(endpoint, |facts| match event {
            ProtocolEvent::Announced(version) => WorkerFacts {
                protocol_version: Some(version),
                last_seen_at: Some(at),
                ..facts
            },
            ProtocolEvent::TooOld {
                announced,
                required,
            } => WorkerFacts {
                protocol_version: Some(announced.unwrap_or(0)),
                last_seen_at: Some(at),
                last_refusal: Some(ProtocolRefusal {
                    at,
                    announced,
                    required,
                }),
                ..facts
            },
        });
    }

    /// Every worker heard from, `local` before `remote`.
    pub fn snapshot(&self) -> Vec<WorkerFacts> {
        self.0.read().values().cloned().collect()
    }

    fn update(&self, endpoint: &RunnerEndpoint, change: impl FnOnce(WorkerFacts) -> WorkerFacts) {
        let name = worker_name(endpoint);
        let mut workers = self.0.write();
        let facts = workers
            .remove(name)
            .unwrap_or_else(|| WorkerFacts::unheard(name));
        workers.insert(name, change(facts));
    }
}

fn worker_name(endpoint: &RunnerEndpoint) -> &'static str {
    match endpoint {
        RunnerEndpoint::Unix(_) => "local",
        RunnerEndpoint::Tls(_) => "remote",
    }
}

static WORKER_REGISTRY: LazyLock<WorkerRegistry> = LazyLock::new(WorkerRegistry::default);

/// What this process has heard from its runner daemons.
pub fn worker_registry() -> &'static WorkerRegistry {
    &WORKER_REGISTRY
}

/// Keep the protocol every runner daemon announces in [`worker_registry`].
pub fn record_worker_protocols() {
    observe_protocol(Arc::new(|endpoint, event| {
        worker_registry().observe(endpoint, event, Utc::now())
    }));
}

/// Ask the worker how it is, log the answer and keep it in
/// [`worker_registry`], so a hub that cannot reach it says so at boot rather
/// than on the first coding round.
pub async fn log_worker_health(endpoint: &RunnerEndpoint) {
    let health = dispatch(
        endpoint,
        Request::Health,
        drained_sink(),
        CancellationToken::new(),
        None,
    )
    .await;
    match health {
        Ok(Outcome::Healthy(health)) => {
            worker_registry().healthy(endpoint, &health, Utc::now());
            info!(
                worker = %endpoint,
                scopes = health.scopes,
                capacity = health.capacity,
                pool_free_gib = health.pool_free_bytes / (1 << 30),
                goldens = health.goldens.len(),
                version = %health.version,
                "coding worker reachable"
            )
        }
        Ok(other) => {
            warn!(worker = %endpoint, answer = ?other, "coding worker answered a health check oddly")
        }
        Err(e) => {
            warn!(worker = %endpoint, error = %e, "coding worker unreachable; coding rounds will fail until it is")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use parking_lot::Mutex;
    use workforce_agent::tools::exec_runner::{
        run_exec, serve, ExecSummary, HomeSpec, JobHandler, JobLink, Listener, WorkerHealth,
    };
    use workforce_projects::ProjectRepo;

    /// A worker that remembers what it was asked and answers from a script.
    /// Commands run on this host with the scope's roots mapped under `root`,
    /// so the sandbox's shell helpers are exercised for real.
    struct FakeWorker {
        requests: Mutex<Vec<Request>>,
        refuse_prepare: Option<RefusalReason>,
        prepared: Mutex<std::collections::HashSet<ScopeId>>,
        homes: Mutex<std::collections::HashSet<String>>,
        root: PathBuf,
    }

    impl FakeWorker {
        fn map(&self, text: &str) -> String {
            [SCOPE_WORK_ROOT, SCOPE_HOME]
                .iter()
                .fold(text.to_string(), |acc, scope_root| {
                    acc.replace(
                        scope_root,
                        &self.root.join(&scope_root[1..]).display().to_string(),
                    )
                })
        }
    }

    #[async_trait]
    impl JobHandler for FakeWorker {
        async fn run(&self, request: Request, link: JobLink) -> Result<ServerFrame> {
            self.requests.lock().push(request.clone());
            Ok(match request {
                Request::Prepare(prepare) => match self.refuse_prepare {
                    Some(reason) => ServerFrame::Refused { reason },
                    None => {
                        let reused = !self.prepared.lock().insert(prepare.scope.clone());
                        let home_reused = match &prepare.home {
                            HomeSpec::Ephemeral => reused,
                            HomeSpec::Keyed { key, .. } => !self.homes.lock().insert(key.clone()),
                        };
                        let workdir = match prepare.repos.as_slice() {
                            [only] => format!("/work/{}", only.alias),
                            _ => "/work".to_string(),
                        };
                        std::fs::create_dir_all(self.map(&workdir)).unwrap();
                        ServerFrame::Prepared(Prepared {
                            workdir,
                            repos: prepare
                                .repos
                                .iter()
                                .map(|repo| (repo.alias.clone(), format!("/work/{}", repo.alias)))
                                .collect(),
                            setup: Vec::new(),
                            reused,
                            home_reused,
                            jobs: false,
                        })
                    }
                },
                Request::Exec(exec) if exec.scope.is_some() => {
                    let mapped = ExecRequest {
                        args: exec.args.iter().map(|a| self.map(a)).collect(),
                        workdir: exec.workdir.as_deref().map(|w| self.map(w)),
                        scope: None,
                        ..exec
                    };
                    let summary =
                        run_exec(&mapped, &[], EnvMode::Inherit { remove: &[] }, link.sink())
                            .await?;
                    ServerFrame::Done(summary)
                }
                Request::Release { scope } if scope.as_str().starts_with("gone") => {
                    ServerFrame::Refused {
                        reason: RefusalReason::UnknownScope,
                    }
                }
                Request::Release { .. } => ServerFrame::Done(ExecSummary {
                    exit_code: Some(0),
                    attempts: 1,
                    truncated: false,
                    timed_out: false,
                }),
                Request::Health => ServerFrame::Healthy(WorkerHealth::without_scopes()),
                Request::Exec(_)
                | Request::Interactive(_)
                | Request::Run(_)
                | Request::Forget { .. } => ServerFrame::Refused {
                    reason: RefusalReason::ProtocolMismatch,
                },
            })
        }
    }

    fn current_uid() -> u32 {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        a.peer_cred().unwrap().uid()
    }

    async fn worker(
        dir: &Path,
        refuse_prepare: Option<RefusalReason>,
    ) -> (RunnerEndpoint, Arc<FakeWorker>) {
        let handler = Arc::new(FakeWorker {
            requests: Mutex::new(Vec::new()),
            refuse_prepare,
            prepared: Mutex::new(std::collections::HashSet::new()),
            homes: Mutex::new(std::collections::HashSet::new()),
            root: dir.join("scope"),
        });
        let socket = dir.join("worker.sock");
        let listener = Listener::Unix {
            path: socket.clone(),
            accept_uid: current_uid(),
        };
        let serving: Arc<dyn JobHandler> = handler.clone();
        tokio::spawn(async move {
            let _ = serve(listener, serving).await;
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (RunnerEndpoint::Unix(socket), handler)
    }

    fn surface() -> McpSurface {
        McpSurface {
            exposed_tools: None,
        }
    }

    fn rails_project() -> Project {
        let mut project = Project::new("agent", "harbor");
        project.repos.push(ProjectRepo {
            alias: "rails".into(),
            source: RepoSource::Github {
                owner: "harborlabs".into(),
                repo: "HarborRails".into(),
            },
            default_branch: Some("develop".into()),
            setup: Some("bundle install".into()),
        });
        project
    }

    #[test]
    fn a_found_scope_names_the_repos_it_was_prepared_without() {
        let prepared = |aliases: &[&str]| Prepared {
            workdir: "/work".into(),
            repos: aliases
                .iter()
                .map(|alias| (alias.to_string(), format!("/work/{alias}")))
                .collect(),
            setup: Vec::new(),
            reused: true,
            home_reused: false,
            jobs: false,
        };
        let mut project = rails_project();
        project.repos.push(ProjectRepo {
            alias: "core-api".into(),
            ..project.repos[0].clone()
        });

        assert!(absent_repos(&prepared(&["core-api", "rails"]), &project).is_empty());
        assert_eq!(absent_repos(&prepared(&["rails"]), &project), ["core-api"]);
    }

    #[test]
    fn remote_scopes_are_stable_dataset_safe_and_bounded() {
        let a = remote_scope("agent", "harbor", "scratch-thread/with spaces");
        let b = remote_scope("agent", "harbor", "scratch-thread/with spaces");
        let other = remote_scope("agent", "other", "scratch-thread/with spaces");
        assert_eq!(a, b);
        assert_ne!(a, other);
        assert!(a.as_str().starts_with("scratch-thread-with-"), "{a}");
        assert!(a.as_str().len() <= 64);
        assert!(a
            .as_str()
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-'));
        let long = remote_scope("agent", "p", &"x".repeat(200));
        assert!(long.as_str().len() <= 64, "{long}");
        let unreadable = remote_scope("agent", "p", "///");
        assert!(
            unreadable
                .as_str()
                .starts_with(|c: char| c.is_ascii_alphanumeric()),
            "{unreadable}"
        );
    }

    #[test]
    fn repo_specs_carry_slug_branch_setup_and_the_owners_token() {
        let project = rails_project();
        let tokens = HashMap::from([("harborlabs".to_string(), "ghp_x".to_string())]);
        let specs = repo_specs(&project, &tokens);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].slug, "harborlabs/HarborRails");
        assert_eq!(specs[0].default_branch, "develop");
        assert_eq!(specs[0].setup.as_deref(), Some("bundle install"));
        assert_eq!(specs[0].token.as_deref(), Some("ghp_x"));

        let mut unset = project.clone();
        unset.repos[0].default_branch = None;
        assert_eq!(repo_specs(&unset, &HashMap::new())[0].default_branch, "");
        assert!(repo_specs(&unset, &HashMap::new())[0].token.is_none());
    }

    #[tokio::test]
    async fn a_keyed_home_is_asked_for_and_a_found_one_is_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, fake) = worker(dir.path(), None).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        let project = rails_project();
        let tokens = HashMap::new();
        let spec = |scope| WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope,
            tokens: &tokens,
            home: HomeSpec::Keyed {
                key: "bench-1".into(),
                ttl_secs: 3600,
            },
            primary: None,
        };

        assert!(launcher.ensure(&spec("thread-1")).await.unwrap().fresh());
        assert!(
            !launcher.ensure(&spec("thread-2")).await.unwrap().fresh(),
            "a new scope over a kept home has the last round's transcript"
        );
        let requests = fake.requests.lock().clone();
        let [Request::Prepare(first), Request::Prepare(second)] = &requests[..] else {
            panic!("unexpected requests: {requests:?}");
        };
        assert_ne!(first.scope, second.scope);
        assert_eq!(first.home, spec("").home);
        assert_eq!(second.home, spec("").home);
    }

    #[tokio::test]
    async fn ensure_prepares_the_scope_and_close_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, fake) = worker(dir.path(), None).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        let project = rails_project();
        let tokens = HashMap::new();
        let spec = WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope: "thread-1",
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
        };

        let sandbox = launcher.ensure(&spec).await.unwrap();
        assert_eq!(sandbox.workdir(), Path::new("/work/rails"));
        assert!(
            sandbox.hub_tool_patterns().is_empty(),
            "MCP off serves nothing"
        );
        assert!(launcher.exists("agent", "harbor", "thread-1").await);
        assert!(sandbox.fresh());
        assert!(
            !launcher.ensure(&spec).await.unwrap().fresh(),
            "the worker found it live"
        );

        launcher.close("agent", "harbor", "thread-1").await.unwrap();
        let requests = fake.requests.lock().clone();
        let expected = remote_scope("agent", "harbor", "thread-1");
        match &requests[..] {
            [Request::Prepare(prepare), Request::Prepare(_), Request::Release { scope }] => {
                assert_eq!(prepare.scope, expected);
                assert_eq!(prepare.ttl_secs, 900);
                assert_eq!(prepare.repos[0].slug, "harborlabs/HarborRails");
                assert_eq!(scope, &expected);
            }
            other => panic!("unexpected requests: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_workspace_of_several_repos_starts_in_its_primary_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, fake) = worker(dir.path(), None).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        let mut project = rails_project();
        project.repos.push(ProjectRepo {
            alias: "core-api".into(),
            source: RepoSource::Github {
                owner: "harborlabs".into(),
                repo: "core-api".into(),
            },
            default_branch: None,
            setup: None,
        });
        let tokens = HashMap::from([("harborlabs".to_string(), "ghp_x".to_string())]);
        let spec = |scope, primary| WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope,
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary,
        };

        let started = launcher
            .ensure(&spec("wf-1", Some("core-api")))
            .await
            .unwrap();
        assert_eq!(started.workdir(), Path::new("/work/core-api"));
        let found = launcher
            .ensure(&spec("wf-1", Some("core-api")))
            .await
            .unwrap();
        assert_eq!(
            found.workdir(),
            Path::new("/work/core-api"),
            "a found scope too"
        );
        assert_eq!(
            launcher
                .ensure(&spec("wf-2", None))
                .await
                .unwrap()
                .workdir(),
            Path::new("/work"),
            "unnamed, the worker's own choice"
        );
        let err = launcher
            .ensure(&spec("wf-3", Some("Harbor_Task_Manager")))
            .await
            .err()
            .expect("a primary the scope does not hold fails");
        assert!(
            err.to_string()
                .contains("no repo `Harbor_Task_Manager` to start in (it holds: rails, core-api)"),
            "{err}"
        );

        let requests = fake.requests.lock().clone();
        let Some(Request::Prepare(first)) = requests.first() else {
            panic!("unexpected requests: {requests:?}");
        };
        let repos: Vec<(&str, &str, Option<&str>)> = first
            .repos
            .iter()
            .map(|repo| {
                (
                    repo.alias.as_str(),
                    repo.slug.as_str(),
                    repo.token.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            repos,
            [
                ("rails", "harborlabs/HarborRails", Some("ghp_x")),
                ("core-api", "harborlabs/core-api", Some("ghp_x")),
            ]
        );
    }

    #[tokio::test]
    async fn the_remote_sandbox_runs_commands_and_files_inside_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, fake) = worker(dir.path(), None).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        let project = rails_project();
        let tokens = HashMap::new();
        let sandbox = launcher
            .ensure(&WorkspaceSpec {
                agent_id: "agent",
                project: &project,
                scope: "thread-2",
                tokens: &tokens,
                home: HomeSpec::Ephemeral,
                primary: None,
            })
            .await
            .unwrap();

        sandbox
            .write(Path::new("spec/new_spec.rb"), "describe 'x' do end\n")
            .await
            .unwrap();
        assert_eq!(
            sandbox
                .read(Path::new("spec/new_spec.rb"), 1 << 20)
                .await
                .unwrap(),
            Some("describe 'x' do end\n".to_string())
        );
        assert_eq!(
            sandbox
                .read(Path::new("spec/new_spec.rb"), 8)
                .await
                .unwrap(),
            Some("describe".to_string())
        );
        sandbox
            .write(Path::new("spec/héllo.txt"), "héllo\n")
            .await
            .unwrap();
        assert_eq!(
            sandbox.read(Path::new("spec/héllo.txt"), 2).await.unwrap(),
            Some("h".to_string()),
            "cut at a character boundary"
        );
        assert!(
            sandbox.read(Path::new("spec"), 8).await.is_err(),
            "a directory is not a file"
        );
        assert_eq!(
            sandbox.read(Path::new("spec/absent.rb"), 8).await.unwrap(),
            None
        );
        sandbox
            .write(Path::new("/home/agent/notes/x.md"), "kept in the home\n")
            .await
            .unwrap();
        assert_eq!(
            sandbox.list(Path::new("spec")).await.unwrap(),
            vec![
                DirEntry {
                    name: "héllo.txt".into(),
                    kind: EntryKind::File
                },
                DirEntry {
                    name: "new_spec.rb".into(),
                    kind: EntryKind::File
                }
            ]
        );
        assert_eq!(
            sandbox.list(Path::new("/work/rails")).await.unwrap(),
            vec![DirEntry {
                name: "spec".into(),
                kind: EntryKind::Dir
            }]
        );
        assert!(sandbox.list(Path::new("nowhere")).await.is_err());

        let out = sandbox
            .exec(
                SandboxCommand::new("sh", ["-c", "cat; basename \"$PWD\"; exit 2"])
                    .cwd("spec")
                    .stdin("in\n"),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code(), 2);
        assert_eq!(out.stdout, "in\nspec\n");

        let every_exec_was_scoped = fake.requests.lock().iter().all(|r| match r {
            Request::Exec(e) => e.scope.is_some(),
            _ => true,
        });
        assert!(every_exec_was_scoped);

        let before = fake.requests.lock().len();
        let oversized = "x".repeat(MAX_WRITE_BYTES + 1);
        let err = sandbox
            .write(Path::new("big"), &oversized)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("byte limit"), "{err}");
        assert!(sandbox.read(Path::new("/etc/passwd"), 8).await.is_err());
        assert!(sandbox.write(Path::new("../x"), "y").await.is_err());
        assert_eq!(
            fake.requests.lock().len(),
            before,
            "refused before any request"
        );
    }

    #[tokio::test]
    async fn a_refused_prepare_is_an_error_naming_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _fake) = worker(dir.path(), Some(RefusalReason::PoolFull)).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let err = launcher
            .ensure(&WorkspaceSpec {
                agent_id: "agent",
                project: &project,
                scope: "s",
                tokens: &tokens,
                home: HomeSpec::Ephemeral,
                primary: None,
            })
            .await
            .err()
            .expect("a refused prepare fails");
        assert!(err.to_string().contains("free-space floor"), "{err}");
    }

    #[tokio::test]
    async fn closing_a_scope_the_worker_already_reclaimed_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _fake) = worker(dir.path(), None).await;
        let launcher = RemoteLauncher::new(endpoint, 900, surface());
        launcher.close("agent", "p", "gone").await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_worker_fails_closed() {
        let launcher = RemoteLauncher::new(
            RunnerEndpoint::Unix(PathBuf::from("/nonexistent/worker.sock")),
            900,
            surface(),
        );
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let err = launcher
            .ensure(&WorkspaceSpec {
                agent_id: "agent",
                project: &project,
                scope: "s",
                tokens: &tokens,
                home: HomeSpec::Ephemeral,
                primary: None,
            })
            .await
            .err()
            .expect("an unreachable worker fails");
        assert!(
            err.to_string()
                .contains("secure command runner unavailable"),
            "{err}"
        );
        log_worker_health(&RunnerEndpoint::Unix(PathBuf::from(
            "/nonexistent/worker.sock",
        )))
        .await;
    }

    fn remote_worker() -> RunnerEndpoint {
        use workforce_agent::tools::exec_runner::{TlsEndpoint, TlsIdentity};
        RunnerEndpoint::Tls(TlsEndpoint {
            addr: "worker.internal:7443".into(),
            server_name: "worker.internal".into(),
            identity: TlsIdentity {
                ca_pem: "ca-pem".into(),
                cert_pem: "cert-pem".into(),
                key_pem: "key-pem".into(),
            },
        })
    }

    #[test]
    fn the_registry_keeps_each_workers_health_and_protocol_under_its_kind_never_its_address() {
        let registry = WorkerRegistry::default();
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        registry.healthy(
            &remote_worker(),
            &WorkerHealth {
                capacity: 4,
                version: "1.2.3".into(),
                ..WorkerHealth::without_scopes()
            },
            at("2026-09-15T04:00:00Z"),
        );
        registry.observe(
            &remote_worker(),
            ProtocolEvent::Announced(PROTOCOL_VERSION),
            at("2026-09-15T04:00:01Z"),
        );
        registry.observe(
            &RunnerEndpoint::Unix(PathBuf::from("/run/exec.sock")),
            ProtocolEvent::TooOld {
                announced: None,
                required: 2,
            },
            at("2026-09-15T04:05:00Z"),
        );
        assert_eq!(
            registry.snapshot(),
            [
                WorkerFacts {
                    protocol_version: Some(0),
                    last_seen_at: Some(at("2026-09-15T04:05:00Z")),
                    last_refusal: Some(ProtocolRefusal {
                        at: at("2026-09-15T04:05:00Z"),
                        announced: None,
                        required: 2,
                    }),
                    ..WorkerFacts::unheard("local")
                },
                WorkerFacts {
                    protocol_version: Some(PROTOCOL_VERSION),
                    version: Some("1.2.3".into()),
                    capacity: Some(4),
                    last_health_at: Some(at("2026-09-15T04:00:00Z")),
                    last_seen_at: Some(at("2026-09-15T04:00:01Z")),
                    ..WorkerFacts::unheard("remote")
                },
            ]
        );

        registry.observe(
            &remote_worker(),
            ProtocolEvent::TooOld {
                announced: Some(2),
                required: PROTOCOL_VERSION,
            },
            at("2026-09-15T12:02:00Z"),
        );
        let remote = registry.snapshot().pop().unwrap();
        assert_eq!(remote.protocol_version, Some(2));
        assert_eq!(remote.required_protocol, PROTOCOL_VERSION);
        assert_eq!(
            remote.version.as_deref(),
            Some("1.2.3"),
            "a refusal keeps the health"
        );
        assert_eq!(
            remote.last_refusal.map(|refusal| refusal.announced),
            Some(Some(2))
        );

        let json = serde_json::to_string(&registry.snapshot()).unwrap();
        for leak in [
            "worker.internal",
            "7443",
            "exec.sock",
            "cert-pem",
            "key-pem",
        ] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
    }
}

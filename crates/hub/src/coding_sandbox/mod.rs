//! Where a coding round runs.
//!
//! A coding session keeps a stable per-(session, project) transcript and
//! workspace and drives many rounds against them, so the unit of isolation is
//! the workspace scope, not the round: [`SandboxLauncher::ensure`] materialises
//! the scope (or finds it) and hands back a [`CodingSandbox`] to run rounds in.
//! The `local` backend is the hub container itself; the `remote` backend is a
//! prepared scope on the `[exec.remote]` worker. Callers see neither: the same
//! workspace spec goes in and the same sandbox comes out.

mod local;
mod remote;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{bail, Result};
use tokio_util::sync::CancellationToken;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
pub use workforce_agent::tools::exec_runner::{ExecOutcome, HomeSpec};
use workforce_common::settings::{ExecConfig, ExecutorConfig, ExecutorKind};
use workforce_projects::Project;

use crate::claude_code::RemotePlacement;
use crate::projects::layout::WorkRoots;
use crate::projects::setup::{SetupReport, SetupRunner};

pub(crate) use local::absolutise_data_dir;
pub use local::{adopt_workdir, LocalLauncher};
pub use remote::{
    log_worker_health, record_worker_protocols, worker_registry, ProtocolRefusal, RemoteLauncher,
    WorkerFacts, WorkerRegistry,
};

/// The workspace a round runs in: which project's repos, under which scope,
/// fetched with which tokens, for whom.
pub struct WorkspaceSpec<'a> {
    pub agent_id: &'a str,
    pub project: &'a Project,
    /// Stable across the rounds that share the workspace; a new scope is a new
    /// checkout.
    pub scope: &'a str,
    /// `{github_owner -> PAT}` for the project's repos.
    pub tokens: &'a HashMap<String, String>,
    /// Where the agent's home comes from: made with the scope and lost with
    /// it, or a keyed home that outlives scopes on a remote worker.
    pub home: HomeSpec,
    /// The repo alias a round on the coding worker starts in; unset, the
    /// worker's own choice: the lone repo, else the root above them all.
    pub primary: Option<&'a str>,
    /// Set for an agent turn's workspace, whose keyed home keeps what its
    /// conversation needs: the worker releases the scope once it has been
    /// idle this long, and sooner, when it is full, to seat another turn.
    /// Never set for a coding session's.
    pub idle_release: Option<Duration>,
}

/// The launcher could not seat the workspace right now; a caller with time to
/// spare may ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the coding worker is at capacity")]
pub struct AtCapacity;

/// A queued prepare the worker cannot seat yet: `ahead` go first. A caller
/// keeps its place by asking again within the worker's hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the coding worker is full; {ahead} ahead in its queue")]
pub struct Queued {
    pub ahead: u32,
}

/// Whether `e` says the coding worker had no seat for the workspace: at
/// capacity, or still queued for one.
pub fn worker_full(e: &eyre::Report) -> bool {
    e.downcast_ref::<AtCapacity>().is_some() || e.downcast_ref::<Queued>().is_some()
}

/// The least and most a caller waits between attempts to seat a workspace.
const CAPACITY_WAIT_MIN: Duration = Duration::from_secs(5);
const CAPACITY_WAIT_MAX: Duration = Duration::from_secs(60);

/// How often a queued caller asks again: well within the worker's hold, so
/// it keeps its place, and soon enough to take a seat as it frees.
const QUEUE_POLL: Duration = Duration::from_secs(3);

/// `ensure`, waiting out a worker at capacity until `deadline`, or until
/// `cancel` fires between attempts. With a `queue` owner the worker keeps the
/// caller's place in its fair queue, asked again every few seconds; a worker
/// without one refuses at capacity and is asked again after a doubling
/// pause. The last pause is cut short to try once more at the deadline.
/// `on_wait` runs before the first pause and whenever the caller's place
/// changes, with how many go first when the worker queues (`None` when it
/// only says it is full), so a surface can say where the round stands. A
/// caller that stops waiting gives its place up. Any other error is returned
/// at once, and so is the last refusal when the wait ends without a seat.
pub async fn ensure_with_wait<F>(
    launcher: &dyn SandboxLauncher,
    spec: &WorkspaceSpec<'_>,
    queue: Option<&str>,
    deadline: Instant,
    cancel: Option<&CancellationToken>,
    mut on_wait: impl FnMut(Option<u32>) -> F,
) -> Result<Arc<dyn CodingSandbox>>
where
    F: std::future::Future<Output = ()>,
{
    // On the runtime's clock, as the pauses are, so a wait is measured the
    // same way it sleeps.
    let deadline = tokio::time::Instant::from_std(deadline);
    let mut pause = CAPACITY_WAIT_MIN;
    let mut told: Option<Option<u32>> = None;
    let mut queued = false;
    loop {
        let attempt = match queue {
            Some(owner) => launcher.ensure_queued(spec, owner).await,
            None => launcher.ensure(spec).await,
        };
        let e = match attempt {
            Err(e) if worker_full(&e) => e,
            Err(e) => {
                // Failed after queueing: the place it held goes too.
                if queued {
                    launcher
                        .leave_queue(spec.agent_id, &spec.project.name, spec.scope)
                        .await;
                }
                return Err(e);
            }
            outcome => return outcome,
        };
        let ahead = e.downcast_ref::<Queued>().map(|q| q.ahead);
        queued |= ahead.is_some();
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let gave_up = match left.is_zero() {
            true => true,
            false => {
                if told != Some(ahead) {
                    on_wait(ahead).await;
                    told = Some(ahead);
                }
                let next = match ahead {
                    Some(_) => QUEUE_POLL,
                    None => pause,
                };
                let pause_over = tokio::time::sleep(next.min(left));
                match cancel {
                    Some(cancel) => tokio::select! {
                        biased;
                        _ = cancel.cancelled() => true,
                        _ = pause_over => false,
                    },
                    None => {
                        pause_over.await;
                        false
                    }
                }
            }
        };
        if gave_up {
            if queued {
                launcher
                    .leave_queue(spec.agent_id, &spec.project.name, spec.scope)
                    .await;
            }
            return Err(e);
        }
        if ahead.is_none() {
            pause = (pause * 2).min(CAPACITY_WAIT_MAX);
        }
    }
}

/// A scope of a project holding no repos, so the CLI starts in an empty
/// directory of its own.
pub struct EmptyScope<'a> {
    pub agent_id: &'a str,
    pub project: &'a str,
    pub scope: &'a str,
    pub home: HomeSpec,
    /// As [`WorkspaceSpec::idle_release`].
    pub idle_release: Option<Duration>,
    /// Who the workspace is for, when it would rather wait its fair turn in
    /// the worker's queue than be refused: see [`ensure_with_wait`].
    pub queue: Option<&'a str>,
}

impl EmptyScope<'_> {
    /// Seat the scope on `launcher`, waiting out a worker at capacity as
    /// [`ensure_with_wait`] does.
    pub async fn seat<F>(
        self,
        launcher: &dyn SandboxLauncher,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
        on_wait: impl FnMut(Option<u32>) -> F,
    ) -> Result<Arc<dyn CodingSandbox>>
    where
        F: std::future::Future<Output = ()>,
    {
        let project = Project::new(self.agent_id, self.project);
        let spec = WorkspaceSpec {
            agent_id: self.agent_id,
            project: &project,
            scope: self.scope,
            tokens: &HashMap::new(),
            home: self.home,
            primary: None,
            idle_release: self.idle_release,
        };
        ensure_with_wait(launcher, &spec, self.queue, deadline, cancel, on_wait).await
    }
}

/// Run `cleanup` on the runtime in scope, for a guard being dropped; when
/// there is none, or `cleanup` fails, say that `left` was left behind.
pub fn clean_up_detached(
    left: String,
    cleanup: impl std::future::Future<Output = Result<()>> + Send + 'static,
) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn(async move {
            if let Err(e) = cleanup.await {
                tracing::warn!(error = %e, "{left}");
            }
        })),
        Err(_) => tracing::warn!("{left}: no runtime to clean it up"),
    }
}

/// What decides a round's MCP tool surface, the same for every backend.
#[derive(Clone)]
pub struct McpSurface {
    /// Tool patterns exposed to the round; `None` when the install's MCP
    /// gate is off and the round gets no hub tools.
    pub exposed_tools: Option<Vec<String>>,
}

impl McpSurface {
    /// No hub tools at all: a round that only answers.
    pub fn none() -> Self {
        Self {
            exposed_tools: None,
        }
    }

    /// The patterns a round is served.
    pub fn hub_tools(&self) -> Vec<String> {
        self.exposed_tools.clone().unwrap_or_default()
    }

    /// The surface a remote round gets: the hub's `code.*` tools act on this
    /// host's checkouts, which a round in a worker scope cannot see, and the
    /// round has the same abilities natively inside the scope.
    pub fn for_remote(&self) -> Self {
        Self {
            exposed_tools: self.exposed_tools.as_ref().map(|patterns| {
                patterns
                    .iter()
                    .filter(|pattern| !pattern.starts_with("code."))
                    .cloned()
                    .collect()
            }),
        }
    }
}

/// A command to run inside a sandbox, as the round's user, with its cwd and
/// file paths resolved against the sandbox.
#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Relative to the workdir, or absolute inside the sandbox; the workdir
    /// when unset.
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    pub timeout: Duration,
}

/// Bound for a command that gave no timeout.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

impl SandboxCommand {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
            stdin: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    pub fn cwd(self, cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: Some(cwd.into()),
            ..self
        }
    }

    pub fn stdin(self, stdin: impl Into<String>) -> Self {
        Self {
            stdin: Some(stdin.into()),
            ..self
        }
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// Resolve `path` inside a sandbox: relative to `workdir`, never through `..`,
/// and only under one of `roots`. The error names the roots so a caller learns
/// the contract from the refusal.
pub(crate) fn sandbox_path(workdir: &Path, path: &Path, roots: &[&Path]) -> Result<PathBuf> {
    if path.components().any(|c| c == Component::ParentDir) {
        bail!("path {} must not contain `..`", path.display());
    }
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    };
    if roots.iter().any(|root| resolved.starts_with(root)) {
        Ok(resolved)
    } else {
        let allowed: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        bail!(
            "path {} is outside the sandbox (allowed under {})",
            resolved.display(),
            allowed.join(", ")
        )
    }
}

/// The first `max_bytes` of `bytes` as text, cut at a character boundary.
pub(crate) fn head_utf8(bytes: &[u8], max_bytes: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// A materialised workspace to run rounds in.
#[async_trait]
pub trait CodingSandbox: Send + Sync {
    /// Where Claude Code starts, as the round sees it.
    fn workdir(&self) -> &Path;

    /// What the project's setup commands did in this workspace, for the pass
    /// that ran them.
    fn setup(&self) -> &[SetupReport];

    /// The hub tool patterns (`jira.*`, `code.read`) a round may call; empty
    /// when the round gets no hub tools.
    fn hub_tool_patterns(&self) -> Vec<String>;

    /// Whether `ensure` made this workspace's home rather than finding it as
    /// the last round left it: a fresh home has no transcript to resume.
    fn fresh(&self) -> bool;

    /// Run a command inside the workspace and collect its output.
    async fn exec(&self, command: SandboxCommand) -> Result<ExecOutcome>;

    /// The first `max_bytes` of a file, or `None` when there is no such file.
    async fn read(&self, path: &Path, max_bytes: usize) -> Result<Option<String>>;

    /// Write a file, creating its directories.
    async fn write(&self, path: &Path, content: &str) -> Result<()>;

    /// The entries of a directory, sorted by name.
    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>>;

    /// The remote scope a round runs in, or `None` on this host: what an
    /// engine hands the CLI transport so its process lands here.
    fn placement(&self) -> Option<RemotePlacement>;
}

#[async_trait]
pub trait SandboxLauncher: Send + Sync {
    /// Materialise the workspace, or find it, and run the project's setup in
    /// it once.
    async fn ensure(&self, spec: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>>;

    /// `ensure`, keeping a place in the worker's fair queue for `owner` when
    /// it cannot seat the workspace yet: refused with [`Queued`] then. A
    /// launcher without a queue only ensures.
    async fn ensure_queued(
        &self,
        spec: &WorkspaceSpec<'_>,
        _owner: &str,
    ) -> Result<Arc<dyn CodingSandbox>> {
        self.ensure(spec).await
    }

    /// The caller queued for `scope` stopped waiting: its place goes to the
    /// next in line. A launcher without a queue has none to give up.
    async fn leave_queue(&self, _agent_id: &str, _project: &str, _scope: &str) {}

    /// Whether a workspace for this scope can be resumed as it was left.
    async fn exists(&self, agent_id: &str, project: &str, scope: &str) -> bool;

    /// Tear the workspace down; one that is already gone is not an error.
    async fn close(&self, agent_id: &str, project: &str, scope: &str) -> Result<()>;

    /// End a durable job left running in the workspace, without preparing
    /// it: a workspace or job that is gone has nothing to end. A launcher
    /// whose rounds are not durable jobs has none.
    async fn end_job(
        &self,
        _agent_id: &str,
        _project: &str,
        _scope: &str,
        _job: &workforce_agent::tools::exec_runner::JobId,
    ) {
    }

    /// Reclaim workspaces under `prefix` that no live run owns, `live` saying
    /// which scopes are owned.
    async fn sweep(
        &self,
        agent_id: &str,
        project: &str,
        prefix: &str,
        live: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
    );
}

/// The launcher `[coding_session.executor]` selects.
pub fn launcher_for(
    executor: &ExecutorConfig,
    exec: &ExecConfig,
    roots: WorkRoots,
    setup: SetupRunner,
    mcp: McpSurface,
) -> Result<Arc<dyn SandboxLauncher>> {
    Ok(match remote_launcher_for(executor, exec, mcp.clone())? {
        Some(remote) => remote,
        None => Arc::new(LocalLauncher::new(roots, setup, mcp)),
    })
}

/// The remote launcher `[coding_session.executor]` selects, or `None` when
/// rounds run on this host.
pub fn remote_launcher_for(
    executor: &ExecutorConfig,
    exec: &ExecConfig,
    mcp: McpSurface,
) -> Result<Option<Arc<dyn SandboxLauncher>>> {
    match executor.kind {
        ExecutorKind::Local => Ok(None),
        ExecutorKind::Remote => {
            let endpoint = RunnerEndpoint::remote_from_config(exec).ok_or_else(|| {
                eyre::eyre!(
                    "[coding_session.executor] kind = \"remote\" needs an [exec.remote] worker"
                )
            })?;
            Ok(Some(Arc::new(RemoteLauncher::new(
                endpoint,
                executor.scope_ttl_secs,
                mcp.for_remote(),
            ))))
        }
    }
}

/// Where coding rounds and agent turns get their workspaces: the configured
/// executor, its setup run through the same runner as the rounds, serving
/// `surface` to them.
pub(crate) fn hub_launcher(
    roots: crate::projects::layout::WorkRoots,
    coding: &workforce_common::settings::CodingSessionConfig,
    exec: &workforce_common::settings::ExecConfig,
    surface: crate::coding_sandbox::McpSurface,
) -> Result<Arc<dyn crate::coding_sandbox::SandboxLauncher>> {
    let runner = workforce_agent::tools::exec_runner::RunnerEndpoint::from_config(exec);
    crate::coding_sandbox::launcher_for(
        &coding.executor,
        exec,
        roots,
        crate::coding_session_tool::DriverProfile::from(coding).setup_runner(runner.as_ref()),
        surface,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn surface() -> McpSurface {
        McpSurface {
            exposed_tools: None,
        }
    }

    fn setup_runner() -> SetupRunner {
        SetupRunner {
            runner: None,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_default_executor_runs_on_this_host() {
        let roots = WorkRoots {
            data_dir: std::env::temp_dir(),
            hot: None,
        };
        let launcher = launcher_for(
            &ExecutorConfig::default(),
            &ExecConfig::default(),
            roots,
            setup_runner(),
            surface(),
        );
        assert!(launcher.is_ok());
        assert!(remote_launcher_for(
            &ExecutorConfig::default(),
            &ExecConfig::default(),
            surface()
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn a_remote_executor_without_a_worker_is_refused() {
        let executor = ExecutorConfig {
            kind: ExecutorKind::Remote,
            ..ExecutorConfig::default()
        };
        let err = remote_launcher_for(&executor, &ExecConfig::default(), surface())
            .err()
            .expect("no worker to run on");
        assert!(err.to_string().contains("[exec.remote]"), "{err}");
    }

    /// Refuses `refusals` times as a worker at capacity, then seats.
    struct Refusing(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl SandboxLauncher for Refusing {
        async fn ensure(&self, _: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
            use std::sync::atomic::Ordering::SeqCst;
            match self.0.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)) {
                Ok(_) => Err(AtCapacity.into()),
                Err(_) => Ok(adopt_workdir(std::env::temp_dir(), Vec::new(), None)),
            }
        }
        async fn exists(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        async fn close(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn sweep(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
        ) {
        }
    }

    async fn wait_for_a_seat(
        refusals: usize,
        deadline: tokio::time::Instant,
        cancel: Option<&CancellationToken>,
    ) -> (Result<Arc<dyn CodingSandbox>>, usize) {
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let spec = WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope: "s",
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
            idle_release: None,
        };
        let launcher = Refusing(std::sync::atomic::AtomicUsize::new(refusals));
        let waits = std::cell::Cell::new(0);
        let notice = &waits;
        let seated = ensure_with_wait(
            &launcher,
            &spec,
            None,
            deadline.into_std(),
            cancel,
            |_| async move { notice.set(notice.get() + 1) },
        )
        .await;
        (seated, waits.get())
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_at_capacity_is_waited_out_with_one_notice() {
        let (seated, waits) = wait_for_a_seat(
            2,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await;
        assert!(seated.is_ok());
        assert_eq!(waits, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_lasts_its_whole_bound_and_no_longer() {
        let started = tokio::time::Instant::now();
        let (seated, _) =
            wait_for_a_seat(usize::MAX, started + Duration::from_secs(100), None).await;
        assert!(seated.is_err());
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(100),
            "the last pause is cut to the deadline, where it tries once more"
        );
        let (seated, waits) = wait_for_a_seat(5, started + Duration::from_secs(200), None).await;
        assert!(seated.is_ok(), "a seat freed inside the bound is taken");
        assert_eq!(waits, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_at_the_deadline_or_when_the_caller_stops() {
        let (seated, waits) = wait_for_a_seat(
            usize::MAX,
            tokio::time::Instant::now() + Duration::from_secs(3),
            None,
        )
        .await;
        assert!(seated.is_err_and(|e| e.downcast_ref::<AtCapacity>().is_some()));
        assert_eq!(waits, 1, "queued once, and tried again at the deadline");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (seated, waits) = wait_for_a_seat(
            usize::MAX,
            tokio::time::Instant::now() + Duration::from_secs(3600),
            Some(&cancel),
        )
        .await;
        assert!(seated.is_err_and(|e| e.downcast_ref::<AtCapacity>().is_some()));
        assert_eq!(waits, 1, "queued once, then stopped");
    }

    /// A worker with a queue: answers each queued prepare with the next of
    /// `places`, then seats it, and remembers who gave up a place.
    struct Queueing {
        places: parking_lot::Mutex<std::collections::VecDeque<u32>>,
        owners: parking_lot::Mutex<Vec<String>>,
        left: std::sync::atomic::AtomicUsize,
    }

    impl Queueing {
        fn new(places: &[u32]) -> Self {
            Self {
                places: parking_lot::Mutex::new(places.iter().copied().collect()),
                owners: parking_lot::Mutex::new(Vec::new()),
                left: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl SandboxLauncher for Queueing {
        async fn ensure(&self, _: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
            Err(AtCapacity.into())
        }
        async fn ensure_queued(
            &self,
            _: &WorkspaceSpec<'_>,
            owner: &str,
        ) -> Result<Arc<dyn CodingSandbox>> {
            self.owners.lock().push(owner.to_string());
            match self.places.lock().pop_front() {
                Some(ahead) => Err(Queued { ahead }.into()),
                None => Ok(adopt_workdir(std::env::temp_dir(), Vec::new(), None)),
            }
        }
        async fn leave_queue(&self, _: &str, _: &str, _: &str) {
            self.left.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn exists(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        async fn close(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn sweep(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
        ) {
        }
    }

    async fn queue_for_a_seat(
        launcher: &Queueing,
        deadline: tokio::time::Instant,
    ) -> (Result<Arc<dyn CodingSandbox>>, Vec<Option<u32>>) {
        queue_or_stop(launcher, deadline, None).await
    }

    async fn queue_or_stop(
        launcher: &Queueing,
        deadline: tokio::time::Instant,
        cancel: Option<&CancellationToken>,
    ) -> (Result<Arc<dyn CodingSandbox>>, Vec<Option<u32>>) {
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let spec = WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope: "s",
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
            idle_release: None,
        };
        let told = std::cell::RefCell::new(Vec::new());
        let notices = &told;
        let seated = ensure_with_wait(
            launcher,
            &spec,
            Some("person-1"),
            deadline.into_std(),
            cancel,
            |ahead| async move { notices.borrow_mut().push(ahead) },
        )
        .await;
        (seated, told.into_inner())
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_seat_keeps_its_place_and_says_each_new_one() {
        let launcher = Queueing::new(&[2, 2, 1, 0]);
        let started = tokio::time::Instant::now();
        let (seated, told) = queue_for_a_seat(&launcher, started + Duration::from_secs(60)).await;
        assert!(seated.is_ok());
        assert_eq!(told, vec![Some(2), Some(1), Some(0)], "said once per place");
        assert_eq!(launcher.owners.lock().len(), 5);
        assert!(launcher.owners.lock().iter().all(|o| o == "person-1"));
        assert_eq!(
            started.elapsed(),
            QUEUE_POLL * 4,
            "asked again often enough to keep the place"
        );
        assert_eq!(launcher.left.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_seat_not_reached_in_time_gives_its_place_up() {
        let launcher = Queueing::new(&[3; 100]);
        let (seated, told) = queue_for_a_seat(
            &launcher,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await;
        assert!(seated.is_err_and(|e| worker_full(&e)));
        assert_eq!(told, vec![Some(3)]);
        assert_eq!(
            launcher.left.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the next in line moves up at once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_seat_given_up_by_its_caller_goes_to_the_next_in_line() {
        let launcher = Queueing::new(&[3; 100]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (seated, _) = queue_or_stop(
            &launcher,
            tokio::time::Instant::now() + Duration::from_secs(3600),
            Some(&cancel),
        )
        .await;
        assert!(seated.is_err_and(|e| worker_full(&e)));
        assert_eq!(launcher.left.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn sandbox_paths_stay_under_the_roots_and_never_climb() {
        let workdir = Path::new("/work/rails");
        let roots = [Path::new("/work"), Path::new("/home/agent")];
        assert_eq!(
            sandbox_path(workdir, Path::new("spec/a_spec.rb"), &roots).unwrap(),
            PathBuf::from("/work/rails/spec/a_spec.rb")
        );
        assert_eq!(
            sandbox_path(workdir, Path::new("/home/agent/.claude"), &roots).unwrap(),
            PathBuf::from("/home/agent/.claude")
        );
        for bad in ["../etc/passwd", "/etc/passwd", "/workspace/x", "a/../../b"] {
            let err = sandbox_path(workdir, Path::new(bad), &roots).unwrap_err();
            assert!(
                err.to_string().contains("..") || err.to_string().contains("outside"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn head_utf8_cuts_at_a_character_boundary() {
        let text = "é".repeat(10);
        assert_eq!(head_utf8(text.as_bytes(), 3), "é");
        assert_eq!(head_utf8(text.as_bytes(), 4), "éé");
        assert_eq!(head_utf8(b"abc", 10), "abc");
    }

    #[test]
    fn absent_executor_table_deserializes_to_local() {
        let cfg: workforce_common::settings::CodingSessionConfig =
            toml::from_str("").expect("empty config parses");
        assert_eq!(cfg.executor.kind, ExecutorKind::Local);
    }
}

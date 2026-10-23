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
}

/// The launcher could not seat the workspace right now; a caller with time to
/// spare may ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the coding worker is at capacity")]
pub struct AtCapacity;

/// The least and most a caller waits between attempts to seat a workspace.
const CAPACITY_WAIT_MIN: Duration = Duration::from_secs(5);
const CAPACITY_WAIT_MAX: Duration = Duration::from_secs(60);

/// `ensure`, waiting out a worker at capacity until `deadline` with a
/// doubling pause, or until `cancel` fires between attempts; `on_wait` runs
/// once, before the first pause, so a surface can say the round is queued.
/// Any other error is returned at once, and so is the capacity refusal when
/// the wait ends without a seat.
pub async fn ensure_with_wait<F>(
    launcher: &dyn SandboxLauncher,
    spec: &WorkspaceSpec<'_>,
    deadline: Instant,
    cancel: Option<&CancellationToken>,
    mut on_wait: impl FnMut() -> F,
) -> Result<Arc<dyn CodingSandbox>>
where
    F: std::future::Future<Output = ()>,
{
    let mut pause = CAPACITY_WAIT_MIN;
    let mut waited = false;
    loop {
        match launcher.ensure(spec).await {
            Err(e)
                if e.downcast_ref::<AtCapacity>().is_some()
                    && Instant::now() + pause < deadline =>
            {
                if !waited {
                    on_wait().await;
                    waited = true;
                }
                let pause_over = tokio::time::sleep(pause);
                match cancel {
                    Some(cancel) => tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(e),
                        _ = pause_over => {}
                    },
                    None => pause_over.await,
                }
                pause = (pause * 2).min(CAPACITY_WAIT_MAX);
            }
            outcome => return outcome,
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
}

impl EmptyScope<'_> {
    /// Seat the scope on `launcher`, waiting out a worker at capacity as
    /// [`ensure_with_wait`] does.
    pub async fn seat<F>(
        self,
        launcher: &dyn SandboxLauncher,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
        on_wait: impl FnMut() -> F,
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
        };
        ensure_with_wait(launcher, &spec, deadline, cancel, on_wait).await
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
        deadline: Instant,
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
        };
        let launcher = Refusing(std::sync::atomic::AtomicUsize::new(refusals));
        let waits = std::cell::Cell::new(0);
        let notice = &waits;
        let seated = ensure_with_wait(&launcher, &spec, deadline, cancel, || async move {
            notice.set(notice.get() + 1)
        })
        .await;
        (seated, waits.get())
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_at_capacity_is_waited_out_with_one_notice() {
        let (seated, waits) =
            wait_for_a_seat(2, Instant::now() + Duration::from_secs(60), None).await;
        assert!(seated.is_ok());
        assert_eq!(waits, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_at_the_deadline_or_when_the_caller_stops() {
        let (seated, waits) =
            wait_for_a_seat(usize::MAX, Instant::now() + Duration::from_secs(3), None).await;
        assert!(seated.is_err_and(|e| e.downcast_ref::<AtCapacity>().is_some()));
        assert_eq!(waits, 0, "no pause fits, so nothing was queued");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (seated, waits) = wait_for_a_seat(
            usize::MAX,
            Instant::now() + Duration::from_secs(3600),
            Some(&cancel),
        )
        .await;
        assert!(seated.is_err_and(|e| e.downcast_ref::<AtCapacity>().is_some()));
        assert_eq!(waits, 1, "queued once, then stopped");
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

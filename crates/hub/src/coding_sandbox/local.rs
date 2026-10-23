//! The hub container as the backend: workspaces are `work/session/<scope>`
//! trees over the shared repo bases, and `claude` is spawned here (or on the
//! hub-local runner when one is configured).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use workforce_agent::tools::config_guard;
use workforce_agent::tools::exec::EXEC_SECRET_VARS;
use workforce_agent::tools::exec_runner::{
    run_or_dispatch, EnvMode, ExecOutcome, ExecRequest, RunnerEndpoint,
};
use workforce_agent::tools::paths::CanonicalRoots;

use super::{
    head_utf8, sandbox_path, CodingSandbox, DirEntry, EntryKind, McpSurface, SandboxCommand,
    SandboxLauncher, WorkspaceSpec,
};
use crate::claude_code::{self, RemotePlacement};
use crate::projects::layout::WorkRoots;
use crate::projects::setup::{self, SetupReport, SetupRunner};
use crate::projects::work_session;

pub struct LocalLauncher {
    roots: WorkRoots,
    setup: SetupRunner,
    mcp: McpSurface,
    /// Materialising or removing a workspace writes the shared repo base's
    /// worktree registry, so those steps take turns; setup and rounds run
    /// outside the lock.
    base: tokio::sync::Mutex<()>,
}

impl LocalLauncher {
    pub fn new(roots: WorkRoots, setup: SetupRunner, mcp: McpSurface) -> Self {
        Self {
            roots: absolutise_roots(roots),
            setup,
            mcp,
            base: tokio::sync::Mutex::new(()),
        }
    }
}

/// Resolve a configured root to an absolute path. The default data_dir is
/// relative ("./data"), which makes the derived session workdir — and thus its
/// MCP config path — relative too. Claude Code is spawned with
/// `current_dir = workdir` and re-resolves a relative `--mcp-config` against
/// it, doubling the path (e.g.
/// `/app/data/.../session/<id>/data/.../session/<id>/.workforce-mcp-….json`) and
/// failing with "MCP config file not found". Anchoring to the cwd keeps every
/// derived path absolute regardless of the configured root.
pub(crate) fn absolutise_data_dir(data_dir: PathBuf) -> PathBuf {
    if data_dir.is_absolute() {
        return data_dir;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&data_dir))
        .unwrap_or(data_dir)
}

fn absolutise_roots(roots: WorkRoots) -> WorkRoots {
    WorkRoots {
        data_dir: absolutise_data_dir(roots.data_dir),
        hot: roots.hot.map(absolutise_data_dir),
    }
}

#[async_trait]
impl SandboxLauncher for LocalLauncher {
    async fn ensure(&self, spec: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
        let (workdir, fresh) = {
            let _base = self.base.lock().await;
            let existed = work_session::session_exists(
                &self.roots,
                spec.agent_id,
                &spec.project.name,
                spec.scope,
            );
            let workdir = work_session::ensure(
                &self.roots,
                spec.agent_id,
                spec.project,
                spec.scope,
                spec.tokens,
            )
            .await?;
            (workdir, !existed)
        };
        let setup = setup::run_once(spec.project, &workdir, &self.setup).await;
        Ok(Arc::new(LocalSandbox {
            workdir,
            setup,
            fresh,
            hub_tools: self.mcp.hub_tools(),
            runner: self.setup.runner.clone(),
            env: self.setup.env.clone(),
        }))
    }

    async fn exists(&self, agent_id: &str, project: &str, scope: &str) -> bool {
        work_session::session_exists(&self.roots, agent_id, project, scope)
    }

    async fn close(&self, agent_id: &str, project: &str, scope: &str) -> Result<()> {
        let _base = self.base.lock().await;
        work_session::cleanup(&self.roots, agent_id, project, scope).await
    }

    async fn sweep(
        &self,
        agent_id: &str,
        project: &str,
        prefix: &str,
        live: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
    ) {
        let _base = self.base.lock().await;
        let _ = work_session::cleanup_orphans(&self.roots, agent_id, project, prefix, live).await;
    }
}

struct LocalSandbox {
    workdir: PathBuf,
    setup: Vec<SetupReport>,
    fresh: bool,
    hub_tools: Vec<String>,
    /// The hub-local runner commands go through when one is configured, with
    /// the environment setup and rounds share.
    runner: Option<RunnerEndpoint>,
    env: Vec<(String, String)>,
}

/// A workspace materialised by someone else (a workflow worktree) as a
/// sandbox for one round that may call `hub_tools`.
pub fn adopt_workdir(
    workdir: PathBuf,
    hub_tools: Vec<String>,
    runner: Option<RunnerEndpoint>,
) -> Arc<dyn CodingSandbox> {
    Arc::new(LocalSandbox {
        workdir,
        setup: Vec::new(),
        fresh: false,
        hub_tools,
        runner,
        env: claude_code::inherited_toolchain_env(),
    })
}

impl LocalSandbox {
    fn roots(&self) -> CanonicalRoots {
        CanonicalRoots::new(std::slice::from_ref(&self.workdir))
    }

    /// The real location of `path` in the workdir. The round's own commands
    /// write the workdir, so a link there must not carry the hub, which does
    /// these reads and writes, anywhere else.
    fn path(&self, path: &Path) -> Result<PathBuf> {
        let lexical = sandbox_path(&self.workdir, path, &[&self.workdir])?;
        self.roots()
            .resolve(&lexical)
            .ok_or_else(|| eyre::eyre!("path {} leads outside the sandbox", path.display()))
    }
}

#[async_trait]
impl CodingSandbox for LocalSandbox {
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
            env: self.env.iter().cloned().chain(command.env).collect(),
            stdin: command.stdin,
            scope: None,
            mcp: None,
        };
        run_or_dispatch(
            self.runner.as_ref(),
            request,
            std::slice::from_ref(&self.workdir),
            EnvMode::Inherit {
                remove: EXEC_SECRET_VARS,
            },
        )
        .await
    }

    async fn read(&self, path: &Path, max_bytes: usize) -> Result<Option<String>> {
        let path = self.path(path)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(head_utf8(&bytes, max_bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).wrap_err_with(|| format!("reading {}", path.display())),
        }
    }

    async fn write(&self, path: &Path, content: &str) -> Result<()> {
        let path = self.path(path)?;
        let roots = self.roots();
        let content = content.to_owned();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                config_guard::create_dirs(&roots, parent)?;
            }
            config_guard::write(&roots, &path, content.as_bytes())
                .map_err(|e| eyre::eyre!("writing {}: {e}", path.display()))
        })
        .await?
    }

    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let path = self.path(path)?;
        let mut dir = tokio::fs::read_dir(&path)
            .await
            .wrap_err_with(|| format!("listing {}", path.display()))?;
        let mut entries = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let kind = match entry.file_type().await {
                Ok(t) if t.is_dir() => EntryKind::Dir,
                Ok(t) if t.is_file() => EntryKind::File,
                _ => EntryKind::Other,
            };
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn hub_tool_patterns(&self) -> Vec<String> {
        self.hub_tools.clone()
    }

    fn placement(&self) -> Option<RemotePlacement> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use workforce_agent::tools::exec_runner::HomeSpec;
    use workforce_projects::Project;

    fn launcher(root: &Path) -> LocalLauncher {
        LocalLauncher::new(
            WorkRoots {
                data_dir: root.to_path_buf(),
                hot: None,
            },
            SetupRunner {
                runner: None,
                env: Vec::new(),
                timeout: Duration::from_secs(5),
            },
            McpSurface {
                exposed_tools: Some(vec!["jira.*".to_string()]),
            },
        )
    }

    #[test]
    fn absolutise_data_dir_makes_relative_absolute_and_leaves_absolute_untouched() {
        let resolved = absolutise_data_dir(PathBuf::from("data"));
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(resolved.ends_with("data"));
        let abs = PathBuf::from("/app/data");
        assert_eq!(absolutise_data_dir(abs.clone()), abs);
    }

    /// The round's own commands can plant a link in its workdir; the hub's
    /// writes for it must not follow one into the agent's workspace.
    #[tokio::test]
    async fn a_link_out_of_the_workdir_carries_no_read_or_write() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::write(
            workspace.join("config.toml"),
            "[access]\nmode = \"allowlist\"\n",
        )
        .unwrap();
        let workdir = workspace.join("projects/app/work/session/s1");
        std::fs::create_dir_all(&workdir).unwrap();
        std::os::unix::fs::symlink(&workspace, workdir.join("escape")).unwrap();
        let sandbox = adopt_workdir(workdir.clone(), Vec::new(), None);

        assert!(sandbox
            .write(
                Path::new("escape/config.toml"),
                "[access]\nmode = \"open\"\n"
            )
            .await
            .is_err());
        assert!(sandbox
            .read(Path::new("escape/config.toml"), 1024)
            .await
            .is_err());
        assert!(std::fs::read_to_string(workspace.join("config.toml"))
            .unwrap()
            .contains("allowlist"));

        sandbox
            .write(Path::new("src/main.rs"), "fn main() {}")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workdir.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
    }

    #[tokio::test]
    async fn closing_a_transcript_scope_spares_a_scratch_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = launcher(dir.path());
        let project = Project::new("allan", "skills");
        let tokens = HashMap::new();
        let uuid = "11111111-1111-4111-8111-111111111111";
        let scratch = crate::projects::workspace_scope(false, "sess-1", uuid);
        let spec = |scope: &'static str| WorkspaceSpec {
            agent_id: "allan",
            project: &project,
            scope,
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
        };

        let transcript = launcher.ensure(&spec(uuid)).await.unwrap();
        let scratch_dir = launcher
            .ensure(&WorkspaceSpec {
                scope: &scratch,
                ..spec(uuid)
            })
            .await
            .unwrap()
            .workdir()
            .to_path_buf();
        tokio::fs::write(scratch_dir.join("lib.rs"), "fn main() {}")
            .await
            .unwrap();

        launcher.close("allan", "skills", uuid).await.unwrap();

        assert!(!transcript.workdir().exists());
        assert!(
            scratch_dir.join("lib.rs").exists(),
            "a scratch workspace is never uuid-keyed, so rotation must not touch it"
        );
    }

    #[tokio::test]
    async fn the_local_sandbox_runs_commands_and_keeps_files_inside_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = launcher(dir.path());
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let spec = WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope: "scope-x",
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
        };
        let sandbox = launcher.ensure(&spec).await.unwrap();
        assert!(sandbox.fresh());
        assert!(!launcher.ensure(&spec).await.unwrap().fresh());

        sandbox
            .write(Path::new("notes/a.txt"), "héllo\n")
            .await
            .unwrap();
        assert_eq!(
            sandbox
                .read(Path::new("notes/a.txt"), 1 << 20)
                .await
                .unwrap(),
            Some("héllo\n".to_string())
        );
        assert_eq!(
            sandbox.read(Path::new("notes/a.txt"), 2).await.unwrap(),
            Some("h".to_string())
        );
        assert_eq!(
            sandbox.read(Path::new("notes/missing"), 10).await.unwrap(),
            None
        );
        assert_eq!(
            sandbox.list(Path::new("notes")).await.unwrap(),
            vec![DirEntry {
                name: "a.txt".into(),
                kind: EntryKind::File
            }]
        );

        let out = sandbox
            .exec(
                SandboxCommand::new("sh", ["-c", "cat; pwd; echo err >&2; exit 3"])
                    .cwd("notes")
                    .stdin("from stdin\n"),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code(), 3);
        assert!(out.stdout.starts_with("from stdin\n"), "{}", out.stdout);
        assert!(out.stdout.trim_end().ends_with("/notes"), "{}", out.stdout);
        assert_eq!(out.stderr.trim(), "err");

        for escape in ["../outside", "/etc/passwd"] {
            assert!(
                sandbox.read(Path::new(escape), 10).await.is_err(),
                "{escape}"
            );
            assert!(
                sandbox.write(Path::new(escape), "x").await.is_err(),
                "{escape}"
            );
            assert!(
                sandbox
                    .exec(SandboxCommand::new("true", Vec::<String>::new()).cwd(escape))
                    .await
                    .is_err(),
                "{escape}"
            );
        }
    }

    #[tokio::test]
    async fn a_repo_less_workspace_is_materialised_found_and_closed() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = launcher(dir.path());
        let project = Project::new("agent", "bench");
        let tokens = HashMap::new();
        let spec = WorkspaceSpec {
            agent_id: "agent",
            project: &project,
            scope: "scope-1",
            tokens: &tokens,
            home: HomeSpec::Ephemeral,
            primary: None,
        };

        assert!(!launcher.exists("agent", "bench", "scope-1").await);
        let sandbox = launcher.ensure(&spec).await.unwrap();
        assert!(sandbox.workdir().is_dir());
        assert!(sandbox.setup().is_empty(), "no repos, no setup");
        assert_eq!(sandbox.hub_tool_patterns(), ["jira.*"]);
        assert!(launcher.exists("agent", "bench", "scope-1").await);

        launcher.close("agent", "bench", "scope-1").await.unwrap();
        assert!(!launcher.exists("agent", "bench", "scope-1").await);
        launcher
            .close("agent", "bench", "scope-1")
            .await
            .expect("closing a closed workspace is not an error");
    }
}

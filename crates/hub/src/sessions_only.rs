//! "Always work in real sessions": an agent that oversees the crew
//! (`[oversight] enabled`, at a level that starts sessions) with
//! `[oversight.work] prefer_sessions` does no coding work in-process, so the
//! work always happens in a session the person can watch, type into and take
//! over. While the switch is on, these calls are refused with a pointer to
//! `crew.start_session`:
//!
//! - a coding tool's (`code.*`, `coding.*`) call that changes or runs
//!   something: an edit, a build, a test, a commit, a coding session or
//!   fan-out of its own;
//! - every `exec.*` call: a shell runs anything;
//! - a file write, edit or delete (`filesystem.write`, `filesystem.edit`,
//!   `filesystem.delete`, `fs.remove`) that touches a path, its links
//!   followed, inside a git work tree, a Desk session's folder, a project
//!   checkout or the hub's worktrees.
//!
//! What only reads still runs, and so do writes elsewhere in the agent's own
//! workspace (its notes, memory and scratch files). Workflow coding steps and
//! pilot rounds run their own CLIs, not these tools, and are not held.
//!
//! The switch is read from the agent's config at each call, so turning it on
//! or off applies to the next call. It is enforced in the tool itself
//! ([`SessionsOnly`]), so every path a call takes — a turn, a worker
//! sub-agent's snapshot, a CLI turn's gated MCP — meets it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{SurfaceEffect, Tool, ToolDefinition, ToolResult};
use workforce_core::computer::Effect;

/// Why a call is refused while the switch is on: the same words for every
/// held tool and for a sub-agent that would do the work.
pub const ALWAYS_SESSIONS: &str = "Desk is set to always work in real sessions: start one \
with crew.start_session so the person can watch it. Here only read-only lookups run: reads, \
search, notes in the agent's own workspace, and sub-agents with profile \"worker\".";

/// Whether the agent whose workspace is `workspace` always works in real
/// sessions: it oversees the crew at a level that starts sessions (Watch
/// starts none) with `[oversight.work] prefer_sessions` on.
pub fn always_sessions(workspace: &Path) -> bool {
    let oversight = workforce_common::settings::AgentWorkspaceConfig::load(workspace)
        .ok()
        .flatten()
        .and_then(|c| c.oversight().ok())
        .unwrap_or_default();
    oversight.enabled
        && oversight.level != workforce_common::oversight::Level::Watch
        && oversight.work.prefer_sessions
}

/// What a held tool is, for what of its calls is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// `code.*`, `coding.*`: refused unless the call only reads.
    Coding,
    /// `exec.*`: always refused.
    Shell,
    /// A file write, edit or delete: refused where it touches code.
    Files,
}

impl Held {
    fn of(name: &str) -> Option<Self> {
        if name.starts_with("code.") || name.starts_with("coding.") {
            return Some(Self::Coding);
        }
        if name.starts_with("exec.") {
            return Some(Self::Shell);
        }
        matches!(
            name,
            "filesystem.write" | "filesystem.edit" | "filesystem.delete" | "fs.remove"
        )
        .then_some(Self::Files)
    }
}

/// Whether `name` is a tool held to the switch: a coding tool, a shell, or
/// a file write.
pub fn is_held(name: &str) -> bool {
    Held::of(name).is_some()
}

/// Whether a call with `effect` only reads.
fn reads_only(effect: &Effect) -> bool {
    matches!(effect, Effect::None | Effect::Read { .. })
}

/// The folders whose files are code, beyond any git work tree: the hub's
/// worktrees and project checkouts, and the folders Desk's sessions run in.
pub type CodeRoots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// Where an agent's held tools look: its workspace (its config, and where
/// its own notes may be written) and the folders that hold code.
#[derive(Clone)]
pub struct Bounds {
    workspace: PathBuf,
    code: CodeRoots,
}

impl Bounds {
    pub fn new(workspace: &Path, code: CodeRoots) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            code,
        }
    }

    /// An agent's bounds on this hub: `roots` (its worktrees and project
    /// checkouts) and the folders of Desk's sessions, read at each call.
    pub fn of_agent(workspace: &Path, roots: Vec<PathBuf>) -> Self {
        Self::new(
            workspace,
            Arc::new(move || {
                let mut all = roots.clone();
                all.extend(desk_session_folders());
                all
            }),
        )
    }

    /// Whether writing or deleting `path` touches code: inside a git work
    /// tree (other than one the workspace itself sits in), a code root, or
    /// a Desk session's folder. Links are followed first.
    fn touches_code(&self, path: &Path) -> bool {
        let path = resolved(path);
        let workspace = resolved(&self.workspace);
        if (self.code)()
            .iter()
            .map(|root| resolved(root))
            .any(|root| path.starts_with(&root))
        {
            return true;
        }
        // A repo of its own inside the workspace counts; a repo the
        // workspace itself lives in doesn't make its notes code.
        let stop = path.starts_with(&workspace).then_some(workspace.as_path());
        path.ancestors()
            .take_while(|dir| stop.is_none_or(|stop| dir.starts_with(stop) && *dir != stop))
            .any(|dir| dir.join(".git").exists())
    }

    /// Why a call of a `held` tool acting as `effect` may not run now.
    fn refusal(&self, held: Held, effect: &Effect) -> Option<&'static str> {
        let refused = match held {
            Held::Coding => !reads_only(effect),
            Held::Shell => true,
            Held::Files => match effect {
                Effect::None | Effect::Read { .. } => false,
                Effect::Write { paths } | Effect::Delete { paths } => {
                    paths.iter().any(|p| self.touches_code(p))
                }
                Effect::Move { from, to } => from.iter().chain([to]).any(|p| self.touches_code(p)),
                Effect::Exec { .. }
                | Effect::Session { .. }
                | Effect::Connector { .. }
                | Effect::Unknown => true,
            },
        };
        (refused && always_sessions(&self.workspace)).then_some(ALWAYS_SESSIONS)
    }
}

/// `path` with its links followed: the deepest part that exists is
/// canonicalised and the rest kept, so a file not yet written resolves
/// through a linked folder.
fn resolved(path: &Path) -> PathBuf {
    let path: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let mut rest = Vec::new();
    let mut base = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return path,
        }
    }
}

/// The folders Desk's sessions run in, on a hub running Desk.
fn desk_session_folders() -> Vec<PathBuf> {
    #[cfg(feature = "postgres")]
    if let Some(desk) = crate::desk_driver::desk() {
        return desk
            .list()
            .into_iter()
            .map(|s| PathBuf::from(s.cwd))
            .collect();
    }
    Vec::new()
}

/// Hold the shell and file tools in `tools` to the switch, in place (the
/// ones the registry was built with). Coding tools are held as they are
/// registered.
pub fn hold_registered(tools: &workforce_agent::tools::registry::ToolRegistry, bounds: &Bounds) {
    for name in tools.names() {
        let general = matches!(Held::of(&name), Some(Held::Shell | Held::Files));
        if let Some(tool) = tools.get(&name).filter(|_| general) {
            tools.register(SessionsOnly::wrap(tool, bounds));
        }
    }
}

/// `service`'s bounds: its workspace, the hub's worktrees and project
/// checkouts, and Desk's session folders.
pub fn bounds_of(service: &crate::serve::AgentService) -> Bounds {
    let roots = crate::serve::compose::tool_roots(&service.config, &service.workspace);
    Bounds::of_agent(&service.workspace, roots.into_iter().skip(1).collect())
}

/// A held tool that refuses its working calls while its agent always works
/// in real sessions.
pub struct SessionsOnly {
    inner: Arc<dyn Tool>,
    held: Held,
    bounds: Bounds,
}

impl SessionsOnly {
    /// `tool`, held to the switch within `bounds` when it is a held tool
    /// ([`is_held`]); any other tool as it is.
    pub fn wrap(tool: Arc<dyn Tool>, bounds: &Bounds) -> Arc<dyn Tool> {
        let Some(held) = Held::of(tool.name()) else {
            return tool;
        };
        Arc::new(Self {
            inner: tool,
            held,
            bounds: bounds.clone(),
        })
    }

    /// Why the call with `input` may not run now, if it may not.
    fn refusal(&self, input: &serde_json::Value) -> Option<&'static str> {
        self.bounds.refusal(self.held, &self.inner.effect(input))
    }
}

#[async_trait]
impl Tool for SessionsOnly {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        if let Some(why) = self.refusal(&input) {
            tracing::info!(
                tool = self.inner.name(),
                "coding work refused in-process: the agent always works in real sessions"
            );
            return Ok(ToolResult::failure(why, start.elapsed().as_millis() as u64));
        }
        self.inner.execute(input).await
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        self.inner.soft_deadline()
    }

    fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
        self.inner.approval_summary(input)
    }

    fn approval_requirement(&self) -> Option<String> {
        self.inner.approval_requirement()
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        self.inner.surface_effect(input)
    }

    fn effect(&self, input: &serde_json::Value) -> Effect {
        self.inner.effect(input)
    }

    async fn collect(
        &self,
        input: serde_json::Value,
        plan: serde_json::Value,
        checkpoint: Option<String>,
    ) -> Result<ToolResult> {
        self.inner.collect(input, plan, checkpoint).await
    }

    async fn abandon(&self, plan: serde_json::Value, checkpoint: Option<String>) {
        self.inner.abandon(plan, checkpoint).await
    }

    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_agent::tools::registry::ToolRegistry;

    /// A tool that says what each call would do, and runs it.
    struct Probe {
        name: &'static str,
        effect: fn(&serde_json::Value) -> Effect,
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult::success(json!("ran"), 0))
        }
        fn effect(&self, input: &serde_json::Value) -> Effect {
            (self.effect)(input)
        }
    }

    fn paths(input: &serde_json::Value) -> Vec<PathBuf> {
        vec![PathBuf::from(
            input["path"].as_str().unwrap_or("/nowhere/f"),
        )]
    }
    fn write(input: &serde_json::Value) -> Effect {
        Effect::Write {
            paths: paths(input),
        }
    }
    fn delete(input: &serde_json::Value) -> Effect {
        Effect::Delete {
            paths: paths(input),
        }
    }
    fn exec(_: &serde_json::Value) -> Effect {
        Effect::Exec {
            command: "cargo test".into(),
            cwd: None,
        }
    }
    fn session(_: &serde_json::Value) -> Effect {
        Effect::Session { cwd: None }
    }
    fn read(input: &serde_json::Value) -> Effect {
        Effect::Read {
            paths: paths(input),
        }
    }
    fn git(input: &serde_json::Value) -> Effect {
        match input["action"].as_str() {
            Some("status" | "log") => read(input),
            _ => exec(input),
        }
    }
    fn unknown(_: &serde_json::Value) -> Effect {
        Effect::Unknown
    }

    const ON: &str =
        "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = true\n";

    /// An agent's workspace holding `config`, a git checkout, a hub
    /// worktrees folder and a Desk session folder beside it.
    struct Place {
        _dir: tempfile::TempDir,
        root: PathBuf,
        bounds: Bounds,
    }

    impl Place {
        fn new(config: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let workspace = root.join("agent");
            std::fs::create_dir_all(workspace.join("notes")).unwrap();
            std::fs::write(workspace.join("config.toml"), config).unwrap();
            std::fs::create_dir_all(root.join("repo/.git")).unwrap();
            std::fs::create_dir_all(root.join("repo/src")).unwrap();
            std::fs::create_dir_all(workspace.join("vendored/.git")).unwrap();
            std::fs::create_dir_all(root.join("worktrees/wt-1")).unwrap();
            std::fs::create_dir_all(root.join("session-folder")).unwrap();
            std::fs::create_dir_all(root.join("elsewhere")).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.join("repo/src"), workspace.join("link-to-src"))
                .unwrap();
            let code_roots = vec![root.join("worktrees"), root.join("session-folder")];
            let bounds = Bounds::new(&workspace, Arc::new(move || code_roots.clone()));
            Self {
                _dir: dir,
                root,
                bounds,
            }
        }

        fn set(&self, config: &str) {
            std::fs::write(self.root.join("agent/config.toml"), config).unwrap();
        }

        fn path(&self, rel: &str) -> String {
            self.root.join(rel).to_string_lossy().into_owned()
        }

        fn tool(
            &self,
            name: &'static str,
            effect: fn(&serde_json::Value) -> Effect,
        ) -> Arc<dyn Tool> {
            SessionsOnly::wrap(Arc::new(Probe { name, effect }), &self.bounds)
        }

        async fn runs(
            &self,
            name: &'static str,
            effect: fn(&serde_json::Value) -> Effect,
            input: serde_json::Value,
        ) -> bool {
            let result = self.tool(name, effect).execute(input).await.unwrap();
            if !result.success {
                assert_eq!(result.error.as_deref(), Some(ALWAYS_SESSIONS), "{name}");
            }
            result.success
        }
    }

    /// A tool's name, how it says a call acts, and the call.
    type Call = (
        &'static str,
        fn(&serde_json::Value) -> Effect,
        serde_json::Value,
    );

    /// Every held tool's working call: refused while the switch is on.
    fn working_calls(at: &Place) -> Vec<Call> {
        vec![
            ("coding.session", session, json!({"task": "fix it"})),
            ("coding.fanout", session, json!({"tasks": ["a", "b"]})),
            ("coding.claude", session, json!({"prompt": "fix it"})),
            (
                "code.edit",
                write,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            (
                "code.find_replace",
                write,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            ("code.refactor", write, json!({"action": "rename"})),
            ("code.build", exec, json!({})),
            ("code.cargo", exec, json!({"command": "build"})),
            ("code.test", exec, json!({})),
            ("code.lint", exec, json!({})),
            ("code.git", git, json!({"action": "commit", "message": "m"})),
            ("code.anything_new", unknown, json!({})),
            ("exec.run", exec, json!({"command": "ls"})),
            ("exec.run", read, json!({"path": at.path("agent/notes")})),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            (
                "filesystem.edit",
                write,
                json!({"path": at.path("repo/new/dir/mod.rs")}),
            ),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("worktrees/wt-1/a.rs")}),
            ),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("session-folder/a.rs")}),
            ),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("agent/vendored/x.rs")}),
            ),
            (
                "filesystem.delete",
                delete,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            (
                "fs.remove",
                delete,
                json!({"path": at.path("session-folder/a.rs")}),
            ),
            ("filesystem.write", unknown, json!({})),
            #[cfg(unix)]
            (
                "filesystem.write",
                write,
                json!({"path": at.path("agent/link-to-src/lib.rs")}),
            ),
        ]
    }

    #[tokio::test]
    async fn every_held_call_is_refused_while_the_agent_always_works_in_sessions() {
        let at = Place::new(ON);
        for (name, effect, input) in working_calls(&at) {
            assert!(
                !at.runs(name, effect, input.clone()).await,
                "{name} {input} ran"
            );
        }
    }

    #[tokio::test]
    async fn every_held_call_runs_with_the_switch_off_or_at_watch() {
        for config in [
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = false\n",
            "[oversight]\nenabled = false\n[oversight.work]\nprefer_sessions = true\n",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n[oversight.work]\nprefer_sessions = true\n",
            "",
        ] {
            let at = Place::new(config);
            for (name, effect, input) in working_calls(&at) {
                assert!(at.runs(name, effect, input).await, "{name} refused under {config:?}");
            }
        }
    }

    #[tokio::test]
    async fn reads_and_the_agents_own_notes_still_run_under_the_switch() {
        let at = Place::new(ON);
        let allowed: Vec<Call> = vec![
            (
                "code.read",
                read,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            ("code.search", read, json!({})),
            ("code.tree", read, json!({})),
            ("code.diff", read, json!({})),
            ("code.git", git, json!({"action": "status"})),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("agent/notes/today.md")}),
            ),
            (
                "filesystem.edit",
                write,
                json!({"path": at.path("agent/memory/new.md")}),
            ),
            (
                "filesystem.delete",
                delete,
                json!({"path": at.path("agent/notes/old.md")}),
            ),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("elsewhere/list.txt")}),
            ),
            ("filesystem.write", read, json!({})),
            ("memory.remember", unknown, json!({})),
        ];
        for (name, effect, input) in allowed {
            assert!(
                at.runs(name, effect, input.clone()).await,
                "{name} {input} refused"
            );
        }
    }

    #[tokio::test]
    async fn the_switch_is_read_at_each_call() {
        let at = Place::new("");
        let input = json!({"path": at.path("repo/src/lib.rs")});
        assert!(at.runs("code.edit", write, input.clone()).await);
        at.set(ON);
        assert!(!at.runs("code.edit", write, input).await);
    }

    #[tokio::test]
    async fn a_registry_built_with_a_shell_and_file_tools_has_them_held() {
        let at = Place::new(ON);
        let registry = ToolRegistry::new();
        for (name, effect) in [
            ("exec.run", exec as fn(&serde_json::Value) -> Effect),
            ("filesystem.write", write),
            ("filesystem.read", read),
            ("memory.remember", write),
        ] {
            registry.register(Arc::new(Probe { name, effect }));
        }
        hold_registered(&registry, &at.bounds);
        let call = |name: &str, input: serde_json::Value| {
            let tool = registry.get(name).unwrap();
            async move { tool.execute(input).await.unwrap().success }
        };
        assert!(!call("exec.run", json!({})).await);
        assert!(!call("filesystem.write", json!({"path": at.path("repo/a.rs")})).await);
        assert!(
            call(
                "filesystem.write",
                json!({"path": at.path("agent/notes/a.md")})
            )
            .await
        );
        assert!(call("filesystem.read", json!({})).await);
        assert!(call("memory.remember", json!({})).await, "not a held tool");
        assert!(is_held("coding.session") && is_held("exec.run") && is_held("fs.remove"));
        assert!(!is_held("crew.start_session") && !is_held("filesystem.read"));
    }
}

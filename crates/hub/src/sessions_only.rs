//! How the boss works (`[oversight.work] style`, [`WorkStyle`]): an agent
//! that oversees the crew (`[oversight] enabled`, at a level that starts
//! sessions) does its work in real sessions the person can watch, type into
//! and take over, as its style says.
//!
//! **Only in sessions** refuses, with a pointer to `crew.start_session`:
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
//! **Prefer sessions** refuses the coding tools' working calls and the file
//! writes inside code alike, but lets commands run: ssh, gh, reading logs,
//! a quick check. A command that is itself coding work is refused as one:
//! a build, a test or a package install (`cargo build`, `npm test`,
//! `make`), a change made with git (`git commit`, `git apply`), an edit in
//! place (`sed -i`), or a write, move or delete (`>`, `tee`, `rm`, `mv`,
//! `cp`) of a path inside code. A command run on another machine over ssh
//! is that machine's ops, and runs. What a command may do is judged beyond
//! this as it is for any agent (its approvals, exec containment); the
//! person's directives say what is theirs. **Do it myself** refuses
//! nothing.
//!
//! What a call does decides as much as what its tool is called: any tool
//! whose call runs a command is a command, and any whose call writes,
//! deletes or moves a path inside code (an MCP server's, a WASM skill's) is
//! a change to code.
//!
//! What only reads still runs, and so do writes elsewhere in the agent's own
//! workspace (its notes, memory and scratch files). Workflow coding steps and
//! pilot rounds run their own CLIs, not these tools, and are not held.
//!
//! The style is read from the agent's config at each call, so changing it
//! applies to the next call. It is enforced in the tool itself
//! ([`SessionsOnly`]), so every path a call takes — a turn, a worker
//! sub-agent's snapshot, a CLI turn's gated MCP — meets it.

mod command;

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{Tool, ToolDefinition, ToolResult};
use workforce_common::oversight::{Level, Oversight, WorkStyle};
use workforce_core::computer::Effect;

/// What a held call would do, for which style refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doing {
    /// It runs a command: a shell's, or any tool's that runs one.
    Command,
    /// It changes code, or is a coding tool's working call.
    Code,
    /// A sub-agent that would do work rather than look things up.
    Helper,
}

/// The words a refusal starts with, for the style that refuses it.
fn says(style: WorkStyle) -> &'static str {
    match style {
        WorkStyle::PreferSessions => {
            "Desk's “How it works” setting for you is Prefer sessions: changing code and long \
             tasks go to a real session the person can watch; start one with \
             crew.start_session. Checks, lookups and ops you may still run yourself (ssh, gh, \
             reading files and logs, a quick command), and sub-agents with profile \"worker\"."
        }
        _ => {
            "Desk's “How it works” setting for you is Only in sessions: all work goes to a real \
             session the person can watch; start one with crew.start_session. Here only \
             read-only lookups run: reads, search, notes in the agent's own workspace, and \
             sub-agents with profile \"worker\"."
        }
    }
}

/// The option that would let what is refused run, named for the person.
fn allows(style: WorkStyle, doing: Doing) -> &'static str {
    match (style, doing) {
        (WorkStyle::SessionsOnly, Doing::Command) => {
            " “Prefer sessions” would let you run checks, lookups and ops like this yourself; \
             tell the person if that's what they want."
        }
        _ => " Only “Do it myself” would let you do this here; tell the person if that's what they want.",
    }
}

/// What follows when the session it is pointed to would only be
/// suggested: at Suggest, or at Act with “Hand out work” off.
fn then(o: &Oversight) -> &'static str {
    match o.level {
        Level::Suggest => " At Suggest, the start is a suggestion.",
        Level::Act | Level::Full if !o.act.hand_out_work => {
            " “Hand out work” is off, so the start is a suggestion; tell the person turning \
             “Hand out work” on lets you start sessions."
        }
        _ => "",
    }
}

fn oversight_of(workspace: &Path) -> Oversight {
    workforce_common::settings::AgentWorkspaceConfig::load(workspace)
        .ok()
        .flatten()
        .and_then(|c| c.oversight().ok())
        .unwrap_or_default()
}

/// What a call doing `doing` is told, for the agent whose workspace is
/// `workspace`: `None` when its style lets it run. The refusal names the
/// setting, the option that would let it, and whether the session it is
/// pointed to would only be suggested.
pub fn always_sessions_refusal(workspace: &Path, doing: Doing) -> Option<String> {
    refusal_for(&oversight_of(workspace), doing)
}

/// [`always_sessions_refusal`] for the oversight `o`.
pub fn refusal_for(o: &Oversight, doing: Doing) -> Option<String> {
    if !o.enabled || o.level == Level::Watch {
        return None;
    }
    let refused = match o.work.style {
        WorkStyle::SessionsOnly => true,
        WorkStyle::PreferSessions => doing != Doing::Command,
        WorkStyle::Direct => false,
    };
    refused.then(|| {
        format!(
            "{}{}{}",
            says(o.work.style),
            allows(o.work.style, doing),
            then(o)
        )
    })
}

/// What a held tool is, for what of its calls is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// `code.*`, `coding.*`: refused unless the call only reads.
    Coding,
    /// `exec.*`: refused when only in sessions; preferring sessions, when
    /// the command is coding work ([`Bounds::command_is_code`]).
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
            "filesystem.write"
                | "filesystem.edit"
                | "filesystem.delete"
                | "filesystem.trash"
                | "fs.remove"
        )
        .then_some(Self::Files)
    }
}

/// Whether `name` is a tool held to the boss's style: a coding tool, a
/// shell, or a file write.
pub fn is_held(name: &str) -> bool {
    Held::of(name).is_some()
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
        let roots: Vec<PathBuf> = (self.code)()
            .iter()
            .map(|root| folded_buf(resolved(root)))
            .collect();
        self.touches_code_in(path, &roots)
    }

    /// [`Self::touches_code`] with the code roots read, resolved and folded.
    fn touches_code_in(&self, path: &Path, roots: &[PathBuf]) -> bool {
        let path = resolved(path);
        let workspace = resolved(&self.workspace);
        let folded = folded(&path);
        if roots.iter().any(|root| folded.starts_with(root)) {
            return true;
        }
        // A repo of its own inside the workspace counts; a repo the
        // workspace itself lives in doesn't make its notes code.
        let stop = path.starts_with(&workspace).then_some(workspace.as_path());
        path.ancestors()
            .take_while(|dir| stop.is_none_or(|stop| dir.starts_with(stop) && *dir != stop))
            .any(|dir| dir.join(".git").exists())
    }

    /// Why a call of a tool `held` names (none for any other tool) acting
    /// as `effect` may not run now. What a call does decides, whatever its
    /// tool is called: a command it runs is a command, and a write, delete
    /// or move that touches code a change to code. A coding tool is a
    /// change to code besides for what it can't say it only reads.
    fn refusal(&self, held: Option<Held>, effect: &Effect) -> Option<Cow<'static, str>> {
        // Most calls are none of this: say so before reading anything.
        let may_be = matches!(held, Some(Held::Shell))
            || match effect {
                Effect::None | Effect::Read { .. } => false,
                Effect::Exec { .. }
                | Effect::Write { .. }
                | Effect::Delete { .. }
                | Effect::Trash { .. }
                | Effect::Move { .. } => true,
                Effect::Session { .. } | Effect::Connector { .. } | Effect::Unknown => {
                    held.is_some()
                }
            };
        if !may_be {
            return None;
        }
        let o = oversight_of(&self.workspace);
        // Nothing to judge when its style refuses neither.
        refusal_for(&o, Doing::Code)?;
        let touches = |paths: &[&PathBuf]| paths.iter().any(|p| self.touches_code(p));
        let doing = match (held, effect) {
            // A coding tool's build or test is coding work, never a check.
            (Some(Held::Coding), Effect::None | Effect::Read { .. }) => None,
            (Some(Held::Coding), _) => Some(Doing::Code),
            (_, Effect::Exec { command, cwd }) => {
                Some(match self.command_is_code(command, cwd.as_deref()) {
                    true => Doing::Code,
                    false => Doing::Command,
                })
            }
            (Some(Held::Shell), _) => Some(Doing::Command),
            (_, Effect::None | Effect::Read { .. }) => None,
            (_, Effect::Write { paths } | Effect::Delete { paths } | Effect::Trash { paths }) => {
                touches(&paths.iter().collect::<Vec<_>>()).then_some(Doing::Code)
            }
            (_, Effect::Move { from, to }) => {
                touches(&from.iter().chain([to]).collect::<Vec<_>>()).then_some(Doing::Code)
            }
            (Some(Held::Files), _) => Some(Doing::Code),
            (None, Effect::Session { .. } | Effect::Connector { .. } | Effect::Unknown) => None,
        }?;
        refusal_for(&o, doing).map(Cow::Owned)
    }
}

impl Bounds {
    /// Whether the shell command `command`, run in `cwd` (the workspace
    /// when not said, a relative one from it), is coding work rather than a
    /// check ([`command::is_code`]). The code roots are read once for it.
    fn command_is_code(&self, command: &str, cwd: Option<&Path>) -> bool {
        let here = match cwd {
            Some(c) if c.is_absolute() => c.to_path_buf(),
            Some(c) => self.workspace.join(c),
            None => self.workspace.clone(),
        };
        let roots: Vec<PathBuf> = (self.code)()
            .iter()
            .map(|root| folded_buf(resolved(root)))
            .collect();
        command::is_code(command, &here, &|p: &Path| self.touches_code_in(p, &roots))
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

/// `path` as compared on this computer: on macOS, whose disks ignore case,
/// lowercased, so `~/Code/App` and `~/code/app` are one folder.
fn folded(path: &Path) -> PathBuf {
    folded_buf(path.to_path_buf())
}

fn folded_buf(path: PathBuf) -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path
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

/// Hold every tool `tools` has and is given later — and every copy of it
/// takes — to its style within `bounds`: MCP, WASM and skill tools as much
/// as the hub's own, since what a call does decides.
pub fn hold(tools: &workforce_agent::tools::registry::ToolRegistry, bounds: &Bounds) {
    let bounds = bounds.clone();
    tools.guard_with(Arc::new(move |tool| SessionsOnly::wrap(tool, &bounds)));
}

/// A tool that refuses its working calls while its agent always works in
/// real sessions.
pub struct SessionsOnly {
    inner: Arc<dyn Tool>,
    held: Option<Held>,
    bounds: Bounds,
}

impl SessionsOnly {
    /// `tool`, held to the boss's style within `bounds`.
    pub fn wrap(tool: Arc<dyn Tool>, bounds: &Bounds) -> Arc<dyn Tool> {
        Arc::new(Self {
            held: Held::of(tool.name()),
            inner: tool,
            bounds: bounds.clone(),
        })
    }

    /// Why the call with `input` may not run now, if it may not.
    fn refusal(&self, input: &serde_json::Value) -> Option<Cow<'static, str>> {
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
                "work refused in-process: the agent's style sends it to real sessions"
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

    fn approval_notice(&self) -> Option<String> {
        self.inner.approval_notice()
    }

    workforce_agent::delegate_call_facts!(inner);

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
    /// A command as its input says it (`cargo test` when not said), run
    /// where `workdir` says.
    fn exec(input: &serde_json::Value) -> Effect {
        Effect::Exec {
            command: input["command"].as_str().unwrap_or("cargo test").into(),
            cwd: input["workdir"].as_str().map(PathBuf::from),
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
    const OFF: &str =
        "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = false\n";
    const PREFER: &str =
        "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nstyle = \"prefer_sessions\"\n";

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
                let said = result.error.as_deref().unwrap_or_default();
                assert!(
                    said.starts_with("Desk's “How it works” setting"),
                    "{name}: {said}"
                );
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

    /// Every held tool's working call: refused only in sessions.
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

    /// Prefer sessions: a command runs (ssh, gh, a log read), while a
    /// change to code and a coding tool's working call still go to a
    /// session, each refusal naming the option that would let it.
    #[tokio::test]
    async fn preferring_sessions_runs_commands_and_still_sends_code_to_a_session() {
        let at = Place::new(PREFER);
        for (name, effect, input) in [
            (
                "exec.run",
                exec as fn(&serde_json::Value) -> Effect,
                json!({"command": "ssh devbox uptime"}),
            ),
            ("exec.run", read, json!({"path": at.path("agent/notes")})),
            ("mcp__ops__shell", exec, json!({"command": "gh pr list"})),
            (
                "code.read",
                read,
                json!({"path": at.path("repo/src/lib.rs")}),
            ),
            (
                "filesystem.write",
                write,
                json!({"path": at.path("agent/notes/a.md")}),
            ),
        ] {
            assert!(
                at.runs(name, effect, input.clone()).await,
                "{name} {input} refused"
            );
        }
        // Reads and ops over a repo run, and a command on another machine.
        let repo = at.path("repo");
        for command in [
            "git log -5",
            "git status && git diff",
            "cat src/lib.rs | grep fn > /tmp/fns.txt",
            "ssh devbox 'cd ~/src/vellev && cargo build && make install'",
            "gh pr checks 12 2>&1",
        ] {
            let input = json!({"command": command, "workdir": repo});
            assert!(at.runs("exec.run", exec, input).await, "{command} refused");
        }
        let mut changes: Vec<Call> = working_calls(&at)
            .into_iter()
            .filter(|(name, ..)| !name.starts_with("exec."))
            .collect();
        assert!(changes.len() > 10);
        // A command that is itself coding work goes to a session too: a
        // build or test, a change made with git, an edit in place, or a
        // write, move or delete inside code.
        for command in [
            "cargo build --release",
            "npm run build",
            "git commit -am 'fix'",
            "FOO=1 sudo make",
            "sed -i 's/a/b/' src/lib.rs",
            "echo x > src/lib.rs",
            "echo x >>src/lib.rs",
            "cat a | tee src/new.rs",
            "rm -rf src",
            "cd src && git apply p.diff",
        ] {
            changes.push((
                "exec.run",
                exec,
                json!({"command": command, "workdir": repo}),
            ));
        }
        changes.push((
            "mcp__ops__shell",
            exec,
            json!({"command": "cp /tmp/a.rs src/a.rs", "workdir": repo}),
        ));
        for (name, effect, input) in changes {
            let r = at.tool(name, effect).execute(input.clone()).await.unwrap();
            assert!(!r.success, "{name} {input} ran");
            let said = r.error.unwrap_or_default();
            assert!(
                said.contains("Prefer sessions") && said.contains("“Do it myself” would let you"),
                "{name}: {said}"
            );
        }
        // Only in sessions refuses the command, naming Prefer sessions.
        at.set(ON);
        let r = at
            .tool("exec.run", exec)
            .execute(json!({"command": "ssh devbox uptime"}))
            .await
            .unwrap();
        let said = r.error.unwrap_or_default();
        assert!(!r.success && said.contains("Only in sessions"), "{said}");
        assert!(
            said.contains("“Prefer sessions” would let you run"),
            "{said}"
        );
    }

    /// A config from before `style` keeps working as it did: its switch on
    /// is Only in sessions, off is Do it myself; `style` wins over it.
    #[tokio::test]
    async fn an_old_config_works_as_it_did_and_style_wins_over_it() {
        for (config, ssh_runs, edit_runs) in [
            (ON, false, false),
            (OFF, true, true),
            ("[oversight]\nenabled = true\nlevel = \"act\"\n", false, false),
            (
                "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = true\nstyle = \"prefer_sessions\"\n",
                true,
                false,
            ),
            (
                "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = true\nstyle = \"direct\"\n",
                true,
                true,
            ),
        ] {
            let at = Place::new(config);
            let ssh = at.runs("exec.run", exec, json!({"command": "ssh devbox"})).await;
            let edit = at
                .runs("code.edit", write, json!({"path": at.path("repo/src/lib.rs")}))
                .await;
            assert_eq!((ssh, edit), (ssh_runs, edit_runs), "{config}");
        }
    }

    #[tokio::test]
    async fn every_held_call_runs_with_the_switch_off_or_at_watch() {
        for config in [
            OFF,
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nstyle = \"direct\"\n",
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
    async fn any_tool_is_held_by_what_its_call_does_whatever_its_name() {
        let at = Place::new(ON);
        // An MCP server's shell, a WASM skill's build: they run commands.
        assert!(!at.runs("mcp__ops__shell", exec, json!({})).await);
        assert!(!at.runs("wasm_skill.build", exec, json!({})).await);
        // An MCP server's file write lands in code; elsewhere it runs.
        assert!(
            !at.runs(
                "mcp__fs__write_file",
                write,
                json!({"path": at.path("repo/src/a.rs")})
            )
            .await
        );
        assert!(
            !at.runs(
                "wasm_skill.scaffold",
                write,
                json!({"path": at.path("worktrees/wt-1/s")})
            )
            .await
        );
        assert!(
            at.runs(
                "mcp__fs__write_file",
                write,
                json!({"path": at.path("agent/notes/a.md")})
            )
            .await
        );
        // Starting a session, a connector, a tool that names nothing: run.
        assert!(at.runs("crew.start_session", session, json!({})).await);
        assert!(at.runs("memory.remember", unknown, json!({})).await);
    }

    /// The refusal points to a session it starts, names the setting and
    /// the option that would let the call, and says when that start is
    /// only a suggestion and which switch would let it start it itself.
    #[test]
    fn the_refusal_says_when_the_session_it_points_to_is_only_suggested() {
        let at = |config: &str, doing: Doing| {
            let place = Place::new(config);
            always_sessions_refusal(&place.bounds.workspace, doing)
        };
        let act = at(ON, Doing::Code).expect("Act starts sessions by default");
        assert!(act.contains("crew.start_session") && act.contains("Only in sessions"));
        assert!(!act.contains("is a suggestion"), "{act}");
        let off = at(
            &format!("{ON}[oversight.act]\nhand_out_work = false\n"),
            Doing::Code,
        )
        .unwrap();
        assert!(off.contains(workforce_common::oversight::switch::HAND_OUT_WORK));
        assert!(off.contains("the start is a suggestion"), "{off}");
        let suggest = at(
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n",
            Doing::Code,
        )
        .unwrap();
        assert!(
            suggest.ends_with("At Suggest, the start is a suggestion."),
            "{suggest}"
        );
        assert_eq!(
            at(
                "[oversight]\nenabled = true\nlevel = \"watch\"\n",
                Doing::Code
            ),
            None
        );
        assert_eq!(at(OFF, Doing::Code), None);
        // A helper doing work goes to a session in both styles that prefer them.
        assert!(at(PREFER, Doing::Helper).is_some());
        assert_eq!(at(PREFER, Doing::Command), None);
    }

    #[tokio::test]
    async fn a_registry_holds_every_tool_it_has_and_gets_and_its_copies_do_too() {
        let at = Place::new(ON);
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(Probe {
            name: "exec.run",
            effect: exec,
        }));
        hold(&registry, &at.bounds);
        registry.register(Arc::new(Probe {
            name: "mcp__fs__write_file",
            effect: write,
        }));
        let copy = ToolRegistry::follow(&registry, |_| true);
        copy.register(Arc::new(Probe {
            name: "exec.run",
            effect: exec,
        }));
        let runs = |registry: &ToolRegistry, name: &str, input: serde_json::Value| {
            let tool = registry.get(name).unwrap();
            async move { tool.execute(input).await.unwrap().success }
        };
        assert!(!runs(&registry, "exec.run", json!({})).await);
        assert!(
            !runs(
                &registry,
                "mcp__fs__write_file",
                json!({"path": at.path("repo/a.rs")})
            )
            .await
        );
        assert!(
            runs(
                &registry,
                "mcp__fs__write_file",
                json!({"path": at.path("agent/notes/a.md")})
            )
            .await
        );
        assert!(
            !runs(&copy, "exec.run", json!({})).await,
            "a turn's own exec.run"
        );
        assert!(is_held("coding.session") && is_held("exec.run") && is_held("fs.remove"));
        assert!(!is_held("crew.start_session") && !is_held("filesystem.read"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_code_folder_named_in_another_case_is_the_same_folder_on_macos() {
        let at = Place::new(ON);
        let upper = at.path("Session-Folder/a.rs");
        assert!(
            !at.runs("mcp__fs__write_file", write, json!({"path": upper}))
                .await
        );
    }
}

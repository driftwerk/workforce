//! "Always work in real sessions": an agent that oversees the crew
//! (`[oversight] enabled`, at a level that starts sessions) with
//! `[oversight.work] prefer_sessions` does no coding work in-process. Every
//! call of a coding tool that changes or runs something — an edit, a build,
//! a test, a commit, a coding session or fan-out of its own — is refused
//! with a pointer to `crew.start_session`, so the work always happens in a
//! session the person can watch, type into and take over. A call that only
//! reads (a file, a search, a diff, `git status`) still runs.
//!
//! The switch is read from the agent's config at each call, so turning it
//! on or off applies to the next call. It is enforced in the tool itself
//! ([`SessionsOnly`]), so every path a call takes — a turn, a CLI turn's
//! gated MCP, a workflow step — meets it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{SurfaceEffect, Tool, ToolDefinition, ToolResult};
use workforce_core::computer::Effect;

/// Why a call is refused while the switch is on: the same words for a
/// coding tool and for a sub-agent that would do the work.
pub const ALWAYS_SESSIONS: &str = "Desk is set to always work in real sessions: start one \
with crew.start_session so the person can watch it. Here only read-only lookups run: reads, \
search, and sub-agents with profile \"worker\".";

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

/// Whether `name` is a tool that does coding work in-process: the native
/// `code.*` tools and the `coding.*` sessions, fan-outs and CLI calls.
pub fn is_coding_tool(name: &str) -> bool {
    name.starts_with("code.") || name.starts_with("coding.")
}

/// Whether a call with `effect` only reads.
fn reads_only(effect: &Effect) -> bool {
    matches!(effect, Effect::None | Effect::Read { .. })
}

/// A coding tool that refuses its working calls while its agent always
/// works in real sessions.
pub struct SessionsOnly {
    inner: Arc<dyn Tool>,
    workspace: PathBuf,
}

impl SessionsOnly {
    /// `tool`, held to the switch of the agent at `workspace` when it is a
    /// coding tool ([`is_coding_tool`]); any other tool as it is.
    pub fn wrap(tool: Arc<dyn Tool>, workspace: &Path) -> Arc<dyn Tool> {
        if !is_coding_tool(tool.name()) {
            return tool;
        }
        Arc::new(Self {
            inner: tool,
            workspace: workspace.to_path_buf(),
        })
    }

    /// Why the call with `input` may not run now, if it may not.
    fn refusal(&self, input: &serde_json::Value) -> Option<&'static str> {
        (!reads_only(&self.inner.effect(input)) && always_sessions(&self.workspace))
            .then_some(ALWAYS_SESSIONS)
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

    /// A coding tool that says what each call would do, and runs it.
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

    fn write(_: &serde_json::Value) -> Effect {
        Effect::Write {
            paths: vec![PathBuf::from("/w/src/lib.rs")],
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
    fn read(_: &serde_json::Value) -> Effect {
        Effect::Read {
            paths: vec![PathBuf::from("/w/src/lib.rs")],
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

    fn workspace(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        dir
    }

    const ON: &str =
        "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = true\n";

    /// A tool's name, how it says a call acts, and the call.
    type Call = (
        &'static str,
        fn(&serde_json::Value) -> Effect,
        serde_json::Value,
    );

    /// Every coding tool, as each would say a working call of it acts.
    fn working_calls() -> Vec<Call> {
        vec![
            ("coding.session", session, json!({"task": "fix it"})),
            ("coding.fanout", session, json!({"tasks": ["a", "b"]})),
            ("coding.claude", session, json!({"prompt": "fix it"})),
            ("code.edit", write, json!({"path": "src/lib.rs"})),
            (
                "code.find_replace",
                write,
                json!({"find": "a", "replace": "b"}),
            ),
            ("code.refactor", write, json!({"action": "rename"})),
            ("code.build", exec, json!({})),
            ("code.cargo", exec, json!({"command": "build"})),
            ("code.test", exec, json!({})),
            ("code.lint", exec, json!({})),
            ("code.git", git, json!({"action": "commit", "message": "m"})),
            ("code.anything_new", unknown, json!({})),
        ]
    }

    async fn run(tool: &Arc<dyn Tool>, input: serde_json::Value) -> ToolResult {
        tool.execute(input).await.unwrap()
    }

    #[tokio::test]
    async fn every_coding_tool_is_refused_while_the_agent_always_works_in_sessions() {
        let dir = workspace(ON);
        for (name, effect, input) in working_calls() {
            let tool = SessionsOnly::wrap(Arc::new(Probe { name, effect }), dir.path());
            let result = run(&tool, input).await;
            assert!(!result.success, "{name} ran");
            assert_eq!(result.error.as_deref(), Some(ALWAYS_SESSIONS), "{name}");
            assert_eq!(tool.name(), name, "it keeps its name");
        }
    }

    #[tokio::test]
    async fn every_coding_tool_runs_with_the_switch_off_or_at_watch() {
        let configs = [
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nprefer_sessions = false\n",
            "[oversight]\nenabled = false\n[oversight.work]\nprefer_sessions = true\n",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n[oversight.work]\nprefer_sessions = true\n",
            "",
        ];
        for config in configs {
            let dir = workspace(config);
            for (name, effect, input) in working_calls() {
                let tool = SessionsOnly::wrap(Arc::new(Probe { name, effect }), dir.path());
                let result = run(&tool, input).await;
                assert!(result.success, "{name} refused under {config:?}");
            }
        }
    }

    #[tokio::test]
    async fn read_only_lookups_still_run_under_the_switch() {
        let dir = workspace(ON);
        let reads: [Call; 7] = [
            ("code.read", read, json!({"path": "src/lib.rs"})),
            ("code.search", read, json!({"query": "fn main"})),
            ("code.find", read, json!({"pattern": "*.rs"})),
            ("code.tree", read, json!({})),
            ("code.symbols", read, json!({})),
            ("code.diff", read, json!({})),
            ("code.git", git, json!({"action": "status"})),
        ];
        for (name, effect, input) in reads {
            let tool = SessionsOnly::wrap(Arc::new(Probe { name, effect }), dir.path());
            assert!(run(&tool, input).await.success, "{name} refused");
        }
    }

    #[tokio::test]
    async fn the_switch_is_read_at_each_call() {
        let dir = workspace("");
        let tool = SessionsOnly::wrap(
            Arc::new(Probe {
                name: "code.edit",
                effect: write,
            }),
            dir.path(),
        );
        assert!(run(&tool, json!({})).await.success);
        std::fs::write(dir.path().join("config.toml"), ON).unwrap();
        assert!(!run(&tool, json!({})).await.success);
    }

    #[test]
    fn only_coding_tools_are_held_to_it() {
        let dir = workspace(ON);
        let registry = ToolRegistry::new();
        for name in ["code.edit", "memory.remember", "exec.run"] {
            registry.register(SessionsOnly::wrap(
                Arc::new(Probe {
                    name,
                    effect: write,
                }),
                dir.path(),
            ));
        }
        assert!(registry.has("code.edit"));
        assert!(is_coding_tool("coding.session"));
        assert!(!is_coding_tool("crew.start_session"));
        assert!(!is_coding_tool("exec.run"));
    }
}

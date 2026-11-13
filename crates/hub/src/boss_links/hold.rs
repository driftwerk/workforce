//! What a boss conversation's turns may do freely, and what each time only
//! with the person's say (remote bosses, RB6b; design §10, 5): nothing of the
//! group's work leaves the conversation unless the person lets it, call by
//! call.
//!
//! - **Kept** ([`KEPT`]): the few of the agent's own tools that neither send
//!   nor read outside the group, its fenced memory and reading back what this
//!   turn's tools returned. They run as anywhere else.
//! - **Held** ([`Held`]): every other tool the agent has. Each call asks the
//!   person in the conversation first, "Allan wants to use … from Acme's
//!   boss conversation", and runs only if they allow it, once: no "always",
//!   no session or folder grant ([`workforce_agent::tools::dispatch::options_for_tool`]),
//!   and a saved rule never stands in for it (a tool's own requirement is
//!   applied after policy). The design's hold list ([`HELD`]) is among them.
//! - **Not offered at all**: this hub's own crew (`crew.*` of the agent's
//!   own Desk; the crew a boss reaches is the link's), and every tool that
//!   hands work to another run ([`DELEGATING`]), whose run would hold
//!   nothing and stand outside the fence.
//!
//! What is kept is a list of what is safe, not of what sends, because an
//! agent's tools are open-ended (skills, MCP servers, WASM skills): a list of
//! what sends would miss whichever it didn't name. Nothing more is adopted
//! mid-turn, held or not.
//!
//! Memory is kept: in a boss conversation everything the agent remembers
//! is tagged with the group's tag and nothing else is recalled
//! ([`crate::memory_fence::fence_for_turn`]), so no memory write leaves the
//! fence; the `memory` tool also keeps it private there and refuses the
//! team and organisation layers.
//!
//! A tool is kept by what it is, not only by its name: it must be the hub's
//! own, as registered ([`ToolRegistry::is_builtin`]). A skill, an MCP server
//! or anything else registered under a kept name is held instead.

use std::sync::Arc;

use async_trait::async_trait;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{SurfaceEffect, Tool, ToolResult};

/// The design's hold list, as tool names: refused in a boss conversation
/// whatever [`KEPT`] says.
pub const HELD: &[&str] = &[
    "messaging.*",
    "agent.*",
    "agent_message",
    "email.*",
    "http.*",
    "github.*",
];

/// The agent's own tools a boss conversation keeps: its memory (fenced to
/// the group) and reading back what this turn's tools returned.
pub const KEPT: &[&str] = &[
    "memory",
    workforce_agent::spill::RETRIEVE_TOOL_NAME,
    workforce_agent::tools::data::DATA_INSPECT_TOOL_NAME,
    workforce_agent::tools::data::DATA_QUERY_TOOL_NAME,
];

/// The tools that hand work to another run: a sub-agent, another agent, a
/// workflow, a coding session, a scheduled or watching job, a reviewer. A
/// boss conversation never offers them, asked or not: one "Allow once"
/// would start a run of its own, with every tool unheld and outside the
/// group's fence.
pub const DELEGATING: &[&str] = &[
    "subagent",
    "investigate",
    "advisor",
    "research_report",
    "workflow",
    "workflow.*",
    "playbook",
    "playbook.*",
    "coding.*",
    "agent.*",
    "agent_message",
    "scheduler",
    "scheduler.*",
    "reminder",
    "heartbeat.*",
    "watch",
    "monitor",
    "monitor.*",
];

/// Whether `tool` hands work to another run ([`DELEGATING`]).
pub fn delegates(tool: &str) -> bool {
    DELEGATING.iter().any(|p| names(p, tool))
}

/// Whether a boss conversation's turn is offered `tool` at all: anything
/// but this hub's own crew and the tools that [delegate](delegates).
pub fn offered(tool: &str) -> bool {
    !tool.starts_with("crew.") && !delegates(tool)
}

/// One of the agent's own tools in a boss conversation: every call asks the
/// person first, once.
pub struct Held {
    inner: Arc<dyn Tool>,
    why: String,
}

impl Held {
    /// `inner` held in the boss conversation of `agent` for `group`.
    pub fn new(inner: Arc<dyn Tool>, agent: &str, group: &str) -> Self {
        let why = format!(
            "{agent} wants to use {} from {group}'s boss conversation. Anything it sends could \
             carry the group's work outside it, so it runs only if you allow it, this once.",
            inner.name()
        );
        debug_assert!(why.contains("boss conversation"));
        Self { inner, why }
    }
}

#[async_trait]
impl Tool for Held {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        self.inner.execute(input).await
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        self.inner.soft_deadline()
    }

    fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
        self.inner.approval_summary(input)
    }

    fn approval_requirement(&self) -> Option<String> {
        Some(self.why.clone())
    }

    /// The same words, kept first on the card whatever else asks.
    fn approval_notice(&self) -> Option<String> {
        Some(self.why.clone())
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        self.inner.surface_effect(input)
    }

    fn effect(&self, input: &serde_json::Value) -> workforce_core::computer::Effect {
        self.inner.effect(input)
    }

    async fn collect(
        &self,
        input: serde_json::Value,
        plan: serde_json::Value,
        checkpoint: Option<String>,
    ) -> eyre::Result<ToolResult> {
        self.inner.collect(input, plan, checkpoint).await
    }

    async fn abandon(&self, plan: serde_json::Value, checkpoint: Option<String>) {
        self.inner.abandon(plan, checkpoint).await
    }
}

/// Whether `pattern` (a name, or `family.*`) names `tool`.
fn names(pattern: &str, tool: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(family) => tool
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('.')),
        None => pattern == tool,
    }
}

/// Whether `tool` is on the design's hold list.
pub fn held(tool: &str) -> bool {
    HELD.iter().any(|p| names(p, tool))
}

/// Whether `tool` is a name a boss conversation's turn may keep.
pub fn kept_name(tool: &str) -> bool {
    KEPT.contains(&tool) && !held(tool)
}

/// Whether a boss conversation's turn keeps `registry`'s tool `tool`: a
/// kept name, held by the hub's own tool.
pub fn kept(registry: &ToolRegistry, tool: &str) -> bool {
    kept_name(tool) && registry.is_builtin(tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_sends_or_reads_elsewhere_is_not_kept() {
        for t in [
            "messaging.send",
            "agent.chat",
            "agent_message",
            "email.send",
            "http.request",
            "github.pr_create",
            "github.api",
            "web.fetch",
            "subagent",
            "sessions",
            "slack.send",
            "exec.run",
            "filesystem.read",
            "vault.get",
            "knowledge.ask",
            "notes",
            "mcp.install",
            "posthog",
            // This hub's own crew: a boss reaches the link's.
            "crew.overview",
            "crew.nudge",
        ] {
            assert!(!kept_name(t), "{t}");
        }
        for t in ["memory", "tool.retrieve", "data.query"] {
            assert!(kept_name(t), "{t}");
        }
    }

    #[test]
    fn a_kept_name_is_kept_only_while_the_hub_holds_it() {
        use crate::turn_loop::tools::tests::named;
        let registry = ToolRegistry::new();
        // A skill or an MCP server taking a kept name isn't kept.
        registry.register(named("memory"));
        assert!(!kept(&registry, "memory"));
        registry.register_builtin(named("memory"));
        assert!(kept(&registry, "memory"));
        registry.register(named("memory"));
        assert!(!kept(&registry, "memory"), "a shadow registered after it");
        registry.register_builtin(named("web.fetch"));
        assert!(
            !kept(&registry, "web.fetch"),
            "built in, but not a kept name"
        );
    }

    #[test]
    fn what_hands_work_to_another_run_is_never_offered() {
        for t in [
            "subagent",
            "investigate",
            "advisor",
            "research_report",
            "workflow",
            "playbook",
            "coding.claude",
            "coding.fanout",
            "coding.session",
            "agent.chat",
            "agent_message",
            "scheduler",
            "reminder",
            "heartbeat.create",
            "watch",
            "monitor",
            "crew.overview",
        ] {
            assert!(!offered(t), "{t}");
        }
        for t in [
            "http.request",
            "web.fetch",
            "memory",
            "messaging.send",
            "codingx",
        ] {
            assert!(offered(t), "{t}");
        }
    }

    #[test]
    fn the_designs_hold_list_names_families() {
        for t in [
            "messaging.send",
            "agent.chat",
            "email.send",
            "http.request",
            "github.api",
        ] {
            assert!(held(t), "{t}");
        }
        for t in [
            "messagingx.send",
            "http",
            "agentic.thing",
            "memory.remember",
        ] {
            assert!(!held(t), "{t}");
        }
    }
}

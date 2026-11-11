//! What a boss conversation's turns may not do (remote bosses, RB6a): send
//! the group's work anywhere but the conversation itself (design §10, 5),
//! or bring in what the agent holds from elsewhere.
//!
//! For now this is a hard deny, by the turn's tools: a boss conversation's
//! turn is offered the boss tools and the few of the agent's own that
//! neither send nor read outside the group ([`KEPT`]), never anything else,
//! and nothing is adopted back mid-turn. The design's hold list ([`HELD`])
//! is refused with the rest. Routing a held call to the person as an
//! approval ("Allan wants to send Acme's work to …") comes with RB6b.
//!
//! It is a list of what is kept, not of what is held, because an agent's
//! tools are open-ended (skills, MCP servers, WASM skills): `web.fetch`,
//! `subagent`, `sessions` or an MCP server's `post` send or read as
//! `http.request` does, and a list of what is held would miss whichever it
//! didn't name.
//!
//! Memory is kept: in a boss conversation everything the agent remembers
//! is tagged with the group's tag and nothing else is recalled
//! ([`crate::memory_fence::fence_for_turn`]), so no memory write leaves the
//! fence.

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
    "memory.recall",
    "memory.search",
    "memory.remember",
    "memory.forget",
    workforce_agent::spill::RETRIEVE_TOOL_NAME,
    workforce_agent::tools::data::DATA_INSPECT_TOOL_NAME,
    workforce_agent::tools::data::DATA_QUERY_TOOL_NAME,
];

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

/// Whether a boss conversation's turn keeps the agent's own tool `tool`.
pub fn kept(tool: &str) -> bool {
    KEPT.contains(&tool) && !held(tool)
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
            assert!(!kept(t), "{t}");
        }
        for t in ["memory.recall", "memory.remember", "tool.retrieve"] {
            assert!(kept(t), "{t}");
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

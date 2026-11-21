//! What a monitor's `agent_task` turn may do. The event that starts it is
//! outside content, so the turn runs as a scheduled job with `tools` does: on
//! a frozen set of read tools, its own delivery when it posts its result
//! itself, and whatever the monitor names. Its guard is armed from the start
//! and refuses rather than asks, since no one is there to ask; what it refused
//! is told to the monitor's owner.

use std::sync::Arc;

use workforce_agent::runner::{with_content_guard, with_tool_whitelist, ToolWhitelist};
use workforce_agent::security::monitor::{AnomalySignal, ContentGuard, ContentScope};
use workforce_orchestrator::monitor::MonitorMeta;

use crate::serve::{AgentService, CURATION_TOOL_PATTERNS, WORKER_EXCLUDED_READS};

/// The tool a turn posts its own result with.
const DELIVERY_TOOL: &str = "messaging.send";

/// The messaging tools that only look.
const MESSAGING_READS: &[&str] = &[
    "messaging.read",
    "messaging.read_thread",
    "messaging.channel_info",
    "messaging.member_info",
    "messaging.whoami",
    "messaging.list_pins",
    "messaging.emoji_list",
];

/// A fetch can carry what the turn read out in its URL.
const OUTBOUND_READS: &str = "web.";

/// How a monitor's turn delivers its result.
pub(crate) enum Delivery<'a> {
    /// It posts it itself, to the destination resolved for `deliver_to`.
    Posts { dest: &'a str, deliver_to: &'a str },
    /// Its answer is told for it.
    Told,
}

/// The tools a monitor's turn is given: the read set, its own delivery when
/// it posts, and `named`.
pub(crate) fn turn_tools(delivery: &Delivery<'_>, named: &[String]) -> Vec<String> {
    let posts = matches!(delivery, Delivery::Posts { .. }).then_some(DELIVERY_TOOL);
    CURATION_TOOL_PATTERNS
        .iter()
        .copied()
        .filter(|tool| !tool.starts_with(OUTBOUND_READS))
        .chain(
            workforce_skills::adapters::read_only_adapter_tools()
                .into_iter()
                .filter(|tool| !WORKER_EXCLUDED_READS.contains(tool)),
        )
        .chain(
            workforce_skills::adapters::email::READ_TOOLS
                .iter()
                .copied(),
        )
        .chain(MESSAGING_READS.iter().copied())
        .chain(posts)
        .map(str::to_string)
        .chain(named.iter().cloned())
        .collect()
}

/// One monitor's `agent_task` turn, scoped to what its job needs.
pub(crate) struct MonitorTurn {
    guard: Arc<ContentGuard>,
    tools: Vec<String>,
}

impl MonitorTurn {
    pub(crate) fn new(monitor: &MonitorMeta, delivery: &Delivery<'_>, named: &[String]) -> Self {
        let expected = match delivery {
            Delivery::Posts { dest, deliver_to } => vec![dest.to_string(), deliver_to.to_string()],
            Delivery::Told => Vec::new(),
        };
        Self {
            guard: Arc::new(ContentGuard::new(ContentScope {
                expected,
                named: named.to_vec(),
                armed: Some(format!("what monitor '{}' was given", monitor.name)),
                refuses: true,
            })),
            tools: turn_tools(delivery, named),
        }
    }

    /// The turn in `session`, as `owner`, told `task`, posting to `dest`.
    pub(crate) async fn run(
        &self,
        service: &Arc<AgentService>,
        session: &str,
        task: &str,
        dest: Option<&str>,
        owner: Option<&str>,
    ) -> eyre::Result<crate::serve::TurnOutcome> {
        with_tool_whitelist(
            Some(ToolWhitelist::for_own_session(&self.tools)),
            with_content_guard(
                Arc::clone(&self.guard),
                service.process_turn(
                    session,
                    "monitor",
                    task,
                    None,
                    None,
                    dest,
                    None,
                    None,
                    None,
                    None,
                    Some(session),
                    None,
                    owner,
                    crate::serve::RunAuthor::Other,
                ),
            ),
        )
        .await
    }

    /// What the owner is told when the turn's guard refused anything.
    pub(crate) fn notice(&self, monitor: &MonitorMeta) -> Option<String> {
        stopped_notice(monitor, &self.guard.stops())
    }
}

fn stopped_notice(monitor: &MonitorMeta, stops: &[AnomalySignal]) -> Option<String> {
    let (first, _) = stops.split_first()?;
    let refused = stops
        .iter()
        .map(|signal| format!("- {}", signal.description()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Your monitor '{name}' (`{id}`) was processing outside content, and its turn tried \
         something that content may have asked for, so it was refused:\n{refused}\n\
         Nothing was done. If that was meant, name the tool in the monitor's `tools` \
         (for example `{tool}`).",
        name = monitor.name,
        id = monitor.id,
        tool = first.tool(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT_OF_SCOPE: &[&str] = &[
        "vault.get",
        "vault.set",
        "exec.run",
        "email.send",
        "email.forward",
        "email.delete",
        "email.move",
        "email.archive",
        "email.rules",
        "monitor",
        "scheduler",
        "workflow",
        "system.enable_skill",
        "web.fetch",
        "web.search",
        "filesystem.write",
        "memory",
        "notes",
        "browser.click",
    ];

    fn meta() -> MonitorMeta {
        serde_json::from_value(serde_json::json!({
            "id": "inbox",
            "name": "Inbox watch",
            "action": { "type": "notify", "target": "#ops", "template": "x" },
        }))
        .unwrap()
    }

    fn posts() -> Delivery<'static> {
        Delivery::Posts {
            dest: "C1",
            deliver_to: "#ops",
        }
    }

    #[test]
    fn the_default_set_reads_and_delivers_and_does_nothing_else() {
        let tools = turn_tools(&posts(), &[]);
        for tool in OUT_OF_SCOPE {
            assert!(!tools.iter().any(|t| t == tool), "{tool} is in scope");
        }
        for tool in [
            "email.read",
            "email.search",
            "messaging.read",
            "messaging.send",
            "filesystem.read",
        ] {
            assert!(tools.iter().any(|t| t == tool), "{tool} is missing");
        }
    }

    #[test]
    fn a_told_turn_cannot_post_and_named_tools_are_added() {
        let told = turn_tools(&Delivery::Told, &["email.move".into()]);
        assert!(!told.iter().any(|t| t == DELIVERY_TOOL));
        assert!(told.iter().any(|t| t == "email.move"));
    }

    #[test]
    fn the_guard_starts_armed_refusing_and_expects_its_delivery() {
        let turn = MonitorTurn::new(&meta(), &posts(), &[]);
        let to = |channel: &str| workforce_agent::tools::SurfaceEffect::Writes {
            platform: "slack".into(),
            destination: channel.into(),
        };
        assert!(turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("C1")))
            .is_none());
        assert!(turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("#ops")))
            .is_none());
        let held = turn
            .guard
            .check("messaging.send", &serde_json::json!({}), Some(&to("C666")))
            .unwrap();
        assert!(held.refused);
        let notice = turn.notice(&meta()).unwrap();
        assert!(notice.contains("Inbox watch"), "{notice}");
        assert!(notice.contains("`messaging.send`"), "{notice}");
        assert!(MonitorTurn::new(&meta(), &posts(), &[])
            .notice(&meta())
            .is_none());
    }
}

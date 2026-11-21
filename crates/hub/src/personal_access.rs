//! Where a person's private connectors (their bank:
//! [`PERSONAL`](workforce_common::connector_binding::PERSONAL)) may be read:
//! in a Desk conversation only, the person's own, and on work nobody is
//! there to watch (a job, a workflow it set up, a heartbeat) only where that
//! work names their tools in its `tools`. Everywhere else (Slack, a monitor,
//! a channel) their tools are hidden, and a call to one is refused.

use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_common::connector_binding::PERSONAL;

const OFF_DESK: &str =
    "The person's bank is read only in a Desk conversation with them. Ask them there.";
const UNNAMED: &str = "Work nobody is watching reads the person's bank only when it names the bank tools in its `tools` and answers in Desk.";

/// Why `tool`, a personal connector's, can't be called now; `None` when it
/// can, or isn't one.
pub fn refusal(tool: &str) -> Option<&'static str> {
    if !workforce_common::connector_binding::is_personal(tool) {
        return None;
    }
    let Some(desk) = workforce_agent::runner::current_desk() else {
        return Some(OFF_DESK);
    };
    let watched = desk.attended && crate::hub_core::origins::automated_origin().is_none();
    let named = workforce_agent::runner::tool_whitelist().is_some_and(|w| {
        w.tools
            .iter()
            .any(|entry| crate::turn_loop::tools::names_tool(entry, tool))
    });
    (!watched && !named).then_some(UNNAMED)
}

/// Hides every personal connector's tools from a run that may not read them.
pub struct PersonalDataAccess;

impl ToolAccess for PersonalDataAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        ToolAccessSnapshot::new(
            PERSONAL
                .iter()
                .flat_map(|c| crate::skill_loader::static_skill_tool_names(c))
                .filter_map(|tool| refusal(tool).map(|why| (tool.to_string(), why.to_string())))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::{with_desk, with_tool_whitelist, ToolWhitelist};
    use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};

    fn desk(attended: bool) -> Option<DeskScope> {
        Some(DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, "desk", None, false),
            owner: "ann".into(),
            attended,
            relayed: false,
        })
    }

    fn blocked() -> Vec<String> {
        let snap = PersonalDataAccess.snapshot();
        let mut names: Vec<String> = snap.blocked_names().map(str::to_string).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn off_desk_every_bank_tool_is_hidden_and_nothing_else() {
        let names = blocked();
        assert_eq!(names.len(), 4);
        assert!(names.iter().all(|n| n.starts_with("bank.")));
        assert_eq!(refusal("bank.transactions"), Some(OFF_DESK));
        assert_eq!(refusal("linear.issue_get"), None);
    }

    #[tokio::test]
    async fn the_person_s_own_desk_conversation_reads_it() {
        assert!(with_desk(desk(true), async { blocked() }).await.is_empty());
    }

    #[tokio::test]
    async fn a_job_in_desk_reads_only_the_bank_tools_it_names() {
        let named = with_desk(
            desk(false),
            with_tool_whitelist(
                Some(ToolWhitelist::for_own_session(&[
                    "bank.accounts".to_string(),
                    "bank.workspaces".to_string(),
                ])),
                async { blocked() },
            ),
        )
        .await;
        assert_eq!(named, ["bank.balances", "bank.transactions"]);

        let whole = with_desk(
            desk(false),
            with_tool_whitelist(
                Some(ToolWhitelist::for_own_session(&["bank".to_string()])),
                async { blocked() },
            ),
        )
        .await;
        assert!(whole.is_empty());

        let unnamed = with_desk(desk(false), async { blocked() }).await;
        assert_eq!(unnamed.len(), 4);
        assert_eq!(
            with_desk(desk(false), async { refusal("bank.accounts") }).await,
            Some(UNNAMED)
        );
    }
}

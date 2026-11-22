//! Where a person's private connectors (their bank:
//! [`PERSONAL`](workforce_common::connector_binding::PERSONAL)) may be read:
//! in the person's own Desk conversation only (their DM with the agent, or a
//! call), never a room, and on work nobody is there to watch (a job, a
//! workflow it set up, a heartbeat, the overseer's relayed turns) only where
//! that work names their tools in its `tools`. Everywhere else (Slack, a
//! monitor, a channel) their tools are hidden, and a call to one is refused.

use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_common::channels::desk::{DeskPlace, DeskTarget};
use workforce_common::connector_binding::{connector_of, is_read, names_personal, PERSONAL};

const OFF_DESK: &str =
    "The person's bank is read only in a Desk conversation with them. Ask them there.";
const IN_ROOM: &str = "The person's bank is read only in their own Desk conversation with you, never in a room others share.";
const UNNAMED: &str = "Work nobody is watching reads the person's bank only when it names the bank tools in its `tools` and answers in Desk.";

/// What a job that reads the person's bank is told when it wouldn't answer
/// in their own Desk conversation.
pub const JOB_OFF_DESK: &str =
    "A job that reads the person's bank answers only in their own Desk conversation with you, never a room or a channel: make it from there.";

/// What work that reads the person's bank is told when it holds more than
/// reads beside it.
pub const ONLY_READS: &str =
    "Work that reads the person's bank names nothing beside it but a built-in connector's reads: nothing that writes or sends, no web, http or MCP tool, and no whole namespace.";

/// Whether `place` is the person's alone: their DM with the agent (or a
/// thread on it), or a call.
pub fn is_private(place: &DeskPlace) -> bool {
    matches!(place, DeskPlace::Dm { .. } | DeskPlace::Call)
}

/// Why `tool`, a personal connector's, can't be called now; `None` when it
/// can, or isn't one.
pub fn refusal(tool: &str) -> Option<&'static str> {
    if !workforce_common::connector_binding::is_personal(tool) {
        return None;
    }
    why_not(Some(tool))
}

/// Whether the run in hand may read the person's bank at all: what a reply
/// carrying it (a conversation that read it, BankSync's own pages) is
/// shown by.
pub fn may_read() -> bool {
    read_refusal().is_none()
}

/// Why the run in hand may not read the person's bank at all, in words.
pub fn read_refusal() -> Option<&'static str> {
    why_not(None)
}

/// Why the run in hand may not read the person's bank (`tool` of it, or any
/// of it). Only in their own DM or call; and work nobody watches (a job, a
/// heartbeat, a workflow, a monitor, the overseer's relayed turn) only
/// where its `tools` name the bank and hold nothing else but reads, so
/// nothing could carry it on.
fn why_not(tool: Option<&str>) -> Option<&'static str> {
    let Some(desk) = workforce_agent::runner::current_desk() else {
        return Some(OFF_DESK);
    };
    if !is_private(&desk.target.place) {
        return Some(IN_ROOM);
    }
    let watched =
        desk.attended && !desk.relayed && crate::hub_core::origins::automated_origin().is_none();
    if watched {
        return None;
    }
    let Some(whitelist) = workforce_agent::runner::tool_whitelist() else {
        return Some(UNNAMED);
    };
    let named = whitelist.tools.iter().any(|entry| match tool {
        Some(tool) => crate::turn_loop::tools::names_tool(entry, tool),
        None => names_personal(entry),
    });
    match named {
        false => Some(UNNAMED),
        true => only_reads_beside(&whitelist.tools).then_some(ONLY_READS),
    }
}

/// Whether `tools` holds something beside the bank that isn't a known
/// read: only a built-in connector's own tool whose read the hub knows (as
/// its computer gate keeps it) may sit beside it. A name alone says
/// nothing: `web.*`, `http.*` and an MCP server's tools reach out whatever
/// they are called.
fn only_reads_beside(tools: &[String]) -> bool {
    tools.iter().any(|t| !names_personal(t) && !known_read(t))
}

/// A built-in connector's own tool that only reads.
fn known_read(tool: &str) -> bool {
    connector_of(tool).is_some_and(|connector| {
        crate::skill_loader::static_skill_tool_names(connector)
            .iter()
            .any(|t| *t == tool)
    }) && is_read(tool)
}

/// Why a job given `tools`, answering at `desk`, may not read the person's
/// bank; `None` when it may, or doesn't name it. It answers in their own
/// Desk conversation (never a room, nor a channel it names), and names
/// nothing beside the bank but reads. Checked when a job is made or changed,
/// by any path; [`refusal`] holds it again as it runs.
pub fn job_refusal(tools: &[String], desk: Option<&DeskTarget>) -> Option<&'static str> {
    if !tools.iter().any(|t| names_personal(t)) {
        return None;
    }
    if !desk.is_some_and(|t| is_private(&t.place)) {
        return Some(JOB_OFF_DESK);
    }
    only_reads_beside(tools).then_some(ONLY_READS)
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
    use workforce_common::channels::desk::{DeskScope, DeskTarget};

    fn desk_at(place: DeskPlace, attended: bool, relayed: bool) -> Option<DeskScope> {
        Some(DeskScope {
            target: DeskTarget::of_turn(&place, "desk", None, false),
            owner: "ann".into(),
            attended,
            relayed,
        })
    }

    fn desk(attended: bool) -> Option<DeskScope> {
        desk_at(DeskPlace::Dm { thread: None }, attended, false)
    }

    fn naming_bank<F: std::future::Future>(f: F) -> impl std::future::Future<Output = F::Output> {
        with_tool_whitelist(
            Some(ToolWhitelist::for_own_session(&["bank".to_string()])),
            f,
        )
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

    #[tokio::test]
    async fn the_overseer_s_relayed_turn_never_reads_it_unless_it_names_the_bank() {
        let relayed = desk_at(DeskPlace::Dm { thread: None }, true, true);
        assert_eq!(
            with_desk(relayed.clone(), async { refusal("bank.balances") }).await,
            Some(UNNAMED)
        );
        assert_eq!(
            with_desk(relayed, naming_bank(async { refusal("bank.balances") })).await,
            None
        );
    }

    #[tokio::test]
    async fn work_descending_from_automation_needs_the_bank_named() {
        let automated = crate::hub_core::origins::descending_from(
            Some("heartbeat".into()),
            with_desk(desk(true), async { refusal("bank.accounts") }),
        )
        .await;
        assert_eq!(automated, Some(UNNAMED));
    }

    #[tokio::test]
    async fn a_room_never_reads_it_even_named_and_a_dm_thread_or_call_does() {
        let room = desk_at(
            DeskPlace::Room {
                room: uuid::Uuid::nil(),
                name: "family".into(),
                thread: None,
            },
            true,
            false,
        );
        assert_eq!(
            with_desk(room, naming_bank(async { refusal("bank.accounts") })).await,
            Some(IN_ROOM)
        );
        let legacy = desk_at(DeskPlace::LegacyRoom { slug: "old".into() }, true, false);
        assert_eq!(
            with_desk(legacy, async { refusal("bank.accounts") }).await,
            Some(IN_ROOM)
        );
        for place in [
            DeskPlace::Dm {
                thread: Some("m1".into()),
            },
            DeskPlace::Call,
        ] {
            assert_eq!(
                with_desk(desk_at(place, true, false), async {
                    refusal("bank.accounts")
                })
                .await,
                None
            );
        }
    }

    /// A heartbeat (or any unwatched run) that holds a tool that sends
    /// beside the bank never reads it: what it read could leave with it.
    #[tokio::test]
    async fn unwatched_work_holding_a_send_beside_the_bank_never_reads_it() {
        let holding = |tools: &[&str]| {
            Some(ToolWhitelist::for_turn(
                &tools.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            ))
        };
        let heartbeat = |tools: Option<ToolWhitelist>| {
            crate::hub_core::origins::descending_from(
                Some("heartbeat".into()),
                with_desk(
                    desk(false),
                    with_tool_whitelist(tools, async {
                        (refusal("bank.transactions"), may_read())
                    }),
                ),
            )
        };
        assert_eq!(
            heartbeat(holding(&["bank", "slack.post_message"])).await,
            (Some(ONLY_READS), false)
        );
        assert_eq!(
            heartbeat(holding(&["bank", "messaging.send"])).await,
            (Some(ONLY_READS), false)
        );
        assert_eq!(
            heartbeat(holding(&["bank", "linear.issue_get"])).await,
            (None, true)
        );
        for beside in ["web.search", "acme_mcp.get_x"] {
            assert_eq!(
                heartbeat(holding(&["bank", beside])).await,
                (Some(ONLY_READS), false),
                "{beside}"
            );
        }
        assert_eq!(
            heartbeat(holding(&["linear.issue_get"])).await,
            (Some(UNNAMED), false)
        );
        assert!(with_desk(desk(true), async { may_read() }).await);
        assert!(!may_read(), "never off Desk");
    }

    #[test]
    fn a_job_reads_the_bank_only_answering_in_the_person_s_dm_with_only_reads_beside_it() {
        let at = |place: DeskPlace| DeskTarget::of_turn(&place, "desk", None, false);
        let tools = |t: &[&str]| t.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        let dm = at(DeskPlace::Dm { thread: None });
        assert_eq!(job_refusal(&tools(&["linear"]), None), None);
        assert_eq!(job_refusal(&tools(&["bank"]), Some(&dm)), None);
        assert_eq!(job_refusal(&tools(&["bank"]), None), Some(JOB_OFF_DESK));
        assert_eq!(
            job_refusal(&tools(&["bank", "messaging.send"]), Some(&dm)),
            Some(ONLY_READS)
        );
        assert_eq!(
            job_refusal(&tools(&["bank", "linear.issue_get"]), Some(&dm)),
            None
        );
        // Beside the bank, only a built-in connector's known reads: a name
        // that reads like one says nothing of a web, http or MCP tool.
        for beside in [
            "web.search",
            "web.fetch",
            "http.get",
            "acme_mcp.get_x",
            "linear.get_x",
            "linear",
        ] {
            assert_eq!(
                job_refusal(&tools(&["bank", beside]), Some(&dm)),
                Some(ONLY_READS),
                "{beside}"
            );
        }
    }
}

//! The daily check a bank's setup makes for the person: one agent reads each
//! bank's approval at 09:00 and, on the days it is due (a week before it
//! lapses, the day before and the day), reminds them in their own Desk
//! conversation; on every other day it says nothing. One agent watches each
//! connection: the one whose own it is, or for "all my agents" the first by
//! name it reaches. The job goes once no bank it watches is left.

use workforce_common::channels::desk::{DeskPlace, DeskTarget, DESK_CHANNEL};
use workforce_common::delivery::DeliveryTarget;

use crate::hub_core::schedules::{HubJob, JobPayload, Schedule, NOTHING_TO_SAY};

/// The job's name, as the person's job list shows it.
pub const JOB: &str = "Bank approvals";

/// The only tools its turn is offered: reads of the bank alone.
pub const TOOLS: [&str; 2] = ["bank.workspaces", "bank.accounts"];

/// The job that watches the person's bank approvals.
pub fn job() -> HubJob {
    HubJob {
        description: "Reminds you in Desk before a bank's approval lapses.".into(),
        schedule: Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
        },
        payload: JobPayload::AgentTurn {
            message: format!(
                "Check the person's bank approvals: list their bank connections with \
                 bank.workspaces, then read each with bank.accounts. For each whose \
                 `approval.remind` is true, tell them in one sentence that its approval at \
                 the bank lapses about {{approval.until}} and that they renew it in \
                 BankSync, then press Reconnect on it in Desk's Connections. Never say an \
                 amount, a balance or an account number. If none needs reminding, answer \
                 exactly {NOTHING_TO_SAY} and nothing else."
            ),
            channel: None,
            channel_id: None,
            thread_id: None,
            model: None,
            user_id: None,
            max_rounds: Some(8),
            agent_owns_delivery: false,
            tools: Some(TOOLS.map(str::to_string).to_vec()),
        },
        deliver_to: DeliveryTarget::desk(DeskTarget::of_turn(
            &DeskPlace::Dm { thread: None },
            DESK_CHANNEL,
            None,
            false,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::{with_desk, with_tool_whitelist, ToolWhitelist};
    use workforce_common::channels::desk::DeskScope;

    /// The job reads the bank as its run is scoped: unattended, in the
    /// person's DM, from the scheduler, naming only bank reads.
    #[tokio::test]
    async fn the_watch_reads_the_bank_as_its_run_is_scoped_and_nothing_else() {
        let job = job();
        let JobPayload::AgentTurn { tools, .. } = &job.payload else {
            panic!("an agent turn");
        };
        let tools = tools.clone().unwrap();
        let target = job.deliver_to.desk_target().unwrap().clone();
        assert!(crate::personal_access::is_private(&target.place));
        let scope = DeskScope {
            target,
            owner: "ann".into(),
            attended: false,
            relayed: false,
        };
        let refusals = crate::hub_core::origins::descending_from(
            Some("scheduler".into()),
            with_desk(
                Some(scope),
                with_tool_whitelist(Some(ToolWhitelist::for_own_session(&tools)), async {
                    ["bank.accounts", "bank.workspaces", "bank.transactions"]
                        .map(crate::personal_access::refusal)
                }),
            ),
        )
        .await;
        assert_eq!(refusals[..2], [None, None]);
        assert!(refusals[2].is_some(), "it reads nothing it doesn't name");
        assert!(tools
            .iter()
            .all(|t| workforce_common::connector_binding::is_read(t)));
    }
}

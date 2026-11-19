//! A boss's urgent orders (design note §4), as the crew gives them: how a
//! `crew.nudge` with `urgent` reaches Desk's line, and the boss's interrupt
//! of a turn that didn't read its stand-down in time.
//!
//! Desk steers the words in ([`workforce_desk::Desk::line_send_with`]);
//! here the urgency is carried from the tool to the send beside the action
//! (not in it, so the action's shape stays one), and the interrupt goes
//! through the gate `crew.interrupt` does.

use std::future::Future;

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::line::{Rushed, Urgency};

use super::actions::{Action, Gate};
use super::CrewHub;

tokio::task_local! {
    /// How urgently the order being performed is sent, for the line send
    /// in [`CrewHub::perform`]'s nudge.
    static URGENCY: Option<Urgency>;
}

/// The words the model reads for `crew.nudge`'s `urgent`.
pub(crate) const URGENT_DESCRIPTION: &str = "true only when this must reach a session that is working now, mid-turn: a correction or a stop that can't wait for its turn to end. It is read with the session's next tool call's result (Codex: said into its turn). A session that isn't working gets it at once anyway; one that can't take words mid-turn gets it when its turn ends. At most 3 urgent orders per session in 10 minutes.";

/// What a tool's `input` asks of its order's urgency: `urgent: true` is an
/// urgent order.
pub(crate) fn of(input: &Value) -> Option<Urgency> {
    input["urgent"]
        .as_bool()
        .unwrap_or(false)
        .then_some(Urgency::Order)
}

/// Runs `f` (a perform) with its order sent `urgency`.
pub(crate) async fn sent<F: Future>(urgency: Option<Urgency>, f: F) -> F::Output {
    URGENCY.scope(urgency, f).await
}

/// How urgently the order being performed is sent: none outside [`sent`].
pub(crate) fn current() -> Option<Urgency> {
    URGENCY.try_with(|u| *u).ok().flatten()
}

/// What the boss is told of an urgent nudge to `name`, when its words aren't
/// the usual ones for its delivery.
pub(crate) fn told(name: &str, rushed: Option<Rushed>) -> Option<String> {
    rushed
        .and_then(Rushed::said)
        .map(|said| format!("told {name}; {said}"))
}

impl CrewHub {
    /// `boss`'s escalating stand-down (line row `row`) wasn't read by
    /// `session` in time: interrupts its turn as `crew.interrupt` does,
    /// through the same gate. At a gate that leaves it to the person, it is
    /// suggested to them.
    pub(crate) async fn steer_overdue_now(
        &self,
        boss: &str,
        session: Uuid,
        row: Uuid,
    ) -> Result<String, String> {
        let Some((service, oversight)) = self.overseer_named(boss) else {
            return Err(format!("{boss} isn't a boss here"));
        };
        let action = Action::Interrupt { session };
        let mark = workforce_desk::line::marker(row);
        let reason = format!(
            "It hasn't read its stand-down ({mark}) in {}s, and a job that can't be undone may start meanwhile.",
            self.desk.steer_wait().as_secs()
        );
        let by = self.by_boss(&service, &reason);
        match self.gate(&action, &oversight) {
            Gate::Run => {
                let by = self.by_at_level(by, &oversight);
                Box::pin(self.perform(&action, by)).await
            }
            Gate::Suggest(why) => {
                let input = json!({ "session_id": session.to_string(), "reason": reason });
                self.suggest("crew.interrupt", &input, &action, &by).await?;
                Err(format!(
                    "interrupting it was suggested to the person, as {why}"
                ))
            }
            Gate::No(why) => Err(why.into_owned()),
            Gate::Refuse => Err("the boss only watches".into()),
        }
    }
}

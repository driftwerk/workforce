//! A boss's urgent orders (design note §4), as the crew gives them: how an
//! order's `urgent` (its [`super::assignments::Order`]) reaches Desk's
//! line, and the boss's interrupt of a turn that didn't read its
//! stand-down in time.
//!
//! Desk steers the words in ([`workforce_desk::Desk::line_send_with`]);
//! the interrupt goes through the gate `crew.interrupt` does, bound to the
//! turn the stand-down was sent in ([`workforce_desk::Desk::interrupt_turn`]).

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;
use workforce_desk::crew;
use workforce_desk::line::{Rushed, TurnMark, Urgency, URGENT_PER_SESSION, URGENT_WINDOW};

use super::actions::{Action, Gate};
use super::assignments::Order;
use super::CrewHub;

/// The words the model reads for the send tools' `urgent`, with the
/// limit's numbers from the desk's own.
pub(crate) fn description() -> String {
    format!(
        "true only when this must reach a session that is working now, mid-turn: a correction that can't wait for its turn to end. \
         It is read with the session's next tool call's result (Codex: said into its turn), so a session inside a long tool (a build, gh run watch, a subagent) reads it only when that tool returns: for what can't wait that long, crew.interrupt. \
         A session that isn't working gets it at once anyway; one that can't take words mid-turn gets it when its turn ends. \
         At most {URGENT_PER_SESSION} urgent orders per session in {} minutes; past that they queue. Ignored with reply_to: an answer goes to the question that waits on it.",
        URGENT_WINDOW.as_secs() / 60
    )
}

/// How urgently `order` is sent by `sender`: a boss's urgent order. Only a
/// boss's words are steered in; the person's Send now is their own.
pub(crate) fn of(order: &Order, sender: &workforce_desk::LineSender) -> Option<Urgency> {
    (order.urgent && matches!(sender, workforce_desk::LineSender::Overseer(_)))
        .then_some(Urgency::Order)
}

/// What the boss is told of an urgent nudge to `name`, when its words aren't
/// the usual ones for its delivery.
pub(crate) fn told(name: &str, rushed: Option<Rushed>) -> Option<String> {
    rushed
        .and_then(Rushed::said)
        .map(|said| format!("told {name}; {said}"))
}

/// How often a suggestion to interrupt is looked at again, to withdraw it
/// once its stand-down is read or its turn is over.
const WITHDRAW_LOOK: Duration = Duration::from_millis(500);

impl CrewHub {
    /// `boss`'s escalating stand-down (line row `row`) wasn't read by
    /// `session` in turn `turn`: interrupts that turn as `crew.interrupt`
    /// does, through the same gate, and only while the row is still unread.
    /// At a gate that leaves it to the person, it is suggested to them, and
    /// the suggestion is withdrawn once the row is read or the turn ends.
    pub(crate) async fn steer_overdue_now(
        &self,
        boss: &str,
        session: Uuid,
        row: Uuid,
        turn: TurnMark,
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
                let order = Order {
                    only_turn: Some((row, turn)),
                    ..Default::default()
                };
                Box::pin(self.perform_with(&action, by, &order)).await
            }
            Gate::Suggest(why) => {
                let input = json!({ "session_id": session.to_string(), "reason": reason });
                let s = self.suggest("crew.interrupt", &input, &action, &by).await?;
                self.withdraw_when_read(s, session, row, turn);
                Err(format!(
                    "interrupting it was suggested to the person, as {why}"
                ))
            }
            Gate::No(why) => Err(why.into_owned()),
            Gate::Refuse => Err("the boss only watches".into()),
        }
    }

    /// The suggestion `s` to interrupt `session` for its stand-down `row`
    /// holds only while that row is unread in turn `turn`: withdrawn after,
    /// so the person's yes never interrupts a later turn.
    fn withdraw_when_read(&self, s: crew::Suggestion, session: Uuid, row: Uuid, turn: TurnMark) {
        let desk = self.desk.clone();
        tokio::spawn(async move {
            while desk.steer_unread(session, turn, row).await {
                tokio::time::sleep(WITHDRAW_LOOK).await;
            }
            let store = desk.store();
            let Ok(Some(mut now)) = store.suggestion(&s.id).await else {
                return;
            };
            if now.state != crew::SuggestionState::Open {
                return;
            }
            now.state = crew::SuggestionState::Dismissed;
            if let Err(e) = store.put_suggestion(&now).await {
                tracing::warn!(suggestion = %s.id, error = %e, "a suggestion to interrupt couldn't be withdrawn");
                return;
            }
            let mut by = crew::By::desk("Its stand-down was read, or its turn ended.");
            by.suggestion_id = Some(now.id.clone());
            desk.note(by.event(
                crew::Kind::Dismissed,
                now.subject.clone(),
                format!("withdrew the suggestion: {}", now.text),
            ));
        });
    }
}

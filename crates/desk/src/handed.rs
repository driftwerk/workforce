//! A person's request that their agent on another hub handed to their Desk
//! boss (DW-770, designs/dw-770-remote-goal-to-desk.md in workforce): kept
//! once, delivered to one boss as the person's own words, and what Desk
//! owes back said where they asked.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::goal::{self, Consent};
use crate::plan::{Plan, PlanState};

/// The longest request Desk takes, in characters.
pub const WORDS_MAX: usize = 1_000;

/// One handed request, as this Desk keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handed {
    /// The agent's hub's id for it.
    pub id: Uuid,
    /// The agent, as Desk links it: `<hub>/<agent>`.
    pub agent: String,
    pub agent_name: String,
    /// Where the person asked, as people say it ("Slack").
    pub place: String,
    /// The person's own words, whole.
    pub words: String,
    /// The session it is about, as Desk names it: `<id>` on this computer,
    /// `<machine>/<id>` on a machine.
    pub session: Option<String>,
    pub asked_at: DateTime<Utc>,
    /// The boss it went to; none when it was refused.
    pub boss: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// What keeping a request taken found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// Kept now, for the first time.
    New,
    /// Kept before, but never delivered nor answered: a Desk that stopped
    /// before it did.
    Undelivered,
    /// Delivered to `boss` before, with nothing said back on it yet.
    Unanswered { boss: String },
    /// Delivered or refused, and said back on.
    Done,
}

/// A word owed back to where a request was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    pub id: i64,
    pub handed: Uuid,
    pub agent: String,
    pub text: String,
}

/// Whether `h`'s words agree `goal`, and only it: the goal said in them,
/// and nothing in them refusing or changing it.
pub fn agrees(h: &Handed, goal: &str) -> bool {
    goal::says_goal(&h.words, goal) && goal::agreement(&h.words, goal) == Consent::Yes
}

/// Whether `plan`, whose tickets' sessions are `sessions`, carries `h`: its
/// boss's, still under way, and on the session `h` names or agreed in `h`'s
/// words once it was delivered.
pub fn covers(h: &Handed, plan: &Plan, sessions: &[String]) -> bool {
    let (Some(boss), Some(delivered)) = (h.boss.as_deref(), h.delivered_at) else {
        return false;
    };
    let on_session = h.session.as_ref().is_some_and(|s| sessions.contains(s));
    let agreed_in_it = plan
        .agreed
        .as_deref()
        .zip(plan.agreed_at)
        .is_some_and(|(words, at)| at >= delivered && goal::says_goal(&h.words, words));
    boss == plan.boss
        && matches!(plan.state, PlanState::Running | PlanState::Paused)
        && (on_session || agreed_in_it)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::plan;

    fn handed(session: Option<String>) -> Handed {
        Handed {
            id: Uuid::new_v4(),
            agent: "driftwerk/allan".into(),
            agent_name: "allan".into(),
            place: "Slack".into(),
            words: "ok keep that video creation on Track in desk please".into(),
            session,
            asked_at: Utc::now(),
            boss: Some("sarah".into()),
            delivered_at: Some(Utc::now()),
        }
    }

    #[test]
    fn a_plan_carries_a_request_on_its_session_or_agreed_in_its_words_after_it() {
        let session = Uuid::new_v4().to_string();
        let mut p = plan(PlanState::Running);
        let h = handed(Some(session.clone()));
        assert!(covers(&h, &p, std::slice::from_ref(&session)));
        assert!(!covers(&h, &p, &["another".into()]));
        let on_machine = handed(Some(format!("devbox/{session}")));
        assert!(covers(&on_machine, &p, &[format!("devbox/{session}")]));
        assert!(!covers(&on_machine, &p, std::slice::from_ref(&session)));

        let h = handed(None);
        p.agreed = Some("keep that video creation on Track".into());
        p.agreed_at = h.delivered_at.map(|d| d + chrono::Duration::seconds(1));
        assert!(covers(&h, &p, &[]));
        p.agreed_at = h.delivered_at.map(|d| d - chrono::Duration::hours(1));
        assert!(!covers(&h, &p, &[]), "agreed before it came");
        p.agreed_at = h.delivered_at;
        p.agreed = Some("ship the docs".into());
        assert!(!covers(&h, &p, &[]), "other words");

        let h = handed(Some(session.clone()));
        let on = [session];
        assert!(!covers(&h, &plan(PlanState::Done), &on), "finished");
        assert!(!covers(
            &Handed {
                boss: Some("other".into()),
                ..h.clone()
            },
            &p,
            &on
        ));
        assert!(!covers(&Handed { boss: None, ..h }, &p, &on), "refused");
    }

    #[test]
    fn a_request_agrees_only_the_goal_it_says() {
        let h = handed(None);
        assert!(agrees(&h, "keep that video creation on Track"));
        assert!(!agrees(&h, "ship the docs"), "a goal it doesn't say");
        let but = Handed {
            words: "keep the video creation on track but not the docs".into(),
            ..h
        };
        assert!(!agrees(&but, "keep the video creation on track"));
    }
}

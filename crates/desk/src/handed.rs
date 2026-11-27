//! A person's request that their agent on another hub handed to their Desk
//! boss (DW-770, designs/dw-770-remote-goal-to-desk.md in workforce): kept
//! once, delivered to one boss as the person's own words, and what Desk
//! owes back said where they asked.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::goal;
use crate::plan::{Plan, PlanState};

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
    pub session: Option<Uuid>,
    pub asked_at: DateTime<Utc>,
    /// The boss it went to; none when it was refused.
    pub boss: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// A word owed back to where a request was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    pub id: i64,
    pub handed: Uuid,
    pub agent: String,
    pub text: String,
}

/// Whether `plan`, whose tickets' sessions are `sessions`, carries `h`: its
/// boss's, still under way, and on the session `h` names or agreed in `h`'s
/// words once it was delivered.
pub fn covers(h: &Handed, plan: &Plan, sessions: &[String]) -> bool {
    let (Some(boss), Some(delivered)) = (h.boss.as_deref(), h.delivered_at) else {
        return false;
    };
    let on_session = h
        .session
        .is_some_and(|s| sessions.iter().any(|id| *id == s.to_string()));
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

    fn handed(session: Option<Uuid>) -> Handed {
        Handed {
            id: Uuid::new_v4(),
            agent: "driftwerk/allan".into(),
            agent_name: "Allan".into(),
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
        let session = Uuid::new_v4();
        let mut p = plan(PlanState::Running);
        let h = handed(Some(session));
        assert!(covers(&h, &p, &[session.to_string()]));
        assert!(!covers(&h, &p, &["another".into()]));

        let h = handed(None);
        p.agreed = Some("keep that video creation on Track".into());
        p.agreed_at = h.delivered_at.map(|d| d + chrono::Duration::seconds(1));
        assert!(covers(&h, &p, &[]));
        p.agreed_at = h.delivered_at.map(|d| d - chrono::Duration::hours(1));
        assert!(!covers(&h, &p, &[]), "agreed before it came");
        p.agreed_at = h.delivered_at;
        p.agreed = Some("ship the docs".into());
        assert!(!covers(&h, &p, &[]), "other words");

        let h = handed(Some(session));
        let on = [session.to_string()];
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
}

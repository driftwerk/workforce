//! What a boss leaves to the person about one of its sessions, said in its
//! DM. Its words are made where the DM is: here for this hub's own boss,
//! and on the Mac for the home's own boss of a machine's sessions, which
//! the machine tells only these fields ([`super::relayed_decide`]).

use serde::{Deserialize, Serialize};

use super::decide::RETRIES;

/// One thing a boss leaves to the person, of the session `session` (as the
/// person knows it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Left {
    /// It asks the person `ask`, which only they can answer.
    Asks { session: String, ask: String },
    /// Its question `ask` wasn't answered, as `why`: in its terminal when
    /// only that answers it.
    Unanswered {
        session: String,
        ask: String,
        why: String,
        terminal: bool,
    },
    /// The boss answered `ask`, and Desk refused it as `why`.
    AnswerRefused {
        session: String,
        ask: String,
        why: String,
    },
    /// Its plan `plan` waits on the person, who is asked `ask`.
    PlanAsks {
        session: String,
        plan: String,
        ask: String,
    },
    /// Its plan `plan` wasn't reviewed, as `why`.
    PlanUnreviewed {
        session: String,
        plan: String,
        why: String,
    },
    /// The boss reviewed `plan`, and Desk refused it as `why`.
    PlanRefused {
        session: String,
        plan: String,
        why: String,
    },
    /// Its request `ask` waits on the person, as `why`.
    Waits {
        session: String,
        ask: String,
        why: String,
    },
    /// Its request `ask` is still the boss's, slow as `why`; the person's
    /// once its decision `given_up` failing.
    Slow {
        session: String,
        ask: String,
        why: String,
        given_up: bool,
    },
    /// Its request is the person's again: Act fully isn't in force for it.
    YoursAgain { session: String },
    /// `more` of the oldest of these made room for newer ones before they
    /// were said.
    Crowded { more: usize },
}

impl Left {
    /// What the person reads.
    pub(crate) fn words(&self) -> String {
        match self {
            Self::Asks { session, ask } => format!("{session} asks: {ask}"),
            Self::Unanswered {
                session,
                ask,
                why,
                terminal,
            } => {
                let place = if *terminal { ", in its terminal" } else { "" };
                format!("{session} asks “{ask}”. I couldn't answer it, as {why}: it's yours{place}.")
            }
            Self::AnswerRefused { session, ask, why } => {
                format!("{session} asks “{ask}”. I answered, but Desk refused it: {why}")
            }
            Self::PlanAsks { session, plan, ask } => {
                format!("{session}'s plan “{plan}” waits for you: {ask}")
            }
            Self::PlanUnreviewed { session, plan, why } => format!(
                "{session}'s plan “{plan}” waits for you in its terminal: I couldn't review it, as {why}."
            ),
            Self::PlanRefused { session, plan, why } => format!(
                "{session}'s plan “{plan}” waits for you: I reviewed it, but Desk refused it: {why}"
            ),
            Self::Waits { session, ask, why } => {
                format!("{session} is waiting on you to allow {ask}: {why}")
            }
            Self::Slow {
                session,
                ask,
                why,
                given_up,
            } => {
                let given = match given_up {
                    true => format!(
                        " (it failed {RETRIES} times over, so it's yours to answer now)"
                    ),
                    false => String::new(),
                };
                format!("{session} asks for {ask}: {why}{given}")
            }
            Self::YoursAgain { session } => format!(
                "{session}'s request is yours again: Act fully isn't in force for it now, so I didn't do what I decided"
            ),
            Self::Crowded { more } => format!(
                "{more} more things I left to you came faster than I could tell you here: see those sessions in Desk there"
            ),
        }
    }

    /// It with each of its words made by `f`.
    pub(crate) fn with_text(self, f: impl Fn(String) -> String) -> Self {
        match self {
            Self::Asks { session, ask } => Self::Asks {
                session: f(session),
                ask: f(ask),
            },
            Self::Unanswered {
                session,
                ask,
                why,
                terminal,
            } => Self::Unanswered {
                session: f(session),
                ask: f(ask),
                why: f(why),
                terminal,
            },
            Self::AnswerRefused { session, ask, why } => Self::AnswerRefused {
                session: f(session),
                ask: f(ask),
                why: f(why),
            },
            Self::PlanAsks { session, plan, ask } => Self::PlanAsks {
                session: f(session),
                plan: f(plan),
                ask: f(ask),
            },
            Self::PlanUnreviewed { session, plan, why } => Self::PlanUnreviewed {
                session: f(session),
                plan: f(plan),
                why: f(why),
            },
            Self::PlanRefused { session, plan, why } => Self::PlanRefused {
                session: f(session),
                plan: f(plan),
                why: f(why),
            },
            Self::Waits { session, ask, why } => Self::Waits {
                session: f(session),
                ask: f(ask),
                why: f(why),
            },
            Self::Slow {
                session,
                ask,
                why,
                given_up,
            } => Self::Slow {
                session: f(session),
                ask: f(ask),
                why: f(why),
                given_up,
            },
            Self::YoursAgain { session } => Self::YoursAgain {
                session: f(session),
            },
            Self::Crowded { more } => Self::Crowded { more },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Left;

    #[test]
    fn what_is_left_reads_the_same_wherever_it_is_said() {
        let left = Left::Unanswered {
            session: "VEL-9".into(),
            ask: "Ship it?".into(),
            why: "its model failed: down".into(),
            terminal: true,
        };
        assert_eq!(
            left.words(),
            "VEL-9 asks “Ship it?”. I couldn't answer it, as its model failed: down: it's yours, in its terminal."
        );
        let wire = serde_json::to_value(&left).unwrap();
        assert_eq!(wire["kind"], "unanswered");
        assert_eq!(serde_json::from_value::<Left>(wire).unwrap(), left);
        assert_eq!(
            left.with_text(|t| t.to_uppercase()).words(),
            "VEL-9 asks “SHIP IT?”. I couldn't answer it, as ITS MODEL FAILED: DOWN: it's yours, in its terminal."
        );
        assert!(
            serde_json::from_value::<Left>(serde_json::json!({"kind": "said", "text": "hi"}))
                .is_err()
        );
    }
}

//! Classifying what kind of work a request is, so a router can pick the
//! path that suits it before any model turn is spent.
//!
//! One `Choice` over a closed set of use cases, read from the same state as
//! the scope gate ([`crate::request::state`]). `ambiguous` is the escape
//! option, and anything below the floor is `ambiguous` too: a router must
//! be able to tell "unsure" from a class.

use crate::question::{Answer, Question};
use serde_json::Value;
use std::collections::BTreeMap;

/// Id of the question in the request and the answer.
pub const QUESTION_ID: &str = "use_case";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseCase {
    Database,
    Code,
    Investigation,
    Action,
    Ambiguous,
}

impl UseCase {
    pub const ALL: [Self; 5] = [
        Self::Database,
        Self::Code,
        Self::Investigation,
        Self::Action,
        Self::Ambiguous,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Code => "code",
            Self::Investigation => "investigation",
            Self::Action => "action",
            Self::Ambiguous => "ambiguous",
        }
    }

    fn meaning(self) -> &'static str {
        match self {
            Self::Database => {
                "Answered by querying data: counts, totals, lists, records, a report or a metric"
            }
            Self::Code => {
                "About source code: how something is implemented, where it lives, a review, or a change to make"
            }
            Self::Investigation => {
                "Finding out why something happened: an incident, a failure, an error or unexpected behaviour to diagnose"
            }
            Self::Action => {
                "Asks for something to be done in a system: create, update, send, schedule, deploy or close something"
            }
            Self::Ambiguous => {
                "Fits none of the others, fits several equally, or is too vague to tell"
            }
        }
    }

    fn of(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.label() == label)
    }
}

/// The question set: which use case the request is.
pub fn questions() -> BTreeMap<String, Question> {
    let question = Question::choice(
        "What kind of work does `message` ask for? Judge by what is being asked, not by what \
         is mentioned along the way.",
        UseCase::ALL.map(|u| (u.label(), Value::String(u.meaning().into()))),
    );
    BTreeMap::from([(QUESTION_ID.to_string(), question)])
}

/// Where a request should go, and how sure the model was of its pick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Route {
    pub use_case: UseCase,
    pub probability: f64,
}

/// The use case to route on: the model's pick when it reaches `floor`,
/// otherwise — or for an answer of the wrong shape or an unknown label —
/// `ambiguous`.
pub fn route(answer: &Answer, floor: f64) -> Route {
    let probability = answer.top_probability();
    let picked = match answer {
        Answer::Choice { choice, .. } if probability >= floor => UseCase::of(choice),
        _ => None,
    };
    Route {
        use_case: picked.unwrap_or(UseCase::Ambiguous),
        probability,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(label: &str, p: f64) -> Answer {
        Answer::Choice {
            choice: label.into(),
            probabilities: BTreeMap::from([(label.to_string(), p)]),
            confidence: p,
        }
    }

    #[test]
    fn the_question_offers_every_use_case_with_its_meaning() {
        let Question::Choice { criteria, .. } = &questions()[QUESTION_ID] else {
            panic!("choice expected");
        };
        assert_eq!(
            criteria.keys().map(String::as_str).collect::<Vec<_>>(),
            ["action", "ambiguous", "code", "database", "investigation"]
        );
        assert!(criteria
            .values()
            .all(|meaning| meaning.as_str().is_some_and(|m| !m.is_empty())));
    }

    #[test]
    fn only_a_confident_known_use_case_routes() {
        assert_eq!(
            route(&pick("database", 0.9), 0.7).use_case,
            UseCase::Database
        );
        assert_eq!(route(&pick("code", 0.7), 0.7).use_case, UseCase::Code);
        let unsure = route(&pick("action", 0.5), 0.7);
        assert_eq!(
            (unsure.use_case, unsure.probability),
            (UseCase::Ambiguous, 0.5)
        );
        assert_eq!(
            route(&pick("made-up", 0.99), 0.7).use_case,
            UseCase::Ambiguous
        );
        assert_eq!(
            route(&Answer::Noul { noul: 0.99 }, 0.7).use_case,
            UseCase::Ambiguous
        );
    }
}

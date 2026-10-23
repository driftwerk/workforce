//! Deciding whether a message is on-topic for a scoped agent: the pre-agent
//! gate that today costs a model turn per inbound message.
//!
//! One yes/no question whose criteria are the scope's own topic definition,
//! asked of the request ([`crate::request::state`]). The gate fails open, so
//! only a confident "off-topic" rejects: `p(off-topic)` must reach the floor.

use crate::question::{Answer, NoulCriteria, Question};
use serde_json::Value;
use std::collections::BTreeMap;

/// Id of the question in the request and the answer.
pub const QUESTION_ID: &str = "on_topic";

/// What the model concluded about a message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeVerdict {
    pub off_topic: bool,
    /// The model's probability that the message is off-topic.
    pub p_off_topic: f64,
}

/// The questions: one yes/no, is the message on-topic judged by the scope's
/// topic.
pub fn questions(topic: &str) -> BTreeMap<String, Question> {
    BTreeMap::from([(QUESTION_ID.to_string(), question(topic))])
}

fn question(topic: &str) -> Question {
    Question::Noul {
        instructions: Value::String(
            "Is this message on-topic for an assistant with the following remit? Answer yes \
             when the message is something that assistant should handle, no otherwise."
                .into(),
        ),
        criteria: Some(NoulCriteria {
            yes: Some(Value::String(format!(
                "The message is about, or asks for help with: {topic}"
            ))),
            no: Some(Value::String(
                "The message is small talk, or about something outside that remit".into(),
            )),
        }),
    }
}

/// Off-topic only when the model is at least `floor` sure of it; anything
/// less — or an answer of the wrong shape — lets the message through, as the
/// gate always has on doubt.
pub fn verdict(answer: &Answer, floor: f64) -> ScopeVerdict {
    match answer {
        Answer::Noul { noul } => ScopeVerdict {
            off_topic: 1.0 - noul >= floor,
            p_off_topic: 1.0 - noul,
        },
        _ => ScopeVerdict {
            off_topic: false,
            p_off_topic: 0.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_judges_by_the_topic() {
        match questions("bug reports and triage")
            .remove(QUESTION_ID)
            .unwrap()
        {
            Question::Noul {
                criteria: Some(c), ..
            } => {
                assert!(c
                    .yes
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .ends_with("bug reports and triage"));
                assert!(c.no.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_a_confident_off_topic_rejects() {
        let sure = verdict(&Answer::Noul { noul: 0.1 }, 0.7);
        assert!(sure.off_topic);
        assert!((sure.p_off_topic - 0.9).abs() < 1e-9);
        assert!(!verdict(&Answer::Noul { noul: 0.4 }, 0.7).off_topic);
        assert!(!verdict(&Answer::Noul { noul: 0.95 }, 0.7).off_topic);
        let wrong_shape = Answer::Choice {
            choice: "x".into(),
            probabilities: Default::default(),
            confidence: 1.0,
        };
        assert!(!verdict(&wrong_shape, 0.7).off_topic);
        assert!(
            !verdict(&wrong_shape, 0.0).off_topic,
            "never rejects on shape alone"
        );
    }
}

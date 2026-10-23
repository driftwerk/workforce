//! Recorded examples to training records, through the very builder the
//! live path sends with ([`Input::request`]): one example in, one
//! `{state, questions, label}` out, carrying the example's own tags so a
//! record joins back to where it came from. Where the examples come from
//! (workflow instances, decision records, a labelling pass) is the caller's
//! business.

use crate::input::{Input, Request};
use crate::question::Question;
use crate::text::RequestBudget;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// What an example says about itself. None of it reaches the model: it is
/// carried from an example to its record untouched.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Tags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Who gave the label: `agent`, `person`, `correction`, `classifier` …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// How much the example counts for; 1 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// The day it was recorded, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    /// Anything else worth keeping beside the record.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

/// One recorded decision: the inputs its use builds a request from, and the
/// right answer (`null` while nobody has given one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Example {
    #[serde(flatten)]
    pub input: Input,
    pub label: Value,
    #[serde(flatten)]
    pub tags: Tags,
}

/// What the model is trained on: exactly the `state` and `questions` a live
/// request carries, and the label it should have answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(flatten)]
    pub tags: Tags,
    pub state: Value,
    #[serde(serialize_with = "as_sent")]
    pub questions: BTreeMap<String, Question>,
    pub label: Value,
}

/// `questions` in the bytes a live request carries them in: the client puts
/// them in a `Value`, whose objects order their keys.
fn as_sent<S: serde::Serializer>(
    questions: &BTreeMap<String, Question>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde_json::to_value(questions)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

/// Why an example made no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LeftOut {
    /// Nobody has labelled it.
    Unlabelled,
    /// The live path would have asked nothing of it.
    Empty,
}

impl LeftOut {
    pub fn as_str(self) -> &'static str {
        match self {
            LeftOut::Unlabelled => "no label",
            LeftOut::Empty => "nothing to decide about",
        }
    }
}

/// The record `example` trains on within `budget`.
pub fn record(example: &Example, budget: RequestBudget) -> Result<Record, LeftOut> {
    if example.label.is_null() {
        return Err(LeftOut::Unlabelled);
    }
    let Request { state, questions } = example.input.request(budget).ok_or(LeftOut::Empty)?;
    Ok(Record {
        tags: example.tags.clone(),
        state,
        questions,
        label: example.label.clone(),
    })
}

/// One JSONL line of recorded example to one JSONL line of training record,
/// or why it makes none.
pub fn line(
    example: &str,
    budget: RequestBudget,
) -> Result<Result<String, LeftOut>, serde_json::Error> {
    match record(&serde_json::from_str(example)?, budget) {
        Ok(record) => serde_json::to_string(&record).map(Ok),
        Err(left_out) => Ok(Err(left_out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch::{self, BranchOption, StepResult};
    use crate::text::StateBudget;
    use crate::{question, request, routing, scope};
    use serde_json::json;

    fn exported(example: Value) -> Record {
        let out = line(&example.to_string(), RequestBudget::DEFAULT)
            .unwrap()
            .expect("a record");
        serde_json::from_str(&out).unwrap()
    }

    /// The client sends `{model, state, questions}` built as a `Value`; a
    /// record's `state` and `questions` are those very bytes.
    #[test]
    fn a_record_carries_state_and_questions_in_the_bytes_a_live_request_does() {
        let questions = scope::questions("billing");
        let state = json!({"message": "Refund me", "agent": "ops"});
        let sent =
            json!({"model": "kev-latest", "state": state, "questions": questions}).to_string();
        let record = Record {
            tags: Tags::default(),
            state: state.clone(),
            questions: questions.clone(),
            label: json!({"q": "yes"}),
        };
        let line = serde_json::to_string(&record).unwrap();
        for span in [
            format!("\"state\":{state}"),
            format!("\"questions\":{}", json!(questions)),
        ] {
            assert!(sent.contains(&span), "{span} in {sent}");
            assert!(line.contains(&span), "{span} in {line}");
        }
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), record);
    }

    #[test]
    fn a_branch_example_becomes_the_state_and_question_the_fallback_sends() {
        let result = StepResult {
            step: "Review".into(),
            outputs: BTreeMap::from([("verdict".into(), "tests missing".into())]),
            report: Some("Needs tests.".into()),
            response: "Looked at it.\nPlease add tests.".into(),
        };
        let branches = vec![
            BranchOption::new("approved", Some("Merge".into())),
            BranchOption::new("changes-requested", None),
        ];
        let task = "If anything is missing: OUTCOME: changes-requested";
        let record = exported(json!({
            "use": "branch_fallback",
            "step": "Review",
            "task": task,
            "outputs": {"verdict": "tests missing"},
            "report": "Needs tests.",
            "response": "Looked at it.\nPlease add tests.",
            "branches": [
                {"label": "approved", "leads_to": "Merge"},
                {"label": "changes-requested"}
            ],
            "label": "changes-requested",
        }));
        let questions = branch::questions(task, &branches);
        assert_eq!(
            Some(record.state),
            branch::state(
                &result,
                &branches,
                RequestBudget::DEFAULT.state(question::chars(&questions))
            )
        );
        assert_eq!(record.questions, questions);
        assert_eq!(record.label, "changes-requested");
    }

    #[test]
    fn request_examples_share_one_state_and_differ_in_their_questions() {
        let message = "Deploy failed.\n```\na\nb\nc\n```\nWhich commit broke it?";
        let scope_record = exported(json!({
            "use": "scope_gate", "message": message, "topic": "deploys", "label": true
        }));
        let routing_record = exported(json!({
            "use": "routing", "message": message, "label": "investigation"
        }));
        assert_eq!(
            scope_record.state,
            json!({"message": "Deploy failed.\n[3 lines of code]\nWhich commit broke it?"})
        );
        assert_eq!(
            Some(&scope_record.state),
            request::state(message, StateBudget::DEFAULT).as_ref()
        );
        assert_eq!(routing_record.state, scope_record.state);
        assert_eq!(scope_record.questions, scope::questions("deploys"));
        assert_eq!(routing_record.questions, routing::questions());
        assert_eq!(scope_record.label, true);
    }

    #[test]
    fn supplied_states_are_parsed_and_wrapped_as_the_step_and_the_tool_do() {
        let questions = json!({"kind": {"type": "noul", "instructions": "Is it a bug?"}});
        let step = exported(json!({
            "use": "decision_step", "task": r#"{"title": "Crash"}"#,
            "questions": questions, "label": "yes"
        }));
        assert_eq!(step.state, json!({"title": "Crash"}));
        let tool = exported(json!({
            "use": "tool", "state": "It crashes on save", "questions": questions, "label": "yes"
        }));
        assert_eq!(tool.state, json!({"state": "It crashes on save"}));
        assert_eq!(tool.questions["kind"], Question::noul("Is it a bug?"));
    }

    #[test]
    fn an_example_with_nothing_to_decide_about_is_no_record() {
        let questions = json!({"kind": {"type": "noul", "instructions": "Is it a bug?"}});
        for nothing in [
            json!({"use": "routing", "message": "  ", "label": "ambiguous"}),
            json!({"use": "scope_gate", "message": "", "topic": "t", "label": false}),
            json!({"use": "branch_fallback", "step": "s", "response": "\n", "branches": [], "label": "x"}),
            json!({"use": "decision_step", "task": " ", "questions": questions, "label": "no"}),
            json!({"use": "tool", "state": {}, "questions": questions, "label": "no"}),
        ] {
            assert_eq!(
                line(&nothing.to_string(), RequestBudget::DEFAULT).unwrap(),
                Err(LeftOut::Empty),
                "{nothing}"
            );
        }
    }

    #[test]
    fn a_line_that_is_not_an_example_is_an_error() {
        assert!(line("{}", RequestBudget::DEFAULT).is_err());
        assert!(line(r#"{"use": "nonsense"}"#, RequestBudget::DEFAULT).is_err());
        assert!(line(
            r#"{"use": "routing", "message": "hi"}"#,
            RequestBudget::DEFAULT
        )
        .is_err());
    }

    #[test]
    fn an_example_round_trips_through_its_json() {
        let example: Example = serde_json::from_value(json!({
            "use": "branch_fallback", "step": "Review", "response": "ok",
            "branches": [{"label": "approved"}], "label": "approved",
        }))
        .unwrap();
        let again: Example =
            serde_json::from_value(serde_json::to_value(&example).unwrap()).unwrap();
        assert_eq!(again, example);
    }

    #[test]
    fn a_record_carries_its_examples_tags_and_an_untagged_one_carries_none() {
        let tagged = json!({
            "use": "routing", "message": "Why did the import fail?", "agent": "ops",
            "channel": "slack", "label": "investigation",
            "id": "routing:42", "source": "person", "weight": 3.0, "day": "2026-10-01",
            "meta": {"session": "s1"},
        });
        let out: Value = serde_json::from_str(
            &line(&tagged.to_string(), RequestBudget::DEFAULT)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({
                "id": "routing:42", "source": "person", "weight": 3.0, "day": "2026-10-01",
                "meta": {"session": "s1"},
                "state": {"message": "Why did the import fail?"},
                "questions": routing::questions(),
                "label": "investigation",
            })
        );
        let bare = line(
            &json!({"use": "routing", "message": "hi", "label": "ambiguous"}).to_string(),
            RequestBudget::DEFAULT,
        )
        .unwrap()
        .unwrap();
        let bare: Value = serde_json::from_str(&bare).unwrap();
        assert_eq!(
            bare.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["label", "questions", "state"]
        );
    }

    #[test]
    fn an_example_nobody_labelled_is_left_out_and_says_so() {
        let unlabelled = json!({"use": "routing", "message": "hi", "label": null});
        assert_eq!(
            line(&unlabelled.to_string(), RequestBudget::DEFAULT).unwrap(),
            Err(LeftOut::Unlabelled)
        );
        assert_eq!(LeftOut::Unlabelled.as_str(), "no label");
        assert_eq!(LeftOut::Empty.as_str(), "nothing to decide about");
    }

    #[test]
    fn a_routing_example_from_before_agent_and_channel_builds_the_same_request() {
        let old = exported(json!({"use": "routing", "message": "Deploy it", "label": "action"}));
        let new = exported(json!({
            "use": "routing", "message": "Deploy it", "agent": "ops", "channel": "gateway",
            "label": "action"
        }));
        assert_eq!((old.state, old.questions), (new.state, new.questions));
    }
}

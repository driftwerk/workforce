//! What each use builds its request from, and the one place it is built.
//! The hub's live paths and the offline exporter both go through
//! [`Input::request`], so a model is trained on exactly the states and
//! questions it is later asked.

use crate::branch::{self, BranchOption, StepResult};
use crate::call::Use;
use crate::first::Floors;
use crate::question::{self, Question};
use crate::text::{self, RequestBudget, StateBudget};
use crate::{request, routing, scope, supplied};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The inputs of one decision, tagged with its use
/// (`"use": "branch_fallback"` …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "use", rename_all = "snake_case")]
pub enum Input {
    BranchFallback {
        #[serde(flatten)]
        result: StepResult,
        /// The step's task as its definition writes it, unrendered.
        #[serde(default)]
        task: String,
        branches: Vec<BranchOption>,
    },
    ScopeGate {
        message: String,
        /// The scope's resolved topic definition.
        topic: String,
    },
    Routing {
        message: String,
        /// The agent the request was for and the kind of channel it came
        /// in on (`slack`, `gateway`, …). Carried for whoever labels or
        /// stratifies examples; the state is the message alone, the same
        /// one the scope gate reads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    DecisionStep {
        /// The step's rendered task.
        task: String,
        questions: BTreeMap<String, Question>,
    },
    Tool {
        state: Value,
        questions: BTreeMap<String, Question>,
    },
    DecideFirst {
        /// The step's rendered `decide_first.state`.
        state: String,
        questions: BTreeMap<String, Question>,
        /// What an answer had to reach to settle the step. Carried for
        /// whoever judges a recorded answer; never sent.
        floors: Floors,
    },
}

/// What a backend is sent, the model name aside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

impl Input {
    pub fn usage(&self) -> Use {
        match self {
            Input::BranchFallback { .. } => Use::BranchFallback,
            Input::ScopeGate { .. } => Use::ScopeGate,
            Input::Routing { .. } => Use::Routing,
            Input::DecisionStep { .. } => Use::DecisionStep,
            Input::Tool { .. } => Use::Tool,
            Input::DecideFirst { .. } => Use::DecideFirst,
        }
    }

    /// The request this input makes within `budget`, or `None` when it
    /// carries nothing to decide about: a blank message, a step that
    /// produced no text, an empty state. The questions take what they need
    /// up to half the budget and the state is built within the rest, so
    /// the whole request stays inside the length a model is trained at.
    pub fn request(&self, budget: RequestBudget) -> Option<Request> {
        let questions = question::fit(self.questions(), budget.questions());
        let state = self.state(budget.state(question::chars(&questions)))?;
        Some(Request { state, questions })
    }

    /// The questions this input asks, as written: before a budget holds
    /// them to their share.
    pub fn questions(&self) -> BTreeMap<String, Question> {
        match self {
            Input::BranchFallback { task, branches, .. } => branch::questions(task, branches),
            Input::ScopeGate { topic, .. } => scope::questions(topic),
            Input::Routing { .. } => routing::questions(),
            Input::DecisionStep { questions, .. }
            | Input::Tool { questions, .. }
            | Input::DecideFirst { questions, .. } => questions.clone(),
        }
    }

    fn state(&self, budget: StateBudget) -> Option<Value> {
        match self {
            Input::BranchFallback {
                result, branches, ..
            } => branch::state(result, branches, budget),
            Input::ScopeGate { message, .. } | Input::Routing { message, .. } => {
                request::state(message, budget)
            }
            Input::DecisionStep { task, .. } => supplied::state(supplied::parse(task), budget),
            Input::DecideFirst { state, .. } => supplied::state(supplied::parse(state), budget),
            Input::Tool { state, .. } => supplied::state(state.clone(), budget),
        }
    }
}

impl Request {
    /// The characters of text the request carries: what a budget counts.
    pub fn chars(&self) -> usize {
        text::chars(&self.state) + question::chars(&self.questions)
    }

    /// This request and `other` as one call, when that changes neither:
    /// they ask of the same state, under different ids, and fit `budget`
    /// together. Otherwise each is asked alone, as it is trained.
    pub fn joined(&self, other: &Request, budget: RequestBudget) -> Option<Request> {
        let questions = question::batch([self.questions.clone(), other.questions.clone()]).ok()?;
        let joined = Request {
            state: self.state.clone(),
            questions,
        };
        (self.state == other.state && joined.chars() <= budget.get()).then_some(joined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn branching(options: usize, rule: usize, response: &str) -> Input {
        let labels: Vec<String> = (0..options).map(|i| format!("outcome-{i}")).collect();
        let task: String = labels
            .iter()
            .map(|label| {
                format!(
                    "{} then OUTCOME: {label}\n",
                    "When this holds. ".repeat(rule)
                )
            })
            .collect();
        Input::BranchFallback {
            result: StepResult {
                step: "Triage the ticket".into(),
                response: response.into(),
                ..Default::default()
            },
            task,
            branches: labels
                .into_iter()
                .map(|label| BranchOption::new(label, None))
                .collect(),
        }
    }

    #[test]
    fn a_whole_request_stays_within_its_budget_however_long_its_options_are() {
        let response = "I read the ticket and its history. ".repeat(200);
        for (options, budget) in [(2, 1_500), (6, 1_500), (12, 1_500), (6, 400)] {
            let budget = RequestBudget::chars(budget);
            let request = branching(options, 30, &response).request(budget).unwrap();
            let Question::Choice {
                criteria,
                instructions,
            } = &request.questions[branch::QUESTION_ID]
            else {
                panic!("choice expected");
            };
            assert_eq!(criteria.len(), options + 1, "every option is still offered");
            // What is never cut: the option names and what is asked.
            let names: usize = criteria.keys().map(|name| name.chars().count()).sum();
            let fixed = names + text::chars(instructions);
            let asked = question::chars(&request.questions);
            assert!(
                asked <= budget.questions().max(fixed),
                "{options} options: the questions take at most half, or only what is never cut"
            );
            assert!(
                request.chars() <= budget.get().max(budget.get() - budget.questions() + fixed),
                "{options} options: {} > {}",
                request.chars(),
                budget.get()
            );
            assert_eq!(
                fixed > budget.questions(),
                budget.get() == 400,
                "only the 400-character request is too small for what it must ask"
            );
            assert_eq!(
                branching(options, 30, &response).request(budget),
                Some(request),
                "stable"
            );
        }
    }

    #[test]
    fn option_names_too_long_for_the_budget_still_leave_a_state_to_ask_of() {
        let budget = RequestBudget::chars(200);
        let request = branching(40, 30, "It is outcome-3.")
            .request(budget)
            .unwrap();
        assert_eq!(request.state["final_lines"], "It is outcome-3.");
        let Question::Choice {
            criteria,
            instructions,
        } = &request.questions[branch::QUESTION_ID]
        else {
            panic!("choice expected");
        };
        assert_eq!(criteria.len(), 41);
        let written = branching(40, 30, "It is outcome-3.").questions();
        let Question::Choice {
            instructions: asked,
            ..
        } = &written[branch::QUESTION_ID]
        else {
            panic!("choice expected");
        };
        assert_eq!(instructions, asked, "what is asked is never cut");
        assert!(text::chars(instructions) > 0);
    }

    #[test]
    fn questions_that_fit_are_sent_as_written_and_the_state_gets_the_rest() {
        let input = branching(2, 1, &"Some working notes. ".repeat(300));
        let budget = RequestBudget::DEFAULT;
        let request = input.request(budget).unwrap();
        assert_eq!(request.questions, input.questions());
        let asked = question::chars(&request.questions);
        assert!(asked < budget.questions());
        assert!(
            text::chars(&request.state) > budget.get() - asked - 40,
            "the state uses what the questions leave"
        );
    }

    #[test]
    fn long_criteria_are_cut_to_their_opening_and_short_ones_kept_whole() {
        let questions = BTreeMap::from([(
            "q".to_string(),
            Question::choice(
                "Which?",
                [
                    ("a", json!("Short rule.")),
                    ("b", json!(format!("Starts here. {}", "More. ".repeat(200)))),
                    ("c", Value::Null),
                ],
            ),
        )]);
        let fitted = question::fit(questions.clone(), 120);
        assert!(question::chars(&fitted) <= 120);
        let Question::Choice {
            instructions,
            criteria,
        } = &fitted["q"]
        else {
            panic!("choice expected");
        };
        assert_eq!(instructions, "Which?");
        assert_eq!(criteria["a"], "Short rule.");
        assert!(criteria["b"].as_str().unwrap().starts_with("Starts here."));
        assert_eq!(criteria["c"], Value::Null);
        assert_eq!(question::fit(questions.clone(), 5_000), questions);
    }

    #[test]
    fn two_requests_are_one_call_only_when_that_changes_neither() {
        let budget = RequestBudget::DEFAULT;
        let about = |message: &str| {
            let scope = Input::ScopeGate {
                message: message.into(),
                topic: "deploys".into(),
            };
            let routing = Input::Routing {
                message: message.into(),
                agent: None,
                channel: None,
            };
            (
                scope.request(budget).unwrap(),
                routing.request(budget).unwrap(),
            )
        };
        let (scope, routing) = about("Which deploy broke the importer?");
        let joined = scope.joined(&routing, budget).expect("one call");
        assert_eq!(joined.state, scope.state);
        assert_eq!(joined.questions.len(), 2);
        assert!(joined.chars() <= budget.get());

        let (scope, routing) =
            about(&"The importer has been failing since the deploy. ".repeat(60));
        assert_eq!(
            scope.joined(&routing, budget),
            None,
            "a message too long for both sets of questions is asked of each alone"
        );
        assert_eq!(scope.joined(&scope, budget), None, "the same id twice");
    }
}

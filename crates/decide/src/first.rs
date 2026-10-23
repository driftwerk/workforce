//! Settling an agent step's outcome before its turn runs.
//!
//! The question is a `Choice` over the outcomes the step can end in — its
//! declared branches, each meaning what the step's task says of it, plus
//! whatever options its author describes — and `none-of-these` unless the
//! author withholds it, asked of a short state the author wrote for the
//! purpose. An answer settles the step only when it reaches that option's
//! own floor: the common, cheap outcome can be settled at a floor the rare,
//! costly one is never allowed to reach, and everything else is left to the
//! agent's turn.

use crate::branch::{self, BranchOption, NONE_OF_THESE};
use crate::question::{Answer, Question};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Id of the question in the request and the answer.
pub const QUESTION_ID: &str = "outcome";

const QUESTION: &str =
    "Which of these outcomes does the state call for? Judge only by what the state says.";

/// What an option's probability must reach to settle a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Floors {
    /// The floor of an option that has none of its own.
    pub min_probability: f64,
    /// An option's own floor; `None` for one the model must never settle.
    #[serde(default)]
    pub options: BTreeMap<String, Option<f64>>,
}

impl Floors {
    /// The floor `option` must reach, or `None` when it may never settle.
    pub fn of(&self, option: &str) -> Option<f64> {
        self.options
            .get(option)
            .copied()
            .unwrap_or(Some(self.min_probability))
    }
}

/// Whether a settle at `floor` needs the chosen option to outweigh every
/// other together, so no answer split between two options reaches it.
/// Without the escape, only such a floor leaves a near-tie to the agent.
pub fn breaks_ties(floor: f64) -> bool {
    floor > 0.5
}

/// The questions: one `Choice` over the step's branch labels, each
/// described from its `task` as the branch fallback describes them, then
/// the options the author `described` (a branch's meaning replaced, any
/// other option added), and the escape option when `escape` offers it.
pub fn questions(
    question: Option<&str>,
    task: &str,
    branches: &[String],
    described: &BTreeMap<String, String>,
    escape: bool,
) -> BTreeMap<String, Question> {
    let branches: Vec<BranchOption> = branches
        .iter()
        .map(|label| BranchOption::new(label.clone(), None))
        .collect();
    let stated = described
        .iter()
        .filter(|(_, meaning)| !meaning.trim().is_empty())
        .map(|(option, meaning)| (option.clone(), Value::String(meaning.clone())));
    let unstated = described.keys().map(|option| (option.clone(), Value::Null));
    let criteria: BTreeMap<String, Value> = unstated
        .chain(
            branches
                .iter()
                .map(|branch| branch.label.clone())
                .zip(branch::criteria(task, &branches)),
        )
        .chain(stated)
        .chain(escape.then(|| {
            (
                NONE_OF_THESE.to_string(),
                Value::String(
                    "None of the outcomes above clearly applies, or the state does not say enough to tell"
                        .to_string(),
                ),
            )
        }))
        .collect();
    let question = Question::Choice {
        instructions: json!({ "question": question.unwrap_or(QUESTION) }),
        criteria,
    };
    BTreeMap::from([(QUESTION_ID.to_string(), question)])
}

/// The options `questions` offer, the escape option aside.
pub fn options(questions: &BTreeMap<String, Question>) -> Vec<&str> {
    match questions.get(QUESTION_ID) {
        Some(Question::Choice { criteria, .. }) => criteria
            .keys()
            .map(String::as_str)
            .filter(|option| *option != NONE_OF_THESE)
            .collect(),
        _ => Vec::new(),
    }
}

/// The options `questions` offer that nothing describes: a branch the
/// step's task never speaks of and the block does not describe, or an
/// option described as nothing. A model cannot tell such an option from
/// the others, so a question with one is not asked.
pub fn unmeant(questions: &BTreeMap<String, Question>) -> Vec<&str> {
    match questions.get(QUESTION_ID) {
        Some(Question::Choice { criteria, .. }) => criteria
            .iter()
            .filter(|(_, meaning)| meaning.is_null())
            .map(|(option, _)| option.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

/// The outcome `answer` settles the step on, with its probability: the
/// chosen one of `options` when it reached its own floor. `None` for the
/// escape option or anything else that was not offered, an option that may
/// never settle, an unsure answer, or an answer of another shape.
pub fn settle(answer: &Answer, options: &[&str], floors: &Floors) -> Option<(String, f64)> {
    let Answer::Choice {
        choice,
        probabilities,
        ..
    } = answer
    else {
        return None;
    };
    if !options.contains(&choice.as_str()) {
        return None;
    }
    let p = probabilities.get(choice).copied().unwrap_or(0.0);
    floors
        .of(choice)
        .filter(|floor| p >= *floor)
        .map(|_| (choice.clone(), p))
}

/// Whether the outcome label an agent `wrote` names `option`: the option
/// itself, or the option followed by something that is not more label
/// (`ignore — a newsletter`), as a branch is matched.
fn names(wrote: &str, option: &str) -> bool {
    let more_label = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    wrote
        .strip_prefix(option)
        .is_some_and(|rest| !rest.starts_with(more_label))
}

/// The option the agent's own turn chose, which is the label its run
/// teaches: the option its outcome line `wrote`, else the branch the step
/// was `routed` on, else — the step took its default route — the one option
/// that is not a branch, when there is exactly one, else the escape option.
pub fn agent_choice(
    options: &[&str],
    branches: &[String],
    wrote: Option<&str>,
    routed: Option<&str>,
) -> String {
    let written = wrote.and_then(|wrote| {
        options
            .iter()
            .copied()
            .filter(|option| names(wrote, option))
            .max_by_key(|option| option.len())
    });
    let default_route = || {
        let unrouted: Vec<&str> = options
            .iter()
            .copied()
            .filter(|option| !branches.iter().any(|branch| branch == option))
            .collect();
        match unrouted[..] {
            [only] => only,
            _ => NONE_OF_THESE,
        }
    };
    written
        .or(routed.filter(|routed| options.contains(routed)))
        .unwrap_or_else(default_route)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Input;
    use crate::text::RequestBudget;

    const TASK: &str = "Classify the ticket.\n\
        A crash or a wrong result: OUTCOME: bug\n\
        Customers cannot work at all: OUTCOME: urgent\n\
        Anything else needs no action.";

    fn branches() -> Vec<String> {
        vec!["bug".to_string(), "urgent".to_string()]
    }

    fn described() -> BTreeMap<String, String> {
        BTreeMap::from([(
            "ignore".to_string(),
            "A newsletter, an auto-reply or spam".to_string(),
        )])
    }

    fn criteria(described: &BTreeMap<String, String>) -> BTreeMap<String, Value> {
        match questions(None, TASK, &branches(), described, true).remove(QUESTION_ID) {
            Some(Question::Choice { criteria, .. }) => criteria,
            other => panic!("choice expected, got {other:?}"),
        }
    }

    fn choice(top: &str, p: f64) -> Answer {
        Answer::Choice {
            choice: top.into(),
            probabilities: BTreeMap::from([(top.to_string(), p)]),
            confidence: p,
        }
    }

    fn floors() -> Floors {
        Floors {
            min_probability: 0.9,
            options: BTreeMap::from([
                ("urgent".to_string(), None),
                ("bug".to_string(), Some(0.95)),
            ]),
        }
    }

    #[test]
    fn the_options_are_the_branches_as_the_task_means_them_plus_what_the_author_describes() {
        let criteria = criteria(&described());
        assert_eq!(
            criteria.keys().collect::<Vec<_>>(),
            ["bug", "ignore", NONE_OF_THESE, "urgent"]
        );
        assert_eq!(criteria["bug"], "A crash or a wrong result: OUTCOME: bug");
        assert_eq!(criteria["ignore"], "A newsletter, an auto-reply or spam");
        assert!(
            criteria[NONE_OF_THESE].is_string(),
            "the escape is always there"
        );
        let asked = questions(None, TASK, &branches(), &described(), true);
        assert_eq!(options(&asked), ["bug", "ignore", "urgent"]);
    }

    #[test]
    fn a_withheld_escape_offers_only_the_options_and_an_unsure_answer_still_settles_nothing() {
        let asked = questions(None, TASK, &branches(), &described(), false);
        let Question::Choice { criteria, .. } = &asked[QUESTION_ID] else {
            panic!("choice expected");
        };
        assert_eq!(
            criteria.keys().collect::<Vec<_>>(),
            ["bug", "ignore", "urgent"]
        );
        assert_eq!(options(&asked), OFFERED);
        assert_eq!(
            settle(&choice("ignore", 0.89), &options(&asked), &floors()),
            None
        );
        assert_eq!(
            settle(&choice("urgent", 1.0), &options(&asked), &floors()),
            None
        );
        assert_eq!(
            agent_choice(&options(&asked), &branches(), None, None),
            "ignore",
            "the default route is still the one option that is no branch"
        );
    }

    #[test]
    fn a_described_branch_replaces_the_tasks_meaning_and_a_blank_one_keeps_it() {
        let own = BTreeMap::from([
            ("bug".to_string(), "Something is broken".to_string()),
            ("urgent".to_string(), " ".to_string()),
            ("later".to_string(), String::new()),
        ]);
        let criteria = criteria(&own);
        assert_eq!(criteria["bug"], "Something is broken");
        assert_eq!(
            criteria["urgent"],
            "Customers cannot work at all: OUTCOME: urgent"
        );
        assert_eq!(criteria["later"], Value::Null);
        assert_eq!(
            unmeant(&questions(None, TASK, &branches(), &own, true)),
            ["later"]
        );
        let silent = ["bug".to_string(), "refund".to_string()];
        assert_eq!(
            unmeant(&questions(None, TASK, &silent, &described(), true)),
            ["refund"],
            "a branch the task never speaks of means nothing to the model"
        );
        assert!(unmeant(&questions(None, TASK, &branches(), &described(), true)).is_empty());
    }

    #[test]
    fn the_authors_question_is_asked_in_place_of_the_stock_one() {
        let asked = |question| match &questions(question, TASK, &branches(), &described(), true)
            [QUESTION_ID]
        {
            Question::Choice { instructions, .. } => instructions["question"].clone(),
            other => panic!("choice expected, got {other:?}"),
        };
        assert_eq!(asked(None), QUESTION);
        assert_eq!(asked(Some("What is this ticket?")), "What is this ticket?");
    }

    const OFFERED: [&str; 3] = ["bug", "ignore", "urgent"];

    fn settled(answer: &Answer) -> Option<(String, f64)> {
        settle(answer, &OFFERED, &floors())
    }

    #[test]
    fn an_answer_settles_only_at_its_own_options_floor() {
        assert_eq!(
            settled(&choice("ignore", 0.93)),
            Some(("ignore".to_string(), 0.93))
        );
        assert_eq!(settled(&choice("ignore", 0.9)).unwrap().1, 0.9);
        assert_eq!(settled(&choice("ignore", 0.89)), None);
        assert_eq!(settled(&choice("bug", 0.93)), None, "its own floor");
        assert!(settled(&choice("bug", 0.96)).is_some());
    }

    #[test]
    fn a_never_option_the_escape_and_an_answer_nobody_offered_settle_nothing() {
        let floors = floors();
        assert_eq!(settled(&choice("urgent", 1.0)), None);
        assert_eq!(settled(&choice(NONE_OF_THESE, 1.0)), None);
        assert_eq!(settled(&choice("refund", 1.0)), None);
        assert_eq!(settled(&Answer::Noul { noul: 1.0 }), None);
        assert_eq!(floors.of("urgent"), None);
        assert_eq!(floors.of("ignore"), Some(0.9));
    }

    #[test]
    fn the_agents_choice_is_its_line_then_its_branch_then_the_default_routes_option() {
        let options = ["bug", "ignore", "urgent"];
        let chose = |wrote, routed| agent_choice(&options, &branches(), wrote, routed);
        assert_eq!(chose(Some("bug"), Some("bug")), "bug");
        assert_eq!(chose(Some("ignore — a newsletter"), None), "ignore");
        assert_eq!(chose(None, Some("urgent")), "urgent");
        assert_eq!(
            chose(None, None),
            "ignore",
            "the one option that is no branch"
        );
        assert_eq!(
            chose(Some("ignored"), None),
            "ignore",
            "not its name, so the default"
        );
        assert_eq!(chose(Some("nonsense"), Some("bug")), "bug");

        let only_branches = ["bug", "urgent"];
        assert_eq!(
            agent_choice(&only_branches, &branches(), None, None),
            NONE_OF_THESE
        );
        let two_defaults = ["bug", "ignore", "later", "urgent"];
        assert_eq!(
            agent_choice(&two_defaults, &branches(), None, None),
            NONE_OF_THESE,
            "nothing says which of two default options it was"
        );
        assert_eq!(
            agent_choice(&two_defaults, &branches(), Some("later"), None),
            "later"
        );
    }

    #[test]
    fn the_floors_ride_on_the_input_and_never_in_the_request() {
        let input = |floors: Floors| Input::DecideFirst {
            state: r#"{"subject": "Weekly digest", "body": "Unsubscribe here"}"#.into(),
            questions: questions(None, TASK, &branches(), &described(), true),
            floors,
        };
        let strict = input(floors()).request(RequestBudget::DEFAULT).unwrap();
        let lax = input(Floors {
            min_probability: 0.5,
            options: BTreeMap::new(),
        })
        .request(RequestBudget::DEFAULT)
        .unwrap();
        assert_eq!(strict, lax);
        assert_eq!(
            serde_json::to_string(&strict.state).unwrap(),
            r#"{"body":"Unsubscribe here","subject":"Weekly digest"}"#
        );
        let stored = serde_json::to_value(input(floors())).unwrap();
        assert_eq!(stored["use"], "decide_first");
        assert_eq!(stored["floors"]["options"]["urgent"], Value::Null);
        assert_eq!(
            serde_json::from_value::<Input>(stored).unwrap(),
            input(floors())
        );
    }
}

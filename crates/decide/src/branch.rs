//! Choosing a workflow branch from a step's result when the agent forgot, or
//! garbled, its `OUTCOME:` line.
//!
//! The question is a `Choice` over the step's own declared labels plus
//! `none-of-these`, asked of what the step concluded: its declared outputs,
//! its report and its closing lines. The labels are a closed set the
//! workflow author wrote, which is exactly what a System One model is for;
//! the escape option and a probability floor keep it from guessing when the
//! text reached no verdict.
//!
//! A branch is written as `{ when, goto }`, so the definition never states
//! what a label means. The step's task does: it is where the author tells
//! the agent when to choose each outcome, so a label's criterion is the
//! task's own sentences about it. Only a label the task never mentions is
//! described by where it leads.

use crate::question::{Answer, Question};
use crate::text::{fit, fit_all, names, plain, Keep, StateBudget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The option that means "the text reaches none of the declared outcomes".
pub const NONE_OF_THESE: &str = "none-of-these";

/// Id of the question in the request and the answer.
pub const QUESTION_ID: &str = "branch";

/// What a garbled outcome line still starts with: the best hint there is.
const OUTCOME_LINE: &str = "OUTCOME:";

/// How much of the task a label's criterion may quote, in characters.
const CRITERION_CHARS: usize = 240;

/// A branch a step declares: its label and the name of the step it routes
/// to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchOption {
    pub label: String,
    #[serde(default)]
    pub leads_to: Option<String>,
}

impl BranchOption {
    pub fn new(label: impl Into<String>, leads_to: Option<String>) -> Self {
        Self {
            label: label.into(),
            leads_to,
        }
    }
}

/// What a step produced, as the workflow recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StepResult {
    /// The step's name, or its id when it has none.
    pub step: String,
    /// The declared outputs the response carried.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    /// The step's report, when it has one.
    #[serde(default)]
    pub report: Option<String>,
    /// The agent's whole response.
    pub response: String,
}

/// `text` without its first `start` … `end` block. A block that never ends
/// is left in place unless `open_until` names what stops it instead.
fn without_block(text: &str, start: &str, end: &str, open_until: Option<&str>) -> String {
    let Some(from) = text.find(start) else {
        return text.to_string();
    };
    let rest = &text[from + start.len()..];
    let closed = rest.find(end).map(|at| at + end.len());
    let open = || open_until.map(|stop| rest.find(stop).unwrap_or(rest.len()));
    match closed.or_else(open) {
        Some(to) => format!("{}{}", &text[..from], &rest[to..]),
        None => text.to_string(),
    }
}

/// The response's prose: what is left once the blocks the state carries on
/// their own are taken out — `OUTPUTS:` always, `REPORT:` when the step has
/// a report. An outputs block cut short runs to the `OUTCOME:` line or the
/// end, as the workflow parser reads it.
fn prose(result: &StepResult) -> String {
    let prose = without_block(
        &result.response,
        "OUTPUTS:",
        "END_OUTPUTS",
        Some(OUTCOME_LINE),
    );
    match result.report {
        Some(_) => without_block(&prose, "REPORT:", "END_REPORT", None),
        None => prose,
    }
}

/// The state: which step this is and what it concluded — its outputs, its
/// report and its closing lines — sharing what the step's name leaves of the
/// budget, each with pasted logs and code collapsed. Sentences that name one of the `options`, and a
/// garbled outcome line, are kept before anything else, so a conclusion
/// reached in the middle of a long result is not cut away. `None` when the
/// step produced no text at all: there is nothing to decide about.
pub fn state(result: &StepResult, options: &[BranchOption], budget: StateBudget) -> Option<Value> {
    let prose = prose(result);
    let verdicts: Vec<String> = options
        .iter()
        .map(|option| option.label.clone())
        .chain([OUTCOME_LINE.to_string()])
        .collect();
    let texts: Vec<(&str, Keep<'_>)> = result
        .outputs
        .values()
        .map(|value| (value.as_str(), Keep::START))
        .chain([
            (
                result.report.as_deref().unwrap_or_default(),
                Keep::START.naming(&verdicts),
            ),
            (prose.as_str(), Keep::END.naming(&verdicts)),
        ])
        .collect();
    let named = result.step.chars().count();
    let mut fitted = fit_all(&texts, budget.get().saturating_sub(named)).into_iter();
    let outputs: Map<String, Value> = result
        .outputs
        .keys()
        .cloned()
        .zip(fitted.by_ref().map(Value::String))
        .filter(|(_, value)| value.as_str() != Some(""))
        .collect();
    let report = fitted.next().unwrap_or_default();
    let final_lines = fitted.next().unwrap_or_default();
    if outputs.is_empty() && report.is_empty() && final_lines.is_empty() {
        return None;
    }
    let mut state = Map::from_iter([("step".to_string(), json!(result.step))]);
    if !outputs.is_empty() {
        state.insert("outputs".into(), Value::Object(outputs));
    }
    state.extend(
        [("report", report), ("final_lines", final_lines)]
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(field, text)| (field.to_string(), Value::String(text))),
    );
    Some(Value::Object(state))
}

/// What choosing each label means, in `options`' order: the lines of the
/// step's `task` that speak of that label and of no other, else the step
/// the label leads to. A label contained in another (`done` in
/// `done-with-notes`) does not count as the other being named.
pub fn criteria(task: &str, options: &[BranchOption]) -> Vec<Value> {
    let labels: Vec<String> = options.iter().map(|o| plain(o.label.trim())).collect();
    let said: Vec<(String, &str)> = task
        .lines()
        .map(|line| (plain(line), line.trim()))
        .collect();
    options
        .iter()
        .zip(&labels)
        .map(|(option, label)| {
            let of_another = |text: &str| {
                labels
                    .iter()
                    .any(|other| other != label && !names(label, other) && names(text, other))
            };
            let about: Vec<&str> = said
                .iter()
                .filter(|(text, _)| names(text, label) && !of_another(text))
                .map(|(_, line)| *line)
                .collect();
            let led = || {
                option
                    .leads_to
                    .as_ref()
                    .map(|step| format!("What the step concluded calls for the next step: {step}"))
            };
            Some(about)
                .filter(|about| !about.is_empty())
                .map(|about| fit(&about.join("\n"), CRITERION_CHARS, Keep::START))
                .or_else(led)
                .map_or(Value::Null, Value::String)
        })
        .collect()
}

/// The questions: one `Choice` over the labels and the escape option, each
/// label described from the step's `task` as its author wrote it (the
/// definition's text, placeholders and all, so it is the same on every
/// run).
pub fn questions(task: &str, options: &[BranchOption]) -> BTreeMap<String, Question> {
    let mut criteria: BTreeMap<String, Value> = options
        .iter()
        .map(|o| o.label.clone())
        .zip(criteria(task, options))
        .collect();
    criteria.insert(
        NONE_OF_THESE.to_string(),
        Value::String(
            "The step reaches none of the outcomes above, or does not say which one it reached"
                .to_string(),
        ),
    );
    let question = Question::Choice {
        instructions: json!({
            "question": "Which of the declared outcomes did the step reach? Judge only by what its `outputs`, `report` and `final_lines` say it concluded, not by what they discuss.",
        }),
        criteria,
    };
    BTreeMap::from([(QUESTION_ID.to_string(), question)])
}

/// The label to route on, with its probability, or `None` when the model
/// chose the escape option, chose something outside the declared set, or was
/// not sure enough.
pub fn choose(
    answer: &Answer,
    options: &[BranchOption],
    min_probability: f64,
) -> Option<(String, f64)> {
    let Answer::Choice {
        choice,
        probabilities,
        ..
    } = answer
    else {
        return None;
    };
    if choice == NONE_OF_THESE || !options.iter().any(|o| &o.label == choice) {
        return None;
    }
    let p = probabilities.get(choice).copied().unwrap_or(0.0);
    (p >= min_probability).then(|| (choice.clone(), p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ELISION;

    fn options() -> Vec<BranchOption> {
        vec![
            BranchOption::new("approved", Some("Merge the PR".into())),
            BranchOption::new("changes-requested", None),
        ]
    }

    fn choice(top: &str, p: f64) -> Answer {
        Answer::Choice {
            choice: top.into(),
            probabilities: BTreeMap::from([
                (top.to_string(), p),
                (NONE_OF_THESE.to_string(), 1.0 - p),
            ]),
            confidence: p,
        }
    }

    fn criteria_of(task: &str, options: &[BranchOption]) -> BTreeMap<String, Value> {
        match questions(task, options).remove(QUESTION_ID) {
            Some(Question::Choice { criteria, .. }) => criteria,
            other => panic!("choice expected, got {other:?}"),
        }
    }

    const REVIEW_TASK: &str = "Review PR {{pr_number}} against the ticket.\n\
        - CI must be green. If failing → OUTCOME: changes-requested\n\
        - ALL checks pass: OUTCOME: approved\n\
        - ANY check fails: list issues by severity then OUTCOME: changes-requested\n\
        End with exactly one of approved or changes-requested.";

    #[test]
    fn a_label_means_what_the_task_says_of_it_and_of_no_other() {
        let criteria = criteria_of(REVIEW_TASK, &options());
        assert_eq!(
            criteria.keys().collect::<Vec<_>>(),
            ["approved", "changes-requested", NONE_OF_THESE]
        );
        assert_eq!(criteria["approved"], "- ALL checks pass: OUTCOME: approved");
        assert_eq!(
            criteria["changes-requested"],
            "- CI must be green. If failing → OUTCOME: changes-requested\n\
             - ANY check fails: list issues by severity then OUTCOME: changes-requested"
        );
        assert_eq!(
            questions(REVIEW_TASK, &options()),
            questions(REVIEW_TASK, &options()),
            "stable"
        );
    }

    #[test]
    fn a_label_the_task_never_mentions_is_described_by_where_it_leads() {
        let criteria = criteria_of("Review it.", &options());
        assert_eq!(
            criteria["approved"],
            "What the step concluded calls for the next step: Merge the PR"
        );
        assert_eq!(criteria["changes-requested"], Value::Null);
    }

    #[test]
    fn labels_that_lead_to_the_same_step_are_told_apart_by_the_task() {
        let to = |label: &str, step: &str| BranchOption::new(label, Some(step.into()));
        let shared = [
            to("approved", "Merge"),
            to("rejected", "Notify"),
            to("stuck", "Notify"),
        ];
        let task = "If the change is wrong in principle: OUTCOME: rejected\n\
                    If you cannot tell after two passes, say Stuck.";
        let criteria = criteria_of(task, &shared);
        assert_eq!(
            criteria["rejected"],
            "If the change is wrong in principle: OUTCOME: rejected"
        );
        assert_eq!(
            criteria["stuck"],
            "If you cannot tell after two passes, say Stuck."
        );
        assert_eq!(
            criteria["approved"],
            "What the step concluded calls for the next step: Merge"
        );
        let silent = criteria_of("Review it.", &shared);
        assert_eq!(silent["rejected"], silent["stuck"]);
        assert_eq!(
            silent["stuck"],
            "What the step concluded calls for the next step: Notify"
        );
    }

    #[test]
    fn a_label_inside_another_and_a_label_in_other_words_are_matched_as_words() {
        let nested = [
            BranchOption::new("done", None),
            BranchOption::new("done-with-notes", None),
            BranchOption::new("no", None),
        ];
        let task = "All good: done.\nGood but worth a remark: Done with notes.\n\
                    I do not know any reason to say otherwise.";
        let criteria = criteria_of(task, &nested);
        assert_eq!(criteria["done"], "All good: done.");
        assert_eq!(
            criteria["done-with-notes"],
            "Good but worth a remark: Done with notes."
        );
        assert_eq!(criteria["no"], Value::Null, "'know' and 'not' are not 'no'");
    }

    #[test]
    fn a_long_rule_is_held_to_the_criterion_budget() {
        let task = format!(
            "Approved means every check passed. {}",
            "Approved also needs this further condition to hold. ".repeat(40)
        );
        let criteria = criteria_of(&task, &options());
        let approved = criteria["approved"].as_str().unwrap();
        assert!(approved.chars().count() <= CRITERION_CHARS, "{approved}");
        assert!(approved.starts_with("Approved means every check passed."));
        assert!(approved.ends_with(ELISION));
    }

    #[test]
    fn only_a_confident_declared_label_routes() {
        let opts = options();
        assert_eq!(
            choose(&choice("approved", 0.86), &opts, 0.7),
            Some(("approved".into(), 0.86))
        );
        assert_eq!(choose(&choice("approved", 0.6), &opts, 0.7), None);
        assert_eq!(choose(&choice(NONE_OF_THESE, 0.9), &opts, 0.7), None);
        assert_eq!(choose(&choice("made-up", 0.99), &opts, 0.7), None);
        assert_eq!(choose(&Answer::Noul { noul: 0.99 }, &opts, 0.7), None);
    }

    fn result(response: &str) -> StepResult {
        StepResult {
            step: "Review the PR".into(),
            response: response.into(),
            ..Default::default()
        }
    }

    fn built(result: &StepResult, budget: StateBudget) -> Value {
        let state = state(result, &options(), budget).expect("a state");
        assert!(text_chars(&state) <= budget.get(), "{state}");
        assert_eq!(
            super::state(result, &options(), budget),
            Some(state.clone()),
            "the same input builds the same state"
        );
        state
    }

    fn text_chars(state: &Value) -> usize {
        let outputs = state.get("outputs").and_then(Value::as_object);
        outputs
            .into_iter()
            .flat_map(|o| o.values())
            .chain(state.get("step"))
            .chain(state.get("report"))
            .chain(state.get("final_lines"))
            .map(|v| v.as_str().unwrap().chars().count())
            .sum()
    }

    #[test]
    fn a_short_result_is_the_state_whole_with_each_part_once() {
        let response = "I read the diff.\n\nOUTPUTS:\nverdict: tests are missing\nEND_OUTPUTS\n\n\
                        REPORT:\nThe change needs tests before it can merge.\nEND_REPORT\n\n\
                        Please add tests and fix the panic.";
        let state = built(
            &StepResult {
                outputs: BTreeMap::from([("verdict".into(), "tests are missing".into())]),
                report: Some("The change needs tests before it can merge.".into()),
                ..result(response)
            },
            StateBudget::DEFAULT,
        );
        assert_eq!(
            state,
            json!({
                "step": "Review the PR",
                "outputs": {"verdict": "tests are missing"},
                "report": "The change needs tests before it can merge.",
                "final_lines": "I read the diff.\n\nPlease add tests and fix the panic.",
            })
        );
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"final_lines":"I read the diff.\n\nPlease add tests and fix the panic.","outputs":{"verdict":"tests are missing"},"report":"The change needs tests before it can merge.","step":"Review the PR"}"#,
            "the bytes a model is trained on: field order is part of the state"
        );
        assert_eq!(
            built(&result("LGTM, approving."), StateBudget::DEFAULT),
            json!({"step": "Review the PR", "final_lines": "LGTM, approving."})
        );
    }

    #[test]
    fn a_step_that_produced_nothing_has_no_state() {
        for nothing in ["", "  \n\t", "OUTPUTS:\nEND_OUTPUTS"] {
            assert_eq!(
                state(&result(nothing), &options(), StateBudget::DEFAULT),
                None,
                "{nothing:?}"
            );
        }
    }

    #[test]
    fn a_report_block_the_step_did_not_ask_for_stays_in_the_prose() {
        let response = "Checked.\nREPORT:\nApproved, nothing to change.\nEND_REPORT";
        assert_eq!(
            built(&result(response), StateBudget::DEFAULT)["final_lines"],
            response
        );
    }

    #[test]
    fn a_long_result_keeps_outputs_the_report_and_the_closing_lines_within_the_budget() {
        let chatter = "I looked at another file and found nothing of note. ".repeat(200);
        let log: String = (0..80)
            .map(|i| format!("2026-09-26T10:00:{:02}Z INFO test: case {i} ok\n", i % 60))
            .collect();
        let response = format!(
            "{chatter}\n```rust\nfn a() {{}}\nfn b() {{}}\nfn c() {{}}\n```\n{log}\
             OUTPUTS:\nsummary: two tests fail\nEND_OUTPUTS\n\
             REPORT:\nNot ready. {chatter}\nEND_REPORT\n\
             {chatter}\nSo the change is not ready: two tests fail and need fixing first."
        );
        let step = StepResult {
            outputs: BTreeMap::from([("summary".into(), "two tests fail".into())]),
            report: Some(format!("Not ready. {chatter}")),
            ..result(&response)
        };
        let state = built(&step, StateBudget::DEFAULT);
        assert_eq!(state["outputs"]["summary"], "two tests fail");
        assert!(state["report"].as_str().unwrap().starts_with("Not ready."));
        let closing = state["final_lines"].as_str().unwrap();
        assert!(
            closing.ends_with("two tests fail and need fixing first."),
            "{closing}"
        );
        assert!(closing.starts_with(ELISION));
        assert!(!closing.contains("END_REPORT") && !closing.contains("summary:"));

        let noisy = format!("Checked.\n{log}```\na\nb\nc\n```\nApproved, nothing to change.");
        assert_eq!(
            built(&result(&noisy), StateBudget::DEFAULT)["final_lines"],
            "Checked.\n[80 lines of log]\n[3 lines of code]\nApproved, nothing to change."
        );
    }

    #[test]
    fn a_conclusion_in_the_middle_of_a_long_result_is_kept() {
        let before = "I opened the next file and read through it carefully. ".repeat(60);
        let after = "Then I tidied my notes and listed the files I had read. ".repeat(60);
        for verdict in [
            "Having read all of it, the change is approved.",
            "Overall: Changes requested, because two tests fail.",
            "**OUTCOME: aproved**",
        ] {
            let response = format!("{before}\n{verdict}\n{after}\nThat is everything I did.");
            let state = built(&result(&response), StateBudget::DEFAULT);
            let kept = state["final_lines"].as_str().unwrap();
            assert!(kept.contains(verdict), "{verdict}: {kept}");
            assert!(kept.ends_with("That is everything I did."), "{kept}");
        }
        let report = format!("{before}The verdict is approved. {after}");
        let state = built(
            &StepResult {
                report: Some(report),
                ..result("Done.")
            },
            StateBudget::DEFAULT,
        );
        let kept = state["report"].as_str().unwrap();
        assert!(kept.contains("The verdict is approved."), "{kept}");
        assert!(kept.starts_with("I opened the next file"), "{kept}");
    }

    #[test]
    fn a_result_that_names_its_labels_in_passing_still_ends_on_its_verdict() {
        let passing = "In the last round this was not approved and changes were requested. ";
        let response = format!(
            "{}I went through every file again.\nSo it is not ready to merge yet.",
            passing.repeat(60)
        );
        let state = built(&result(&response), StateBudget::DEFAULT);
        let kept = state["final_lines"].as_str().unwrap();
        assert!(
            kept.ends_with("I went through every file again.\nSo it is not ready to merge yet."),
            "{kept}"
        );
        assert!(kept.contains("changes were requested"), "{kept}");
    }

    #[test]
    fn a_huge_report_and_huge_outputs_leave_room_for_each_other_and_the_closing_lines() {
        let step = StepResult {
            outputs: BTreeMap::from([
                (
                    "summary".into(),
                    "The importer fails on empty files. ".repeat(300),
                ),
                ("ticket".into(), "OPS-42".into()),
            ]),
            report: Some("| file | result |\n".repeat(2_000)),
            ..result(&format!(
                "{}\nIn short: not ready.",
                "A line of working notes.\n".repeat(2_000)
            ))
        };
        let state = built(&step, StateBudget::DEFAULT);
        assert_eq!(state["outputs"]["ticket"], "OPS-42");
        let part = |field: &str| state[field].as_str().unwrap().chars().count();
        let summary = state["outputs"]["summary"]
            .as_str()
            .unwrap()
            .chars()
            .count();
        for kept in [summary, part("report"), part("final_lines")] {
            assert!((400..=495).contains(&kept), "an even share each: {kept}");
        }
        assert!(state["final_lines"]
            .as_str()
            .unwrap()
            .ends_with("In short: not ready."));
    }

    #[test]
    fn a_garbled_outcome_line_and_an_unclosed_outputs_block_are_read_as_the_parser_reads_them() {
        let response = "Done reviewing.\nOUTPUTS:\nverdict: fine\n\nOUTCOME: aproved";
        assert_eq!(
            built(&result(response), StateBudget::DEFAULT)["final_lines"],
            "Done reviewing.\nOUTCOME: aproved"
        );
        let unclosed_report = StepResult {
            report: Some("still writing".into()),
            ..result("REPORT:\nstill writing")
        };
        assert_eq!(
            built(&unclosed_report, StateBudget::DEFAULT)["final_lines"],
            "REPORT:\nstill writing"
        );
    }

    #[test]
    fn multibyte_results_stay_within_the_budget() {
        let step = StepResult {
            outputs: BTreeMap::from([("résumé".into(), "é".repeat(3_000))]),
            report: Some("報告。 ".repeat(800)),
            ..result(&"結論は承認です。 ".repeat(800))
        };
        let state = built(&step, StateBudget::chars(300));
        assert!(state["final_lines"]
            .as_str()
            .unwrap()
            .ends_with("結論は承認です。"));
    }
}

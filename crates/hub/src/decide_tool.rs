//! `decide.ask` — put one or more questions about a state to the install's
//! decision model (Kev or Jev) and get calibrated answers back: no model
//! turn, a few hundred milliseconds, and probabilities an agent can act on
//! (triage a batch of tickets, score leads, gate a list of candidates).
//!
//! Registered only when `[decision]` is on and `[decision.uses] tool` is.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::workflow::definition::{DecideKind, DecideSpec, NOUL_OPTIONS};

use crate::credentials::Caller;
use crate::decide::{answer_fields, build_question, decisions, explain, kind_name, Decisions};
use crate::vault::Vault;

pub const NAME: &str = "decide.ask";
const MAX_QUESTIONS: usize = 32;

pub struct DecideTool {
    decisions: Arc<Decisions>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

/// The tool for one agent, when the install has a decision backend and the
/// tool use is on. Each call is asked with the turn's user's keys.
pub fn decide_tool(
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> Option<Arc<dyn Tool>> {
    decisions()
        .filter(|d| d.tool_enabled())
        .map(|decisions| Arc::new(DecideTool::new(decisions, agent_vault, global_vault)) as _)
}

impl DecideTool {
    pub(crate) fn new(
        decisions: Arc<Decisions>,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            decisions,
            agent_vault,
            global_vault,
        }
    }
}

#[derive(Deserialize)]
struct AskInput {
    state: Value,
    questions: Vec<AskQuestion>,
}

#[derive(Deserialize)]
struct AskQuestion {
    #[serde(default)]
    id: Option<String>,
    question: String,
    #[serde(default)]
    kind: DecideKind,
    #[serde(default)]
    options: BTreeMap<String, String>,
    #[serde(default)]
    levels: Vec<String>,
}

impl AskQuestion {
    fn spec(&self) -> Result<DecideSpec, String> {
        match self.kind {
            DecideKind::Choice if self.options.len() < 2 => {
                return Err(format!(
                    "question {:?} is a choice and needs at least two options",
                    self.question
                ));
            }
            DecideKind::Noul
                if self
                    .options
                    .keys()
                    .any(|k| !NOUL_OPTIONS.contains(&k.as_str())) =>
            {
                return Err(format!(
                    "question {:?} is yes/no; describe only 'yes' and 'no' in options",
                    self.question
                ));
            }
            DecideKind::Score if self.levels.len() < 2 || self.levels.len() > 10 => {
                return Err(format!(
                    "question {:?} is a score and needs two to ten levels, lowest first",
                    self.question
                ));
            }
            _ => {}
        }
        Ok(DecideSpec {
            question: self.question.clone(),
            kind: self.kind,
            options: self.options.clone(),
            levels: self.levels.clone(),
            min_probability: None,
        })
    }
}

#[async_trait]
impl Tool for DecideTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Ask the decision model one or more questions about a state and get calibrated answers \
         in well under a second, with no reasoning turn. Use it for judgement calls over data you \
         already have: classify (kind 'choice' with named options), yes/no checks (kind 'noul'), \
         or grading on an ordered scale (kind 'score' with levels, lowest first). Each answer \
         carries a label, its probability and every option's probability; treat a probability \
         below ~0.7 as undecided. Put everything the question needs into `state`, and ask about \
         ONE record per call (several questions about it are fine): a state holding a list of \
         records makes the answers unreliable. Keep the state short: only the facts the \
         questions need, about 1,500 characters of text. Pasted logs, stack traces, code blocks \
         and long quotes are collapsed to a marker such as `[42 lines of log]`, and longer text \
         is cut down to its opening, so summarise first rather than paste. For yes/no, describe \
         what yes and no mean in `options`; the model judges by those."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": {
                    "description": "The facts the questions are asked of: a short JSON object (preferred) or text, about 1,500 characters of text at most."
                },
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Key for this answer in the result (default: q1, q2, …)." },
                            "question": { "type": "string", "description": "Phrased so the state alone answers it." },
                            "kind": { "type": "string", "enum": ["choice", "noul", "score"], "description": "choice (default): one of the options; noul: yes or no; score: a level on an ordered scale." },
                            "options": { "type": "object", "additionalProperties": { "type": "string" }, "description": "choice: option name → what it means (at least two). noul: what 'yes' and 'no' mean." },
                            "levels": { "type": "array", "items": { "type": "string" }, "description": "score only: two to ten level names, lowest first." }
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["state", "questions"]
        })
    }

    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let duration = || start.elapsed().as_millis() as u64;
        let input: AskInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("invalid input: {e}"),
                    duration(),
                ))
            }
        };
        if input.questions.is_empty() || input.questions.len() > MAX_QUESTIONS {
            return Ok(ToolResult::failure(
                format!("ask between 1 and {MAX_QUESTIONS} questions per call"),
                duration(),
            ));
        }
        let specs: Vec<(String, DecideSpec)> = match input
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                q.spec()
                    .map(|spec| (q.id.clone().unwrap_or_else(|| format!("q{}", i + 1)), spec))
            })
            .collect()
        {
            Ok(specs) => specs,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };
        let questions: BTreeMap<_, _> = specs
            .iter()
            .map(|(id, spec)| (id.clone(), build_question(spec, &[])))
            .collect();
        if questions.len() != specs.len() {
            return Ok(ToolResult::failure(
                "question ids must be distinct",
                duration(),
            ));
        }
        let caller = Caller::in_scope(self.agent_vault.clone(), self.global_vault.clone());
        let decision = match self
            .decisions
            .ask_tool(&caller, input.state, questions)
            .await
        {
            Ok(Some(d)) => d,
            Ok(None) => {
                return Ok(ToolResult::failure(
                    "the state is empty: put the facts the questions are asked of into `state`",
                    duration(),
                ))
            }
            Err(e) if e.is_overloaded() => {
                return Ok(ToolResult::failure(
                    format!("{}. Try again shortly, or decide without it.", explain(&e)),
                    duration(),
                ))
            }
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("decision model unavailable: {}", explain(&e)),
                    duration(),
                ))
            }
        };
        let answers: serde_json::Map<String, Value> = specs
            .iter()
            .filter_map(|(id, spec)| {
                decision.response.answers.get(id).map(|answer| {
                    let mut fields = answer_fields(answer, &spec.levels);
                    fields.insert("kind".into(), json!(kind_name(spec.kind)));
                    (id.clone(), Value::Object(fields))
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({
                "backend": decision.backend,
                "model": decision.response.model,
                "fell_back": decision.fell_back,
                "answers": answers,
            }),
            duration(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_becomes_a_spec_only_with_a_usable_answer_shape() {
        let q = |kind, options: &[(&str, &str)], levels: &[&str]| AskQuestion {
            id: None,
            question: "Q".into(),
            kind,
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            levels: levels.iter().map(|l| l.to_string()).collect(),
        };
        assert!(q(DecideKind::Choice, &[("a", ""), ("b", "")], &[])
            .spec()
            .is_ok());
        assert!(q(DecideKind::Choice, &[("a", "")], &[])
            .spec()
            .unwrap_err()
            .contains("two options"));
        assert!(q(DecideKind::Noul, &[], &[]).spec().is_ok());
        assert!(
            q(DecideKind::Noul, &[("yes", "blocked"), ("no", "fine")], &[])
                .spec()
                .is_ok()
        );
        assert!(q(DecideKind::Noul, &[("maybe", "")], &[])
            .spec()
            .unwrap_err()
            .contains("only 'yes' and 'no'"));
        assert!(q(DecideKind::Score, &[], &["low", "high"]).spec().is_ok());
        assert!(q(DecideKind::Score, &[], &["only"])
            .spec()
            .unwrap_err()
            .contains("two to ten"));
    }

    #[tokio::test]
    async fn several_questions_about_a_long_state_are_one_request_and_an_empty_state_is_refused() {
        use crate::decide::testing::{config_for, recording_backend};

        let (url, seen) = recording_backend("bug", std::time::Duration::ZERO).await;
        let tool = DecideTool::new(
            Arc::new(crate::decide::build(&config_for(url)).unwrap().unwrap()),
            None,
            None,
        );
        let questions = json!([
            { "id": "kind", "question": "What is it?", "options": { "bug": "broken", "feature": "new" } },
            { "id": "urgent", "question": "Urgent?", "kind": "noul" }
        ]);
        let state = json!({
            "title": "Checkout fails",
            "body": format!("Customers cannot pay. {}", "It has happened before. ".repeat(300)),
        });
        let result = tool
            .execute(json!({ "state": state, "questions": questions }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(seen.lock().len(), 1);
        let sent = seen.lock()[0].clone();
        assert_eq!(sent["questions"].as_object().unwrap().len(), 2);
        assert_eq!(sent["state"]["title"], "Checkout fails");
        let body = sent["state"]["body"].as_str().unwrap();
        assert!(body.starts_with("Customers cannot pay.") && body.chars().count() < 1_500);

        for empty in [json!({}), json!(""), json!(null)] {
            let refused = tool
                .execute(json!({ "state": empty, "questions": questions }))
                .await
                .unwrap();
            assert!(!refused.success);
            assert!(refused
                .error
                .is_some_and(|error| error.starts_with("the state is empty")));
        }
        assert_eq!(seen.lock().len(), 1, "an empty state is never sent");
    }

    #[test]
    fn input_defaults_ids_and_kind() {
        let input: AskInput = serde_json::from_value(json!({
            "state": {"ticket": "OPS-1"},
            "questions": [
                { "question": "Bug?", "kind": "noul" },
                { "id": "sev", "question": "How bad?", "kind": "score", "levels": ["low", "high"] }
            ]
        }))
        .unwrap();
        assert_eq!(input.questions.len(), 2);
        assert!(input.questions[0].id.is_none());
        assert_eq!(input.questions[1].id.as_deref(), Some("sev"));
        assert_eq!(input.questions[0].kind, DecideKind::Noul);
    }
}

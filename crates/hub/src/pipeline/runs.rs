//! The workflow runs an item's record names, and which of them a pipeline
//! may join: what the cost and time views of an item read, and what
//! `pipeline.runs` shows a skill.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_orchestrator::workflow::instance::{
    StepRecord, AGENT_ENGINE, DECISION_ENGINE, REPORT_ENGINE, TOOL_ENGINE, WORKFLOW_ENGINE,
};

use super::clip;
pub(crate) use crate::hub_core::pipelines::text;
pub use crate::hub_core::pipelines::Run;
use crate::hub_core::pipelines::Scope;

/// The longest error text a run view carries.
pub const ERROR_MAX: usize = 300;

/// The runs `rows` hold, in order. A run executes as the agent it recorded
/// at start; one that recorded none, as the agent its workflow declares in
/// `declared` (by workflow key).
pub fn runs_as_declared(
    rows: Vec<WorkflowInstanceRow>,
    declared: &HashMap<String, String>,
) -> impl Iterator<Item = Run> + '_ {
    rows.into_iter().map(Run::from).map(|run| {
        let agent_id = run
            .agent_id
            .or_else(|| declared.get(&run.workflow_id).cloned());
        Run { agent_id, ..run }
    })
}

/// [`runs_as_declared`], by id.
pub fn runs_by_id(
    rows: Vec<WorkflowInstanceRow>,
    declared: &HashMap<String, String>,
) -> HashMap<String, Run> {
    runs_as_declared(rows, declared)
        .map(|run| (run.id.clone(), run))
        .collect()
}

/// The agent whose runs an agent's pipeline joins; none on any other.
pub fn agent_of(scope: &Scope) -> Option<&str> {
    match scope {
        Scope::Agent(agent) => Some(agent.as_str()),
        _ => None,
    }
}

/// Whether a pipeline may join `run`: any run on a global or user pipeline,
/// only `agent`'s own on an agent's.
pub fn joinable(run: &Run, agent: Option<&str>) -> bool {
    agent.is_none_or(|agent| run.agent_id.as_deref() == Some(agent))
}

/// How much of a run's step history a view carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepDetail {
    #[default]
    None,
    /// The steps a coding engine served: not tool, report or agent-loop steps.
    Coding,
    All,
}

impl StepDetail {
    fn keeps(self, step: &StepRecord) -> bool {
        match self {
            StepDetail::None => false,
            StepDetail::Coding => step.provenance.engine.as_deref().is_some_and(|engine| {
                ![
                    TOOL_ENGINE,
                    REPORT_ENGINE,
                    AGENT_ENGINE,
                    DECISION_ENGINE,
                    WORKFLOW_ENGINE,
                ]
                .contains(&engine)
            }),
            StepDetail::All => true,
        }
    }
}

/// A run as `pipeline.runs` shows it: never a step's response or outputs.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub run: String,
    pub workflow_id: String,
    pub status: String,
    pub current_step: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub agent_id: Option<String>,
    pub lease: Lease,
    pub failure_reason: Option<String>,
    pub steps: Vec<StepView>,
}

/// A run's execution lease, `live` until it runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub expires_at: Option<DateTime<Utc>>,
    pub live: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepView {
    pub step_id: String,
    pub outcome: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub failed: bool,
    pub engine: Option<String>,
    pub model: Option<String>,
    pub handed_over: Vec<HandedOver>,
}

/// An engine a step's fallback chain moved on from, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandedOver {
    pub engine: String,
    pub model: Option<String>,
    pub error: Option<String>,
}

/// Error text fit for a skill to read: secrets redacted, at most
/// [`ERROR_MAX`] characters.
fn bounded(text: &str) -> String {
    clip(&crate::redact::redact_secrets(text), ERROR_MAX)
}

/// `run` as `pipeline.runs` shows it at `now`, with the steps `detail` keeps.
pub fn run_view(run: Run, detail: StepDetail, now: DateTime<Utc>) -> RunView {
    RunView {
        lease: Lease {
            expires_at: run.lease_expires_at,
            live: run.lease_expires_at.is_some_and(|at| at > now),
        },
        failure_reason: run.failure_reason.as_deref().map(bounded),
        steps: run
            .steps
            .into_iter()
            .filter(|step| detail.keeps(step))
            .map(|step| StepView {
                handed_over: step
                    .provenance
                    .handed_over
                    .into_iter()
                    .map(|attempt| HandedOver {
                        error: attempt.error.as_deref().map(bounded),
                        engine: attempt.engine,
                        model: attempt.model,
                    })
                    .collect(),
                engine: step.provenance.engine,
                model: step.provenance.model,
                step_id: step.step_id,
                outcome: step.outcome,
                started_at: step.started_at,
                completed_at: step.completed_at,
                failed: step.failed,
            })
            .collect(),
        run: run.id,
        workflow_id: run.workflow_id,
        status: run.status,
        current_step: run.current_step,
        created_at: run.created_at,
        updated_at: run.updated_at,
        agent_id: run.agent_id,
    }
}

/// Where an item's record names a run: the stage it was recorded in and when.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Sighting<'a> {
    pub run: &'a str,
    pub stage: &'a str,
    pub at: &'a str,
}

/// Each run a record names, once, at its first sighting, earliest first. The
/// history is read first; a stage block then adds a run the capped history
/// no longer holds.
pub(crate) fn sightings(body: &Value) -> Vec<Sighting<'_>> {
    let history = body
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|event| {
            Some(Sighting {
                run: text(event, "run").filter(|run| !run.is_empty())?,
                stage: text(event, "stage").unwrap_or_default(),
                at: text(event, "at").unwrap_or_default(),
            })
        });
    let blocks = body
        .get("stages")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(stage, block)| {
            Some(Sighting {
                run: text(block, "run").filter(|run| !run.is_empty())?,
                stage: stage.as_str(),
                at: text(block, "at").unwrap_or_default(),
            })
        });
    let mut seen = HashSet::new();
    let mut found: Vec<Sighting<'_>> = history
        .chain(blocks)
        .filter(|sighting| seen.insert(sighting.run))
        .collect();
    found.sort_by_key(|sighting| sighting.at);
    found
}

/// The runs an item's record names, each once, earliest first.
pub fn runs_named(body: &Value) -> Vec<String> {
    sightings(body)
        .into_iter()
        .map(|sighting| sighting.run.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;
    use workforce_orchestrator::workflow::instance::{EngineAttempt, StepProvenance};

    use super::*;
    use crate::pipeline::fixtures::{record, row, run, T0, T1, T2, T3};

    #[test]
    fn sightings_keep_each_runs_first_stage_and_skip_missing_runs() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("r1")),
                (T1, "implement", "returned", None),
                (T2, "review", "claimed", Some("r2")),
                (T3, "verify", "rung", Some("r1")),
                ("2026-09-12T12:00:00Z", "verify", "rung", Some("")),
            ],
        );
        let seen: Vec<(&str, &str)> = sightings(&item.body)
            .iter()
            .map(|sighting| (sighting.run, sighting.stage))
            .collect();
        assert_eq!(seen, [("r1", "implement"), ("r2", "review")]);
        assert_eq!(runs_named(&item.body), ["r1", "r2"]);
        assert!(runs_named(&json!({})).is_empty());
        assert!(runs_named(&json!({"history": "none", "stages": {"x": {"run": 7}}})).is_empty());
    }

    #[test]
    fn a_run_that_recorded_no_agent_runs_as_the_agent_its_workflow_declares() {
        let declared = HashMap::from([("order-intake".to_string(), "example".to_string())]);
        let runs = runs_by_id(
            vec![
                row("legacy", "order-intake", None, Vec::new()),
                row("stamped", "order-intake", Some("quill"), Vec::new()),
                row("undeclared", "hubspot-ticket-triage", None, Vec::new()),
            ],
            &declared,
        );
        let agents: HashMap<&str, (&str, Option<&str>)> = runs
            .iter()
            .map(|(id, run)| (id.as_str(), (run.id.as_str(), run.agent_id.as_deref())))
            .collect();
        assert_eq!(
            agents,
            HashMap::from([
                ("legacy", ("legacy", Some("example"))),
                ("stamped", ("stamped", Some("quill"))),
                ("undeclared", ("undeclared", None)),
            ])
        );
    }

    #[test]
    fn runs_as_declared_keeps_the_rows_order() {
        let ids: Vec<String> = runs_as_declared(
            vec![
                row("newest", "order-intake", None, Vec::new()),
                row("oldest", "order-review", None, Vec::new()),
            ],
            &HashMap::new(),
        )
        .map(|run| run.id)
        .collect();
        assert_eq!(ids, ["newest", "oldest"]);
    }

    #[test]
    fn an_agents_pipeline_joins_only_that_agents_runs_and_a_global_one_joins_every_run() {
        let runs = runs_by_id(
            vec![
                row("mine", "order-intake", Some("example"), Vec::new()),
                row("quills", "order-intake", Some("quill"), Vec::new()),
                row("nobodys", "order-intake", None, Vec::new()),
            ],
            &HashMap::new(),
        );
        let joined = |agent| {
            let mut ids: Vec<&str> = runs
                .values()
                .filter(|run| joinable(run, agent))
                .map(|run| run.id.as_str())
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(joined(Some("example")), ["mine"]);
        assert_eq!(joined(None), ["mine", "nobodys", "quills"]);
        assert_eq!(agent_of(&Scope::Agent("example".into())), Some("example"));
        assert_eq!(agent_of(&Scope::Global), None);
        assert_eq!(agent_of(&Scope::User("alice".into())), None);
    }

    fn served_by(engine: Option<&str>) -> StepRecord {
        StepRecord {
            step_id: engine.unwrap_or("legacy").into(),
            outcome: Some("done".into()),
            agent_response: "the whole transcript".into(),
            outputs: Some(json!({"secret_looking": "value"})),
            report: None,
            started_at: T0.parse().unwrap(),
            completed_at: T1.parse().unwrap(),
            rounds: 3,
            cost_usd: 1.0,
            budget_capped: false,
            failed: false,
            correction: None,
            provenance: StepProvenance {
                engine: engine.map(str::to_string),
                ..Default::default()
            },
        }
    }

    #[test]
    fn a_row_carries_its_current_step_lease_and_failure_reason_into_the_run() {
        let failed = WorkflowInstanceRow {
            current_step_id: Some("implement".into()),
            lease_expires_at: Some("2026-09-15T05:00:00Z".into()),
            variables: json!({"_failure_reason": "requires item-picker >=0.10.0; installed 0.9.0"})
                .to_string(),
            ..row("r1", "order-intake", Some("example"), Vec::new())
        };
        let run = Run::from(failed);
        assert_eq!(run.current_step.as_deref(), Some("implement"));
        assert_eq!(
            run.lease_expires_at,
            Some("2026-09-15T05:00:00Z".parse().unwrap())
        );
        assert_eq!(
            run.failure_reason.as_deref(),
            Some("requires item-picker >=0.10.0; installed 0.9.0")
        );
        let unparsed = Run::from(WorkflowInstanceRow {
            lease_expires_at: Some("2026-09-15 05:00:00".into()),
            ..row("r2", "order-intake", None, Vec::new())
        });
        assert_eq!(
            (unparsed.lease_expires_at, unparsed.failure_reason),
            (None, None)
        );
    }

    #[test]
    fn steps_none_coding_and_all_keep_what_they_name() {
        let steps = [
            Some(TOOL_ENGINE),
            Some(REPORT_ENGINE),
            Some(AGENT_ENGINE),
            Some("claude-code"),
            Some("codex"),
            None,
        ]
        .map(served_by)
        .to_vec();
        let kept = |detail| -> Vec<String> {
            run_view(
                Run {
                    steps: steps.clone(),
                    ..run("r1", T0, None, Vec::new())
                },
                detail,
                T2.parse().unwrap(),
            )
            .steps
            .into_iter()
            .map(|step| step.step_id)
            .collect()
        };
        assert!(kept(StepDetail::None).is_empty());
        assert_eq!(kept(StepDetail::Coding), ["claude-code", "codex"]);
        assert_eq!(kept(StepDetail::All).len(), 6);
        assert_eq!(
            serde_json::from_value::<StepDetail>(json!("coding")).unwrap(),
            StepDetail::Coding
        );
        assert!(serde_json::from_value::<StepDetail>(json!("heavy")).is_err());
    }

    #[test]
    fn a_run_view_keeps_hand_over_errors_redacted_and_bounded_and_never_a_response() {
        let pat = concat!("ghp_EX", "AMPLE00000000000000000000000000000");
        let step = StepRecord {
            provenance: StepProvenance {
                engine: Some("claude-code:bedrock".into()),
                model: Some("opus".into()),
                handed_over: vec![
                    EngineAttempt {
                        engine: "codex".into(),
                        model: Some("sol".into()),
                        error: Some(format!(
                            "output consumer dropped with token {pat} {}",
                            "x".repeat(400)
                        )),
                        ..Default::default()
                    },
                    EngineAttempt {
                        engine: "claude-code".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..served_by(Some("claude-code"))
        };
        let view = run_view(
            Run {
                failure_reason: Some(format!("halted: {pat}")),
                steps: vec![step],
                ..run("r1", T0, None, Vec::new())
            },
            StepDetail::All,
            T2.parse().unwrap(),
        );
        let attempts = &view.steps[0].handed_over;
        let codex = attempts[0].error.as_deref().unwrap();
        assert!(codex.starts_with("output consumer dropped with token [REDACTED GITHUB_PAT] x"));
        assert_eq!(codex.chars().count(), ERROR_MAX);
        assert!(codex.ends_with('…'));
        assert!(!codex.contains(pat));
        assert_eq!(
            (attempts[1].engine.as_str(), &attempts[1].error),
            ("claude-code", &None)
        );
        assert_eq!(
            view.failure_reason.as_deref(),
            Some("halted: [REDACTED GITHUB_PAT]")
        );
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("the whole transcript"), "{json}");
        assert!(!json.contains("secret_looking"), "{json}");
    }

    #[test]
    fn a_lease_is_live_only_until_it_runs_out() {
        let now: DateTime<Utc> = T2.parse().unwrap();
        let lease = |expires_at| {
            run_view(
                Run {
                    lease_expires_at: expires_at,
                    ..run("r1", T0, None, Vec::new())
                },
                StepDetail::None,
                now,
            )
            .lease
        };
        assert!(lease(Some(now + Duration::seconds(1))).live);
        assert!(!lease(Some(now)).live);
        assert!(!lease(Some(now - Duration::minutes(12))).live);
        assert_eq!(
            lease(None),
            Lease {
                expires_at: None,
                live: false
            }
        );
    }
}

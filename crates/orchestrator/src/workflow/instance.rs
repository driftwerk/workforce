//! Workflow instance — runtime state for a running workflow.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A running instance of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInstance {
    pub id: String,
    pub workflow_id: String,
    pub status: InstanceStatus,
    pub current_step_id: Option<String>,
    pub step_history: Vec<StepRecord>,
    pub variables: serde_json::Value,
    pub trigger: TriggerInfo,
    /// The agent the run executes as: its workflow's declared agent, stamped at
    /// start. `None` when the definition names none, and on runs started before
    /// runs recorded it.
    pub agent_id: Option<String>,
    /// If this instance was spawned by another workflow step, the parent's instance ID.
    pub parent_instance_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Status of a workflow instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    /// Admitted by a trigger that found the workflow at `max_concurrent`:
    /// fully built and saved, but holding for a slot. Neither terminal nor
    /// active — it counts against no capacity, has no executor, and is
    /// promoted to `Running` (oldest first) when a sibling finishes.
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    /// Early exit via handoff.
    Exited,
    /// Explicitly cancelled/rejected by a user or agent.
    Cancelled,
}

impl InstanceStatus {
    /// Whether a run in this status is over.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Exited | Self::Cancelled
        )
    }
}

impl std::fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "queued"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Exited => write!(f, "exited"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::str::FromStr for InstanceStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "exited" => Ok(Self::Exited),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown instance status: '{other}'")),
        }
    }
}

/// Record of a step run: a completed step, or with `failed` set, a run of it
/// that stopped the instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_id: String,
    pub outcome: Option<String>,
    pub agent_response: String,
    /// Structured outputs extracted from the agent response (if step declared outputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Value>,
    /// Concise human-facing report parsed from the agent response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub rounds: u32,
    /// What the whole step spent, fallback attempts it handed over from included.
    pub cost_usd: f64,
    /// Whether this step was force-completed due to hitting its round budget.
    #[serde(default)]
    pub budget_capped: bool,
    /// A run of the step that did not complete it: its engine ran and spent,
    /// then the step failed. Routing, resume and prompt context skip it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    /// Why the engine sent this run of the step back for another: its
    /// package did not read, or a verifier rejected it. Such a run is a
    /// failed attempt, so it is `failed` too: a hub from before corrections
    /// existed skips it as it skips any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<Correction>,
    /// What actually ran the step. Absent on records written before it was
    /// captured, so every field is optional and the block flattens away.
    #[serde(default, flatten)]
    pub provenance: StepProvenance,
}

/// Why a run of a step was sent back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionCause {
    /// Its package was missing fields or held values that do not fit.
    Malformed,
    /// A verifier rejected what it produced.
    Rejected,
}

/// What the engine told a step when it sent a run of it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correction {
    pub cause: CorrectionCause,
    /// What was wrong, as the step's next attempt is told it.
    pub detail: String,
}

/// One engine run inside a step that did not serve it: a fallback chain
/// moved on to the next entry after it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineAttempt {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// Why the chain moved on from this attempt: the first line of its error,
    /// or the last line of the failed round's own account of itself. Absent on
    /// records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The engine a tool step's provenance names.
pub const TOOL_ENGINE: &str = "wasm";
/// The engine a report step's provenance names.
pub const REPORT_ENGINE: &str = "report";
/// The engine a decision step's provenance names.
pub const DECISION_ENGINE: &str = "decision";
/// The engine a step that ran another workflow names.
pub const WORKFLOW_ENGINE: &str = "workflow";
/// The engine a step the hub's own agent loop ran names.
pub const AGENT_ENGINE: &str = "agent";

/// The engine-owned variable holding the steering terms a run started under:
/// the hub's to read when someone asks to steer the run, never a step's.
pub const STEERING_KEY: &str = "_steering";

/// Who chose the route a branching step took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeBy {
    /// The step's own `OUTCOME:` line named a declared branch.
    OutcomeLine,
    /// No declared outcome: the step took its default route.
    Default,
    /// The iteration cap forced the outcome.
    IterationCap,
    /// The decision model: for a step that ended with no route of its own,
    /// or one it settled before its turn ([`DecidedFirst::settled`]).
    DecisionModel,
    /// Whoever resumed the run gave it; the run's [`Intervention`] says who.
    Resume,
}

/// Who acted on a run from outside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    Person {
        id: String,
    },
    /// An agent acting with nobody behind it.
    Agent {
        id: String,
    },
    /// The hub itself: a recovery, a client with no identity.
    System,
}

impl Actor {
    /// `person` when there is one, else the agent, else the hub.
    pub fn of(person: Option<String>, agent: Option<String>) -> Self {
        let named = |id: Option<String>| id.filter(|id| !id.is_empty());
        match (named(person), named(agent)) {
            (Some(id), _) => Actor::Person { id },
            (None, Some(id)) => Actor::Agent { id },
            (None, None) => Actor::System,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionKind {
    Resume,
    Goto,
    Skip,
    Retry,
    Pause,
    Abort,
}

/// The route the run was on when someone stepped in: its last completed
/// step, and what that step had chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chosen {
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<OutcomeBy>,
}

/// Someone moving a run by hand: resuming it (with an outcome or without),
/// sending it to a step, skipping one, retrying a failed one, pausing it or
/// stopping it. It is the
/// run's history, kept by whoever records it and never among the run's
/// variables: nothing a step is shown or a template renders can hold one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intervention {
    pub kind: InterventionKind,
    pub actor: Actor,
    pub at: DateTime<Utc>,
    /// The step the run stood at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_step: Option<String>,
    /// The step it stands at afterwards; none when the run ended there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_step: Option<String>,
    /// The outcome a resume gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Whether the step went on with `outcome`; absent when the resume gave
    /// none, and on interventions kept before it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<Chosen>,
}

/// Where a step's answer came from: the engine and model that produced it,
/// what it consumed, and which revision of the definition it ran under. A
/// grade of the step's output is only comparable to another grade when these
/// match, and a fallback that quietly moved a step onto another model is
/// invisible without them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepProvenance {
    /// [`TOOL_ENGINE`] for a tool step, [`REPORT_ENGINE`] for a report step,
    /// otherwise the coding engine or agent runtime that served the round (a
    /// fallback chain names the member that actually ran, not the one asked
    /// for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The model the serving round ran on, when the engine reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When the step's branch was chosen by a decision model rather than by
    /// the agent's own `OUTCOME:` line: the backend and probability, e.g.
    /// `kev p=0.86`, or `kev (shadow) p=0.86` when it was only logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_decided_by: Option<String>,
    /// Who chose the route a branching step took. Absent on a step with no
    /// branches, and on records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_by: Option<OutcomeBy>,
    /// What the decision model answered when it was asked before the
    /// step's turn and its answer reached that option's floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_first: Option<DecidedFirst>,
    /// Fresh (non-cached) input tokens across the whole step, handed-over
    /// attempts included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens across the whole step, handed-over attempts included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// BLAKE3 of the definition source the step ran under; `None` for a
    /// disk-seeded definition with no stored revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
    /// The earlier attempts a fallback chain moved on from, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handed_over: Vec<EngineAttempt>,
}

/// What came of asking the decision model before a step's turn
/// (`decide_first`), when its answer reached that option's floor. A step it
/// settled is otherwise recorded as any pick of the decision model is
/// (`outcome_by = "decision_model"`), so a build that predates this field
/// reads the record, and drops only this field when it saves it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecidedFirst {
    /// The answer settled the step: no turn ran, and it routed on the pick.
    pub settled: bool,
    /// The answer: `kev ignore p=0.93`, and for a turn that ran all the
    /// same why it ran and what the agent chose (`kev (shadow) ignore
    /// p=0.93, agent agreed`).
    pub answer: String,
}

/// Keys a later template reads a step's provenance under, beside the step's
/// declared outputs: `{{steps.<id>._engine}}` and `{{steps.<id>._model}}`.
pub const PROVENANCE_OUTPUTS: [&str; 2] = ["_engine", "_model"];

impl StepProvenance {
    /// Whether the decision model settled the step before its turn.
    pub fn settled_first(&self) -> bool {
        self.decided_first
            .as_ref()
            .is_some_and(|first| first.settled)
    }

    /// The step's outputs as later templates see them: the declared outputs
    /// plus `_engine` and `_model` for whichever of the two this carries.
    pub fn exposed(&self, outputs: Option<&serde_json::Value>) -> Option<serde_json::Value> {
        let stamps: serde_json::Map<String, serde_json::Value> = PROVENANCE_OUTPUTS
            .into_iter()
            .zip([&self.engine, &self.model])
            .filter_map(|(key, value)| Some((key.to_string(), value.as_deref()?.into())))
            .collect();
        if stamps.is_empty() {
            return outputs.cloned();
        }
        match outputs {
            None => Some(serde_json::Value::Object(stamps)),
            Some(serde_json::Value::Object(declared)) => Some(serde_json::Value::Object(
                declared.clone().into_iter().chain(stamps).collect(),
            )),
            Some(other) => Some(other.clone()),
        }
    }
}

/// How a workflow was triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub source: TriggerSource,
    pub payload: serde_json::Value,
}

/// The source that started the workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TriggerSource {
    Cli,
    Webhook {
        source: String,
        event_type: String,
    },
    Handoff {
        handoff_id: String,
    },
    Api,
    Http {
        id: String,
    },
    Monitor {
        monitor_id: String,
    },
    /// Started by a `workflow = "…"` step of another run.
    Workflow {
        instance_id: String,
        step_id: String,
    },
}

/// The first prior step output called `name`, if any step produced one.
pub fn step_output<'a>(variables: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    variables
        .get("steps")
        .and_then(serde_json::Value::as_object)
        .and_then(|steps| {
            steps
                .values()
                .filter_map(serde_json::Value::as_object)
                .find_map(|outputs| outputs.get(name).and_then(serde_json::Value::as_str))
        })
}

/// The ticket every model turn's cost is booked to: the trigger's own
/// `ticket_id` (a Linear or Jira webhook), else the one a prior step claimed
/// and named as its `ticket_id` output (a scheduled picker).
pub fn booked_ticket(variables: &serde_json::Value) -> Option<&str> {
    variables
        .get("ticket_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| step_output(variables, "ticket_id"))
}

/// `variables` with `steps.<step_id>` set to `outputs`: the view a step's own
/// report template renders against, and the merge `record_step_with_outputs`
/// performs. `None` outputs leave the variables untouched.
pub fn with_step_outputs(
    variables: &serde_json::Value,
    step_id: &str,
    outputs: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut merged = variables.clone();
    if let (Some(outputs), Some(vars)) = (outputs, merged.as_object_mut()) {
        let steps = vars.entry("steps").or_insert_with(|| serde_json::json!({}));
        if let Some(steps) = steps.as_object_mut() {
            steps.insert(step_id.to_string(), outputs.clone());
        }
    }
    merged
}

impl WorkflowInstance {
    /// Create a new workflow instance.
    pub fn new(
        workflow_id: &str,
        trigger: TriggerInfo,
        first_step_id: &str,
        parent_instance_id: Option<String>,
    ) -> Self {
        let now = Utc::now();
        // Variables seed from the trigger payload minus the engine-owned `_…`
        // namespace (`_run_as`, `_channel_id`, `_thread_id`, …): those carry
        // identity and delivery routing, and only the engine may write them —
        // an external trigger body (webhook, HTTP) must not smuggle them in.
        // The verbatim payload survives on `trigger` for audit.
        let variables = match &trigger.payload {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .filter(|(key, _)| !key.starts_with('_'))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            other => other.clone(),
        };
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_id: workflow_id.to_string(),
            status: InstanceStatus::Running,
            current_step_id: Some(first_step_id.to_string()),
            step_history: Vec::new(),
            variables,
            trigger,
            agent_id: None,
            parent_instance_id,
            created_at: now,
            updated_at: now,
        }
    }

    /// Whether the instance is holding for a `max_concurrent` slot. Guards on
    /// operator actions must check this as well as [`Self::is_terminal`]: a
    /// queued run has no executor and holds no slot, so moving it to `Paused`
    /// or `Completed` would both exceed the cap and drop it out of the queue.
    pub fn is_queued(&self) -> bool {
        self.status == InstanceStatus::Queued
    }

    /// Whether the workflow is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub fn record_step(
        &mut self,
        step_id: &str,
        outcome: Option<String>,
        agent_response: &str,
        started_at: DateTime<Utc>,
        rounds: u32,
        cost_usd: f64,
    ) {
        self.record_step_with_outputs(
            step_id,
            outcome,
            agent_response,
            None,
            None,
            started_at,
            rounds,
            cost_usd,
            false,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_step_with_outputs(
        &mut self,
        step_id: &str,
        outcome: Option<String>,
        agent_response: &str,
        outputs: Option<serde_json::Value>,
        report: Option<String>,
        started_at: DateTime<Utc>,
        rounds: u32,
        cost_usd: f64,
        budget_capped: bool,
    ) {
        self.record_step_with_provenance(
            step_id,
            outcome,
            agent_response,
            outputs,
            report,
            started_at,
            rounds,
            cost_usd,
            budget_capped,
            StepProvenance::default(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_step_with_provenance(
        &mut self,
        step_id: &str,
        outcome: Option<String>,
        agent_response: &str,
        outputs: Option<serde_json::Value>,
        report: Option<String>,
        started_at: DateTime<Utc>,
        rounds: u32,
        cost_usd: f64,
        budget_capped: bool,
        provenance: StepProvenance,
    ) {
        let now = Utc::now();
        self.variables = with_step_outputs(
            &self.variables,
            step_id,
            provenance.exposed(outputs.as_ref()).as_ref(),
        );

        self.step_history.push(StepRecord {
            step_id: step_id.to_string(),
            outcome,
            agent_response: agent_response.to_string(),
            outputs,
            report,
            started_at,
            completed_at: now,
            rounds,
            cost_usd,
            budget_capped,
            failed: false,
            correction: None,
            provenance,
        });
        self.updated_at = now;
    }

    /// Record a run of `step_id` the engine is sending back because of
    /// `correction`: a failed attempt that says what its next one must put
    /// right. The variables are left alone: what it produced, kept as
    /// `package` for that next attempt to read, is not something a later step
    /// may.
    #[allow(clippy::too_many_arguments)]
    pub fn record_correction(
        &mut self,
        step_id: &str,
        response: &str,
        package: Option<serde_json::Value>,
        correction: Correction,
        started_at: DateTime<Utc>,
        rounds: u32,
        cost_usd: f64,
        budget_capped: bool,
        provenance: StepProvenance,
    ) {
        let now = Utc::now();
        self.step_history.push(StepRecord {
            step_id: step_id.to_string(),
            outcome: None,
            agent_response: response.to_string(),
            outputs: package,
            report: None,
            started_at,
            completed_at: now,
            rounds,
            cost_usd,
            budget_capped,
            failed: true,
            correction: Some(correction),
            provenance,
        });
        self.updated_at = now;
    }

    /// How many times the step now due has been sent back for `cause` since
    /// it last completed.
    pub fn corrections_of(&self, step_id: &str, cause: CorrectionCause) -> u32 {
        self.step_history
            .iter()
            .rev()
            .map_while(|r| r.correction.as_ref().filter(|_| r.step_id == step_id))
            .filter(|correction| correction.cause == cause)
            .count() as u32
    }

    /// The correction the step now due was last sent back with, and the run
    /// it was sent back from.
    pub fn pending_correction(&self, step_id: &str) -> Option<(&Correction, &StepRecord)> {
        self.step_history
            .last()
            .filter(|r| r.step_id == step_id)
            .and_then(|r| Some((r.correction.as_ref()?, r)))
    }

    /// Record a run of `step_id` that spent and then failed. The variables
    /// are left alone: the step produced nothing a later step may read.
    pub fn record_failed_attempt(
        &mut self,
        step_id: &str,
        response: &str,
        started_at: DateTime<Utc>,
        rounds: u32,
        cost_usd: f64,
        provenance: StepProvenance,
    ) {
        let now = Utc::now();
        self.step_history.push(StepRecord {
            step_id: step_id.to_string(),
            outcome: None,
            agent_response: response.to_string(),
            outputs: None,
            report: None,
            started_at,
            completed_at: now,
            rounds,
            cost_usd,
            budget_capped: false,
            failed: true,
            correction: None,
            provenance,
        });
        self.updated_at = now;
    }

    /// `actor` stepping in on the run as it stands: call before moving it.
    /// Whoever keeps it says where the run stood afterwards.
    pub fn intervention(&self, kind: InterventionKind, actor: Actor) -> Intervention {
        Intervention {
            kind,
            actor,
            at: Utc::now(),
            from_step: self.current_step_id.clone(),
            to_step: None,
            outcome: None,
            applied: None,
            chosen: self
                .step_history
                .iter()
                .rev()
                .find(|r| !r.failed)
                .map(|r| Chosen {
                    step_id: r.step_id.clone(),
                    outcome: r.outcome.clone(),
                    by: r.provenance.outcome_by,
                }),
        }
    }

    /// Set the outcome the last completed run of `step_id` routes on, and
    /// who gave it, for [`super::engine::WorkflowExecutor::complete_step`]
    /// to read.
    pub fn preset_outcome(&mut self, step_id: &str, outcome: &str, by: OutcomeBy) {
        if let Some(last) = self
            .step_history
            .last_mut()
            .filter(|r| r.step_id == step_id && !r.failed)
        {
            last.outcome = Some(outcome.to_string());
            last.provenance.outcome_by = Some(by);
        }
    }

    /// Set the outcome the decision model settled the last completed run of
    /// `step_id` on before its turn, with its `answer`: the step routes on
    /// that pick alone.
    pub fn settle_first(&mut self, step_id: &str, outcome: &str, answer: String) {
        self.preset_outcome(step_id, outcome, OutcomeBy::DecisionModel);
        if let Some(last) = self
            .step_history
            .last_mut()
            .filter(|r| r.step_id == step_id && !r.failed)
        {
            last.provenance.decided_first = Some(DecidedFirst {
                settled: true,
                answer,
            });
        }
    }

    /// Mark the last record failed when it is a completed run of `step_id`:
    /// routing, resume and prompts then skip it, and a retry runs the step
    /// afresh.
    pub fn fail_last_record(&mut self, step_id: &str) {
        if let Some(last) = self
            .step_history
            .last_mut()
            .filter(|r| r.step_id == step_id && !r.failed)
        {
            last.failed = true;
        }
    }

    /// The last record, when it is a completed run of `step_id`.
    pub fn last_record_of(&self, step_id: &str) -> Option<&StepRecord> {
        self.step_history
            .last()
            .filter(|r| r.step_id == step_id && !r.failed)
    }

    /// Total cost across every recorded step run, failed attempts included.
    pub fn total_cost(&self) -> f64 {
        self.step_history.iter().map(|s| s.cost_usd).sum()
    }

    /// Total rounds across every recorded step run, failed attempts included.
    pub fn total_rounds(&self) -> u32 {
        self.step_history.iter().map(|s| s.rounds).sum()
    }

    /// Number of completed steps.
    pub fn completed_step_count(&self) -> usize {
        self.step_history.iter().filter(|s| !s.failed).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_trigger() -> TriggerInfo {
        TriggerInfo {
            source: TriggerSource::Cli,
            payload: serde_json::json!({"ticket_id": "DW-100"}),
        }
    }

    #[test]
    fn a_recorded_step_exposes_its_engine_and_model_to_later_templates() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "review", None);
        inst.record_step_with_provenance(
            "review",
            None,
            "OUTCOME: fix",
            Some(serde_json::json!({"verdict": "fix"})),
            None,
            Utc::now(),
            1,
            0.0,
            false,
            StepProvenance {
                engine: Some("codex".into()),
                model: Some("gpt-6-astra".into()),
                ..Default::default()
            },
        );
        let review = &inst.variables["steps"]["review"];
        assert_eq!(review["verdict"], "fix");
        assert_eq!(review["_engine"], "codex");
        assert_eq!(review["_model"], "gpt-6-astra");
    }

    #[test]
    fn a_record_from_before_outcomes_were_kept_still_loads() {
        let old = serde_json::json!({
            "step_id": "review", "outcome": null, "agent_response": "OUTCOME: approved",
            "started_at": "2026-09-01T10:00:00Z", "completed_at": "2026-09-01T10:01:00Z",
            "rounds": 3, "cost_usd": 0.1, "engine": "agent", "branch_decided_by": "kev p=0.86",
        });
        let record: StepRecord = serde_json::from_value(old).unwrap();
        assert_eq!(record.outcome, None);
        assert_eq!(record.provenance.outcome_by, None);
        assert_eq!(record.provenance.engine.as_deref(), Some("agent"));
        let again = serde_json::to_value(&record).unwrap();
        assert!(again.get("outcome_by").is_none(), "absent stays absent");

        let new: StepRecord = serde_json::from_value(serde_json::json!({
            "step_id": "review", "outcome": "approved", "agent_response": "",
            "started_at": "2026-09-01T10:00:00Z", "completed_at": "2026-09-01T10:01:00Z",
            "rounds": 1, "cost_usd": 0.0, "outcome_by": "decision_model",
        }))
        .unwrap();
        assert_eq!(new.provenance.outcome_by, Some(OutcomeBy::DecisionModel));
    }

    #[test]
    fn a_step_settled_first_says_so_and_a_record_from_before_it_still_loads() {
        let before: StepRecord = serde_json::from_value(serde_json::json!({
            "step_id": "classify", "outcome": "bug", "agent_response": "OUTCOME: bug",
            "started_at": "2026-09-01T10:00:00Z", "completed_at": "2026-09-01T10:01:00Z",
            "rounds": 1, "cost_usd": 0.05, "outcome_by": "decision_model",
            "branch_decided_by": "kev p=0.86",
        }))
        .unwrap();
        assert_eq!(before.provenance.outcome_by, Some(OutcomeBy::DecisionModel));
        assert_eq!(before.provenance.decided_first, None);
        assert!(serde_json::to_value(&before)
            .unwrap()
            .get("decided_first")
            .is_none());

        let mut inst = WorkflowInstance::new("triage", test_trigger(), "classify", None);
        inst.record_step("classify", None, "settled", Utc::now(), 0, 0.0);
        inst.settle_first("classify", "ignore", "kev ignore p=0.93".into());
        let stored = serde_json::to_value(&inst.step_history[0]).unwrap();
        assert_eq!(
            stored["outcome_by"], "decision_model",
            "a value every build reads"
        );
        assert_eq!(
            stored["decided_first"],
            serde_json::json!({"settled": true, "answer": "kev ignore p=0.93"})
        );
        let again: StepRecord = serde_json::from_value(stored.clone()).unwrap();
        assert!(again.provenance.settled_first());
        assert_eq!(again.outcome.as_deref(), Some("ignore"));

        let mut dropped = stored;
        dropped.as_object_mut().unwrap().remove("decided_first");
        let older: StepRecord = serde_json::from_value(dropped).unwrap();
        assert_eq!(
            (older.outcome.as_deref(), older.provenance.outcome_by),
            (Some("ignore"), Some(OutcomeBy::DecisionModel)),
            "an older build that drops the field keeps a decision-model pick"
        );
    }

    #[test]
    fn an_intervention_says_who_stepped_in_where_and_what_had_been_chosen() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "review", None);
        inst.record_step("review", None, "looks fine", Utc::now(), 1, 0.0);
        inst.preset_outcome("review", "approved", OutcomeBy::DecisionModel);
        inst.record_failed_attempt("merge", "boom", Utc::now(), 1, 0.0, Default::default());
        inst.current_step_id = Some("merge".into());
        let variables = inst.variables.clone();

        let goto = inst.intervention(InterventionKind::Goto, Actor::Person { id: "alice".into() });
        assert_eq!(goto.kind, InterventionKind::Goto);
        assert_eq!(goto.actor, Actor::Person { id: "alice".into() });
        assert_eq!(goto.from_step.as_deref(), Some("merge"));
        assert_eq!(
            (goto.to_step.as_deref(), goto.outcome.as_deref()),
            (None, None)
        );
        assert_eq!(
            goto.chosen,
            Some(Chosen {
                step_id: "review".into(),
                outcome: Some("approved".into()),
                by: Some(OutcomeBy::DecisionModel),
            }),
            "the failed attempt is not what was chosen"
        );
        assert_eq!(
            serde_json::to_value(&goto).unwrap()["actor"],
            serde_json::json!({"kind": "person", "id": "alice"})
        );
        assert_eq!(inst.variables, variables, "nothing is put on the run");
    }

    #[test]
    fn an_actor_is_the_person_when_there_is_one_then_the_agent_then_the_hub() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            Actor::of(some("alice"), some("ops")),
            Actor::Person { id: "alice".into() }
        );
        assert_eq!(
            Actor::of(some(""), some("ops")),
            Actor::Agent { id: "ops".into() }
        );
        assert_eq!(Actor::of(None, None), Actor::System);
    }

    #[test]
    fn fail_last_record_marks_only_the_last_completed_run() {
        let mut instance = WorkflowInstance::new("wf", test_trigger(), "review", None);
        instance.record_step("review", None, "first", Utc::now(), 1, 0.0);
        instance.record_step("draft", None, "between", Utc::now(), 1, 0.0);
        instance.fail_last_record("review");
        assert!(instance.step_history.iter().all(|r| !r.failed));
        instance.record_step("review", None, "second", Utc::now(), 1, 0.0);
        instance.fail_last_record("review");
        let failed: Vec<bool> = instance.step_history.iter().map(|r| r.failed).collect();
        assert_eq!(failed, [false, false, true]);
        assert!(instance.last_record_of("review").is_none());
    }

    #[test]
    fn a_step_without_provenance_exposes_neither_key() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "a", None);
        inst.record_step_with_outputs(
            "a",
            None,
            "",
            Some(serde_json::json!({"x": "1"})),
            None,
            Utc::now(),
            1,
            0.0,
            false,
        );
        inst.record_step("b", None, "", Utc::now(), 1, 0.0);
        assert_eq!(inst.variables["steps"]["a"], serde_json::json!({"x": "1"}));
        assert!(inst.variables["steps"].get("b").is_none());

        let engine_only = StepProvenance {
            engine: Some("wasm".into()),
            ..Default::default()
        };
        assert_eq!(
            engine_only.exposed(None),
            Some(serde_json::json!({"_engine": "wasm"}))
        );
        let scalar = serde_json::json!("prose");
        assert_eq!(engine_only.exposed(Some(&scalar)), Some(scalar));
    }

    #[test]
    fn a_step_record_carries_its_provenance_and_older_records_read_without_it() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "review", None);
        let now = Utc::now();
        inst.record_step_with_provenance(
            "review",
            None,
            "OUTCOME: fix",
            None,
            None,
            now,
            12,
            0.42,
            false,
            StepProvenance {
                engine: Some("claude_code_bedrock".into()),
                model: Some("claude-opus-4-8".into()),
                branch_decided_by: None,
                outcome_by: None,
                decided_first: None,
                input_tokens: Some(180_000),
                output_tokens: Some(9_000),
                definition_hash: Some("ab".repeat(32)),
                handed_over: Vec::new(),
            },
        );
        let json = serde_json::to_value(&inst.step_history[0]).unwrap();
        // Flattened: the fields sit beside the record's own, so a consumer
        // that reads `model` off a step never has to know about the block.
        assert_eq!(json["engine"], "claude_code_bedrock");
        assert_eq!(json["model"], "claude-opus-4-8");
        assert_eq!(json["input_tokens"], 180_000);
        assert_eq!(json["cost_usd"], 0.42);
        assert!(json.get("provenance").is_none());

        // A record written before provenance existed still loads, empty.
        let legacy = serde_json::json!({
            "step_id": "review",
            "outcome": null,
            "agent_response": "",
            "started_at": now,
            "completed_at": now,
            "rounds": 1,
            "cost_usd": 0.0
        });
        let record: StepRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(record.provenance, StepProvenance::default());
        let again = serde_json::to_value(&record).unwrap();
        assert!(again.get("engine").is_none(), "absent stays absent");
    }

    fn attempt(engine: &str, cost_usd: f64) -> EngineAttempt {
        EngineAttempt {
            engine: engine.into(),
            model: Some("claude-opus-4-8".into()),
            input_tokens: 1_000,
            output_tokens: 100,
            cost_usd,
            error: None,
        }
    }

    #[test]
    fn a_failed_attempt_keeps_its_cost_and_leaves_the_variables_alone() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "implement", None);
        let before = inst.variables.clone();
        let started_at = Utc::now();
        inst.record_failed_attempt(
            "implement",
            "claude-code failed (exit 1)",
            started_at,
            7,
            1.25,
            StepProvenance {
                engine: Some("claude-code".into()),
                input_tokens: Some(2_000),
                ..Default::default()
            },
        );
        let record = &inst.step_history[0];
        assert!(record.failed);
        assert_eq!(record.outcome, None);
        assert_eq!(record.outputs, None);
        assert_eq!(record.report, None);
        assert!(!record.budget_capped);
        assert_eq!(record.agent_response, "claude-code failed (exit 1)");
        assert_eq!((record.rounds, record.cost_usd), (7, 1.25));
        assert_eq!(record.started_at, started_at);
        assert_eq!(record.provenance.engine.as_deref(), Some("claude-code"));
        assert_eq!(inst.updated_at, record.completed_at);
        assert_eq!(inst.variables, before, "no `_engine` stamp, no outputs");
        assert!((inst.total_cost() - 1.25).abs() < f64::EPSILON);
        assert_eq!(inst.total_rounds(), 7);
        assert_eq!(inst.completed_step_count(), 0);
    }

    /// A hub from before corrections existed reads a sent-back run as the
    /// failed attempt it is, and this one reads the records that hub wrote.
    #[test]
    fn a_sent_back_run_and_a_record_from_before_corrections_read_on_either_side() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "review", None);
        inst.record_correction(
            "review",
            "no block",
            None,
            Correction {
                cause: CorrectionCause::Malformed,
                detail: "'verdict' is missing".into(),
            },
            Utc::now(),
            1,
            0.01,
            false,
            StepProvenance::default(),
        );
        let stored = serde_json::to_value(&inst.step_history[0]).unwrap();
        assert_eq!(stored["failed"], true);
        assert!(stored.get("outcome").is_none_or(serde_json::Value::is_null));
        assert!(inst.last_record_of("review").is_none());
        assert_eq!(inst.completed_step_count(), 0);

        let before: StepRecord = serde_json::from_value(serde_json::json!({
            "step_id": "review",
            "outcome": "retry-missing-outputs",
            "agent_response": "no block",
            "outputs": null,
            "started_at": "2026-09-01T10:00:00Z",
            "completed_at": "2026-09-01T10:01:00Z",
            "rounds": 2,
            "cost_usd": 0.01,
        }))
        .unwrap();
        assert!(!before.failed && before.correction.is_none());
        inst.step_history.push(before);
        assert_eq!(
            inst.corrections_of("review", CorrectionCause::Malformed),
            0,
            "a run that was retrying under the old rule starts its repairs afresh"
        );
    }

    #[test]
    fn the_last_record_of_a_step_is_none_when_it_failed() {
        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "review", None);
        assert!(inst.last_record_of("review").is_none());
        inst.record_step("review", None, "looks good", Utc::now(), 1, 0.1);
        assert_eq!(
            inst.last_record_of("review")
                .map(|r| r.agent_response.as_str()),
            Some("looks good")
        );
        assert!(inst.last_record_of("implement").is_none());
        inst.record_failed_attempt(
            "review",
            "boom",
            Utc::now(),
            1,
            0.2,
            StepProvenance::default(),
        );
        assert!(inst.last_record_of("review").is_none());
    }

    #[test]
    fn handed_over_and_failed_stay_absent_when_empty_and_round_trip_when_set() {
        let now = Utc::now();
        let legacy = serde_json::json!({
            "step_id": "implement",
            "outcome": null,
            "agent_response": "",
            "started_at": now,
            "completed_at": now,
            "rounds": 3,
            "cost_usd": 0.5,
            "engine": "claude-code:bedrock"
        });
        let record: StepRecord = serde_json::from_value(legacy).unwrap();
        assert!(!record.failed);
        assert!(record.provenance.handed_over.is_empty());
        let written = serde_json::to_value(&record).unwrap();
        assert!(written.get("failed").is_none());
        assert!(written.get("handed_over").is_none());

        let mut inst = WorkflowInstance::new("bug-fix", test_trigger(), "implement", None);
        inst.record_failed_attempt(
            "implement",
            "no usable answer",
            now,
            4,
            2.5,
            StepProvenance {
                engine: Some("claude-code:bedrock".into()),
                handed_over: vec![attempt("claude-code", 1.0), attempt("codex", 0.5)],
                ..Default::default()
            },
        );
        let json = serde_json::to_value(&inst.step_history[0]).unwrap();
        assert_eq!(json["failed"], true);
        assert_eq!(json["handed_over"][0]["engine"], "claude-code");
        assert_eq!(json["handed_over"][1]["cost_usd"], 0.5);
        assert!(json["handed_over"][0].get("error").is_none());
        let back: StepRecord = serde_json::from_value(json).unwrap();
        assert!(back.failed);
        assert_eq!(back.provenance, inst.step_history[0].provenance);

        let errored = EngineAttempt {
            error: Some("secure command runner unavailable".into()),
            ..attempt("codex", 0.0)
        };
        let written = serde_json::to_value(&errored).unwrap();
        assert_eq!(written["error"], "secure command runner unavailable");
        assert_eq!(
            serde_json::from_value::<EngineAttempt>(written).unwrap(),
            errored
        );
        let legacy_attempt: EngineAttempt = serde_json::from_value(serde_json::json!({
            "engine": "codex",
            "input_tokens": 0,
            "output_tokens": 0,
            "cost_usd": 0.0
        }))
        .unwrap();
        assert_eq!(legacy_attempt.error, None);
    }

    #[test]
    fn booked_ticket_comes_from_the_trigger_or_a_picking_step() {
        assert_eq!(booked_ticket(&serde_json::json!({})), None);

        let picked = serde_json::json!({
            "steps": { "pick": { "ticket_id": "ENG-106", "target_repo": "o/r" } }
        });
        assert_eq!(booked_ticket(&picked), Some("ENG-106"));
        assert_eq!(step_output(&picked, "target_repo"), Some("o/r"));
        assert_eq!(step_output(&picked, "missing"), None);

        let triggered = serde_json::json!({
            "ticket_id": "DW-400",
            "steps": { "pick": { "ticket_id": "ENG-106" } }
        });
        assert_eq!(booked_ticket(&triggered), Some("DW-400"));
    }

    #[test]
    fn a_non_string_ticket_id_is_not_a_booking() {
        assert_eq!(
            booked_ticket(&serde_json::json!({ "ticket_id": 2613 })),
            None
        );
        assert_eq!(
            booked_ticket(&serde_json::json!({
                "ticket_id": null,
                "steps": { "pick": { "ticket_id": 7 }, "claim": { "ticket_id": "ENG-1" } }
            })),
            Some("ENG-1")
        );
        for scalar in [
            serde_json::json!("ENG-1"),
            serde_json::json!(null),
            serde_json::json!(["ENG-1"]),
            serde_json::json!({ "steps": "ENG-1" }),
        ] {
            assert_eq!(booked_ticket(&scalar), None, "{scalar}");
        }
    }

    #[test]
    fn new_instance() {
        let inst = WorkflowInstance::new("bug-fix", test_trigger(), "investigate", None);
        assert_eq!(inst.workflow_id, "bug-fix");
        assert_eq!(inst.status, InstanceStatus::Running);
        assert_eq!(inst.current_step_id.as_deref(), Some("investigate"));
        assert!(inst.step_history.is_empty());
        assert!(!inst.is_terminal());
        assert!(inst.parent_instance_id.is_none());
    }

    #[test]
    fn record_step_and_totals() {
        let mut inst = WorkflowInstance::new("test", test_trigger(), "step1", None);
        let now = Utc::now();

        inst.record_step_with_outputs("step1", None, "Did step 1", None, None, now, 5, 0.01, false);
        inst.record_step_with_outputs(
            "step2",
            Some("done".into()),
            "Did step 2",
            None,
            None,
            now,
            3,
            0.02,
            true,
        );

        assert_eq!(inst.completed_step_count(), 2);
        assert_eq!(inst.total_rounds(), 8);
        assert!((inst.total_cost() - 0.03).abs() < f64::EPSILON);
        assert!(!inst.step_history[0].budget_capped);
        assert!(inst.step_history[1].budget_capped);
    }

    #[test]
    fn with_step_outputs_is_a_pure_merge() {
        let vars = serde_json::json!({"ticket": "DW-1", "steps": {"a": {"x": "1"}}});
        let merged = with_step_outputs(&vars, "b", Some(&serde_json::json!({"y": "2"})));
        assert_eq!(merged["steps"]["a"]["x"], "1");
        assert_eq!(merged["steps"]["b"]["y"], "2");
        assert_eq!(merged["ticket"], "DW-1");
        assert!(vars["steps"].get("b").is_none());
        assert_eq!(with_step_outputs(&vars, "b", None), vars);
        let scalar = serde_json::json!("not an object");
        assert_eq!(
            with_step_outputs(&scalar, "b", Some(&serde_json::json!({}))),
            scalar
        );
    }

    #[test]
    fn terminal_states() {
        let mut inst = WorkflowInstance::new("test", test_trigger(), "s1", None);
        assert!(!inst.is_terminal());

        inst.status = InstanceStatus::Completed;
        assert!(inst.is_terminal());

        inst.status = InstanceStatus::Failed;
        assert!(inst.is_terminal());

        inst.status = InstanceStatus::Exited;
        assert!(inst.is_terminal());

        inst.status = InstanceStatus::Paused;
        assert!(!inst.is_terminal());

        inst.status = InstanceStatus::Queued;
        assert!(!inst.is_terminal());

        inst.status = InstanceStatus::Cancelled;
        assert!(inst.is_terminal());
    }

    #[test]
    fn status_display_and_parse() {
        assert_eq!(InstanceStatus::Running.to_string(), "running");
        assert_eq!(InstanceStatus::Paused.to_string(), "paused");
        assert_eq!(
            "completed".parse::<InstanceStatus>(),
            Ok(InstanceStatus::Completed)
        );
        assert!("bogus".parse::<InstanceStatus>().is_err());
    }

    #[test]
    fn queued_status_round_trips_through_display_and_parse() {
        assert_eq!(InstanceStatus::Queued.to_string(), "queued");
        assert_eq!(
            "queued".parse::<InstanceStatus>(),
            Ok(InstanceStatus::Queued)
        );
        assert_eq!(
            serde_json::to_value(InstanceStatus::Queued).unwrap(),
            serde_json::json!("queued")
        );
        assert_eq!(
            serde_json::from_value::<InstanceStatus>(serde_json::json!("queued")).unwrap(),
            InstanceStatus::Queued
        );
    }

    #[test]
    fn trigger_info_serialization() {
        let trigger = TriggerInfo {
            source: TriggerSource::Webhook {
                source: "linear".into(),
                event_type: "issue.assigned".into(),
            },
            payload: serde_json::json!({"issue_id": "DW-350"}),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: TriggerInfo = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed.source, TriggerSource::Webhook { .. }));
    }

    #[test]
    fn variables_carry_trigger_payload() {
        let trigger = TriggerInfo {
            source: TriggerSource::Cli,
            payload: serde_json::json!({"repo": "driftwerk/workforce"}),
        };
        let inst = WorkflowInstance::new("test", trigger, "s1", None);
        assert_eq!(inst.variables["repo"], "driftwerk/workforce");
    }

    /// `_run_as` scopes execution identity and `_channel_id`/`_thread_id`
    /// route delivery — a webhook body must not be able to set them. The
    /// verbatim payload stays on `trigger` for audit.
    #[test]
    fn engine_owned_keys_in_a_trigger_payload_never_reach_variables() {
        let trigger = TriggerInfo {
            source: TriggerSource::Webhook {
                source: "external".into(),
                event_type: "push".into(),
            },
            payload: serde_json::json!({
                "repo": "driftwerk/workforce",
                "_run_as": "someone-else",
                "_channel_id": "C_ATTACKER",
                "_thread_id": "168.42",
            }),
        };
        let inst = WorkflowInstance::new("test", trigger, "s1", None);
        assert_eq!(inst.variables["repo"], "driftwerk/workforce");
        for key in ["_run_as", "_channel_id", "_thread_id"] {
            assert!(
                inst.variables.get(key).is_none(),
                "engine-owned key '{key}' must be stripped from variables"
            );
        }
        assert_eq!(
            inst.trigger.payload["_channel_id"], "C_ATTACKER",
            "the audit copy on the trigger stays verbatim"
        );
    }
}

//! Agent-callable tool for managing and designing TOML-defined workflows.
//!
//! Actions: list, start, status, resume, reject, abort, instances,
//!          validate, save, get, schema.
//! Mirrors the `SchedulerTool` pattern.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_common::settings::DecisionUse;
use workforce_orchestrator::workflow::definition::WorkflowDefinition;
use workforce_orchestrator::workflow::instance::Actor;

use crate::def_write_auth::{
    run_as_write, write_visibility, DefOp, DefWriteGrant, TurnDefWriteAuth,
};
use crate::resource_access::DefListing;
use crate::run_steering::{Steer, SteerAction, SteerRefused, Steerer};
use crate::workflow_runtime::{def_key, TriggerUrls};

/// `owner` value that addresses the shared system row for `get` and `start`
/// when the caller also has a copy of their own under the same id.
const SYSTEM_ROW: &str = "system";
const INSTANCES_DEFAULT_LIMIT: usize = 50;
const INSTANCES_MAX_LIMIT: usize = 500;

pub struct WorkflowTool {
    runtime: Arc<dyn crate::hosts::WorkflowHost>,
    /// The agent ID that owns this tool instance, used as the originating
    /// agent for workflow DM delivery.
    agent_id: String,
    scheduler: Option<Arc<dyn crate::hosts::SchedulerHost>>,
    /// Who may save which definition row, for the turn's user — the same rule
    /// the gateway applies. The bare default treats every turn as a member's.
    write_auth: TurnDefWriteAuth,
}

/// Why a failed instance stopped. `WorkflowExecutor::fail` records it into the
/// instance variables; `status` is the only instance-inspection path an agent
/// has, so without this a failure reads as a bare "failed".
fn failure_reason(variables: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(variables)
        .ok()?
        .get("_failure_reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The package an exit step handed over, if the run ended with one.
fn handoff_package(variables: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(variables)
        .ok()?
        .get(workforce_orchestrator::workflow::package::HANDOFF_KEY)
        .map(bounded)
}

/// Bounded summary of the last recorded step — enough to locate a failure
/// without pulling every step's full agent response into a tool result.
fn last_step_summary(step_history: &str) -> Option<serde_json::Value> {
    let steps: Vec<serde_json::Value> = serde_json::from_str(step_history).ok()?;
    let last = steps.last()?;
    Some(serde_json::json!({
        "step_id": last.get("step_id"),
        "outcome": last.get("outcome"),
        "rounds": last.get("rounds"),
        "cost_usd": last.get("cost_usd"),
        "completed_at": last.get("completed_at"),
        "failed": attempt_failed(last),
    }))
}

/// Whether a recorded step is a run of it that failed rather than completed.
fn attempt_failed(step: &serde_json::Value) -> bool {
    step.get("failed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Longest string a step output keeps in a status reply; a review summary
/// or a diff can run to tens of kilobytes, and the caller wants the shape,
/// not the transcript.
const OUTPUT_VALUE_CAP: usize = 2_000;

/// `value` with every string, at any depth, cut to [`OUTPUT_VALUE_CAP`]
/// characters and marked as cut.
fn bounded(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) => Value::String(match s.char_indices().nth(OUTPUT_VALUE_CAP) {
            Some((cut, _)) => format!("{}…", &s[..cut]),
            None => s.clone(),
        }),
        Value::Array(items) => Value::Array(items.iter().map(bounded).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), bounded(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Serialised outputs a status reply carries across all its steps. Newest
/// steps keep theirs; once the budget is spent, older steps keep their
/// metadata and say their outputs were left out.
const STEPS_OUTPUT_BUDGET: usize = 24_000;

/// Every recorded step with what it produced: outcome, timing, cost, the
/// engine and model that ran it, and its outputs, bounded per string and
/// in total. The agent response itself stays out; the outputs are the
/// contract.
fn step_history_summary(step_history: &str) -> Vec<serde_json::Value> {
    let steps = serde_json::from_str::<Vec<serde_json::Value>>(step_history).unwrap_or_default();
    let mut spent = 0usize;
    let mut summaries: Vec<serde_json::Value> = steps
        .iter()
        .rev()
        .map(|step| {
            let outputs = step.get("outputs").map(bounded);
            let size = outputs.as_ref().map_or(0, |o| o.to_string().len());
            let within = spent + size <= STEPS_OUTPUT_BUDGET;
            spent += size;
            let mut summary = serde_json::json!({
                "step_id": step.get("step_id"),
                "outcome": step.get("outcome"),
                "started_at": step.get("started_at"),
                "completed_at": step.get("completed_at"),
                "rounds": step.get("rounds"),
                "cost_usd": step.get("cost_usd"),
                "engine": step.get("engine"),
                "model": step.get("model"),
                "failed": attempt_failed(step),
                "outputs": if within { outputs } else { None },
            });
            if !within {
                summary["outputs_omitted"] = serde_json::Value::Bool(true);
            }
            if let Some(correction) = step.get("correction") {
                summary["correction"] = bounded(correction);
            }
            if let Some(first) = step.get("decided_first") {
                summary["decided_first"] = first.clone();
            }
            summary
        })
        .collect();
    summaries.reverse();
    summaries
}

impl WorkflowTool {
    pub fn new(
        runtime: Arc<dyn crate::hosts::WorkflowHost>,
        agent_id: String,
        scheduler: Option<Arc<dyn crate::hosts::SchedulerHost>>,
    ) -> Self {
        Self {
            runtime,
            agent_id,
            scheduler,
            write_auth: TurnDefWriteAuth::default(),
        }
    }

    pub fn with_write_auth(mut self, write_auth: TurnDefWriteAuth) -> Self {
        self.write_auth = write_auth;
        self
    }
}

#[derive(Deserialize)]
struct WorkflowInput {
    action: String,
    #[serde(default)]
    workflow_id: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    gate: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    status_filter: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    feedback: Option<String>,
    #[serde(default)]
    definition: Option<serde_json::Value>,
    #[serde(default)]
    toml_content: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    run_as: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The first `limit` of `instances` (the store lists newest-updated first),
/// with `total` so a caller can tell the list was cut.
fn instances_payload(instances: &[WorkflowInstanceRow], limit: usize) -> serde_json::Value {
    let list: Vec<serde_json::Value> = instances
        .iter()
        .take(limit)
        .map(|row| {
            serde_json::json!({
                "instance_id": row.id,
                "workflow_id": row.workflow_id,
                "status": row.status,
                "current_step": row.current_step_id,
                "created_at": row.created_at,
                "updated_at": row.updated_at,
            })
        })
        .collect();
    serde_json::json!({
        "count": list.len(),
        "total": instances.len(),
        "instances": list,
    })
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }

    fn description(&self) -> &str {
        "Manage and design workflows: list, start, status, resume, reject, abort, instances, validate, save, get, delete, schema"
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "start", "status", "resume", "reject", "abort", "instances",
                             "validate", "save", "get", "delete", "schema"],
                    "description": "Action to perform"
                },
                "workflow_id": {
                    "type": "string",
                    "description": "Workflow definition ID (required for 'start', 'save', 'get')"
                },
                "payload": {
                    "type": "object",
                    "description": "Context payload for the workflow (for 'start'). \
                        Required field names differ per workflow — call action='get' \
                        with workflow_id first to see the context schema, OR call start \
                        and read the validation error which lists every required field. \
                        Common pitfalls: the field is `repo` not `workspace`, and \
                        `ticket_id` not `ticket`."
                },
                "instance_id": {
                    "type": "string",
                    "description": "Workflow instance UUID string (required for 'status')"
                },
                "outcome": {
                    "type": "string",
                    "description": "Outcome label to resume with (for 'resume'); at a decision, a button's outcome or label"
                },
                "gate": {
                    "type": "string",
                    "description": "The open decision's gate (from 'status'), for 'resume' at a decision"
                },
                "channel_id": {
                    "type": "string",
                    "description": "Slack channel ID for status delivery (for 'start', 'resume')"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Slack thread timestamp for threaded delivery (for 'start')"
                },
                "status_filter": {
                    "type": "string",
                    "enum": ["queued", "running", "paused", "completed", "failed", "exited", "cancelled"],
                    "description": "Filter instances by status (for 'instances' action)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": INSTANCES_MAX_LIMIT,
                    "description": "Most recently updated instances to return (for 'instances' action; default 50)"
                },
                "reason": {
                    "type": "string",
                    "description": "Rejection reason (for 'reject' action)"
                },
                "feedback": {
                    "type": "string",
                    "description": "Detailed feedback for the agent (for 'reject' action)"
                },
                "toml_content": {
                    "type": "string",
                    "description": "Raw TOML workflow definition (for 'validate' and 'save'). Alternative to 'definition'."
                },
                "owner": {
                    "type": "string",
                    "description": "Identity id of the workflow's owner, to read, edit, start or \
                        delete another member's workflow that was shared with you (for 'get', \
                        'save', 'start' and 'delete'). Omit for your own — or, as an \
                        administrator, the shared system workflow. 'system' reads or starts \
                        the shared workflow when you also have a copy of your own under the \
                        same id."
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "global"],
                    "description": "Who may see and run a workflow you own (for 'save'): 'private' \
                        (the default), 'team' — anyone in a team with you — or 'global' — anyone. \
                        Only the owner may change it."
                },
                "run_as": {
                    "type": "string",
                    "enum": ["owner", "me"],
                    "description": "Who a member-owned workflow's automated runs (webhook, schedule, \
                        HTTP trigger) execute as (for 'save'): 'owner', or 'me' — the saver, whose \
                        access the runs then use. Automation pauses while the last save was by \
                        someone other than the run identity, so saving a shared workflow on its \
                        owner's behalf pauses it until the owner re-saves unless you take it over \
                        with 'me'. Omit to leave it unchanged."
                }
            },
            "required": ["action"]
        });

        // Build the definition schema separately to avoid macro recursion limits.
        let decide_first = crate::decide::decisions()
            .is_some_and(|d| d.decide_first_use().mode != DecisionUse::Off);
        schema["properties"]["definition"] = definition_json_schema(decide_first);

        schema
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: WorkflowInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid workflow input: {}", e))?;

        match input.action.as_str() {
            "list" => self.handle_list(start).await,
            "start" => self.handle_start(input, start).await,
            "status" => self.handle_status(input, start).await,
            "resume" => self.handle_resume(input, start).await,
            "reject" => self.handle_reject(input, start).await,
            "abort" => self.handle_abort(input, start).await,
            "instances" => self.handle_instances(input, start).await,
            "schema" => Ok(Self::handle_schema(start)),
            "validate" => Ok(Self::handle_validate(input, start)),
            "save" => self.handle_save(input, start).await,
            "get" => self.handle_get(input, start).await,
            "delete" => self.handle_delete(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Use: list, start, status, resume, reject, abort, instances, validate, save, get, delete, schema",
                    other
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl WorkflowTool {
    async fn resolve_channel_thread(
        &self,
        channel_id: Option<String>,
        thread_id: Option<String>,
    ) -> (Option<String>, Option<String>) {
        if channel_id.is_some() {
            return (channel_id, thread_id);
        }
        // Try scheduler messaging context first (available on the default agent)
        if let Some(ref sched) = self.scheduler {
            let session_key = workforce_agent::runner::CURRENT_SESSION_KEY
                .try_with(|k| k.clone())
                .unwrap_or_default();
            if let Some((cid, tid)) = sched.get_messaging_context(&session_key).await {
                tracing::debug!(
                    channel_id = %cid,
                    thread_id = ?tid,
                    session_key = %session_key,
                    "Resolved workflow channel/thread from messaging context"
                );
                return (Some(cid), tid.or(thread_id));
            }
        }
        // Fallback: read from task-local context (works for ALL agents, including
        // those without a scheduler — the task-locals are set by
        // process_message_with_progress before the agent run starts).
        let task_cid = workforce_agent::runner::CURRENT_CHANNEL_ID
            .try_with(|c| c.clone())
            .ok()
            .filter(|s| !s.is_empty());
        let task_tid = workforce_agent::runner::CURRENT_THREAD_ID
            .try_with(|t| t.clone())
            .ok()
            .filter(|s| !s.is_empty());
        if let Some(cid) = task_cid {
            tracing::info!(
                channel_id = %cid,
                thread_id = ?task_tid,
                "Resolved workflow channel/thread from task-local context"
            );
            return (Some(cid), task_tid.or(thread_id));
        }
        tracing::warn!("No channel context available for workflow — channel/thread will be None");
        (channel_id, thread_id)
    }

    async fn handle_list(&self, start: Instant) -> Result<ToolResult> {
        // Scope to the turn's user: their own, the global ones and any shared
        // with them — an administrator sees every row, exactly as `may_read_def`
        // grants them and the gateway's listing already answers. A member never
        // gets the unfiltered set, which would leak every owner's private
        // workflow names.
        let scope = DefListing::for_turn(
            &self.write_auth,
            self.runtime.def_store(),
            workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        )
        .await;
        let defs = self.runtime.list_definitions_visible(&scope).await;
        let defs = self.runtime.annotate_automation(defs).await;
        let list = self.runtime.summaries_json(&defs).await;

        Ok(ToolResult::success(
            serde_json::json!({
                "workflows": list,
                "count": list.len()
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_start(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let workflow_id = match input.workflow_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'workflow_id' — which workflow to start?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let owner = match self
            .readable_owner(
                &workflow_id,
                input.owner.as_deref().filter(|o| !o.is_empty()),
            )
            .await
        {
            Ok(owner) => owner,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let key = def_key(owner.as_deref(), &workflow_id);
        let payload = input.payload.unwrap_or(serde_json::json!({}));

        let (channel_id, thread_id) = self
            .resolve_channel_thread(input.channel_id, input.thread_id)
            .await;

        // `_channel_id`/`_thread_id` are engine-owned: `start_workflow` seeds
        // them into instance variables from the arguments below, and the
        // constructor strips them out of payloads.

        // Auto-detect parent instance when this tool is called from within a
        // workflow step. The session key format is "workflow:{instance_id}:{step_id}".
        let parent_instance_id = workforce_agent::runner::CURRENT_SESSION_KEY
            .try_with(|key| {
                key.strip_prefix("workflow:")
                    .and_then(|rest| rest.split(':').next())
                    .map(|id| id.to_string())
            })
            .ok()
            .flatten();

        match Arc::clone(&self.runtime)
            .start_workflow(
                &key,
                payload.clone(),
                channel_id,
                thread_id,
                Some(self.agent_id.clone()),
                parent_instance_id,
            )
            .await
        {
            Ok(instance_id) => Ok(ToolResult::success(
                serde_json::json!({
                    "instance_id": instance_id,
                    "workflow_id": workflow_id,
                    "status": "running",
                    "message": format!(
                        "Workflow '{workflow_id}' started as instance {instance_id}. \
                         STOP HERE. The workflow is running in a separate subprocess \
                         and will post its own progress and results to this thread. \
                         Do NOT attempt to do the ticket's work yourself — no code \
                         reads, file edits, git commands, or tool calls of any kind \
                         beyond this tool call. Your only remaining job for this \
                         request is a one-sentence acknowledgement that the workflow \
                         was started (include the instance id), then end your turn."
                    )
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to start workflow: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_status(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let instance_id = match input.instance_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'instance_id' — which instance to check?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match self
            .runtime
            .readable_run(&instance_id, &self.write_auth.writer())
            .await?
        {
            Some(row) => Ok(ToolResult::success(
                serde_json::json!({
                    "instance_id": row.id,
                    "workflow_id": row.workflow_id,
                    "status": row.status,
                    "current_step": row.current_step_id,
                    "created_at": row.created_at,
                    "updated_at": row.updated_at,
                    "failure_reason": failure_reason(&row.variables),
                    "handoff": handoff_package(&row.variables),
                    "last_step": last_step_summary(&row.step_history),
                    "steps": step_history_summary(&row.step_history),
                    "decision": self
                        .runtime
                        .decision_view(&row, self.write_auth.writer().identity_id.as_deref()),
                }),
                start.elapsed().as_millis() as u64,
            )),
            None => Ok(ToolResult::failure(
                format!("Instance '{}' not found", instance_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_resume(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let instance_id = match input.instance_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'instance_id' — which instance to resume?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let elapsed = || start.elapsed().as_millis() as u64;
        let gate = match input.gate.as_deref().map(uuid::Uuid::parse_str).transpose() {
            Ok(gate) => gate,
            Err(_) => {
                return Ok(ToolResult::failure(
                    crate::workflow_runtime::DecisionRefused::Outdated.to_string(),
                    elapsed(),
                ))
            }
        };
        let (channel_id, thread_id) = self
            .resolve_channel_thread(input.channel_id, input.thread_id)
            .await;
        let writer = self.write_auth.writer();
        let actor = Actor::of(writer.identity_id.clone(), Some(self.agent_id.clone()));
        let ask = crate::workflow_runtime::ResumeAsk {
            outcome: input.outcome.clone(),
            gate,
            feedback: None,
            channel_id,
            thread_id,
            originating_agent_id: Some(self.agent_id.clone()),
        };
        Ok(
            match Arc::clone(&self.runtime)
                .resume_as(&instance_id, ask, &Steerer { writer, actor })
                .await
            {
                Ok(crate::workflow_runtime::Resumed::Moved { applied }) => ToolResult::success(
                    serde_json::json!({
                        "instance_id": instance_id,
                        "status": "running",
                        "message": crate::workflow_runtime::resumed_message(input.outcome.as_deref(), applied, "'"),
                        "outcome": input.outcome,
                        "outcome_applied": applied
                    }),
                    elapsed(),
                ),
                Ok(crate::workflow_runtime::Resumed::Decided(made)) => ToolResult::success(
                    serde_json::json!({
                        "instance_id": instance_id,
                        "status": "running",
                        "message": format!("Decided: {}.", made.summary),
                        "decided": { "outcome": made.outcome, "label": made.label, "summary": made.summary },
                    }),
                    elapsed(),
                ),
                Err(crate::workflow_runtime::ResumeRefused::Failed(why)) => {
                    ToolResult::failure(format!("Failed to resume: {why}"), elapsed())
                }
                Err(refused) => ToolResult::failure(refused.to_string(), elapsed()),
            },
        )
    }

    async fn handle_reject(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let instance_id = match input.instance_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'instance_id' — which instance to reject?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let by = match self.steer(&instance_id, SteerAction::Reject).await {
            Ok(by) => by,
            Err(refused) => {
                return Ok(ToolResult::failure(
                    refused.to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let (channel_id, thread_id) = self
            .resolve_channel_thread(input.channel_id, input.thread_id)
            .await;

        match Arc::clone(&self.runtime)
            .reject_workflow_step(
                &instance_id,
                input.reason.as_deref(),
                input.feedback.as_deref(),
                channel_id,
                thread_id,
                Some(self.agent_id.clone()),
                &by,
            )
            .await
        {
            Ok(applied) => Ok(ToolResult::success(
                serde_json::json!({
                    "instance_id": instance_id,
                    "status": "running",
                    "reason": input.reason,
                    "outcome_applied": applied,
                    "message": crate::workflow_runtime::resumed_message(
                        Some(crate::workflow_runtime::REJECTED),
                        applied,
                        "'"
                    )
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to reject: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_abort(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let instance_id = match input.instance_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'instance_id' — which instance to abort?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let by = match self.steer(&instance_id, SteerAction::Abort).await {
            Ok(by) => by,
            Err(refused) => {
                return Ok(ToolResult::failure(
                    refused.to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        match Arc::clone(&self.runtime)
            .abort_workflow(&instance_id, &by)
            .await
        {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({
                    "instance_id": instance_id,
                    "status": "cancelled",
                    "message": "Workflow cancelled"
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to abort: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Leave for the turn's user, through this agent, to `action` the run
    /// `instance_id`.
    async fn steer(
        &self,
        instance_id: &str,
        action: SteerAction,
    ) -> std::result::Result<Steer, SteerRefused> {
        let writer = self.write_auth.writer();
        let actor = Actor::of(writer.identity_id.clone(), Some(self.agent_id.clone()));
        self.runtime
            .authorize_steer(instance_id, &Steerer { writer, actor }, action)
            .await
    }

    /// The runs the turn's user may see; a run they may not see is, to them,
    /// not there.
    async fn handle_instances(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let rows = self
            .runtime
            .list_instances(input.status_filter.as_deref(), input.workflow_id.as_deref())?;
        let instances = self
            .runtime
            .readable_runs(rows, &self.write_auth.writer())
            .await;
        let limit = input
            .limit
            .unwrap_or(INSTANCES_DEFAULT_LIMIT)
            .clamp(1, INSTANCES_MAX_LIMIT);
        Ok(ToolResult::success(
            instances_payload(&instances, limit),
            start.elapsed().as_millis() as u64,
        ))
    }

    fn resolve_toml_content(input: &WorkflowInput) -> std::result::Result<String, String> {
        if let Some(ref def_json) = input.definition {
            let def: WorkflowDefinition = serde_json::from_value(def_json.clone())
                .map_err(|e| format!("Invalid workflow definition JSON: {}", e))?;
            toml::to_string_pretty(&def)
                .map_err(|e| format!("Failed to serialize workflow to TOML: {}", e))
        } else if let Some(ref toml_str) = input.toml_content {
            Ok(toml_str.clone())
        } else {
            Err("Provide either 'definition' (JSON object) or 'toml_content' (TOML string)".into())
        }
    }

    fn handle_schema(start: Instant) -> ToolResult {
        ToolResult::success(
            serde_json::json!({
                "schema_reference": WORKFLOW_SCHEMA_REFERENCE,
            }),
            start.elapsed().as_millis() as u64,
        )
    }

    fn handle_validate(input: WorkflowInput, start: Instant) -> ToolResult {
        let elapsed = || start.elapsed().as_millis() as u64;
        let toml_content = match Self::resolve_toml_content(&input) {
            Ok(t) => t,
            Err(e) => return ToolResult::failure(e, elapsed()),
        };

        // A definition that will not run is a failed call, not a successful
        // report of failure: the diagnostic has to land where the caller
        // handles errors, or it reads as a pass and gets saved anyway.
        match WorkflowDefinition::from_toml(&toml_content) {
            Err(e) => ToolResult::failure(format!("{e:#}"), elapsed()),
            Ok(def) => match def.authoring_issues() {
                issues if issues.is_empty() => ToolResult::success(
                    serde_json::json!({
                        "valid": true,
                        "workflow_id": def.workflow.id,
                        "name": def.workflow.name,
                        "step_count": def.steps.len(),
                        "step_ids": def.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                    }),
                    elapsed(),
                ),
                issues => ToolResult::failure(
                    format!(
                        "Workflow '{}' will not run as written:\n  - {}",
                        def.workflow.id,
                        issues.join("\n  - ")
                    ),
                    elapsed(),
                ),
            },
        }
    }

    async fn handle_save(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let workflow_id = match input.workflow_id {
            Some(ref id) if !id.is_empty() => id.clone(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'workflow_id' — what ID should this workflow have?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let toml_content = match Self::resolve_toml_content(&input) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
            }
        };

        let elapsed = || start.elapsed().as_millis() as u64;
        let grant = match self
            .write_auth
            .authorize(
                self.runtime.def_store().cloned(),
                workforce_orchestrator::workflow_store::KIND_WORKFLOW,
                &workflow_id,
                input.owner.as_deref().filter(|o| !o.is_empty()),
                DefOp::Save,
            )
            .await
        {
            Ok(grant) => grant,
            Err(denied) => {
                return Ok(ToolResult::failure(
                    format!("Cannot save workflow '{workflow_id}': {denied}"),
                    elapsed(),
                ));
            }
        };
        let stored = self
            .runtime
            .definition_ownership(&workflow_id, grant.owner.as_deref())
            .await
            .map(|(_, visibility)| visibility);
        let visibility = match write_visibility(&grant, input.visibility.as_deref(), stored) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, elapsed())),
        };
        let run_as = match run_as_write(&grant, input.run_as.as_deref()) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::failure(e, elapsed())),
        };
        match self
            .runtime
            .save_definition_source(
                &workflow_id,
                &toml_content,
                grant.owner.as_deref(),
                visibility,
                run_as,
                workforce_orchestrator::workflow_store::EditContext::new(
                    "agent_tool",
                    grant.actor.as_deref(),
                ),
            )
            .await
        {
            Ok(warnings) => {
                let key = def_key(grant.owner.as_deref(), &workflow_id);
                let run_identity = match &grant.owner {
                    Some(owner) => Some(
                        self.runtime
                            .definition_run_as(&workflow_id, Some(owner))
                            .await
                            .unwrap_or_else(|| owner.clone()),
                    ),
                    None => None,
                };
                let automated_runs_blocked = self
                    .runtime
                    .automated_run_block(&key, run_identity.as_deref())
                    .await
                    .map(|b| b.to_string());
                // The reload inside `save_definition_source` has already minted
                // any new trigger path, so the first save can name it.
                let trigger_urls = self.runtime.trigger_urls(&key).await;
                let message = self
                    .saved_message(
                        &workflow_id,
                        &grant,
                        visibility,
                        run_identity.as_deref(),
                        automated_runs_blocked.as_deref(),
                        &trigger_urls,
                    )
                    .await;
                Ok(ToolResult::success(
                    self.runtime
                        .with_trigger_urls(
                            &key,
                            serde_json::json!({
                                "saved": true,
                                "workflow_id": workflow_id,
                                "owner": grant.owner,
                                "visibility": visibility,
                                "run_as": run_identity,
                                "automated_runs_blocked": automated_runs_blocked,
                                "key": key,
                                "message": message,
                                "warnings": warnings,
                            }),
                        )
                        .await,
                    elapsed(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to save workflow: {:#}", e),
                elapsed(),
            )),
        }
    }

    /// What a save did, said plainly: a member's save of an id that also
    /// exists as a shared definition is a copy of their own, and the shared
    /// one is unchanged; for an owned workflow, who its automation runs as and
    /// whether this save paused it.
    async fn saved_message(
        &self,
        id: &str,
        grant: &DefWriteGrant,
        visibility: workforce_core::access::Visibility,
        run_identity: Option<&str>,
        automated_runs_blocked: Option<&str>,
        trigger_urls: &[TriggerUrls],
    ) -> String {
        let mut message = if self.shadows_shared(id, grant).await {
            format!(
                "Saved as your {} copy of '{id}'; the shared '{id}' is unchanged. \
                 Your copy is what '{id}' now means for you; address the shared one with \
                 owner \"system\".",
                visibility.as_str()
            )
        } else {
            format!("Workflow '{id}' saved and loaded")
        };
        if let Some(identity) = run_identity {
            let who = match grant.actor.as_deref() == Some(identity) {
                true => "you".to_string(),
                false if grant.owner.as_deref() == Some(identity) => "its owner".to_string(),
                false => identity.to_string(),
            };
            message.push_str(&format!(" Automated runs execute as {who}."));
        }
        if let Some(reason) = automated_runs_blocked {
            message.push_str(&format!(" Automation is paused: {reason}."));
        }
        if let Some(sentence) = http_trigger_sentence(trigger_urls) {
            message.push_str(&sentence);
        }
        message
    }

    /// The row `get` reads: the named owner's, when the turn's user may read
    /// it; otherwise the user's own copy of `id` when there is one, else the
    /// shared definition. A row the user may not read is, to them, not there.
    async fn readable_owner(
        &self,
        id: &str,
        requested: Option<&str>,
    ) -> std::result::Result<Option<String>, String> {
        if requested == Some(SYSTEM_ROW) {
            return Ok(None);
        }
        let Some(owner) = requested else {
            let own = match crate::user_config::current_identity_id() {
                Some(user) => self
                    .runtime
                    .definition_ownership(id, Some(&user))
                    .await
                    .map(|_| user),
                None => None,
            };
            return Ok(own);
        };
        let not_found = || format!("Workflow '{id}' not found");
        let (_, visibility) = self
            .runtime
            .definition_ownership(id, Some(owner))
            .await
            .ok_or_else(not_found)?;
        self.write_auth
            .may_read(
                self.runtime.def_store().cloned(),
                workforce_orchestrator::workflow_store::KIND_WORKFLOW,
                id,
                Some(owner),
                visibility,
            )
            .await
            .then(|| Some(owner.to_string()))
            .ok_or_else(not_found)
    }

    /// Whether the caller's row sits beside a shared definition of the same
    /// id — the case every message about "your copy" describes.
    async fn shadows_shared(&self, id: &str, grant: &DefWriteGrant) -> bool {
        grant.owner.is_some() && self.runtime.definition_ownership(id, None).await.is_some()
    }

    /// Remove a definition the caller may write: their own row, or the
    /// shared one for an administrator. Deleting a row the caller does not
    /// have never reads as deleting the shared one.
    async fn handle_delete(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;
        let Some(workflow_id) = input.workflow_id.filter(|id| !id.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'workflow_id' — which workflow to delete?",
                elapsed(),
            ));
        };
        let grant = match self
            .write_auth
            .authorize(
                self.runtime.def_store().cloned(),
                workforce_orchestrator::workflow_store::KIND_WORKFLOW,
                &workflow_id,
                input.owner.as_deref().filter(|o| !o.is_empty()),
                DefOp::Delete,
            )
            .await
        {
            Ok(grant) => grant,
            Err(denied) => {
                return Ok(ToolResult::failure(
                    format!("Cannot delete workflow '{workflow_id}': {denied}"),
                    elapsed(),
                ));
            }
        };
        if self
            .runtime
            .definition_ownership(&workflow_id, grant.owner.as_deref())
            .await
            .is_none()
        {
            let message = if self.shadows_shared(&workflow_id, &grant).await {
                format!(
                    "You have no copy of '{workflow_id}' to delete; the shared '{workflow_id}' \
                     is the system's and only an administrator may delete it."
                )
            } else {
                format!("Workflow '{workflow_id}' not found")
            };
            return Ok(ToolResult::failure(message, elapsed()));
        }
        match self
            .runtime
            .delete_definition(
                &workflow_id,
                grant.owner.as_deref(),
                workforce_orchestrator::workflow_store::EditContext::new(
                    "agent_tool",
                    grant.actor.as_deref(),
                ),
            )
            .await
        {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({ "deleted": true, "workflow_id": workflow_id, "owner": grant.owner }),
                elapsed(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to delete workflow: {e:#}"),
                elapsed(),
            )),
        }
    }

    async fn handle_get(&self, input: WorkflowInput, start: Instant) -> Result<ToolResult> {
        let workflow_id = match input.workflow_id {
            Some(ref id) if !id.is_empty() => id.clone(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'workflow_id' — which workflow to retrieve?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let owner = match self
            .readable_owner(
                &workflow_id,
                input.owner.as_deref().filter(|o| !o.is_empty()),
            )
            .await
        {
            Ok(owner) => owner,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let toml_str = match self
            .runtime
            .get_definition_source(&workflow_id, owner.as_deref())
        {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{:#}", e),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match WorkflowDefinition::from_toml(&toml_str)
            .and_then(|def| serde_json::to_value(&def).map_err(|e| eyre::eyre!("{}", e)))
        {
            Ok(json_def) => Ok(ToolResult::success(
                self.runtime
                    .with_trigger_urls(
                        &def_key(owner.as_deref(), &workflow_id),
                        serde_json::json!({
                            "workflow_id": workflow_id,
                            "owner": owner,
                            "definition": json_def,
                        }),
                    )
                    .await,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Workflow '{}' failed to parse: {:#}", workflow_id, e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Where each of a saved workflow's HTTP triggers answers: the minted
/// address, which is unaffected by who owns the workflow, or the
/// owner-derived one on a hub that mints nothing. `None` for a definition
/// with no HTTP trigger, so the message stays silent about them.
fn http_trigger_sentence(trigger_urls: &[TriggerUrls]) -> Option<String> {
    let addresses: Vec<String> = trigger_urls
        .iter()
        .filter_map(|trigger| {
            trigger
                .url
                .as_ref()
                .or(trigger.legacy_url.as_ref())
                .map(|url| format!("'{}' at {url}", trigger.trigger_id))
        })
        .collect();
    let label = match addresses.len() {
        1 => "trigger",
        _ => "triggers",
    };
    (!addresses.is_empty()).then(|| format!(" HTTP {label}: {}.", addresses.join("; ")))
}

/// The structured definition the tool accepts; `decide_first` is offered
/// only where the install has the use on, so with it off every prompt that
/// carries the tool stays as it was.
fn definition_json_schema(decide_first: bool) -> serde_json::Value {
    let branch_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "when": { "type": "string", "description": "Semantic outcome label (e.g. 'approved', 'rejected', 'needs-work')" },
            "goto": { "type": "string", "description": "Target step ID to jump to" }
        },
        "required": ["when", "goto"]
    });

    let output_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Output field name" },
            "description": { "type": "string", "description": "What this output contains" },
            "type": { "type": "string", "enum": ["string", "number", "integer", "boolean", "list", "object"], "description": "The JSON type the value is read as (default 'string'). A value that does not fit sends the step back." },
            "required": { "type": "boolean", "description": "Whether the step must supply it (default true)" },
            "validate": { "type": "string", "enum": ["number", "not_empty"], "description": "Optional rule on the written text" }
        },
        "required": ["name"]
    });

    let mut step_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Unique step identifier" },
            "name": { "type": "string", "description": "Human-readable step name" },
            "task": { "type": "string", "description": "Natural language instruction for the agent, or a tool step's JSON input. Omit it on a step that only renders its template report. Use {{variable}} for context variables and {{steps.step_id.field}} for previous step outputs." },
            "branches": { "type": "array", "description": "Conditional routing — agent picks an outcome, engine routes to the target step", "items": branch_schema },
            "goto": { "type": "string", "description": "Unconditional jump to this step ID (cannot combine with exit)" },
            "pause": { "type": "string", "description": "Pause workflow here for human review. Value is the reason (e.g. 'awaiting-review')" },
            "exit": { "type": "boolean", "description": "Terminal step — workflow ends here (cannot combine with goto)" },
            "handoff": { "type": "string", "description": "On an exit step: leave a handoff package of this type on the run, holding the step's outputs" },
            "receives": { "type": "array", "items": { "type": "string" }, "description": "Step ids whose packages (outputs) this step is handed. When set, the step sees the run's inputs and those packages only — no other step's outputs and no earlier step's prose. Each must declare outputs and be able to run before this step; not for a step with phases." },
            "verify": { "description": "Have a verifier check this step's output against its task, its outputs and any criteria before the run moves on: true/false, or { criteria = [..], corrections = n }. Agent steps only. The verifier has no tools: each criterion must be one the step's outputs settle.", "oneOf": [
                { "type": "boolean" },
                { "type": "object", "properties": {
                    "criteria": { "type": "array", "items": { "type": "string" } },
                    "corrections": { "type": "integer", "minimum": 0, "maximum": 3 }
                } }
            ] },
            "skill": { "type": "string", "description": "Inject this SKILL.md content as additional context" },
            "agent": { "type": "string", "description": "Override which agent executes this step" },
            "model": { "type": "string", "description": "Override model for this step" },
            "max_rounds": { "type": "integer", "description": "Per-step round budget (overrides global)" },
            "outputs": { "type": "array", "description": "Declared structured outputs — extracted from agent response for use in subsequent steps", "items": output_schema },
            "report": { "type": "string", "description": "Human-facing report. With report_render = 'agent' (default) the model writes a REPORT block following this text as guidance; with 'template' the engine renders it as a Jinja template after the step completes and posts it verbatim — deterministic, no model." },
            "report_render": { "type": "string", "enum": ["agent", "template"], "description": "Who produces `report`: the model (default) or the engine. 'template' is required for a report on a tool step or on a step with no task, and may reference the step's own outputs." },
            "skills": { "type": "array", "items": { "type": "string" }, "description": "Skills to pre-load before this step (e.g. ['linear', 'github'])" },
            "preferred_tools": { "type": "array", "items": { "type": "string" }, "description": "Preferred tools in dot-notation (e.g. ['code.git', 'linear.issue_get'])" },
            "exclusive_tools": { "type": "boolean", "description": "When true, restrict agent to only preferred_tools + skill tools. Use for models struggling with large tool sets." },
            "engine": { "type": "string", "enum": ["default", "claude-code", "codex", "claude-pilot"], "description": "Execution engine. Default runs the normal agent loop." },
            "effort": { "type": "string", "enum": ["low", "medium", "high", "max"], "description": "Reasoning effort — claude-code, codex and claude-pilot engines only" },
            "tool": { "type": "string", "description": "Run a WASM skill tool directly instead of an agent turn: 'skill-name.tool_name'. The resolved task is its JSON input; the result's top-level fields become this step's outputs. Deterministic — no LLM. Cannot combine with pause or phases; a report on a tool step needs report_render = 'template'." },
            "decide": { "type": "object", "description": "Ask the install's decision model (Kev/Jev) instead of running an agent turn: no LLM, a few hundred ms. `task` is the state the question is asked of: keep it short (about 1,500 characters of text; pasted logs and code are collapsed to markers and longer text is cut to its opening), so pass a summary or a few fields, not a whole document. The answer's fields (label, probability, probabilities; score and level for kind 'score') become outputs; the winning option becomes the outcome when it is a declared branch label at or above the floor. Cannot combine with tool, pause or phases.", "properties": {
                "question": { "type": "string", "description": "The question, phrased so the state alone answers it" },
                "kind": { "type": "string", "enum": ["choice", "noul", "score"], "description": "choice (default): one of the options — branches carry option names; noul: yes/no — branches are 'yes'/'no'; score: a level on an ordered scale — branches carry level names" },
                "options": { "type": "object", "additionalProperties": { "type": "string" }, "description": "choice: option name → what it means (empty means the step's branch labels). noul: what 'yes' and 'no' mean — give these, a bare yes/no question is weak" },
                "levels": { "type": "array", "items": { "type": "string" }, "description": "score only: two to ten level names, lowest first" },
                "min_probability": { "type": "number", "description": "The winning option's probability must reach this to set an outcome (default: the install's [decision.branch_fallback] min_probability)" }
            }, "required": ["question"] },
            "workflow": { "type": ["string", "object"], "description": "Run another workflow as this step, no LLM: 'workflow-id', or { id, wait }. `task` is the child's trigger payload as a JSON object (empty = none). With wait (default true) the step blocks until the child ends and its final status — completed | failed | cancelled | exited — is the outcome (failed/cancelled halt this run unless a branch claims them); every output the child's steps produced is an output here, plus instance_id, status, failure_reason and steps. With wait = false the outcome is 'started' and instance_id is the output. Cannot combine with tool, decide, pause or phases.", "properties": {
                "id": { "type": "string" },
                "wait": { "type": "boolean" }
            } },
            "decision": { "type": "object", "description": "On a pause step with a delivered report: ask named people to choose what happens next, with one Slack button per choice. See action 'schema', Decisions.", "properties": {
                "title": { "type": "string", "description": "Card header, Jinja over the run's variables (default: the step name)" },
                "context": { "type": "string", "description": "Line under the header, Jinja over the run's variables" },
                "channel": { "type": "string", "description": "\"dm\", or a literal Slack channel id (C… or G…). Default: the run's thread, or each decider directly when the run has no channel" },
                "deciders": { "type": ["string", "array"], "items": { "type": "string" }, "description": "Identity ids, emails, team:<name> and <@U…> mentions, split on commas and newlines. Entries may read only the workflow's non-secret [[variables]]" },
                "allow_self_decision": { "type": "boolean", "description": "Whether the run's starter may decide (default true)" },
                "expires_after": { "type": "string", "description": "1m to 30d, e.g. '20h'; needs on_expiry" },
                "on_expiry": { "type": "string", "description": "The outcome of the button expiry presses" },
                "buttons": { "type": "array", "minItems": 1, "maxItems": 25, "items": { "type": "object", "properties": {
                    "label": { "type": "string", "description": "1 to 75 characters, unique ignoring case" },
                    "outcome": { "type": "string", "description": "[A-Za-z0-9_-]+, unique; recorded on the step" },
                    "style": { "type": "string", "enum": ["primary", "danger"] },
                    "done": { "type": "string", "description": "What the closed card says was done (default: the label)" },
                    "confirm": { "type": "string", "description": "A confirmation question shown before the press" },
                    "goto": { "type": "string", "description": "Route: run this step next" },
                    "end": { "type": "string", "enum": ["complete", "cancel"], "description": "Route: end the run" },
                    "workflow": { "type": ["string", "object"], "description": "Start another workflow, never waited on, before the route: 'id' or { id, payload }" },
                    "set": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Literal variables written when the decision settles" }
                }, "required": ["label", "outcome"] } }
            }, "required": ["deciders", "buttons"] },
            "persona": { "type": "string", "description": "Replace the identity block in the system prompt for this step" },
            "max_iterations": { "type": "integer", "description": "Cap how many times this step may run per instance before the outcome is forced" },
            "iteration_cap_outcome": { "type": "string", "description": "Outcome to force when max_iterations is hit; defaults to a branch whose when = 'stuck'" },
            "context_eviction_interval": { "type": "integer", "description": "Evict stale tool outputs every N rounds (default: disabled)" },
            "context_eviction_keep_last": { "type": "integer", "description": "Preserve this many recent tool outputs per type during eviction (default: 3)" }
        },
        "required": ["id", "name"]
    });
    if decide_first {
        step_schema["properties"]["decide_first"] = serde_json::json!({ "type": "object", "description": "Agent step with branches: ask the decision model for its outcome before the turn, and skip the turn when the answer reaches that option's floor. See action 'schema', Decide first.", "properties": {
                "state": { "type": "string" },
                "mode": { "type": "string", "enum": ["on", "shadow", "off"] },
                "question": { "type": "string" },
                "options": { "type": "object", "additionalProperties": { "type": "string" } },
                "escape": { "type": "boolean" },
                "min_probability": { "type": "number" },
                "floors": { "type": "object", "additionalProperties": { "type": ["number", "string"] } },
                "outputs": { "type": "object", "additionalProperties": { "type": "string" } }
            }, "required": ["state"] });
    }

    let context_field_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "required": { "type": "boolean", "description": "Whether this input is required (default: false)" },
            "description": { "type": "string", "description": "What this input is for" }
        }
    });

    let workflow_meta_schema = serde_json::json!({
        "type": "object",
        "description": "Workflow metadata",
        "properties": {
            "id": { "type": "string", "description": "Unique workflow identifier (kebab-case)" },
            "name": { "type": "string", "description": "Human-readable workflow name" },
            "description": { "type": "string", "description": "What this workflow does" },
            "version": { "type": "string", "description": "Workflow version (default: '1.0')" },
            "agent": { "type": "string", "description": "Default agent for all steps (overridable per step)" },
            "context": { "type": "object", "description": "Declared input parameters. Keys are variable names.", "additionalProperties": context_field_schema },
            "steerable_by_others": { "type": "boolean", "description": "Whether people other than a run's starter may steer it (resume, approve, reject, goto, skip, retry, pause, stop) when the workflow reaches them. Default true; the starter and administrators always may. Switching it off binds runs already under way." }
        },
        "required": ["id", "name"]
    });

    let trigger_schema = serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "description": "Webhook trigger: fires when a matching webhook event arrives.",
                "properties": {
                    "type": { "const": "webhook", "description": "Trigger kind. Defaults to 'webhook' if omitted." },
                    "source": { "type": "string", "description": "Webhook source (e.g. 'github', 'linear')" },
                    "event_type": { "type": "string", "description": "Event type (e.g. 'Issue.update', 'pull_request')" },
                    "match": { "type": "object", "description": "Dot-path conditions that must all match (AND logic).", "additionalProperties": { "type": "string" } }
                },
                "required": ["source", "event_type"]
            },
            {
                "type": "object",
                "description": "Schedule trigger: fires on a cron expression. Registered with the hub scheduler at workflow load time.",
                "properties": {
                    "type": { "const": "schedule" },
                    "cron": { "type": "string", "description": "5-field cron expression (UTC unless 'tz' is set)" },
                    "tz": { "type": "string", "description": "IANA timezone name (e.g. 'Europe/Berlin')" },
                    "run_as": { "type": "string", "description": "Identity to run the workflow as — sets CURRENT_USER_ID during execution so per-user tool paths resolve correctly" },
                    "payload": { "type": "object", "description": "Arbitrary payload merged into TriggerInfo.payload when the workflow starts" }
                },
                "required": ["type", "cron"]
            }
        ]
    });

    serde_json::json!({
        "type": "object",
        "description": "Structured JSON workflow definition (for 'validate' and 'save'). Alternative to 'toml_content'.",
        "properties": {
            "workflow": workflow_meta_schema,
            "step": { "type": "array", "description": "Workflow steps executed in sequence (with optional branching)", "items": step_schema },
            "trigger": { "type": "array", "description": "Triggers that auto-start this workflow (webhook or schedule)", "items": trigger_schema }
        },
        "required": ["workflow", "step"]
    })
}

const WORKFLOW_SCHEMA_REFERENCE: &str = r#"
# Workflow Definition Schema Reference

## Structure

A workflow definition has three top-level sections:
- `workflow` (required) — metadata and declared inputs
- `step` (required, array) — ordered steps the agent executes
- `trigger` (optional, array) — webhook and/or schedule triggers for auto-starting

## [workflow] section

| Field       | Type              | Required | Default | Description |
|-------------|-------------------|----------|---------|-------------|
| id          | string            | yes      |         | Unique identifier (kebab-case, e.g. "feature-impl") |
| name        | string            | yes      |         | Human-readable name |
| description | string            | no       | ""      | What this workflow does |
| version     | string            | no       | "1.0"   | Workflow version |
| agent       | string            | no       |         | Default agent for all steps (overridable per step) |
| context     | map of fields     | no       | {}      | Declared input parameters |
| delivery    | string            | no       | "full"  | "full" posts the plan banner, progress and every step's response; "reports" posts only what steps' reports say |
| max_concurrent | integer        | no       | 1       | Instances a schedule may have active at once; a slot that fires at the cap is skipped, so a workflow whose runs outlast its cadence needs more than one |
| failure_report | string         | no       |         | Jinja template rendered into every halt message, after the workflow name, step and instance id, so the channel sees what the run was about (the ticket, the PR). Sees the instance variables plus `step` and `error`; a value that does not exist yet renders empty rather than failing. Must not read a secret variable |
| pause_on_usage_cap | bool       | no       | false   | Hold the run instead of failing it when a step's last coding engine refuses because the subscription's usage window is spent. The instance pauses on that step and the maintenance sweep resumes it, re-running the step, once the window clears. A transient 429 is not a spent window and still fails the run. Held instances count against `max_concurrent`, so a workflow at the default cap of one admits no new scheduled run while a held one waits — the cadence stops until the subscription is back |
| steerable_by_others | bool      | no       | true    | Whether people other than the person who started a run may steer it — resume, approve or reject it, goto, skip or retry a step, pause or stop it — when the workflow reaches them (everyone's, their team's, or shared with them). The starter and administrators always may, and the run keeps acting as its starter whoever steers it. Switching it off, or making the workflow private, binds runs already under way; widening it does not open them |

### Context fields
Each key in `context` is a variable name. Value has:
- `required` (bool, default false) — whether the input must be provided
- `description` (string) — what this input is for

## [[step]] section (array of steps)

| Field                      | Type          | Required | Default | Description |
|----------------------------|---------------|----------|---------|-------------|
| id                         | string        | yes      |         | Unique step identifier |
| name                       | string        | yes      |         | Human-readable name |
| task                       | string        | no*      |         | Agent instruction, or a tool step's JSON input. *Required unless `tool` is set or the step is report-only |
| branches                   | array         | no       | []      | Conditional routing based on agent outcome |
| goto                       | string        | no       |         | Unconditional jump to step ID (cannot combine with exit) |
| pause                      | string        | no       |         | Pause here for human review (value = reason) |
| exit                       | bool          | no       | false   | Terminal step (cannot combine with goto) |
| handoff                    | string        | no       |         | On an exit step, leave a handoff package of this type on the run |
| skill                      | string        | no       |         | Inject SKILL.md content as additional context |
| agent                      | string        | no       |         | Override which agent runs this step |
| model                      | string        | no       |         | Override model for this step |
| max_rounds                 | integer       | no       |         | Per-step round budget |
| outputs                    | array         | no       | []      | Declared structured outputs: the step's package |
| receives                   | string array  | no       |         | Steps whose packages this step is handed, in place of every earlier step |
| verify                     | bool or table | no       |         | Check the step's output before the run moves on — see State packages below |
| report                     | string        | no       |         | Human-facing report — see Reports below |
| report_render              | string        | no       | "agent" | Who writes `report`: "agent" (the model) or "template" (engine-rendered Jinja) |
| tool                       | string        | no       |         | Run a WASM skill tool directly (`skill.tool`); `task` is its JSON input |
| decide_first               | table         | no       |         | Ask the decision model for the outcome before the agent's turn — see Decide first below |
| decision                   | table         | no       |         | On a pause step: named people choose what happens next with buttons — see Decisions below |
| skills                     | string array  | no       | []      | Skills to pre-load (e.g. ["linear", "github"]) |
| preferred_tools            | string array  | no       | []      | Preferred tools in dot-notation |
| exclusive_tools            | bool          | no       | false   | Restrict to preferred_tools + skill tools only |
| context_eviction_interval  | integer       | no       |         | Evict stale context every N rounds |
| context_eviction_keep_last | integer       | no       | 3       | Keep this many recent outputs per type |

### Branches
Each branch has:
- `when` (string, required) — semantic outcome label (e.g. "approved", "rejected")
- `goto` (string, required) — target step ID

### Outputs
Each output has:
- `name` (string, required) — field name
- `description` (string) — what this output contains
- `type` (string) — "string" (default), "number", "integer", "boolean", "list" or "object"
- `required` (bool, default true) — `false` lets the step leave it out
- `validate` (string, "number" or "not_empty") — optional rule on the written text

### State packages
A step's `outputs` are its package: the engine reads them from the step's
OUTPUTS block into their declared types and stores them with the run. A
missing or mistyped field sends the step back with what was wrong, up to 2
times, then stops the run.

`receives = ["extract", "design"]` hands a step those steps' packages and the
run's inputs, and nothing else from earlier steps. Each named step must declare
outputs and be able to run before this one; one that did not run this time is
listed as handing over nothing. A step with `phases` cannot use it. Without it
a step is shown every earlier step: its package, or the tail of its response
when it declares none.

`verify = true` (or `verify = { criteria = ["every route has a test"],
corrections = 2 }`) has a verifier check the output against the task, the
outputs and the criteria before the run moves on. A rejected output sends the
step back with the failing checks (once unless `corrections` or the agent's
`[handoff] max_corrections` says otherwise, at most 3 times); one still
rejected, or a verdict that cannot be read, stops the run. Left unset, the
step agent's `[handoff] verify` decides for steps that declare outputs.

The verifier has no tools and sees only the task, the outputs and the criteria.
Write each criterion so the outputs settle it, and declare the outputs it needs:
"there is a route for every table in the task" works; "the tests pass" does not
(declare `tests_passed` as a boolean output and require it to be true). A
criterion the verifier cannot settle counts as failed and stops the run.

## [[trigger]] section (optional, array)

Each trigger has a `type` field (defaults to "webhook" if omitted for back-compat).

### Webhook trigger (`type = "webhook"`)

| Field      | Type            | Required | Description |
|------------|-----------------|----------|-------------|
| type       | string          | no       | "webhook" (default) |
| source     | string          | yes      | Webhook source ("github", "linear", "hubspot") |
| event_type | string          | yes      | Event type (e.g. "Issue.update", "pull_request", "contact.creation") |
| match      | map str→str     | no       | Dot-path conditions, all must match (AND logic) |
| secret     | string          | yes      | `[[vault:<key>]]` marker; HMAC secret that verifies the payload |

Senders deliver to `{[hub].public_url}/webhooks/{source}`. HubSpot: the secret is the
app's client secret; `[hub].public_url` must be set because HubSpot signs the full request
URL; each event in a delivery batch is matched separately, with `event_type` taken from
its `subscriptionType` and the single event object as the workflow payload.

### Schedule trigger (`type = "schedule"`)

| Field   | Type   | Required | Description |
|---------|--------|----------|-------------|
| type    | string | yes      | "schedule" |
| cron    | string | yes      | 5-field cron expression (UTC unless `tz` is set) |
| tz      | string | no       | IANA timezone name (e.g. "Europe/Berlin") |
| run_as  | string | no       | Identity to run the workflow as — sets `CURRENT_USER_ID` during execution |
| payload | object | no       | Arbitrary payload merged into `TriggerInfo.payload` on start |

Example:
```toml
[[trigger]]
type = "schedule"
cron = "0 8 * * *"
tz = "Europe/Berlin"
run_as = "U01KINGS"
```

### HTTP trigger (`type = "http"`)

Addressed by `POST /triggers/t.{hex}` — a permanent path the hub mints for each
trigger and reports in `save`, `get` and `list` as `trigger_urls`. It is not derived
from who owns the workflow, so it survives the workflow changing hands. The
owner-derived forms it has always answered on keep answering: `POST /triggers/{id}`
for a system definition, `POST /triggers/{owner}/{id}` for a member-owned one. The
request body becomes the workflow payload.

| Field    | Type        | Required | Description |
|----------|-------------|----------|-------------|
| type     | string      | yes      | "http" |
| id       | string      | yes      | Endpoint id, globally unique, `[A-Za-z0-9_-]+` |
| match    | map str→str | no       | Dot-path conditions on the body, all must match |
| auth     | table       | yes      | Caller authentication (see below) |
| response | string      | no       | "streaming" (SSE, default) or "fire_forget" |

`[trigger.auth]` modes: `bearer` (default; `token` is a `[[vault:...]]` marker
compared against `Authorization: Bearer <token>`), `hmac` (`secret` is a
`[[vault:...]]` marker verifying an `X-Signature-256: sha256=<hex>` body HMAC),
and `oidc` (`issuer` + `audience` + non-empty `allowed_principals`; optional
`principal_claim` (default `sub`) and `run_as_caller`).

Example:
```toml
[[trigger]]
type = "http"
id = "deploy-notifier"
response = "fire_forget"
[trigger.auth]
mode = "bearer"
token = "[[vault:deploy/trigger-token]]"
```

## Validation Rules

1. Must have at least one step
2. Step IDs must be unique
3. Branch and step `goto` targets must reference existing step IDs
4. `engine` is `default`, `claude-code`, `codex` or `claude-pilot`; `validate` is `number` or `not_empty`
5. A step key matching no field is reported, not silently ignored
6. `tool = "skill-name.tool_name"` runs a WASM skill tool directly — no agent turn, and the
   result's top-level fields become the step's outputs. Use it for deterministic steps
7. A step with neither `task` nor `tool` is report-only: it needs `report_render = "template"`
   with a `report`, declares no outputs or phases, cannot override `engine`, and may have
   branches only with `pause`
8. `report_render = "template"` requires a `report`, and its Jinja must compile
9. A step cannot have both `exit = true` and `goto`
10. Every webhook trigger must declare a `secret`; every HTTP trigger must declare `auth`
11. HTTP trigger ids must be unique across all loaded workflows

## Run identity

An automated run (webhook, schedule, HTTP trigger) of a member-owned workflow
executes as its **run identity**: the owner, or the identity named by a save
with `run_as = "me"` — only the saver can name themselves, since the run uses
their credentials and grants. Automation pauses (the `list` action reports
`automated_runs_blocked` with the reason) whenever the last save was by someone
other than the run identity, the run identity lost its manage share, or it is
inactive. The run identity re-saving, or the owner saving with `run_as = "owner"`,
resumes it. Interactive starts always run as the caller.

## Template Variables

Use `{{variable}}` syntax in step `task` fields:
- `{{variable_name}}` — a context field; DECLARE it in `[workflow.context]` or `[[variables]]` first
- `{{steps.step_id.output_name}}` — an output of a step that runs on EVERY path to here
- `{{?anything}}` — optional: renders empty instead of failing when absent
- `{{_worktree_path}}` / `{{_channel_id}}` / `{{_thread_id}}` — engine-supplied, never declared, may be absent

A reference that cannot resolve fails the step BEFORE the agent runs. Saving is
refused if a reference names a step some path skips — write `{{?...}}` when you
mean "may be absent".

Coding steps on a remote executor share one scope on the coding worker per
instance. A prior step's `target_repo` output (`owner/repo`) is checked out at
`/work/<repo>` and rounds start there; a `workspace_repos` output (`owner/repo`
entries, whitespace or comma separated) adds more checkouts beside it, target
first, repeats dropped; two owners' same-named repos are refused. Output both
BEFORE the first coding step: later steps reuse the scope unchanged. Each repo is
fetched with its owner's GitHub token, and rounds get the target owner's. The
local executor checks out the target alone.

## Reports

A step's `report` is what the channel sees. `report_render` decides who writes it:

- `"agent"` (default) — the engine resolves the `{{…}}` references and asks the model to
  end with a REPORT:/END_REPORT block following that text as guidance. The block is what
  posts, so wording varies run to run. A model-written report on a `tool` step is never
  produced (no model runs) and is flagged at save.
- `"template"` — the engine renders `report` itself as a Jinja template after the step
  completes and posts the result verbatim. Deterministic, no model turn; legal on tool
  steps and on report-only steps. The step's own outputs are visible
  (`{{ steps.<this step>.field }}`).

Template syntax (Jinja): `{{ steps.ticket.results | length }}`,
`{% for t in steps.ticket.results %}• {{ t.key }}{% else %}• none{% endfor %}`,
`{% if steps.fetch.rate_limited %}…{% endif %}`. Outputs travel as text; a template sees
a string that is a JSON array/object or a boolean word (`true`/`false`, any case) as that
value, so it can loop, index and test them. Numbers and prose stay text — use `| int`
before comparing or doing arithmetic, and only loop over an output that is a JSON array
(looping over plain text walks its characters). Printing or iterating a missing value
fails the step, naming the line and the unresolved path. A step some path skips is the
template's to guard: test the step itself, `{% if steps.b %}{{ steps.b.x }}{% endif %}` —
reading an attribute of a missing value fails even inside `if`. Block tags on their own
lines leave no blank lines. Syntax errors are refused at save.

Under `delivery = "reports"` a run posts exactly the reports its steps deliver; a
rendered report also posts under `delivery = "full"`, verbatim.

## Design Patterns

### Linear pipeline
Steps execute in order, last step has `exit = true`.

### Branch-and-converge
A step declares branches with different outcomes routing to different steps.
Multiple branches can converge back to a shared step.

**Declaring `branches` removes the step's fall-through.** Its ONLY exits are the
branch targets; finishing without a matching outcome stops the workflow. If the
branches are exception paths and the next step is the happy path, say so with
`goto`:

```toml
[[step]]
id = "prepare"
goto = "merge"                                                    # default path
branches = [{ when = "gates-failed", goto = "request-changes" }]  # exceptions
```

Without that `goto`, `merge` is unreachable and the workflow can never finish.

### Decide first
An agent step that mostly reaches the same safe outcome — a triage step that
ignores nine tickets in ten — can let the install's decision model settle that
outcome before the turn runs, and pay for the turn only on the rest:

```toml
[[step]]
id = "classify"
name = "Classify the ticket"
task = """
Classify the ticket.
A crash or a wrong result: OUTCOME: bug
Customers blocked: OUTCOME: urgent
"""
outputs = [{ name = "category" }, { name = "reason" }]
branches = [{ when = "bug", goto = "fix" }, { when = "urgent", goto = "page" }]
goto = "close"

[step.decide_first]
state = '{ "subject": "{{subject}}", "body": "{{body}}" }'
options = { ignore = "A newsletter, an auto-reply or spam: nothing to do" }
min_probability = 0.9
floors = { urgent = "never", bug = 0.97 }
outputs = { category = "{{decision.label}}", reason = "Settled by the decision model at {{decision.probability}}" }
```

- `state` is the few fields the outcome is decided from, with `{{variables}}`
  (JSON passes as structure); not the task, which is instructions. About
  1,500 characters together with the question.
- The options are the branch labels (meaning what the task says of each) plus
  any named in `options`; a name that is no branch settles onto `goto`. Every
  option needs a meaning: a branch no line of the task names must be described
  in `options`, or the definition is refused.
- The question also offers `none-of-these`, which never settles. Where the
  options already cover every route — the branches and exactly one option for
  the default route — `escape = false` leaves it out, so the model divides its
  answer between the real outcomes; an unsure answer still falls short of the
  floor, which must then be above 0.5. Set `question` when the stock wording does not fit the state.
- `outputs` fills each declared output when the model settles the step, from
  `{{decision.label}}`, `{{decision.probability}}`, `{{decision.backend}}`,
  `{{decision.model}}` and the run's variables.
- The step settles only when the chosen option reaches its own floor;
  `"never"` keeps an outcome for the agent alone. Set floors asymmetrically:
  the common, cheap outcome at a high floor, the rare, costly one never.
- A settled step completes with the block's `outputs` as its package (every
  required output needs a template) and its outcome on the record as the
  decision model's (`outcome_by = "decision_model"`, with `decided_first.settled`
  true); its engine is `decision`, its cost 0. A
  model-written `report` is not written for it: its one-line response is
  posted instead, so prefer `report_render = "template"` on such a step.
- Anything else — below the floor, the escape answer, a busy or slow model,
  no key, the use switched off — runs the turn as if the block were absent.
- `mode = "shadow"` asks and records but always runs the turn; the step's
  `decided_first` says what the model would have settled and whether the agent
  agreed. Prove a step in shadow before turning it on.
- The install's `[decision.uses.decide_first] mode` (off by default) and the
  agent's own `config.toml` cap how far any step goes.

### Pause-for-approval
Set `pause = "awaiting-review"` on a step. The workflow pauses until a human
resumes (with an outcome) or rejects (with feedback). Combine with a `report`
template for clean Slack notifications. To name who decides and give them
buttons, add a `decision` (see Decisions below).

### Decisions (buttons people press)
`decide` lets a model choose a branch; `decision` asks people. A pause step
with a delivered report (the card's body) may carry `[step.decision]`:

```toml
[[step]]
id = "review"
name = "Review the digest"
report_render = "template"
report = "To: {{ steps.draft.recipient }}"
pause = "awaiting-approval"
branches = [{ when = "send", goto = "send_email" }]

[step.decision]
title = "Send today's digest to {{ steps.draft.recipient }}?"
deciders = ["{{ approvers }}", "<@U0123ABCD>"]
allow_self_decision = false
expires_after = "20h"
on_expiry = "cancel"

[[step.decision.buttons]]
label = "Send"
outcome = "send"
style = "primary"
confirm = "Send this email now?"

[[step.decision.buttons]]
label = "Redraft"
outcome = "redraft"
goto = "draft"

[[step.decision.buttons]]
label = "Cancel"
outcome = "cancel"
style = "danger"
end = "cancel"
```

- **Fields:** `title` and `context` (Jinja over the run's variables), `channel`
  (`"dm"` or a literal `C…`/`G…` id), `deciders` (required), `allow_self_decision`
  (default true), `expires_after` with `on_expiry` (together; 1m to 30d), and 1
  to 25 `buttons`.
- **Routes:** each button has exactly one: a branch whose `when` equals its
  `outcome`, its own `goto`, or `end = "complete" | "cancel"`. A decision step
  has no step-level `goto`, is not an exit, and never falls through.
- **Effects:** `set = { key = "literal" }` writes variables when the decision
  settles (no `_` keys, no `steps`, no secrets, no templates). `workflow = "id"`
  or `{ id, payload = { … } }` starts another workflow before the route, never
  waited on (`wait = true` is refused) and never with `end = "cancel"`.
- **Deciders:** identity ids, emails, `team:<name>` and `<@U…>` mentions,
  split on commas and newlines. An entry may read only the workflow's own
  non-secret `[[variables]]`, so no payload can choose who decides. They are
  resolved when the step pauses; suspended people are left out.
- **Delivery:** no `channel` posts into the run's thread (or by DM when the run
  has none); `channel = "C…"` posts there with a pointer in the run's thread
  (the run's own channel posts into its thread);
  `"dm"` sends each decider their own card. A card that cannot be posted fails
  the step with the fix to make.
- **Who may decide:** only the named deciders, on every surface — the buttons,
  the button's label typed in the run's thread, the REST resume and this tool's
  `resume` with `outcome` (and the `gate` from `status`). Neither the starter
  nor an administrator decides unless named, and an agent with no person behind
  it never does. While a decision is open nobody may resume, reject, goto, skip,
  retry or pause the run, and only the starter or an administrator may stop it.
- **Expiry:** past `expires_after` the hub presses the `on_expiry` button
  within about a minute.

### Loop-back on rejection
A review step branches: "approved" → next step, "rejected" → back to the
work step. The agent receives rejection feedback in its context.

### Exit with handoff
Set `exit = true` and `handoff = "escalation_type"` to leave a handoff package
on the run: `{ type, step, package, summary }`, where `package` is the exit
step's own outputs. `status` returns it as `handoff`.

### Report-only step
A step with neither `task` nor `tool` runs no model: it renders its template
report and moves on. Pair it with `delivery = "reports"` for scheduled runs
whose only output is a channel message built from earlier steps:

```toml
[[step]]
id = "report"
name = "Report"
exit = true
report_render = "template"
report = """
*Triage — last 24h*
{% for t in steps.ticket.results %}
• {{ t.key }} — {{ t.action }}
{% else %}
• nothing new
{% endfor %}
"""
```

It may `pause` (post, then wait for `approve`/`reject`); `branches` are allowed
only with `pause`, since a human is then the one choosing the outcome.

## Example

{
  "workflow": {
    "id": "pr-review-merge",
    "name": "PR Review & Merge",
    "description": "Review a PR, request changes if needed, merge on approval",
    "context": {
      "pr_number": { "required": true, "description": "Pull request number" },
      "repo": { "required": true, "description": "Repository in owner/repo format" }
    }
  },
  "step": [
    {
      "id": "review",
      "name": "Review PR",
      "task": "Review PR #{{pr_number}} in {{repo}}. Analyze the diff, check for bugs, style issues, and test coverage.\n\nOUTCOME: approved | changes-requested",
      "skills": ["github"],
      "preferred_tools": ["github.pr_diff", "github.get_file"],
      "outputs": [
        { "name": "review_summary", "description": "Summary of findings" }
      ],
      "branches": [
        { "when": "approved", "goto": "merge" },
        { "when": "changes-requested", "goto": "request-changes" }
      ]
    },
    {
      "id": "request-changes",
      "name": "Request Changes",
      "task": "Post a review comment on PR #{{pr_number}} requesting changes based on: {{steps.review.review_summary}}",
      "pause": "awaiting-author-update",
      "report": "Changes requested on PR #{{pr_number}}: {{steps.review.review_summary}}",
      "branches": [
        { "when": "updated", "goto": "review" },
        { "when": "closed", "goto": "done" }
      ]
    },
    {
      "id": "merge",
      "name": "Merge PR",
      "task": "Merge PR #{{pr_number}} in {{repo}}.",
      "skills": ["github"],
      "goto": "done"
    },
    {
      "id": "done",
      "name": "Done",
      "task": "Report final status of PR #{{pr_number}}.",
      "exit": true
    }
  ]
}

## Design Procedure

1. Call `workflow { action: "schema" }` to get this reference (you already have it)
2. Understand the user's requirements — ask clarifying questions
3. Design the step flow: identify steps, branching logic, pause points
4. Generate the definition as a JSON object
5. Call `workflow { action: "validate", definition: {...} }` to check for errors
6. If validation fails, fix the errors and re-validate
7. Present the design to the user for review
8. Call `workflow { action: "save", workflow_id: "...", definition: {...} }` to persist
9. Optionally test with `workflow { action: "start", workflow_id: "...", payload: {...} }`
10. Use `workflow { action: "get", workflow_id: "..." }` to retrieve and modify existing workflows
"#;

#[cfg(test)]
mod tests {
    use super::{
        failure_reason, http_trigger_sentence, instances_payload, last_step_summary,
        step_history_summary, TriggerUrls, WorkflowInstanceRow, OUTPUT_VALUE_CAP,
        STEPS_OUTPUT_BUDGET,
    };

    fn urls(trigger_id: &str, url: Option<&str>, legacy_url: Option<&str>) -> TriggerUrls {
        TriggerUrls {
            trigger_id: trigger_id.to_string(),
            url: url.map(str::to_string),
            legacy_url: legacy_url.map(str::to_string),
        }
    }

    /// A save names the minted address, which is what a caller should be
    /// configured with — falling back to the owner-derived one only where
    /// nothing was minted, and staying silent when there is no HTTP trigger.
    #[test]
    fn a_save_names_the_address_a_caller_should_use() {
        let minted = urls(
            "deploy",
            Some("https://hub/triggers/t.abc"),
            Some("https://hub/triggers/deploy"),
        );
        let sentence = http_trigger_sentence(&[minted]).expect("an HTTP trigger is named");
        assert_eq!(
            sentence,
            " HTTP trigger: 'deploy' at https://hub/triggers/t.abc."
        );

        let unminted = urls("deploy", None, Some("https://hub/triggers/deploy"));
        assert_eq!(
            http_trigger_sentence(&[unminted]).as_deref(),
            Some(" HTTP trigger: 'deploy' at https://hub/triggers/deploy.")
        );

        let two = [
            urls("a", Some("https://hub/triggers/t.aaa"), None),
            urls("b", Some("https://hub/triggers/t.bbb"), None),
        ];
        assert_eq!(
            http_trigger_sentence(&two).as_deref(),
            Some(" HTTP triggers: 'a' at https://hub/triggers/t.aaa; 'b' at https://hub/triggers/t.bbb.")
        );

        assert_eq!(http_trigger_sentence(&[]), None);
        assert_eq!(http_trigger_sentence(&[urls("gone", None, None)]), None);
    }

    #[test]
    fn instances_payload_cuts_to_the_limit_and_reports_the_total() {
        let row = |id: &str| WorkflowInstanceRow {
            id: id.to_string(),
            workflow_id: "wf".to_string(),
            status: "completed".to_string(),
            current_step_id: None,
            step_history: "[]".to_string(),
            variables: "{}".to_string(),
            trigger_json: "{}".to_string(),
            agent_id: None,
            created_at: String::new(),
            updated_at: String::new(),
            parent_instance_id: None,
            channel_id: None,
            thread_id: None,
            ..Default::default()
        };
        let rows = [row("newest"), row("older"), row("oldest")];
        let payload = instances_payload(&rows, 2);
        assert_eq!(payload["count"], 2);
        assert_eq!(payload["total"], 3);
        assert_eq!(payload["instances"][0]["instance_id"], "newest");
        assert_eq!(payload["instances"][1]["instance_id"], "older");
        assert_eq!(instances_payload(&rows, 10)["count"], 3);
    }

    #[test]
    fn a_step_summary_says_what_the_model_settled_first_and_nothing_more_for_any_other_step() {
        let history = r#"[{"step_id":"classify","outcome":"ignore","agent_response":"settled",
            "rounds":0,"cost_usd":0.0,"engine":"decision","model":"kev",
            "outcome_by":"decision_model","decided_first":{"settled":true,"answer":"kev ignore p=0.93"}},
            {"step_id":"review","outcome":"approved","agent_response":"ok","rounds":2,"cost_usd":0.04,
            "outcome_by":"outcome_line"}]"#;
        let steps = step_history_summary(history);
        assert_eq!(
            steps[0]["decided_first"],
            serde_json::json!({"settled": true, "answer": "kev ignore p=0.93"})
        );
        assert_eq!(steps[0]["engine"], "decision");
        let review = steps[1].as_object().unwrap();
        assert!(
            !review.contains_key("decided_first") && !review.contains_key("outcome_by"),
            "a step without the block reads as it did: {review:?}"
        );
    }

    #[test]
    fn failure_reason_reads_the_recorded_reason() {
        let vars = r#"{"_failure_reason":"cannot resolve template variable(s)"}"#;
        assert_eq!(
            failure_reason(vars).as_deref(),
            Some("cannot resolve template variable(s)")
        );
    }

    #[test]
    fn failure_reason_is_absent_for_a_healthy_or_unparseable_instance() {
        assert!(failure_reason(r#"{"steps":{}}"#).is_none());
        assert!(failure_reason("not json").is_none());
    }

    #[test]
    fn last_step_summary_is_bounded_and_names_the_step() {
        let history = r#"[{"step_id":"fetch","outcome":null,"agent_response":"...","rounds":3,
            "cost_usd":0.02,"completed_at":"2026-08-26T07:00:00Z"},
            {"step_id":"report","outcome":"done","agent_response":"a very long response",
             "rounds":1,"cost_usd":0.01,"completed_at":"2026-08-26T07:00:10Z"}]"#;
        let summary = last_step_summary(history).unwrap();
        assert_eq!(summary["step_id"], "report");
        assert_eq!(summary["outcome"], "done");
        assert!(
            summary.get("agent_response").is_none(),
            "the full response must not ride along in a tool result"
        );
    }

    #[test]
    fn a_failed_attempt_reads_as_failed_in_both_summaries() {
        let history = r#"[{"step_id":"implement","outcome":null,"rounds":12,"cost_usd":1.5,
            "completed_at":"2026-09-13T08:00:00Z","engine":"claude-code"},
            {"step_id":"implement","outcome":null,"rounds":3,"cost_usd":0.4,
             "completed_at":"2026-09-13T09:00:00Z","engine":"claude-code","failed":true}]"#;
        assert_eq!(last_step_summary(history).unwrap()["failed"], true);
        let steps = step_history_summary(history);
        assert_eq!(steps[0]["failed"], false);
        assert_eq!(steps[1]["failed"], true);
    }

    #[test]
    fn last_step_summary_is_absent_without_history() {
        assert!(last_step_summary("[]").is_none());
        assert!(last_step_summary("not json").is_none());
    }

    #[test]
    fn step_history_summary_carries_every_step_with_bounded_outputs() {
        let long = "x".repeat(5_000);
        let history = serde_json::json!([
            {"step_id":"pick","outcome":null,"agent_response":"…","rounds":1,"cost_usd":0.0,
             "started_at":"2026-09-13T08:46:25Z","completed_at":"2026-09-13T08:46:29Z",
             "engine":"wasm","outputs":{"ticket_id":"ENG-1","labels":["a","b"]}},
            {"step_id":"review","outcome":"fix","agent_response":"…","rounds":40,"cost_usd":4.9,
             "started_at":"2026-09-13T08:46:29Z","completed_at":"2026-09-13T08:55:00Z",
             "engine":"claude-code","model":"opus","outputs":{"summary":long,"verdict":"fix"}}
        ])
        .to_string();
        let steps = step_history_summary(&history);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["step_id"], "pick");
        assert_eq!(steps[0]["engine"], "wasm");
        assert_eq!(steps[0]["outputs"]["ticket_id"], "ENG-1");
        assert_eq!(steps[0]["outputs"]["labels"][1], "b");
        assert_eq!(steps[1]["model"], "opus");
        assert_eq!(steps[1]["outputs"]["verdict"], "fix");
        let summary = steps[1]["outputs"]["summary"].as_str().unwrap();
        assert_eq!(summary.chars().count(), OUTPUT_VALUE_CAP + 1);
        assert!(summary.ends_with('…'));
        assert!(steps[1].get("agent_response").is_none());
        assert!(step_history_summary("not json").is_empty());
    }

    #[test]
    fn step_history_summary_keeps_the_newest_outputs_within_the_total_budget() {
        let big = "y".repeat(OUTPUT_VALUE_CAP);
        let steps: Vec<serde_json::Value> = (0..20)
            .map(|i| {
                serde_json::json!({"step_id": format!("s{i}"), "outputs": {"summary": big, "findings": big}})
            })
            .collect();
        let summary = step_history_summary(&serde_json::Value::Array(steps).to_string());
        assert_eq!(summary.len(), 20);
        let kept: Vec<bool> = summary.iter().map(|s| !s["outputs"].is_null()).collect();
        assert!(kept.last().copied().unwrap());
        assert!(!kept.first().copied().unwrap());
        let first_kept = kept.iter().position(|k| *k).unwrap();
        assert!(
            kept[first_kept..].iter().all(|k| *k),
            "kept outputs are the newest run"
        );
        assert_eq!(summary[0]["outputs_omitted"], true);
        assert!(summary[19].get("outputs_omitted").is_none());
        assert_eq!(summary[0]["step_id"], "s0");
        let total: usize = summary
            .iter()
            .filter(|s| !s["outputs"].is_null())
            .map(|s| s["outputs"].to_string().len())
            .sum();
        assert!(total <= STEPS_OUTPUT_BUDGET);
    }

    use super::*;
    use std::time::Instant;

    fn make_input(
        definition: Option<serde_json::Value>,
        toml_content: Option<String>,
    ) -> WorkflowInput {
        WorkflowInput {
            action: String::new(),
            workflow_id: None,
            payload: None,
            instance_id: None,
            outcome: None,
            gate: None,
            channel_id: None,
            thread_id: None,
            status_filter: None,
            reason: None,
            feedback: None,
            definition,
            toml_content,
            owner: None,
            visibility: None,
            run_as: None,
            limit: None,
        }
    }

    #[test]
    fn resolve_toml_from_json_definition() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "test-wf", "name": "Test" },
                "step": [{ "id": "s1", "name": "Step 1", "task": "Do it." }]
            })),
            None,
        );
        let toml = WorkflowTool::resolve_toml_content(&input).unwrap();
        assert!(toml.contains("id = \"test-wf\""));
        assert!(toml.contains("id = \"s1\""));
    }

    #[test]
    fn resolve_toml_from_raw_toml() {
        let raw = r#"
[workflow]
id = "raw"
name = "Raw"

[[step]]
id = "s1"
name = "S1"
task = "Do it."
"#;
        let input = make_input(None, Some(raw.to_string()));
        let toml = WorkflowTool::resolve_toml_content(&input).unwrap();
        assert_eq!(toml, raw);
    }

    #[test]
    fn resolve_toml_json_takes_priority() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "json-wins", "name": "JSON" },
                "step": [{ "id": "s1", "name": "S1", "task": "Do it." }]
            })),
            Some("ignored".to_string()),
        );
        let toml = WorkflowTool::resolve_toml_content(&input).unwrap();
        assert!(toml.contains("id = \"json-wins\""));
    }

    #[test]
    fn resolve_toml_neither_provided() {
        let input = make_input(None, None);
        let err = WorkflowTool::resolve_toml_content(&input).unwrap_err();
        assert!(err.contains("Provide either"));
    }

    #[test]
    fn resolve_toml_invalid_json() {
        let input = make_input(Some(serde_json::json!({ "bad": true })), None);
        let err = WorkflowTool::resolve_toml_content(&input).unwrap_err();
        assert!(err.contains("Invalid workflow definition JSON"));
    }

    #[test]
    fn validate_valid_definition() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "v", "name": "Valid" },
                "step": [
                    { "id": "a", "name": "A", "task": "Do A." },
                    { "id": "b", "name": "B", "task": "Do B.", "exit": true }
                ]
            })),
            None,
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(result.success);
        assert_eq!(result.output["valid"], true);
        assert_eq!(result.output["workflow_id"], "v");
        assert_eq!(result.output["step_count"], 2);
        assert_eq!(result.output["step_ids"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn validate_invalid_branch_target() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "bad", "name": "Bad" },
                "step": [{
                    "id": "s1", "name": "S1", "task": "Go.",
                    "branches": [{ "when": "done", "goto": "nonexistent" }]
                }]
            })),
            None,
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(
            !result.success,
            "a definition that will not run must fail the call, not report success"
        );
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("nonexistent"));
    }

    #[test]
    fn validate_empty_steps() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "empty", "name": "Empty" },
                "step": []
            })),
            None,
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no steps"));
    }

    #[test]
    fn validate_raw_toml() {
        let input = make_input(
            None,
            Some(
                r#"
[workflow]
id = "toml-test"
name = "TOML Test"

[[step]]
id = "s1"
name = "S1"
task = "Do it."
"#
                .to_string(),
            ),
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(result.success);
        assert_eq!(result.output["valid"], true);
        assert_eq!(result.output["workflow_id"], "toml-test");
    }

    #[test]
    fn validate_invalid_raw_toml() {
        let input = make_input(None, Some("not valid toml {{{".to_string()));
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(!result.success);
    }

    #[test]
    fn validate_fails_and_names_every_authoring_issue() {
        let input = make_input(
            Some(serde_json::json!({
                "workflow": { "id": "stranded", "name": "Stranded" },
                "step": [
                    { "id": "prepare", "name": "P", "task": "Go.",
                      "branches": [{ "when": "failed", "goto": "prepare" }] },
                    { "id": "merge", "name": "M", "task": "Go.", "exit": true }
                ]
            })),
            None,
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("'merge' is unreachable"), "{error}");
        assert!(error.contains("goto = \"merge\""), "{error}");
    }

    #[test]
    fn schema_returns_reference() {
        let result = WorkflowTool::handle_schema(Instant::now());
        assert!(result.success);
        let reference = result.output["schema_reference"].as_str().unwrap();
        assert!(reference.contains("Workflow Definition Schema Reference"));
        assert!(reference.contains("Template Variables"));
        assert!(reference.contains("Design Patterns"));
        assert!(reference.contains("Validation Rules"));
    }

    #[test]
    fn the_references_decide_first_example_is_a_step_that_saves() {
        let example = WORKFLOW_SCHEMA_REFERENCE
            .split("### Decide first")
            .nth(1)
            .and_then(|section| section.split("```toml\n").nth(1))
            .and_then(|block| block.split("```").next())
            .expect("an example");
        let def = WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"triage\"\nname = \"Triage\"\n\
             context = {{ subject = {{ required = true }}, body = {{ required = true }} }}\n\n\
             {example}\n\
             [[step]]\nid = \"fix\"\nname = \"Fix\"\ntask = \"t\"\nexit = true\n\n\
             [[step]]\nid = \"page\"\nname = \"Page\"\ntask = \"t\"\nexit = true\n\n\
             [[step]]\nid = \"close\"\nname = \"Close\"\ntask = \"t\"\nexit = true\n"
        ))
        .unwrap();
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
        assert!(def.steps[0].decide_first.is_some());
        let step = &definition_json_schema(true)["properties"]["step"]["items"];
        assert_eq!(
            step["properties"]["decide_first"]["required"],
            serde_json::json!(["state"])
        );
        let off = &definition_json_schema(false)["properties"]["step"]["items"];
        assert!(
            off["properties"].get("decide_first").is_none(),
            "with the use off the tool's schema is as it was"
        );
    }

    #[test]
    fn the_schema_offers_decision() {
        let step = &definition_json_schema(false)["properties"]["step"]["items"]["properties"];
        assert_eq!(
            step["decision"]["required"],
            serde_json::json!(["deciders", "buttons"])
        );
        let button = &step["decision"]["properties"]["buttons"]["items"]["properties"];
        for key in [
            "label", "outcome", "style", "done", "confirm", "goto", "end", "workflow", "set",
        ] {
            assert!(button[key].is_object(), "a button lists '{key}'");
        }
        assert!(WORKFLOW_SCHEMA_REFERENCE.contains("### Decisions (buttons people press)"));
        assert!(WORKFLOW_SCHEMA_REFERENCE.contains("| decision "));
        let example = WORKFLOW_SCHEMA_REFERENCE
            .split("### Decisions (buttons people press)")
            .nth(1)
            .and_then(|section| section.split("```toml\n").nth(1))
            .and_then(|block| block.split("```").next())
            .expect("an example");
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"daily-digest-email\"\nname = \"Digest\"\n\n\
             [[variables]]\nid = \"approvers\"\nvalue = \"approver.one@example.com\"\n\n\
             [[step]]\nid = \"draft\"\nname = \"Draft\"\ntask = \"t\"\n\
             outputs = [{{ name = \"recipient\" }}]\n\n\
             {example}\n\
             [[step]]\nid = \"send_email\"\nname = \"Send\"\ntask = \"t\"\nexit = true\n"
        ))
        .expect("the reference's example saves");
    }

    #[test]
    fn validate_refuses_a_bad_decision() {
        let input = make_input(
            None,
            Some(
                "[workflow]\nid = \"digest\"\nname = \"Digest\"\n\n\
                 [[step]]\nid = \"review\"\nname = \"Review\"\ntask = \"\"\n\
                 report = \"r\"\nreport_render = \"template\"\npause = \"awaiting-approval\"\n\n\
                 [step.decision]\ndeciders = \"approver.one@example.com\"\n\n\
                 [[step.decision.buttons]]\nlabel = \"Send\"\noutcome = \"send\"\n"
                    .to_string(),
            ),
        );
        let result = WorkflowTool::handle_validate(input, Instant::now());
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("needs exactly one route"), "{error}");
    }

    #[test]
    fn definition_schema_has_required_structure() {
        let schema = definition_json_schema(false);
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["workflow"].is_object());
        assert!(schema["properties"]["step"].is_object());
        assert!(schema["properties"]["trigger"].is_object());
        assert_eq!(schema["required"], serde_json::json!(["workflow", "step"]));
    }

    #[test]
    fn json_definition_roundtrips_through_toml() {
        let def_json = serde_json::json!({
            "workflow": {
                "id": "roundtrip",
                "name": "Roundtrip Test",
                "description": "Test roundtrip",
                "context": {
                    "ticket_id": { "required": true, "description": "The ticket" }
                }
            },
            "step": [
                {
                    "id": "investigate",
                    "name": "Investigate",
                    "task": "Look into {{ticket_id}}.",
                    "max_rounds": 50,
                    "outputs": [{ "name": "summary", "description": "Findings" }],
                    "branches": [
                        { "when": "fixed", "goto": "done" },
                        { "when": "needs-work", "goto": "implement" }
                    ]
                },
                {
                    "id": "implement",
                    "name": "Implement",
                    "task": "Fix it.",
                    "goto": "done"
                },
                {
                    "id": "done",
                    "name": "Done",
                    "task": "Report.",
                    "exit": true
                }
            ]
        });

        let input = make_input(Some(def_json), None);
        let toml = WorkflowTool::resolve_toml_content(&input).unwrap();

        let parsed = WorkflowDefinition::from_toml(&toml).unwrap();
        assert_eq!(parsed.workflow.id, "roundtrip");
        assert_eq!(parsed.steps.len(), 3);
        assert_eq!(parsed.steps[0].branches.len(), 2);
        assert_eq!(parsed.steps[0].max_rounds, Some(50));
        assert_eq!(parsed.steps[0].outputs[0].name, "summary");
        assert!(parsed.workflow.context["ticket_id"].required);
    }

    #[test]
    fn the_definition_schema_and_a_saved_definition_carry_the_package_grammar() {
        use workforce_orchestrator::workflow::package::FieldType;

        let schema = definition_json_schema(false);
        let step = &schema["properties"]["step"]["items"]["properties"];
        let output = &step["outputs"]["items"]["properties"];
        for key in ["name", "description", "type", "required", "validate"] {
            assert!(output[key].is_object(), "outputs lists '{key}'");
        }
        for kind in output["type"]["enum"].as_array().unwrap() {
            serde_json::from_value::<FieldType>(kind.clone())
                .unwrap_or_else(|e| panic!("the schema offers a type nothing reads: {kind}: {e}"));
        }
        assert_eq!(output["type"]["enum"].as_array().unwrap().len(), 6);
        assert!(step["receives"].is_object() && step["verify"].is_object());

        let definition = |kind: &str| {
            make_input(
                Some(serde_json::json!({
                    "workflow": { "id": "packaged", "name": "Packaged" },
                    "step": [
                        {
                            "id": "extract",
                            "name": "Extract",
                            "task": "Extract the schema.",
                            "outputs": [
                                { "name": "tables", "type": kind },
                                { "name": "notes", "required": false }
                            ]
                        },
                        {
                            "id": "tests",
                            "name": "Tests",
                            "task": "Write a test per table.",
                            "receives": ["extract"],
                            "verify": { "criteria": ["every table has a test"], "corrections": 2 },
                            "exit": true
                        }
                    ]
                })),
                None,
            )
        };

        let toml = WorkflowTool::resolve_toml_content(&definition("list")).unwrap();
        let parsed = WorkflowDefinition::from_toml(&toml).unwrap();
        assert_eq!(parsed.steps[0].outputs[0].kind, FieldType::List);
        assert!(!parsed.steps[0].outputs[1].required);
        assert_eq!(
            parsed.steps[1].receives.as_deref(),
            Some(&["extract".to_string()][..])
        );
        let verify = parsed.steps[1].verify.as_ref().unwrap();
        assert!(verify.enabled);
        assert_eq!(verify.criteria, ["every table has a test"]);
        assert_eq!(verify.corrections, Some(2));

        let refused = WorkflowTool::resolve_toml_content(&definition("array")).unwrap_err();
        assert!(refused.contains("unknown variant `array`"), "{refused}");
    }
}

#[cfg(test)]
mod write_auth_tests {
    use super::*;
    use crate::authz::{always_admin, never_admin};
    use crate::def_write_auth::TurnDefWriteAuth;
    use crate::serve::Compose as _;
    use crate::workflow_runtime::WorkflowRuntime;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_core::access::{Grant, GrantLevel, Visibility};
    use workforce_orchestrator::workflow_store::WorkflowStore;

    const DEMO: &str = "[workflow]\nid = \"demo\"\nname = \"Demo\"\n\n\
        [[step]]\nid = \"a\"\nname = \"A\"\ntask = \"do the thing\"\nexit = true\n";
    const DEMO_EDITED: &str = "[workflow]\nid = \"demo\"\nname = \"Demo edited\"\n\n\
        [[step]]\nid = \"a\"\nname = \"A\"\ntask = \"do the thing\"\nexit = true\n";

    /// A runtime over `{dir}/workflows`, with the service's data under `dir`
    /// too; the `TempDir` lives as long as the test holds it.
    async fn fs_runtime() -> (Arc<WorkflowRuntime>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = dir.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        let service = crate::serve::AgentService::from_config(cfg, None)
            .await
            .expect("test service");
        let telemetry = Arc::clone(&service.telemetry_store);
        let runtime = WorkflowRuntime::new(service, telemetry, dir.path().join("workflows"));
        (Arc::new(runtime), dir)
    }

    fn demo_file(dir: &TempDir) -> PathBuf {
        dir.path().join("workflows").join("demo.toml")
    }

    fn tool(runtime: &Arc<WorkflowRuntime>, admin: bool) -> WorkflowTool {
        let is_admin = if admin { always_admin() } else { never_admin() };
        WorkflowTool::new(
            Arc::clone(runtime) as Arc<dyn crate::hosts::WorkflowHost>,
            "agent".into(),
            None,
        )
        .with_write_auth(TurnDefWriteAuth::default().with_admin_policy(is_admin))
    }

    async fn as_user<F: std::future::Future>(uid: &str, fut: F) -> F::Output {
        CURRENT_USER_ID.scope(uid.to_string(), fut).await
    }

    fn save(toml: &str) -> serde_json::Value {
        serde_json::json!({ "action": "save", "workflow_id": "demo", "toml_content": toml })
    }

    fn delete(workflow_id: &str) -> serde_json::Value {
        serde_json::json!({ "action": "delete", "workflow_id": workflow_id })
    }

    fn seed_run(dir: &TempDir, id: &str, variables: serde_json::Value) {
        workforce_common::persistence::SqliteStore::open(&dir.path().join("telemetry.db"))
            .unwrap()
            .save_workflow_instance(&WorkflowInstanceRow {
                id: id.to_string(),
                workflow_id: "demo".to_string(),
                status: "completed".to_string(),
                step_history:
                    r#"[{"step_id":"a","outcome":"done","agent_response":"private findings"}]"#
                        .to_string(),
                variables: variables.to_string(),
                trigger_json: r#"{"source":{"type":"api"},"payload":{}}"#.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            })
            .unwrap();
    }

    fn status(id: &str) -> serde_json::Value {
        serde_json::json!({ "action": "status", "instance_id": id })
    }

    fn listed(res: &ToolResult) -> Vec<String> {
        res.output["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["instance_id"].as_str().unwrap().to_string())
            .collect()
    }

    /// A run's step outputs are what it read on someone's behalf, so another
    /// member can neither read nor list it — only the identity it runs as or
    /// is billed to, and an administrator. A run that carries no principal is
    /// a system run and stays visible. Whether they may steer it is the
    /// steering rule's to say; with its workflow gone, they may not.
    #[tokio::test]
    async fn a_member_cannot_reach_another_users_workflow_run() {
        let (runtime, dir) = fs_runtime().await;
        seed_run(&dir, "alices", serde_json::json!({ "_run_as": "alice" }));
        seed_run(&dir, "billed", serde_json::json!({ "_bill_to": "alice" }));
        seed_run(&dir, "system", serde_json::json!({}));
        let member = tool(&runtime, false);
        let admin = tool(&runtime, true);

        for id in ["alices", "billed"] {
            let res = as_user("bob", member.execute(status(id))).await.unwrap();
            assert!(!res.success, "{res:?}");
            assert!(res
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not found"));
            for action in ["abort", "resume", "reject"] {
                let steer = serde_json::json!({ "action": action, "instance_id": id });
                let res = as_user("bob", member.execute(steer)).await.unwrap();
                assert!(!res.success, "{action}: {res:?}");
                assert_eq!(
                    res.error.as_deref(),
                    Some(crate::run_steering::SteerRefused::Gone.to_string().as_str()),
                    "{action}"
                );
            }
            assert!(
                as_user("alice", member.execute(status(id)))
                    .await
                    .unwrap()
                    .success
            );
        }
        let instances = serde_json::json!({ "action": "instances" });
        let bobs = as_user("bob", member.execute(instances.clone()))
            .await
            .unwrap();
        assert_eq!(listed(&bobs), ["system"]);
        let mut alices = listed(
            &as_user("alice", member.execute(instances.clone()))
                .await
                .unwrap(),
        );
        alices.sort();
        assert_eq!(alices, ["alices", "billed", "system"]);

        assert!(
            as_user("bob", admin.execute(status("alices")))
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            listed(&member.execute(instances).await.unwrap()),
            ["system"]
        );
        assert!(member.execute(status("system")).await.unwrap().success);
    }

    /// Background work started with no person in scope bills a `system:` or
    /// `agent:` lane; the heartbeat or job that started it, which has no
    /// identity, must still find it.
    #[test]
    fn a_run_billed_to_a_lane_alone_is_a_system_run() {
        let row = |variables: serde_json::Value| WorkflowInstanceRow {
            variables: variables.to_string(),
            ..Default::default()
        };
        let reader = |identity: Option<&str>| crate::def_write_auth::DefWriter {
            identity_id: identity.map(str::to_string),
            is_admin: false,
        };
        for lane in [
            serde_json::json!({ "_bill_to": "system:heartbeat" }),
            serde_json::json!({ "_run_as": "system:reports", "_bill_to": "agent:ops" }),
        ] {
            assert!(
                crate::workflow_runtime::run_is_for(&row(lane.clone()), &reader(None)),
                "{lane}"
            );
            assert!(
                crate::workflow_runtime::run_is_for(&row(lane.clone()), &reader(Some("bob"))),
                "{lane}"
            );
        }
        let alices = row(serde_json::json!({ "_run_as": "alice", "_bill_to": "system:reports" }));
        assert!(crate::workflow_runtime::run_is_for(
            &alices,
            &reader(Some("alice"))
        ));
        assert!(!crate::workflow_runtime::run_is_for(
            &alices,
            &reader(Some("bob"))
        ));
        assert!(!crate::workflow_runtime::run_is_for(&alices, &reader(None)));
    }

    #[test]
    fn a_run_whose_variables_do_not_parse_is_an_administrators_alone() {
        let row = WorkflowInstanceRow {
            variables: "{not json".to_string(),
            ..Default::default()
        };
        let reader = |identity: Option<&str>, is_admin| crate::def_write_auth::DefWriter {
            identity_id: identity.map(str::to_string),
            is_admin,
        };
        assert!(!crate::workflow_runtime::run_is_for(
            &row,
            &reader(Some("bob"), false)
        ));
        assert!(!crate::workflow_runtime::run_is_for(
            &row,
            &reader(None, false)
        ));
        assert!(crate::workflow_runtime::run_is_for(
            &row,
            &reader(None, true)
        ));
    }

    #[tokio::test]
    async fn admin_delete_removes_the_system_definition() {
        let (runtime, dir) = fs_runtime().await;
        let admin = tool(&runtime, true);
        assert!(admin.execute(save(DEMO)).await.unwrap().success);
        assert!(demo_file(&dir).exists());

        let res = admin.execute(delete("demo")).await.unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["deleted"], true);
        assert_eq!(res.output["workflow_id"], "demo");
        assert_eq!(res.output["owner"], serde_json::Value::Null);
        assert!(!demo_file(&dir).exists());

        let gone = admin
            .execute(serde_json::json!({ "action": "get", "workflow_id": "demo" }))
            .await
            .unwrap();
        assert!(!gone.success, "{gone:?}");

        let again = admin.execute(delete("demo")).await.unwrap();
        assert!(!again.success);
        assert!(again
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not found"));
        let missing = admin
            .execute(serde_json::json!({ "action": "delete" }))
            .await
            .unwrap();
        assert!(missing
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("workflow_id"));
    }

    /// With a store, a member deletes their own copy and the shared row
    /// stays. An administrator naming that member's row reaches it in place —
    /// the override — while another member naming it is refused.
    #[tokio::test]
    async fn member_delete_removes_only_their_own_copy() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let (runtime, _dir) = fs_runtime().await;
                runtime.set_store(Arc::clone(&store));
                let admin = tool(&runtime, true);
                let member = tool(&runtime, false);
                assert!(admin.execute(save(DEMO)).await.unwrap().success);
                assert!(
                    as_user("alice", member.execute(save(DEMO_EDITED)))
                        .await
                        .unwrap()
                        .success
                );

                let res = as_user("alice", member.execute(delete("demo")))
                    .await
                    .unwrap();
                assert!(res.success, "{res:?}");
                assert_eq!(res.output["owner"], "alice");
                assert!(store
                    .get("demo", "workflow", Some("alice"))
                    .await
                    .unwrap()
                    .is_none());
                assert_eq!(
                    store
                        .get("demo", "workflow", None)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(DEMO),
                    "the shared row is untouched"
                );

                assert!(
                    as_user("alice", member.execute(save(DEMO_EDITED)))
                        .await
                        .unwrap()
                        .success
                );
                let named = || {
                    let mut input = delete("demo");
                    input["owner"] = serde_json::json!("alice");
                    input
                };
                // Another member is refused; the row stays.
                let res = as_user("bob", member.execute(named())).await.unwrap();
                assert!(!res.success, "{res:?}");
                assert!(res
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("owned by alice"));
                assert!(store
                    .get("demo", "workflow", Some("alice"))
                    .await
                    .unwrap()
                    .is_some());

                // An administrator reaches it in place, and the shared row is
                // still not what they deleted.
                let res = as_user("root", admin.execute(named())).await.unwrap();
                assert!(res.success, "{res:?}");
                assert_eq!(res.output["owner"], "alice");
                assert!(store
                    .get("demo", "workflow", Some("alice"))
                    .await
                    .unwrap()
                    .is_none());
                assert_eq!(
                    store
                        .get("demo", "workflow", None)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(DEMO),
                    "the shared row is untouched"
                );
            },
        )
        .await;
    }

    /// A member is placed on their own row; with none, the shared definition
    /// is named as the administrator's and left alone.
    #[tokio::test]
    async fn member_delete_never_touches_the_system_definition() {
        let (runtime, dir) = fs_runtime().await;
        assert!(
            tool(&runtime, true)
                .execute(save(DEMO))
                .await
                .unwrap()
                .success
        );
        let res = as_user("alice", tool(&runtime, false).execute(delete("demo")))
            .await
            .unwrap();
        assert!(!res.success, "{res:?}");
        assert!(
            res.error
                .as_deref()
                .unwrap_or_default()
                .contains("only an administrator may delete it"),
            "{res:?}"
        );
        assert!(demo_file(&dir).exists());
    }

    #[tokio::test]
    async fn admin_save_writes_the_system_definition() {
        let (runtime, dir) = fs_runtime().await;
        let res = tool(&runtime, true).execute(save(DEMO)).await.unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["owner"], serde_json::Value::Null);
        assert_eq!(res.output["visibility"], "global");
        assert_eq!(res.output["key"], "demo");
        assert_eq!(std::fs::read_to_string(demo_file(&dir)).unwrap(), DEMO);
    }

    /// A member's save is placed on their own row, never the system one — on a
    /// filesystem deployment there is no such row, so nothing is written.
    #[tokio::test]
    async fn member_save_never_touches_the_system_definition() {
        let (runtime, dir) = fs_runtime().await;
        assert!(
            tool(&runtime, true)
                .execute(save(DEMO))
                .await
                .unwrap()
                .success
        );

        let res = as_user("alice", tool(&runtime, false).execute(save(DEMO_EDITED)))
            .await
            .unwrap();
        assert!(!res.success, "{res:?}");
        assert!(res
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("database store"));
        assert_eq!(std::fs::read_to_string(demo_file(&dir)).unwrap(), DEMO);
    }

    #[tokio::test]
    async fn unbound_turn_cannot_save() {
        let (runtime, dir) = fs_runtime().await;
        let res = tool(&runtime, false).execute(save(DEMO)).await.unwrap();
        assert!(!res.success, "{res:?}");
        assert!(res
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not signed in"));
        assert!(!demo_file(&dir).exists());
    }

    /// Naming another member's row is an administrator's override, so what
    /// refuses here is the deployment rather than the rule: a filesystem hub
    /// holds no member's row at all. A member is still refused by the rule,
    /// and neither touches the shared definition.
    #[tokio::test]
    async fn naming_another_members_row_reaches_it_rather_than_the_shared_one() {
        let (runtime, dir) = fs_runtime().await;
        let named = || {
            let mut input = save(DEMO);
            input["owner"] = serde_json::json!("bob");
            input
        };
        let overridden = as_user("root", tool(&runtime, true).execute(named()))
            .await
            .unwrap();
        assert!(!overridden.success, "{overridden:?}");
        assert!(
            overridden
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("require a database store"),
            "{overridden:?}"
        );

        let refused = as_user("alice", tool(&runtime, false).execute(named()))
            .await
            .unwrap();
        assert!(!refused.success, "{refused:?}");
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("owned by bob"),
            "{refused:?}"
        );
        assert!(!demo_file(&dir).exists());
    }

    /// With a store, a member re-saving a shared id gets their own copy and is
    /// told so; the shared definition, and what an admin later does to it,
    /// stay separate from the copy.
    #[tokio::test(flavor = "multi_thread")]
    async fn member_save_is_their_own_copy_and_the_shared_one_is_unchanged() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let (runtime, _dir) = fs_runtime().await;
                runtime.set_store(Arc::clone(&store));
                let admin = tool(&runtime, true);
                let member = tool(&runtime, false);
                assert!(admin.execute(save(DEMO)).await.unwrap().success);

                let res = as_user("alice", member.execute(save(DEMO_EDITED)))
                    .await
                    .unwrap();
                assert!(res.success, "{res:?}");
                assert_eq!(res.output["owner"], "alice");
                assert_eq!(res.output["visibility"], "private");
                assert_eq!(res.output["key"], "alice/demo");
                let message = res.output["message"].as_str().unwrap();
                assert!(message.contains("your private copy of 'demo'"), "{message}");
                assert!(
                    message.contains("the shared 'demo' is unchanged"),
                    "{message}"
                );
                assert_eq!(
                    store
                        .get("demo", "workflow", None)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(DEMO)
                );
                assert_eq!(
                    store
                        .get("demo", "workflow", Some("alice"))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(DEMO_EDITED)
                );

                // `get` reads the member's own copy first; `list` shows both.
                let got = as_user(
                    "alice",
                    member.execute(serde_json::json!({ "action": "get", "workflow_id": "demo" })),
                )
                .await
                .unwrap();
                assert_eq!(got.output["owner"], "alice");
                assert_eq!(got.output["definition"]["workflow"]["name"], "Demo edited");
                let listed = as_user(
                    "alice",
                    member.execute(serde_json::json!({ "action": "list" })),
                )
                .await
                .unwrap();
                assert_eq!(listed.output["count"], 2);

                // The owner may publish their copy; the admin edits only the system row.
                let mut publish = save(DEMO_EDITED);
                publish["visibility"] = serde_json::json!("global");
                let res = as_user("alice", member.execute(publish)).await.unwrap();
                assert!(res.success, "{res:?}");
                assert_eq!(res.output["visibility"], "global");
                let res = as_user("root", admin.execute(save(DEMO))).await.unwrap();
                assert!(res.success, "{res:?}");
                assert_eq!(res.output["owner"], serde_json::Value::Null);
                assert_eq!(
                    runtime.definition_ownership("demo", Some("alice")).await,
                    Some((Some("alice".into()), Visibility::Global))
                );

                // A run-share grantee reads the owner's row by naming it; anyone
                // else is told it is not there.
                let mut privately = save(DEMO_EDITED);
                privately["visibility"] = serde_json::json!("private");
                assert!(as_user("alice", member.execute(privately)).await.unwrap().success);
                store
                    .set_shares(
                        "workflow",
                        "demo",
                        "alice",
                        &[Grant::user("bob", GrantLevel::Run)],
                        "alice",
                    )
                    .await
                    .unwrap();
                let get_alices = || {
                    serde_json::json!({ "action": "get", "workflow_id": "demo", "owner": "alice" })
                };
                let bob = as_user("bob", member.execute(get_alices())).await.unwrap();
                assert!(bob.success, "{bob:?}");
                assert_eq!(bob.output["owner"], "alice");
                assert_eq!(bob.output["definition"]["workflow"]["name"], "Demo edited");
                let carol = as_user("carol", member.execute(get_alices())).await.unwrap();
                assert!(!carol.success, "{carol:?}");
                assert!(carol.error.as_deref().unwrap_or_default().contains("not found"));
            },
        )
        .await;
    }
}

#[cfg(test)]
mod steering_tests {
    use super::*;
    use crate::authz::{always_admin, never_admin};
    use crate::run_steering::SteerRefused;
    use crate::workflow_runtime::testing::{gate_steps, hub, Hub, WORKFLOW};
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_core::access::Visibility;
    use workforce_orchestrator::workflow::instance::{InstanceStatus, InterventionKind};

    /// `who` asking the `workflow` tool to `action` the run `id`, through an
    /// agent that knows whether they are an administrator.
    async fn ask(hub: &Hub, who: &str, admin: bool, action: &str, id: &str) -> ToolResult {
        ask_with(
            hub,
            who,
            admin,
            serde_json::json!({ "action": action, "instance_id": id }),
        )
        .await
    }

    async fn ask_with(hub: &Hub, who: &str, admin: bool, input: serde_json::Value) -> ToolResult {
        let policy = if admin { always_admin() } else { never_admin() };
        let tool = WorkflowTool::new(hub.host(), "agent".into(), None)
            .with_write_auth(TurnDefWriteAuth::default().with_admin_policy(policy));
        CURRENT_USER_ID
            .scope(who.to_string(), tool.execute(input))
            .await
            .unwrap()
    }

    /// A run Alice started, waiting on a decision Bob makes.
    async fn waiting_on_bob(hub: &Hub) -> String {
        hub.with_slack();
        let key = hub
            .define_steps(
                Visibility::Private,
                false,
                &crate::workflow_runtime::testing::decision_steps(&hub.bob, ""),
            )
            .await;
        hub.run_to_decision(&key, Some(&hub.alice), Some("CRUN000001"))
            .await
    }

    fn resume_with(run: &str, outcome: &str, gate: Option<&str>) -> serde_json::Value {
        serde_json::json!({ "action": "resume", "instance_id": run, "outcome": outcome, "gate": gate })
    }

    #[tokio::test]
    async fn the_tool_decides_only_for_a_named_decider() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        for (who, admin) in [(&hub.alice, false), (&hub.carol, true)] {
            let refused = ask_with(&hub, who, admin, resume_with(&run, "send", None)).await;
            assert_eq!(refused.error.as_deref(), Some("Only Bob can decide this."));
        }
        let decided = ask_with(&hub, &hub.bob, false, resume_with(&run, "Send", None)).await;
        assert!(decided.success, "{decided:?}");
        assert_eq!(decided.output["decided"]["summary"], "Sent by Bob");
        assert_eq!(hub.ran(&run).await, ["draft", "review", "send"]);
    }

    #[tokio::test]
    async fn a_gate_from_status_is_honoured_and_a_stale_one_refused() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;
        let status = ask(&hub, &hub.bob, false, "status", &run).await;
        let gate = status.output["decision"]["gate"]
            .as_str()
            .unwrap()
            .to_string();

        let stale = ask_with(
            &hub,
            &hub.bob,
            false,
            resume_with(&run, "send", Some(&uuid::Uuid::new_v4().to_string())),
        )
        .await;
        assert_eq!(
            stale.error.as_deref(),
            Some("That decision is closed; the run has moved on.")
        );
        let pressed = ask_with(
            &hub,
            &hub.bob,
            false,
            resume_with(&run, "send", Some(&gate)),
        )
        .await;
        assert!(pressed.success, "{pressed:?}");
    }

    #[tokio::test]
    async fn status_shows_an_open_decision() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        let status = ask(&hub, &hub.alice, false, "status", &run).await;

        assert!(status.success, "{status:?}");
        let decision = &status.output["decision"];
        assert_eq!(decision["title"], "Review the digest");
        assert_eq!(decision["waiting_on"], serde_json::json!(["Bob"]));
        assert_eq!(decision["may_decide"], false);
        assert_eq!(decision["choices"][1]["label"], "Redraft");
    }

    #[tokio::test]
    async fn reject_at_a_decision_is_refused() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        let refused = ask(&hub, &hub.alice, false, "reject", &run).await;

        assert!(!refused.success);
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("This run is waiting on a decision (Send · Redraft"),
            "{refused:?}"
        );
        assert_eq!(hub.status(&run), InstanceStatus::Paused);
    }

    #[tokio::test]
    async fn the_starter_may_steer_their_run_through_the_tool() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let res = ask(&hub, &hub.alice, false, "abort", &run).await;
        assert!(res.success, "{res:?}");
        assert_eq!(hub.status(&run), InstanceStatus::Cancelled);
    }

    #[tokio::test]
    async fn an_admin_may_steer_anyones_run_through_the_tool() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let res = ask(&hub, &hub.carol, true, "abort", &run).await;
        assert!(res.success, "{res:?}");
        assert_eq!(hub.status(&run), InstanceStatus::Cancelled);
    }

    #[tokio::test]
    async fn another_member_may_steer_a_shared_run_through_the_tool_and_is_named_for_it() {
        let hub = hub().await;
        let kept = crate::decision_records::testing::record_in_memory();
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let res = ask(&hub, &hub.bob, false, "abort", &run).await;
        assert!(res.success, "{res:?}");
        assert_eq!(hub.status(&run), InstanceStatus::Cancelled);
        let kept = kept.interventions_of(&run, 1).await;
        assert_eq!(
            kept.iter().map(|i| (i.kind, &i.actor)).collect::<Vec<_>>(),
            [(
                InterventionKind::Abort,
                &Actor::Person {
                    id: hub.bob.clone()
                }
            )]
        );
    }

    #[tokio::test]
    async fn another_member_may_not_steer_through_the_tool_once_the_flag_is_off() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        for action in ["abort", "resume", "reject"] {
            let res = ask(&hub, &hub.bob, false, action, &run).await;
            assert_eq!(
                res.error,
                Some(
                    SteerRefused::Closed {
                        workflow: WORKFLOW.into()
                    }
                    .to_string()
                ),
                "{action}"
            );
        }
        assert_eq!(hub.status(&run), InstanceStatus::Paused);
    }

    #[tokio::test]
    async fn another_member_may_not_steer_a_private_run_through_the_tool() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let res = ask(&hub, &hub.bob, false, "abort", &run).await;
        assert_eq!(
            res.error,
            Some(
                SteerRefused::Private {
                    workflow: WORKFLOW.into()
                }
                .to_string()
            )
        );
        assert_eq!(hub.status(&run), InstanceStatus::Paused);
    }

    #[tokio::test]
    async fn a_flag_switched_off_mid_run_binds_the_tool() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        hub.define(Visibility::Global, false).await;
        let res = ask(&hub, &hub.bob, false, "abort", &run).await;
        assert!(!res.success, "{res:?}");
        assert_eq!(hub.status(&run), InstanceStatus::Paused);
    }

    /// An agent's resume says whether the step it stood at went on with
    /// the outcome; a gate awaiting a person still takes none from it.
    #[tokio::test]
    async fn the_tool_says_whether_a_resumes_outcome_was_applied() {
        let hub = hub().await;
        let key = hub.define_gate(Visibility::Private, false).await;
        let awaiting = hub.run_to_wait(&key, Some(&hub.alice)).await;
        let res = ask_with(
            &hub,
            &hub.alice,
            false,
            serde_json::json!({ "action": "resume", "instance_id": awaiting, "outcome": "approved" }),
        )
        .await;
        assert!(
            res.error
                .as_deref()
                .is_some_and(|e| e.contains("paused for human approval")),
            "{res:?}"
        );

        let key = hub
            .define_steps(Visibility::Private, false, &gate_steps("waiting"))
            .await;
        let waiting = hub.run_to_wait(&key, Some(&hub.alice)).await;
        let res = ask(&hub, &hub.alice, false, "reject", &waiting).await;
        assert_eq!(
            (&res.output["outcome_applied"], &res.output["message"]),
            (
                &serde_json::json!(true),
                &serde_json::json!("Workflow resumed with outcome 'rejected'.")
            ),
            "{res:?}"
        );
        assert_eq!(hub.ran(&waiting).await, ["gate", "fix"]);

        let held = hub.run_of(&key, Some(&hub.alice)).await;
        let res = ask_with(
            &hub,
            &hub.alice,
            false,
            serde_json::json!({ "action": "resume", "instance_id": held, "outcome": "approved" }),
        )
        .await;
        assert_eq!(
            (&res.output["outcome_applied"], &res.output["message"]),
            (
                &serde_json::json!(false),
                &serde_json::json!(
                    "Workflow resumed, but its step did not go on with outcome 'approved'."
                )
            ),
            "{res:?}"
        );
    }

    #[test]
    fn the_schema_reference_documents_the_flag() {
        assert!(WORKFLOW_SCHEMA_REFERENCE.contains("| steerable_by_others"));
    }
}

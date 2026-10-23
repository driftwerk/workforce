//! `subagent` tool — lets agents spawn background tasks in isolated sessions.
//!
//! A thin facade over the async job runner: each sub-agent is an `Agent` job
//! (`run_isolated`), so it inherits the runner's persistence, concurrency
//! limit, retry, recovery, and completion notifications. Access is controlled
//! by the policy engine via `tool: "subagent"`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::text::truncate_str;
use workforce_orchestrator::workflow::package::{OutputField, VerifySpec};

use crate::authz::{never_admin, IsAdminFn};
use crate::hub_core::jobs::{
    AgentJobConfig, AgentProfile, HandoffSpec, JobConfig, JobManifest, JobReader, JobType,
};
use crate::job_runner::DEFAULT_TIMEOUT_SECS;
use crate::serve::AgentService;
use crate::stop_registry::{spawn_dereg_watcher, BackgroundWork, CancelTarget};

/// Tool that lets agents spawn and manage sub-agents.
pub struct SubAgentTool {
    service: Arc<AgentService>,
    is_admin: IsAdminFn,
}

impl SubAgentTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self {
            service,
            is_admin: never_admin(),
        }
    }

    /// Inject the real admin policy (the same `IsAdminFn` the vault tools get).
    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    /// The calling turn as a reader of jobs.
    fn reader(&self) -> JobReader {
        JobReader::of_turn((self.is_admin)())
    }

    fn runner(&self) -> Option<&Arc<dyn crate::hosts::JobHost>> {
        self.service.job_runner.as_ref()
    }
}

#[derive(Deserialize)]
struct SubAgentInput {
    action: String,
    /// Task description for the sub-agent (for `spawn`).
    #[serde(default)]
    task: Option<String>,
    /// Model override (for `spawn`). Uses parent's model if omitted.
    #[serde(default)]
    model: Option<String>,
    /// Custom system prompt for the sub-agent (for `spawn`).
    #[serde(default)]
    system_prompt: Option<String>,
    /// Execution profile (for `spawn`): "worker" (lightweight, read-only, cheap)
    /// or "full" (default — complete tools and the agent's identity).
    #[serde(default)]
    profile: Option<String>,
    /// The package the sub-agent must hand back (for `spawn`), declared as a
    /// workflow step's `outputs` are.
    #[serde(default)]
    outputs: Vec<OutputField>,
    /// Whether a verifier checks the result before it is handed back (for
    /// `spawn`): a flag, or criteria and correction rounds.
    #[serde(default)]
    verify: Option<VerifySpec>,
    /// Timeout in seconds (for `spawn`). Default 600.
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Task ID (for `status`, `cancel`, `result`).
    #[serde(default)]
    task_id: Option<String>,
    /// Slack channel ID for result announcement (auto-captured from messaging context).
    #[serde(default)]
    channel_id: Option<String>,
    /// Slack thread ID for result announcement.
    #[serde(default)]
    thread_id: Option<String>,
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Spawn background sub-agents for parallel task execution"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "status", "list", "cancel", "result"],
                    "description": "Action to perform"
                },
                "task": {
                    "type": "string",
                    "description": "Task description for the sub-agent to work on (for 'spawn')"
                },
                "model": {
                    "type": "string",
                    "description": "Model override for sub-agent"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Custom system prompt for the sub-agent (for 'spawn')"
                },
                "profile": {
                    "type": "string",
                    "enum": ["full", "worker"],
                    "description": "Execution profile (for 'spawn'). 'worker' is a lightweight, read-only, cheap-model investigator for research/lookups; 'full' (default) is a complete agent. Prefer 'worker' for parallel investigation."
                },
                "outputs": {
                    "type": "array",
                    "description": "The package the sub-agent must hand back (for 'spawn'). It is validated against these fields, a malformed one is sent back for repair, and 'result' returns it as `package` in place of the sub-agent's prose.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string", "description": "What the field must hold" },
                            "type": {
                                "type": "string",
                                "enum": ["string", "number", "integer", "boolean", "list", "object"],
                                "description": "Default 'string'"
                            },
                            "required": { "type": "boolean", "description": "Default true" },
                            "validate": { "type": "string", "enum": ["number", "not_empty"], "description": "Optional rule on the written text" }
                        },
                        "required": ["name"]
                    }
                },
                "verify": {
                    "description": "Have a verifier check the result against the task and `outputs` before it is handed back (for 'spawn'): true, or { criteria: [..], corrections: n }. A rejected result is sent back with the failing checks; one still rejected fails the task.",
                    "oneOf": [
                        { "type": "boolean" },
                        {
                            "type": "object",
                            "properties": {
                                "criteria": { "type": "array", "items": { "type": "string" } },
                                "corrections": { "type": "integer", "minimum": 0, "maximum": 3 }
                            }
                        }
                    ]
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (for 'spawn', default 600)"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID (for 'status', 'cancel', 'result')"
                },
                "channel_id": {
                    "type": "string",
                    "description": "Slack channel ID for announcing the result"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Slack thread ID for announcing the result in-thread."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: SubAgentInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid subagent input: {}", e))?;

        match input.action.as_str() {
            "spawn" => self.handle_spawn(input, start).await,
            "status" => self.handle_status(input, start).await,
            "list" => self.handle_list(start).await,
            "cancel" => self.handle_cancel(input, start).await,
            "result" => self.handle_result(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Use: spawn, status, list, cancel, result",
                    other
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl SubAgentTool {
    async fn handle_spawn(&self, input: SubAgentInput, start: Instant) -> Result<ToolResult> {
        let task = match input.task {
            Some(t) if !t.is_empty() => t,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'task' — what should the sub-agent work on?",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let Some(runner) = self.runner() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                start.elapsed().as_millis() as u64,
            ));
        };

        let handoff = HandoffSpec {
            outputs: input.outputs,
            verify: input.verify,
        };
        if let Some(defect) = handoff.defect() {
            return Ok(ToolResult::failure(
                format!("Invalid hand-over: {defect}"),
                start.elapsed().as_millis() as u64,
            ));
        }

        let timeout_secs = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let profile = match input.profile.as_deref() {
            Some("worker") => AgentProfile::Worker,
            _ => AgentProfile::Full,
        };

        // Resolve messaging context so the runner announces the result in-thread.
        let (channel_id, thread_id) = if input.channel_id.is_some() {
            (input.channel_id, input.thread_id)
        } else if let Some(ref sched) = self.service.scheduler {
            let session_key = CURRENT_SESSION_KEY
                .try_with(|k| k.clone())
                .unwrap_or_default();
            match sched.get_messaging_context(&session_key).await {
                Some((cid, tid)) => (Some(cid), tid),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let mut manifest = JobManifest::new(
            self.service.agent_id.as_str().to_string(),
            JobType::Agent,
            JobConfig::Agent(AgentJobConfig {
                model: input.model,
                system_prompt: input.system_prompt,
                handoff: (handoff != HandoffSpec::default()).then_some(handoff),
                ..AgentJobConfig::new(
                    task.clone(),
                    profile,
                    crate::user_config::current_principal(),
                )
            }),
            Some(task.chars().take(80).collect()),
            timeout_secs,
        );
        manifest.notify_channel = channel_id.clone();
        manifest.notify_thread = thread_id.clone();

        match runner.submit(manifest).await {
            Ok(task_id) => {
                // A sub-agent that announces to a channel is detached work that
                // outlives this turn — register it so a human's "stop" there can
                // cancel it. Single jobs have no `ProgressReporter` to deregister
                // them, so a lightweight watcher drops the entry on completion.
                if let Some(channel) = channel_id {
                    self.service.background_work.write().await.insert(
                        task_id.clone(),
                        BackgroundWork {
                            target: CancelTarget::Job(task_id.clone()),
                            channel,
                            thread: thread_id,
                            requester: CURRENT_USER_ID.try_with(|u| u.clone()).ok(),
                            label: format!(
                                "sub-agent: {}",
                                task.chars().take(60).collect::<String>()
                            ),
                        },
                    );
                    spawn_dereg_watcher(
                        Arc::clone(&self.service.background_work),
                        runner.clone(),
                        task_id.clone(),
                        CancelTarget::Job(task_id.clone()),
                    );
                }
                Ok(ToolResult::success(
                    serde_json::json!({
                        "task_id": task_id,
                        "status": "running",
                        "timeout_secs": timeout_secs,
                        "message": format!("Sub-agent spawned. Check status with action='status', task_id='{}'", task_id),
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to spawn sub-agent: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_status(&self, input: SubAgentInput, start: Instant) -> Result<ToolResult> {
        let task_id = match input.task_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'task_id'",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let Some(runner) = self.runner() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                start.elapsed().as_millis() as u64,
            ));
        };

        match runner.job_detail_for(&task_id, &self.reader()).await {
            Some((manifest, output)) => {
                let mut info = serde_json::json!({
                    "task_id": manifest.job_id,
                    "task": manifest.agent_task_text(),
                    "status": manifest.agent_status_label(),
                    "started_at": manifest.started_at.unwrap_or(manifest.submitted_at).to_rfc3339(),
                });
                if let Some(finished) = manifest.completed_at {
                    info["finished_at"] = serde_json::json!(finished.to_rfc3339());
                    info["duration_secs"] = serde_json::json!(manifest.elapsed_secs() as i64);
                }
                if let Some(out) = output.filter(|o| !o.full_output.is_empty()) {
                    info["result_preview"] = serde_json::json!(truncate_str(&out.full_output, 500));
                }
                if let Some(err) = manifest.error {
                    info["error"] = serde_json::json!(err);
                }
                Ok(ToolResult::success(
                    info,
                    start.elapsed().as_millis() as u64,
                ))
            }
            None => Ok(ToolResult::failure(
                format!("Task '{}' not found", task_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_list(&self, start: Instant) -> Result<ToolResult> {
        let Some(runner) = self.runner() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                start.elapsed().as_millis() as u64,
            ));
        };

        let reader = self.reader();
        let list: Vec<serde_json::Value> = runner
            .list_by_type(JobType::Agent)
            .await
            .iter()
            .filter(|m| m.readable_by(&reader))
            .map(|m| {
                serde_json::json!({
                    "task_id": m.job_id,
                    "task": m.agent_task_text().chars().take(100).collect::<String>(),
                    "status": m.agent_status_label(),
                    "started_at": m.started_at.unwrap_or(m.submitted_at).to_rfc3339(),
                    "finished_at": m.completed_at.map(|f| f.to_rfc3339()),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({ "tasks": list, "count": list.len() }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_cancel(&self, input: SubAgentInput, start: Instant) -> Result<ToolResult> {
        let task_id = match input.task_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'task_id'",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let Some(runner) = self.runner() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                start.elapsed().as_millis() as u64,
            ));
        };

        let cancelled = match runner.job_detail_for(&task_id, &self.reader()).await {
            Some(_) => runner.cancel(&task_id).await,
            None => Ok(false),
        };
        match cancelled {
            Ok(true) => Ok(ToolResult::success(
                serde_json::json!({ "task_id": task_id, "status": "cancelling" }),
                start.elapsed().as_millis() as u64,
            )),
            Ok(false) => Ok(ToolResult::failure(
                format!("Task '{}' not found or already finished", task_id),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to cancel task '{}': {}", task_id, e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_result(&self, input: SubAgentInput, start: Instant) -> Result<ToolResult> {
        let task_id = match input.task_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'task_id'",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let Some(runner) = self.runner() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                start.elapsed().as_millis() as u64,
            ));
        };

        match runner.job_detail_for(&task_id, &self.reader()).await {
            Some((manifest, output)) => {
                let (result, findings, package) = match output {
                    Some(o) => {
                        let part =
                            |key: &str| o.tool_result.as_ref().and_then(|t| t.get(key).cloned());
                        (
                            (!o.full_output.is_empty()).then_some(o.full_output),
                            part("findings"),
                            part("package"),
                        )
                    }
                    None => (None, None, None),
                };
                if result.is_some() || manifest.error.is_some() {
                    Ok(ToolResult::success(
                        handed_back(
                            &manifest.job_id,
                            manifest.agent_status_label(),
                            result,
                            findings,
                            package,
                            manifest.error,
                        ),
                        start.elapsed().as_millis() as u64,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!(
                            "Task '{}' is still {}",
                            task_id,
                            manifest.agent_status_label()
                        ),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
            None => Ok(ToolResult::failure(
                format!("Task '{}' not found", task_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// What `result` returns of a finished sub-agent. One that handed back a
/// package is read through it: the caller asked for structured state, so the
/// prose around it stays with the sub-agent.
fn handed_back(
    task_id: &str,
    status: &str,
    result: Option<String>,
    findings: Option<serde_json::Value>,
    package: Option<serde_json::Value>,
    error: Option<String>,
) -> serde_json::Value {
    let mut out = serde_json::json!({
        "task_id": task_id,
        "status": status,
        "findings": findings,
        "error": error,
    });
    match package {
        Some(package) => out["package"] = package,
        None => out["result"] = serde_json::json!(result),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_with_a_package_returns_it_in_place_of_the_prose() {
        let prose = "I read forty files and here is how I got there…".to_string();
        let package = serde_json::json!({ "tables": ["orders"], "count": 1 });

        let packaged = handed_back(
            "j1",
            "completed",
            Some(prose.clone()),
            None,
            Some(package.clone()),
            None,
        );
        assert_eq!(packaged["package"], package);
        assert!(packaged.get("result").is_none());
        assert!(!packaged.to_string().contains("forty files"));

        let plain = handed_back("j1", "completed", Some(prose.clone()), None, None, None);
        assert_eq!(plain["result"], prose);
        assert!(plain.get("package").is_none());
    }

    #[test]
    fn spawn_reads_a_package_schema_and_a_verifier_from_its_input() {
        let input: SubAgentInput = serde_json::from_value(serde_json::json!({
            "action": "spawn",
            "task": "Extract the schema",
            "outputs": [
                { "name": "tables", "type": "list", "description": "table names" },
                { "name": "notes", "required": false }
            ],
            "verify": { "criteria": ["every table has a key"], "corrections": 2 }
        }))
        .unwrap();
        assert_eq!(input.outputs.len(), 2);
        assert!(!input.outputs[1].required);
        let verify = input.verify.unwrap();
        assert!(verify.enabled);
        assert_eq!(verify.corrections, Some(2));

        let flagged: SubAgentInput =
            serde_json::from_value(serde_json::json!({ "action": "spawn", "verify": true }))
                .unwrap();
        assert!(flagged.verify.unwrap().enabled && flagged.outputs.is_empty());
    }
}

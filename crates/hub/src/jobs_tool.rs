//! `jobs` tool — lets agents submit and manage background jobs.
//!
//! Provides: jobs.run, jobs.exec, jobs.monitor, jobs.watch_ci,
//! jobs.chain, jobs.list, jobs.result, jobs.cancel,
//! jobs.chain_cancel, jobs.chain_status, jobs.stream, jobs.stream_status

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::authz::{never_admin, IsAdminFn};
use crate::hub_core::jobs::{
    BackoffStrategy, ChainStep, FailureType, JobConfig, JobManifest, JobReader, JobType,
    MonitorJobConfig, NotifyOn, RetryPolicy, ShellJobConfig, StreamConfig, ToolJobConfig,
    DEFAULT_STREAM_THROTTLE_SECS,
};
use crate::job_runner::{
    JobRunner, DEFAULT_MAX_POLLS, DEFAULT_POLL_INTERVAL_SECS, DEFAULT_TIMEOUT_SECS,
    MIN_POLL_INTERVAL_SECS,
};

/// Tool that lets agents submit and manage background jobs.
pub struct JobsTool {
    runner: JobRunner,
    agent_id: String,
    is_admin: IsAdminFn,
}

impl JobsTool {
    pub fn new(runner: JobRunner, agent_id: String) -> Self {
        Self {
            runner,
            agent_id,
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
}

#[derive(Deserialize)]
struct JobsInput {
    action: String,

    // ── jobs.run params ──
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,

    // ── jobs.exec params ──
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    workdir: Option<String>,

    // ── jobs.monitor params ──
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    max_polls: Option<u64>,

    // ── jobs.watch_ci params ──
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    r#ref: Option<String>,
    #[serde(default)]
    owner: Option<String>,

    // ── Shared params ──
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    notify_channel: Option<String>,
    #[serde(default)]
    notify_thread: Option<String>,

    // ── jobs.result / jobs.cancel params ──
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,

    // ── jobs.list params ──
    #[serde(default)]
    status: Option<String>,

    // ── Retry params ──
    #[serde(default)]
    retry: Option<RetryInput>,

    // ── Streaming params ──
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    throttle_secs: Option<u64>,

    // ── Chain params ──
    #[serde(default)]
    steps: Option<Vec<ChainStepInput>>,
    #[serde(default)]
    chain_id: Option<String>,

    // ── Stream toggle params ──
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct RetryInput {
    #[serde(default = "default_max_attempts")]
    max_attempts: u32,
    #[serde(default = "default_backoff")]
    backoff: String,
    #[serde(default = "default_initial_delay")]
    initial_delay_secs: u64,
    #[serde(default)]
    retry_on: Vec<String>,
    #[serde(default)]
    jitter: bool,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_backoff() -> String {
    "exponential".to_string()
}
fn default_initial_delay() -> u64 {
    5
}

#[derive(Deserialize)]
struct ChainStepInput {
    id: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    retry: Option<RetryInput>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    stream: Option<bool>,
    // Monitor-specific
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    max_polls: Option<u64>,
}

fn parse_retry(input: &RetryInput) -> RetryPolicy {
    let backoff = match input.backoff.as_str() {
        "linear" => BackoffStrategy::Linear,
        "fixed" => BackoffStrategy::Fixed,
        _ => BackoffStrategy::Exponential,
    };

    let retry_on: Vec<FailureType> = input
        .retry_on
        .iter()
        .filter_map(|s| match s.as_str() {
            "timeout" => Some(FailureType::Timeout),
            "non_zero_exit" | "exit_code_nonzero" => Some(FailureType::NonZeroExit),
            "tool_error" => Some(FailureType::ToolError),
            _ => None,
        })
        .collect();

    RetryPolicy {
        max_attempts: input.max_attempts.max(1),
        backoff,
        initial_delay_secs: input.initial_delay_secs.max(1),
        retry_on,
        jitter: input.jitter,
    }
}

fn parse_chain_step(input: &ChainStepInput) -> Result<ChainStep> {
    let config = if let Some(ref tool) = input.tool {
        if let Some(ref until) = input.until {
            // Monitor job
            JobConfig::Monitor(MonitorJobConfig {
                tool: tool.clone(),
                params: input
                    .params
                    .clone()
                    .unwrap_or(serde_json::Value::Object(Default::default())),
                interval_secs: input
                    .interval_secs
                    .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
                    .max(MIN_POLL_INTERVAL_SECS),
                until: until.clone(),
                max_polls: input.max_polls.unwrap_or(DEFAULT_MAX_POLLS),
                polls_completed: 0,
                notify_on: NotifyOn::Condition,
                watch_path: None,
            })
        } else {
            // Tool job
            JobConfig::Tool(ToolJobConfig {
                tool: tool.clone(),
                params: input
                    .params
                    .clone()
                    .unwrap_or(serde_json::Value::Object(Default::default())),
            })
        }
    } else if let Some(ref command) = input.command {
        JobConfig::Shell(ShellJobConfig {
            command: command.clone(),
            workdir: input.workdir.clone(),
        })
    } else {
        return Err(eyre::eyre!(
            "Step '{}' must specify either 'tool' or 'command'",
            input.id
        ));
    };

    Ok(ChainStep {
        id: input.id.clone(),
        config,
        depends_on: input.depends_on.clone(),
        label: input.label.clone(),
        retry: input.retry.as_ref().map(parse_retry),
        timeout_secs: input.timeout_secs,
        stream: input.stream.unwrap_or(false),
    })
}

#[async_trait]
impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "Submit and manage background jobs with chaining and retry"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["run", "exec", "monitor", "watch_ci", "chain", "list", "result",
                             "cancel", "chain_cancel", "chain_status", "stream", "stream_status"],
                    "description": "Action to perform"
                },
                "tool": { "type": "string", "description": "Tool name (for 'run', 'monitor')" },
                "params": { "type": "object", "description": "Tool parameters (for 'run', 'monitor')" },
                "command": { "type": "string", "description": "Shell command (for 'exec')" },
                "workdir": { "type": "string", "description": "Working directory (for 'exec')" },
                "interval_secs": { "type": "integer", "description": "Poll interval (for 'monitor', default: 30)" },
                "until": { "type": "string", "description": "Condition expression (for 'monitor')" },
                "max_polls": { "type": "integer", "description": "Max polls (for 'monitor', default: 60)" },
                "repo": { "type": "string", "description": "Repository (for 'watch_ci')" },
                "ref": { "type": "string", "description": "Git ref (for 'watch_ci')" },
                "owner": { "type": "string", "description": "Repo owner (for 'watch_ci')" },
                "label": { "type": "string", "description": "Human-readable label" },
                "timeout_secs": { "type": "integer", "description": "Job timeout (default: 600)" },
                "notify_channel": { "type": "string", "description": "Notification channel" },
                "notify_thread": { "type": "string", "description": "Notification thread" },
                "job_id": { "type": "string", "description": "Job ID (for 'result', 'cancel', 'stream', 'stream_status')" },
                "tail": { "type": "integer", "description": "Last N lines of output (for 'result')" },
                "status": { "type": "string", "description": "Filter by status (for 'list')" },
                "retry": {
                    "type": "object",
                    "description": "Retry policy",
                    "properties": {
                        "max_attempts": { "type": "integer", "description": "Max attempts including first (default: 3)" },
                        "backoff": { "type": "string", "description": "Backoff strategy (default: exponential)" },
                        "initial_delay_secs": { "type": "integer", "description": "Initial delay before first retry (default: 5)" },
                        "retry_on": { "type": "array", "items": { "type": "string" }, "description": "Failure types to retry on" },
                        "jitter": { "type": "boolean", "description": "Add jitter to delays (default: false)" }
                    }
                },
                "stream": { "type": "boolean", "description": "Enable output streaming (for 'run', 'exec')" },
                "throttle_secs": { "type": "integer", "description": "Stream throttle interval (default: 5)" },
                "steps": {
                    "type": "array",
                    "description": "Chain steps (for 'chain')",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Step ID" },
                            "tool": { "type": "string", "description": "Tool to execute" },
                            "command": { "type": "string", "description": "Shell command" },
                            "params": { "type": "object", "description": "Tool parameters" },
                            "depends_on": { "type": "array", "items": { "type": "string" }, "description": "Step IDs this depends on" },
                            "label": { "type": "string" },
                            "retry": { "type": "object", "description": "Retry policy for this step" },
                            "timeout_secs": { "type": "integer" },
                            "stream": { "type": "boolean" }
                        },
                        "required": ["id"]
                    }
                },
                "chain_id": { "type": "string", "description": "Chain ID (for 'chain_cancel', 'chain_status')" },
                "enabled": { "type": "boolean", "description": "Enable/disable streaming (for 'stream')" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: JobsInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid jobs input: {}", e))?;

        match input.action.as_str() {
            "run" => self.handle_run(input, start).await,
            "exec" => self.handle_exec(input, start).await,
            "monitor" => self.handle_monitor(input, start).await,
            "watch_ci" => self.handle_watch_ci(input, start).await,
            "chain" => self.handle_chain(input, start).await,
            "list" => self.handle_list(input, start).await,
            "result" => self.handle_result(input, start).await,
            "cancel" => self.handle_cancel(input, start).await,
            "chain_cancel" => self.handle_chain_cancel(input, start).await,
            "chain_status" => self.handle_chain_status(input, start).await,
            "stream" => self.handle_stream_toggle(input, start).await,
            "stream_status" => self.handle_stream_status(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Available: run, exec, monitor, watch_ci, chain, \
                     list, result, cancel, chain_cancel, chain_status, stream, stream_status",
                    other
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl JobsTool {
    fn build_retry_policy(&self, input: &JobsInput) -> Option<RetryPolicy> {
        input.retry.as_ref().map(parse_retry)
    }

    fn build_stream_config(&self, input: &JobsInput) -> Option<StreamConfig> {
        if input.stream.unwrap_or(false) {
            Some(StreamConfig {
                enabled: true,
                throttle_secs: input.throttle_secs.unwrap_or(DEFAULT_STREAM_THROTTLE_SECS),
            })
        } else {
            None
        }
    }

    /// Handle `jobs.run` — submit a tool job.
    async fn handle_run(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let retry_policy = self.build_retry_policy(&input);
        let stream_config = self.build_stream_config(&input);

        let tool = match input.tool {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'tool'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let params = input
            .params
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let timeout = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut manifest = JobManifest::new(
            self.agent_id.clone(),
            JobType::Tool,
            JobConfig::Tool(ToolJobConfig {
                tool: tool.clone(),
                params,
            }),
            input.label.clone(),
            timeout,
        );
        manifest.retry_policy = retry_policy;
        manifest.stream_config = stream_config;
        manifest.notify_channel = input.notify_channel;
        manifest.notify_thread = input.notify_thread;

        match self.runner.submit(manifest).await {
            Ok(job_id) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "submitted",
                    "type": "tool",
                    "tool": tool,
                    "label": input.label,
                    "retry": input.retry.as_ref().map(|r| serde_json::json!({
                        "max_attempts": r.max_attempts,
                        "backoff": r.backoff,
                    })),
                    "streaming": input.stream.unwrap_or(false),
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to submit job: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.exec` — submit a shell job.
    async fn handle_exec(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let retry_policy = self.build_retry_policy(&input);
        let stream_config = self.build_stream_config(&input);

        let command = match input.command {
            Some(c) => c,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'command'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let timeout = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut manifest = JobManifest::new(
            self.agent_id.clone(),
            JobType::Shell,
            JobConfig::Shell(ShellJobConfig {
                command: command.clone(),
                workdir: input.workdir,
            }),
            input.label.clone(),
            timeout,
        );
        manifest.retry_policy = retry_policy;
        manifest.stream_config = stream_config;
        manifest.notify_channel = input.notify_channel;
        manifest.notify_thread = input.notify_thread;

        match self.runner.submit(manifest).await {
            Ok(job_id) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "submitted",
                    "type": "shell",
                    "command": if command.len() > 80 {
                        format!("{}…", &command[..command.floor_char_boundary(77)])
                    } else {
                        command
                    },
                    "label": input.label,
                    "retry": input.retry.as_ref().map(|r| serde_json::json!({
                        "max_attempts": r.max_attempts,
                        "backoff": r.backoff,
                    })),
                    "streaming": input.stream.unwrap_or(false),
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to submit job: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.monitor` — submit a monitor job.
    async fn handle_monitor(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let retry_policy = self.build_retry_policy(&input);

        let tool = match input.tool {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'tool'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let until = match input.until {
            Some(u) => u,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'until' (condition expression)",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let interval = input
            .interval_secs
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(MIN_POLL_INTERVAL_SECS);
        let max_polls = input.max_polls.unwrap_or(DEFAULT_MAX_POLLS);
        let timeout = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let params = input
            .params
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let mut manifest = JobManifest::new(
            self.agent_id.clone(),
            JobType::Monitor,
            JobConfig::Monitor(MonitorJobConfig {
                tool: tool.clone(),
                params,
                interval_secs: interval,
                until: until.clone(),
                max_polls,
                polls_completed: 0,
                notify_on: NotifyOn::Condition,
                watch_path: None,
            }),
            input.label.clone(),
            timeout,
        );
        manifest.retry_policy = retry_policy;
        manifest.notify_channel = input.notify_channel;
        manifest.notify_thread = input.notify_thread;

        match self.runner.submit(manifest).await {
            Ok(job_id) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "submitted",
                    "type": "monitor",
                    "tool": tool,
                    "interval_secs": interval,
                    "until": until,
                    "max_polls": max_polls,
                    "label": input.label,
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to submit monitor job: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.watch_ci` — CI monitoring preset.
    async fn handle_watch_ci(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let repo = match input.repo {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'repo'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let git_ref = match input.r#ref {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'ref'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let owner = input.owner.unwrap_or_else(|| "driftwerk".to_string());
        let label = input.label.unwrap_or_else(|| {
            format!(
                "CI monitor: {}/{} @ {}",
                owner,
                repo,
                &git_ref[..8.min(git_ref.len())]
            )
        });

        let ci_params = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "ref": git_ref,
        });

        let interval = input
            .interval_secs
            .unwrap_or(30)
            .max(MIN_POLL_INTERVAL_SECS);
        let max_polls = input.max_polls.unwrap_or(DEFAULT_MAX_POLLS);
        let timeout = input.timeout_secs.unwrap_or(1800); // 30 min default for CI.

        let mut manifest = JobManifest::new(
            self.agent_id.clone(),
            JobType::Monitor,
            JobConfig::Monitor(MonitorJobConfig {
                tool: "github.ci_status".to_string(),
                params: ci_params,
                interval_secs: interval,
                until: "overall != \"pending\"".to_string(),
                max_polls,
                polls_completed: 0,
                notify_on: NotifyOn::Condition,
                watch_path: None,
            }),
            Some(label.clone()),
            timeout,
        );
        manifest.notify_channel = input.notify_channel;
        manifest.notify_thread = input.notify_thread;

        match self.runner.submit(manifest).await {
            Ok(job_id) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "submitted",
                    "type": "monitor",
                    "preset": "watch_ci",
                    "label": label,
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to submit CI monitor: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.chain` — submit a chain of jobs.
    async fn handle_chain(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let step_inputs = match input.steps {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing or empty 'steps' array. Provide at least one step.",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Parse steps.
        let mut steps = Vec::new();
        for step_input in &step_inputs {
            match parse_chain_step(step_input) {
                Ok(step) => steps.push(step),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid step: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        }

        match self
            .runner
            .submit_chain(
                steps,
                self.agent_id.clone(),
                input.notify_channel,
                input.notify_thread,
                None,
            )
            .await
        {
            Ok((chain_id, step_jobs)) => Ok(ToolResult::success(
                serde_json::json!({
                    "chain_id": chain_id,
                    "status": "submitted",
                    "steps": step_jobs.iter().map(|(step_id, job_id)| {
                        serde_json::json!({
                            "step_id": step_id,
                            "job_id": job_id,
                        })
                    }).collect::<Vec<_>>(),
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to submit chain: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Whether the calling turn may see `job_id`.
    async fn readable(&self, job_id: &str) -> bool {
        self.runner.readable_by(job_id, &self.reader()).await
    }

    /// Handle `jobs.list`.
    async fn handle_list(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let filter = input.status.as_deref();
        let reader = self.reader();
        let summaries = self.runner.list(filter, &reader).await;
        let chain_summaries = self.runner.list_chains(filter, &reader).await;

        // Limit to 50 most recent for readability.
        let display: Vec<_> = summaries.into_iter().take(50).collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "jobs": display,
                "chains": chain_summaries,
                "active_count": self.runner.active_count().await,
                "queue_depth": self.runner.queue_depth().await,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    /// Handle `jobs.result`.
    async fn handle_result(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'job_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        if !self.readable(&job_id).await {
            return Ok(ToolResult::failure(
                format!("Job '{job_id}' not found"),
                start.elapsed().as_millis() as u64,
            ));
        }
        match self.runner.get_result(&job_id, input.tail).await {
            Ok(result) => Ok(ToolResult::success(
                result,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("{}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.cancel`.
    async fn handle_cancel(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'job_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        if !self.readable(&job_id).await {
            return Ok(ToolResult::failure(
                format!("Job '{}' not found or already in terminal state", job_id),
                start.elapsed().as_millis() as u64,
            ));
        }
        match self.runner.cancel(&job_id).await {
            Ok(true) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "cancelled",
                    "message": "Job cancelled successfully",
                }),
                start.elapsed().as_millis() as u64,
            )),
            Ok(false) => Ok(ToolResult::failure(
                format!("Job '{}' not found or already in terminal state", job_id),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to cancel job: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.chain_cancel` — cancel all jobs in a chain.
    async fn handle_chain_cancel(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let chain_id = match input.chain_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'chain_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let cancelled = match self
            .runner
            .chain_summary_for(&chain_id, &self.reader())
            .await
        {
            Some(_) => self.runner.cancel_chain(&chain_id).await,
            None => Ok(false),
        };
        match cancelled {
            Ok(true) => Ok(ToolResult::success(
                serde_json::json!({
                    "chain_id": chain_id,
                    "status": "cancelled",
                    "message": "Chain and all pending jobs cancelled",
                }),
                start.elapsed().as_millis() as u64,
            )),
            Ok(false) => Ok(ToolResult::failure(
                format!(
                    "Chain '{}' not found or already in terminal state",
                    chain_id
                ),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to cancel chain: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.chain_status` — get chain details.
    async fn handle_chain_status(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let chain_id = match input.chain_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'chain_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match self
            .runner
            .chain_summary_for(&chain_id, &self.reader())
            .await
        {
            Some(summary) => Ok(ToolResult::success(
                serde_json::to_value(&summary).unwrap_or_default(),
                start.elapsed().as_millis() as u64,
            )),
            None => Ok(ToolResult::failure(
                format!("Chain '{}' not found", chain_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.stream` — toggle streaming on/off for a running job.
    async fn handle_stream_toggle(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'job_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let enabled = input.enabled.unwrap_or(true);

        let toggled = match self.readable(&job_id).await {
            true => self.runner.toggle_stream(&job_id, enabled).await,
            false => Ok(false),
        };
        match toggled {
            Ok(true) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "streaming": enabled,
                    "message": format!("Streaming {} for job {}", if enabled { "enabled" } else { "disabled" }, job_id),
                }),
                start.elapsed().as_millis() as u64,
            )),
            Ok(false) => Ok(ToolResult::failure(
                format!(
                    "Job '{}' not found or not running with streaming support",
                    job_id
                ),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to toggle stream: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Handle `jobs.stream_status` — check if streaming is active for a job.
    async fn handle_stream_status(&self, input: JobsInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'job_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let streaming = match self.readable(&job_id).await {
            true => self.runner.stream_status(&job_id).await,
            false => None,
        };
        match streaming {
            Some(enabled) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "streaming": enabled,
                }),
                start.elapsed().as_millis() as u64,
            )),
            None => Ok(ToolResult::failure(
                format!("Job '{}' not found or not running", job_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::always_admin;
    use crate::hub_core::jobs::{AgentExecutor, AgentJobConfig, AgentProfile, AgentRunResult};
    use crate::job_runner::{JobPersistence, ToolExecutor};
    use std::sync::Arc;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::session::TurnPrincipal;

    struct NoTools;

    #[async_trait]
    impl ToolExecutor for NoTools {
        async fn execute_tool(
            &self,
            _tool: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult> {
            Ok(ToolResult::failure("unused", 0))
        }
        fn has_tool(&self, _tool: &str) -> bool {
            false
        }
    }

    /// A sub-agent that never finishes, so its job stays running.
    struct Pending;

    #[async_trait]
    impl AgentExecutor for Pending {
        async fn run_agent_job(
            &self,
            _config: &AgentJobConfig,
            _channel: &str,
        ) -> Result<AgentRunResult> {
            std::future::pending().await
        }
    }

    fn agent_job(run_as: Option<&str>) -> JobConfig {
        JobConfig::Agent(AgentJobConfig::new(
            "read the ledger".to_string(),
            AgentProfile::Worker,
            TurnPrincipal::run_as(run_as.map(str::to_string)),
        ))
    }

    /// A runner holding one sub-agent job of Ada's, one a system lane
    /// started, and a chain whose only step is another job of Ada's.
    async fn runner_with_adas_work() -> (tempfile::TempDir, JobRunner, [String; 3]) {
        let tmp = tempfile::tempdir().unwrap();
        let runner = JobRunner::new(
            "agent".to_string(),
            JobPersistence::new(tmp.path().to_path_buf()),
            None,
            None,
            None,
            None,
            Arc::new(NoTools),
            5,
        );
        runner.set_agent_executor(Arc::new(Pending));
        let submit = |config| {
            runner.submit(JobManifest::new(
                "agent".to_string(),
                JobType::Agent,
                config,
                None,
                600,
            ))
        };
        let hers = submit(agent_job(Some("ada"))).await.unwrap();
        let systems = submit(agent_job(Some("system:heartbeat"))).await.unwrap();
        let (chain, _) = runner
            .submit_chain(
                vec![ChainStep {
                    id: "dig".to_string(),
                    config: agent_job(Some("ada")),
                    depends_on: Vec::new(),
                    label: None,
                    retry: None,
                    timeout_secs: None,
                    stream: false,
                }],
                "agent".to_string(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        (tmp, runner, [hers, systems, chain])
    }

    async fn call(tool: &JobsTool, user: &str, input: serde_json::Value) -> ToolResult {
        CURRENT_USER_ID
            .scope(user.to_string(), tool.execute(input))
            .await
            .unwrap()
    }

    fn listed(result: &ToolResult, key: &str, id_key: &str) -> Vec<String> {
        result.output[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[id_key].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn another_persons_jobs_and_chains_are_not_there_for_anyone_but_them() {
        let (_tmp, runner, [hers, systems, chain]) = runner_with_adas_work().await;
        let tool = JobsTool::new(runner, "agent".to_string());
        let list = serde_json::json!({ "action": "list" });

        let bobs = call(&tool, "bob", list.clone()).await;
        assert_eq!(listed(&bobs, "jobs", "job_id"), [systems]);
        assert!(listed(&bobs, "chains", "chain_id").is_empty());

        let adas = call(&tool, "ada", list).await;
        assert_eq!(listed(&adas, "jobs", "job_id").len(), 3);
        assert_eq!(
            listed(&adas, "chains", "chain_id"),
            std::slice::from_ref(&chain)
        );

        for (input, said) in [
            (
                serde_json::json!({ "action": "result", "job_id": hers }),
                format!("Job '{hers}' not found"),
            ),
            (
                serde_json::json!({ "action": "stream_status", "job_id": hers }),
                format!("Job '{hers}' not found or not running"),
            ),
            (
                serde_json::json!({ "action": "chain_status", "chain_id": chain }),
                format!("Chain '{chain}' not found"),
            ),
            (
                serde_json::json!({ "action": "chain_cancel", "chain_id": chain }),
                format!("Chain '{chain}' not found or already in terminal state"),
            ),
            (
                serde_json::json!({ "action": "cancel", "job_id": hers }),
                format!("Job '{hers}' not found or already in terminal state"),
            ),
        ] {
            let refused = call(&tool, "bob", input.clone()).await;
            assert!(!refused.success, "{input}");
            assert_eq!(refused.error.as_deref(), Some(said.as_str()));
        }

        let own = serde_json::json!({ "action": "chain_status", "chain_id": chain });
        assert!(call(&tool, "ada", own).await.success);
        let cancel = serde_json::json!({ "action": "cancel", "job_id": hers });
        assert!(
            call(&tool, "ada", cancel).await.success,
            "Bob's attempt left her job running for her to cancel"
        );
    }

    /// A shell or tool job a turn submits is that person's, as their
    /// sub-agent jobs are.
    #[tokio::test]
    async fn a_shell_job_is_not_there_for_anyone_but_who_submitted_it() {
        let (_tmp, runner, [_, systems, _]) = runner_with_adas_work().await;
        let tool = JobsTool::new(runner, "agent".to_string());
        let submitted = call(
            &tool,
            "ada",
            serde_json::json!({ "action": "exec", "command": "true" }),
        )
        .await;
        let job = submitted.output["job_id"].as_str().unwrap().to_string();

        let bobs = call(&tool, "bob", serde_json::json!({ "action": "list" })).await;
        assert_eq!(listed(&bobs, "jobs", "job_id"), [systems]);
        let result = serde_json::json!({ "action": "result", "job_id": job });
        let refused = call(&tool, "bob", result.clone()).await;
        assert_eq!(
            refused.error.as_deref(),
            Some(format!("Job '{job}' not found").as_str())
        );
        assert!(call(&tool, "ada", result).await.success);
    }

    #[tokio::test]
    async fn an_administrator_sees_every_job_and_chain() {
        let (_tmp, runner, [hers, _, chain]) = runner_with_adas_work().await;
        let tool = JobsTool::new(runner, "agent".to_string()).with_admin_policy(always_admin());

        let all = call(&tool, "root", serde_json::json!({ "action": "list" })).await;
        assert_eq!(listed(&all, "jobs", "job_id").len(), 3);
        assert_eq!(
            listed(&all, "chains", "chain_id"),
            std::slice::from_ref(&chain)
        );
        let result = serde_json::json!({ "action": "result", "job_id": hers });
        assert!(call(&tool, "root", result).await.success);
        let status = serde_json::json!({ "action": "chain_status", "chain_id": chain });
        assert!(call(&tool, "root", status).await.success);
    }
}

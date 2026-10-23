//! The shapes of the jobs the hub runs: their ids, configs, manifests, chains and outputs.

use crate::hub_core::claims::Tier;
use chrono::DateTime;
use chrono::Utc;
use eyre::Result;
use serde::Deserialize;
use serde::Serialize;
use workforce_common::session::TurnPrincipal;
use workforce_orchestrator::workflow::package::OutputField;
use workforce_orchestrator::workflow::package::VerifySpec;

/// Snapshot of the job-runner's in-memory maps, for memory observability.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobRunnerCacheStats {
    pub jobs: u64,
    pub handles: u64,
    pub queue: u64,
    pub chains: u64,
    pub streamers: u64,
}

/// Unique job identifier.
pub type JobId = String;

/// Unique chain identifier.
pub type ChainId = String;

/// Default stream throttle interval in seconds.
pub const DEFAULT_STREAM_THROTTLE_SECS: u64 = 5;

/// Type of job to execute.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Execute a registered tool with parameters.
    Tool,
    /// Execute a shell command (wraps exec.run).
    Shell,
    /// Poll a tool until a condition is met.
    Monitor,
    /// Run an isolated LLM sub-agent.
    Agent,
}

impl std::fmt::Display for JobType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobType::Tool => write!(f, "tool"),
            JobType::Shell => write!(f, "shell"),
            JobType::Monitor => write!(f, "monitor"),
            JobType::Agent => write!(f, "agent"),
        }
    }
}

/// Status of a job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Job has been submitted but not yet started (waiting for a slot).
    Submitted,
    /// Job is currently executing.
    Running,
    /// Monitor job is actively polling.
    Monitoring,
    /// Job completed successfully.
    Completed,
    /// Job failed (tool error, timeout, crash, hub restart).
    Failed,
    /// Job was cancelled by the user.
    Cancelled,
    /// Job is waiting for a retry after failure.
    Retrying,
    /// Job was cancelled because a dependency failed.
    DependencyFailed,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Submitted => write!(f, "submitted"),
            JobStatus::Running => write!(f, "running"),
            JobStatus::Monitoring => write!(f, "monitoring"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
            JobStatus::Cancelled => write!(f, "cancelled"),
            JobStatus::Retrying => write!(f, "retrying"),
            JobStatus::DependencyFailed => write!(f, "dependency_failed"),
        }
    }
}

impl JobStatus {
    /// Whether the job is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Completed
                | JobStatus::Failed
                | JobStatus::Cancelled
                | JobStatus::DependencyFailed
        )
    }

    /// Whether the job is actively running.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            JobStatus::Running | JobStatus::Monitoring | JobStatus::Submitted | JobStatus::Retrying
        )
    }
}

/// Configuration for a tool job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJobConfig {
    /// Tool name to execute (e.g. "code_build", "github_ci_status").
    pub tool: String,
    /// Parameters to pass to the tool.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Configuration for a shell job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellJobConfig {
    /// Shell command to execute.
    pub command: String,
    /// Working directory.
    #[serde(default)]
    pub workdir: Option<String>,
}

/// When a monitor job notifies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOn {
    /// Notify once when the `until` condition becomes true, then finish.
    #[default]
    Condition,
    /// Notify each time the polled value changes, and keep watching.
    Change,
}

/// Configuration for a monitor job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorJobConfig {
    /// Tool to poll.
    pub tool: String,
    /// Parameters to pass to the tool on each poll.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Poll interval in seconds.
    pub interval_secs: u64,
    /// Condition expression evaluated against tool result (for `Condition`).
    pub until: String,
    /// Maximum number of polls before giving up.
    pub max_polls: u64,
    /// Number of polls completed so far.
    #[serde(default)]
    pub polls_completed: u64,
    /// When to notify: on a condition, or on change.
    #[serde(default)]
    pub notify_on: NotifyOn,
    /// For `Change`: dot-path into the tool result to diff (whole result if absent),
    /// so volatile fields (timestamps, counters) don't trigger spurious notifications.
    #[serde(default)]
    pub watch_path: Option<String>,
}

/// Execution profile for an agent job.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentProfile {
    /// Full agent: complete tool registry and enhanced identity prompt.
    #[default]
    Full,
    /// Lightweight research worker: read-only tool subset, minimal prompt,
    /// cheap model, capped rounds.
    Worker,
    /// Self-learning curation turn: the agent's full round budget and model
    /// (overridable via `[self_learning] model`), but a read-only research
    /// registry plus the single `self_learning.record` write tool. The
    /// registry IS the read-only boundary (see
    /// `crate::serve::AgentService::curation_tool_registry`).
    Curator,
}

/// Configuration for an agent job — runs an isolated LLM sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentJobConfig {
    /// Task the sub-agent should carry out.
    pub task: String,
    /// Execution profile (defaults to a full agent).
    #[serde(default)]
    pub profile: AgentProfile,
    /// Model override; falls back to the profile's default model when absent.
    #[serde(default)]
    pub model: Option<String>,
    /// System-prompt override; falls back to the profile's default prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Identity the sub-agent runs as, captured at submit time because the job
    /// executes in a detached task where the caller's task-locals are gone.
    #[serde(default)]
    pub user_id: Option<String>,
    /// The billing identity the submitting turn carried apart from
    /// `user_id`, captured the same way.
    #[serde(default)]
    pub bill_to: Option<String>,
    /// The step's part in a research chain's adjudication, if it has one —
    /// declared here so the runner never infers it from what upstream steps
    /// happened to produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<ResearchRole>,
    /// The package the sub-agent must hand back and whether a verifier
    /// checks it, when the caller asked for either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandoffSpec>,
    /// The automated source the submitting turn descends from
    /// ([`crate::hub_core::origins::automated_origin`]), captured like `user_id`: the
    /// sub-agent's own session no longer names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automated_origin: Option<String>,
}

impl AgentJobConfig {
    /// A job running `task` as `principal`, on the profile's default model
    /// and prompt.
    pub fn new(task: String, profile: AgentProfile, principal: TurnPrincipal) -> Self {
        Self {
            task,
            profile,
            model: None,
            system_prompt: None,
            user_id: principal.run_as,
            bill_to: principal.bill_to,
            research: None,
            handoff: None,
            automated_origin: crate::hub_core::origins::automated_origin(),
        }
    }

    /// Who the job runs and bills as.
    pub fn principal(&self) -> TurnPrincipal {
        TurnPrincipal {
            run_as: self.user_id.clone(),
            bill_to: self.bill_to.clone(),
        }
    }
}

/// Who asks to see a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobReader {
    /// An administrator, who sees every job.
    Admin,
    /// Anyone else, who sees the jobs run for them.
    Person(TurnPrincipal),
}

impl JobReader {
    /// The turn in scope as a reader; `is_admin` is the shared admin gate's
    /// answer for it.
    pub fn of_turn(is_admin: bool) -> Self {
        Self::new(is_admin, crate::user_config::current_principal())
    }

    pub fn new(is_admin: bool, principal: TurnPrincipal) -> Self {
        match is_admin {
            true => Self::Admin,
            false => Self::Person(principal),
        }
    }
}

/// The specific job configuration (union of job types).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobConfig {
    Tool(ToolJobConfig),
    Shell(ShellJobConfig),
    Monitor(MonitorJobConfig),
    Agent(AgentJobConfig),
}

/// Backoff strategy for retry policies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackoffStrategy {
    /// Fixed delay between retries.
    Fixed,
    /// Linearly increasing delay: initial * attempt_number.
    Linear,
    /// Exponentially increasing delay: initial * 2^(attempt-1).
    Exponential,
}

impl std::fmt::Display for BackoffStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackoffStrategy::Fixed => write!(f, "fixed"),
            BackoffStrategy::Linear => write!(f, "linear"),
            BackoffStrategy::Exponential => write!(f, "exponential"),
        }
    }
}

/// Types of failures that can trigger a retry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureType {
    /// Job timed out.
    Timeout,
    /// Shell command returned non-zero exit code.
    NonZeroExit,
    /// Tool execution returned an error.
    ToolError,
}

/// Retry policy configuration for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of total attempts (including the first). 1 = no retry.
    pub max_attempts: u32,
    /// Backoff strategy between retries.
    pub backoff: BackoffStrategy,
    /// Initial delay in seconds before the first retry.
    pub initial_delay_secs: u64,
    /// Which failure types trigger a retry. Empty = all retryable failures.
    #[serde(default)]
    pub retry_on: Vec<FailureType>,
    /// Add random jitter to backoff delays (up to 25% of the computed delay).
    #[serde(default)]
    pub jitter: bool,
}

impl RetryPolicy {
    /// Calculate the delay in seconds before the given attempt number (1-indexed).
    /// Attempt 1 is the first *retry* (second overall execution).
    pub fn delay_for_attempt(&self, retry_attempt: u32) -> u64 {
        let base = match self.backoff {
            BackoffStrategy::Fixed => self.initial_delay_secs,
            BackoffStrategy::Linear => self.initial_delay_secs * (retry_attempt as u64),
            BackoffStrategy::Exponential => {
                self.initial_delay_secs * 2u64.saturating_pow(retry_attempt.saturating_sub(1))
            }
        };

        if self.jitter {
            // Add up to 25% jitter.
            let jitter_range = base / 4;
            if jitter_range > 0 {
                let jitter = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64)
                    % jitter_range;
                base + jitter
            } else {
                base
            }
        } else {
            base
        }
    }

    /// Check if the given failure type should trigger a retry.
    pub fn should_retry(&self, failure: &FailureType) -> bool {
        if self.retry_on.is_empty() {
            // Empty means retry on all retryable types.
            true
        } else {
            self.retry_on.contains(failure)
        }
    }
}

/// State of retry attempts for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryState {
    /// Current attempt number (1 = first try, 2 = first retry, etc.).
    pub current_attempt: u32,
    /// Timestamp of the next retry (if waiting).
    #[serde(default)]
    pub next_retry_at: Option<DateTime<Utc>>,
    /// History of all attempts.
    #[serde(default)]
    pub attempts: Vec<JobAttempt>,
}

impl Default for RetryState {
    fn default() -> Self {
        Self {
            current_attempt: 1,
            next_retry_at: None,
            attempts: Vec::new(),
        }
    }
}

/// Record of a single job attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAttempt {
    /// Attempt number (1-indexed).
    pub attempt: u32,
    /// When this attempt started.
    pub started_at: DateTime<Utc>,
    /// When this attempt completed.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// Status of this attempt.
    pub status: AttemptStatus,
    /// Error message if the attempt failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Path to the output file for this attempt.
    #[serde(default)]
    pub output_path: Option<String>,
}

/// Status of a single retry attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Attempt is currently running.
    Running,
    /// Attempt succeeded.
    Succeeded,
    /// Attempt failed.
    Failed,
    /// Attempt timed out.
    TimedOut,
}

/// Configuration for output streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    /// Whether streaming is enabled for this job.
    pub enabled: bool,
    /// Minimum interval between progress updates in seconds.
    #[serde(default = "default_throttle_secs")]
    pub throttle_secs: u64,
}

pub fn default_throttle_secs() -> u64 {
    DEFAULT_STREAM_THROTTLE_SECS
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            throttle_secs: DEFAULT_STREAM_THROTTLE_SECS,
        }
    }
}

/// A step in a job chain — defines a single job with its dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    /// Unique step ID within the chain (user-provided, e.g. "build", "test").
    pub id: String,
    /// The job configuration for this step.
    pub config: JobConfig,
    /// Step IDs that this step depends on. Empty = root step.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Optional human-readable label for the job.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional retry policy for this step.
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
    /// Optional timeout override for this step.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Whether to stream output for this step.
    #[serde(default)]
    pub stream: bool,
}

/// Full job manifest — persisted to disk and held in memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobManifest {
    /// Unique job identifier.
    pub job_id: JobId,
    /// Agent that owns this job.
    pub agent_id: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: Option<String>,
    /// Job type.
    pub job_type: JobType,
    /// Current status.
    pub status: JobStatus,
    /// Job-specific configuration.
    pub config: JobConfig,
    /// Timeout in seconds.
    pub timeout_secs: u64,
    /// When the job was submitted.
    pub submitted_at: DateTime<Utc>,
    /// When the job started executing.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// When the job completed (success, failure, or cancellation).
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// Error message if the job failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Exit code for shell jobs.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Session targeting: the session that submitted this job.
    #[serde(default)]
    pub session_channel: Option<String>,
    /// Channel for notification delivery.
    #[serde(default)]
    pub notify_channel: Option<String>,
    /// Thread for notification delivery.
    #[serde(default)]
    pub notify_thread: Option<String>,
    /// Chain ID if this job belongs to a chain.
    #[serde(default)]
    pub chain_id: Option<ChainId>,
    /// Step ID within the chain (if chained).
    #[serde(default)]
    pub chain_step_id: Option<String>,
    /// Retry policy (if configured).
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,
    /// Retry state (current attempt, history, next retry time).
    #[serde(default)]
    pub retry_state: Option<RetryState>,
    /// Stream configuration.
    #[serde(default)]
    pub stream_config: Option<StreamConfig>,
    /// Context from upstream chain jobs (output references).
    #[serde(default)]
    pub upstream_context: Option<serde_json::Value>,
    /// Who a tool, shell or monitor job runs as and is billed to, captured
    /// from the turn that submitted it, since the job executes in a detached
    /// task the turn's identity does not reach. An agent job keeps its own in
    /// its config. A manifest written before this was kept names nobody.
    #[serde(default)]
    pub submitted_by: TurnPrincipal,
}

impl JobManifest {
    /// Create a new job manifest.
    pub fn new(
        agent_id: String,
        job_type: JobType,
        config: JobConfig,
        label: Option<String>,
        timeout_secs: u64,
    ) -> Self {
        let job_id = format!("j-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let submitted_by = match config {
            JobConfig::Agent(_) => TurnPrincipal::default(),
            _ => crate::user_config::current_principal(),
        };
        Self {
            job_id,
            agent_id,
            label,
            job_type,
            status: JobStatus::Submitted,
            config,
            timeout_secs,
            submitted_at: Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            exit_code: None,
            session_channel: None,
            notify_channel: None,
            notify_thread: None,
            chain_id: None,
            chain_step_id: None,
            retry_policy: None,
            retry_state: None,
            stream_config: None,
            upstream_context: None,
            submitted_by,
        }
    }

    /// Who the job runs as and is billed to.
    pub fn principal(&self) -> TurnPrincipal {
        match &self.config {
            JobConfig::Agent(job) => job.principal(),
            _ => self.submitted_by.clone(),
        }
    }

    /// Elapsed seconds since submission.
    pub fn elapsed_secs(&self) -> f64 {
        let end = self.completed_at.unwrap_or_else(Utc::now);
        (end - self.submitted_at).num_milliseconds() as f64 / 1000.0
    }

    /// Format elapsed time as human-readable string.
    pub fn elapsed_display(&self) -> String {
        let secs = self.elapsed_secs();
        if secs < 60.0 {
            format!("{:.1}s", secs)
        } else if secs < 3600.0 {
            let mins = (secs / 60.0).floor();
            let remaining = secs - mins * 60.0;
            format!("{}m {:.0}s", mins as u64, remaining)
        } else {
            let hours = (secs / 3600.0).floor();
            let remaining_mins = ((secs - hours * 3600.0) / 60.0).floor();
            format!("{}h {}m", hours as u64, remaining_mins as u64)
        }
    }

    /// Classify the failure type for retry decisions.
    pub fn classify_failure(&self) -> Option<FailureType> {
        if self.status != JobStatus::Failed {
            return None;
        }
        if let Some(ref err) = self.error {
            let lower = err.to_lowercase();
            if lower.contains("timed out") || lower.contains("timeout") {
                return Some(FailureType::Timeout);
            }
        }
        // Check exit code for shell jobs.
        if let Some(code) = self.exit_code {
            if code != 0 {
                return Some(FailureType::NonZeroExit);
            }
        }
        // Default to tool error.
        Some(FailureType::ToolError)
    }

    /// Whether `reader` may see this job and what it produced: an
    /// administrator, or a person the job ran as or is billed to. A job
    /// naming no person — one a `system:`/`agent:` lane or a turn bound to
    /// nobody started — stays visible to the agent's users, as a workflow run
    /// naming nobody does.
    pub fn readable_by(&self, reader: &JobReader) -> bool {
        match reader {
            JobReader::Admin => true,
            JobReader::Person(reader) => self.principal().readable_by(reader),
        }
    }

    /// Task text for an `Agent` job (falls back to the label for other kinds).
    pub fn agent_task_text(&self) -> String {
        match &self.config {
            JobConfig::Agent(a) => a.task.clone(),
            _ => self.label.clone().unwrap_or_default(),
        }
    }

    /// Model override recorded for an `Agent` job, if any.
    pub fn agent_model(&self) -> Option<&str> {
        match &self.config {
            JobConfig::Agent(a) => a.model.as_deref(),
            _ => None,
        }
    }

    /// Sub-agent-facing status label, distinguishing timeouts from other
    /// failures (mirrors the legacy `SubAgentStatus` vocabulary).
    pub fn agent_status_label(&self) -> &'static str {
        match self.status {
            JobStatus::Submitted
            | JobStatus::Running
            | JobStatus::Monitoring
            | JobStatus::Retrying => "running",
            JobStatus::Completed => "completed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed | JobStatus::DependencyFailed => {
                let err = self.error.as_deref().unwrap_or_default().to_lowercase();
                if err.contains("cancelled") {
                    "cancelled"
                } else if matches!(self.classify_failure(), Some(FailureType::Timeout)) {
                    "timed_out"
                } else {
                    "failed"
                }
            }
        }
    }

    /// Format retry status for display (e.g. "retrying (attempt 2/3)").
    pub fn retry_display(&self) -> Option<String> {
        let policy = self.retry_policy.as_ref()?;
        let state = self.retry_state.as_ref()?;
        if policy.max_attempts > 1 {
            Some(format!(
                "attempt {}/{}",
                state.current_attempt, policy.max_attempts
            ))
        } else {
            None
        }
    }
}

/// Job output stored to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutput {
    /// Job ID.
    pub job_id: JobId,
    /// The tool's result (for tool/monitor jobs).
    #[serde(default)]
    pub tool_result: Option<serde_json::Value>,
    /// Stdout (for shell jobs).
    #[serde(default)]
    pub stdout: Option<String>,
    /// Stderr (for shell jobs).
    #[serde(default)]
    pub stderr: Option<String>,
    /// Exit code (for shell jobs).
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Full output text (for truncation/tailing).
    #[serde(default)]
    pub full_output: String,
    /// Attempt number this output belongs to (for retried jobs).
    #[serde(default)]
    pub attempt: Option<u32>,
}

impl JobOutput {
    /// Get the last N lines of the full output.
    pub fn tail(&self, n: usize) -> String {
        let lines: Vec<&str> = self.full_output.lines().collect();
        if lines.len() <= n {
            self.full_output.clone()
        } else {
            lines[lines.len() - n..].join("\n")
        }
    }
}

/// Summary of a chain for listing.
#[derive(Debug, serde::Serialize)]
pub struct ChainSummary {
    pub chain_id: String,
    pub status: crate::hub_core::jobs::ChainStatus,
    pub total_steps: usize,
    pub completed_steps: usize,
    pub failed_steps: usize,
    pub running_steps: usize,
    pub pending_steps: usize,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
    pub elapsed_secs: f64,
    pub steps: Vec<StepSummary>,
}

#[derive(Debug, serde::Serialize)]
pub struct StepSummary {
    pub step_id: String,
    pub job_id: String,
    pub status: JobStatus,
    pub depends_on: Vec<String>,
}

impl ChainSummary {
    pub fn from_chain(chain: &JobChain) -> Self {
        let elapsed = chain
            .completed_at
            .unwrap_or_else(chrono::Utc::now)
            .signed_duration_since(chain.submitted_at)
            .num_milliseconds() as f64
            / 1000.0;

        let steps: Vec<StepSummary> = chain
            .step_jobs
            .iter()
            .map(|s| {
                let depends_on = chain
                    .upstream_steps(&s.step_id)
                    .iter()
                    .map(|d| d.to_string())
                    .collect();
                StepSummary {
                    step_id: s.step_id.clone(),
                    job_id: s.job_id.clone(),
                    status: s.status.clone(),
                    depends_on,
                }
            })
            .collect();

        ChainSummary {
            chain_id: chain.id.clone(),
            status: chain.compute_status(),
            total_steps: chain.step_jobs.len(),
            completed_steps: chain
                .step_jobs
                .iter()
                .filter(|s| s.status == JobStatus::Completed)
                .count(),
            failed_steps: chain
                .step_jobs
                .iter()
                .filter(|s| matches!(s.status, JobStatus::Failed | JobStatus::DependencyFailed))
                .count(),
            running_steps: chain
                .step_jobs
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        JobStatus::Running | JobStatus::Monitoring | JobStatus::Retrying
                    )
                })
                .count(),
            pending_steps: chain
                .step_jobs
                .iter()
                .filter(|s| s.status == JobStatus::Submitted)
                .count(),
            submitted_at: chain.submitted_at,
            elapsed_secs: elapsed,
            steps,
        }
    }
}

/// A research chain step's part in adjudication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ResearchRole {
    /// Try to prove the findings of the researcher step `target` wrong.
    Challenge { target: String },
    /// Write the report from the adjudicated findings of `angles`.
    Synthesise {
        angles: Vec<Angle>,
        low_confidence_below: u8,
    },
}

/// One investigated angle: its researcher step and, in adversarial mode, the
/// challenger step attacking it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Angle {
    pub label: String,
    pub researcher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenger: Option<String>,
}

/// What a sub-agent call asks of its result: a package schema, a verifier, or
/// both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSpec {
    /// The package the sub-agent must return, declared as a step's `outputs`
    /// are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySpec>,
}

impl HandoffSpec {
    /// Why the spec cannot be honoured, if it cannot.
    pub fn defect(&self) -> Option<String> {
        let mut names = std::collections::HashSet::new();
        self.outputs
            .iter()
            .find(|field| field.name.trim().is_empty() || !names.insert(field.name.as_str()))
            .map(|field| format!("output '{}' is blank or declared twice", field.name))
            .or_else(|| self.outputs.iter().find_map(OutputField::defect))
            .or_else(|| self.verify.as_ref().and_then(VerifySpec::defect))
    }
}

/// Status of a job chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChainStatus {
    /// Chain has been submitted, jobs are pending/running.
    Pending,
    /// At least one job in the chain is running.
    Running,
    /// All jobs completed successfully.
    Completed,
    /// Some jobs completed, some failed (not all downstream cancelled).
    PartialFailure,
    /// A root or critical job failed, causing cascading cancellation.
    Failed,
    /// Chain was explicitly cancelled.
    Cancelled,
}

impl std::fmt::Display for ChainStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainStatus::Pending => write!(f, "pending"),
            ChainStatus::Running => write!(f, "running"),
            ChainStatus::Completed => write!(f, "completed"),
            ChainStatus::PartialFailure => write!(f, "partial_failure"),
            ChainStatus::Failed => write!(f, "failed"),
            ChainStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl ChainStatus {
    /// Whether the chain is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ChainStatus::Completed
                | ChainStatus::PartialFailure
                | ChainStatus::Failed
                | ChainStatus::Cancelled
        )
    }
}

/// A job chain — a DAG of jobs with dependency relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobChain {
    /// Unique chain identifier.
    pub id: ChainId,
    /// Agent that submitted this chain.
    pub agent_id: String,
    /// Mapping from step ID → assigned job ID (filled in on submission).
    pub step_jobs: Vec<StepJobMapping>,
    /// Dependency edges: (dependency_step_id, dependent_step_id).
    pub edges: Vec<(String, String)>,
    /// Current chain status.
    pub status: ChainStatus,
    /// When the chain was submitted.
    pub submitted_at: DateTime<Utc>,
    /// When the chain completed (all jobs done).
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// Session channel for notification targeting.
    #[serde(default)]
    pub session_channel: Option<String>,
    /// Channel for notification delivery.
    #[serde(default)]
    pub notify_channel: Option<String>,
    /// Thread for notification delivery.
    #[serde(default)]
    pub notify_thread: Option<String>,
}

/// Mapping from a chain step ID to its assigned job ID and status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepJobMapping {
    /// Step ID within the chain.
    pub step_id: String,
    /// Assigned job ID.
    pub job_id: JobId,
    /// Current status of this step's job.
    pub status: JobStatus,
}

impl JobChain {
    /// Compute the aggregated chain status from step statuses.
    pub fn compute_status(&self) -> ChainStatus {
        let all_terminal = self.step_jobs.iter().all(|s| s.status.is_terminal());
        if !all_terminal {
            // Check if any are running.
            let any_running = self.step_jobs.iter().any(|s| {
                matches!(
                    s.status,
                    JobStatus::Running | JobStatus::Monitoring | JobStatus::Retrying
                )
            });
            if any_running {
                return ChainStatus::Running;
            }
            return ChainStatus::Pending;
        }

        // All terminal — determine outcome.
        let any_failed = self
            .step_jobs
            .iter()
            .any(|s| matches!(s.status, JobStatus::Failed | JobStatus::DependencyFailed));
        let any_cancelled = self
            .step_jobs
            .iter()
            .any(|s| s.status == JobStatus::Cancelled);
        let all_completed = self
            .step_jobs
            .iter()
            .all(|s| s.status == JobStatus::Completed);

        if all_completed {
            ChainStatus::Completed
        } else if any_cancelled && !any_failed {
            ChainStatus::Cancelled
        } else {
            // Mix of failures and completions.
            let any_completed = self
                .step_jobs
                .iter()
                .any(|s| s.status == JobStatus::Completed);
            if any_completed && any_failed {
                ChainStatus::PartialFailure
            } else {
                ChainStatus::Failed
            }
        }
    }

    /// Get the job ID for a step ID.
    pub fn job_id_for_step(&self, step_id: &str) -> Option<&str> {
        self.step_jobs
            .iter()
            .find(|s| s.step_id == step_id)
            .map(|s| s.job_id.as_str())
    }

    /// Get the step ID for a job ID.
    pub fn step_id_for_job(&self, job_id: &str) -> Option<&str> {
        self.step_jobs
            .iter()
            .find(|s| s.job_id == job_id)
            .map(|s| s.step_id.as_str())
    }

    /// Get downstream step IDs that depend on the given step.
    pub fn downstream_steps(&self, step_id: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|(dep, _)| dep == step_id)
            .map(|(_, dependent)| dependent.as_str())
            .collect()
    }

    /// Get upstream step IDs that the given step depends on.
    pub fn upstream_steps(&self, step_id: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|(_, dependent)| dependent == step_id)
            .map(|(dep, _)| dep.as_str())
            .collect()
    }

    /// Update a step's status by job ID.
    pub fn update_step_status(&mut self, job_id: &str, status: JobStatus) {
        if let Some(step) = self.step_jobs.iter_mut().find(|s| s.job_id == job_id) {
            step.status = status;
        }
    }
}

/// Result of running a sub-agent: its final response, plus — when it recorded
/// findings (worker profile) — their staged envelope and the structured set,
/// and the validated package its caller declared, when it declared one.
#[derive(Debug, Default)]
pub struct AgentRunResult {
    pub response: String,
    pub findings: Option<serde_json::Value>,
    pub claims: Vec<crate::hub_core::jobs::Finding>,
    pub package: Option<serde_json::Value>,
}

impl AgentRunResult {
    /// This run as the one that follows `earlier` on the same task: its own
    /// answer, with `earlier`'s findings where it recorded none itself.
    pub fn after(self, earlier: Self) -> Self {
        Self {
            findings: self.findings.or(earlier.findings),
            claims: match self.claims.is_empty() {
                true => earlier.claims,
                false => self.claims,
            },
            ..self
        }
    }

    /// What a downstream chain step reads of this run: the findings handle
    /// (`findings`), the structured findings (`claims`) and the package it
    /// handed back (`package`).
    pub fn tool_result(&self) -> Option<serde_json::Value> {
        let mut out = serde_json::Map::new();
        if let Some(staged) = &self.findings {
            out.insert("findings".to_string(), staged.clone());
        }
        if !self.claims.is_empty() {
            out.insert("claims".to_string(), serde_json::json!(self.claims));
        }
        if let Some(package) = &self.package {
            out.insert("package".to_string(), package.clone());
        }
        (!out.is_empty()).then_some(serde_json::Value::Object(out))
    }
}

/// One recorded finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub claim: String,
    /// 0–100. `None` when the worker gave none, or none that reads as a
    /// number on that scale — treated as the lowest.
    #[serde(default)]
    pub confidence: Option<u8>,
    /// `file:line`, URL or `tool:<name> <what>` locators; only well-formed ones
    /// are kept.
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Set when the finding is a challenger's verdict on another finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<Challenge>,
}

impl Finding {
    /// Whether `other` records the same claim (about the same target) again,
    /// as a worker does to fix a refused citation or revise its confidence.
    pub fn same_as(&self, other: &Finding) -> bool {
        let key = |f: &Finding| {
            f.claim
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        let target = |f: &Finding| f.challenge.as_ref().map(|c| c.target.clone());
        key(self) == key(other) && target(self) == target(other)
    }

    /// Fold a later record of the same claim into this one: the latest
    /// wording, confidence, evidence and verdict, and every citation either
    /// gave — so restating a claim never costs it the citations it had.
    pub fn restate(&mut self, later: Finding) {
        let mut citations = std::mem::take(&mut self.citations);
        citations.extend(later.citations.iter().cloned());
        let mut seen = std::collections::HashSet::new();
        citations.retain(|c| seen.insert(c.clone()));
        citations.truncate(CITATIONS_KEPT);
        *self = Finding {
            citations,
            evidence: if later.evidence.is_empty() {
                std::mem::take(&mut self.evidence)
            } else {
                later.evidence
            },
            ..later
        };
    }
}

/// A challenger's verdict on the finding with id `target`, in claim
/// grounding's tiers: `Verified` (it holds), `Contradicted` (refuted),
/// `Unverified` (could not be settled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    pub target: String,
    pub verdict: Tier,
}

pub const CITATIONS_KEPT: usize = 10;

/// Trait for running an isolated LLM sub-agent as a job.
///
/// Late-bound via [`JobRunner::set_agent_executor`] because the concrete
/// implementation needs the fully-built `AgentService`, which does not exist
/// when the runner is constructed.
#[async_trait::async_trait]
pub trait AgentExecutor: Send + Sync {
    /// Run a sub-agent for `config` in the isolated session `channel`.
    async fn run_agent_job(&self, config: &AgentJobConfig, channel: &str)
        -> Result<AgentRunResult>;
}

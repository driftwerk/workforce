//! Core types for the async job runner.

pub use crate::hub_core::jobs::{
    AgentJobConfig, AgentProfile, AttemptStatus, BackoffStrategy, ChainId, ChainStep, FailureType,
    JobAttempt, JobConfig, JobId, JobManifest, JobOutput, JobReader, JobStatus, JobType,
    MonitorJobConfig, NotifyOn, RetryPolicy, RetryState, ShellJobConfig, StreamConfig,
    ToolJobConfig, DEFAULT_STREAM_THROTTLE_SECS,
};
pub use crate::hub_core::jobs::{ChainStatus, JobChain, StepJobMapping};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Maximum concurrent jobs per agent (default).
pub const DEFAULT_CONCURRENT_LIMIT: usize = 5;

/// Default job timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Default monitor poll interval in seconds.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

/// Minimum monitor poll interval in seconds.
pub const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// Default maximum poll count for monitor jobs.
pub const DEFAULT_MAX_POLLS: u64 = 60;

/// Maximum lines of output included in notifications.
pub const NOTIFICATION_TAIL_LINES: usize = 50;

// ── Retry Types ──────────────────────────────────────────────────────

// ── Streaming Types ──────────────────────────────────────────────────

/// Progress payload published via NATS for streaming output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProgress {
    /// Job ID.
    pub job_id: JobId,
    /// Lines of output since last update.
    pub lines: Vec<String>,
    /// Timestamp of this update.
    pub timestamp: DateTime<Utc>,
    /// Total bytes of output produced so far.
    pub bytes_total: u64,
    /// Elapsed seconds since job started.
    pub elapsed_secs: f64,
}

// ── Chain Types ──────────────────────────────────────────────────────

/// Notification payload published via JetStream when a job completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobNotification {
    /// Job ID.
    pub job_id: JobId,
    /// Agent ID that owns the job.
    pub agent_id: String,
    /// Job type.
    pub job_type: JobType,
    /// Human-readable label.
    pub label: Option<String>,
    /// Final status.
    pub status: JobStatus,
    /// Success indicator.
    pub success: bool,
    /// Elapsed time in seconds.
    pub elapsed_secs: f64,
    /// Truncated output summary (last N lines).
    pub output_summary: String,
    /// Exit code for shell jobs.
    pub exit_code: Option<i32>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Monitor poll count (for monitor jobs).
    pub polls: Option<u64>,
    /// Session channel for notification targeting.
    pub session_channel: Option<String>,
    /// Channel for notification delivery.
    pub notify_channel: Option<String>,
    /// Thread for notification delivery.
    pub notify_thread: Option<String>,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
    /// Chain ID (if part of a chain).
    #[serde(default)]
    pub chain_id: Option<ChainId>,
    /// Retry attempt info (if retried).
    #[serde(default)]
    pub retry_info: Option<String>,
}

/// Summary of a job for listing.
#[derive(Debug, Serialize)]
pub struct JobSummary {
    pub job_id: JobId,
    pub agent_id: String,
    #[serde(rename = "type")]
    pub job_type: JobType,
    pub label: Option<String>,
    pub status: JobStatus,
    pub submitted_at: DateTime<Utc>,
    pub elapsed_secs: f64,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// For monitor jobs: polls completed / max polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polls: Option<String>,
    /// Tool or command being executed.
    pub detail: String,
    /// Chain ID (if part of a chain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<ChainId>,
    /// Retry status display (e.g. "attempt 2/3").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_info: Option<String>,
    /// Whether streaming is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
}

impl From<&JobManifest> for JobSummary {
    fn from(m: &JobManifest) -> Self {
        let (polls, detail) = match &m.config {
            JobConfig::Tool(t) => (None, format!("tool:{}", t.tool)),
            JobConfig::Shell(s) => {
                let cmd = if s.command.len() > 60 {
                    format!("{}…", &s.command[..s.command.floor_char_boundary(57)])
                } else {
                    s.command.clone()
                };
                (None, format!("shell:{}", cmd))
            }
            JobConfig::Monitor(mon) => {
                let polls = Some(format!("{}/{}", mon.polls_completed, mon.max_polls));
                let detail = match mon.notify_on {
                    NotifyOn::Change => {
                        let path = mon.watch_path.as_deref().unwrap_or("*");
                        format!("watch:{} on change of {}", mon.tool, path)
                    }
                    NotifyOn::Condition => format!("monitor:{} until '{}'", mon.tool, mon.until),
                };
                (polls, detail)
            }
            JobConfig::Agent(a) => {
                let task = if a.task.len() > 60 {
                    format!("{}…", &a.task[..a.task.floor_char_boundary(57)])
                } else {
                    a.task.clone()
                };
                (None, format!("agent:{}", task))
            }
        };

        let streaming = m.stream_config.as_ref().map(|sc| sc.enabled).filter(|&e| e);

        JobSummary {
            job_id: m.job_id.clone(),
            agent_id: m.agent_id.clone(),
            job_type: m.job_type.clone(),
            label: m.label.clone(),
            status: m.status.clone(),
            submitted_at: m.submitted_at,
            elapsed_secs: m.elapsed_secs(),
            exit_code: m.exit_code,
            error: m.error.clone(),
            polls,
            detail,
            chain_id: m.chain_id.clone(),
            retry_info: m.retry_display(),
            streaming,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::TurnPrincipal;

    fn agent_manifest(status: JobStatus, error: Option<&str>) -> JobManifest {
        let mut m = JobManifest::new(
            "a".to_string(),
            JobType::Agent,
            JobConfig::Agent(AgentJobConfig::new(
                "t".to_string(),
                AgentProfile::Full,
                TurnPrincipal::default(),
            )),
            None,
            600,
        );
        m.status = status;
        m.error = error.map(|e| e.to_string());
        m
    }

    #[test]
    fn agent_profile_defaults_to_full_when_absent() {
        let cfg: AgentJobConfig =
            serde_json::from_value(serde_json::json!({ "task": "x" })).unwrap();
        assert_eq!(cfg.profile, AgentProfile::Full);
        let worker: AgentProfile = serde_json::from_value(serde_json::json!("worker")).unwrap();
        assert_eq!(worker, AgentProfile::Worker);
    }

    #[test]
    fn an_agent_job_is_readable_by_the_person_it_ran_for_and_an_administrator() {
        let job_of = |run_as: Option<&str>, bill_to: Option<&str>| {
            let mut m = agent_manifest(JobStatus::Completed, None);
            if let JobConfig::Agent(a) = &mut m.config {
                a.user_id = run_as.map(str::to_string);
                a.bill_to = bill_to.map(str::to_string);
            }
            m
        };
        let reader = |id: &str| JobReader::new(false, TurnPrincipal::run_as(Some(id.to_string())));
        let nobody = JobReader::new(false, TurnPrincipal::default());

        let hers = job_of(Some("ada"), Some("team-lead"));
        assert!(hers.readable_by(&reader("ada")));
        assert!(hers.readable_by(&reader("team-lead")));
        assert!(!hers.readable_by(&reader("bob")));
        assert!(!hers.readable_by(&nobody));
        assert!(hers.readable_by(&JobReader::new(
            true,
            TurnPrincipal::run_as(Some("bob".to_string()))
        )));

        assert!(job_of(None, None).readable_by(&reader("bob")));
        assert!(job_of(Some("system:heartbeat"), None).readable_by(&nobody));
    }

    /// A tool, shell or monitor job is the submitting turn's, as an agent job
    /// is its caller's: it runs as that person and only they and an
    /// administrator see it.
    #[tokio::test]
    async fn a_tool_or_shell_job_is_the_submitting_persons() {
        let shell = || {
            JobManifest::new(
                "a".to_string(),
                JobType::Shell,
                JobConfig::Shell(ShellJobConfig {
                    command: "true".to_string(),
                    workdir: None,
                }),
                None,
                600,
            )
        };
        let reader = |id: &str| JobReader::new(false, TurnPrincipal::run_as(Some(id.to_string())));

        let hers = crate::user_config::as_principal(
            &TurnPrincipal::run_as(Some("ada".to_string())),
            async { shell() },
        )
        .await;
        assert_eq!(hers.principal(), TurnPrincipal::run_as(Some("ada".into())));
        assert!(hers.readable_by(&reader("ada")));
        assert!(!hers.readable_by(&reader("bob")));
        assert!(hers.readable_by(&JobReader::Admin));

        assert!(shell().readable_by(&reader("bob")), "submitted by nobody");

        let mut kept = serde_json::to_value(&hers).unwrap();
        kept.as_object_mut().unwrap().remove("submitted_by");
        let earlier: JobManifest = serde_json::from_value(kept).unwrap();
        assert_eq!(earlier.principal(), TurnPrincipal::default());
        assert!(
            earlier.readable_by(&reader("bob")),
            "kept before jobs named anyone"
        );
    }

    #[test]
    fn agent_task_text_and_model_read_config() {
        let mut m = agent_manifest(JobStatus::Running, None);
        if let JobConfig::Agent(a) = &mut m.config {
            a.model = Some("haiku".to_string());
        }
        assert_eq!(m.agent_task_text(), "t");
        assert_eq!(m.agent_model(), Some("haiku"));
    }

    #[test]
    fn agent_status_label_maps_terminal_states() {
        let cases = [
            (JobStatus::Submitted, None, "running"),
            (JobStatus::Running, None, "running"),
            (JobStatus::Completed, None, "completed"),
            (JobStatus::Cancelled, None, "cancelled"),
            (
                JobStatus::Failed,
                Some("Job timed out after 600s"),
                "timed_out",
            ),
            (JobStatus::Failed, Some("Job cancelled"), "cancelled"),
            (JobStatus::Failed, Some("boom"), "failed"),
        ];
        for (status, error, expected) in cases {
            assert_eq!(
                agent_manifest(status, error).agent_status_label(),
                expected,
                "status {error:?}"
            );
        }
    }
}

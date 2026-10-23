//! Retry logic for jobs — backoff strategies, retry decisions, and state management.

use chrono::{Duration, Utc};
use tracing::info;

use super::types::{AttemptStatus, JobAttempt, JobManifest, JobStatus, RetryState};

/// Determine whether a failed job should be retried.
///
/// Returns `true` if:
/// - The job has a retry policy with max_attempts > 1
/// - The current attempt is less than max_attempts
/// - The failure type is retryable per the policy
/// - The failure is not a cancellation or dependency failure
pub fn should_retry(manifest: &JobManifest) -> bool {
    let policy = match &manifest.retry_policy {
        Some(p) if p.max_attempts > 1 => p,
        _ => return false,
    };

    let default_state = RetryState::default();
    let state = manifest.retry_state.as_ref().unwrap_or(&default_state);

    // Don't retry if we've exhausted attempts.
    if state.current_attempt >= policy.max_attempts {
        return false;
    }

    // Don't retry cancellations or dependency failures.
    if matches!(
        manifest.status,
        JobStatus::Cancelled | JobStatus::DependencyFailed
    ) {
        return false;
    }

    // Check if the failure type is retryable.
    match manifest.classify_failure() {
        Some(failure_type) => policy.should_retry(&failure_type),
        None => false, // Not a failure — shouldn't retry.
    }
}

/// Prepare a manifest for retry: update status, increment attempt,
/// schedule next retry time, and record the failed attempt.
///
/// Returns the updated manifest with `Retrying` status and a populated
/// `next_retry_at` field.
pub fn prepare_retry(manifest: &mut JobManifest, error: Option<&str>) {
    let policy = match &manifest.retry_policy {
        Some(p) => p.clone(),
        None => return,
    };

    let mut state = manifest.retry_state.take().unwrap_or_default();

    // Record the failed attempt.
    let attempt_record = JobAttempt {
        attempt: state.current_attempt,
        started_at: manifest.started_at.unwrap_or_else(Utc::now),
        completed_at: Some(Utc::now()),
        status: if manifest
            .error
            .as_ref()
            .map(|e| e.contains("timed out") || e.contains("timeout"))
            .unwrap_or(false)
        {
            AttemptStatus::TimedOut
        } else {
            AttemptStatus::Failed
        },
        error: error
            .map(|s| s.to_string())
            .or_else(|| manifest.error.clone()),
        output_path: Some(format!("output_attempt_{}.json", state.current_attempt)),
    };
    state.attempts.push(attempt_record);

    // Increment attempt counter.
    state.current_attempt += 1;

    // Calculate delay and schedule next retry.
    let retry_attempt = state.current_attempt - 1; // Retry number (1-indexed).
    let delay_secs = policy.delay_for_attempt(retry_attempt);
    let next_retry = Utc::now() + Duration::seconds(delay_secs as i64);
    state.next_retry_at = Some(next_retry);

    info!(
        job_id = %manifest.job_id,
        attempt = state.current_attempt,
        max_attempts = policy.max_attempts,
        delay_secs = delay_secs,
        backoff = %policy.backoff,
        "Scheduling retry"
    );

    // Update manifest.
    manifest.status = JobStatus::Retrying;
    manifest.started_at = None;
    manifest.completed_at = None;
    manifest.error = None;
    manifest.exit_code = None;
    manifest.retry_state = Some(state);
}

/// Record a successful attempt in the retry state.
pub fn record_success(manifest: &mut JobManifest) {
    if let Some(ref mut state) = manifest.retry_state {
        let attempt_record = JobAttempt {
            attempt: state.current_attempt,
            started_at: manifest.started_at.unwrap_or_else(Utc::now),
            completed_at: Some(Utc::now()),
            status: AttemptStatus::Succeeded,
            error: None,
            output_path: Some(format!("output_attempt_{}.json", state.current_attempt)),
        };
        state.attempts.push(attempt_record);
        state.next_retry_at = None;
    }
}

/// Check if a retrying job is ready to execute (backoff delay has elapsed).
pub fn is_ready_for_retry(manifest: &JobManifest) -> bool {
    if manifest.status != JobStatus::Retrying {
        return false;
    }
    match &manifest.retry_state {
        Some(state) => match state.next_retry_at {
            Some(next) => Utc::now() >= next,
            None => true, // No scheduled time = ready now.
        },
        None => false,
    }
}

/// Calculate the remaining delay before a retry is ready (in seconds).
/// Returns 0 if the retry is already ready.
pub fn remaining_delay_secs(manifest: &JobManifest) -> u64 {
    match &manifest.retry_state {
        Some(state) => match state.next_retry_at {
            Some(next) => {
                let remaining = next - Utc::now();
                if remaining.num_seconds() > 0 {
                    remaining.num_seconds() as u64
                } else {
                    0
                }
            }
            None => 0,
        },
        None => 0,
    }
}

/// Format retry attempt history for display.
pub fn format_attempts(manifest: &JobManifest) -> Vec<serde_json::Value> {
    match &manifest.retry_state {
        Some(state) => state
            .attempts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "attempt": a.attempt,
                    "started_at": a.started_at,
                    "completed_at": a.completed_at,
                    "status": format!("{:?}", a.status),
                    "error": a.error,
                })
            })
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_runner::types::*;

    fn make_manifest(status: JobStatus, error: Option<&str>) -> JobManifest {
        let mut m = JobManifest::new(
            "test-agent".to_string(),
            JobType::Shell,
            JobConfig::Shell(ShellJobConfig {
                command: "echo hello".to_string(),
                workdir: None,
            }),
            Some("test job".to_string()),
            60,
        );
        m.status = status;
        m.error = error.map(|s| s.to_string());
        m.started_at = Some(Utc::now());
        m
    }

    #[test]
    fn test_should_retry_with_policy() {
        let mut m = make_manifest(JobStatus::Failed, Some("tool error"));
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Exponential,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        });
        m.retry_state = Some(RetryState::default());
        assert!(should_retry(&m));
    }

    #[test]
    fn test_should_not_retry_without_policy() {
        let m = make_manifest(JobStatus::Failed, Some("tool error"));
        assert!(!should_retry(&m));
    }

    #[test]
    fn test_should_not_retry_max_attempts_reached() {
        let mut m = make_manifest(JobStatus::Failed, Some("tool error"));
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        });
        m.retry_state = Some(RetryState {
            current_attempt: 3,
            next_retry_at: None,
            attempts: vec![],
        });
        assert!(!should_retry(&m));
    }

    #[test]
    fn test_should_not_retry_cancellation() {
        let mut m = make_manifest(JobStatus::Cancelled, None);
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        });
        assert!(!should_retry(&m));
    }

    #[test]
    fn test_should_not_retry_dependency_failure() {
        let mut m = make_manifest(JobStatus::DependencyFailed, None);
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        });
        assert!(!should_retry(&m));
    }

    #[test]
    fn test_should_not_retry_wrong_failure_type() {
        let mut m = make_manifest(JobStatus::Failed, Some("timed out after 60s"));
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 5,
            retry_on: vec![FailureType::NonZeroExit], // Only retry on non-zero exit.
            jitter: false,
        });
        m.retry_state = Some(RetryState::default());
        assert!(!should_retry(&m));
    }

    #[test]
    fn test_backoff_exponential() {
        let policy = RetryPolicy {
            max_attempts: 5,
            backoff: BackoffStrategy::Exponential,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        };
        assert_eq!(policy.delay_for_attempt(1), 5); // 5 * 2^0
        assert_eq!(policy.delay_for_attempt(2), 10); // 5 * 2^1
        assert_eq!(policy.delay_for_attempt(3), 20); // 5 * 2^2
    }

    #[test]
    fn test_backoff_linear() {
        let policy = RetryPolicy {
            max_attempts: 5,
            backoff: BackoffStrategy::Linear,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        };
        assert_eq!(policy.delay_for_attempt(1), 5);
        assert_eq!(policy.delay_for_attempt(2), 10);
        assert_eq!(policy.delay_for_attempt(3), 15);
    }

    #[test]
    fn test_backoff_fixed() {
        let policy = RetryPolicy {
            max_attempts: 5,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 10,
            retry_on: vec![],
            jitter: false,
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
        assert_eq!(policy.delay_for_attempt(2), 10);
        assert_eq!(policy.delay_for_attempt(3), 10);
    }

    #[test]
    fn test_prepare_retry() {
        let mut m = make_manifest(JobStatus::Failed, Some("something broke"));
        m.retry_policy = Some(RetryPolicy {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed,
            initial_delay_secs: 5,
            retry_on: vec![],
            jitter: false,
        });
        m.retry_state = Some(RetryState::default());

        prepare_retry(&mut m, Some("something broke"));

        assert_eq!(m.status, JobStatus::Retrying);
        let state = m.retry_state.as_ref().unwrap();
        assert_eq!(state.current_attempt, 2);
        assert!(state.next_retry_at.is_some());
        assert_eq!(state.attempts.len(), 1);
        assert_eq!(state.attempts[0].status, AttemptStatus::Failed);
    }
}

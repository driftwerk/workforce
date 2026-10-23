//! Tracks in-flight **detached** background work (research chains, spawned
//! sub-agents) by conversation, so a human's "stop" can cancel work that
//! outlives the agent turn that started it.
//!
//! Live agent *turns* are tracked separately in
//! [`crate::serve::AgentService::active_runners`] (each handle already carries
//! its `channel`/`thread`); this registry is only for work that runs on the job
//! runner and posts back to a channel on its own — e.g. `investigate`'s chain or
//! a `subagent.spawn` job. The Slack dispatch loop reads both when a human says
//! "stop" (see `is_stop_intent` + the stop fast-path in `slack_manager`).
//!
//! Entries deregister themselves: `investigate`'s `ProgressReporter` removes its
//! chain when it finishes, and single jobs (which have no such owner) get a
//! lightweight [`spawn_dereg_watcher`]. The dispatch loop therefore never does a
//! liveness sweep — cancelling already-finished work is a harmless no-op.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::hosts::JobHost;
use crate::hub_core::jobs::ChainStatus;

/// How often a dereg watcher polls its target for terminality.
const DEREG_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What to cancel, and how, for one piece of detached background work.
#[derive(Debug, Clone)]
pub enum CancelTarget {
    /// A job chain (e.g. `investigate`): cancel via [`JobRunner::cancel_chain`].
    Chain(String),
    /// A single job (e.g. `subagent.spawn`): cancel via [`JobRunner::cancel`].
    Job(String),
    /// Detached, non-job background work owning a [`CancellationToken`] (e.g.
    /// `coding.fanout`'s in-process executor). Cancel by triggering the token;
    /// the executor selects on it and owns its own deregistration.
    Token(CancellationToken),
}

impl CancelTarget {
    /// Cancel the target. Returns whether something was actually cancelled
    /// (`false` if it had already finished — a harmless no-op).
    pub async fn cancel(&self, runner: &dyn JobHost) -> bool {
        match self {
            CancelTarget::Chain(id) => runner.cancel_chain(id).await.unwrap_or(false),
            CancelTarget::Job(id) => runner.cancel(id).await.unwrap_or(false),
            CancelTarget::Token(token) => {
                let newly = !token.is_cancelled();
                token.cancel();
                newly
            }
        }
    }

    /// Whether the target has reached a terminal state. A missing/unknown target
    /// is treated as terminal so a dereg watcher exits rather than spinning.
    pub async fn is_terminal(&self, runner: &dyn JobHost) -> bool {
        match self {
            CancelTarget::Chain(id) => runner
                .chain_summary(id)
                .await
                .map(|s| s.status.is_terminal())
                .unwrap_or(true),
            CancelTarget::Job(id) => runner
                .get_result(id, None)
                .await
                .ok()
                .and_then(|r| {
                    r.get("status")
                        .and_then(|v| v.as_str())
                        .map(is_terminal_job_status)
                })
                .unwrap_or(true),
            CancelTarget::Token(token) => token.is_cancelled(),
        }
    }
}

/// Whether a serialized `JobStatus` string is terminal (mirrors
/// `JobStatus::is_terminal`, kept as a string match because `get_result` returns
/// the status as JSON).
fn is_terminal_job_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "dependency_failed"
    )
}

/// One piece of detached background work, addressable by conversation.
#[derive(Debug, Clone)]
pub struct BackgroundWork {
    pub target: CancelTarget,
    /// Raw platform channel id the work posts to. Matches
    /// `ActiveRunnerHandle.channel` and the inbound message's `channel`.
    pub channel: String,
    /// Thread within the channel, if any.
    pub thread: Option<String>,
    /// Platform user id that started the work (for attribution/logging).
    pub requester: Option<String>,
    /// Short human-readable label for the stop acknowledgement.
    pub label: String,
}

/// In-flight detached background work, keyed by a unique id (chain or job id).
pub type BackgroundWorkRegistry = Arc<RwLock<HashMap<String, BackgroundWork>>>;

/// Does a piece of work (or a live turn) at `item_channel`/`item_thread` fall in
/// the scope of a "stop" posted in `channel`/`scope_thread`?
///
/// A top-level stop (`scope_thread == None`) is **channel-wide** — it halts
/// everything the agent is loudly doing in that channel, which is what a human
/// means by "stop" in a busy public channel, and is the case that crosses the
/// session-isolation boundary (B's top-level stop reaches A's thread-scoped
/// research). A stop posted *inside a thread* is scoped to that thread, so it
/// doesn't disturb unrelated work elsewhere in the channel.
pub fn matches_scope(
    item_channel: &str,
    item_thread: Option<&str>,
    channel: &str,
    scope_thread: Option<&str>,
) -> bool {
    item_channel == channel && (scope_thread.is_none() || item_thread == scope_thread)
}

/// Spawn a detached task that removes `work_id` from `registry` once its target
/// reaches a terminal state. Gives single jobs — which, unlike `investigate`'s
/// `ProgressReporter`, have no lifecycle owner — a dereg owner, so the registry
/// self-cleans off the dispatch hot path (no liveness sweep there). Bounded by
/// the job's own timeout; a missing target is terminal, so this never spins.
pub fn spawn_dereg_watcher(
    registry: BackgroundWorkRegistry,
    runner: Arc<dyn JobHost>,
    work_id: String,
    target: CancelTarget,
) {
    tokio::spawn(async move {
        loop {
            if target.is_terminal(runner.as_ref()).await {
                break;
            }
            tokio::time::sleep(DEREG_POLL_INTERVAL).await;
        }
        registry.write().await.remove(&work_id);
    });
}

/// What a finished investigation should deliver to its channel.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The chain was cancelled (a human stopped it) — show a quiet "stopped"
    /// card instead of a report or a spurious failure.
    Stopped,
    /// The chain finished — deliver the synthesised report.
    Report(String),
    /// The chain failed — show the failure reason.
    Failed(String),
}

/// Decide what to deliver from the chain's final status and the report fetch.
/// `Cancelled` wins even if the report fetch erred (a late cancel can race the
/// poll loop), so a stopped investigation never posts a spurious "failed".
pub fn delivery_for(
    final_status: Option<&ChainStatus>,
    report: Result<String, String>,
) -> Delivery {
    if matches!(final_status, Some(ChainStatus::Cancelled)) {
        return Delivery::Stopped;
    }
    match report {
        Ok(r) => Delivery::Report(r),
        Err(e) => Delivery::Failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_stop_is_channel_wide_across_threads() {
        // Two pieces of work in the same channel but different threads (i.e.
        // different sessions under thread_isolation). A top-level "stop"
        // (scope_thread = None) must select BOTH — the cross-session guarantee.
        assert!(matches_scope("C1", Some("t1"), "C1", None));
        assert!(matches_scope("C1", Some("t2"), "C1", None));
        assert!(matches_scope("C1", None, "C1", None));
        // ...but not work in a different channel.
        assert!(!matches_scope("C2", Some("t1"), "C1", None));
    }

    #[test]
    fn threaded_stop_is_scoped_to_its_thread() {
        assert!(matches_scope("C1", Some("t1"), "C1", Some("t1")));
        assert!(!matches_scope("C1", Some("t2"), "C1", Some("t1")));
        // A channel-root chain (thread=None) is not caught by a stop inside a thread.
        assert!(!matches_scope("C1", None, "C1", Some("t1")));
    }

    #[test]
    fn delivery_cancelled_beats_failed_report() {
        assert_eq!(
            delivery_for(Some(&ChainStatus::Cancelled), Err("synth cancelled".into())),
            Delivery::Stopped
        );
        // Even an Ok report is suppressed when the chain was cancelled.
        assert_eq!(
            delivery_for(Some(&ChainStatus::Cancelled), Ok("report".into())),
            Delivery::Stopped
        );
    }

    #[test]
    fn delivery_completed_returns_report() {
        assert_eq!(
            delivery_for(Some(&ChainStatus::Completed), Ok("the report".into())),
            Delivery::Report("the report".into())
        );
        assert_eq!(
            delivery_for(Some(&ChainStatus::Failed), Err("boom".into())),
            Delivery::Failed("boom".into())
        );
        // No summary + a failed fetch ⇒ failed (not stopped).
        assert_eq!(
            delivery_for(None, Err("timed out".into())),
            Delivery::Failed("timed out".into())
        );
    }

    #[test]
    fn terminal_job_status_strings() {
        assert!(is_terminal_job_status("completed"));
        assert!(is_terminal_job_status("cancelled"));
        assert!(is_terminal_job_status("dependency_failed"));
        assert!(!is_terminal_job_status("running"));
        assert!(!is_terminal_job_status("submitted"));
    }
}

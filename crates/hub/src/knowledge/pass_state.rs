//! State exposed by `MaintenanceRunner` to operator surfaces: the
//! in-flight pass snapshot, per-target queue, and the
//! `PassRunOutcome` variants that `knowledge.refresh_index` and
//! `projects.index` dispatch on.

use chrono::{DateTime, Utc};
use serde::Serialize;
use workforce_codebase_knowledge::lease::IndexLease;
use workforce_projects::RepoSource;

use super::global_index::PassReport;

/// Scope of a maintenance pass. `Hub` covers every target the
/// `MaintenanceRunner` enumerates; `Project` is scoped to one
/// `(agent_id, name)` tuple.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PassScope {
    Hub,
    Project { agent_id: String, name: String },
}

/// Per-target state inside an in-flight pass. Pulls happen in parallel
/// up front, so individual targets sit in `Queued` until `index_one`
/// pulls them into `Indexing`. Pull failures land directly in
/// `Failed`; successful indexes in `Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    Queued,
    Indexing,
    Done,
    Failed,
}

/// Snapshot of the live lease row for the target being indexed. Refreshed
/// lazily on each `snapshot_state()` read; absent when the target isn't
/// in `Indexing` or when the lease lookup itself errored.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseSnapshot {
    pub phase: String,
    pub acquired_at_ms: i64,
    pub last_heartbeat_at_ms: i64,
    pub entities_total: Option<u64>,
    pub entities_processed: Option<u64>,
    pub summaries_total: Option<u64>,
    pub summaries_processed: Option<u64>,
}

impl From<&IndexLease> for LeaseSnapshot {
    fn from(l: &IndexLease) -> Self {
        Self {
            phase: l.phase.as_str().to_string(),
            acquired_at_ms: l.acquired_at_ms,
            last_heartbeat_at_ms: l.last_heartbeat_at_ms,
            entities_total: l.entities_total,
            entities_processed: l.entities_processed,
            summaries_total: l.summaries_total,
            summaries_processed: l.summaries_processed,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetQueueEntry {
    pub id: String,
    pub source: RepoSource,
    pub state: TargetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_lease: Option<LeaseSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PassStateSnapshot {
    pub scope: PassScope,
    pub mode: &'static str,
    pub started_at: DateTime<Utc>,
    pub current_target_index: usize,
    pub targets: Vec<TargetQueueEntry>,
}

/// Outcome of a `run_pass*` call. Always a successful `ToolResult` at
/// the tool layer — `AlreadyInFlight` is a meaningful answer ("no need
/// to start a new pass — here's the existing one"), not an error.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PassRunOutcome {
    Completed(PassReport),
    /// The pass was accepted and is now running in the background. The
    /// caller did NOT wait for it — `snapshot` is the initial queue, and
    /// completion/failures must be confirmed via `knowledge.status`.
    Started {
        message: &'static str,
        snapshot: PassStateSnapshot,
    },
    AlreadyInFlight {
        message: &'static str,
        in_flight: PassStateSnapshot,
    },
}

pub const ALREADY_IN_FLIGHT_MESSAGE: &str =
    "A maintenance pass is already running. This call did not start a new one. \
     Re-call when the pass completes, or call knowledge.status for live progress.";

pub const STARTED_MESSAGE: &str =
    "Indexing started in the background. This is NOT complete — call \
     knowledge.status to track progress and confirm completion/failures. Do not \
     report success until status shows the pass has finished.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_state_serialises_as_snake_case() {
        assert_eq!(
            serde_json::to_value(TargetState::Queued).unwrap(),
            serde_json::json!("queued")
        );
        assert_eq!(
            serde_json::to_value(TargetState::Indexing).unwrap(),
            serde_json::json!("indexing")
        );
        assert_eq!(
            serde_json::to_value(TargetState::Done).unwrap(),
            serde_json::json!("done")
        );
        assert_eq!(
            serde_json::to_value(TargetState::Failed).unwrap(),
            serde_json::json!("failed")
        );
    }

    #[test]
    fn pass_scope_serialises_with_kind_tag() {
        let v = serde_json::to_value(PassScope::Hub).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "hub" }));
        let v = serde_json::to_value(PassScope::Project {
            agent_id: "allan".into(),
            name: "contoso".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "kind": "project",
                "agent_id": "allan",
                "name": "contoso"
            })
        );
    }

    #[test]
    fn pass_run_outcome_already_in_flight_shape() {
        let snap = PassStateSnapshot {
            scope: PassScope::Hub,
            mode: "incremental",
            started_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            current_target_index: 1,
            targets: vec![],
        };
        let v = serde_json::to_value(PassRunOutcome::AlreadyInFlight {
            message: ALREADY_IN_FLIGHT_MESSAGE,
            in_flight: snap,
        })
        .unwrap();
        assert_eq!(v["outcome"], "already_in_flight");
        assert!(v["message"].as_str().unwrap().contains("already running"));
    }

    #[test]
    fn pass_run_outcome_started_shape() {
        let snap = PassStateSnapshot {
            scope: PassScope::Hub,
            mode: "incremental",
            started_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            current_target_index: 0,
            targets: vec![],
        };
        let v = serde_json::to_value(PassRunOutcome::Started {
            message: STARTED_MESSAGE,
            snapshot: snap,
        })
        .unwrap();
        assert_eq!(v["outcome"], "started");
        assert!(v["message"].as_str().unwrap().contains("NOT complete"));
    }
}

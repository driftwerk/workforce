//! Expiry sweeper + boot reconciliation for routed approvals.
//!
//! Every `interval`, claims each lapsed-and-still-pending row as
//! `expired` (first-wins, so it never races a live button click),
//! rewrites its DMs + notice, and launches the expire resume directive.
//! On boot it additionally rewrites any already-resolved rows whose DMs
//! may still show buttons (restart reconciliation). Live pending rows
//! need no action — the button handler is fully DB-backed.

use std::sync::Arc;
use std::time::Duration;

use super::resume::{launch_resume, resume_service};
use super::slack::rewrite_all_targets;
use super::store::{ApprovalOutcome, PendingApproval, PendingApprovalStore};
use crate::agent_registry::AgentRegistry;

const RECONCILE_LIMIT: usize = 256;

/// Spawn the sweeper loop. Runs the boot reconciliation once, then ticks
/// every `interval`. Resolves the requesting agent's service per row.
pub fn spawn(
    registry: Arc<AgentRegistry>,
    store: Arc<dyn PendingApprovalStore>,
    interval: Duration,
) {
    tokio::spawn(async move {
        reconcile_on_boot(&registry, &store).await;
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            sweep_once(&registry, &store).await;
        }
    });
}

/// Claim and finalise every lapsed pending row.
async fn sweep_once(registry: &Arc<AgentRegistry>, store: &Arc<dyn PendingApprovalStore>) {
    let now = chrono::Utc::now();
    let due = match store.due_for_expiry(now) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "approval sweeper: due_for_expiry failed");
            return;
        }
    };
    for row in due {
        // First-wins: only the caller that flips pending→expired finalises.
        match store.claim(&row.request_id, ApprovalOutcome::Expired, "") {
            Ok(true) => {
                super::resolved(store.as_ref(), &row.request_id);
                finalise_expired(registry, store, &row).await
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, request = %row.request_id, "approval sweeper: claim failed")
            }
        }
    }
}

async fn finalise_expired(
    registry: &Arc<AgentRegistry>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
) {
    // Any service shares the surface/identity registries used by the
    // rewrite; resolve the requesting agent so resume runs on the right one.
    if let Some(svc) = resume_service(registry, row) {
        rewrite_all_targets(&svc, store, row, ApprovalOutcome::Expired, "").await;
    }
    launch_resume(registry, row, ApprovalOutcome::Expired, "the request").await;
}

/// On boot, rewrite DMs for rows resolved before the restart whose
/// messages may still carry live buttons. Idempotent — re-running a
/// rewrite over already-clean DMs is harmless.
async fn reconcile_on_boot(registry: &Arc<AgentRegistry>, store: &Arc<dyn PendingApprovalStore>) {
    let resolved = match store.recently_resolved(RECONCILE_LIMIT) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "approval sweeper: boot reconciliation read failed");
            return;
        }
    };
    for row in resolved {
        let Some(svc) = resume_service(registry, &row) else {
            continue;
        };
        let outcome =
            ApprovalOutcome::from_str_lossy(&row.status).unwrap_or(ApprovalOutcome::Denied);
        let resolver = row.resolved_by.clone().unwrap_or_default();
        rewrite_all_targets(&svc, store, &row, outcome, &resolver).await;
    }
}

//! A person's role changes, as the lifecycle records them and the gates read them.

use chrono::Utc;
use uuid::Uuid;
use workforce_identity::lifecycle::LifecycleOp;
use workforce_identity::lifecycle::LifecycleOpStore;
use workforce_identity::lifecycle::OpStatus;

/// The kind of operation a role change records.
pub const ROLE_CHANGE: &str = "role";

/// Record that `subject`'s role changed from `from` to `to`, so a run that
/// later needs what `from` granted is told the person lost it
/// ([`crate::cap_gate::Stop::Lost`]) rather than left to answer as if they
/// never held it. Best effort: a change it cannot record is logged, and a run
/// it would have stopped answers as before.
pub fn record_role_change(
    ops: &dyn LifecycleOpStore,
    subject: Uuid,
    from: &str,
    to: &str,
    actor: &Actor,
) {
    if from == to {
        return;
    }
    let now = Utc::now();
    let op = LifecycleOp {
        id: Uuid::new_v4(),
        subject,
        kind: ROLE_CHANGE.to_string(),
        target: None,
        actor: actor.name(),
        names: vec![subject.to_string()],
        status: OpStatus::Complete,
        report: serde_json::json!({ "from": from, "to": to }),
        started_at: now,
        lease_until: now,
        finished_at: Some(now),
    };
    match ops.claim_op(&op, now) {
        Ok(true) => {
            tracing::info!(target: "audit.identity_lifecycle", %subject, from, to, "role changed")
        }
        Ok(false) => {
            tracing::warn!(target: "audit.identity_lifecycle", %subject, from, to, "Role change not recorded: another operation holds the person")
        }
        Err(e) => {
            tracing::warn!(target: "audit.identity_lifecycle", %subject, error = %format!("{e:#}"), "Role change not recorded")
        }
    }
}

/// The roles `subject` held before each recorded change of role.
pub fn earlier_roles(ops: &dyn LifecycleOpStore, subject: Uuid) -> Vec<String> {
    match ops.list_ops(Some(subject), 200) {
        Ok(recorded) => recorded
            .into_iter()
            .filter(|op| op.kind == ROLE_CHANGE)
            .filter_map(|op| op.report.get("from")?.as_str().map(str::to_string))
            .collect(),
        Err(e) => {
            tracing::warn!(%subject, error = %format!("{e:#}"), "Failed to read a person's earlier roles");
            Vec::new()
        }
    }
}

/// Who runs an operation: an administrator's identity, or the hub's owner
/// token, which names nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor(pub Option<Uuid>);

impl Actor {
    /// How records name them (a job's `edited_by`, a grant's `granted_by`).
    pub fn name(&self) -> String {
        self.0.map_or_else(
            || crate::hub_core::schedules::OWNER_TOKEN_EDITOR.to_string(),
            |id| id.to_string(),
        )
    }
}

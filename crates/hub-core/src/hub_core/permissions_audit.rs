//! Shared audit emission for cascade + capability decisions.
//!
//! Both the `/api/permissions/cascade` handler and the in-tool
//! [`crate::knowledge::access_guard::KnowledgeAccessGuard`] need to
//! emit the same `AccessAuditEvent::PermissionDecision` shape. Putting
//! the encoding in one place keeps the audit log consistent so
//! dashboards built on the structured tracing target stay valid no
//! matter which surface produced the decision.
//!
//! The capability engine (the pre-dispatch [`crate::cap_gate::HubCapabilityGate`]
//! and the gateway `require_capability_*` helpers) emits its hard denies through
//! the same sink via [`capability_deny_event`] / [`emit_capability_deny`], so a
//! member-tries-admin attempt surfaces in `GET /api/permissions/audit` — not only
//! in the `sentinel.audit` tracing target — under the same
//! [`AuditOperation::PermissionDecision`] the cascade uses.

use workforce_memory::access_audit::{self, AccessAuditEvent, AuditDecision, AuditOperation};
use workforce_permissions::types::PermissionState;

/// Map a cascade [`PermissionState`] onto an [`AccessAuditEvent`] and
/// emit it. `Deny` becomes a denial; everything else becomes `Allow`
/// at this layer (the runtime prompt is what gates Ask / AlwaysAsk —
/// the audit signal is "this filter let the call through").
///
/// The cascade state itself rides on `owner_identity_id` so the
/// structured log line carries it without a schema change to
/// `AccessAuditEvent`.
pub fn emit_decision(user_id: &str, tool_name: &str, state: PermissionState) {
    let decision = match state {
        PermissionState::Deny => AuditDecision::DeniedOwnerMismatch,
        _ => AuditDecision::Allow,
    };
    let mut event = AccessAuditEvent::new(
        AuditOperation::PermissionDecision,
        decision,
        Some(user_id),
        None,
        Some(tool_name),
    );
    event.owner_identity_id = Some(state.as_str().to_string());
    access_audit::emit(event);
}

/// Build (without emitting) the [`AccessAuditEvent`] for a capability-engine
/// hard `Deny`. Shared by the pre-dispatch gate and the gateway capability
/// helpers so the event construction lives in exactly one place. The pure
/// builder lets tests assert the encoding without racing the process-wide
/// `OnceLock` sink.
///
/// * `actor` — the caller that was denied (tool-turn `user_id` or the
///   gateway caller's `identity_id`); `None` for an unresolved caller.
/// * `action` — the tool name or the route's capability key.
/// * `capability` — the capability key the action required.
///
/// `Deny` maps to [`AuditDecision::DeniedOwnerMismatch`] — the same denial
/// variant [`emit_decision`] already uses (the audit enum has no dedicated
/// `Deny`) — and carries the required capability on `owner_identity_id`,
/// mirroring how [`emit_decision`] rides the cascade state there.
pub fn capability_deny_event(
    actor: Option<&str>,
    action: &str,
    capability: &str,
) -> AccessAuditEvent {
    let mut event = AccessAuditEvent::new(
        AuditOperation::PermissionDecision,
        AuditDecision::DeniedOwnerMismatch,
        actor,
        None,
        Some(action),
    );
    event.owner_identity_id = Some(capability.to_string());
    event
}

/// Emit a capability-engine hard `Deny` to the shared access-audit sink so it
/// shows in `GET /api/permissions/audit`. Thin wrapper over
/// [`capability_deny_event`].
pub fn emit_capability_deny(actor: Option<&str>, action: &str, capability: &str) {
    access_audit::emit(capability_deny_event(actor, action, capability));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_deny_event_encodes_actor_action_and_capability() {
        let event = capability_deny_event(Some("user-42"), "agent.delete", "agents_manage");
        assert_eq!(event.operation, AuditOperation::PermissionDecision);
        assert_eq!(event.decision, AuditDecision::DeniedOwnerMismatch);
        assert_eq!(event.requester_identity_id.as_deref(), Some("user-42"));
        assert_eq!(event.resource_id.as_deref(), Some("agent.delete"));
        // The required capability rides on owner_identity_id, mirroring how
        // `emit_decision` carries the cascade state there.
        assert_eq!(event.owner_identity_id.as_deref(), Some("agents_manage"));
    }

    #[test]
    fn capability_deny_event_allows_an_unresolved_actor() {
        let event = capability_deny_event(None, "role.delete", "personas_manage");
        assert_eq!(event.requester_identity_id, None);
        assert_eq!(event.resource_id.as_deref(), Some("role.delete"));
        assert_eq!(event.owner_identity_id.as_deref(), Some("personas_manage"));
    }
}

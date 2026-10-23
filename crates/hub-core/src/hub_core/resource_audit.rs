//! The one place a system administrator's reach past an owner, and a change
//! of who owns a resource, are recorded.
//!
//! The two are separate events and are never collapsed into one, because they
//! answer different questions. [`override_granted`] answers *did anybody reach
//! past an owner?* and is absent from the common case of an owner giving their
//! own resource away. [`owner_changed`] answers *who owns this now?* and fires
//! on every transfer, or the ownership history has holes exactly where nothing
//! went wrong. An administrator transferring somebody else's resource emits
//! both.
//!
//! An override is a write somebody has to be able to find afterwards: an
//! administrator — or, over an external skill, whoever else holds that kind's
//! bypass — edited, shared, re-scoped or deleted a resource that is not theirs.
//! The event is emitted where the authority is decided — inside
//! [`crate::def_write_auth::authorize_def_write`] for a definition, inside
//! [`crate::skill_access::SkillCaller`]'s two gates for a skill — so no caller
//! can hold an override without one. An owner's own write, and a `manage`
//! grantee's, record nothing, which is what lets the log separate an override
//! from ordinary work.
//!
//! Reads are deliberately not recorded: an administrator's listing already
//! sees every row, so a per-row read event would drown the log; cross-user
//! *data* reads are covered by the memory access audit this writes to.

use workforce_memory::access_audit::{emit, AccessAuditEvent, AuditDecision, AuditOperation};

/// Record one administrator acting on `(kind, id)` past its owner. `action` is
/// what they did: a [`crate::def_write_auth::DefOp`] for a definition, the
/// standing they took for a skill.
pub fn override_granted(
    kind: &str,
    id: &str,
    owner: Option<&str>,
    actor: Option<&str>,
    action: &str,
) {
    emit(AccessAuditEvent::new(
        AuditOperation::ResourceAccessOverride,
        AuditDecision::Allow,
        actor,
        owner,
        Some(&format!("{kind}:{id}")),
    ));
    tracing::info!(
        target: "audit.resource_access_override",
        kind = %kind,
        resource = %id,
        owner = ?owner,
        actor = ?actor,
        action = %action,
        "Administrator overrode a resource's owner"
    );
}

/// Record `(kind, id)` changing hands. `to` is the identity that holds it now
/// and goes in the event's owner field, following the `AgentOwnerChange`
/// precedent: the shared [`AccessAuditEvent`] has no previous-value field and
/// must not grow one, so `from` rides the tracing line instead.
pub fn owner_changed(kind: &str, id: &str, from: Option<&str>, to: &str, actor: Option<&str>) {
    emit(AccessAuditEvent::new(
        AuditOperation::ResourceOwnerChange,
        AuditDecision::Allow,
        actor,
        Some(to),
        Some(&format!("{kind}:{id}")),
    ));
    tracing::info!(
        target: "audit.resource_owner_change",
        kind = %kind,
        resource = %id,
        from_owner = ?from,
        owner = %to,
        actor = ?actor,
        "Resource ownership transferred"
    );
}

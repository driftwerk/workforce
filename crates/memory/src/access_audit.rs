//! Per-request audit log for memory + session reads (DW-527).
//!
//! Every memory `recall` / `list` / `count` emits an [`AccessAuditEvent`]
//! describing who asked, who owns each result, and what decision the
//! per-user isolation gate reached. Operators register a sink via
//! [`set_global_sink`] (typically once at hub startup) — the default sink
//! emits structured `tracing` records under target `memory.access_audit`,
//! so log pipelines that already ingest tracing can build a "cross-identity
//! reads in last 24h" dashboard with no schema work.
//!
//! The hot path is intentionally cheap: events are dropped on the floor
//! when no sink is registered, and the tracing sink reuses the existing
//! span machinery rather than allocating a new transport.
//!
//! Tests should call sinks directly via `AccessAuditSink::record` rather
//! than through [`emit`] — the global sink is a `OnceLock` and only the
//! first registration sticks, so a test that swaps in a capturing sink
//! would race with whatever the binary registered at startup.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::MemoryFilter;

/// Decision an access check reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditDecision {
    /// Per-user gate accepted the read; the row was returned.
    #[serde(rename = "allow")]
    Allow,
    /// Filter named no identity and didn't opt into `system_unscoped`;
    /// the call was refused at the gate (DW-518).
    #[serde(rename = "deny_unscoped")]
    DeniedUnscoped,
    /// Defence-in-depth post-filter dropped a row whose `identity_id`
    /// didn't match the requester (DW-525).
    #[serde(rename = "deny_owner_mismatch")]
    DeniedOwnerMismatch,
}

impl AuditDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::DeniedUnscoped => "deny_unscoped",
            Self::DeniedOwnerMismatch => "deny_owner_mismatch",
        }
    }
}

/// Operation that produced the audit event. Useful for filtering the log
/// by code path without parsing free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOperation {
    Recall,
    List,
    Count,
    SessionRead,
    SessionWrite,
    /// Admin reassigned an agent's owner_id / owner_type. Resource = agent_id.
    /// (Audit F10, 2026-05-15.)
    AgentOwnerChange,
    /// Admin promoted/changed an identity's role, status, or password.
    /// Resource = target identity_id. (Audit F11, 2026-05-15.)
    IdentityElevate,
    /// Admin merged, transferred from or deleted an identity. Resource = the
    /// subject identity; target = the identity merged into or the recipient.
    IdentityLifecycle,
    /// Cascade evaluator reached a decision for a `(user, agent, tool)`
    /// triple. Resource = tool_name. Decision encodes Allow / Deny;
    /// Ask / AlwaysAsk surface as `Allow` here (the prompt is the gate,
    /// not the audit event). Phase 8 of the User RBAC + Permission
    /// Cascade plan.
    PermissionDecision,
    /// Admin created / updated / deleted a permission rule. Resource =
    /// rule UUID. Pairs with the per-row tracing fields the writer
    /// emits (`scope`, `scope_id`, `tool_pattern`, `state`).
    PermissionRuleWrite,
    /// Admin created / updated / deleted a custom role. Resource = the role key.
    RoleManage,
    /// A system administrator reached past a resource's owner — edited,
    /// shared, re-scoped or deleted somebody else's workflow, playbook,
    /// monitor or skill. Resource = `"{kind}:{id}"`.
    ResourceAccessOverride,
    /// An owned resource changed hands. Resource = `"{kind}:{id}"`, owner =
    /// the identity that holds it now. Emitted on *every* transfer, unlike
    /// [`Self::ResourceAccessOverride`], which answers the different question
    /// of whether anybody reached past an owner.
    ResourceOwnerChange,
    /// Somebody steered a workflow run that was under way — resumed,
    /// approved or rejected it, moved it between steps, paused or stopped it —
    /// or was refused. Resource = `"workflow_run:{instance_id}"`, owner = the
    /// identity the run acts as.
    WorkflowRunSteer,
}

impl AuditOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recall => "recall",
            Self::List => "list",
            Self::Count => "count",
            Self::SessionRead => "session_read",
            Self::SessionWrite => "session_write",
            Self::AgentOwnerChange => "agent_owner_change",
            Self::IdentityElevate => "identity_elevate",
            Self::IdentityLifecycle => "identity_lifecycle",
            Self::PermissionDecision => "permission_decision",
            Self::PermissionRuleWrite => "permission_rule_write",
            Self::RoleManage => "role_manage",
            Self::ResourceAccessOverride => "resource_access_override",
            Self::ResourceOwnerChange => "resource_owner_change",
            Self::WorkflowRunSteer => "workflow_run_steer",
        }
    }
}

/// One audit record. Field order matches typical dashboard queries:
/// "who", "what", "decision", "context".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessAuditEvent {
    pub timestamp: DateTime<Utc>,
    /// Identity that initiated the read; `None` when the call opted into
    /// `system_unscoped` (consolidation / audit tooling).
    pub requester_identity_id: Option<String>,
    /// Identity that owns the row this event refers to; `None` for the
    /// gate-level deny (no specific row).
    pub owner_identity_id: Option<String>,
    pub operation: AuditOperation,
    pub decision: AuditDecision,
    /// Memory ID when the event refers to a single row; `None` for
    /// gate-level events that affect the whole call.
    pub resource_id: Option<String>,
    /// Made by shadow mode's read of the replica (DW-676), asking again what
    /// the user's read already asked: not a read of its own.
    #[serde(default)]
    pub shadow: bool,
}

impl AccessAuditEvent {
    pub fn new(
        operation: AuditOperation,
        decision: AuditDecision,
        requester: Option<&str>,
        owner: Option<&str>,
        resource_id: Option<&str>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            requester_identity_id: requester.map(str::to_owned),
            owner_identity_id: owner.map(str::to_owned),
            operation,
            decision,
            resource_id: resource_id.map(str::to_owned),
            shadow: in_shadow(),
        }
    }
}

/// Sink that receives audit events. Implementations must be cheap on the
/// hot path — recall calls emit one event per returned row.
pub trait AccessAuditSink: Send + Sync {
    fn record(&self, event: &AccessAuditEvent);
}

/// Default sink: emit a structured `tracing` event under target
/// `memory.access_audit`. Operators wire log pipelines to that target;
/// no separate transport needed.
pub struct TracingAuditSink;

impl AccessAuditSink for TracingAuditSink {
    fn record(&self, event: &AccessAuditEvent) {
        tracing::info!(
            target: "memory.access_audit",
            timestamp = %event.timestamp.to_rfc3339(),
            requester = event.requester_identity_id.as_deref().unwrap_or("system"),
            owner = event.owner_identity_id.as_deref().unwrap_or("-"),
            operation = event.operation.as_str(),
            decision = event.decision.as_str(),
            resource_id = event.resource_id.as_deref().unwrap_or("-"),
            shadow = event.shadow,
        );
    }
}

/// Fan-out sink that delegates each event to every wrapped sink. Used
/// at hub bootstrap to send each record to both the tracing pipeline
/// and the in-process [`RingBufferAuditSink`] that backs the admin UI.
pub struct FanOutAuditSink {
    sinks: Vec<Arc<dyn AccessAuditSink>>,
}

impl FanOutAuditSink {
    pub fn new(sinks: Vec<Arc<dyn AccessAuditSink>>) -> Self {
        Self { sinks }
    }
}

impl AccessAuditSink for FanOutAuditSink {
    fn record(&self, event: &AccessAuditEvent) {
        for sink in &self.sinks {
            sink.record(event);
        }
    }
}

/// Bounded in-memory ring buffer of recent audit events. Powers the
/// `/api/permissions/audit` endpoint without needing a separate
/// persistent store; operators who need long-retention dashboards keep
/// ingesting the structured tracing target instead.
pub struct RingBufferAuditSink {
    capacity: usize,
    inner: parking_lot::Mutex<std::collections::VecDeque<AccessAuditEvent>>,
}

impl RingBufferAuditSink {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: parking_lot::Mutex::new(std::collections::VecDeque::with_capacity(
                capacity.max(1),
            )),
        }
    }

    /// Snapshot of the buffer in newest-first order. Cheap on hot
    /// paths (one clone per event, bounded by `capacity`); callers can
    /// filter the result in Rust before returning to a client.
    pub fn snapshot(&self) -> Vec<AccessAuditEvent> {
        let guard = self.inner.lock();
        guard.iter().rev().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

impl AccessAuditSink for RingBufferAuditSink {
    fn record(&self, event: &AccessAuditEvent) {
        let mut guard = self.inner.lock();
        if guard.len() == self.capacity {
            guard.pop_front();
        }
        guard.push_back(event.clone());
    }
}

static GLOBAL_SINK: OnceLock<Arc<dyn AccessAuditSink>> = OnceLock::new();
static RING_BUFFER: OnceLock<Arc<RingBufferAuditSink>> = OnceLock::new();

/// Install a shared ring-buffer sink and return a handle to it. Call
/// this *before* [`set_global_sink`] when the operator wants the
/// admin UI's audit view to see events. Subsequent calls return the
/// previously-installed handle so multiple bootstrap paths converge
/// on one buffer.
pub fn install_ring_buffer(capacity: usize) -> Arc<RingBufferAuditSink> {
    // `get_or_init` and not check-then-set: two callers racing the check would
    // each build a buffer, and the one whose `set` lost would be handed its own
    // orphan — converging on two buffers rather than the one promised above.
    Arc::clone(RING_BUFFER.get_or_init(|| Arc::new(RingBufferAuditSink::new(capacity))))
}

/// Read-only handle on the process-wide ring buffer, if one was
/// installed. The hub API handler reads through this; tests can
/// install their own buffer to exercise the snapshot path.
pub fn ring_buffer() -> Option<Arc<RingBufferAuditSink>> {
    RING_BUFFER.get().cloned()
}

/// Register the process-wide audit sink. Called once at hub startup.
/// Returns `Err` if a sink was already registered — sinks are intended
/// to be set once and immutable for the process lifetime.
pub fn set_global_sink(sink: Arc<dyn AccessAuditSink>) -> Result<(), &'static str> {
    GLOBAL_SINK
        .set(sink)
        .map_err(|_| "audit sink already initialised")
}

/// Emit an event to the global sink, if one is registered. No-op
/// otherwise — calls in unit tests with no sink installed pay only an
/// `OnceLock::get()`.
pub fn emit(event: AccessAuditEvent) {
    if let Some(sink) = GLOBAL_SINK.get() {
        sink.record(&event);
    }
}

tokio::task_local! {
    static SHADOW_READ: ();
}

/// Run `read` as shadow mode's read: each audit event it makes is
/// labelled [`AccessAuditEvent::shadow`].
pub async fn as_shadow<F: std::future::Future>(read: F) -> F::Output {
    SHADOW_READ.scope((), read).await
}

/// Whether the current task is running a shadow read ([`as_shadow`]).
pub fn in_shadow() -> bool {
    SHADOW_READ.try_with(|_| ()).is_ok()
}

/// Convenience: emit a gate-level "denied unscoped" event. Used when a
/// recall/list/count call is rejected before any row is touched.
pub fn record_unscoped_denial(operation: AuditOperation, filter: &MemoryFilter) {
    emit(AccessAuditEvent::new(
        operation,
        AuditDecision::DeniedUnscoped,
        filter.identity_id.as_deref(),
        None,
        None,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// An event made within a shadow read is labelled so, and one made
    /// outside it, even on the same task after, is not (DW-676 A8: the
    /// shadow's read of the replica is not a read of its own).
    #[tokio::test]
    async fn events_of_a_shadow_read_are_labelled_and_no_others() {
        let event = || {
            AccessAuditEvent::new(
                AuditOperation::Recall,
                AuditDecision::Allow,
                Some("bob"),
                None,
                None,
            )
        };
        assert!(!event().shadow);
        assert!(as_shadow(async { event() }).await.shadow);
        assert!(!event().shadow);
    }

    #[test]
    fn ring_buffer_evicts_oldest_first() {
        let rb = RingBufferAuditSink::new(3);
        for i in 0..5 {
            rb.record(&AccessAuditEvent::new(
                AuditOperation::PermissionDecision,
                AuditDecision::Allow,
                Some("alice"),
                Some(&i.to_string()),
                Some("tool"),
            ));
        }
        let snap = rb.snapshot();
        assert_eq!(snap.len(), 3);
        // Newest first: events 4, 3, 2 should remain.
        let owners: Vec<&str> = snap
            .iter()
            .map(|e| e.owner_identity_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(owners, vec!["4", "3", "2"]);
    }

    #[test]
    fn fan_out_delegates_to_each_sink() {
        let captured_a = Arc::new(Mutex::new(0));
        let captured_b = Arc::new(Mutex::new(0));
        struct Counter(Arc<Mutex<u32>>);
        impl AccessAuditSink for Counter {
            fn record(&self, _: &AccessAuditEvent) {
                *self.0.lock() += 1;
            }
        }
        let a: Arc<dyn AccessAuditSink> = Arc::new(Counter(Arc::clone(&captured_a)));
        let b: Arc<dyn AccessAuditSink> = Arc::new(Counter(Arc::clone(&captured_b)));
        let fan = FanOutAuditSink::new(vec![a, b]);
        fan.record(&AccessAuditEvent::new(
            AuditOperation::PermissionDecision,
            AuditDecision::Allow,
            None,
            None,
            None,
        ));
        assert_eq!(*captured_a.lock(), 1);
        assert_eq!(*captured_b.lock(), 1);
    }

    struct CapturingSink {
        events: Mutex<Vec<AccessAuditEvent>>,
    }

    impl CapturingSink {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl AccessAuditSink for CapturingSink {
        fn record(&self, event: &AccessAuditEvent) {
            self.events.lock().push(event.clone());
        }
    }

    #[test]
    fn emit_is_no_op_when_no_sink_registered() {
        // Running before any global sink is set: must not panic.
        emit(AccessAuditEvent::new(
            AuditOperation::Recall,
            AuditDecision::Allow,
            Some("alice"),
            Some("alice"),
            Some("mem-1"),
        ));
    }

    #[test]
    fn audit_event_serialises_with_snake_case_decision() {
        let ev = AccessAuditEvent::new(
            AuditOperation::List,
            AuditDecision::DeniedOwnerMismatch,
            Some("alice"),
            Some("bob"),
            Some("mem-2"),
        );
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"decision\":\"deny_owner_mismatch\""));
        assert!(json.contains("\"operation\":\"list\""));
    }

    #[test]
    fn as_str_matches_serde_serialisation() {
        let decisions = [
            AuditDecision::Allow,
            AuditDecision::DeniedUnscoped,
            AuditDecision::DeniedOwnerMismatch,
        ];
        for d in decisions {
            let json = serde_json::to_value(d).unwrap();
            assert_eq!(json.as_str(), Some(d.as_str()));
        }

        let ops = [
            AuditOperation::Recall,
            AuditOperation::List,
            AuditOperation::Count,
            AuditOperation::SessionRead,
            AuditOperation::SessionWrite,
            AuditOperation::AgentOwnerChange,
            AuditOperation::IdentityElevate,
            AuditOperation::IdentityLifecycle,
            AuditOperation::PermissionDecision,
            AuditOperation::PermissionRuleWrite,
            AuditOperation::RoleManage,
            AuditOperation::ResourceAccessOverride,
            AuditOperation::ResourceOwnerChange,
            AuditOperation::WorkflowRunSteer,
        ];
        for op in ops {
            let json = serde_json::to_value(op).unwrap();
            assert_eq!(json.as_str(), Some(op.as_str()));
        }
    }

    #[test]
    fn capturing_sink_collects_recorded_events() {
        let sink = CapturingSink::new();
        let ev = AccessAuditEvent::new(
            AuditOperation::Recall,
            AuditDecision::Allow,
            Some("alice"),
            Some("alice"),
            Some("mem-3"),
        );
        sink.record(&ev);
        assert_eq!(sink.events.lock().len(), 1);
    }
}

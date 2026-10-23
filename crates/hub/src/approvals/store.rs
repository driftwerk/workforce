//! Durable pending-approval store.
//!
//! Persists routed (designated-approver) approvals so they survive hub
//! restarts and resolve hours/days later. Mirrors the
//! `workforce-permissions` store shape exactly: a synchronous trait
//! behind `Arc<dyn>`, a SQLite backend (always compiled, `init_schema`)
//! and a Postgres backend (gated by the hub `postgres` feature, a real
//! refinery migration).
//!
//! Two tables: `pending_approvals` (one row per request, also carries
//! the origin-channel "⏳ waiting" notice) and `pending_approval_targets`
//! (one row per DM we posted — also the authorisation ACL).

#[cfg(test)]
use chrono::{DateTime, Utc};
#[cfg(test)]
use workforce_agent::runner::PendingApprovalCore;

/// Picks the rows a prune batch deletes; both backends bind the cutoff and
/// the batch size to it.
const PRUNABLE: &str = "status IN ('approved', 'denied', 'expired') AND expires_at < ";

/// What every backend's prune must do alike.
#[cfg(test)]
pub(crate) fn prune_contract(store: &dyn PendingApprovalStore) {
    let now = Utc::now();
    let request = |requester: &str, expires_at: DateTime<Utc>| {
        let core = PendingApprovalCore {
            request_id: uuid::Uuid::new_v4().to_string(),
            agent_id: "agent".into(),
            session_key: "slack:ws:C1:T1".into(),
            channel_id: "C1".into(),
            requester_user_id: requester.into(),
            tool_name: "vault.delete".into(),
            tool_input: serde_json::json!({"path": "x"}),
            reason: "sensitive".into(),
            allow_self_approval: false,
            approver_user_ids: vec![],
            ttl: std::time::Duration::from_secs(3600),
        };
        store.insert_core(&core, None, None, expires_at).unwrap();
        store.add_target(&core.request_id, "dave", "U1").unwrap();
        core.request_id
    };
    let long_ago = now - chrono::Duration::days(40);
    let approved = request("alice", long_ago);
    let expired = request("alice", long_ago);
    let lapsed_but_pending = request("alice", long_ago);
    let recent = request("alice", now - chrono::Duration::days(1));
    store
        .claim(&approved, ApprovalOutcome::Approved, "dave")
        .unwrap();
    store.claim(&expired, ApprovalOutcome::Expired, "").unwrap();
    store
        .claim(&recent, ApprovalOutcome::Denied, "dave")
        .unwrap();

    let cutoff = now - chrono::Duration::days(30);
    assert_eq!(
        store
            .prune_resolved(long_ago - chrono::Duration::days(1))
            .unwrap(),
        0
    );
    assert_eq!(store.prune_resolved(cutoff).unwrap(), 2);
    for gone in [&approved, &expired] {
        assert!(store.get(gone).unwrap().is_none());
        assert!(
            store.targets(gone).unwrap().is_empty(),
            "its targets go with it"
        );
    }
    assert!(
        store
            .get(&lapsed_but_pending)
            .unwrap()
            .unwrap()
            .is_pending(),
        "a pending request is never pruned"
    );
    assert!(store.get(&recent).unwrap().is_some());
    assert_eq!(store.approver_targets("dave").unwrap(), 2);
    assert_eq!(store.prune_resolved(cutoff).unwrap(), 0);
}

/// Sum per-name counts read as `(name, count)` rows.
fn sum_holders(
    rows: impl IntoIterator<Item = (String, i64)>,
) -> std::collections::BTreeMap<String, usize> {
    rows.into_iter()
        .fold(std::collections::BTreeMap::new(), |mut held, (name, n)| {
            *held.entry(name).or_default() += usize::try_from(n).unwrap_or_default();
            held
        })
}

/// One row per request raised and per approver slot, by who it names.
const HOLDER_ROWS: &str =
    "SELECT requester_id, COUNT(*) FROM pending_approvals GROUP BY requester_id \
     UNION ALL SELECT approver_id, COUNT(*) FROM pending_approval_targets GROUP BY approver_id";

/// What every backend's identity operations must do alike.
#[cfg(test)]
pub(crate) fn identity_operations_contract(store: &dyn PendingApprovalStore) {
    let core = |requester: &str| PendingApprovalCore {
        request_id: uuid::Uuid::new_v4().to_string(),
        agent_id: "agent".into(),
        session_key: "slack:ws:C1:T1".into(),
        channel_id: "C1".into(),
        requester_user_id: requester.into(),
        tool_name: "vault.delete".into(),
        tool_input: serde_json::json!({"path": "x"}),
        reason: "sensitive".into(),
        allow_self_approval: false,
        approver_user_ids: vec![],
        ttl: std::time::Duration::from_secs(3600),
    };
    let expires = Utc::now() + chrono::Duration::hours(1);
    let pending = core("alice");
    let resolved = core("alice");
    let others = core("carol");
    for request in [&pending, &resolved, &others] {
        store.insert_core(request, None, None, expires).unwrap();
    }
    store
        .claim(&resolved.request_id, ApprovalOutcome::Approved, "dave")
        .unwrap();
    store.add_target(&others.request_id, "alice", "U1").unwrap();
    store.add_target(&others.request_id, "dave", "U2").unwrap();

    assert_eq!(store.requested_by("alice").unwrap().len(), 2);
    assert_eq!(store.approver_targets("alice").unwrap(), 1);
    assert_eq!(
        store.identity_holders().unwrap(),
        std::collections::BTreeMap::from([
            ("alice".to_string(), 3),
            ("carol".to_string(), 1),
            ("dave".to_string(), 1),
        ])
    );

    assert_eq!(store.reassign_requester("alice", "bob").unwrap(), 1);
    assert_eq!(
        store.requested_by("bob").unwrap()[0].request_id,
        pending.request_id
    );
    assert_eq!(
        store.requested_by("alice").unwrap()[0].request_id,
        resolved.request_id,
        "a resolved request stays a record of who asked"
    );

    assert_eq!(store.drop_approver("alice").unwrap(), 1);
    assert_eq!(store.approver_targets("alice").unwrap(), 0);
    assert_eq!(store.targets(&others.request_id).unwrap().len(), 1);

    assert_eq!(store.forget_requester("alice").unwrap(), 1);
    assert!(store.get(&resolved.request_id).unwrap().is_none());
    assert!(store.requested_by("alice").unwrap().is_empty());
    assert!(store.get(&others.request_id).unwrap().is_some());
}

pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub mod postgres_migrate;

pub use sqlite::SqlitePendingApprovalStore;

pub use crate::hub_core::approvals::{
    ApprovalOutcome, PendingApproval, PendingApprovalStore, Target,
};
#[cfg(feature = "postgres")]
pub use postgres::PostgresPendingApprovalStore;

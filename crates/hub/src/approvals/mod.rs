//! Approval routing & designated approvers (hub orchestration).
//!
//! Once the policy + cascade layers settle a tool call on
//! `RequireApproval`, [`router::HubApprovalRouter`] decides *who* is
//! asked and *how*. Routed approvals are persisted in
//! [`store::PendingApprovalStore`], delivered as Slack DMs ([`slack`]) or
//! in the Desk conversation a run was made in ([`desk`]), resolved
//! first-wins ([`decide`]), and the approved action is re-executed
//! deterministically by a fresh turn ([`resume`]). A background
//! [`sweeper`] expires lapsed requests.

pub mod decide;
pub(crate) mod desk;
pub mod resume;
pub mod router;
pub mod slack;
pub mod store;
pub mod sweeper;

pub use router::HubApprovalRouter;
pub use store::{
    ApprovalOutcome, PendingApproval, PendingApprovalStore, SqlitePendingApprovalStore, Target,
};

#[cfg(feature = "postgres")]
pub use store::PostgresPendingApprovalStore;

/// What needs each of `approvers` changed: a request now asks them, or one
/// that did is resolved. Their Desk floors are told.
pub(crate) fn approvers_changed<'a>(approvers: impl IntoIterator<Item = &'a str>) {
    #[cfg(feature = "postgres")]
    approvers.into_iter().for_each(|approver| {
        crate::floor::signal::changed(
            workforce_common::channels::desk::DeskSignal::Floor,
            Some(approver),
        )
    });
    #[cfg(not(feature = "postgres"))]
    let _ = approvers.into_iter();
}

/// `request_id` was resolved: its approvers are told.
pub(crate) fn resolved(store: &dyn PendingApprovalStore, request_id: &str) {
    match store.targets(request_id) {
        Ok(targets) => approvers_changed(targets.iter().map(|t| t.approver_id.as_str())),
        Err(e) => {
            tracing::warn!(error = %e, request = %request_id, "reading a resolved request's approvers")
        }
    }
}

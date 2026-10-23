//! Approval routing & designated approvers (hub orchestration).
//!
//! Once the policy + cascade layers settle a tool call on
//! `RequireApproval`, [`router::HubApprovalRouter`] decides *who* is
//! asked and *how*. Routed approvals are persisted in
//! [`store::PendingApprovalStore`], delivered as Slack DMs
//! ([`slack`]), resolved first-wins, and the approved action is
//! re-executed deterministically by a fresh turn ([`resume`]). A
//! background [`sweeper`] expires lapsed requests.

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

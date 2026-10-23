//! The pending approvals the service holds for the surfaces that decide them.

use chrono::DateTime;
use chrono::Utc;
use eyre::Result;
use workforce_agent::runner::PendingApprovalCore;

/// Terminal status of a routed approval. `pending` is the implicit
/// starting state and never an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    Expired,
}

impl ApprovalOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// One persisted pending-approval row. The authoritative copy of
/// everything the resume turn needs — independent of any live runner.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub agent_id: String,
    pub session_key: String,
    pub workspace_id: Option<String>,
    pub channel_id: Option<String>,
    pub thread_ts: Option<String>,
    pub requester_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub reason: String,
    pub allow_self_approval: bool,
    pub status: String,
    pub resolved_by: Option<String>,
    pub channel_notice: Option<String>,
    pub channel_notice_ts: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl PendingApproval {
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

/// One DM we posted (or will post) to an eligible approver. The
/// `approver_id` / `slack_user_id` pair is also the authorisation ACL:
/// only a clicker whose Slack id is here may action the request.
#[derive(Debug, Clone)]
pub struct Target {
    pub request_id: String,
    pub approver_id: String,
    pub slack_user_id: String,
    pub dm_channel: Option<String>,
    pub message_ts: Option<String>,
}

/// Persistence layer for routed approvals. Synchronous (matches the
/// runner's `defer` hot path and the permissions store) with SQLite +
/// Postgres impls behind one interface.
pub trait PendingApprovalStore: Send + Sync {
    /// Persist the core row. Idempotent on `request_id` — re-running the
    /// deferral (e.g. a runner retry) is a no-op. Called by the runner's
    /// `ApprovalRouter::defer` *before* the synthetic pending result is
    /// returned, so the row exists the instant deferral commits.
    fn insert_core(
        &self,
        core: &PendingApprovalCore,
        workspace_id: Option<&str>,
        thread_ts: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Record one delivery target (the DM ACL). Idempotent on
    /// `(request_id, approver_id)`.
    fn add_target(&self, request_id: &str, approver_id: &str, slack_user_id: &str) -> Result<()>;

    /// Fill the DM coordinates after a target's DM is posted, so the
    /// fan-out rewrite can find every approver's message.
    fn set_target_message(
        &self,
        request_id: &str,
        approver_id: &str,
        dm_channel: &str,
        message_ts: &str,
    ) -> Result<()>;

    /// Record the origin-channel "⏳ waiting" notice coordinates.
    fn set_channel_notice(&self, request_id: &str, channel: &str, ts: &str) -> Result<()>;

    fn get(&self, request_id: &str) -> Result<Option<PendingApproval>>;
    fn targets(&self, request_id: &str) -> Result<Vec<Target>>;

    /// Atomic first-wins claim: transitions `pending` → resolved for
    /// exactly one caller. Returns `Ok(true)` iff this call performed the
    /// transition (`rows_affected == 1`).
    fn claim(&self, request_id: &str, outcome: ApprovalOutcome, resolver_id: &str) -> Result<bool>;

    /// Rows still `pending` whose `expires_at` is at or before `now`.
    fn due_for_expiry(&self, now: DateTime<Utc>) -> Result<Vec<PendingApproval>>;

    /// Resolved rows whose DMs may still show live buttons — used by the
    /// boot reconciliation pass. Bounded; ordered most-recent first.
    fn recently_resolved(&self, limit: usize) -> Result<Vec<PendingApproval>>;

    /// Every request `requester` raised, resolved or not, newest first.
    fn requested_by(&self, requester: &str) -> Result<Vec<PendingApproval>>;

    /// How many requests name `approver` among their approvers.
    fn approver_targets(&self, approver: &str) -> Result<usize>;

    /// Hand `from`'s still-pending requests to `to`. Resolved ones are a
    /// record of who asked and stay as they are.
    fn reassign_requester(&self, from: &str, to: &str) -> Result<usize>;

    /// Delete every request `requester` raised, with its targets: each
    /// carries the tool call's full arguments.
    fn forget_requester(&self, requester: &str) -> Result<usize>;

    /// Take `approver` off every request's approvers. A pending request is
    /// left to whoever else may approve it, or to expire.
    fn drop_approver(&self, approver: &str) -> Result<usize>;

    /// Delete resolved or expired requests whose expiry fell before
    /// `before`, with their targets, one bounded pass at a time
    /// ([`workforce_common::telemetry::prune_in_batches`]); returns how many
    /// went. A pending request is never deleted.
    fn prune_resolved(&self, before: DateTime<Utc>) -> Result<usize>;

    /// For everyone who raised a request or is one's approver, how many
    /// requests and approver slots name them.
    fn identity_holders(&self) -> Result<std::collections::BTreeMap<String, usize>>;
}

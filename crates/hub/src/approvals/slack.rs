//! Slack delivery, button authorisation, and resolution fan-out for
//! routed approvals.
//!
//! Pure block construction is reused from `slack_blocks` /
//! `slack_manager`; this module owns the *effects* — opening DMs,
//! posting/updating messages, and recording delivery coordinates on the
//! store. Persistence always precedes every user-visible post.

use std::sync::{Arc, Weak};

use tokio::sync::mpsc;
use uuid::Uuid;
use workforce_agent::runner::ToolInvocation;
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_core::policy::{PermissionChoice, PermissionPromptOptions};

use crate::hosts::{RegistryView, RoutedApproval};
use crate::hub_core::approvals::{ApprovalOutcome, PendingApproval, PendingApprovalStore, Target};
use crate::serve::AgentService;
use crate::slack_manager::{permission_ack_text, resolved_permission_blocks, ResolutionOutcome};

/// Who clicked a self-serve approval card, with the standing they click
/// with. The same check answers a click for a `/cc` turn and for a runner
/// turn.
pub struct Clicker<'a> {
    pub slack_id: &'a str,
    pub identity_store: Option<Arc<dyn workforce_identity::store::IdentityStore>>,
}

/// What a click on a self-serve approval card came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Delivered,
    /// The clicker may not give this answer; they are told why.
    Refused(&'static str),
    /// No turn holds the prompt the card names.
    Unheld,
}

impl Clicker<'_> {
    /// Whether this clicker may give `choice` to a prompt raised in
    /// `requester`'s turn. The requester may answer their own prompt; someone
    /// who manages permissions may allow or deny it once, but an Always
    /// answer saves a rule for the requester, so only they may give it.
    pub fn may_answer(
        &self,
        requester: &str,
        choice: PermissionChoice,
    ) -> Result<(), &'static str> {
        if self.slack_id == requester {
            return Ok(());
        }
        if matches!(
            choice,
            PermissionChoice::AllowAlways | PermissionChoice::DenyAlways
        ) {
            return Err("Only the person who asked can choose Always: it saves a rule for them.");
        }
        let store = self.identity_store.as_ref();
        store
            .and_then(|s| s.get_by_linked_id("slack", self.slack_id).ok().flatten())
            .is_some_and(|identity| {
                crate::authz::holds_capability(
                    store,
                    &identity.id.to_string(),
                    crate::authz::Capability::PermissionsManage,
                )
            })
            .then_some(())
            .ok_or(
                "Only the person who asked, or someone who manages permissions, can answer this.",
            )
    }
}

/// Resolve an internal identity UUID to its linked Slack id, if any.
fn slack_id_for(svc: &AgentService, user_id: &str) -> Option<String> {
    let store = svc.identity_store.read().clone()?;
    let uuid = Uuid::parse_str(user_id).ok()?;
    let identity = store.get_by_id(uuid).ok().flatten()?;
    identity
        .linked_ids
        .get("slack")
        .filter(|s| !s.is_empty())
        .cloned()
}

/// Deliver a routed approval: persist one target per approver, DM each,
/// then post the origin-channel "⏳ waiting" notice (skipped for an origin
/// with no channel, such as a remote MCP call). Called from the
/// `ApprovalRequired` progress handler once the durable core row exists
/// (written by `defer`). Best-effort per target — an approver whose DM
/// fails is logged, not fatal, since others may still resolve it.
#[allow(clippy::too_many_arguments)]
pub async fn deliver_routed_approval(
    svc: &Arc<AgentService>,
    surface: &Arc<dyn ChannelSurface>,
    store: &Arc<dyn PendingApprovalStore>,
    request_id: &str,
    agent_id: &str,
    tool_name: &str,
    detail: &str,
    reason: &str,
    options: &PermissionPromptOptions,
    approver_user_ids: &[String],
    requester_user_id: &str,
    origin_channel: &str,
    origin_thread: Option<&str>,
) {
    let requester_slack = slack_id_for(svc, requester_user_id).unwrap_or_default();
    let mut delivered_mentions: Vec<String> = Vec::new();

    for approver_id in approver_user_ids {
        let Some(slack_id) = slack_id_for(svc, approver_id) else {
            tracing::warn!(approver = %approver_id, "routed approver has no Slack link at delivery time; skipping DM");
            continue;
        };
        if let Err(e) = store.add_target(request_id, approver_id, &slack_id) {
            tracing::warn!(error = %e, request = %request_id, "failed to persist approval target");
            continue;
        }
        super::approvers_changed([approver_id.as_str()]);
        let dm_channel = match surface.open_conversation(&slack_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, approver = %slack_id, "failed to open approval DM");
                continue;
            }
        };
        let blocks = crate::slack_blocks::routed_permission_prompt_blocks(
            agent_id,
            tool_name,
            detail,
            reason,
            request_id,
            options,
            &requester_slack,
        );
        let fallback = format!(
            ":warning: Approval required: agent `{agent_id}` wants to run `{tool_name}` \
             (requested by <@{requester_slack}>). Reply `approve {request_id}` or `reject {request_id}`."
        );
        match surface
            .send_message(&OutboundMessage {
                channel: dm_channel.clone(),
                text: fallback,
                thread_id: None,
                username: None,
                icon_url: None,
                blocks: Some(blocks),
            })
            .await
        {
            Ok(sent) => {
                if let Err(e) =
                    store.set_target_message(request_id, approver_id, &dm_channel, &sent.message_id)
                {
                    tracing::warn!(error = %e, "failed to record approval DM coordinates");
                }
                delivered_mentions.push(format!("<@{slack_id}>"));
            }
            Err(e) => {
                tracing::warn!(error = %e, approver = %slack_id, "failed to post approval DM")
            }
        }
    }

    if origin_channel.is_empty() {
        return;
    }
    let who = if delivered_mentions.is_empty() {
        "a designated approver".to_string()
    } else {
        delivered_mentions.join(", ")
    };
    match surface
        .send_message(&OutboundMessage {
            channel: origin_channel.to_string(),
            text: format!("⏳ Waiting for approval from {who} to run `{tool_name}`."),
            thread_id: origin_thread.map(str::to_string),
            username: None,
            icon_url: None,
            blocks: None,
        })
        .await
    {
        Ok(sent) => {
            if let Err(e) = store.set_channel_notice(request_id, origin_channel, &sent.message_id) {
                tracing::warn!(error = %e, "failed to record channel notice coordinates");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to post origin-channel approval notice"),
    }
}

const ROUTED_BUFFER: usize = 16;

/// DM routed approvals to their approvers for as long as the returned sender
/// lives and `registry` does. Delivery is best effort: a hub without a Slack
/// surface or approval store logs and lets the row expire.
pub fn spawn_routed_delivery(registry: Weak<dyn RegistryView>) -> mpsc::Sender<RoutedApproval> {
    let (tx, mut rx) = mpsc::channel(ROUTED_BUFFER);
    tokio::spawn(async move {
        while let Some(routed) = rx.recv().await {
            let Some(registry) = registry.upgrade() else {
                return;
            };
            deliver(registry.as_ref(), routed).await;
        }
    });
    tx
}

/// Hand a routed approval to delivery without ever blocking the caller:
/// the durable row exists either way, so a backed-up delivery queue costs
/// approvers a DM, not a turn its progress.
pub fn hand_off(routed: &mpsc::Sender<RoutedApproval>, approval: RoutedApproval) {
    if let Err(e) = routed.try_send(approval) {
        tracing::warn!(error = %e, "routed approval not handed to delivery; its approvers will not be messaged");
    }
}

async fn deliver(registry: &dyn RegistryView, routed: RoutedApproval) {
    let Some(svc) = registry.get(&routed.agent_id) else {
        tracing::warn!(agent = %routed.agent_id, request = %routed.request_id, "routed approval names an unknown agent");
        return;
    };
    let store = svc.pending_approval_store.read().clone();
    let surface = svc
        .surface_registry
        .read()
        .await
        .all_for("slack")
        .into_iter()
        .next();
    let (Some(store), Some(surface)) = (store, surface) else {
        tracing::warn!(request = %routed.request_id, "routed approval has no Slack surface or store to reach its approvers");
        return;
    };
    deliver_routed_approval(
        &svc,
        &surface,
        &store,
        &routed.request_id,
        &routed.agent_id,
        &routed.tool_name,
        &routed.detail,
        &routed.reason,
        &routed.options,
        &routed.approver_user_ids,
        &routed.requester_user_id,
        routed.channel.as_deref().unwrap_or_default(),
        routed.thread.as_deref(),
    )
    .await;
}

/// One routed approval carried out of a non-progress run in its invocation
/// record. The runner writes the request id, the resolved approvers (the
/// pending row does not persist them), and the human-readable detail into
/// the invocation's `output` — the progress path records `Null` there, so
/// its consumer-delivered approvals can never be extracted twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutedRequest {
    pub request_id: String,
    pub approver_user_ids: Vec<String>,
    pub detail: String,
}

/// Extract the routed approvals a run carried out in its invocation records.
/// Pure; ignores non-approval invocations, the progress loop's `Null`-output
/// shape, and any record without a string `approval_request_id`.
pub(crate) fn routed_requests(invocations: &[ToolInvocation]) -> Vec<RoutedRequest> {
    invocations
        .iter()
        .filter(|inv| inv.policy_decision == "require_approval")
        .filter_map(|inv| {
            let request_id = inv.output.get("approval_request_id")?.as_str()?.to_string();
            let approver_user_ids = inv
                .output
                .get("approver_user_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let detail = inv
                .output
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(RoutedRequest {
                request_id,
                approver_user_ids,
                detail,
            })
        })
        .collect()
}

/// Deliver the approver DMs + origin-channel notice for every routed
/// approval a non-progress run (scheduler, monitor, resume, slash command)
/// raised. The progress path's consumer owns delivery for live turns; this
/// is its post-run counterpart, called by `process_message` right after
/// invocation recording. Zero cost when the turn routed nothing. A missing
/// store/row/surface is warn-skipped — the row stays pending and the
/// sweeper's expiry → denial-directive resume is the documented degradation.
pub async fn deliver_routed_for_invocations(
    svc: &Arc<AgentService>,
    invocations: &[ToolInvocation],
) {
    let routed = routed_requests(invocations);
    if routed.is_empty() {
        return;
    }
    let Some(store) = svc.pending_approval_store.read().clone() else {
        tracing::warn!("routed approvals raised but no pending-approval store is configured");
        return;
    };
    // The person a run made in Desk acts for is asked there; designated
    // approvers in Slack, as ever.
    let desk = workforce_agent::runner::current_desk().filter(|scope| !scope.attended);
    for req in routed {
        let row = match store.get(&req.request_id) {
            Ok(Some(row)) if row.is_pending() => row,
            Ok(_) => {
                tracing::warn!(request = %req.request_id, "routed approval row missing or no longer pending; skipping delivery");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, request = %req.request_id, "failed to load routed approval row; skipping delivery");
                continue;
            }
        };
        let (on_desk, in_slack): (Vec<String>, Vec<String>) = req
            .approver_user_ids
            .iter()
            .cloned()
            .partition(|approver| desk.is_some() && *approver == row.requester_id);
        if let (Some(scope), false) = (&desk, on_desk.is_empty()) {
            super::desk::deliver_routed_approval(svc, &store, &row, &req.detail, &scope.target)
                .await;
        }
        if in_slack.is_empty() {
            continue;
        }
        let Some(surface) = resolution_surface(svc, &row).await else {
            tracing::warn!(request = %req.request_id, "no Slack surface to deliver routed approval; row stays pending for the sweeper");
            continue;
        };
        deliver_routed_approval(
            svc,
            &surface,
            &store,
            &req.request_id,
            &row.agent_id,
            &row.tool_name,
            &req.detail,
            &row.reason,
            &PermissionPromptOptions::ask(false),
            &in_slack,
            &row.requester_id,
            row.channel_id.as_deref().unwrap_or_default(),
            row.thread_ts.as_deref(),
        )
        .await;
    }
}

/// Rewrite every approver's DM and the origin-channel notice to the
/// resolved state, dropping the buttons. Idempotent: a target whose DM
/// coordinates aren't recorded (or whose update fails) is simply skipped,
/// and re-running after a previous rewrite is harmless. Used by both the
/// button path (approve/deny) and the sweeper (expire).
pub async fn rewrite_all_targets(
    svc: &Arc<AgentService>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
    outcome: ApprovalOutcome,
    resolver_user_id: &str,
) {
    let surface = match resolution_surface(svc, row).await {
        Some(s) => s,
        None => {
            tracing::warn!(request = %row.request_id, "no surface to rewrite resolved approval");
            return;
        }
    };
    let (res_outcome, ack) = outcome_blocks(outcome);
    let resolver_display = resolver_display(svc, resolver_user_id);

    let prompt = reconstructed_prompt(svc, row);
    let targets = store.targets(&row.request_id).unwrap_or_default();
    for t in &targets {
        let (Some(dm), Some(ts)) = (&t.dm_channel, &t.message_ts) else {
            continue;
        };
        let blocks = resolved_permission_blocks(
            Some(&prompt),
            res_outcome,
            ack.clone(),
            Some(&resolver_display),
            resolver_user_id,
        );
        let outbound = OutboundMessage {
            channel: dm.clone(),
            text: ack.clone(),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: Some(blocks),
        };
        if let Err(e) = surface.update_message(dm, ts, &outbound).await {
            tracing::warn!(error = %e, request = %row.request_id, "failed to rewrite approver DM");
        }
    }

    if let (Some(channel), Some(ts)) = (&row.channel_notice, &row.channel_notice_ts) {
        let outbound = OutboundMessage {
            channel: channel.clone(),
            text: format!("{ack} by {resolver_display}"),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: None,
        };
        if let Err(e) = surface.update_message(channel, ts, &outbound).await {
            tracing::warn!(error = %e, "failed to rewrite origin-channel approval notice");
        }
    }
}

/// Rewrite just one clicker's DM (the loser of a first-wins race). The
/// clicker still sees the resolved card even though someone else decided.
pub async fn rewrite_single_target(
    svc: &Arc<AgentService>,
    surface: &Arc<dyn ChannelSurface>,
    target: &Target,
    row: &PendingApproval,
) {
    let outcome = ApprovalOutcome::from_str_lossy(&row.status).unwrap_or(ApprovalOutcome::Denied);
    let (res_outcome, ack) = outcome_blocks(outcome);
    let resolver_display = resolver_display(svc, row.resolved_by.as_deref().unwrap_or_default());
    let (Some(dm), Some(ts)) = (&target.dm_channel, &target.message_ts) else {
        return;
    };
    let prompt = reconstructed_prompt(svc, row);
    let blocks = resolved_permission_blocks(
        Some(&prompt),
        res_outcome,
        ack.clone(),
        Some(&resolver_display),
        row.resolved_by.as_deref().unwrap_or_default(),
    );
    let outbound = OutboundMessage {
        channel: dm.clone(),
        text: ack,
        thread_id: None,
        username: None,
        icon_url: None,
        blocks: Some(blocks),
    };
    if let Err(e) = surface.update_message(dm, ts, &outbound).await {
        tracing::warn!(error = %e, "failed to rewrite loser DM");
    }
}

/// Rebuild the original prompt's Block Kit array from the persisted row,
/// so the resolved card keeps the (flipped) header + detail. The buttons
/// it carries are stripped by `resolved_permission_blocks`. Reuses the
/// same builder and the same summary of the call the DM was posted with,
/// so a resolved card shows an approver no more of the requester's call
/// than the live one did.
fn reconstructed_prompt(svc: &AgentService, row: &PendingApproval) -> serde_json::Value {
    let requester_slack = slack_id_for(svc, &row.requester_id).unwrap_or_default();
    let detail =
        workforce_agent::runner::approval_detail(&svc.tools, &row.tool_name, &row.tool_input);
    let blocks = crate::slack_blocks::routed_permission_prompt_blocks(
        &row.agent_id,
        &row.tool_name,
        &detail,
        &row.reason,
        &row.request_id,
        &PermissionPromptOptions::default(),
        &requester_slack,
    );
    serde_json::Value::Array(blocks)
}

fn outcome_blocks(outcome: ApprovalOutcome) -> (ResolutionOutcome, String) {
    match outcome {
        ApprovalOutcome::Approved => (
            ResolutionOutcome::Approved,
            permission_ack_text(PermissionChoice::AllowOnce),
        ),
        ApprovalOutcome::Denied => (
            ResolutionOutcome::Denied,
            permission_ack_text(PermissionChoice::DenyOnce),
        ),
        ApprovalOutcome::Expired => (
            ResolutionOutcome::Expired,
            "⌛ expired without approval".to_string(),
        ),
    }
}

/// Human-readable resolver: the identity's display name, falling back to
/// a Slack mention when it can be resolved, else the raw id.
fn resolver_display(svc: &AgentService, resolver_user_id: &str) -> String {
    if let Some(store) = svc.identity_store.read().clone() {
        if let Ok(uuid) = Uuid::parse_str(resolver_user_id) {
            if let Ok(Some(identity)) = store.get_by_id(uuid) {
                if let Some(slack) = identity.linked_ids.get("slack").filter(|s| !s.is_empty()) {
                    return format!("<@{slack}>");
                }
                if !identity.display_name.is_empty() {
                    return identity.display_name;
                }
            }
        }
    }
    if resolver_user_id.is_empty() {
        "the system".to_string()
    } else {
        format!("<@{resolver_user_id}>")
    }
}

/// Pick a Slack surface for the origin workspace to perform the rewrite.
async fn resolution_surface(
    svc: &Arc<AgentService>,
    row: &PendingApproval,
) -> Option<Arc<dyn ChannelSurface>> {
    let registry = svc.surface_registry.read().await;
    if let Some(ws) = &row.workspace_id {
        let key = workforce_common::channels::SurfaceKey::workspace("slack", ws.clone());
        if let Some(s) = registry.get(&key) {
            return Some(Arc::clone(s));
        }
    }
    registry.all_for("slack").into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(policy_decision: &str, output: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            tool_name: "vault.delete".to_string(),
            input: serde_json::json!({"id": 7}),
            output,
            success: false,
            policy_decision: policy_decision.to_string(),
            duration_ms: 0,
            denial_reason: None,
            error: None,
        }
    }

    /// The card an approver keeps after a request is settled shows the call
    /// as the live card did, not the rest of what the requester passed it.
    #[tokio::test]
    async fn a_resolved_card_shows_no_more_of_the_call_than_the_live_one() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let now = chrono::Utc::now();
        let row = PendingApproval {
            request_id: "r1".to_string(),
            agent_id: "agent".to_string(),
            session_key: "slack:w:D1:1".to_string(),
            workspace_id: None,
            channel_id: None,
            thread_ts: None,
            requester_id: "alice".to_string(),
            tool_name: "demo.post".to_string(),
            tool_input: serde_json::json!({
                "method": "POST",
                "url": "https://example.test/notes",
                "body": "REQUESTERS-PRIVATE-BODY",
            }),
            reason: "needs approval".to_string(),
            allow_self_approval: false,
            status: "approved".to_string(),
            resolved_by: None,
            channel_notice: None,
            channel_notice_ts: None,
            created_at: now,
            expires_at: now,
        };

        let card = reconstructed_prompt(&service, &row).to_string();

        assert!(card.contains("POST https://example.test/notes"), "{card}");
        assert!(!card.contains("REQUESTERS-PRIVATE-BODY"), "{card}");
    }

    #[test]
    fn routed_requests_extracts_the_runner_payload() {
        let payload = serde_json::json!({
            "approval_request_id": "req-1",
            "approver_user_ids": ["bob", "carol"],
            "detail": "{\"id\":7}",
        });
        let extracted = routed_requests(&[invocation("require_approval", payload)]);
        assert_eq!(
            extracted,
            vec![RoutedRequest {
                request_id: "req-1".to_string(),
                approver_user_ids: vec!["bob".to_string(), "carol".to_string()],
                detail: "{\"id\":7}".to_string(),
            }]
        );
    }

    #[test]
    fn routed_requests_ignores_progress_shapes_and_non_approvals() {
        let invocations = vec![
            // Progress-loop routed shape: consumer already delivered.
            invocation("require_approval", serde_json::Value::Null),
            // Legacy self-serve instant error.
            invocation("require_approval", serde_json::Value::Null),
            // Ordinary allow/deny outcomes.
            invocation("allow", serde_json::json!({"rows": 3})),
            invocation("deny", serde_json::json!({"approval_request_id": "req-x"})),
            // Approval-shaped but no string id.
            invocation(
                "require_approval",
                serde_json::json!({"approval_request_id": 42}),
            ),
        ];
        assert!(routed_requests(&invocations).is_empty());
    }

    #[test]
    fn routed_requests_tolerates_missing_approvers_and_detail() {
        let extracted = routed_requests(&[invocation(
            "require_approval",
            serde_json::json!({"approval_request_id": "req-2"}),
        )]);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].request_id, "req-2");
        assert!(extracted[0].approver_user_ids.is_empty());
        assert!(extracted[0].detail.is_empty());
    }

    /// The coordinate handshake: the runner's invocation payload keys the
    /// durable row `defer` persisted, and the row still carries everything
    /// the post-run delivery needs (tool, requester, channel, thread) plus
    /// target/notice writes for the DM fan-out.
    #[test]
    fn routed_payload_keys_the_persisted_row_and_delivery_writes() {
        use crate::approvals::store::SqlitePendingApprovalStore;
        use workforce_agent::runner::PendingApprovalCore;

        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let core = PendingApprovalCore {
            request_id: "req-sched-1".to_string(),
            agent_id: "atlas".to_string(),
            session_key: "slack:T1:D_SCHED:".to_string(),
            channel_id: "D_SCHED".to_string(),
            requester_user_id: "scheduler".to_string(),
            tool_name: "vault.delete".to_string(),
            tool_input: serde_json::json!({"id": 7}),
            reason: "four-eyes".to_string(),
            allow_self_approval: false,
            approver_user_ids: vec!["bob".to_string()],
            ttl: std::time::Duration::from_secs(3600),
        };
        store
            .insert_core(
                &core,
                Some("T1"),
                None,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();

        let payload = serde_json::json!({
            "approval_request_id": "req-sched-1",
            "approver_user_ids": ["bob"],
            "detail": "{\"id\":7}",
        });
        let routed = routed_requests(&[invocation("require_approval", payload)]);
        assert_eq!(routed.len(), 1);

        let row = store.get(&routed[0].request_id).unwrap().expect("row");
        assert!(row.is_pending());
        assert_eq!(row.tool_name, "vault.delete");
        assert_eq!(row.requester_id, "scheduler");
        assert_eq!(row.channel_id.as_deref(), Some("D_SCHED"));
        assert_eq!(row.workspace_id.as_deref(), Some("T1"));

        // The delivery writes the DM ACL + notice coordinates on that row.
        store
            .add_target(&row.request_id, "bob", "U_BOB")
            .expect("target persisted");
        store
            .set_target_message(&row.request_id, "bob", "D_BOB", "111.222")
            .expect("dm coordinates persisted");
        store
            .set_channel_notice(&row.request_id, "D_SCHED", "333.444")
            .expect("notice coordinates persisted");
        let targets = store.targets(&row.request_id).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].slack_user_id, "U_BOB");
        assert_eq!(targets[0].dm_channel.as_deref(), Some("D_BOB"));
        let row = store.get(&row.request_id).unwrap().unwrap();
        assert_eq!(row.channel_notice.as_deref(), Some("D_SCHED"));
        assert_eq!(row.channel_notice_ts.as_deref(), Some("333.444"));
    }

    #[test]
    fn outcome_blocks_map_each_terminal_state() {
        let (o, ack) = outcome_blocks(ApprovalOutcome::Approved);
        assert_eq!(o, ResolutionOutcome::Approved);
        assert!(ack.contains("allowed"));
        let (o, ack) = outcome_blocks(ApprovalOutcome::Denied);
        assert_eq!(o, ResolutionOutcome::Denied);
        assert!(ack.contains("denied"));
        let (o, ack) = outcome_blocks(ApprovalOutcome::Expired);
        assert_eq!(o, ResolutionOutcome::Expired);
        assert!(ack.contains("expired"));
    }

    /// The resolved DM card reconstructed for a routed approval keeps the
    /// (flipped) header + detail and drops the action buttons.
    #[test]
    fn resolved_routed_card_flips_header_and_strips_buttons() {
        let prompt = crate::slack_blocks::routed_permission_prompt_blocks(
            "atlas",
            "vault.delete",
            "{\"id\":7}",
            "four-eyes rule",
            "req-1",
            &PermissionPromptOptions::default(),
            "U_ALICE",
        );
        let original = serde_json::Value::Array(prompt);
        let resolved = resolved_permission_blocks(
            Some(&original),
            ResolutionOutcome::Approved,
            permission_ack_text(PermissionChoice::AllowOnce),
            Some("<@U_BOB>"),
            "bob",
        );
        let dump = serde_json::to_string(&resolved).unwrap();
        assert!(
            !resolved
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("actions")),
            "buttons must be gone: {dump}"
        );
        assert!(
            dump.contains("Approval granted"),
            "header not flipped: {dump}"
        );
        assert!(dump.contains("resolved by <@U_BOB>"), "no resolver: {dump}");
        assert!(
            dump.contains("Requested by <@U_ALICE>"),
            "requester context lost: {dump}"
        );
    }
}

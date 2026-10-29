//! A routed approval whose approver is reached in Desk: the person a run
//! made in one of their Desk conversations acts for, asked there to approve
//! a call of the run's. They answer through
//! `POST /api/approvals/{request_id}/decision`.

use std::sync::Arc;

use workforce_common::channels::desk::{DeliveryKind, DeskTarget};

use super::store::{PendingApproval, PendingApprovalStore};
use crate::serve::AgentService;

/// What the person is shown: the call, why it needs them, and what it would
/// do.
fn asking(row: &PendingApproval, detail: &str) -> String {
    let what = [row.reason.trim(), detail.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Approval needed: `{}` wants to run `{}`.\n{what}",
        row.agent_id, row.tool_name
    )
}

/// Ask `row`'s requester at `target` to approve it, recording them as its
/// approver. Whether the request reached their conversation.
pub(crate) async fn deliver_routed_approval(
    svc: &Arc<AgentService>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
    detail: &str,
    target: &DeskTarget,
) -> bool {
    if let Err(e) = store.add_target(&row.request_id, &row.requester_id, "") {
        tracing::warn!(error = %e, request = %row.request_id, "failed to record the Desk approver");
        return false;
    }
    let delivery = crate::desk_surface::Delivery {
        request_id: Some(row.request_id.clone()),
        ..crate::desk_surface::Delivery::text(DeliveryKind::Approval, asking(row, detail))
    };
    match crate::desk_surface::deliver(svc, &row.requester_id, target, delivery).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, request = %row.request_id, "failed to ask for approval on Desk");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ask_names_the_agent_the_call_and_why() {
        let row = PendingApproval {
            request_id: "r1".into(),
            agent_id: "scout".into(),
            session_key: "scheduler:j1".into(),
            workspace_id: None,
            channel_id: None,
            thread_ts: None,
            requester_id: "p1".into(),
            tool_name: "slack.send".into(),
            tool_input: serde_json::json!({}),
            reason: "Posts outside Desk".into(),
            allow_self_approval: true,
            status: "pending".into(),
            resolved_by: None,
            channel_notice: None,
            channel_notice_ts: None,
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now(),
        };
        assert_eq!(
            asking(&row, "  "),
            "Approval needed: `scout` wants to run `slack.send`.\nPosts outside Desk"
        );
    }
}

//! Resume launcher — deterministic pre-execution of the approved action.
//!
//! On **approve** a fresh turn runs in the original session as the
//! requester; the runner pre-executes the persisted `(tool, input)` via
//! the [`CURRENT_PREEXECUTE`] task-local, seeds the real result into
//! history, and continues. On **deny**/**expire** the same turn runs with
//! no pre-execution and a directive so the agent reacts (tells the user /
//! picks another path) instead of silently dropping the thread.

use std::sync::Arc;

use workforce_agent::runner::{with_desk, PreExecutedTool, CURRENT_CHANNEL_ID, CURRENT_PREEXECUTE};
use workforce_common::channels::OutboundMessage;

use super::store::{ApprovalOutcome, PendingApproval};
use crate::agent_registry::AgentRegistry;
use crate::serve::AgentService;

/// Resolve the [`AgentService`] for a pending row's agent. The resume turn
/// MUST run on the agent that requested the approval — its system prompt
/// and tool registry are what the pre-executed action depends on — so we
/// look it up by `row.agent_id` and only fall back to the default service
/// when that agent is unknown (e.g. renamed since the request was filed).
pub fn resume_service(
    registry: &Arc<AgentRegistry>,
    row: &PendingApproval,
) -> Option<Arc<AgentService>> {
    registry
        .get(&row.agent_id)
        .or_else(|| registry.resolve(None))
}

/// Launch the resume turn for a resolved approval and deliver the agent's
/// response into the original conversation. Runs on the requesting agent's
/// service. `resolver_display` is a human-readable mention/name for the
/// directive.
pub async fn launch_resume(
    registry: &Arc<AgentRegistry>,
    row: &PendingApproval,
    outcome: ApprovalOutcome,
    resolver_display: &str,
) {
    let Some(svc) = resume_service(registry, row) else {
        tracing::warn!(request = %row.request_id, agent = %row.agent_id, "no service to resume approval");
        return;
    };
    let svc = &svc;
    let directive = directive_for(row, outcome, resolver_display);
    let channel_id = row.channel_id.clone().unwrap_or_default();
    // A request made in a Desk conversation, or by a run made in one, is
    // resumed answering to it, unattended.
    let desk = svc
        .sessions
        .find(&row.session_key, &row.requester_id)
        .ok()
        .flatten()
        .and_then(|session| session.desk_origin())
        .map(|target| workforce_common::channels::desk::DeskScope {
            target,
            attended: false,
        });

    // Mirror the scheduler: scope the delivery channel so the resumed
    // turn loads the right per-user visibility, then (for approve) scope
    // the exact tool the human authorised so the runner pre-executes it.
    let result = match outcome {
        ApprovalOutcome::Approved => {
            let pre = PreExecutedTool {
                tool_name: row.tool_name.clone(),
                tool_input: row.tool_input.clone(),
            };
            with_desk(
                desk,
                CURRENT_CHANNEL_ID.scope(
                    channel_id.clone(),
                    CURRENT_PREEXECUTE.scope(pre, run_turn(svc, row, &directive)),
                ),
            )
            .await
        }
        ApprovalOutcome::Denied | ApprovalOutcome::Expired => {
            with_desk(
                desk,
                CURRENT_CHANNEL_ID.scope(channel_id.clone(), run_turn(svc, row, &directive)),
            )
            .await
        }
    };

    match result {
        Ok(response) if !response.trim().is_empty() => {
            deliver(svc, row, &response).await;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, request = %row.request_id, "approval resume turn failed")
        }
    }
}

async fn run_turn(
    svc: &Arc<AgentService>,
    row: &PendingApproval,
    directive: &str,
) -> eyre::Result<String> {
    // The session is keyed on `(channel = session_key, owner = user)`, so the
    // resume must pass the requester as `user` to reload the *original*
    // conversation (with the agent's "I've requested approval…" turn) rather
    // than minting a fresh empty session. `run_as` resolves the requester's
    // creds/memory the same way the original turn did.
    svc.process_message(
        &row.session_key,
        &row.requester_id,
        directive,
        None,
        None,
        row.channel_id.as_deref(),
        row.thread_ts.as_deref(),
        None,
        None,
        None,
        None,
        None,
        Some(&row.requester_id),
    )
    .await
}

fn directive_for(row: &PendingApproval, outcome: ApprovalOutcome, resolver: &str) -> String {
    let tool = &row.tool_name;
    match outcome {
        ApprovalOutcome::Approved => format!(
            "[Approval] {resolver} approved your request to run `{tool}`; the action has been \
             carried out — its result is above. Continue from that result."
        ),
        ApprovalOutcome::Denied => format!(
            "[Approval] {resolver} denied your request to run `{tool}`; do not perform it. \
             Inform the user and proceed accordingly."
        ),
        ApprovalOutcome::Expired => format!(
            "[Approval] your request to run `{tool}` expired without approval and was not \
             carried out. Inform the user and pick another path if appropriate."
        ),
    }
}

/// Deliver the resumed turn's final text to the original conversation:
/// its Slack thread, mirroring the scheduler's surface fan-out, else the
/// Desk conversation it answers to.
async fn deliver(svc: &Arc<AgentService>, row: &PendingApproval, response: &str) {
    let Some(channel) = row.channel_id.clone().filter(|c| !c.is_empty()) else {
        let session = svc
            .sessions
            .find(&row.session_key, &row.requester_id)
            .ok()
            .flatten();
        if let Some(session) = session {
            if let Err(e) =
                crate::desk_surface::resumed(svc, &session, &row.requester_id, response).await
            {
                tracing::warn!(error = %e, request = %row.request_id, "failed to deliver approval-resume response to Desk");
            }
        }
        return;
    };
    let registry = svc.surface_registry.read().await;
    let surfaces = match &row.workspace_id {
        Some(ws) => {
            let key = workforce_common::channels::SurfaceKey::workspace("slack", ws.clone());
            registry
                .get(&key)
                .map(|s| vec![Arc::clone(s)])
                .unwrap_or_else(|| registry.all_for("slack"))
        }
        None => registry.all_for("slack"),
    };
    for surface in surfaces {
        if let Err(e) = surface
            .send_message(&OutboundMessage {
                channel: channel.clone(),
                text: response.to_string(),
                thread_id: row.thread_ts.clone(),
                username: None,
                icon_url: None,
                blocks: None,
            })
            .await
        {
            tracing::warn!(error = %e, "failed to deliver approval-resume response");
        }
    }
}

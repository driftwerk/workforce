//! Deciding a routed approval, however the approver answers: a button in a
//! Slack DM, or Desk's `POST /api/approvals/{request_id}/decision`. The
//! first decision wins; it rewrites the approvers' Slack cards and resumes
//! the run that asked.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use workforce_core::policy::PermissionChoice;

use super::store::{ApprovalOutcome, PendingApproval, Target};
use crate::agent_registry::AgentRegistry;
use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};
use crate::serve::AgentService;

/// Who answers a routed approval.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Decider<'a> {
    /// A Slack user, by their Slack id.
    Slack(&'a str),
    /// A person, by their identity id.
    Person(&'a str),
}

impl Decider<'_> {
    /// Whether they are one of the request's approvers.
    pub(crate) fn eligible(&self, targets: &[Target]) -> bool {
        targets.iter().any(|target| match self {
            Self::Slack(slack_id) => !slack_id.is_empty() && target.slack_user_id == *slack_id,
            Self::Person(identity) => target.approver_id == *identity,
        })
    }

    /// The identity the decision is recorded as.
    fn resolver_id(&self, svc: &AgentService) -> String {
        match self {
            Self::Slack(slack_id) => svc
                .identity_store
                .read()
                .clone()
                .and_then(|store| store.get_by_linked_id("slack", slack_id).ok().flatten())
                .map(|identity| identity.id.to_string())
                .unwrap_or_else(|| slack_id.to_string()),
            Self::Person(identity) => identity.to_string(),
        }
    }

    /// How the resumed run names who decided.
    fn display(&self, svc: &AgentService) -> String {
        match self {
            Self::Slack(slack_id) => format!("<@{slack_id}>"),
            Self::Person(identity) => {
                crate::desk_surface::person_name(svc.identity_store.read().as_ref(), identity)
                    .unwrap_or_else(|| "The person you work for".to_string())
            }
        }
    }
}

/// What a decision came to.
#[derive(Debug)]
pub(crate) enum Decided {
    /// This decision resolved the request; the run resumes.
    Resolved(PendingApproval),
    /// Another decision, or expiry, got there first.
    AlreadyResolved(PendingApproval),
}

/// Why a decision was not taken.
#[derive(Debug)]
pub(crate) enum DecideError {
    /// No routed request has that id.
    Unknown,
    /// The decider is not one of its approvers.
    NotAnApprover,
    Failed(eyre::Report),
}

/// Decide the routed request `request_id` as `decider`, choosing `choice`.
pub(crate) async fn decide(
    registry: &Arc<AgentRegistry>,
    request_id: &str,
    decider: Decider<'_>,
    choice: PermissionChoice,
) -> Result<Decided, DecideError> {
    let svc = registry.resolve(None).ok_or(DecideError::Unknown)?;
    let store = svc
        .pending_approval_store
        .read()
        .clone()
        .ok_or(DecideError::Unknown)?;
    let row = store
        .get(request_id)
        .map_err(DecideError::Failed)?
        .ok_or(DecideError::Unknown)?;
    let targets = store.targets(request_id).map_err(DecideError::Failed)?;
    if !decider.eligible(&targets) {
        return Err(DecideError::NotAnApprover);
    }
    let outcome = match choice.is_allow() {
        true => ApprovalOutcome::Approved,
        false => ApprovalOutcome::Denied,
    };
    let resolver_id = decider.resolver_id(&svc);
    let won = store
        .claim(request_id, outcome, &resolver_id)
        .map_err(DecideError::Failed)?;
    let row = store.get(request_id).ok().flatten().unwrap_or(row);
    if !won {
        return Ok(Decided::AlreadyResolved(row));
    }
    super::approvers_changed(targets.iter().map(|t| t.approver_id.as_str()));
    super::slack::rewrite_all_targets(&svc, &store, &row, outcome, &resolver_id).await;
    super::resume::launch_resume(registry, &row, outcome, &decider.display(&svc)).await;
    Ok(Decided::Resolved(row))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DecisionBody {
    choice: PermissionChoice,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DecisionQuery {
    token: Option<String>,
}

fn refused(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// `POST /api/approvals/{request_id}/decision {choice}`: the signed-in
/// person, or the person a paired device belongs to, decides a routed
/// approval they are an approver of. Answers `{"status": "approved" |
/// "denied" | "already_resolved", "resolved_by"?}`.
pub(crate) async fn decision(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Query(query): Query<DecisionQuery>,
    Json(body): Json<DecisionBody>,
) -> Response {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => {
            return refused(StatusCode::UNAUTHORIZED, "not signed in")
        }
        Some(AuthFailure::StoreError) => {
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not check that token",
            )
        }
        None => {}
    }
    let Some(person) = caller.identity_id.as_deref() else {
        return refused(
            StatusCode::FORBIDDEN,
            "only a person decides an approval: sign in, or pair this device to yourself",
        );
    };
    if crate::standing::suspended(state.identity_store.as_ref(), person).is_some() {
        return refused(StatusCode::FORBIDDEN, "this person is suspended");
    }
    match decide(
        &state.registry,
        &request_id,
        Decider::Person(person),
        body.choice,
    )
    .await
    {
        Ok(Decided::Resolved(row)) => Json(json!({ "status": row.status })).into_response(),
        Ok(Decided::AlreadyResolved(row)) => Json(json!({
            "status": "already_resolved",
            "resolved_by": row.resolved_by,
        }))
        .into_response(),
        Err(DecideError::Unknown) => refused(StatusCode::NOT_FOUND, "no such approval request"),
        Err(DecideError::NotAnApprover) => refused(
            StatusCode::FORBIDDEN,
            "you are not an approver of this request",
        ),
        Err(DecideError::Failed(e)) => {
            tracing::warn!(error = %e, request = %request_id, "deciding an approval failed");
            refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not record the decision; retry",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(approver: &str, slack: &str) -> Target {
        Target {
            request_id: "r1".into(),
            approver_id: approver.into(),
            slack_user_id: slack.into(),
            dm_channel: None,
            message_ts: None,
        }
    }

    #[test]
    fn only_an_approver_decides_by_whichever_way_they_answer() {
        let targets = [target("p1", "U1"), target("p2", "")];
        assert!(Decider::Slack("U1").eligible(&targets));
        assert!(!Decider::Slack("U9").eligible(&targets));
        assert!(Decider::Person("p2").eligible(&targets));
        assert!(!Decider::Person("p3").eligible(&targets));
        assert!(
            !Decider::Slack("").eligible(&targets),
            "a Desk approver's empty Slack id is no one's"
        );
    }
}

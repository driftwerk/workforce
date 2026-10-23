//! The eval endpoints `workforce eval` calls, each as a person:
//!
//! - `POST /api/evals/grade`: the eval grader over HTTP, admitted by the
//!   person's budget and billed to them, as for a chat turn.
//! - `POST /api/evals/know-how`: what their eval turns on an agent would be
//!   shown of its know-how, counted ([`crate::know_how::preview`]).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::eval_grader::{defect, GradeRequest, GradeResponse};
use crate::gateway::{
    check_action_rate_limit, check_owner_auth, rate_limited_response, resolve_caller_context,
    AgentViewer, GatewayState, WsQuery,
};
use crate::know_how::preview::{Preview, PreviewRequest, MAX_QUESTIONS, MAX_QUESTION_CHARS};

fn refused(status: StatusCode, reason: &str) -> Response {
    (status, Json(serde_json::json!({ "error": reason }))).into_response()
}

pub async fn grade(
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
    Json(request): Json<GradeRequest>,
) -> Result<Json<GradeResponse>, Response> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let person = caller
        .identity_id
        .clone()
        .ok_or_else(|| refused(StatusCode::UNAUTHORIZED, "grading needs a person's sign-in"))?;
    if !caller.has_scope(crate::jwt::SCOPE_CHAT) {
        return Err(refused(StatusCode::FORBIDDEN, "the chat scope is required"));
    }
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(rate_limited_response)?;
    if let Some(reason) = defect(&request) {
        return Err(refused(StatusCode::BAD_REQUEST, reason));
    }
    let agent_id = request
        .agent_id
        .clone()
        .unwrap_or_else(|| state.registry.default_id().to_string());
    let service = state
        .registry
        .get(&agent_id)
        .filter(|_| AgentViewer::new(&state, caller).can_see(&agent_id))
        .ok_or_else(|| refused(StatusCode::NOT_FOUND, "no such agent"))?
        .clone();
    Ok(Json(
        crate::eval_grader::grade_answer(&service, &person, &request).await,
    ))
}

pub async fn know_how(
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
    Json(request): Json<PreviewRequest>,
) -> Result<Json<Preview>, Response> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let person = caller.identity_id.clone().ok_or_else(|| {
        refused(
            StatusCode::UNAUTHORIZED,
            "counting know-how needs a person's sign-in",
        )
    })?;
    if !caller.has_scope(crate::jwt::SCOPE_CHAT) {
        return Err(refused(StatusCode::FORBIDDEN, "the chat scope is required"));
    }
    // A config of the caller's choosing could name any agent as the one a
    // copy reads: only someone who may write an agent's config may send one.
    if request.config.is_some() && !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "only a system admin may count under a config of their own",
        ));
    }
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(rate_limited_response)?;
    if request.questions.len() > MAX_QUESTIONS
        || request
            .questions
            .iter()
            .any(|q| q.chars().count() > MAX_QUESTION_CHARS)
    {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            "too many questions, or one too long",
        ));
    }
    let own = request
        .config
        .as_deref()
        .map(workforce_common::settings::AgentWorkspaceConfig::parse)
        .transpose()
        .map_err(|e| {
            refused(
                StatusCode::BAD_REQUEST,
                &format!("invalid config.toml: {e}"),
            )
        })?;
    let agent_id = request
        .agent_id
        .clone()
        .unwrap_or_else(|| state.registry.default_id().to_string());
    let service = state
        .registry
        .get(&agent_id)
        .filter(|_| AgentViewer::new(&state, caller).can_see(&agent_id))
        .ok_or_else(|| refused(StatusCode::NOT_FOUND, "no such agent"))?
        .clone();
    crate::know_how::agent::preview(&service, &person, own, &request.questions)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::warn!(target: "know_how", agent = %agent_id, error = %e, "know-how preview failed");
            refused(StatusCode::INTERNAL_SERVER_ERROR, "know-how store error")
        })
}

//! Engagement mode management API endpoints (DW-425).
//!
//! - `GET /api/engagement/modes` — List per-agent channel/thread mode overrides.
//! - `PUT /api/engagement/modes` — Set a mode override for one agent.
//! - `DELETE /api/engagement/modes` — Remove an agent's mode override.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use tracing::info;
use workforce_common::engagement::EngagementScope;
use workforce_common::settings::ResponseMode;

use crate::gateway::GatewayState;

use super::errors::ApiError;

// ── Request bodies ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EngagementSetBody {
    pub agent_id: String,
    pub channel: String,
    pub thread_id: Option<String>,
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct EngagementDeleteBody {
    pub agent_id: String,
    pub channel: String,
    pub thread_id: Option<String>,
}

// ── Endpoints ─────────────────────────────────────────────────────────────

/// GET /api/engagement/modes — List per-agent channel/thread mode overrides.
pub async fn engagement_list_modes(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;

    let entries: Vec<serde_json::Value> = state
        .engagement_overrides
        .list(None)
        .await
        .iter()
        .map(|record| {
            serde_json::json!({
                "agent_id": record.agent_id,
                "channel": record.channel,
                "thread_id": record.thread,
                "mode": record.mode,
                "set_by": record.set_by,
                "set_at": record.set_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "count": entries.len(),
        "overrides": entries,
    })))
}

/// PUT /api/engagement/modes — Set a mode override for one agent.
pub async fn engagement_set_mode(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<EngagementSetBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !super::check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err(ApiError::unauthorized());
    }

    let mode = parse_response_mode(&body.mode).ok_or_else(|| {
        ApiError::bad_request(format!(
            "invalid mode '{}'. Valid modes: all, mention_only, quiet",
            body.mode
        ))
    })?;

    let scope = EngagementScope::new(&body.agent_id, &body.channel, body.thread_id.clone());
    state.engagement_overrides.set(scope, mode, "api").await;
    crate::engagement_tool::persist_overrides(state.vault.as_ref(), &state.engagement_overrides)
        .await;

    info!(
        agent_id = %body.agent_id,
        channel = %body.channel,
        mode = %body.mode,
        "Engagement mode set via API"
    );
    Ok(Json(serde_json::json!({
        "agent_id": body.agent_id,
        "channel": body.channel,
        "thread_id": body.thread_id,
        "mode": body.mode,
    })))
}

/// DELETE /api/engagement/modes — Remove an agent's mode override.
pub async fn engagement_delete_mode(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<EngagementDeleteBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !super::check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err(ApiError::unauthorized());
    }

    let scope = EngagementScope::new(&body.agent_id, &body.channel, body.thread_id.clone());
    if state.engagement_overrides.remove(&scope).await {
        crate::engagement_tool::persist_overrides(
            state.vault.as_ref(),
            &state.engagement_overrides,
        )
        .await;
        info!(
            agent_id = %body.agent_id,
            channel = %body.channel,
            "Engagement mode removed via API"
        );
        Ok(Json(serde_json::json!({
            "removed": {
                "agent_id": body.agent_id,
                "channel": body.channel,
                "thread_id": body.thread_id,
            }
        })))
    } else {
        Err(ApiError::not_found(format!(
            "no mode override for agent '{}' in '{}'",
            body.agent_id, body.channel
        )))
    }
}

/// Parse a response mode string into the enum variant.
fn parse_response_mode(s: &str) -> Option<ResponseMode> {
    match s.to_lowercase().as_str() {
        "all" => Some(ResponseMode::All),
        "mention_only" => Some(ResponseMode::MentionOnly),
        "quiet" => Some(ResponseMode::Quiet),
        _ => None,
    }
}

//! Per-user relationship API — `GET /api/me/relationship`.
//!
//! Returns the authenticated user's own working-relationship summary. Identity
//! comes from the JWT, never from input, so one user can never read another's
//! relationship. Optional `agent` scopes to a single agent; `days` sets the
//! window.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use crate::gateway::GatewayState;
use crate::relationship::summarise;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

const DEFAULT_WINDOW_DAYS: i64 = 90;
const MAX_WINDOW_DAYS: i64 = 365;

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

#[derive(Deserialize)]
pub struct RelationshipQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    agent: Option<String>,
}

pub async fn my_relationship(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<RelationshipQuery>,
) -> ApiResult<serde_json::Value> {
    let identity = super::identity::require_identity(&state, &headers)?;
    let telemetry = state.telemetry_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "telemetry store unavailable",
        )
    })?;

    let days = q
        .days
        .unwrap_or(DEFAULT_WINDOW_DAYS)
        .clamp(1, MAX_WINDOW_DAYS);
    let agent = q.agent.filter(|a| !a.is_empty() && a != "all");
    let now = chrono::Utc::now();

    let events = telemetry
        .relationship_events_in_range(
            agent.as_deref(),
            Some(&identity),
            now - chrono::Duration::days(days),
            now,
        )
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("loading relationship: {e}"),
            )
        })?;

    let summary = summarise(&events, now, days);
    serde_json::to_value(&summary).map(Json).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("serialising summary: {e}"),
        )
    })
}

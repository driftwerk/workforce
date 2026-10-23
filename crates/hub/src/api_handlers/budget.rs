//! Budget status + runtime-override endpoints backing the admin UI's Budget
//! page. Reads gate like the stats routes (SystemAdmin/Manager); mutations
//! gate like config-overlay writes (`require_admin_action`: owner token +
//! rate limit) and are audited by the override store itself.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::identity::require_admin_or_manager;
use crate::budget_overrides::{BudgetOverride, OverrideAuditEntry, OverrideScope};
use crate::cost_governor::BudgetStatus;
use crate::gateway::GatewayState;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg.into()})))
}

#[derive(Serialize)]
pub struct BudgetStatusResponse {
    #[serde(flatten)]
    pub status: BudgetStatus,
    /// Identity uuid → display name, for the identities table.
    pub identity_names: std::collections::HashMap<String, String>,
    pub audit: Vec<OverrideAuditEntry>,
}

pub async fn get_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<BudgetStatusResponse> {
    require_admin_or_manager(&state, &headers)?;
    let service = state
        .registry
        .all_services()
        .into_iter()
        .next()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no agents registered"))?;
    let agent_ids: Vec<String> = state
        .registry
        .agent_ids()
        .iter()
        .map(|a| a.to_string())
        .collect();
    let status = service
        .cost_governor
        .status(&agent_ids, None, true)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<(String, f64)> = status
        .identities
        .iter()
        .map(|i| (i.key.clone(), i.spent_usd))
        .collect();
    let identity_names =
        super::identity_display::hydrate_identity_rows(state.identity_store.as_ref(), rows)
            .into_iter()
            .map(|(p, _)| (p.id.clone(), p.display_name))
            .collect();

    let audit = crate::budget_overrides::global_store()
        .map(|s| {
            s.audit(50).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "budget override audit read failed");
                Vec::new()
            })
        })
        .unwrap_or_default();

    Ok(Json(BudgetStatusResponse {
        status,
        identity_names,
        audit,
    }))
}

#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Deserialize)]
pub struct PutOverrideBody {
    pub scope: String,
    #[serde(default)]
    pub key: String,
    pub daily_usd: f64,
    /// "today", a number of hours, or absent for no expiry — the same
    /// semantics as the `budget` tool.
    #[serde(default)]
    pub expires: Option<serde_json::Value>,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn put_override(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(body): Json<PutOverrideBody>,
) -> ApiResult<serde_json::Value> {
    let caller =
        crate::gateway::require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let Some(store) = crate::budget_overrides::global_store() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no budget override store configured",
        ));
    };
    let scope = OverrideScope::parse(&body.scope).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "scope must be hub, agent or identity",
        )
    })?;
    if scope != OverrideScope::Hub && body.key.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "key is required for this scope",
        ));
    }
    if !body.daily_usd.is_finite() || body.daily_usd < 0.0 {
        return Err(err(StatusCode::BAD_REQUEST, "daily_usd must be >= 0"));
    }
    let service = state
        .registry
        .all_services()
        .into_iter()
        .next()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no agents registered"))?;
    if scope == OverrideScope::Identity
        && service
            .cost_governor
            .config()
            .exempt_identities
            .contains(&body.key)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "identity is exempt from budgets — no override needed",
        ));
    }
    let expires_at = crate::budget_tool::parse_expiry(body.expires.as_ref(), &service.config)
        .map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    let row = BudgetOverride {
        scope,
        key: if scope == OverrideScope::Hub {
            String::new()
        } else {
            body.key
        },
        daily_usd: body.daily_usd,
        expires_at,
        note: body.note,
        created_by: caller
            .identity_id
            .unwrap_or_else(|| "owner-token".to_string()),
        updated_at: Utc::now(),
    };
    store
        .upsert(&row)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"saved": true})))
}

#[derive(Deserialize)]
pub struct ClearOverrideBody {
    pub scope: String,
    #[serde(default)]
    pub key: String,
}

pub async fn clear_override(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(body): Json<ClearOverrideBody>,
) -> ApiResult<serde_json::Value> {
    let caller =
        crate::gateway::require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let Some(store) = crate::budget_overrides::global_store() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no budget override store configured",
        ));
    };
    let scope = OverrideScope::parse(&body.scope).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "scope must be hub, agent or identity",
        )
    })?;
    let actor = caller
        .identity_id
        .unwrap_or_else(|| "owner-token".to_string());
    let removed = store
        .remove(scope, &body.key, &actor)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"removed": removed})))
}

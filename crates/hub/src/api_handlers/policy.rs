//! Policy management API endpoints (DW-425).
//!
//! - `GET /api/policy/rules` — List all policy rules.
//! - `POST /api/policy/rules` — Add a mutable policy rule.
//! - `PUT /api/policy/rules/{name}` — Update a policy rule.
//! - `DELETE /api/policy/rules/{name}` — Remove a mutable policy rule.
//! - `GET /api/policy/proposals` — List policy proposals.
//! - `POST /api/policy/proposals` — Create a new policy proposal.
//! - `POST /api/policy/proposals/{id}/approve` — Approve a proposal.
//! - `POST /api/policy/proposals/{id}/reject` — Reject a proposal.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use tracing::info;

use crate::gateway::GatewayState;

use super::errors::ApiError;

// ── Query / Body types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ProposalQuery {
    pub token: Option<String>,
    pub status: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ProposalCreateBody {
    pub agent_id: Option<String>,
    pub proposed_rule: String,
    pub justification: String,
}

#[derive(Debug, Deserialize)]
pub struct ProposalReviewBody {
    pub reviewed_by: Option<String>,
    pub note: Option<String>,
}

// ── Rules endpoints ───────────────────────────────────────────────────────

/// GET /api/policy/rules — List all policy rules.
pub async fn policy_list_rules(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let rules = service.policy.list_all_rules();

    let rules_json: Vec<serde_json::Value> = rules
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "scope": r.scope,
                "tool": r.tool,
                "action": r.action,
                "priority": r.priority,
                "constitutional": r.constitutional,
                "conditions": r.conditions,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "rules": rules_json,
        "total": rules.len(),
        "constitutional": service.policy.constitutional_count(),
    })))
}

/// POST /api/policy/rules — Add a mutable policy rule.
pub async fn policy_create_rule(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    let rule: workforce_policy::rules::PolicyRule = serde_json::from_value(body).map_err(|e| {
        ApiError::bad_request("invalid policy rule format").with_detail(e.to_string())
    })?;

    let name = rule.name.clone();
    service.policy.add_rules(vec![rule]);

    info!(rule = %name, "Policy rule added via API");
    Ok(Json(serde_json::json!({
        "added": name,
        "total_rules": service.policy.rule_count(),
    })))
}

/// PUT /api/policy/rules/{name} — Update a policy rule (remove old + add new).
pub async fn policy_update_rule(
    Path(name): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    // Remove old rule first — returns false if it was constitutional or didn't exist
    if !service.policy.remove_mutable_rule(&name) {
        return Err(ApiError::not_found(format!(
            "rule '{}' not found or is constitutional",
            name
        )));
    }

    let mut rule: workforce_policy::rules::PolicyRule =
        serde_json::from_value(body).map_err(|e| {
            ApiError::bad_request("invalid policy rule format").with_detail(e.to_string())
        })?;
    rule.name = name.clone();
    service.policy.add_rules(vec![rule]);

    info!(rule = %name, "Policy rule updated via API");
    Ok(Json(serde_json::json!({ "updated": name })))
}

/// DELETE /api/policy/rules/{name} — Remove a mutable policy rule.
pub async fn policy_delete_rule(
    Path(name): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    if service.policy.remove_mutable_rule(&name) {
        info!(rule = %name, "Policy rule deleted via API");
        Ok(Json(serde_json::json!({ "deleted": name })))
    } else {
        Err(ApiError::not_found(format!(
            "rule '{}' not found or is constitutional",
            name
        )))
    }
}

// ── Proposals endpoints ───────────────────────────────────────────────────

/// GET /api/policy/proposals — List policy proposals.
pub async fn policy_list_proposals(
    Query(query): Query<ProposalQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let limit = query.limit.unwrap_or(50).min(200);

    let proposals = service
        .telemetry_store
        .list_policy_proposals(query.status.as_deref(), limit)
        .map_err(|e| ApiError::internal("failed to query proposals").with_detail(e.to_string()))?;

    let proposals_json: Vec<serde_json::Value> = proposals
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "agent_id": p.agent_id,
                "proposed_rule": p.proposed_rule,
                "justification": p.justification,
                "status": p.status,
                "reviewed_by": p.reviewed_by,
                "review_note": p.review_note,
                "created_at": p.created_at,
                "reviewed_at": p.reviewed_at,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "proposals": proposals_json,
        "count": proposals.len(),
    })))
}

/// POST /api/policy/proposals — Create a new policy proposal.
pub async fn policy_create_proposal(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ProposalCreateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    let id = service
        .telemetry_store
        .create_policy_proposal(
            &body.agent_id.unwrap_or_else(|| "api".to_string()),
            &body.proposed_rule,
            &body.justification,
        )
        .map_err(|e| ApiError::internal("failed to create proposal").with_detail(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": id,
        "status": "pending",
    })))
}

/// POST /api/policy/proposals/{id}/approve — Approve a proposal.
pub async fn policy_approve_proposal(
    Path(id): Path<i64>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ProposalReviewBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    // Verify proposal exists and is still pending
    let proposal = service
        .telemetry_store
        .get_policy_proposal(id)
        .map_err(|e| ApiError::internal("failed to fetch proposal").with_detail(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("proposal {} not found", id)))?;

    if proposal.status != "pending" {
        return Err(ApiError::conflict(format!(
            "proposal {} is already '{}'",
            id, proposal.status
        )));
    }

    service
        .telemetry_store
        .review_policy_proposal(
            id,
            "approved",
            &body.reviewed_by.unwrap_or_else(|| "owner".to_string()),
            body.note.as_deref(),
        )
        .map_err(|e| ApiError::internal("failed to approve proposal").with_detail(e.to_string()))?;

    // Apply the rule to the policy engine — both the legacy in-memory
    // PolicyEngine (so existing eval paths keep matching) and the new
    // permission_rules store (so the cascade evaluator picks it up too).
    let mut materialised = None;
    if let Ok(rule) =
        serde_json::from_str::<workforce_policy::rules::PolicyRule>(&proposal.proposed_rule)
    {
        service.policy.add_rules(vec![rule.clone()]);

        if let Some(evaluator) = state.permission_evaluator.as_ref() {
            if let Some(permission_rule) =
                workforce_permissions::migration::translate_proposal(&rule, None)
            {
                match evaluator.store().upsert_rule(&permission_rule) {
                    Ok(()) => {
                        materialised = Some(permission_rule.id.to_string());
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            proposal_id = id,
                            "Failed to materialise approved proposal into permission_rules"
                        );
                    }
                }
            }
        }
    }

    info!(proposal_id = id, "Policy proposal approved via API");
    Ok(Json(serde_json::json!({
        "approved": id,
        "permission_rule_id": materialised,
    })))
}

/// POST /api/policy/proposals/{id}/reject — Reject a proposal.
pub async fn policy_reject_proposal(
    Path(id): Path<i64>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ProposalReviewBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;

    let proposal = service
        .telemetry_store
        .get_policy_proposal(id)
        .map_err(|e| ApiError::internal("failed to fetch proposal").with_detail(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("proposal {} not found", id)))?;

    if proposal.status != "pending" {
        return Err(ApiError::conflict(format!(
            "proposal {} is already '{}'",
            id, proposal.status
        )));
    }

    service
        .telemetry_store
        .review_policy_proposal(
            id,
            "rejected",
            &body.reviewed_by.unwrap_or_else(|| "owner".to_string()),
            body.note.as_deref(),
        )
        .map_err(|e| ApiError::internal("failed to reject proposal").with_detail(e.to_string()))?;

    info!(proposal_id = id, "Policy proposal rejected via API");
    Ok(Json(serde_json::json!({ "rejected": id })))
}

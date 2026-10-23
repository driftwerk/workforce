//! Permission cascade API endpoints (phases 5 + 6 + 8).
//!
//! Reads: `GET /api/permissions/{rules,cascade,roles}` and
//! `GET /api/permissions/identities/{identity_id}/roles`.
//!
//! Writes: `POST/PUT/DELETE /api/permissions/rules[/{id}]`,
//! `POST/PUT/DELETE /api/permissions/roles[/{id}]`,
//! `POST/DELETE /api/permissions/roles/{role_id}/rules[/{rule_id}]`,
//! `POST/DELETE /api/permissions/identities/{identity_id}/roles[/{role_id}]`.
//!
//! Reads enforce [`crate::authz::Capability::PermissionsManage`] via
//! [`crate::gateway::require_capability_read`]; writes go through
//! [`crate::gateway::require_capability_action`] (the same capability plus the
//! per-identity rate limit), with role↔identity assign/unassign gated on
//! [`crate::authz::Capability::RolesAssign`] — one matrix, live status. The
//! legal-targets guard (phase-2) for `User`-scope rules still applies. All
//! endpoints return
//! `503 Service Unavailable` when the operator hasn't opted into the
//! cascade (`[permissions] enabled = false`). Decisions and writes
//! emit `AuditOperation::PermissionDecision` /
//! `PermissionRuleWrite` to the shared access-audit sink.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use workforce_memory::access_audit::{self, AccessAuditEvent, AuditDecision, AuditOperation};
use workforce_permissions::cache::PermissionEvaluator;
use workforce_permissions::cascade::{self, CascadeRequest};
use workforce_permissions::store::PermissionStore;
use workforce_permissions::types::{
    ApproverRef, PermissionFilter, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef,
    PermissionRule, PermissionScope, PermissionState,
};
use workforce_permissions::{CapabilityStore, CustomRole};

use crate::gateway::GatewayState;
use crate::permissions_audit::emit_decision;

use super::errors::ApiError;

// ── Query / body types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RulesQuery {
    pub token: Option<String>,
    pub scope_type: Option<String>,
    pub scope_id: Option<String>,
    pub tool_pattern: Option<String>,
    pub origin: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CascadeQuery {
    pub token: Option<String>,
    pub user_id: String,
    pub agent_id: String,
    pub tool_name: String,
    /// Comma-separated team IDs the user belongs to relative to this
    /// agent. When omitted the cascade evaluator pulls no Team-scope
    /// rules — useful for "what would this look like ignoring teams?".
    pub team_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RolesQuery {
    pub token: Option<String>,
    pub team_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdentityRolesPath {
    pub token: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────

/// GET /api/permissions/rules — list rules with optional filters.
pub async fn list_rules(
    Query(query): Query<RulesQuery>,
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

    let store = permission_store(&state)?;
    let filter = PermissionFilter {
        scope: query
            .scope_type
            .as_deref()
            .map(PermissionScope::from_str_lossy),
        scopes: None,
        scope_id: query.scope_id.clone(),
        tool_pattern: query.tool_pattern.clone(),
        origin: query
            .origin
            .as_deref()
            .map(PermissionOrigin::from_str_lossy),
        limit: query.limit.map(|n| n.min(500)),
    };
    let rules = store
        .list_rules(&filter)
        .map_err(|e| ApiError::internal("failed to list rules").with_detail(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "rules": rules.iter().map(rule_to_json).collect::<Vec<_>>(),
        "count": rules.len(),
        "generation": store.current_generation().unwrap_or(0),
    })))
}

/// GET /api/permissions/cascade — show the effective state and which
/// rules contributed at each level for a `(user, agent, tool)` triple.
pub async fn cascade(
    Query(query): Query<CascadeQuery>,
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

    let evaluator = permission_evaluator(&state)?;
    let team_ids: Vec<String> = query
        .team_ids
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let request = CascadeRequest {
        user_id: &query.user_id,
        agent_id: &query.agent_id,
        team_ids: &team_ids,
        tool_name: &query.tool_name,
    };

    let effective = evaluator
        .evaluate(&request)
        .map_err(|e| ApiError::internal("cascade evaluation failed").with_detail(e.to_string()))?;
    emit_decision(&query.user_id, &query.tool_name, effective);

    // Load the same rules the evaluator did, then split by scope so
    // the UI can show which level contributed what.
    let store = evaluator.store();
    let mut scopes: Vec<(PermissionScope, &str)> = Vec::with_capacity(team_ids.len() + 2);
    for t in &team_ids {
        scopes.push((PermissionScope::Team, t.as_str()));
    }
    scopes.push((PermissionScope::Agent, query.agent_id.as_str()));
    scopes.push((PermissionScope::User, query.user_id.as_str()));
    let rules = store
        .rules_for_scopes(&scopes)
        .map_err(|e| ApiError::internal("loading scope rules failed").with_detail(e.to_string()))?;

    let rules_by_scope = group_rules_by_scope(&rules, &query.tool_name);

    Ok(Json(serde_json::json!({
        "request": {
            "user_id": query.user_id,
            "agent_id": query.agent_id,
            "team_ids": team_ids,
            "tool_name": query.tool_name,
        },
        "effective": effective.as_str(),
        "rules_by_scope": rules_by_scope,
    })))
}

/// GET /api/permissions/roles?team_id=<uuid> — list roles. Requires
/// `team_id` because roles are team-scoped in the data model.
pub async fn list_roles(
    Query(query): Query<RolesQuery>,
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

    let store = permission_store(&state)?;
    let team_id_str = query
        .team_id
        .ok_or_else(|| ApiError::bad_request("team_id query parameter is required"))?;
    let team_id = Uuid::from_str(&team_id_str)
        .map_err(|_| ApiError::bad_request("team_id must be a UUID"))?;

    let roles = store
        .list_roles_for_team(team_id)
        .map_err(|e| ApiError::internal("failed to list roles").with_detail(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "roles": roles.iter().map(role_to_json).collect::<Vec<_>>(),
        "count": roles.len(),
    })))
}

/// GET /api/permissions/identities/{identity_id}/roles — return every
/// role the identity holds plus the rules each role grants.
pub async fn list_identity_roles(
    Path(identity_id): Path<String>,
    Query(query): Query<IdentityRolesPath>,
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

    let store = permission_store(&state)?;
    let uid = Uuid::from_str(&identity_id)
        .map_err(|_| ApiError::bad_request("identity_id must be a UUID"))?;
    let roles = store.roles_for_identity(uid).map_err(|e| {
        ApiError::internal("loading identity roles failed").with_detail(e.to_string())
    })?;

    let mut role_payloads = Vec::with_capacity(roles.len());
    for role in &roles {
        let rules = store.rules_for_role(role.id).map_err(|e| {
            ApiError::internal("loading rules for role failed").with_detail(e.to_string())
        })?;
        role_payloads.push(serde_json::json!({
            "role": role_to_json(role),
            "rules": rules.iter().map(rule_to_json).collect::<Vec<_>>(),
        }));
    }

    Ok(Json(serde_json::json!({
        "identity_id": identity_id,
        "roles": role_payloads,
    })))
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub token: Option<String>,
    /// Only return events emitted by these operations (comma-separated).
    /// Defaults to the cascade-related ones — `permission_decision` and
    /// `permission_rule_write` — so the page isn't drowned in memory
    /// recalls. Pass `*` (or an empty value) to disable the filter.
    pub operations: Option<String>,
    /// Maximum rows to return. Clamped to the ring buffer's capacity
    /// (5000). Defaults to 200.
    pub limit: Option<usize>,
    /// Filter to events whose requester matches this id. Optional.
    pub user_id: Option<String>,
    /// Filter to events whose resource_id contains this substring.
    /// Optional. For permission events resource_id is the tool name or
    /// rule UUID.
    pub resource: Option<String>,
}

/// GET /api/permissions/audit — recent audit events from the
/// in-process ring buffer. Returns the cascade-related operations by
/// default; pass `?operations=*` to widen.
pub async fn list_audit(
    Query(query): Query<AuditQuery>,
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

    let buffer =
        workforce_memory::access_audit::ring_buffer().ok_or_else(ApiError::service_unavailable)?;

    let want_all_ops = matches!(query.operations.as_deref(), Some("*"));
    let op_filter: Option<std::collections::HashSet<String>> = if want_all_ops {
        None
    } else {
        let raw = query
            .operations
            .as_deref()
            .unwrap_or("permission_decision,permission_rule_write");
        Some(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    };
    let limit = query.limit.unwrap_or(200).min(5_000);
    let user_id = query
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let resource = query
        .resource
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let events: Vec<serde_json::Value> = buffer
        .snapshot()
        .into_iter()
        .filter(|e| {
            op_filter
                .as_ref()
                .is_none_or(|set| set.contains(e.operation.as_str()))
        })
        .filter(|e| user_id.is_none_or(|u| e.requester_identity_id.as_deref() == Some(u)))
        .filter(|e| {
            resource.is_none_or(|r| {
                e.resource_id
                    .as_deref()
                    .map(|rid| rid.contains(r))
                    .unwrap_or(false)
            })
        })
        .take(limit)
        .map(|e| {
            serde_json::json!({
                "timestamp": e.timestamp,
                "operation": e.operation.as_str(),
                "decision": e.decision.as_str(),
                "user_id": e.requester_identity_id,
                "owner_or_state": e.owner_identity_id,
                "resource": e.resource_id,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "events": events,
        "count": events.len(),
        "buffer_size": buffer.len(),
    })))
}

/// GET /api/capabilities/matrix — the role→capability matrix the admin UI
/// renders for its "who can do what" view, plus the active system defaults.
///
/// `roles` reuses [`crate::authz::capability_summary`] (the same serialized
/// shape `identity.whoami` / `GET /api/me/capabilities` return) for the two
/// live roles, Member and SystemAdmin, both scored at `Active` status.
/// `system_defaults` reflects the live `@destructive → Ask` baseline: present
/// only when [`GatewayState::destructive_baseline_active`] is set — i.e.
/// `[permissions] enabled && destructive_baseline` — captured once at hub
/// construction from `wf_config`.
///
/// Gated on [`crate::authz::Capability::PermissionsManage`] via
/// [`crate::gateway::require_capability_read`], the same admin-only read guard
/// the rest of `/api/permissions/*` uses.
pub async fn capabilities_matrix(
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

    let matrix = crate::authz::capability_matrix();
    let custom_roles = list_custom_roles_or_empty();
    let roles = role_rows(&custom_roles, &matrix);

    let mut system_defaults = Vec::new();
    if state.destructive_baseline_active {
        system_defaults.push(serde_json::json!({
            "pattern": workforce_policy::rules::DESTRUCTIVE_PATTERN,
            "state": PermissionState::Ask.as_str(),
            "description":
                "Confirm destructive actions (delete/remove/…) across all tools",
        }));
    }

    Ok(Json(serde_json::json!({
        "roles": roles,
        "protected_capabilities": protected_capability_keys(),
        "system_defaults": system_defaults,
    })))
}

/// One cell edit in a [`CapabilityMatrixUpdateBody`].
#[derive(Deserialize)]
pub struct CapabilityCellChange {
    pub role: String,
    pub capability: String,
    pub decision: String,
}

/// Body for `PUT /api/capabilities/matrix`.
#[derive(Deserialize)]
pub struct CapabilityMatrixUpdateBody {
    pub changes: Vec<CapabilityCellChange>,
}

/// PUT /api/capabilities/matrix — persist admin overrides to the capability
/// matrix and swap the live snapshot. A cell whose decision equals the built-in
/// default clears the override (the set stays minimal). Takes effect immediately
/// on this replica; the durable audit row is written atomically by the store.
pub async fn save_capabilities_matrix(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CapabilityMatrixUpdateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = crate::authz::capability_store()
        .ok_or_else(|| ApiError::internal("capability store not configured"))?;

    let valid_roles = known_role_keys(store.as_ref())?;
    let changes = body
        .changes
        .iter()
        .map(|c| {
            if !valid_roles.contains(&c.role) {
                return Err(ApiError::bad_request(format!("unknown role '{}'", c.role)));
            }
            let cap = crate::authz::Capability::from_key(&c.capability).ok_or_else(|| {
                ApiError::bad_request(format!("unknown capability '{}'", c.capability))
            })?;
            if crate::authz::is_protected_admin_cap(cap) {
                return Err(ApiError::bad_request(format!(
                    "capability '{}' is reserved for system_admin and cannot be overridden",
                    c.capability
                )));
            }
            let decision = parse_capability_decision(&c.decision)?;
            let stored = (decision != crate::authz::baseline_for(&c.role, cap))
                .then(|| crate::authz::decision_to_stored(decision));
            Ok(workforce_permissions::CapabilityChange {
                role: c.role.clone(),
                capability: c.capability.clone(),
                decision: stored,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    let generation = store
        .apply_changes(&changes, caller_uuid(&caller))
        .map_err(|e| {
            ApiError::internal("failed to save capability matrix").with_detail(e.to_string())
        })?;

    let overrides = store.list_overrides().map_err(|e| {
        ApiError::internal("failed to reload capability overrides").with_detail(e.to_string())
    })?;
    crate::authz::set_capability_matrix(crate::authz::CapabilityMatrix::from_overrides(&overrides));

    let matrix = crate::authz::capability_matrix();
    let custom_roles = list_custom_roles_or_empty();
    let roles = role_rows(&custom_roles, &matrix);
    Ok(Json(serde_json::json!({
        "roles": roles,
        "generation": generation,
    })))
}

/// GET /api/capabilities/audit — the immutable capability-override change log,
/// newest first. Admin-only, same guard as the matrix view.
pub async fn capabilities_audit(
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

    let store = crate::authz::capability_store()
        .ok_or_else(|| ApiError::internal("capability store not configured"))?;
    let entries = store
        .list_audit(200)
        .map_err(|e| {
            ApiError::internal("failed to read capability audit").with_detail(e.to_string())
        })?
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "at": e.at.to_rfc3339(),
                "actor_id": e.actor_id.map(|u| u.to_string()),
                "role": e.role,
                "capability": e.capability,
                "old_decision": e.old_decision.map(|d| d.as_str()),
                "new_decision": e.new_decision.map(|d| d.as_str()),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({ "entries": entries })))
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn parse_capability_decision(s: &str) -> Result<crate::authz::Decision, ApiError> {
    match s {
        "allow" => Ok(crate::authz::Decision::Allow),
        "ask" => Ok(crate::authz::Decision::Ask),
        "deny" => Ok(crate::authz::Decision::Deny),
        other => Err(ApiError::bad_request(format!("unknown decision '{other}'"))),
    }
}

fn permission_store(state: &GatewayState) -> Result<Arc<dyn PermissionStore>, ApiError> {
    Ok(Arc::clone(permission_evaluator(state)?.store()))
}

fn permission_evaluator(state: &GatewayState) -> Result<Arc<PermissionEvaluator>, ApiError> {
    state.permission_evaluator.clone().ok_or_else(|| {
        ApiError::service_unavailable()
            .with_detail("permissions cascade is disabled — set [permissions] enabled = true")
    })
}

fn rule_to_json(rule: &PermissionRule) -> serde_json::Value {
    serde_json::json!({
        "id": rule.id,
        "scope": rule.scope.as_str(),
        "scope_id": rule.scope_id,
        "tool_pattern": rule.tool_pattern,
        "state": rule.state.as_str(),
        "conditions": rule.conditions,
        "approvers": rule.approvers,
        "allow_self_approval": rule.allow_self_approval,
        "origin": rule.origin.as_str(),
        "created_by": rule.created_by,
        "created_at": rule.created_at,
        "updated_at": rule.updated_at,
    })
}

fn role_to_json(role: &PermissionRoleDef) -> serde_json::Value {
    serde_json::json!({
        "id": role.id,
        "team_id": role.team_id,
        "name": role.name,
        "description": role.description,
        "created_at": role.created_at,
    })
}

/// Group rules by their hierarchy level for the cascade response.
/// Within each level the rules are ordered by tool-pattern specificity
/// then by `updated_at` so the UI can show "which rule won".
fn group_rules_by_scope(rules: &[PermissionRule], tool_name: &str) -> serde_json::Value {
    let matches = |r: &&PermissionRule| {
        workforce_permissions::cascade::tool_matches(&r.tool_pattern, tool_name)
    };

    let pick = |scope: PermissionScope| -> Vec<serde_json::Value> {
        let mut filtered: Vec<&PermissionRule> = rules
            .iter()
            .filter(|r| r.scope == scope)
            .filter(matches)
            .collect();
        filtered.sort_by(|a, b| {
            let sa = workforce_permissions::cascade::pattern_specificity(&a.tool_pattern);
            let sb = workforce_permissions::cascade::pattern_specificity(&b.tool_pattern);
            sb.cmp(&sa).then(b.updated_at.cmp(&a.updated_at))
        });
        filtered.iter().map(|r| rule_to_json(r)).collect()
    };

    let mut admin_state: Option<PermissionState> = None;
    let mut admin_states_per_scope = serde_json::Map::new();
    for scope in [
        PermissionScope::Team,
        PermissionScope::Agent,
        PermissionScope::User,
    ] {
        let level = pick(scope);
        let level_state = level
            .iter()
            .filter_map(|v| v.get("state").and_then(|s| s.as_str()))
            .map(PermissionState::from_str_lossy)
            .reduce(PermissionState::most_restrictive);
        if scope != PermissionScope::User {
            admin_state = match (admin_state, level_state) {
                (None, x) => x,
                (Some(prev), None) => Some(prev),
                (Some(prev), Some(next)) => Some(PermissionState::most_restrictive(prev, next)),
            };
        }
        admin_states_per_scope.insert(
            scope.as_str().to_string(),
            serde_json::json!({
                "rules": level,
                "state": level_state.map(|s| s.as_str()),
            }),
        );
    }

    serde_json::json!({
        "team": admin_states_per_scope.get("team"),
        "agent": admin_states_per_scope.get("agent"),
        "user": admin_states_per_scope.get("user"),
        "admin_state": admin_state.map(|s| s.as_str()),
    })
}

// ── Write endpoints (phase 6) ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpsertRuleBody {
    pub scope: String,
    pub scope_id: String,
    pub tool_pattern: String,
    pub state: String,
    #[serde(default)]
    pub conditions: Option<serde_json::Value>,
    /// Designated approvers for routed approval (users + roles). Only
    /// meaningful on Ask/AlwaysAsk rules; empty ⇒ self-serve.
    #[serde(default)]
    pub approvers: Option<Vec<ApproverRef>>,
    /// Whether the requester may satisfy their own approval. Default true.
    #[serde(default)]
    pub allow_self_approval: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRuleBody {
    #[serde(default)]
    pub tool_pattern: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub conditions: Option<serde_json::Value>,
    #[serde(default)]
    pub approvers: Option<Vec<ApproverRef>>,
    #[serde(default)]
    pub allow_self_approval: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleBody {
    pub team_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRoleBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// POST /api/permissions/rules — create a new permission rule. Rules
/// keyed by `(scope_type, scope_id, tool_pattern)` upsert through the
/// store, so calling this with an existing triple replaces the row.
pub async fn create_rule(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpsertRuleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let scope = parse_scope(&body.scope)?;
    let target_state = parse_state(&body.state)?;
    validate_pattern(&body.tool_pattern)?;
    if body.scope_id.trim().is_empty() {
        return Err(ApiError::bad_request("scope_id must not be empty"));
    }

    if scope == PermissionScope::User {
        validate_user_scope_target(&*store, &body.scope_id, &body.tool_pattern, target_state)?;
    }

    let creator_uuid = caller_uuid(&caller);
    let mut rule = PermissionRule::new(
        scope,
        body.scope_id.trim(),
        body.tool_pattern.trim(),
        target_state,
        PermissionOrigin::Admin,
    );
    if let Some(conditions) = body.conditions {
        rule = rule.with_conditions(conditions);
    }
    if let Some(approvers) = body.approvers {
        validate_approvers(&approvers)?;
        rule = rule.with_approvers(approvers);
    }
    if let Some(allow_self) = body.allow_self_approval {
        rule = rule.with_allow_self_approval(allow_self);
    }
    if let Some(uid) = creator_uuid {
        rule = rule.with_created_by(uid);
    }

    store
        .upsert_rule(&rule)
        .map_err(|e| ApiError::internal("failed to write rule").with_detail(e.to_string()))?;
    audit_permission_rule_write("create", &rule, caller.identity_id.as_deref());

    Ok(Json(serde_json::json!({
        "rule": rule_to_json(&rule),
        "generation": store.current_generation().unwrap_or(0),
    })))
}

/// PUT /api/permissions/rules/{id} — patch an existing rule. Pattern
/// changes go through `upsert_rule` (which preserves the row ID), so
/// the cache generation bump invalidates downstream evaluators.
pub async fn update_rule(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateRuleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let rule_id = Uuid::from_str(&id).map_err(|_| ApiError::bad_request("id must be a UUID"))?;
    let mut rule = store
        .get_rule(rule_id)
        .map_err(|e| ApiError::internal("failed to load rule").with_detail(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("rule {id} not found")))?;

    if let Some(pattern) = body.tool_pattern.as_deref() {
        validate_pattern(pattern)?;
        rule.tool_pattern = pattern.trim().to_string();
    }
    if let Some(s) = body.state.as_deref() {
        rule.state = parse_state(s)?;
    }
    if let Some(cond) = body.conditions {
        rule.conditions = if cond.is_null() { None } else { Some(cond) };
    }
    if let Some(approvers) = body.approvers {
        validate_approvers(&approvers)?;
        rule.approvers = approvers;
    }
    if let Some(allow_self) = body.allow_self_approval {
        rule.allow_self_approval = allow_self;
    }
    rule.updated_at = Utc::now();

    if rule.scope == PermissionScope::User {
        validate_user_scope_target(&*store, &rule.scope_id, &rule.tool_pattern, rule.state)?;
    }

    store
        .upsert_rule(&rule)
        .map_err(|e| ApiError::internal("failed to update rule").with_detail(e.to_string()))?;
    audit_permission_rule_write("update", &rule, caller.identity_id.as_deref());
    Ok(Json(serde_json::json!({
        "rule": rule_to_json(&rule),
        "generation": store.current_generation().unwrap_or(0),
    })))
}

/// DELETE /api/permissions/rules/{id} — remove a rule.
pub async fn delete_rule(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let rule_id = Uuid::from_str(&id).map_err(|_| ApiError::bad_request("id must be a UUID"))?;
    let removed = store
        .delete_rule(rule_id)
        .map_err(|e| ApiError::internal("failed to delete rule").with_detail(e.to_string()))?;
    if !removed {
        return Err(ApiError::not_found(format!("rule {id} not found")));
    }
    audit_permission_rule_deletion(&id, caller.identity_id.as_deref());
    Ok(Json(serde_json::json!({
        "deleted": id,
        "generation": store.current_generation().unwrap_or(0),
    })))
}

/// POST /api/permissions/roles — create a permission role attached to a team.
pub async fn create_role(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateRoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let team_id = Uuid::from_str(body.team_id.trim())
        .map_err(|_| ApiError::bad_request("team_id must be a UUID"))?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let role =
        PermissionRoleDef::new(team_id, body.name.trim()).with_description(body.description.trim());

    store
        .upsert_role(&role)
        .map_err(|e| ApiError::internal("failed to create role").with_detail(e.to_string()))?;
    Ok(Json(serde_json::json!({ "role": role_to_json(&role) })))
}

/// PUT /api/permissions/roles/{id} — patch a role's name / description.
pub async fn update_role(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateRoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let role_id = Uuid::from_str(&id).map_err(|_| ApiError::bad_request("id must be a UUID"))?;
    let mut role = store
        .get_role(role_id)
        .map_err(|e| ApiError::internal("failed to load role").with_detail(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("role {id} not found")))?;
    if let Some(name) = body.name {
        if name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        role.name = name.trim().to_string();
    }
    if let Some(desc) = body.description {
        role.description = desc.trim().to_string();
    }
    store
        .upsert_role(&role)
        .map_err(|e| ApiError::internal("failed to update role").with_detail(e.to_string()))?;
    Ok(Json(serde_json::json!({ "role": role_to_json(&role) })))
}

/// DELETE /api/permissions/roles/{id} — drop a role and every rule
/// grant + identity assignment it currently holds (CASCADE on the join
/// tables).
pub async fn delete_role(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;

    let store = permission_store(&state)?;
    let role_id = Uuid::from_str(&id).map_err(|_| ApiError::bad_request("id must be a UUID"))?;
    let removed = store
        .delete_role(role_id)
        .map_err(|e| ApiError::internal("failed to delete role").with_detail(e.to_string()))?;
    if !removed {
        return Err(ApiError::not_found(format!("role {id} not found")));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// GET /api/permissions/roles/{role_id}/rules — list every rule currently
/// granted to a role. Used by the admin UI's roles tab to render the
/// inline rule list under each role row.
pub async fn list_rules_for_role(
    Path(role_id): Path<String>,
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
    let store = permission_store(&state)?;
    let role_uuid =
        Uuid::from_str(&role_id).map_err(|_| ApiError::bad_request("role_id must be a UUID"))?;
    let rules = store.rules_for_role(role_uuid).map_err(|e| {
        ApiError::internal("loading rules for role failed").with_detail(e.to_string())
    })?;
    Ok(Json(serde_json::json!({
        "rules": rules.iter().map(rule_to_json).collect::<Vec<_>>(),
        "count": rules.len(),
    })))
}

/// POST /api/permissions/roles/{role_id}/rules — grant a rule to a role.
/// Body: `{ "rule_id": "<uuid>" }`.
pub async fn grant_rule_to_role(
    Path(role_id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let store = permission_store(&state)?;
    let role_uuid =
        Uuid::from_str(&role_id).map_err(|_| ApiError::bad_request("role_id must be a UUID"))?;
    let rule_id_str = body
        .get("rule_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("rule_id (UUID) is required"))?;
    let rule_uuid =
        Uuid::from_str(rule_id_str).map_err(|_| ApiError::bad_request("rule_id must be a UUID"))?;

    store
        .grant_rule_to_role(role_uuid, rule_uuid)
        .map_err(|e| ApiError::internal("failed to grant rule").with_detail(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "role_id": role_id,
        "rule_id": rule_id_str,
        "status": "granted",
    })))
}

/// DELETE /api/permissions/roles/{role_id}/rules/{rule_id} — revoke a grant.
pub async fn revoke_rule_from_role(
    Path((role_id, rule_id)): Path<(String, String)>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::PermissionsManage,
    )
    .await?;
    let store = permission_store(&state)?;
    let role_uuid =
        Uuid::from_str(&role_id).map_err(|_| ApiError::bad_request("role_id must be a UUID"))?;
    let rule_uuid =
        Uuid::from_str(&rule_id).map_err(|_| ApiError::bad_request("rule_id must be a UUID"))?;
    let removed = store
        .revoke_rule_from_role(role_uuid, rule_uuid)
        .map_err(|e| ApiError::internal("failed to revoke rule").with_detail(e.to_string()))?;
    if !removed {
        return Err(ApiError::not_found("grant not found"));
    }
    Ok(Json(serde_json::json!({ "revoked": rule_id })))
}

/// POST /api/permissions/identities/{identity_id}/roles — assign a role.
/// Body: `{ "role_id": "<uuid>" }`.
pub async fn assign_role_to_identity(
    Path(identity_id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let store = permission_store(&state)?;
    let identity_uuid = Uuid::from_str(&identity_id)
        .map_err(|_| ApiError::bad_request("identity_id must be a UUID"))?;
    let role_id_str = body
        .get("role_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("role_id (UUID) is required"))?;
    let role_uuid =
        Uuid::from_str(role_id_str).map_err(|_| ApiError::bad_request("role_id must be a UUID"))?;

    let assignment = PermissionRoleAssignment {
        identity_id: identity_uuid,
        role_id: role_uuid,
        granted_by: caller_uuid(&caller),
        granted_at: Utc::now(),
    };
    store
        .assign_role_to_identity(&assignment)
        .map_err(|e| ApiError::internal("failed to assign role").with_detail(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "identity_id": identity_id,
        "role_id": role_id_str,
        "status": "assigned",
    })))
}

/// DELETE /api/permissions/identities/{identity_id}/roles/{role_id} —
/// unassign a role from an identity.
pub async fn unassign_role_from_identity(
    Path((identity_id, role_id)): Path<(String, String)>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let store = permission_store(&state)?;
    let identity_uuid = Uuid::from_str(&identity_id)
        .map_err(|_| ApiError::bad_request("identity_id must be a UUID"))?;
    let role_uuid =
        Uuid::from_str(&role_id).map_err(|_| ApiError::bad_request("role_id must be a UUID"))?;
    let removed = store
        .unassign_role_from_identity(identity_uuid, role_uuid)
        .map_err(|e| ApiError::internal("failed to unassign role").with_detail(e.to_string()))?;
    if !removed {
        return Err(ApiError::not_found("assignment not found"));
    }
    Ok(Json(serde_json::json!({ "unassigned": role_id })))
}

// ── Write-side validation helpers ─────────────────────────────────────────

fn parse_scope(s: &str) -> Result<PermissionScope, ApiError> {
    match s {
        "team" => Ok(PermissionScope::Team),
        "agent" => Ok(PermissionScope::Agent),
        "user" => Ok(PermissionScope::User),
        other => Err(ApiError::bad_request(format!(
            "scope must be team / agent / user, got '{other}'"
        ))),
    }
}

fn parse_state(s: &str) -> Result<PermissionState, ApiError> {
    match s {
        "allow" => Ok(PermissionState::Allow),
        "ask" => Ok(PermissionState::Ask),
        "always_ask" => Ok(PermissionState::AlwaysAsk),
        "deny" => Ok(PermissionState::Deny),
        other => Err(ApiError::bad_request(format!(
            "state must be allow / ask / always_ask / deny, got '{other}'"
        ))),
    }
}

fn validate_pattern(pattern: &str) -> Result<(), ApiError> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("tool_pattern must not be empty"));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(ApiError::bad_request(
            "tool_pattern must not contain whitespace",
        ));
    }
    Ok(())
}

/// Validate routed-approval targets: a `User` ref's id must parse as a
/// UUID (an internal identity), and a `Role` ref must name a known role.
fn validate_approvers(approvers: &[ApproverRef]) -> Result<(), ApiError> {
    for approver in approvers {
        match approver {
            ApproverRef::User { id } => {
                Uuid::from_str(id).map_err(|_| {
                    ApiError::bad_request(format!("approver user id must be a UUID: '{id}'"))
                })?;
            }
            ApproverRef::Role { role } => {
                if !matches!(role.as_str(), "system_admin" | "manager" | "member") {
                    return Err(ApiError::bad_request(format!(
                        "approver role must be system_admin / manager / member, got '{role}'"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn caller_uuid(caller: &crate::gateway::CallerContext) -> Option<Uuid> {
    caller
        .identity_id
        .as_deref()
        .and_then(|s| Uuid::from_str(s).ok())
}

// ── Roles & capabilities (the always-on layer) ─────────────────────────────

/// The three built-in roles, in matrix order, with display metadata.
const BUILTIN_ROLES: [(&str, &str, &str); 3] = [
    (
        "member",
        "Member",
        "The default role — self-serves their own data; denied every shared and admin capability.",
    ),
    (
        "manager",
        "Manager",
        "A Member plus read-only operational visibility. Elevate further per-capability via the matrix.",
    ),
    (
        "system_admin",
        "System Admin",
        "Full administrative authority over the workspace.",
    ),
];

fn capability_store_or_err() -> Result<Arc<dyn CapabilityStore>, ApiError> {
    crate::authz::capability_store()
        .ok_or_else(|| ApiError::internal("capability store not configured"))
}

fn list_custom_roles_or_empty() -> Vec<CustomRole> {
    crate::authz::capability_store()
        .and_then(|s| s.list_custom_roles().ok())
        .unwrap_or_default()
}

fn protected_capability_keys() -> Vec<&'static str> {
    crate::authz::Capability::ALL
        .into_iter()
        .filter(|c| crate::authz::is_protected_admin_cap(*c))
        .map(|c| c.key())
        .collect()
}

/// The role keys the matrix / role assignment accept — built-ins ∪ custom.
fn known_role_keys(
    store: &dyn CapabilityStore,
) -> Result<std::collections::HashSet<String>, ApiError> {
    let mut keys: std::collections::HashSet<String> = BUILTIN_ROLES
        .iter()
        .map(|(k, _, _)| k.to_string())
        .collect();
    let custom = store.list_custom_roles().map_err(|e| {
        ApiError::internal("failed to list custom roles").with_detail(e.to_string())
    })?;
    keys.extend(custom.into_iter().map(|r| r.key));
    Ok(keys)
}

/// Lightweight role metadata (no capabilities) — feeds the user-page role picker.
fn role_meta_rows(custom_roles: &[CustomRole]) -> Vec<serde_json::Value> {
    BUILTIN_ROLES
        .iter()
        .map(|(key, label, desc)| {
            serde_json::json!({ "key": key, "label": label, "description": desc, "is_builtin": true })
        })
        .chain(custom_roles.iter().map(|r| {
            serde_json::json!({ "key": r.key, "label": r.name, "description": r.description, "is_builtin": false })
        }))
        .collect()
}

/// One matrix row per role (built-ins then custom), each scored at `Active`.
fn role_rows(
    custom_roles: &[CustomRole],
    matrix: &crate::authz::CapabilityMatrix,
) -> Vec<serde_json::Value> {
    use workforce_identity::types::IdentityStatus;
    let row = |key: &str, label: &str, description: &str, is_builtin: bool| {
        let summary = crate::authz::capability_summary(key, IdentityStatus::Active, matrix);
        serde_json::json!({
            "key": key,
            "label": label,
            "description": description,
            "is_builtin": is_builtin,
            "capabilities": summary.capabilities,
        })
    };
    BUILTIN_ROLES
        .iter()
        .map(|(key, label, desc)| row(key, label, desc, true))
        .chain(
            custom_roles
                .iter()
                .map(|r| row(&r.key, &r.name, &r.description, false)),
        )
        .collect()
}

/// Validate a proposed custom-role key: a lowercase slug that isn't a built-in.
fn validate_custom_role_key(key: &str) -> Result<(), ApiError> {
    let valid = !key.is_empty()
        && key.len() <= 48
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(ApiError::bad_request(
            "role key must be a lowercase slug (a–z, 0–9, _) starting with a letter",
        ));
    }
    if BUILTIN_ROLES.iter().any(|(k, _, _)| *k == key) {
        return Err(ApiError::bad_request(format!(
            "'{key}' is a built-in role and cannot be redefined"
        )));
    }
    Ok(())
}

/// GET /api/capabilities/catalog — the capability dictionary (label, plain
/// description, governed tools, protected flag) the admin tooltips and the My
/// Permissions page render. The single source of truth for "what does this do?".
pub async fn capabilities_catalog(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Capability metadata is non-sensitive (names, descriptions, governed tools)
    // and the member-facing My Permissions page reads it, so any authenticated
    // caller may — not just admins.
    let caller =
        crate::gateway::resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(ApiError::unauthorized());
    }

    let capabilities = crate::authz::Capability::ALL
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "key": c.key(),
                "label": c.label(),
                "description": c.description(),
                "category": c.category(),
                "tools": c.governed_tools(),
                "protected": crate::authz::is_protected_admin_cap(c),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({ "capabilities": capabilities })))
}

/// GET /api/roles — the assignable role catalogue: built-ins plus custom roles.
pub async fn list_all_roles(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let custom = list_custom_roles_or_empty();
    Ok(Json(
        serde_json::json!({ "roles": role_meta_rows(&custom) }),
    ))
}

/// Body for `POST /api/roles`.
#[derive(Deserialize)]
pub struct CreateCustomRoleBody {
    pub token: Option<String>,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// POST /api/roles — define a new team-free custom role.
pub async fn create_custom_role(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateCustomRoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        body.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let store = capability_store_or_err()?;

    let key = body.key.trim().to_ascii_lowercase();
    validate_custom_role_key(&key)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    if store
        .get_custom_role_by_key(&key)
        .map_err(|e| ApiError::internal("role lookup failed").with_detail(e.to_string()))?
        .is_some()
    {
        return Err(ApiError::conflict(format!("role '{key}' already exists")));
    }

    let role = CustomRole {
        id: Uuid::new_v4(),
        key: key.clone(),
        name: name.to_string(),
        description: body.description.trim().to_string(),
        created_at: Utc::now(),
        created_by: caller_uuid(&caller),
    };
    store
        .create_custom_role(&role)
        .map_err(|e| ApiError::internal("failed to create role").with_detail(e.to_string()))?;
    emit_role_audit(&caller, "custom_role_create", &key);
    Ok(Json(
        serde_json::json!({ "status": "created", "id": role.id.to_string(), "key": key }),
    ))
}

/// Body for `PUT /api/roles/{id}`.
#[derive(Deserialize)]
pub struct UpdateCustomRoleBody {
    pub token: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Resolve a custom role from a path segment that is either its UUID or its key.
/// The matrix UI only carries role keys, so the role routes accept either.
fn resolve_custom_role(
    store: &dyn CapabilityStore,
    id_or_key: &str,
) -> Result<CustomRole, ApiError> {
    let by_uuid = Uuid::from_str(id_or_key).ok();
    store
        .list_custom_roles()
        .map_err(|e| ApiError::internal("role lookup failed").with_detail(e.to_string()))?
        .into_iter()
        .find(|r| Some(r.id) == by_uuid || r.key == id_or_key)
        .ok_or_else(|| ApiError::not_found("role not found"))
}

/// PUT /api/roles/{id} — update a custom role's display fields (key is immutable).
pub async fn update_custom_role(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateCustomRoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        body.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let store = capability_store_or_err()?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let role = resolve_custom_role(store.as_ref(), &id)?;
    store
        .update_custom_role(role.id, name, body.description.trim())
        .map_err(|e| ApiError::internal("failed to update role").with_detail(e.to_string()))?;
    emit_role_audit(&caller, "custom_role_update", &role.key);
    Ok(Json(serde_json::json!({ "status": "updated" })))
}

/// DELETE /api/roles/{id} — delete a custom role, reassigning every identity that
/// held it back to `member` and clearing its matrix cells (handled by the store).
pub async fn delete_custom_role(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = crate::gateway::require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::RolesAssign,
    )
    .await?;
    let store = capability_store_or_err()?;
    let role = resolve_custom_role(store.as_ref(), &id)?;

    let reassigned = reassign_role_to_member(&state, &caller, &role.key)?;
    store
        .delete_custom_role(role.id)
        .map_err(|e| ApiError::internal("failed to delete role").with_detail(e.to_string()))?;
    emit_role_audit(&caller, "custom_role_delete", &role.key);
    Ok(Json(
        serde_json::json!({ "status": "deleted", "reassigned": reassigned }),
    ))
}

/// Reassign every identity holding `role_key` back to `member`, recording
/// each change. Returns the count touched. A best-effort sweep — an absent
/// identity store means no users to move.
fn reassign_role_to_member(
    state: &GatewayState,
    caller: &crate::gateway::CallerContext,
    role_key: &str,
) -> Result<usize, ApiError> {
    let Some(store) = state.identity_store.as_ref() else {
        return Ok(0);
    };
    let demoted = demote_holders(store.as_ref(), role_key).map_err(|e| {
        ApiError::internal("reassigning identities failed").with_detail(e.to_string())
    })?;
    let actor = caller
        .identity_id
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    for id in &demoted {
        super::identity::record_role_change(actor, *id, role_key, "member");
    }
    Ok(demoted.len())
}

/// Make every holder of `role_key` a member, reading the whole store a page
/// at a time: a holder past the first page keeps no role that is gone.
/// Returns whom it demoted.
fn demote_holders(
    store: &dyn workforce_identity::store::IdentityStore,
    role_key: &str,
) -> eyre::Result<Vec<uuid::Uuid>> {
    use workforce_identity::store::IdentityFilter;
    use workforce_identity::types::IdentityRole;
    const PAGE: usize = 500;
    let mut holders = Vec::new();
    for offset in (0..).step_by(PAGE) {
        let page = store.list(&IdentityFilter {
            limit: Some(PAGE),
            offset: Some(offset),
            ..IdentityFilter::default()
        })?;
        let last = page.len() < PAGE;
        holders.extend(
            page.into_iter()
                .filter(|i| i.effective_role_key() == role_key),
        );
        if last {
            break;
        }
    }
    for identity in &mut holders {
        identity.set_role(IdentityRole::Member);
        identity.updated_at = Utc::now();
        store.update(identity)?;
    }
    Ok(holders.into_iter().map(|identity| identity.id).collect())
}

fn emit_role_audit(caller: &crate::gateway::CallerContext, action: &str, resource: &str) {
    tracing::info!(target: "sentinel.audit", action = %action, role = %resource, "custom role change");
    access_audit::emit(AccessAuditEvent::new(
        AuditOperation::RoleManage,
        AuditDecision::Allow,
        caller.identity_id.as_deref(),
        None,
        Some(resource),
    ));
}

/// Query for `GET /api/permissions/effective`.
#[derive(Debug, Deserialize)]
pub struct EffectiveQuery {
    pub token: Option<String>,
    pub user_id: String,
    pub agent_id: String,
    pub tool_name: String,
    pub team_ids: Option<String>,
}

/// GET /api/permissions/effective — the REAL effective decision for a
/// `(user, agent, tool)`: the always-on capability matrix plus the rules-cascade
/// overlay when `[permissions] enabled`. For arg-dependent tools it returns the
/// honest per-scope range rather than a single (possibly wrong) answer.
pub async fn effective_permission(
    Query(query): Query<EffectiveQuery>,
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

    let id_store = state
        .identity_store
        .as_ref()
        .ok_or_else(ApiError::service_unavailable)?;
    let uid = Uuid::from_str(&query.user_id)
        .map_err(|_| ApiError::bad_request("user_id must be a UUID"))?;
    let identity = id_store
        .get_by_id(uid)
        .map_err(|e| ApiError::internal("identity lookup failed").with_detail(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("user not found"))?;
    let role_key = identity.effective_role_key().to_string();
    let status = identity.status;

    let matrix = crate::authz::capability_matrix();
    let cap = crate::authz::tool_capability(&query.tool_name, &serde_json::json!({}));
    let matrix_state = cap
        .map(|c| decision_to_state(matrix.decision(&role_key, status, c)))
        .unwrap_or(PermissionState::Allow);

    let rules_enabled = state.permission_evaluator.is_some();
    let rules_state = match state.permission_evaluator.clone() {
        Some(evaluator) => {
            let team_ids = split_team_ids(query.team_ids.as_deref());
            let request = CascadeRequest {
                user_id: &query.user_id,
                agent_id: &query.agent_id,
                team_ids: &team_ids,
                tool_name: &query.tool_name,
            };
            Some(evaluator.evaluate(&request).map_err(|e| {
                ApiError::internal("cascade evaluation failed").with_detail(e.to_string())
            })?)
        }
        None => None,
    };

    let effective = match rules_state {
        Some(r) => PermissionState::most_restrictive(matrix_state, r),
        None => matrix_state,
    };
    let deciding_layer = if rules_state.is_some_and(|r| state_rank(r) > state_rank(matrix_state)) {
        "rules"
    } else if cap.is_some() {
        "capability_matrix"
    } else {
        "ungated"
    };

    Ok(Json(serde_json::json!({
        "tool": query.tool_name,
        "user_role": role_key,
        "capability": cap.map(|c| c.key()),
        "capability_label": cap.map(|c| c.label()),
        "matrix_decision": cap
            .map(|c| decision_to_state(matrix.decision(&role_key, status, c)).as_str())
            .unwrap_or("ungated"),
        "rules_enabled": rules_enabled,
        "rules_decision": rules_state.map(|s| s.as_str()),
        "effective": effective.as_str(),
        "deciding_layer": deciding_layer,
        "arg_dependent": is_arg_dependent_tool(&query.tool_name),
        "branches": arg_branches(&query.tool_name, &role_key, status, &matrix),
    })))
}

fn decision_to_state(d: crate::authz::Decision) -> PermissionState {
    match d {
        crate::authz::Decision::Allow => PermissionState::Allow,
        crate::authz::Decision::Ask => PermissionState::Ask,
        crate::authz::Decision::Deny => PermissionState::Deny,
    }
}

fn state_rank(s: PermissionState) -> u8 {
    match s {
        PermissionState::Allow => 0,
        PermissionState::Ask => 1,
        PermissionState::AlwaysAsk => 2,
        PermissionState::Deny => 3,
    }
}

fn split_team_ids(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

fn is_arg_dependent_tool(tool: &str) -> bool {
    // The tools whose required capability changes with their arguments — each has
    // matching arms in `arg_branches`. (`database.execute` always maps to
    // `DatabaseWrite`; its per-instance `allow_writes` gate is not a capability
    // branch, so it is name-fixed here.)
    matches!(
        tool,
        "vault.set"
            | "vault.move"
            | "claude_code.setup_token"
            | "http.request"
            | "wasm_skill.install"
    )
}

/// For an arg-dependent tool, the distinct capability outcomes per relevant arg
/// shape, each scored for the caller's role — so the lookup is honest instead of
/// guessing one answer. `None` for tools whose capability is fixed by name.
fn arg_branches(
    tool: &str,
    role_key: &str,
    status: workforce_identity::types::IdentityStatus,
    matrix: &crate::authz::CapabilityMatrix,
) -> Option<Vec<serde_json::Value>> {
    let cases: Vec<(&str, serde_json::Value)> = match tool {
        "vault.set" => vec![
            (
                "scope = global (T4)",
                serde_json::json!({ "scope": "global" }),
            ),
            (
                "any per-user or agent-specific scope",
                serde_json::json!({ "scope": "global_per_user" }),
            ),
        ],
        "vault.move" => vec![
            (
                "either endpoint is a shared tier (T3/T4)",
                serde_json::json!({ "from_tier": "agent_specific", "to_tier": "global" }),
            ),
            (
                "both endpoints are per-user",
                serde_json::json!({ "from_tier": "agent_per_user", "to_tier": "global_per_user" }),
            ),
        ],
        "claude_code.setup_token" => vec![
            (
                "scope = agent_shared",
                serde_json::json!({ "scope": "agent_shared" }),
            ),
            (
                "scope = user (default)",
                serde_json::json!({ "scope": "user" }),
            ),
        ],
        "wasm_skill.install" => vec![
            (
                "pre-built bytes (wasm_path, or a digest-pinned release asset)",
                serde_json::json!({ "wasm_path": "skill.wasm" }),
            ),
            (
                "builds on the host (project_path, or a repo cloned and compiled)",
                serde_json::json!({ "project_path": "./skill" }),
            ),
        ],
        "http.request" => vec![
            (
                "method = GET or HEAD",
                serde_json::json!({ "method": "GET" }),
            ),
            (
                "method = POST, PUT, PATCH, DELETE, or unset",
                serde_json::json!({ "method": "POST" }),
            ),
        ],
        _ => return None,
    };
    Some(
        cases
            .into_iter()
            .map(|(when, args)| {
                let cap = crate::authz::tool_capability(tool, &args);
                serde_json::json!({
                    "when": when,
                    "capability": cap.map(|c| c.key()),
                    "decision": cap
                        .map(|c| decision_to_state(matrix.decision(role_key, status, c)).as_str())
                        .unwrap_or("allow"),
                })
            })
            .collect(),
    )
}

/// Enforce phase-2's legal-targets table at write time for User-scope
/// rules. Folds every admin-scope (Team / Agent) rule in the store
/// whose pattern matches the proposed pattern into an `admin_state`,
/// then rejects targets that the runtime cascade would silently
/// ignore. This keeps "write-time validation matches what runtime
/// accepts" — a design invariant.
///
/// Aggregates across *all* Team and Agent rules matching the pattern
/// rather than scoping to a specific (user, agent) pair, since
/// User-scope rules apply across every agent the user uses. System
/// rules live in YAML (not in this store) and are intentionally
/// outside this check — they're already enforced by the runner's
/// PolicyEngine gate.
fn validate_user_scope_target(
    store: &dyn PermissionStore,
    user_id: &str,
    tool_pattern: &str,
    target: PermissionState,
) -> Result<(), ApiError> {
    // Filter to admin-scope rules at the SQL level (`scope_type IN
    // (team, agent)`) so a large store doesn't read User rows we'd
    // discard. Pattern coverage is still evaluated in Rust — patterns
    // can be wildcards and don't compose into a single SQL predicate.
    let filter = PermissionFilter {
        scopes: Some(vec![PermissionScope::Team, PermissionScope::Agent]),
        ..PermissionFilter::default()
    };
    let admin = store.list_rules(&filter).map_err(|e| {
        ApiError::internal("loading rules for write check").with_detail(e.to_string())
    })?;

    let admin_state = admin
        .iter()
        .filter(|r| pattern_covers(&r.tool_pattern, tool_pattern))
        .map(|r| r.state)
        .fold(PermissionState::Allow, PermissionState::most_restrictive);

    if !cascade::user_choice_is_legal(admin_state, target) {
        let legal = cascade::legal_user_targets(admin_state);
        let legal_list = legal
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::bad_request(format!(
            "User-scope state '{}' is not legal under admin_state '{}' for pattern '{}' (allowed: {}). User id: {}",
            target.as_str(),
            admin_state.as_str(),
            tool_pattern,
            if legal_list.is_empty() {
                "none".to_string()
            } else {
                legal_list
            },
            user_id,
        )));
    }
    Ok(())
}

/// True iff every tool name matched by `child` is also matched by
/// `parent` — i.e. the parent rule applies wherever the child rule
/// would. Used for write-time restrictiveness checks, where we need
/// to identify the admin rule(s) that bound the legality of a
/// proposed User-scope row.
fn pattern_covers(parent: &str, child: &str) -> bool {
    if parent == child {
        return true;
    }
    if parent == "*" {
        return true;
    }
    if let Some(parent_prefix) = parent.strip_suffix(".*") {
        if let Some(child_prefix) = child.strip_suffix(".*") {
            return child_prefix == parent_prefix
                || child_prefix.starts_with(&format!("{parent_prefix}."));
        }
        return child.starts_with(&format!("{parent_prefix}."));
    }
    false
}

fn audit_permission_rule_write(op: &'static str, rule: &PermissionRule, actor: Option<&str>) {
    tracing::info!(
        target: "permissions.rule_write",
        op,
        actor = actor.unwrap_or("system"),
        rule_id = %rule.id,
        scope = rule.scope.as_str(),
        scope_id = %rule.scope_id,
        tool_pattern = %rule.tool_pattern,
        state = rule.state.as_str(),
        origin = rule.origin.as_str(),
        "permission_rule_write",
    );
    access_audit::emit(AccessAuditEvent::new(
        AuditOperation::PermissionRuleWrite,
        AuditDecision::Allow,
        actor,
        None,
        Some(&rule.id.to_string()),
    ));
}

fn audit_permission_rule_deletion(rule_id: &str, actor: Option<&str>) {
    tracing::info!(
        target: "permissions.rule_write",
        op = "delete",
        actor = actor.unwrap_or("system"),
        rule_id,
        "permission_rule_write",
    );
    access_audit::emit(AccessAuditEvent::new(
        AuditOperation::PermissionRuleWrite,
        AuditDecision::Allow,
        actor,
        None,
        Some(rule_id),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_custom_role_demotes_every_holder_past_the_first_page() {
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};
        let store = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
        for n in 0..1_001 {
            let mut identity = Identity::new(format!("p{n}"), IdentityRole::Member);
            identity.set_role_key(if n % 2 == 0 { "auditor" } else { "member" });
            store.create(&identity).unwrap();
        }
        assert_eq!(demote_holders(&store, "auditor").unwrap().len(), 501);
        assert!(demote_holders(&store, "auditor").unwrap().is_empty());
    }

    /// The explorer's two halves must agree: a tool flagged arg-dependent has
    /// to supply branches, or the UI renders `arg_dependent: true` with nothing
    /// to show; and a tool with branches must be flagged, or the UI hides them.
    #[test]
    fn arg_dependent_tools_and_their_branches_stay_in_step() {
        let matrix = crate::authz::CapabilityMatrix::default();
        let status = workforce_identity::types::IdentityStatus::Active;
        for tool in [
            "vault.set",
            "vault.move",
            "claude_code.setup_token",
            "http.request",
            "wasm_skill.install",
        ] {
            assert!(is_arg_dependent_tool(tool), "{tool} must be flagged");
            let branches = arg_branches(tool, "member", status, &matrix)
                .unwrap_or_else(|| panic!("{tool} must supply branches"));
            assert_eq!(branches.len(), 2, "{tool}");
        }
        for fixed in ["database.execute", "exec.run", "web.fetch"] {
            assert!(!is_arg_dependent_tool(fixed), "{fixed}");
            assert!(arg_branches(fixed, "member", status, &matrix).is_none());
        }
    }

    /// Without this the explorer answers a flat "deny" for `http.request` — the
    /// empty-args probe fails closed to the mutating branch — and an admin is
    /// told a Member cannot make a request the Member can in fact make.
    #[test]
    fn http_request_branches_separate_reads_from_writes_for_a_member() {
        let matrix = crate::authz::CapabilityMatrix::default();
        let branches = arg_branches(
            "http.request",
            "member",
            workforce_identity::types::IdentityStatus::Active,
            &matrix,
        )
        .expect("branches");
        assert_eq!(branches[0]["capability"], serde_json::Value::Null);
        assert_eq!(branches[0]["decision"], "allow");
        assert_eq!(branches[1]["capability"], "network_write");
        assert_eq!(branches[1]["decision"], "deny");
    }

    #[test]
    fn parse_scope_round_trips() {
        assert_eq!(parse_scope("team").unwrap(), PermissionScope::Team);
        assert_eq!(parse_scope("agent").unwrap(), PermissionScope::Agent);
        assert_eq!(parse_scope("user").unwrap(), PermissionScope::User);
        assert!(parse_scope("system").is_err());
        assert!(parse_scope("").is_err());
    }

    #[test]
    fn parse_state_accepts_all_four_variants() {
        for (input, expected) in [
            ("allow", PermissionState::Allow),
            ("ask", PermissionState::Ask),
            ("always_ask", PermissionState::AlwaysAsk),
            ("deny", PermissionState::Deny),
        ] {
            assert_eq!(parse_state(input).unwrap(), expected);
        }
        assert!(parse_state("require_approval").is_err());
        assert!(parse_state("ALLOW").is_err());
    }

    #[test]
    fn validate_pattern_rejects_empty_and_whitespace() {
        validate_pattern("memory.*").unwrap();
        validate_pattern("vault.delete").unwrap();
        validate_pattern("*").unwrap();
        assert!(validate_pattern("").is_err());
        assert!(validate_pattern("   ").is_err());
        assert!(validate_pattern("memory .*").is_err());
        assert!(validate_pattern("memory.* extra").is_err());
    }

    #[test]
    fn pattern_covers_handles_exact_prefix_and_wildcard() {
        assert!(pattern_covers("*", "anything"));
        assert!(pattern_covers("*", "memory.*"));
        assert!(pattern_covers("memory.*", "memory.recall"));
        assert!(pattern_covers("memory.*", "memory.*"));
        assert!(pattern_covers("memory.*", "memory.recall.*"));
        assert!(pattern_covers("memory.recall", "memory.recall"));
        // Not covered:
        assert!(!pattern_covers("memory.*", "vault.read"));
        assert!(!pattern_covers("memory.recall", "memory.write"));
        assert!(!pattern_covers("memory.recall", "memory.*"));
    }

    #[test]
    fn validate_user_scope_target_enforces_always_ask_parent() {
        use workforce_permissions::sqlite::SqlitePermissionStore;
        use workforce_permissions::types::{PermissionOrigin, PermissionRule};

        let store = SqlitePermissionStore::in_memory().unwrap();
        // Parent team rule pinned at always_ask for memory.* — runtime
        // would silently refuse a User-row "allow" against this. The
        // write-time guard must reject it the same way.
        store
            .upsert_rule(&PermissionRule::new(
                PermissionScope::Team,
                "team-a",
                "memory.*",
                PermissionState::AlwaysAsk,
                PermissionOrigin::Admin,
            ))
            .unwrap();

        let err =
            validate_user_scope_target(&store, "user-1", "memory.recall", PermissionState::Allow)
                .expect_err("Allow under AlwaysAsk must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("always_ask"),
            "error mentions parent state: {msg}"
        );

        // Deny is the only legal target under AlwaysAsk — must succeed.
        validate_user_scope_target(&store, "user-1", "memory.recall", PermissionState::Deny)
            .expect("Deny is legal under AlwaysAsk");

        // Pattern that the parent rule doesn't cover stays unrestricted.
        validate_user_scope_target(&store, "user-1", "vault.delete", PermissionState::Allow)
            .expect("non-overlapping pattern is unrestricted");
    }

    #[test]
    fn validate_user_scope_target_ignores_user_rows_in_parent_chain() {
        // A User-scope row from another user must not constrain a new
        // User-scope write — only Team / Agent rows count as parents.
        use workforce_permissions::sqlite::SqlitePermissionStore;
        use workforce_permissions::types::{PermissionOrigin, PermissionRule};

        let store = SqlitePermissionStore::in_memory().unwrap();
        store
            .upsert_rule(&PermissionRule::new(
                PermissionScope::User,
                "user-other",
                "memory.*",
                PermissionState::Deny,
                PermissionOrigin::UserRuntimeChoice,
            ))
            .unwrap();

        validate_user_scope_target(&store, "user-1", "memory.recall", PermissionState::Allow)
            .expect("other users' rows must not constrain this write");
    }
}

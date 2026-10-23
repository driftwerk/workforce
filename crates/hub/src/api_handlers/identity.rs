use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use workforce_identity::store::IdentityFilter;
use workforce_identity::team::TeamStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus, ProvisioningSource};

use crate::gateway::GatewayState;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

/// Reject assigning a role that is neither a built-in nor a defined custom role,
/// so a typo can't silently strand a user on the lossy `member` fallback.
fn require_known_role_key(role: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    const BUILTINS: [&str; 3] = ["member", "manager", "system_admin"];
    if BUILTINS.contains(&role) {
        return Ok(());
    }
    let known = crate::authz::capability_store()
        .and_then(|s| s.list_custom_roles().ok())
        .is_some_and(|roles| roles.iter().any(|r| r.key == role));
    if known {
        Ok(())
    } else {
        Err(err(
            StatusCode::BAD_REQUEST,
            &format!("unknown role '{role}'"),
        ))
    }
}

fn extract_claims(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::jwt::Claims, (StatusCode, Json<serde_json::Value>)> {
    // Try JWT first — preserves identity_id from the token.
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    if let Some(token) = token {
        if let Some(ref jwt_config) = state.jwt_config {
            if let Ok(claims) = crate::jwt::validate_token(jwt_config, token) {
                return Ok(crate::jwt::current_claims(
                    state.identity_store.as_ref(),
                    claims,
                ));
            }
        }
    }

    // Fall back to static gateway token → SystemAdmin with no identity
    if super::check_owner_auth(state, headers, None) {
        return Ok(crate::jwt::Claims {
            sub: "admin".to_string(),
            role: IdentityRole::SystemAdmin,
            identity_id: None,
            iat: 0,
            exp: u64::MAX,
            scopes: Vec::new(),
            device_id: None,
        });
    }

    Err(err(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

pub(crate) fn require_auth(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::jwt::Claims, (StatusCode, Json<serde_json::Value>)> {
    extract_claims(state, headers)
}

/// Extract the caller's `identity_id` from the JWT, rejecting static admin
/// tokens (no identity) and malformed (non-UUID) ids. Shared by the per-user
/// `/api/me/*` handlers so each is scoped to exactly one human.
pub(crate) fn require_identity(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    require_person(state, headers).map(|(person, _)| person.to_string())
}

/// [`require_identity`], keeping the claims it was read from.
pub(crate) fn require_person(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<(uuid::Uuid, crate::jwt::Claims), (StatusCode, Json<serde_json::Value>)> {
    let claims = require_auth(state, headers)?;
    let person = claims
        .identity_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            err(
                StatusCode::FORBIDDEN,
                "This endpoint requires a user identity (not a static admin token)",
            )
        })
        .and_then(|id| {
            uuid::Uuid::parse_str(id)
                .map_err(|_| err(StatusCode::FORBIDDEN, "Invalid identity format"))
        })?;
    Ok((person, claims))
}

fn require_admin(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::jwt::Claims, (StatusCode, Json<serde_json::Value>)> {
    let claims = extract_claims(state, headers)?;
    if !claims.role.can_manage_system() {
        return Err(err(StatusCode::FORBIDDEN, "Requires SystemAdmin role"));
    }
    Ok(claims)
}

/// Enforce [`crate::authz::Capability::RolesAssign`] for the user-management
/// (identity create/update/delete) routes through the one capability engine.
/// Unlike [`require_admin`], the caller is resolved to a [`crate::authz::Principal`]
/// via a live identity lookup, so a suspended SystemAdmin is denied even with a
/// structurally-valid JWT (the suspended-admin-on-REST residual, closed here too).
/// Header-only auth: these routes carry no `?token=` query param.
pub(crate) async fn require_roles_assign(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::authz::Principal, (StatusCode, Json<serde_json::Value>)> {
    let caller = crate::gateway::resolve_caller_context(state, headers, None).await;
    if !caller.is_authenticated() {
        return Err(err(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let principal = crate::gateway::gateway_principal(&caller, state.identity_store.as_ref());
    crate::authz::enforce(
        &principal,
        crate::authz::Capability::RolesAssign,
        &crate::authz::capability_matrix(),
    )
    .map_err(|_| err(StatusCode::FORBIDDEN, "Requires SystemAdmin role"))?;
    Ok(principal)
}

/// Minting the built-in `system_admin` role is reserved for a genuine, active
/// SystemAdmin — `RolesAssign` alone (which an admin can delegate to a custom
/// role) must not let that role escalate itself or others to full admin.
fn guard_system_admin_grant(
    caller: &crate::authz::Principal,
    role_key: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if role_key == "system_admin" && !crate::authz::is_effective_admin(caller.role, caller.status) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only a System Admin may grant the System Admin role",
        ));
    }
    Ok(())
}

/// Allow SystemAdmin or Manager. Used by stats endpoints that expose
/// other-user data (leaderboards, per-agent breakdowns).
pub(crate) fn require_admin_or_manager(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::jwt::Claims, (StatusCode, Json<serde_json::Value>)> {
    let claims = extract_claims(state, headers)?;
    if !claims.role.can_manage_teams() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Requires SystemAdmin or Manager role",
        ));
    }
    Ok(claims)
}

/// Allow the request when the caller is SystemAdmin / Manager OR is the
/// identity being requested. Used by `GET /api/stats/identities/{id}` and
/// `/identities/{id}/tasks` so users can read their own usage without
/// admin gating other users' data.
pub(crate) fn require_self_or_admin(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    target_identity_id: &str,
) -> Result<crate::jwt::Claims, (StatusCode, Json<serde_json::Value>)> {
    let claims = extract_claims(state, headers)?;
    if claims.role.can_manage_teams() {
        return Ok(claims);
    }
    if claims.identity_id.as_deref() == Some(target_identity_id) {
        return Ok(claims);
    }
    Err(err(
        StatusCode::FORBIDDEN,
        "You can only view your own stats",
    ))
}

fn require_store(
    state: &GatewayState,
) -> Result<&Arc<dyn workforce_identity::store::IdentityStore>, (StatusCode, Json<serde_json::Value>)>
{
    state.identity_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Identity service not configured",
        )
    })
}

fn require_team_store(
    state: &GatewayState,
) -> Result<&Arc<dyn TeamStore>, (StatusCode, Json<serde_json::Value>)> {
    state.team_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Team service not configured",
        )
    })
}

// ── Identity Endpoints ──────────────────────────────────────────────

const DEFAULT_IDENTITY_PAGE: usize = 100;
const MAX_IDENTITY_PAGE: usize = 500;

#[derive(Debug, Default, Deserialize)]
pub struct IdentityListQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub search: Option<String>,
}

/// Resolve a list query into a store filter: `limit` defaults to
/// [`DEFAULT_IDENTITY_PAGE`] and is clamped to `1..=MAX_IDENTITY_PAGE`, blank
/// search terms collapse to `None`.
fn identity_filter_from_query(query: &IdentityListQuery) -> IdentityFilter {
    IdentityFilter {
        limit: Some(
            query
                .limit
                .unwrap_or(DEFAULT_IDENTITY_PAGE)
                .clamp(1, MAX_IDENTITY_PAGE),
        ),
        offset: Some(query.offset.unwrap_or(0)),
        search: query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        ..Default::default()
    }
}

pub async fn list_identities(
    headers: axum::http::HeaderMap,
    Query(query): Query<IdentityListQuery>,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_store(&state)?;
    let filter = identity_filter_from_query(&query);
    let identities = store
        .list(&filter)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let total = store
        .count_filtered(&filter)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let list: Vec<serde_json::Value> = identities
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id.id.to_string(),
                "display_name": id.display_name,
                "email": id.email,
                "role": id.effective_role_key(),
                "status": id.status,
                "provisioning_source": id.provisioning_source,
                "linked_ids": id.linked_ids,
                "created_at": id.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "identities": list,
        "total": total,
        "limit": filter.limit,
        "offset": filter.offset,
    })))
}

pub async fn get_identity(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_store(&state)?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity ID"))?;

    let identity = store
        .get_by_id(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Identity not found"))?;

    Ok(Json(serde_json::json!({
        "id": identity.id.to_string(),
        "display_name": identity.display_name,
        "email": identity.email,
        "role": identity.effective_role_key(),
        "status": identity.status,
        "provisioning_source": identity.provisioning_source,
        "linked_ids": identity.linked_ids,
        "created_at": identity.created_at.to_rfc3339(),
        "updated_at": identity.updated_at.to_rfc3339(),
    })))
}

#[derive(Deserialize)]
pub struct CreateIdentityRequest {
    pub display_name: String,
    pub email: Option<String>,
    pub role: Option<String>,
}

pub async fn create_identity(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<CreateIdentityRequest>,
) -> ApiResult<serde_json::Value> {
    let caller = require_roles_assign(&state, &headers).await?;
    let store = require_store(&state)?;
    if let Some(role) = req.role.as_deref() {
        require_known_role_key(role)?;
        guard_system_admin_grant(&caller, role)?;
    }
    let role = req
        .role
        .as_deref()
        .map(IdentityRole::from_str_lossy)
        .unwrap_or(IdentityRole::Member);

    let mut identity = Identity::new(req.display_name, role);
    // Preserve a custom role key verbatim (`role` above is the lossy enum view).
    if let Some(role_key) = req.role.as_deref() {
        identity.set_role_key(role_key);
    }
    if let Some(email) = req.email {
        identity = identity.with_email(email);
    }

    store
        .create(&identity)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": identity.id.to_string(),
        "display_name": identity.display_name,
        "role": identity.effective_role_key(),
        "status": "created",
    })))
}

#[derive(Deserialize)]
pub struct UpdateIdentityRequest {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
    pub linked_ids: Option<std::collections::HashMap<String, String>>,
}

pub async fn update_identity(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<UpdateIdentityRequest>,
) -> ApiResult<serde_json::Value> {
    let caller = require_roles_assign(&state, &headers).await?;
    let store = require_store(&state)?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity ID"))?;

    let mut identity = store
        .get_by_id(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Identity not found"))?;

    if let Some(name) = req.display_name {
        identity.display_name = name;
    }
    if let Some(email) = req.email {
        identity.email = Some(email);
    }
    let prior_role = identity.effective_role_key().to_string();
    if let Some(role) = req.role {
        require_known_role_key(&role)?;
        guard_system_admin_grant(&caller, &role)?;
        identity.set_role_key(&role);
    }
    let prior_status = identity.status;
    if let Some(status) = req.status {
        identity.status = IdentityStatus::from_str_lossy(&status);
    }
    if let Some(linked_ids) = req.linked_ids {
        identity.linked_ids = linked_ids;
    }
    identity.updated_at = chrono::Utc::now();

    store
        .update(&identity)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    record_role_change(
        caller.identity_id,
        uuid,
        &prior_role,
        identity.effective_role_key(),
    );
    follow_standing(caller.identity_id, uuid, prior_status, identity.status).await;

    Ok(Json(serde_json::json!({ "status": "updated" })))
}

/// Keep a role change on the person's lifecycle record, so a run that later
/// needs what the old role granted is told they lost it.
pub(crate) fn record_role_change(
    actor: Option<uuid::Uuid>,
    subject: uuid::Uuid,
    from: &str,
    to: &str,
) {
    if let Some(lifecycle) = crate::identity_lifecycle::installed() {
        crate::hub_core::roles::record_role_change(
            lifecycle.identities.as_ref(),
            subject,
            from,
            to,
            &crate::hub_core::roles::Actor(actor),
        );
    }
}

/// After a status change into or out of suspension, hold or release what
/// runs as the person, now. The live checks already refuse a suspended
/// person whether or not this runs; a failure here is logged and the
/// holds catch up on the scheduler's and the workflow maintenance's next
/// review.
async fn follow_standing(
    actor: Option<uuid::Uuid>,
    subject: uuid::Uuid,
    before: IdentityStatus,
    after: IdentityStatus,
) {
    let crossed = (before == IdentityStatus::Suspended) != (after == IdentityStatus::Suspended);
    if crossed {
        crate::standing::forget(subject);
    }
    let Some(lifecycle) = crate::identity_lifecycle::installed().filter(|_| crossed) else {
        return;
    };
    if let Err(e) = lifecycle
        .follow_standing(subject, &crate::hub_core::roles::Actor(actor))
        .await
    {
        tracing::warn!(target: "audit.identity_lifecycle", %subject, error = %format!("{e:#}"), "Following a status change failed");
    }
}

#[derive(Deserialize)]
pub struct ElevateRequest {
    pub email: String,
    pub password: Option<String>,
    pub role: Option<String>,
}

pub async fn elevate_identity(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<ElevateRequest>,
) -> ApiResult<serde_json::Value> {
    let admin = require_admin(&state, &headers)?;
    let store = require_store(&state)?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity ID"))?;

    let mut identity = store
        .get_by_id(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Identity not found"))?;

    // F11 (2026-05-15): snapshot pre-change state for the audit trail.
    let prior_role = identity.role;
    let prior_role_key = identity.effective_role_key().to_string();
    let prior_status = identity.status;
    let password_changed = req.password.is_some();
    let new_role = req
        .role
        .as_deref()
        .map(workforce_identity::types::IdentityRole::from_str_lossy);

    identity.email = Some(req.email);
    if let Some(password) = req.password {
        let hash = workforce_identity::password::hash_password(&password)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        identity.password_hash = Some(hash);
    }
    identity.status = IdentityStatus::Active;
    identity.provisioning_source = ProvisioningSource::Manual;
    if let Some(role) = new_role {
        identity.set_role(role);
    }
    identity.updated_at = chrono::Utc::now();

    store
        .update(&identity)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let actor = admin
        .identity_id
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    record_role_change(actor, uuid, &prior_role_key, identity.effective_role_key());
    follow_standing(actor, uuid, prior_status, identity.status).await;

    // Audit F11 (2026-05-15) — never log the password itself.
    workforce_memory::access_audit::emit(workforce_memory::access_audit::AccessAuditEvent::new(
        workforce_memory::access_audit::AuditOperation::IdentityElevate,
        workforce_memory::access_audit::AuditDecision::Allow,
        admin.identity_id.as_deref(),
        Some(&id),
        Some(&id),
    ));
    tracing::info!(
        target: "audit.identity_elevate",
        target_identity = %id,
        from_role = ?prior_role,
        to_role = ?identity.role,
        from_status = ?prior_status,
        to_status = ?identity.status,
        password_changed,
        caller_identity = ?admin.identity_id,
        "Identity elevated"
    );

    Ok(Json(serde_json::json!({ "status": "elevated" })))
}

#[derive(Deserialize)]
pub struct SetPasswordRequest {
    pub password: String,
}

pub async fn set_password(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<SetPasswordRequest>,
) -> ApiResult<serde_json::Value> {
    require_admin(&state, &headers)?;
    let store = require_store(&state)?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity ID"))?;

    if req.password.len() < 8 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Password must be at least 8 characters",
        ));
    }

    let mut identity = store
        .get_by_id(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Identity not found"))?;

    let hash = workforce_identity::password::hash_password(&req.password)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    identity.password_hash = Some(hash);
    identity.updated_at = chrono::Utc::now();

    store
        .update(&identity)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "password_set" })))
}

// ── Team Endpoints ──────────────────────────────────────────────────

pub async fn list_teams(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let teams = store
        .list_teams()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let list: Vec<serde_json::Value> = teams
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id.to_string(),
                "name": t.name,
                "creator_id": t.creator_id.to_string(),
                "member_count": t.members.len(),
                "pending_requests": t.join_requests.len(),
                "created_at": t.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "teams": list, "count": list.len() }),
    ))
}

pub async fn get_team(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let uuid =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;

    let team = store
        .get_team(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    Ok(Json(serde_json::json!({
        "id": team.id.to_string(),
        "name": team.name,
        "creator_id": team.creator_id.to_string(),
        "members": team.members.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        "join_requests": team.join_requests.iter().map(|r| serde_json::json!({
            "identity_id": r.identity_id.to_string(),
            "requested_at": r.requested_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
        "created_at": team.created_at.to_rfc3339(),
        "updated_at": team.updated_at.to_rfc3339(),
    })))
}

#[derive(Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
    pub creator_id: String,
}

pub async fn create_team(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<CreateTeamRequest>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let creator_id = uuid::Uuid::parse_str(&req.creator_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid creator_id"))?;

    let team = workforce_identity::team::Team::new(req.name, creator_id);
    store
        .create_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": team.id.to_string(),
        "name": team.name,
        "status": "created",
    })))
}

#[derive(Deserialize)]
pub struct UpdateTeamRequest {
    pub name: Option<String>,
}

pub async fn update_team(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<UpdateTeamRequest>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let uuid =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;

    let mut team = store
        .get_team(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    if let Some(name) = req.name {
        team.name = name;
        team.updated_at = chrono::Utc::now();
    }

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "updated" })))
}

pub async fn delete_team(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let uuid =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;

    let team = store
        .get_team(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    store
        .delete_team(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    tracing::info!(
        team_id = %uuid,
        creator_id = %team.creator_id,
        "Team deleted — agents with owner_id={} should be reassigned via config",
        uuid
    );

    Ok(Json(serde_json::json!({
        "status": "deleted",
        "note": "Agents owned by this team should be reassigned via config update"
    })))
}

#[derive(Deserialize)]
pub struct MemberRequest {
    pub identity_id: String,
}

pub async fn add_team_member(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<MemberRequest>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let team_id =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;
    let member_id = uuid::Uuid::parse_str(&req.identity_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity_id"))?;

    let mut team = store
        .get_team(team_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    if !team.add_member(member_id) {
        return Err(err(StatusCode::CONFLICT, "Already a member"));
    }

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "member_added" })))
}

pub async fn remove_team_member(
    Path((id, member_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let team_id =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;
    let member_uuid = uuid::Uuid::parse_str(&member_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid member ID"))?;

    let mut team = store
        .get_team(team_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    if !team.remove_member(member_uuid) {
        return Err(err(StatusCode::NOT_FOUND, "Not a member"));
    }

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "member_removed" })))
}

pub async fn request_join_team(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<MemberRequest>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let team_id =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;
    let identity_id = uuid::Uuid::parse_str(&req.identity_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity_id"))?;

    let mut team = store
        .get_team(team_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    if !team.request_join(identity_id) {
        return Err(err(
            StatusCode::CONFLICT,
            "Already a member or request pending",
        ));
    }

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "join_requested" })))
}

pub async fn approve_join_request(
    Path((id, applicant_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let team_id =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;
    let applicant = uuid::Uuid::parse_str(&applicant_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid applicant ID"))?;

    let mut team = store
        .get_team(team_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    team.add_member(applicant);

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "approved" })))
}

pub async fn reject_join_request(
    Path((id, applicant_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    require_auth(&state, &headers)?;
    let store = require_team_store(&state)?;
    let team_id =
        uuid::Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid team ID"))?;
    let applicant = uuid::Uuid::parse_str(&applicant_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid applicant ID"))?;

    let mut team = store
        .get_team(team_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Team not found"))?;

    if !team.reject_join(applicant) {
        return Err(err(StatusCode::NOT_FOUND, "No pending request"));
    }

    store
        .update_team(&team)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({ "status": "rejected" })))
}

// ── Device-Identity Linking (DW-471) ────────────────────────────────

#[derive(Deserialize)]
pub struct LinkDeviceRequest {
    pub device_id: String,
}

pub async fn link_device(
    State(state): State<Arc<GatewayState>>,
    axum::Extension(claims): axum::Extension<crate::jwt::Claims>,
    Json(req): Json<LinkDeviceRequest>,
) -> ApiResult<serde_json::Value> {
    let store = require_store(&state)?;

    let identity_id = claims
        .identity_id
        .as_deref()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "No identity_id in session"))?;

    let uuid = uuid::Uuid::parse_str(identity_id)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity_id"))?;

    let mut identity = store
        .get_by_id(uuid)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Identity not found"))?;

    identity.link_id("ios_device", &req.device_id);

    store
        .update(&identity)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(serde_json::json!({
        "status": "linked",
        "identity_id": identity_id,
        "device_id": req.device_id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_defaults_to_first_page() {
        let filter = identity_filter_from_query(&IdentityListQuery::default());
        assert_eq!(filter.limit, Some(DEFAULT_IDENTITY_PAGE));
        assert_eq!(filter.offset, Some(0));
        assert_eq!(filter.search, None);
    }

    #[test]
    fn limit_is_clamped_to_bounds() {
        let too_small = identity_filter_from_query(&IdentityListQuery {
            limit: Some(0),
            ..Default::default()
        });
        assert_eq!(too_small.limit, Some(1));

        let too_large = identity_filter_from_query(&IdentityListQuery {
            limit: Some(10_000),
            ..Default::default()
        });
        assert_eq!(too_large.limit, Some(MAX_IDENTITY_PAGE));
    }

    #[test]
    fn blank_search_collapses_to_none_and_trims() {
        let blank = identity_filter_from_query(&IdentityListQuery {
            search: Some("   ".to_string()),
            ..Default::default()
        });
        assert_eq!(blank.search, None);

        let trimmed = identity_filter_from_query(&IdentityListQuery {
            search: Some("  Mat ".to_string()),
            ..Default::default()
        });
        assert_eq!(trimmed.search.as_deref(), Some("Mat"));
    }

    #[test]
    fn offset_passes_through() {
        let filter = identity_filter_from_query(&IdentityListQuery {
            offset: Some(40),
            ..Default::default()
        });
        assert_eq!(filter.offset, Some(40));
    }
}

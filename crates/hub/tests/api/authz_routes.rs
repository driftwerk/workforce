//! Route-level authorization tests for the Phase-1 capability migration.
//!
//! Every privileged gateway route now resolves the caller to a
//! [`workforce_hub::authz::Principal`] and calls `enforce(<capability>)`. These
//! tests pin, per migrated capability, the four-way truth table the migration
//! must hold — and the over-gating guard that member-facing routes are *not*
//! newly blocked:
//!
//! * a Member JWT → 403 (the admin-READ IDOR the slice closes),
//! * an active SystemAdmin JWT → passes the gate (not 401/403),
//! * a SystemAdmin whose live status is `Suspended` → 403 (the suspended-admin
//!   residual closed via the live identity lookup, even with a valid JWT),
//! * the static gateway token → passes the gate,
//! * `/api/me` (member-facing) → a Member is NOT denied.
//!
//! "Passes the gate" is asserted as *not* `401`/`403`: the minimal test state
//! wires no agent service / permission evaluator, so an authorized admin call
//! falls through to `503`/`404`/`200` — which still proves authorization
//! succeeded, mirroring the existing `returns_503_when_manager_absent` pattern.

use std::collections::HashMap;
use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // for oneshot

use workforce_common::persistence::{SqliteStore, WorkflowInstanceRow};
use workforce_common::telemetry::TelemetryStore;
use workforce_hub::gateway::{router, GatewayState};
use workforce_hub::store::Scope;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use crate::support::{token_for, GatewayParts, GATEWAY_TOKEN};

/// Identities spanning the role/status axes the matrix scores.
struct Fixtures {
    admin: Identity,
    manager: Identity,
    member: Identity,
    suspended_admin: Identity,
}

/// Build a minimal `GatewayState` with only the three auth-relevant fields
/// real (`auth_token`, `jwt_config`, `identity_store`); every heavy service is
/// `None`/empty. The capability gate runs before any handler touches those, so
/// deny cases reject up front and authorized cases fall through to a benign
/// `503`/`404` (proving the gate passed). Returns the wired identities too.
fn state_with_fixtures() -> (Arc<GatewayState>, Fixtures) {
    state_with_baseline(false)
}

/// As [`state_with_fixtures`] but with control over whether the live
/// `@destructive → Ask` baseline is reported active — the only field the
/// `/api/capabilities/matrix` `system_defaults` view reads.
fn state_with_baseline(destructive_baseline_active: bool) -> (Arc<GatewayState>, Fixtures) {
    state_with_parts(
        destructive_baseline_active,
        None,
        None,
        None,
        shared_skills_dir(),
    )
}

/// The skills directory every state without skills of its own shares.
fn shared_skills_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/authz-test-skills")
}

/// As [`state_with_baseline`] but with a wired device-pairing service, for
/// pinning how routes classify device-token outcomes (valid / invalid /
/// store failure), optionally a global vault and a telemetry store, and the
/// skills directory external skills are read from.
fn state_with_parts(
    destructive_baseline_active: bool,
    device_pairing: Option<Arc<workforce_pairing::DevicePairingService>>,
    vault: Option<Arc<workforce_hub::vault::Vault>>,
    telemetry_store: Option<Arc<dyn TelemetryStore>>,
    skills_dir: std::path::PathBuf,
) -> (Arc<GatewayState>, Fixtures) {
    let store = SqliteIdentityStore::in_memory().expect("in-memory identity store");
    let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
    let manager = Identity::new("Manager", IdentityRole::Manager);
    let member = Identity::new("Member", IdentityRole::Member);
    // The residual case: a SystemAdmin by role whose live status is Suspended.
    let suspended_admin = Identity::new("Suspended Admin", IdentityRole::SystemAdmin)
        .with_status(IdentityStatus::Suspended);
    store.create(&admin).unwrap();
    store.create(&manager).unwrap();
    store.create(&member).unwrap();
    store.create(&suspended_admin).unwrap();

    let state = GatewayParts {
        vault,
        telemetry_store,
        device_pairing,
        destructive_baseline_active,
        ..GatewayParts::new(
            workforce_hub::agent_registry::AgentRegistry::from_parts(
                HashMap::new(),
                "default".to_string(),
            ),
            Arc::new(store),
            skills_dir,
        )
    }
    .state();

    (
        state,
        Fixtures {
            admin,
            manager,
            member,
            suspended_admin,
        },
    )
}

impl Fixtures {
    /// Mint a JWT carrying this identity's id + role (as a real login would).
    fn token(&self, identity: &Identity) -> String {
        token_for(identity)
    }
}

/// Fire a request at the gateway router with an optional bearer token + JSON
/// body, return the status code.
///
/// The body matters: axum runs the `Json<T>` body extractor *before* the
/// handler's in-body capability guard, so a payload that fails deserialization
/// 422s before authorization is reached. To exercise the guard we send a body
/// that each handler's `Json<T>` accepts — the same shape a real caller sends.
async fn status(
    state: Arc<GatewayState>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let req = builder.body(body).unwrap();
    router(state).oneshot(req).await.unwrap().status()
}

/// A status that is *not* an authorization rejection — i.e. the capability gate
/// let the call through (the handler then 503s/404s/200s on the absent service).
fn gate_passed(s: StatusCode) -> bool {
    s != StatusCode::UNAUTHORIZED && s != StatusCode::FORBIDDEN
}

/// Fire a GET and return the `(status, parsed JSON body)`.
async fn get_json(
    state: Arc<GatewayState>,
    path: &str,
    bearer: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let res = router(state)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Run the four-way truth table against one migrated route: Member denied,
/// active admin passes, suspended admin denied, static token passes. `body` is
/// the JSON payload each `Json<T>` handler needs to get past the body extractor
/// and reach the in-handler guard (`None` for the GET/DELETE routes).
async fn assert_admin_route(method: &str, path: &str, body: Option<serde_json::Value>) {
    let (state, fx) = state_with_fixtures();

    let member = fx.token(&fx.member);
    assert_eq!(
        status(state.clone(), method, path, Some(&member), body.clone()).await,
        StatusCode::FORBIDDEN,
        "{method} {path}: a Member must be denied (admin-read IDOR closed)"
    );

    let admin = fx.token(&fx.admin);
    assert!(
        gate_passed(status(state.clone(), method, path, Some(&admin), body.clone()).await),
        "{method} {path}: an active SystemAdmin must pass the gate"
    );

    let suspended = fx.token(&fx.suspended_admin);
    assert_eq!(
        status(state.clone(), method, path, Some(&suspended), body.clone()).await,
        StatusCode::FORBIDDEN,
        "{method} {path}: a SUSPENDED SystemAdmin must be denied via live status"
    );

    assert!(
        gate_passed(status(state.clone(), method, path, Some(GATEWAY_TOKEN), body).await),
        "{method} {path}: the static gateway token must pass the gate"
    );
}

/// Fire a request and return `(status, parsed JSON body)` — the write-route twin
/// of [`get_json`].
async fn req_json(
    state: Arc<GatewayState>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let res = router(state)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Serialises tests that swap the process-global capability store / matrix cells
/// so each sees its own writes. An async mutex so the guard can be held across the
/// `oneshot` awaits without the `await_holding_lock` hazard a sync lock carries.
async fn lock_global_cap_cells() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Bind a fresh in-memory capability store to the process-global cell.
fn wire_fresh_capability_store() {
    let store =
        Arc::new(workforce_permissions::sqlite::SqlitePermissionStore::in_memory().unwrap());
    workforce_hub::authz::set_capability_store(store);
}

// ── VaultSharedRead / Write / Delete (global vault) ────────────────────────

#[tokio::test]
async fn vault_shared_read_route_enforced() {
    assert_admin_route("GET", "/api/vault", None).await;
}

#[tokio::test]
async fn vault_shared_write_route_enforced() {
    assert_admin_route(
        "POST",
        "/api/vault/some.secret.path",
        Some(serde_json::json!({"value": "s3cr3t"})),
    )
    .await;
}

#[tokio::test]
async fn vault_shared_delete_route_enforced() {
    assert_admin_route("DELETE", "/api/vault/some.secret.path", None).await;
}

#[tokio::test]
async fn vault_rename_route_enforced() {
    assert_admin_route(
        "POST",
        "/api/vault/rename",
        Some(serde_json::json!({"from": "a", "to": "b"})),
    )
    .await;
}

#[tokio::test]
async fn vault_rename_answers_each_outcome_with_its_status() {
    let dir = tempfile::tempdir().unwrap();
    let vault =
        Arc::new(workforce_hub::vault::Vault::open(&dir.path().join("vault.enc"), "key").unwrap());
    vault.set("from/path", "v").unwrap();
    vault.set("taken/path", "t").unwrap();
    let (state, _fx) = state_with_parts(
        false,
        None,
        Some(Arc::clone(&vault)),
        None,
        shared_skills_dir(),
    );
    let rename = |from: &str, to: &str| {
        status(
            state.clone(),
            "POST",
            "/api/vault/rename",
            Some(GATEWAY_TOKEN),
            Some(serde_json::json!({"from": from, "to": to})),
        )
    };

    assert_eq!(
        rename("from/path", "from/path").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        rename("from/path", "users/u1/skills/jira/default_workspace").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rename("missing/path", "to/path").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        rename("from/path", "taken/path").await,
        StatusCode::CONFLICT
    );
    assert_eq!(rename("from/path", "to/path").await, StatusCode::OK);
    assert!(vault.get("from/path").is_none());
    assert_eq!(vault.get("to/path").as_deref(), Some("v"));

    std::fs::write(dir.path().join("vault.enc.frozen"), "{}").unwrap();
    assert_eq!(
        rename("to/path", "moved/path").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(vault.get("to/path").as_deref(), Some("v"));
}

// ── PermissionsManage (permissions read + policy write) ────────────────────

#[tokio::test]
async fn permissions_manage_read_route_enforced() {
    // The admin-READ IDOR: a Member JWT used to pass the toothless
    // `require_scope(SCOPE_ADMIN)` here. Now it is a hard Deny.
    assert_admin_route("GET", "/api/permissions/rules", None).await;
}

#[tokio::test]
async fn permissions_manage_policy_write_route_enforced() {
    assert_admin_route("POST", "/api/policy/rules", Some(serde_json::json!({}))).await;
}

#[tokio::test]
async fn permissions_manage_policy_create_proposal_route_enforced() {
    // Previously member-writable (member JWT + `*` scope passed); now gated.
    assert_admin_route(
        "POST",
        "/api/policy/proposals",
        Some(serde_json::json!({"proposed_rule": "noop", "justification": "test"})),
    )
    .await;
}

// ── RolesAssign (identity create) ──────────────────────────────────────────

#[tokio::test]
async fn roles_assign_identity_create_route_enforced() {
    assert_admin_route(
        "POST",
        "/api/identities",
        Some(serde_json::json!({"display_name": "New User"})),
    )
    .await;
}

// ── Over-gating guard: member-facing routes are NOT newly blocked ──────────

#[tokio::test]
async fn member_facing_me_route_not_blocked_for_member() {
    let (state, fx) = state_with_fixtures();
    let member = fx.token(&fx.member);
    // `/api/me` returns the caller's own identity — a Member must reach it.
    let s = status(state, "GET", "/api/me", Some(&member), None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "/api/me must not be newly blocked for a Member, got {s}"
    );
}

#[tokio::test]
async fn admin_identity_create_actually_succeeds_for_admin() {
    // A concrete Allow that reaches the handler (identity store IS wired),
    // proving the static token + the engine produce a real 200, not just a
    // non-deny fall-through.
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);
    let s = status(
        state,
        "POST",
        "/api/identities",
        Some(&admin),
        Some(serde_json::json!({"display_name": "New User"})),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "an active admin creating an identity should succeed, got {s}"
    );
}

// ── /api/capabilities/matrix (role matrix + active system defaults) ────────

/// Pull the `decision` for a capability key out of one role block.
fn matrix_decision<'a>(role_block: &'a serde_json::Value, capability: &str) -> &'a str {
    role_block
        .get("capabilities")
        .and_then(|c| c.as_array())
        .and_then(|caps| {
            caps.iter()
                .find(|c| c.get("capability").and_then(|v| v.as_str()) == Some(capability))
        })
        .and_then(|c| c.get("decision"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("capability {capability} missing from role block"))
}

#[tokio::test]
async fn capabilities_matrix_returns_builtin_roles_with_expected_decisions() {
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);
    let (status, body) = get_json(state, "/api/capabilities/matrix", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);

    let roles = body["roles"].as_array().expect("roles array");
    let by_key = |key: &str| {
        roles
            .iter()
            .find(|r| r["key"] == key)
            .unwrap_or_else(|| panic!("{key} role block"))
    };
    let member = by_key("member");
    let manager = by_key("manager");
    let sysadmin = by_key("system_admin");

    // Member: own-data allow/ask, every admin capability denied.
    assert_eq!(matrix_decision(member, "own_data_rw"), "allow");
    assert_eq!(matrix_decision(member, "own_data_destructive"), "ask");
    assert_eq!(matrix_decision(member, "agents_manage"), "deny");
    assert_eq!(matrix_decision(member, "permissions_manage"), "deny");
    assert_eq!(matrix_decision(member, "vault_shared_delete"), "deny");
    assert_eq!(matrix_decision(member, "ops_read"), "deny");

    // Manager: a distinct, enforced role — Member plus read-only ops visibility.
    assert_eq!(matrix_decision(manager, "ops_read"), "allow");
    assert_eq!(matrix_decision(manager, "shell_exec"), "deny");

    // SystemAdmin: own-data allow/ask, admin capabilities allowed; cross-user
    // read is the one capability still denied (it's `system`-only).
    assert_eq!(matrix_decision(sysadmin, "own_data_rw"), "allow");
    assert_eq!(matrix_decision(sysadmin, "own_data_destructive"), "ask");
    assert_eq!(matrix_decision(sysadmin, "agents_manage"), "allow");
    assert_eq!(matrix_decision(sysadmin, "permissions_manage"), "allow");
    assert_eq!(matrix_decision(sysadmin, "vault_shared_delete"), "allow");
    assert_eq!(matrix_decision(sysadmin, "crossuser_read"), "deny");

    // Protected capabilities are advertised so the UI can lock their cells.
    let protected = body["protected_capabilities"]
        .as_array()
        .expect("protected_capabilities array");
    assert!(protected.iter().any(|c| c == "agent_export"));
}

#[tokio::test]
async fn capabilities_matrix_omits_destructive_default_when_baseline_off() {
    let (state, fx) = state_with_baseline(false);
    let admin = fx.token(&fx.admin);
    let (status, body) = get_json(state, "/api/capabilities/matrix", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["system_defaults"].as_array().expect("array").len(),
        0,
        "no system default when the baseline is inactive"
    );
}

#[tokio::test]
async fn capabilities_matrix_includes_destructive_default_when_baseline_on() {
    let (state, fx) = state_with_baseline(true);
    let admin = fx.token(&fx.admin);
    let (status, body) = get_json(state, "/api/capabilities/matrix", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);

    let defaults = body["system_defaults"].as_array().expect("array");
    assert_eq!(defaults.len(), 1, "one @destructive default when active");
    assert_eq!(defaults[0]["pattern"], "@destructive");
    assert_eq!(defaults[0]["state"], "ask");
    assert!(defaults[0]["description"]
        .as_str()
        .unwrap()
        .contains("destructive"));
}

#[tokio::test]
async fn capabilities_matrix_denies_a_non_admin() {
    // Same PermissionsManage guard as the rest of `/api/permissions/*`: an
    // authenticated Member is 403'd (anonymous would be 401).
    let (state, fx) = state_with_fixtures();
    let member = fx.token(&fx.member);
    let (status, _body) = get_json(state, "/api/capabilities/matrix", Some(&member)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn capabilities_matrix_put_route_enforced() {
    // The save route is PermissionsManage-gated like the read: Member /
    // suspended-admin denied, active admin / static token pass the gate. An
    // empty `changes` is a no-op, so the authorized path can't perturb the
    // shared matrix cell other tests read.
    assert_admin_route(
        "PUT",
        "/api/capabilities/matrix",
        Some(serde_json::json!({ "changes": [] })),
    )
    .await;
}

#[tokio::test]
async fn capabilities_matrix_put_rejects_unknown_keys() {
    // Wire an in-memory override store so the handler reaches its validation
    // (it sets the process-global *store* cell, not the matrix cell, and the
    // rejections below never apply a change — so no matrix-reading test is
    // perturbed).
    let _cells = lock_global_cap_cells().await;
    wire_fresh_capability_store();

    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);
    for bad in [
        serde_json::json!({ "changes": [{ "role": "ghost_role", "capability": "ops_read", "decision": "allow" }] }),
        serde_json::json!({ "changes": [{ "role": "member", "capability": "not_a_capability", "decision": "allow" }] }),
        serde_json::json!({ "changes": [{ "role": "member", "capability": "ops_read", "decision": "maybe" }] }),
        // A protected capability can't be granted to any role via the matrix.
        serde_json::json!({ "changes": [{ "role": "member", "capability": "agent_export", "decision": "allow" }] }),
    ] {
        let s = status(
            state.clone(),
            "PUT",
            "/api/capabilities/matrix",
            Some(&admin),
            Some(bad),
        )
        .await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "unknown role/capability/decision must 400"
        );
    }
}

// ── Capability catalog + roles + effective-permission lookup ───────────────

#[tokio::test]
async fn capabilities_catalog_exposes_descriptions_and_protected_flag() {
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);
    let (status, body) = get_json(state, "/api/capabilities/catalog", Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);

    let caps = body["capabilities"].as_array().expect("capabilities array");
    let shell = caps
        .iter()
        .find(|c| c["key"] == "shell_exec")
        .expect("shell_exec entry");
    assert!(shell["description"].as_str().unwrap().contains("vault key"));
    assert_eq!(shell["protected"], false);
    assert!(shell["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "exec.run"));

    let export = caps
        .iter()
        .find(|c| c["key"] == "agent_export")
        .expect("agent_export entry");
    assert_eq!(export["protected"], true);
}

#[tokio::test]
async fn roles_routes_are_admin_gated() {
    // The admin / static-token paths actually create the `auditor` role in the
    // process-global capability store, so this must isolate like every other
    // role-mutating test or it races them (a stray `auditor` → 409).
    let _cells = lock_global_cap_cells().await;
    wire_fresh_capability_store();
    assert_admin_route("GET", "/api/roles", None).await;
    assert_admin_route(
        "POST",
        "/api/roles",
        Some(serde_json::json!({ "key": "auditor", "name": "Auditor" })),
    )
    .await;
}

#[tokio::test]
async fn capabilities_catalog_readable_by_any_authenticated_user() {
    // The member-facing My Permissions page reads the catalog, so a Member must
    // be able to — it is non-sensitive metadata. Anonymous is still rejected.
    let (state, fx) = state_with_fixtures();
    let member = fx.token(&fx.member);
    let (s, body) = get_json(state.clone(), "/api/capabilities/catalog", Some(&member)).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a member can read the capability catalog"
    );
    assert!(!body["capabilities"].as_array().unwrap().is_empty());

    let (s, _) = get_json(state, "/api/capabilities/catalog", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "anonymous is rejected");
}

#[tokio::test]
async fn effective_permission_reflects_matrix_with_rules_disabled() {
    let (state, fx) = state_with_fixtures(); // permission_evaluator = None
    let admin = fx.token(&fx.admin);

    // A member + an admin-only tool → matrix Deny, rules layer inert.
    let path = format!(
        "/api/permissions/effective?user_id={}&agent_id=a&tool_name=agent.create",
        fx.member.id
    );
    let (status, body) = get_json(state.clone(), &path, Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["capability"], "agents_manage");
    assert_eq!(body["matrix_decision"], "deny");
    assert_eq!(body["rules_enabled"], false);
    assert_eq!(body["effective"], "deny");
    assert_eq!(body["deciding_layer"], "capability_matrix");

    // An arg-dependent tool returns the honest per-scope range, not one guess.
    let vpath = format!(
        "/api/permissions/effective?user_id={}&agent_id=a&tool_name=vault.set",
        fx.member.id
    );
    let (status, vbody) = get_json(state, &vpath, Some(&admin)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vbody["arg_dependent"], true);
    let branches = vbody["branches"].as_array().expect("branches array");
    assert_eq!(branches.len(), 2);
    assert!(branches
        .iter()
        .any(|b| b["capability"] == "vault_shared_write" && b["decision"] == "deny"));
    assert!(branches
        .iter()
        .any(|b| b["capability"] == "own_data_rw" && b["decision"] == "allow"));
}

#[tokio::test]
async fn custom_role_lifecycle_create_grant_assign_enforce_delete() {
    let _cells = lock_global_cap_cells().await;
    wire_fresh_capability_store();
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);

    // Create a team-free custom role.
    let (s, body) = req_json(
        state.clone(),
        "POST",
        "/api/roles",
        Some(&admin),
        Some(serde_json::json!({ "key": "reviewer", "name": "Reviewer", "description": "reads" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create custom role");
    assert!(body["id"].is_string(), "create returns the new role id");

    // Duplicate key, reserved key, and bad slug are all rejected.
    for (payload, why) in [
        (
            serde_json::json!({ "key": "reviewer", "name": "Dup" }),
            "duplicate",
        ),
        (
            serde_json::json!({ "key": "member", "name": "X" }),
            "reserved",
        ),
        (
            serde_json::json!({ "key": "Bad Key", "name": "X" }),
            "bad slug",
        ),
    ] {
        let (s, _) = req_json(
            state.clone(),
            "POST",
            "/api/roles",
            Some(&admin),
            Some(payload),
        )
        .await;
        assert!(
            s == StatusCode::CONFLICT || s == StatusCode::BAD_REQUEST,
            "{why} key must be rejected (got {s})"
        );
    }

    // Grant ops_read to the custom role via the matrix.
    let (s, _) = req_json(
        state.clone(),
        "PUT",
        "/api/capabilities/matrix",
        Some(&admin),
        Some(serde_json::json!({ "changes": [{ "role": "reviewer", "capability": "ops_read", "decision": "allow" }] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "grant ops_read to the custom role");

    // Assign the member to the custom role from their identity page.
    let id_path = format!("/api/identities/{}", fx.member.id);
    let (s, _) = req_json(
        state.clone(),
        "PUT",
        &id_path,
        Some(&admin),
        Some(serde_json::json!({ "role": "reviewer" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "assign reviewer to member");

    // The member now resolves the custom role's grant end-to-end.
    let member = fx.token(&fx.member);
    let (s, me) = get_json(state.clone(), "/api/me/capabilities", Some(&member)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["role"], "reviewer");
    let ops = me["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["capability"] == "ops_read")
        .expect("ops_read row");
    assert_eq!(ops["decision"], "allow", "custom-role grant is enforced");

    // Deleting the role reverts its holders to member. The matrix UI only knows
    // the role key, so delete BY KEY (the route resolves key-or-UUID).
    let (s, del) = req_json(
        state.clone(),
        "DELETE",
        "/api/roles/reviewer",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "delete custom role by key");
    assert!(
        del["reassigned"].as_u64().unwrap() >= 1,
        "holder reassigned"
    );

    let (s, got) = get_json(state, &id_path, Some(&admin)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        got["role"], "member",
        "deleted role's holder reverts to member"
    );

    // Restore the default global matrix so other tests see built-in defaults.
    workforce_hub::authz::set_capability_matrix(workforce_hub::authz::CapabilityMatrix::default());
}

#[tokio::test]
async fn custom_role_update_by_key_round_trips() {
    let _cells = lock_global_cap_cells().await;
    wire_fresh_capability_store();
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);

    let (s, _) = req_json(
        state.clone(),
        "POST",
        "/api/roles",
        Some(&admin),
        Some(serde_json::json!({ "key": "auditor", "name": "Auditor", "description": "x" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The matrix UI only knows the role key, so PUT must resolve by key.
    let (s, _) = req_json(
        state.clone(),
        "PUT",
        "/api/roles/auditor",
        Some(&admin),
        Some(serde_json::json!({ "name": "Senior Auditor", "description": "reads everything" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "update by key");

    let (s, body) = get_json(state.clone(), "/api/roles", Some(&admin)).await;
    assert_eq!(s, StatusCode::OK);
    let auditor = body["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "auditor")
        .expect("auditor role");
    assert_eq!(auditor["label"], "Senior Auditor");
    assert_eq!(auditor["description"], "reads everything");

    // An unknown key 404s.
    let s = status(
        state,
        "PUT",
        "/api/roles/nope",
        Some(&admin),
        Some(serde_json::json!({ "name": "X" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown role key 404s");
}

#[tokio::test]
async fn roles_assign_holder_cannot_mint_system_admin() {
    let _cells = lock_global_cap_cells().await;
    wire_fresh_capability_store();
    let (state, fx) = state_with_fixtures();
    let admin = fx.token(&fx.admin);

    // A custom role granted `roles_assign` — a deliberate delegation of user
    // management to a non-admin role.
    let (s, _) = req_json(
        state.clone(),
        "POST",
        "/api/roles",
        Some(&admin),
        Some(serde_json::json!({ "key": "usermgr", "name": "User Manager" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = req_json(
        state.clone(),
        "PUT",
        "/api/capabilities/matrix",
        Some(&admin),
        Some(serde_json::json!({ "changes": [{ "role": "usermgr", "capability": "roles_assign", "decision": "allow" }] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A user holding that custom role.
    let (s, mgr) = req_json(
        state.clone(),
        "POST",
        "/api/identities",
        Some(&admin),
        Some(serde_json::json!({ "display_name": "Mgr", "role": "usermgr" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let mgr_id = mgr["id"].as_str().unwrap();
    let mgr_token = workforce_hub::jwt::generate_token_with_identity(
        &crate::support::jwt(),
        "Mgr",
        IdentityRole::Member, // the JWT enum is lossy; the live lookup reads role_key
        Some(mgr_id.to_string()),
    )
    .unwrap();

    // The manager CAN assign a non-admin role (roles_assign works)...
    let victim = format!("/api/identities/{}", fx.member.id);
    let s = status(
        state.clone(),
        "PUT",
        &victim,
        Some(&mgr_token),
        Some(serde_json::json!({ "role": "member" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "roles_assign grants ordinary assignment");

    // ...but CANNOT mint a System Admin — that stays reserved for a real admin.
    let s = status(
        state.clone(),
        "PUT",
        &victim,
        Some(&mgr_token),
        Some(serde_json::json!({ "role": "system_admin" })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a roles_assign holder must not be able to mint a system_admin"
    );

    workforce_hub::authz::set_capability_matrix(workforce_hub::authz::CapabilityMatrix::default());
}

// ── /api/agents: device-token outcome classification ──────────────────────
//
// The roster route must tell apart "no credential" (anonymous → empty 200,
// F1), "credential rejected" (401), and "couldn't check" (503). Collapsing
// the last two into the anonymous empty roster is how a stranded iOS device
// silently showed "No agents" for weeks.

/// Every method fails: stands in for a pairing backend that is down. Only
/// `validate_device_token_hash` is reached by these tests; the rest exist to
/// satisfy the trait.
struct FailingPairingStore;

macro_rules! failing_store_methods {
    ($( fn $name:ident(&self $(, $arg:ident : $ty:ty)*) -> $ret:ty; )+) => {
        $( fn $name(&self $(, $arg: $ty)*) -> $ret {
            let _ = ($( &$arg, )*);
            Err(eyre::eyre!("pairing store unavailable"))
        } )+
    };
}

impl workforce_pairing::PairingStore for FailingPairingStore {
    failing_store_methods! {
        fn create_channel_request(&self, req: &workforce_pairing::ChannelPairingRequest) -> eyre::Result<()>;
        fn get_channel_request_by_code(&self, code: &str) -> eyre::Result<Option<workforce_pairing::ChannelPairingRequest>>;
        fn list_pending_channel_requests(&self) -> eyre::Result<Vec<workforce_pairing::ChannelPairingRequest>>;
        fn update_channel_request_status(&self, id: &str, status: workforce_pairing::PairingStatus, approved_by: Option<&str>) -> eyre::Result<()>;
        fn is_sender_allowed(&self, platform: &str, sender_id: &str) -> eyre::Result<bool>;
        fn add_to_allowlist(&self, entry: &workforce_pairing::ChannelAllowlistEntry) -> eyre::Result<()>;
        fn remove_from_allowlist(&self, platform: &str, sender_id: &str) -> eyre::Result<bool>;
        fn list_allowlist(&self, platform: Option<&str>) -> eyre::Result<Vec<workforce_pairing::ChannelAllowlistEntry>>;
        fn expire_stale_requests(&self, cutoff: chrono::DateTime<chrono::Utc>) -> eyre::Result<usize>;
        fn count_pending(&self) -> eyre::Result<usize>;
        fn create_device_request(&self, req: &workforce_pairing::DevicePairingRequest) -> eyre::Result<()>;
        fn get_device_request_by_code(&self, code: &str) -> eyre::Result<Option<workforce_pairing::DevicePairingRequest>>;
        fn list_pending_device_requests(&self) -> eyre::Result<Vec<workforce_pairing::DevicePairingRequest>>;
        fn update_device_request_status(&self, id: &str, status: workforce_pairing::PairingStatus, approved_by: Option<&str>) -> eyre::Result<()>;
        fn claim_device_request(&self, id: &str, approved_by: &str) -> eyre::Result<bool>;
        fn register_device(&self, device: &workforce_pairing::ApprovedDevice) -> eyre::Result<()>;
        fn get_device_by_id(&self, id: &str) -> eyre::Result<Option<workforce_pairing::ApprovedDevice>>;
        fn validate_device_token_hash(&self, token_hash: &str) -> eyre::Result<Option<(workforce_pairing::ApprovedDevice, workforce_pairing::TokenMatch)>>;
        fn revoke_device(&self, id: &str) -> eyre::Result<()>;
        fn list_devices(&self) -> eyre::Result<Vec<workforce_pairing::ApprovedDevice>>;
        fn update_device_last_used(&self, id: &str) -> eyre::Result<()>;
        fn rotate_device_token(&self, id: &str, new_token_hash: &str, new_expires_at: Option<chrono::DateTime<chrono::Utc>>) -> eyre::Result<()>;
        fn set_device_pending_token(&self, id: &str, token: &str) -> eyre::Result<()>;
        fn clear_device_pending_token(&self, id: &str) -> eyre::Result<()>;
        fn find_device_by_client_id(&self, client_id: &str) -> eyre::Result<Option<workforce_pairing::ApprovedDevice>>;
        fn normalize_device_scopes(&self, canonical: &[String]) -> eyre::Result<usize>;
        fn create_node_request(&self, req: &workforce_pairing::NodePairingRequest) -> eyre::Result<()>;
        fn get_node_request_by_code(&self, code: &str) -> eyre::Result<Option<workforce_pairing::NodePairingRequest>>;
        fn list_pending_node_requests(&self) -> eyre::Result<Vec<workforce_pairing::NodePairingRequest>>;
        fn update_node_request_status(&self, id: &str, status: workforce_pairing::PairingStatus, approved_by: Option<&str>) -> eyre::Result<()>;
        fn is_node_approved(&self, node_id: &str) -> eyre::Result<bool>;
        fn get_any_request_by_code(&self, code: &str) -> eyre::Result<Option<workforce_pairing::store::PairingRequestKind>>;
    }
}

fn pairing_service(
    store: Arc<dyn workforce_pairing::PairingStore>,
) -> Arc<workforce_pairing::DevicePairingService> {
    Arc::new(workforce_pairing::DevicePairingService::new(
        store,
        workforce_pairing::PairingConfig {
            device_pairing_enabled: true,
            ..Default::default()
        },
    ))
}

#[tokio::test]
async fn agents_route_anonymous_without_credentials_gets_empty_200() {
    let (state, _fx) = state_with_fixtures();
    let s = status(state, "GET", "/api/agents", None, None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "credential-less anonymous stays 200 (F1 empty roster)"
    );
}

#[tokio::test]
async fn agents_route_rejected_credential_gets_401_not_empty_roster() {
    let (state, _fx) = state_with_parts(
        false,
        Some(pairing_service(Arc::new(
            workforce_pairing::SqlitePairingStore::in_memory().unwrap(),
        ))),
        None,
        None,
        shared_skills_dir(),
    );
    let s = status(state, "GET", "/api/agents", Some("wfp_dead_token"), None).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "a presented-but-invalid token must 401, never masquerade as the empty anonymous roster"
    );
}

#[tokio::test]
async fn agents_route_pairing_store_failure_gets_503_not_empty_roster() {
    let (state, _fx) = state_with_parts(
        false,
        Some(pairing_service(Arc::new(FailingPairingStore))),
        None,
        None,
        shared_skills_dir(),
    );
    let s = status(state, "GET", "/api/agents", Some("wfp_unverifiable"), None).await;
    assert_eq!(
        s,
        StatusCode::SERVICE_UNAVAILABLE,
        "an unverifiable token must fail closed with 503, not fail open to anonymous"
    );
}

// ── /ws/agent: only the gateway token or a live paired device connects ──────

/// The status a WebSocket upgrade of `/ws/agent?token=…` is answered with,
/// over a real connection: `oneshot` carries no upgradable connection, so the
/// upgrade extractor would refuse before the handler's auth ran.
async fn ws_agent_status(state: Arc<GatewayState>, token: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    // RFC 6455 §1.3's sample nonce, encoded here so it isn't a literal that
    // reads as a credential.
    let key = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"the sample nonce",
    );
    let request = format!(
        "GET /ws/agent?token={token} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: {key}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let line = String::from_utf8(head).unwrap();
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {line:?}"))
}

/// A gateway whose device pairing holds one approved device, and its token.
fn state_with_paired_device() -> (
    Arc<GatewayState>,
    Arc<workforce_pairing::DevicePairingService>,
    workforce_pairing::ApprovedDevice,
    String,
) {
    let pairing = pairing_service(Arc::new(
        workforce_pairing::SqlitePairingStore::in_memory().unwrap(),
    ));
    let request = pairing
        .initiate("laptop", vec![], None, None, None, None)
        .unwrap();
    let (device, token) = pairing.approve(&request.code, "admin").unwrap();
    let (state, _fx) = state_with_parts(
        false,
        Some(Arc::clone(&pairing)),
        None,
        None,
        shared_skills_dir(),
    );
    (state, pairing, device, token)
}

#[tokio::test]
async fn ws_agent_refuses_an_unknown_token_when_pairing_is_configured() {
    let (state, _pairing, _device, _token) = state_with_paired_device();
    assert_eq!(ws_agent_status(state, "wfp_unknown").await, 401);
}

#[tokio::test]
async fn ws_agent_refuses_a_revoked_devices_token() {
    let (state, pairing, device, token) = state_with_paired_device();
    pairing.revoke(&device.id).unwrap();
    assert_eq!(ws_agent_status(state, &token).await, 401);
}

#[tokio::test]
async fn ws_agent_pairing_store_failure_gets_503() {
    let (state, _fx) = state_with_parts(
        false,
        Some(pairing_service(Arc::new(FailingPairingStore))),
        None,
        None,
        shared_skills_dir(),
    );
    assert_eq!(ws_agent_status(state, "wfp_unverifiable").await, 503);
}

// ── /api/pipelines/{scope}/{id}/items/{item}/costs ─────────────────────────

/// A pipeline `id` in `scope`, in a collection of its own so tests never
/// share records, holding ORD-1 as two runs recorded it.
async fn seed_costed_pipeline(id: &str, scope: &Scope) {
    use workforce_hub::pipeline::{define, record_event, EventInput, PipelineDefinition};
    use workforce_hub::store::build_document_store;

    let store = build_document_store(&workforce_common::settings::WorkforceConfig::default())
        .await
        .expect("document store");
    let def: PipelineDefinition = serde_json::from_value(serde_json::json!({
        "name": "Costs",
        "collection": id,
        "lanes": [
            {"id": "implementing", "label": "Implementing"},
            {"id": "reviewing", "label": "Reviewing"}
        ],
        "events": {
            "implement/claimed": [{"column": "implementing"}],
            "review/claimed": [{"column": "reviewing"}]
        }
    }))
    .unwrap();
    define(store.as_ref(), scope, id, &def, None, None)
        .await
        .unwrap();
    for (stage, run) in [("implement", "run-implement"), ("review", "run-review")] {
        let event = EventInput {
            item: "ORD-1".into(),
            stage: stage.into(),
            event: "claimed".into(),
            detail: serde_json::Value::Null,
            run: Some(run.into()),
        };
        record_event(store.as_ref(), scope, &def, &event, None)
            .await
            .unwrap();
    }
}

/// Telemetry holding ORD-1's two runs: Opus on Bedrock as example, then Codex
/// on a ChatGPT login as quill.
fn telemetry_with_runs() -> Arc<dyn TelemetryStore> {
    use workforce_orchestrator::workflow::instance::{
        StepProvenance, TriggerInfo, TriggerSource, WorkflowInstance,
    };

    let store = SqliteStore::in_memory().expect("telemetry store");
    for (run, step, engine, model, cost_usd, agent) in [
        (
            "run-implement",
            "implement",
            "claude-code:bedrock",
            "claude-opus-4-8",
            2.5,
            "example",
        ),
        ("run-review", "review", "codex", "gpt-6-astra", 0.5, "quill"),
    ] {
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({"ticket_id": "ORD-1"}),
        };
        let mut instance = WorkflowInstance::new("order-intake", trigger, step, None);
        instance.record_step_with_provenance(
            step,
            None,
            "done",
            None,
            None,
            chrono::Utc::now(),
            12,
            cost_usd,
            false,
            StepProvenance {
                engine: Some(engine.into()),
                model: Some(model.into()),
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                ..Default::default()
            },
        );
        store
            .save_workflow_instance(&WorkflowInstanceRow {
                id: run.into(),
                workflow_id: instance.workflow_id.clone(),
                status: "completed".into(),
                current_step_id: None,
                step_history: serde_json::to_string(&instance.step_history).unwrap(),
                variables: serde_json::to_string(&instance.variables).unwrap(),
                trigger_json: "{}".into(),
                agent_id: Some(agent.into()),
                created_at: instance.created_at.to_rfc3339(),
                updated_at: instance.updated_at.to_rfc3339(),
                parent_instance_id: None,
                channel_id: None,
                thread_id: None,
                ..Default::default()
            })
            .unwrap();
    }
    Arc::new(store)
}

#[tokio::test]
async fn pipeline_item_costs_answer_an_admin_with_the_runs_costs() {
    seed_costed_pipeline("costs-answer", &Scope::Global).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let path = "/api/pipelines/global/costs-answer/items/ORD-1/costs";

    for bearer in [fx.token(&fx.admin), GATEWAY_TOKEN.to_string()] {
        let (status, body) = get_json(state.clone(), path, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pipeline"], "costs-answer");
        assert_eq!(body["scope"], "global");
        assert_eq!(body["item"], "ORD-1");
        assert_eq!(body["total"]["billed_usd"], 2.5);
        assert_eq!(body["total"]["estimated_usd"], 0.5);
        assert_eq!(body["total"]["runs"], 2);
        assert_eq!(body["total"]["input_tokens"], 2_000);
        assert_eq!(body["coverage"]["runs_named"], 2);
        assert_eq!(body["coverage"]["runs_unmatched"], 0);

        let stages = body["stages"].as_array().unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0]["label"], "Implementing");
        assert_eq!(stages[0]["models"][0]["model"], "claude-opus-4-8");
        assert_eq!(stages[0]["models"][0]["billing"], "billed");
        assert_eq!(stages[1]["label"], "Reviewing");
        assert_eq!(stages[1]["models"][0]["engine"], "codex");
        assert_eq!(stages[1]["models"][0]["billing"], "estimated");
        assert_eq!(body["runs"][0]["booked_to"], "ORD-1");
    }
}

#[tokio::test]
async fn pipeline_item_costs_on_an_agents_pipeline_count_only_that_agents_runs() {
    seed_costed_pipeline("costs-agent", &Scope::Agent("example".into())).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let (status, body) = get_json(
        state,
        "/api/pipelines/agent:example/costs-agent/items/ORD-1/costs",
        Some(&fx.token(&fx.admin)),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["coverage"]["runs_costed"], 1);
    assert_eq!(body["coverage"]["runs_unmatched"], 1);
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["run"], "run-implement");
    assert_eq!(body["total"]["billed_usd"], 2.5);
}

#[tokio::test]
async fn pipeline_item_costs_refuse_a_member_and_a_manager_on_a_global_pipeline() {
    seed_costed_pipeline("costs-refused", &Scope::Global).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let path = "/api/pipelines/global/costs-refused/items/ORD-1/costs";

    for identity in [&fx.member, &fx.manager, &fx.suspended_admin] {
        let (status, body) = get_json(state.clone(), path, Some(&fx.token(identity))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{}: {body}",
            identity.display_name
        );
        assert!(body.get("total").is_none());
    }
}

#[tokio::test]
async fn pipeline_item_costs_need_a_telemetry_store() {
    seed_costed_pipeline("costs-no-telemetry", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let (status, body) = get_json(
        state,
        "/api/pipelines/global/costs-no-telemetry/items/ORD-1/costs",
        Some(&fx.token(&fx.admin)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "unavailable");
}

#[tokio::test]
async fn a_missing_item_has_no_costs() {
    seed_costed_pipeline("costs-missing-item", &Scope::Global).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let (status, body) = get_json(
        state,
        "/api/pipelines/global/costs-missing-item/items/ORD-404/costs",
        Some(&fx.token(&fx.admin)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["error"],
        "no item `ORD-404` in pipeline `costs-missing-item`"
    );
}

#[tokio::test]
async fn the_pipeline_definition_says_whether_costs_are_visible() {
    seed_costed_pipeline("costs-flag", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let path = "/api/pipelines/global/costs-flag";

    let (status, body) = get_json(state.clone(), path, Some(&fx.token(&fx.admin))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_view_costs"], true);

    let (status, body) = get_json(state, path, Some(&fx.token(&fx.member))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_view_costs"], false);
}

// ── /api/pipelines/{scope}/{id}/items/{item}/timings ───────────────────────

/// Every key anywhere in `value`, in nested objects and arrays too.
fn keys_in(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .flat_map(|(key, value)| std::iter::once(key.clone()).chain(keys_in(value)))
            .collect(),
        serde_json::Value::Array(items) => items.iter().flat_map(keys_in).collect(),
        _ => Vec::new(),
    }
}

/// Register example with no access policy, so every member's roster shows it.
/// The returned directory holds the agent's data and must outlive the test.
async fn register_example(state: &GatewayState) -> tempfile::TempDir {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = data_dir.path().to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.api_key = Some("test-key".into());
    let service = workforce_hub::serve::AgentService::from_config(config, None)
        .await
        .expect("test service");
    state.registry.insert("example", service);
    data_dir
}

#[tokio::test]
async fn pipeline_item_timings_answer_everyone_who_reads_the_pipeline_without_costs() {
    seed_costed_pipeline("timings-answer", &Scope::Global).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let path = "/api/pipelines/global/timings-answer/items/ORD-1/timings";
    let items = "/api/pipelines/global/timings-answer/items";

    for bearer in [
        fx.token(&fx.member),
        fx.token(&fx.manager),
        fx.token(&fx.admin),
        fx.token(&fx.suspended_admin),
        GATEWAY_TOKEN.to_string(),
    ] {
        let (status, body) = get_json(state.clone(), path, Some(&bearer)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            status,
            get_json(state.clone(), items, Some(&bearer)).await.0,
            "timings answer whoever the items answer"
        );
        assert_eq!(body["pipeline"], "timings-answer");
        assert_eq!(body["scope"], "global");
        assert_eq!(body["item"], "ORD-1");
        let columns: Vec<&str> = body["visits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|visit| visit["column"].as_str().unwrap())
            .collect();
        assert_eq!(columns, ["implementing", "reviewing"]);
        assert_eq!(body["coverage"]["runs_timed"], 2);
        assert_eq!(body["coverage"]["work_unavailable"], false);
        assert!(body["total"]["work_ms"].is_u64(), "{body}");
        let leaked: Vec<String> = keys_in(&body)
            .into_iter()
            .filter(|key| {
                ["cost", "token", "engine", "model", "billed", "estimated"]
                    .iter()
                    .any(|word| key.contains(word))
            })
            .collect();
        assert!(leaked.is_empty(), "{leaked:?}");
    }
}

#[tokio::test]
async fn pipeline_item_timings_on_an_agents_pipeline_follow_the_roster_and_time_only_its_runs() {
    seed_costed_pipeline("timings-agent", &Scope::Agent("example".into())).await;
    seed_costed_pipeline("timings-hidden", &Scope::Agent("quill".into())).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let _example = register_example(&state).await;
    let member = fx.token(&fx.member);

    let (status, body) = get_json(
        state.clone(),
        "/api/pipelines/agent:example/timings-agent/items/ORD-1/timings",
        Some(&member),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["coverage"]["runs_timed"], 1);
    assert_eq!(body["coverage"]["runs_unmatched"], 1);
    assert_eq!(body["runs"][0]["run"], "run-implement");

    let hidden = "/api/pipelines/agent:quill/timings-hidden/items/ORD-1/timings";
    let (status, body) = get_json(state.clone(), hidden, Some(&member)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body,
        serde_json::json!({"code": "not_found", "error": "no pipeline `timings-hidden` in agent:quill"})
    );
    let (status, body) = get_json(state, hidden, Some(&fx.token(&fx.admin))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn pipeline_item_timings_refuse_an_unauthenticated_caller_as_the_items_do() {
    seed_costed_pipeline("timings-anonymous", &Scope::Global).await;
    let (state, _) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let items = "/api/pipelines/global/timings-anonymous/items";
    let timings = "/api/pipelines/global/timings-anonymous/items/ORD-1/timings";
    for bearer in [None, Some("not-a-token")] {
        let refused = status(state.clone(), "GET", timings, bearer, None).await;
        assert!(!gate_passed(refused), "{bearer:?}: {refused}");
        assert_eq!(
            refused,
            status(state.clone(), "GET", items, bearer, None).await
        );
    }
}

#[tokio::test]
async fn pipeline_item_timings_without_a_telemetry_store_leave_out_work() {
    seed_costed_pipeline("timings-no-telemetry", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let (status, body) = get_json(
        state,
        "/api/pipelines/global/timings-no-telemetry/items/ORD-1/timings",
        Some(&fx.token(&fx.member)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["coverage"]["work_unavailable"], true);
    assert_eq!(body["total"]["work_ms"], serde_json::Value::Null);
    assert_eq!(body["visits"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_missing_item_has_no_timings() {
    seed_costed_pipeline("timings-missing-item", &Scope::Global).await;
    let (state, fx) = state_with_parts(
        false,
        None,
        None,
        Some(telemetry_with_runs()),
        shared_skills_dir(),
    );
    let (status, body) = get_json(
        state,
        "/api/pipelines/global/timings-missing-item/items/ORD-404/timings",
        Some(&fx.token(&fx.member)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["error"],
        "no item `ORD-404` in pipeline `timings-missing-item`"
    );
}

// ── /api/pipelines/{scope}/{id}/items?view=board ───────────────────────────

/// A pipeline `id` in `scope` whose Verified lane keeps a week, in a
/// collection of its own, holding a Verified item ten days old and one two
/// days old, a Verifying item thirty days old and a Not fixed item forty days
/// old. Returns when the oldest event happened.
async fn seed_windowed_pipeline(id: &str, scope: &Scope) -> String {
    use workforce_hub::pipeline::{apply_event, define, EventInput, PipelineDefinition};
    use workforce_hub::store::build_document_store;

    let store = build_document_store(&workforce_common::settings::WorkforceConfig::default())
        .await
        .expect("document store");
    let def: PipelineDefinition = serde_json::from_value(serde_json::json!({
        "name": "Windowed",
        "collection": id,
        "lanes": [
            {"id": "verifying", "label": "Verifying"},
            {"id": "verified", "label": "Verified", "window": "7d"}
        ],
        "exits": [{"id": "not-fixed", "label": "Not fixed"}],
        "events": {
            "verify/rung": [{"column": "verifying", "tone": "active"}],
            "verify/verified": [{"column": "verified", "tone": "good"}],
            "verify/failed": [{"column": "not-fixed", "tone": "bad"}]
        }
    }))
    .unwrap();
    define(store.as_ref(), scope, id, &def, None, None)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let days_ago = |days: i64| {
        (now - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    };
    for (item, event, days) in [
        ("OLD-VERIFIED", "verified", 10),
        ("NEW-VERIFIED", "verified", 2),
        ("STALE", "rung", 30),
        ("NOT-FIXED", "failed", 40),
    ] {
        let recorded = EventInput {
            item: item.into(),
            stage: "verify".into(),
            event: event.into(),
            detail: serde_json::Value::Null,
            run: None,
        };
        let rule = def.rule_for("verify", event, &serde_json::Value::Null);
        let body = apply_event(serde_json::json!({}), &recorded, &days_ago(days), rule);
        store
            .set(scope, id, item, body, None, None, None)
            .await
            .unwrap();
    }
    days_ago(40)
}

/// The items a response holds, by id, sorted.
fn item_ids(body: &serde_json::Value) -> Vec<&str> {
    let mut ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["item"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

fn instant(value: &serde_json::Value) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn the_board_view_hides_finished_items_older_than_their_window() {
    let oldest = seed_windowed_pipeline("board-view", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let member = fx.token(&fx.member);

    let (status, body) = get_json(
        state.clone(),
        "/api/pipelines/global/board-view/items?view=board",
        Some(&member),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(item_ids(&body), ["NEW-VERIFIED", "NOT-FIXED", "STALE"]);
    assert_eq!(body["count"], 3);
    assert_eq!(
        body["windows"],
        serde_json::json!({"verified": {"window": "7d", "default": "7d", "hidden": 1}})
    );
    assert_eq!(
        instant(&body["span"]["from"]),
        instant(&serde_json::json!(oldest))
    );
    assert!(body.get("next_cursor").is_none(), "{body}");

    let (status, body) = get_json(
        state,
        "/api/pipelines/global/board-view/items?view=board&windows=verified:all",
        Some(&member),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        item_ids(&body),
        ["NEW-VERIFIED", "NOT-FIXED", "OLD-VERIFIED", "STALE"]
    );
    assert_eq!(
        body["windows"]["verified"],
        serde_json::json!({"window": "all", "default": "7d", "hidden": 0})
    );
}

#[tokio::test]
async fn items_without_a_view_are_the_page_they_always_were() {
    use workforce_hub::pipeline::{load_definition, position_of};
    use workforce_hub::store::{build_document_store, Direction, Query};

    seed_windowed_pipeline("board-page", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let (status, body) = get_json(
        state,
        "/api/pipelines/global/board-page/items",
        Some(&fx.token(&fx.member)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let store = build_document_store(&workforce_common::settings::WorkforceConfig::default())
        .await
        .expect("document store");
    let (_, def) = load_definition(store.as_ref(), &Scope::Global, "board-page")
        .await
        .unwrap()
        .unwrap();
    let page = store
        .list(
            &Scope::Global,
            "board-page",
            &Query {
                limit: Some(500),
                direction: Direction::Desc,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let items: Vec<serde_json::Value> = page
        .documents
        .iter()
        .map(|d| {
            serde_json::json!({
                "item": d.doc_id,
                "column": position_of(&def, &d.body),
                "version": d.version,
                "updated_at": d.updated_at,
                "record": d.body,
            })
        })
        .collect();
    assert_eq!(
        body,
        serde_json::json!({
            "pipeline": "board-page",
            "scope": "global",
            "count": 4,
            "items": items,
            "next_cursor": null,
        })
    );
}

#[tokio::test]
async fn the_board_view_refuses_parameters_it_cannot_honour() {
    seed_windowed_pipeline("board-invalid", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let member = fx.token(&fx.member);
    for query in [
        "view=list",
        "windows=7d",
        "view=board&limit=10",
        "view=board&cursor=x",
        "view=board&windows=verified:2h",
        "view=board&windows=verified",
        "view=board&windows=verified:7d,verified:30d",
    ] {
        let path = format!("/api/pipelines/global/board-invalid/items?{query}");
        let (status, body) = get_json(state.clone(), &path, Some(&member)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["code"], "invalid", "{query}: {body}");
    }
}

#[tokio::test]
async fn the_board_view_follows_the_roster_and_answers_admins_and_the_gateway() {
    seed_windowed_pipeline("board-agent", &Scope::Agent("example".into())).await;
    seed_windowed_pipeline("board-hidden", &Scope::Agent("quill".into())).await;
    let (state, fx) = state_with_fixtures();
    let _example = register_example(&state).await;
    let visible = "/api/pipelines/agent:example/board-agent/items?view=board";
    let hidden = "/api/pipelines/agent:quill/board-hidden/items?view=board";

    let (status, body) = get_json(state.clone(), visible, Some(&fx.token(&fx.member))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 3);

    let (status, body) = get_json(state.clone(), hidden, Some(&fx.token(&fx.member))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body,
        serde_json::json!({"code": "not_found", "error": "no pipeline `board-hidden` in agent:quill"})
    );

    for bearer in [fx.token(&fx.admin), GATEWAY_TOKEN.to_string()] {
        for path in [visible, hidden] {
            let (status, body) = get_json(state.clone(), path, Some(&bearer)).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
        }
    }
}

#[tokio::test]
async fn the_board_view_answers_an_unauthenticated_or_suspended_caller_as_the_items_do() {
    seed_windowed_pipeline("board-anonymous", &Scope::Global).await;
    let (state, fx) = state_with_fixtures();
    let suspended = fx.token(&fx.suspended_admin);
    let items = "/api/pipelines/global/board-anonymous/items";
    let board = "/api/pipelines/global/board-anonymous/items?view=board";
    for (caller, bearer, refused) in [
        ("anonymous", None, true),
        ("a bad token", Some("not-a-token"), true),
        ("a suspended admin", Some(suspended.as_str()), false),
    ] {
        let answered = status(state.clone(), "GET", board, bearer, None).await;
        assert_eq!(
            answered,
            status(state.clone(), "GET", items, bearer, None).await,
            "{caller}"
        );
        assert_eq!(!gate_passed(answered), refused, "{caller}: {answered}");
    }
}

#[tokio::test]
async fn agent_skill_edits_reach_the_agent_without_a_restart() {
    let (state, _) = state_with_fixtures();
    let example = register_example(&state).await;
    let service = state
        .registry
        .get("example")
        .expect("example is registered");
    let installed = example.path().join("skills").join("hello-world");
    let live = || {
        workforce_hub::external_skills::of(&service)
            .snapshot()
            .iter()
            .any(|skill| skill.discovered.path == installed)
    };
    let request = |method: &str, body: Body| {
        Request::builder()
            .method(method)
            .uri("/api/agents/example/skills/hello-world")
            .header("authorization", format!("Bearer {GATEWAY_TOKEN}"))
            .body(body)
            .unwrap()
    };
    assert!(!live());

    let skill_md =
        "---\nname: hello-world\nversion: 1.0.0\ndescription: Says hello\n---\nSay hello.\n";
    let put = router(state.clone())
        .oneshot(request("PUT", Body::from(skill_md)))
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    assert!(
        live(),
        "a saved SKILL.md is live on the agent's next message"
    );

    let delete = router(state)
        .oneshot(request("DELETE", Body::empty()))
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::OK);
    assert!(!live(), "a deleted skill leaves the agent's live set");
}

/// A skill directory with an access record naming `owner`, or none.
fn external_skill(skills_dir: &std::path::Path, name: &str, owner: Option<&str>) {
    let dir = skills_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\nversion: 1.0.0\ndescription: Says {name}\n---\nBody\n"),
    )
    .unwrap();
    if let Some(owner) = owner {
        std::fs::write(
            dir.join(workforce_skills::installer::ACCESS_FILE),
            serde_json::json!({ "owner": owner, "visibility": "private", "shares": [] })
                .to_string(),
        )
        .unwrap();
    }
}

fn skill_names(body: &serde_json::Value, only: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = body["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|skill| skill["name"].as_str())
        .filter(|name| only.contains(name))
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// The Skills page's three reaches and its grantee set, over the wire: `team`
/// is a visibility the routes take, a team is a grantee they take, and the
/// grantee JSON they answer with is the one they were sent.
#[tokio::test]
async fn skill_reach_routes_take_a_team_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    let (state, fx) = state_with_parts(false, None, None, None, skills_dir.clone());
    let (dir, owner) = (skills_dir.clone(), fx.member.id.to_string());
    state
        .external_skills
        .write(move || {
            external_skill(&dir, "alpha", Some(&owner));
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
    let member = fx.token(&fx.member);
    let member = member.as_str();

    for word in ["private", "team", "everyone"] {
        let (code, body) = req_json(
            state.clone(),
            "PUT",
            "/api/external-skills/alpha/visibility",
            Some(member),
            Some(serde_json::json!({ "visibility": word })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{word}");
        assert_eq!(body["visibility"], word);
        let (_, listed) = get_json(state.clone(), "/api/external-skills", Some(member)).await;
        assert_eq!(listed["skills"][0]["visibility"], word);
    }

    let (code, body) = req_json(
        state.clone(),
        "PUT",
        "/api/external-skills/alpha/visibility",
        Some(member),
        Some(serde_json::json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "`public` is not a visibility — use `private`, `team` or `everyone`"
    );

    let grantees = serde_json::json!([
        { "principal_type": "user", "principal_id": "bob", "permission": "run" },
        { "principal_type": "team", "principal_id": "platform", "permission": "manage" },
    ]);
    let (code, body) = req_json(
        state.clone(),
        "PUT",
        "/api/external-skills/alpha/shares",
        Some(member),
        Some(serde_json::json!({ "shares": grantees })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["count"], 2);
    let (code, body) = get_json(
        state.clone(),
        "/api/external-skills/alpha/shares",
        Some(member),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["shares"], grantees);

    // The owner is never their own grantee, and a level nobody holds is a 400.
    for (bad, message) in [
        (
            serde_json::json!([
                { "principal_type": "user", "principal_id": fx.member.id.to_string(), "permission": "run" }
            ]),
            "the owner already has full access to it",
        ),
        (
            serde_json::json!([
                { "principal_type": "group", "principal_id": "platform", "permission": "run" }
            ]),
            "`group` is not a principal type — use `user` or `team`",
        ),
    ] {
        let (code, body) = req_json(
            state.clone(),
            "PUT",
            "/api/external-skills/alpha/shares",
            Some(member),
            Some(serde_json::json!({ "shares": bad })),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], message);
    }
}

#[tokio::test]
async fn external_skill_routes_answer_by_ownership() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    let (state, fx) = state_with_parts(false, None, None, None, skills_dir.clone());
    let (dir, owner) = (skills_dir.clone(), fx.member.id.to_string());
    state
        .external_skills
        .write(move || {
            external_skill(&dir, "alpha", Some(&owner));
            external_skill(&dir, "beta", None);
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
    let member_token = fx.token(&fx.member);
    let member = member_token.as_str();
    let manager = fx.token(&fx.manager);
    let admin = fx.token(&fx.admin);
    let both = ["alpha", "beta"];

    for (caller, expected) in [
        (member, vec!["alpha", "beta"]),
        (manager.as_str(), vec!["beta"]),
        (admin.as_str(), vec!["alpha", "beta"]),
        (GATEWAY_TOKEN, vec!["alpha", "beta"]),
    ] {
        let (status, body) = get_json(state.clone(), "/api/external-skills", Some(caller)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(skill_names(&body, &both), expected);
        let (status, body) = get_json(state.clone(), "/api/skills", Some(caller)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(skill_names(&body, &both), expected);
    }

    for (caller, expected) in [
        (member, StatusCode::OK),
        (manager.as_str(), StatusCode::NOT_FOUND),
        (admin.as_str(), StatusCode::OK),
    ] {
        assert_eq!(
            status(
                state.clone(),
                "GET",
                "/api/skills/alpha",
                Some(caller),
                None
            )
            .await,
            expected
        );
    }

    let everyone = Some(serde_json::json!({ "visibility": "everyone" }));
    assert_eq!(
        status(
            state.clone(),
            "PUT",
            "/api/external-skills/alpha/visibility",
            Some(&manager),
            everyone.clone()
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(
            state.clone(),
            "PUT",
            "/api/external-skills/alpha/shares",
            Some(&manager),
            Some(serde_json::json!({ "shares": [] }))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // A SystemAdmin whose live status is Suspended holds neither half of the
    // bypass — not the office, and not `skills_manage`, which the matrix
    // demotes with the status. They are the member they have become, so
    // another member's private skill is neither theirs to see nor to re-scope.
    let suspended = fx.token(&fx.suspended_admin);
    let (code, listed) = get_json(state.clone(), "/api/external-skills", Some(&suspended)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(skill_names(&listed, &both), vec!["beta"]);
    assert_eq!(
        status(
            state.clone(),
            "PUT",
            "/api/external-skills/alpha/visibility",
            Some(&suspended),
            everyone.clone()
        )
        .await,
        StatusCode::FORBIDDEN
    );

    let (status_code, body) = req_json(
        state.clone(),
        "PUT",
        "/api/external-skills/alpha/visibility",
        Some(member),
        everyone,
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(body["visibility"], "everyone");
    let (_, listed) = get_json(state.clone(), "/api/external-skills", Some(&manager)).await;
    assert_eq!(skill_names(&listed, &both), both);
    let (status_code, body) = req_json(
        state.clone(),
        "PUT",
        "/api/external-skills/beta/visibility",
        Some(&admin),
        Some(serde_json::json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(status_code, StatusCode::CONFLICT);
    assert_eq!(
        body["error"],
        "Skill `beta` has no owner, so everyone can use it whatever its visibility. Install it \
         again with `visibility = \"private\"` to make it a private skill of your own."
    );
    assert!(!skills_dir
        .join("beta")
        .join(workforce_skills::installer::ACCESS_FILE)
        .exists());

    assert_eq!(
        status(
            state.clone(),
            "DELETE",
            "/api/external-skills/alpha",
            Some(&manager),
            None
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let (status_code, body) = req_json(
        state.clone(),
        "DELETE",
        "/api/external-skills/alpha",
        Some(member),
        None,
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({ "skill": "alpha", "uninstalled": true })
    );
    assert!(!skills_dir.join("alpha").exists());

    assert_eq!(
        status(state.clone(), "GET", "/api/external-skills", None, None).await,
        StatusCode::UNAUTHORIZED
    );
}

/// The `/owner` route's status vocabulary over the wire, for the kind that
/// needs no definition store. A transfer is refused for a reason the requester
/// can act on, or it moves the record and says what that changed.
#[tokio::test]
async fn the_skill_owner_route_answers_each_refusal_with_its_own_status() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    let (state, fx) = state_with_parts(false, None, None, None, skills_dir.clone());
    let (dir, owner) = (skills_dir.clone(), fx.member.id.to_string());
    state
        .external_skills
        .write(move || {
            external_skill(&dir, "alpha", Some(&owner));
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
    let member = fx.token(&fx.member);
    let (member, manager) = (member.as_str(), fx.token(&fx.manager));
    let owner = |who: &Identity| serde_json::json!({ "owner": who.id.to_string() });
    let put = |token: Option<&str>, body: serde_json::Value| {
        let (state, token) = (state.clone(), token.map(str::to_string));
        async move {
            req_json(
                state,
                "PUT",
                "/api/external-skills/alpha/owner",
                token.as_deref(),
                Some(body),
            )
            .await
        }
    };

    // Nobody signed in, and a member who is not the owner.
    assert_eq!(
        put(None, owner(&fx.admin)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (code, body) = put(Some(manager.as_str()), owner(&fx.admin)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(
        body["error"].as_str().unwrap().contains("another user"),
        "{body}"
    );

    // A target nobody may be given it: themselves, a name nobody holds, and a
    // member whose live status is suspended.
    for (target, expected) in [
        (owner(&fx.member), "already owns"),
        (
            serde_json::json!({ "owner": uuid::Uuid::new_v4().to_string() }),
            "can't tell who",
        ),
        (owner(&fx.suspended_admin), "not an active member"),
    ] {
        let (code, body) = put(Some(member), target).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains(expected), "{body}");
    }

    let (code, body) = put(Some(member), owner(&fx.admin)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["to"], fx.admin.id.to_string());
    assert_eq!(body["from"], fx.member.id.to_string());
    assert!(
        body["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("no longer edit it")),
        "{body}"
    );
    // The previous owner has nothing left to give away.
    assert_eq!(
        put(Some(member), owner(&fx.manager)).await.0,
        StatusCode::FORBIDDEN
    );
}

/// The keep-access choice over the wire, on the kind that needs no definition
/// store. It rides in the transfer body rather than in a share sent first: the
/// owner cannot be their own grantee while they still own it, so a share ahead
/// of the handover is refused outright, and the whole grantee set a share
/// endpoint replaces would have taken every other grantee with it.
#[tokio::test]
async fn keeping_manage_over_the_owner_route_leaves_every_other_grantee_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    let (state, fx) = state_with_parts(false, None, None, None, skills_dir.clone());
    let (dir, owner) = (skills_dir.clone(), fx.member.id.to_string());
    state
        .external_skills
        .write(move || {
            for name in ["alpha", "beta"] {
                external_skill(&dir, name, Some(&owner));
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .unwrap();
    let (member, receiver) = (fx.token(&fx.member), fx.token(&fx.admin));
    let (member, receiver) = (member.as_str(), receiver.as_str());
    let grantees = serde_json::json!([
        { "principal_type": "user", "principal_id": "bob", "permission": "run" },
        { "principal_type": "team", "principal_id": "platform", "permission": "manage" },
    ]);
    for name in ["alpha", "beta"] {
        let (code, _) = req_json(
            state.clone(),
            "PUT",
            &format!("/api/external-skills/{name}/shares"),
            Some(member),
            Some(serde_json::json!({ "shares": grantees })),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }

    // A share to the owner is refused however it is sent, which is why the
    // choice has to travel with the transfer.
    let (code, body) = req_json(
        state.clone(),
        "PUT",
        "/api/external-skills/alpha/shares",
        Some(member),
        Some(serde_json::json!({ "shares": [
            { "principal_type": "user", "principal_id": fx.member.id.to_string(),
              "permission": "manage" }
        ]})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "the owner already has full access to it");

    let hand_over = |name: &'static str, keep_manage: bool| {
        let state = state.clone();
        let to = fx.admin.id.to_string();
        async move {
            req_json(
                state,
                "PUT",
                &format!("/api/external-skills/{name}/owner"),
                Some(member),
                Some(serde_json::json!({ "owner": to, "keep_manage": keep_manage })),
            )
            .await
        }
    };
    let shares_now = |name: &'static str| {
        let state = state.clone();
        async move {
            get_json(
                state,
                &format!("/api/external-skills/{name}/shares"),
                Some(receiver),
            )
            .await
            .1
        }
    };

    let (code, body) = hand_over("alpha", true).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(
        body["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("You keep `manage` access")),
        "{body}"
    );
    let mut expected = grantees.as_array().unwrap().clone();
    expected.push(serde_json::json!({
        "principal_type": "user",
        "principal_id": fx.member.id.to_string(),
        "permission": "manage",
    }));
    assert_eq!(
        shares_now("alpha").await["shares"],
        serde_json::Value::Array(expected),
        "exactly one manage grant is added and no other grantee moves"
    );

    // Unticked, the same route behaves as it always has.
    let (code, body) = hand_over("beta", false).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        shares_now("beta").await["shares"],
        grantees,
        "a plain transfer leaves the outgoing owner nothing"
    );
}

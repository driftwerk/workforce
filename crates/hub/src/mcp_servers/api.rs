//! REST API for the Settings UI's "MCP Servers" tab.
//!
//! Routes are mounted on the public gateway router AND wrapped in
//! [`crate::gateway::require_admin_auth`] middleware so unauthenticated
//! callers get 401. The `/authorize` endpoint additionally requires a
//! JWT-derived identity (admin static-token isn't sufficient because
//! we need a `uid` to file the resulting OAuth tokens against).
//!
//! Operations always act on the hub's default agent — multi-agent UI
//! is out of scope for v1; non-default agents drive MCP operations
//! through the Slack-side `mcp.*` agent tools.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use workforce_common::settings::{McpAuthConfig, McpServerConfig};
use workforce_mcp_store::InstalledVia;

use super::callback::ManagerLookup;
use super::manager::InstallReport;
use super::presets;
use crate::gateway::{require_admin_auth, resolve_caller_context, GatewayState};

#[derive(Clone)]
pub struct ApiState {
    pub managers: Arc<dyn ManagerLookup>,
    /// `agent_id` to operate on — typically the hub's default agent.
    pub default_agent_id: String,
    /// Reused for auth checks (admin token / JWT / paired device) and
    /// for resolving the calling user's identity on per-user OAuth flows.
    pub gateway: Arc<GatewayState>,
    /// Externally-reachable hub base URL (no trailing slash). When
    /// set, the presets endpoint returns a `suggested_redirect_uri`
    /// the operator can paste verbatim into the vendor app
    /// registration. `None` (default) leaves the UI showing a
    /// placeholder reminding the operator to configure
    /// `[hub].public_url`.
    pub public_url: Option<String>,
}

/// Mount the MCP-server REST API on the supplied router. Path
/// shape: `GET /api/mcp/servers`, `POST /api/mcp/servers`, etc.
/// Wrapped in [`require_admin_auth`] so the routes are not exposed
/// to unauthenticated callers.
pub fn router(state: ApiState) -> Router {
    let gateway = Arc::clone(&state.gateway);
    Router::new()
        .route("/api/mcp/servers", get(list_servers).post(install_server))
        .route("/api/mcp/credential-catalog", get(credential_catalog))
        .route("/api/mcp/presets", get(list_presets))
        .route("/api/mcp/servers/{id}", delete(uninstall_server))
        .route("/api/mcp/servers/{id}/refresh", post(refresh_server))
        .route("/api/mcp/servers/{id}/authorize", post(start_authorize))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            gateway,
            require_admin_auth,
        ))
}

// ── Handlers ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ListResponse {
    servers: Vec<ServerSummary>,
}

#[derive(Debug, Serialize)]
struct ServerSummary {
    id: String,
    endpoint: String,
    auth_kind: String,
    tool_count: usize,
    installed_via: String,
}

async fn list_servers(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    let mut servers = Vec::new();
    for summary in manager.list() {
        let installed_via = manager
            .show(&summary.id)
            .ok()
            .flatten()
            .map(|d| d.installed_via)
            .unwrap_or_default();
        servers.push(ServerSummary {
            id: summary.id,
            endpoint: summary.endpoint,
            auth_kind: summary.auth_kind.to_string(),
            tool_count: summary.tool_count,
            installed_via,
        });
    }
    (StatusCode::OK, Json(ListResponse { servers })).into_response()
}

#[derive(Debug, Serialize)]
struct PresetsResponse {
    presets: Vec<PresetSummary>,
}

#[derive(Debug, Serialize)]
struct PresetSummary {
    id: String,
    label: String,
    endpoint: String,
    auth_kind: String,
    /// For OAuth presets: vault paths the operator must seed before
    /// the user can click "Authorize". For PAT presets: the single
    /// vault path holding the token.
    required_vault_paths: Vec<String>,
    /// For OAuth presets: the underlying provider's app
    /// registration URL. UI links this so non-technical users can
    /// register the OAuth app without searching.
    oauth_app_registration_url: Option<String>,
    /// Suggested value for `mcp/v1/<id>/redirect_uri` — computed
    /// from `[hub].public_url`. `None` when the operator hasn't
    /// configured it yet; the UI should prompt them to do so.
    suggested_redirect_uri: Option<String>,
    /// Hand-curated walkthrough — every step the operator needs to
    /// take to go from "nothing" to a working install. UI renders
    /// each step's `instructions` and optional `url` next to the
    /// matching credential input.
    setup_steps: Vec<presets::SetupStep>,
}

async fn list_presets(State(state): State<ApiState>) -> impl IntoResponse {
    let public_url = state.public_url.as_deref();
    let presets = presets::ALL
        .iter()
        .map(|p| PresetSummary {
            id: p.id.to_string(),
            label: p.label.to_string(),
            endpoint: p.endpoint.to_string(),
            auth_kind: p.auth_kind().to_string(),
            required_vault_paths: p.required_vault_paths(),
            oauth_app_registration_url: p.oauth_app_registration_url().map(|s| s.to_string()),
            setup_steps: p.setup_steps(public_url),
            suggested_redirect_uri: public_url.and_then(|base| {
                p.oauth_provider_name().map(|provider| {
                    format!(
                        "{}/api/auth/{provider}/callback",
                        base.trim_end_matches('/')
                    )
                })
            }),
        })
        .collect();
    (StatusCode::OK, Json(PresetsResponse { presets })).into_response()
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    /// `"linear" | "github" | "atlassian"`. Mutually exclusive with `custom`.
    #[serde(default)]
    preset: Option<String>,
    /// Custom server config — endpoint + auth shape supplied by the UI.
    #[serde(default)]
    custom: Option<CustomConfig>,
    /// Override server_id. Defaults to preset id; required for custom installs.
    #[serde(default)]
    server_id: Option<String>,
    /// When the calling user has no token for this MCP server, fall
    /// back to the shadowed built-in skill. `None` keeps the preset /
    /// custom default of `false`.
    #[serde(default)]
    fallback_to_builtin: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CustomConfig {
    endpoint: String,
    #[serde(default = "default_transport")]
    transport: String,
    auth: McpAuthConfig,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default = "default_refresh_secs")]
    refresh_interval_secs: u64,
    #[serde(default)]
    shadows: Vec<String>,
    #[serde(default)]
    fallback_to_builtin: bool,
    #[serde(default)]
    tools_allow: Vec<String>,
    #[serde(default)]
    tools_deny: Vec<String>,
}

fn default_transport() -> String {
    "streamable-http".to_string()
}
fn default_refresh_secs() -> u64 {
    3600
}

async fn install_server(
    State(state): State<ApiState>,
    Json(req): Json<InstallRequest>,
) -> impl IntoResponse {
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    let (server_id, mut config, preset) = match (req.preset.as_deref(), req.custom) {
        (Some(preset_id), None) => match presets::lookup(preset_id) {
            Some(p) => (
                req.server_id.unwrap_or_else(|| p.id.to_string()),
                p.to_config(),
                Some(p),
            ),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("unknown preset `{preset_id}`")})),
                )
                    .into_response();
            }
        },
        (None, Some(c)) => {
            let id = match req.server_id {
                Some(id) => id,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "custom install requires `server_id`"})),
                    )
                        .into_response();
                }
            };
            let cfg = McpServerConfig {
                endpoint: c.endpoint,
                transport: c.transport,
                auth: c.auth,
                allowed_hosts: c.allowed_hosts,
                refresh_interval_secs: c.refresh_interval_secs,
                shadows: c.shadows,
                fallback_to_builtin: c.fallback_to_builtin,
                tools_allow: c.tools_allow,
                tools_deny: c.tools_deny,
                // New installs are ungrouped (always active). Group later via
                // `mcp.workspace_add` / `cmd: mcp default`.
                workspace: None,
                env: None,
                default: false,
            };
            (id, cfg, None)
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "specify exactly one of `preset` or `custom`"})),
            )
                .into_response();
        }
    };
    if let Some(fb) = req.fallback_to_builtin {
        config.fallback_to_builtin = fb;
    }

    let install = match preset.filter(|p| p.is_dynamic()) {
        Some(p) => {
            manager
                .install_dynamic(
                    &server_id,
                    config,
                    p.endpoint_var.map(|v| v.vault_path),
                    state.public_url.as_deref(),
                    None,
                    InstalledVia::Api,
                )
                .await
        }
        None => manager.install(&server_id, config, InstalledVia::Api).await,
    };
    match install {
        Ok(report) => {
            let report =
                report.with_preset_setup_steps(preset.map(|p| p.id), state.public_url.as_deref());
            // 202 for `NeedsCredentials` so the UI can branch on
            // status code without parsing the body; 200 otherwise.
            let status = match &report {
                InstallReport::Installed { .. } => StatusCode::OK,
                InstallReport::NeedsCredentials { .. } => StatusCode::ACCEPTED,
            };
            (status, Json(report)).into_response()
        }
        Err(e) => {
            warn!(server = %server_id, error = %e, "MCP REST install failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct UninstallQuery {
    #[serde(default)]
    purge: bool,
}

async fn uninstall_server(
    Path(id): Path<String>,
    Query(q): Query<UninstallQuery>,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    match manager.uninstall(&id, q.purge).await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn refresh_server(
    Path(id): Path<String>,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    match manager.refresh(&id).await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn start_authorize(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    // Derive uid from the request's auth context — never trust a
    // body field, otherwise admin A could file MCP tokens onto admin
    // B's per-user vault path. Admin static-token callers don't have
    // an associated identity so we reject them here; they can still
    // call `mcp.authorize` from Slack where the user identity comes
    // from the message author.
    let caller = resolve_caller_context(&state.gateway, &headers, None).await;
    let Some(uid) = caller.identity_id else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "OAuth authorisation requires a user-bound token (JWT or paired device); admin token alone has no identity to file tokens against"
            })),
        )
            .into_response();
    };
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    match manager.authorize_start(&id, &uid).await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Debug, Serialize)]
struct CredentialCatalogResponse {
    entries: Vec<crate::skill_loader::CatalogEntry>,
}

/// `GET /api/mcp/credential-catalog` — vault-catalog entries for the default
/// agent's *installed* MCP servers (preset and custom), derived from each
/// server's persisted auth config. The UI merges these into the static
/// `/api/vault/catalog` so the credential dropdown covers custom servers
/// (e.g. a self-hosted notion/metabase) that aren't curated presets.
async fn credential_catalog(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(manager) = state.managers.manager_for(&state.default_agent_id) else {
        return server_unavailable();
    };
    let mut entries: Vec<crate::skill_loader::CatalogEntry> = manager
        .list()
        .into_iter()
        .filter_map(|summary| match manager.show(&summary.id) {
            Ok(Some(detail)) => match serde_json::from_value::<McpServerConfig>(detail.config) {
                Ok(config) => Some(presets::config_catalog_entries(&summary.id, &config.auth)),
                Err(e) => {
                    warn!(server = %summary.id, error = %e, "skipping unparseable MCP config");
                    None
                }
            },
            _ => None,
        })
        .flatten()
        .collect();
    // Mirror the static catalog's gate so the editor never offers a per-user
    // option the /api/me/vault endpoint would reject (see gateway::vault_catalog).
    for e in &mut entries {
        let rep = crate::skill_loader::representative_path(&e.template);
        e.per_user_allowed =
            e.per_user_allowed && crate::api_handlers::my_settings::is_allowed_path(&rep);
    }
    (StatusCode::OK, Json(CredentialCatalogResponse { entries })).into_response()
}

fn server_unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "MCP manager not available for default agent"})),
    )
        .into_response()
}

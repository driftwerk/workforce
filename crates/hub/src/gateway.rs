use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{debug, info, warn};
use workforce_agent::runner::ProgressEvent;
use workforce_agent::tools::approval::SelfServe;
use workforce_common::channels::desk::{DeskPlace, DeskTurn, TurnStamp};
use workforce_nats::chat::{
    ChatProgress, NatsChatAttachment, NatsChatRequest, TurnControl, TurnControlReply, REFUSED_BUSY,
};
use workforce_nats::envelope::Envelope;

// Re-export protocol types from the shared crate.
pub use crate::broadcast::BroadcastMessage;
pub use workforce_gateway_client::protocol::{
    Attachment, ChatOutcome, ChatRequest, ChatResponse, ClientMessage, ServerMessage,
};

use crate::agent_registry::AgentRegistry;
use crate::api_handlers::identity_display::{resolve_principals, PrincipalKind, PrincipalRef};
use crate::def_write_auth::{
    authorize_def_write, may_read_def, resolve_visibility, run_as_write, write_visibility,
    DefWriteGrant, DefWriter, StoredShares,
};
use crate::hub_core::live_turns::Applied;
use crate::hub_core::schedules::{
    JobPayload, Schedule, ScheduledJob, VISIBILITY_GLOBAL, VISIBILITY_PRIVATE,
};
use crate::local_agent_registry::LocalAgentRegistry;
use crate::serve::AgentService;
use workforce_agent::security::trust::{classify_trust_with_role, ContentSource, TrustLevel};
use workforce_common::sentinel_config::SentinelConfig;
use workforce_orchestrator::workflow_store::RunAsWrite;
// Brings the artifact CRUD/search methods into scope for the `/api/artifacts`
// handlers (`store` is a concrete `KnowledgeStore`; the methods are trait fns).
use workforce_gateway_client::protocol::{LocalAgentHello, LocalExecResult};
use workforce_orchestrator::artifact_store::ArtifactBackend as _;
use workforce_pairing::{ChannelPairingService, DevicePairingService};

/// Gateway state shared across connections.
pub struct GatewayState {
    pub registry: Arc<AgentRegistry>,
    pub nats: Option<async_nats::Client>,
    pub auth_token: String,
    /// Broadcast channel for streaming responses to multiple clients.
    pub broadcast_tx: broadcast::Sender<BroadcastMessage>,
    /// The NATS progress forwarder of each session with a turn in flight.
    pub progress_forwarders: Arc<ProgressForwarders>,
    /// Channel pairing service (if pairing is configured).
    pub channel_pairing: Option<Arc<ChannelPairingService>>,
    /// Device/API pairing service (if enabled).
    pub device_pairing: Option<Arc<DevicePairingService>>,
    /// Sentinel security configuration for content trust classification.
    pub sentinel: SentinelConfig,
    /// Cross-surface user-access gate (DW-543 follow-up). Consulted on every
    /// inbound WS chat message; hot-reloaded by [`crate::serve::AgentService::reload_config`].
    pub access_gate: Arc<crate::access_gate::AccessGate>,
    /// Drain token — signals graceful shutdown, stops new agent tasks from starting.
    pub drain_token: CancellationToken,
    /// Pending node invoke results, keyed by invoke_id.
    /// An agent tool sends an invoke request to the iOS client; the call is
    /// answered when that person's client responds with `node_invoke_result`.
    pub node_invoke_pending: Arc<crate::node_invoke_tool::PendingInvokes>,
    /// Team orchestrator for multi-agent coordination.
    pub orchestrator: Option<Arc<workforce_orchestrator::orchestrator::Orchestrator>>,
    /// Registry of connected local agents (DW-324).
    pub local_agents: Arc<LocalAgentRegistry>,
    /// Voice-bridge connections registered via `voice_listen`, so scheduled
    /// announcements can find (or fail over from) a listening device.
    pub voice_listeners: Arc<crate::voice_announce::VoiceListeners>,
    /// Global vault for secrets management (owner-only API access).
    pub vault: Option<Arc<crate::vault::Vault>>,
    /// DB-backed artifact knowledge base for the `/api/artifacts` UI
    /// (Artifacts v2, DW-553). `None` when Postgres is unavailable.
    pub artifact_store: Option<Arc<workforce_orchestrator::artifact_store::KnowledgeStore>>,
    /// Definition store carrying the `def_revisions` history — used here for
    /// identity-file revision capture (kind=`identity_file`); workflows and
    /// playbooks record through their runtimes. `None` when Postgres is
    /// unavailable (no history, with a startup warning).
    pub def_store: Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    /// Wakes the config-overlay watcher immediately after an overlay write so
    /// agent create/delete take effect in build-time rather than waiting out
    /// the watcher's poll interval. `None` when no watcher runs (test
    /// harnesses) — callers then fall back to eventual consistency.
    pub overlay_kick: Option<Arc<tokio::sync::Notify>>,
    /// Workflow runtime for CRUD management of workflow definitions.
    pub workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
    /// Playbook runtime for CRUD management of playbook definitions.
    pub playbook_runtime: Option<Arc<dyn crate::hosts::PlaybookHost>>,
    /// Consolidation-template runtime for CRUD of the owned JSON templates the
    /// Xero export consolidates against.
    pub consolidation_runtime: Option<Arc<dyn crate::hosts::ConsolidationHost>>,
    /// Channel-monitor engine — exposes the definition runtime (CRUD), the
    /// durability store (activity), and forward-only cursor seeding.
    pub monitor_engine: Option<Arc<dyn crate::hosts::MonitorHost>>,
    /// Shared per-agent engagement-mode overrides — the same store the Slack
    /// handlers and `messaging.*_mode` tools use, so the admin API reads and
    /// writes the live runtime state.
    pub engagement_overrides: Arc<workforce_common::engagement::EngagementOverrides>,
    /// Path to the workforce.toml config file (for raw config endpoints).
    pub config_path: Option<std::path::PathBuf>,
    /// Identity store for user management APIs.
    pub identity_store: Option<Arc<dyn workforce_identity::store::IdentityStore>>,
    /// Team store for team management APIs.
    pub team_store: Option<Arc<dyn workforce_identity::team::TeamStore>>,
    /// Remote MCP token store (lives in the identity store's backend).
    pub remote_mcp_tokens:
        Option<Arc<dyn workforce_identity::remote_mcp_token::RemoteMcpTokenStore>>,
    /// `[remote_mcp]` — the hub-as-MCP-server surface and its token policy.
    pub remote_mcp: workforce_common::settings::RemoteMcpConfig,
    /// Permission cascade evaluator — owns the rule store + LRU cache
    /// and is the single read/write entry point for `/api/permissions/*`
    /// endpoints plus the runner's Always-* persistence path. `None` when
    /// `[permissions] enabled = false` (staged rollout).
    pub permission_evaluator: Option<Arc<workforce_permissions::cache::PermissionEvaluator>>,
    /// Whether the live `@destructive → Ask` system baseline is in effect —
    /// `[permissions] enabled && [permissions] destructive_baseline`, captured
    /// once at construction from the resolved `wf_config`. Surfaced by the
    /// `/api/capabilities/matrix` admin view as a `system_defaults` row so the
    /// auditor sees the active baseline, not just the role matrix.
    pub destructive_baseline_active: bool,
    /// Telemetry store backing the `/api/stats/*` endpoints. Optional only
    /// because the gateway can technically run without telemetry; in
    /// practice every production deployment populates this from
    /// `HubState::telemetry_store`.
    pub telemetry_store: Option<Arc<dyn workforce_common::telemetry::TelemetryStore>>,
    /// JWT config for auth login/refresh endpoints.
    pub jwt_config: Option<crate::jwt::JwtConfig>,
    /// Rate limiter for password logins, keyed on the attempted account
    /// scoped to the client IP. Per-account keying keeps one user's failed
    /// logins from locking out everyone when a proxy collapses all clients
    /// to one source IP. OIDC endpoints are deliberately not gated by this:
    /// they verify no user-supplied password.
    pub rate_limiter:
        Option<crate::rate_limit::SharedRateLimiter<crate::rate_limit::CredentialKey>>,
    /// Rate limiter for the unauthenticated pairing endpoints
    /// (`/api/pair/device/request`, `/api/pair/device/status/{code}`).
    /// Looser threshold (e.g. 30/min) because legitimate clients poll
    /// repeatedly during the approval window, but still tight enough to
    /// make pairing-code brute force computationally infeasible alongside
    /// the 12-char code length (~59 bits).
    pub pair_rate_limiter: Option<crate::rate_limit::SharedRateLimiter<std::net::IpAddr>>,
    /// Per-identity limiter for expensive authenticated operations: chat
    /// (LLM cost), session mutations, agent CRUD, vault writes. Keyed on
    /// identity_id (falls back to device_id). Closes audit finding F4
    /// (2026-05-15). `None` skips the check — test harnesses can omit it.
    pub action_limiter: Option<crate::rate_limit::SharedActionLimiter>,
    /// CIDR allowlist of trusted reverse proxies. The hub trusts the
    /// `X-Forwarded-For` header only when this list is non-empty. With
    /// an empty allowlist all rate-limiter keying falls back to a
    /// single loopback bucket — safer-but-conservative default for
    /// embedded test harnesses. Closes audit finding F12 (2026-05-15).
    pub trusted_proxies: Arc<crate::trusted_proxy::TrustedProxyConfig>,
    /// Global skills directory for non-agent-scoped skill APIs.
    pub skills_dir: std::path::PathBuf,
    /// The live external skill set for `skills_dir`, which the global skill write APIs refresh.
    pub external_skills: Arc<crate::external_skills::ExternalSkills>,
    /// The skill catalog as this hub serves it: `[[skills.catalogs]]`, their tokens, and live
    /// installs.
    pub skill_catalog: Arc<crate::skill_catalog::SkillCatalog>,
    /// The Slack integration, filled once it starts (after the gateway state
    /// is built); empty in test harnesses and before boot completes.
    pub slack: crate::slack_onboarding::SlackSlot,
    /// Optional OIDC/OAuth2 provider for SSO login (Keycloak etc.).
    #[cfg(feature = "oidc")]
    pub oidc_provider: Option<Arc<crate::oidc::OidcProvider>>,
    /// Agents' and people's pictures. `None` without Postgres.
    pub pictures: Option<Arc<dyn crate::pictures::PictureStore>>,
}

impl GatewayState {}

impl GatewayState {
    /// Get the default agent service (convenience for REST endpoints).
    pub(crate) fn default_service(&self) -> Option<Arc<AgentService>> {
        self.registry.default_agent()
    }
}

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
    pub agent_id: Option<String>,
    /// The conversational channel a `/ws` connection anchors its session on,
    /// so each of a person's apps (and each desktop app room) keeps its own
    /// conversation. iOS when omitted.
    pub channel: Option<String>,
    /// The rich blocks a `/ws` client renders, as comma-separated `[rich]`
    /// flag names (`tables,charts,forms,maps,diagrams,images`). Its turns
    /// are taught the ones its agent also has on; a name this hub does not
    /// know is ignored.
    pub rich: Option<String>,
    /// `1` declares a Desk client on a Desk channel: it sends structured
    /// turns and reads the frames only Desk is sent. Ignored elsewhere.
    pub desk: Option<u8>,
    /// The media types a `/ws` client shows, comma-separated
    /// (`image/*,application/pdf,video/*`, or `*/*`): the files agents
    /// deliver that it is sent. Images alone, inline, when omitted. See
    /// [`workforce_gateway_client::protocol::MediaAccept`].
    pub accept: Option<String>,
}

#[derive(serde::Deserialize)]
struct TranscriptQuery {
    limit: Option<usize>,
    before: Option<String>,
    /// Session that `before` belongs to — lets pagination continue across a
    /// thread rollover seam (the cursor is `(before_session, before)`). When
    /// omitted, `before` is interpreted against the requested session.
    before_session: Option<String>,
    /// Bearer token, accepted via query param for clients that can't set
    /// headers. Same as `Authorization: Bearer <token>`.
    token: Option<String>,
}

/// Embedded UI assets (built from ui/ via `make ui`).
#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist/"]
#[include = "**/*"]
struct UiAssets;

/// Serve the root index.
async fn static_index() -> impl IntoResponse {
    serve_embedded("index.html")
}

/// Whether `path` is under the API or websocket prefixes, which a client
/// calls rather than a browser navigates to.
fn is_api_path(path: &str) -> bool {
    let path = path.trim_start_matches('/');
    ["api", "ws"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// Fallback for every path no route matches: an API or websocket path is a
/// JSON 404 (a client must not mistake the dashboard's index.html for an
/// answer); any other path GET serves the static files and SPA routing.
async fn static_fallback(method: Method, uri: axum::http::Uri) -> axum::response::Response {
    let path = uri.path().trim_start_matches('/');
    if is_api_path(path) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )
            .into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    serve_embedded(if path.is_empty() { "index.html" } else { path })
}

fn serve_embedded(path: &str) -> axum::response::Response {
    match UiAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut response =
                axum::response::Response::builder().header("content-type", mime.as_ref());
            if path != "index.html" && (path.contains('-') || path.contains("assets/")) {
                response = response.header("cache-control", "public, max-age=31536000, immutable");
            } else {
                response = response.header("cache-control", "no-cache");
            }
            response
                .body(axum::body::Body::from(file.data.to_vec()))
                .unwrap()
        }
        None => match UiAssets::get("index.html") {
            Some(index) => axum::response::Response::builder()
                .header("content-type", "text/html")
                .header("cache-control", "no-cache")
                .body(axum::body::Body::from(index.data.to_vec()))
                .unwrap(),
            None => axum::response::Response::builder()
                .status(axum::http::StatusCode::NOT_FOUND)
                .header("content-type", "text/plain")
                .body(axum::body::Body::from("UI not built. Run: make ui"))
                .unwrap(),
        },
    }
}

/// Build a `CorsLayer` from explicit operator configuration.
///
/// Replaces the prior `CorsLayer::permissive()` default that combined a
/// wildcard origin with credentialed requests — a CSRF surface flagged in
/// DW-517. Behaviour:
///   * `enabled = false` → empty layer (no `Access-Control-*` headers,
///     i.e. same-origin only).
///   * Empty `allowed_origins` → empty layer; operators must opt in.
///   * `allowed_origins` containing only `"*"` → wildcard with credentials
///     disabled (CORS spec forbids the combination).
///   * Otherwise → exact origin allow-list, credentials enabled, standard
///     REST methods + auth/content-type headers.
pub fn build_cors_layer(enabled: bool, allowed_origins: &[String]) -> CorsLayer {
    if !enabled || allowed_origins.is_empty() {
        return CorsLayer::new();
    }

    let trimmed: Vec<&str> = allowed_origins.iter().map(|s| s.trim()).collect();

    let methods = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ];
    let headers = [AUTHORIZATION, CONTENT_TYPE];

    if trimmed.contains(&"*") {
        if trimmed.len() > 1 {
            warn!(
                "cors_allowed_origins contains '*' alongside explicit origins; \
                 using wildcard (credentials disabled) and ignoring the rest"
            );
        }
        return CorsLayer::new()
            .allow_origin(AllowOrigin::any())
            .allow_methods(methods)
            .allow_headers(headers);
    }

    let parsed: Vec<HeaderValue> = trimmed
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(origin = %o, error = %e, "Skipping invalid CORS origin");
                None
            }
        })
        .collect();

    if parsed.is_empty() {
        return CorsLayer::new();
    }

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods(methods)
        .allow_headers(headers)
        .allow_credentials(true)
}

/// A picture upload's body: the largest picture, and room for framing.
const PICTURE_BODY_LIMIT: usize = crate::pictures::MAX_BYTES + 64 * 1024;

/// Build the gateway router.
pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/", get(static_index))
        .route("/ws", get(ws_handler))
        .route("/ws/agent", get(ws_local_agent_handler))
        .route("/api/chat", post(chat_handler))
        .route("/api/evals/grade", post(crate::api_handlers::evals::grade))
        .route(
            "/api/evals/know-how",
            post(crate::api_handlers::evals::know_how),
        )
        .route("/api/local-agents", get(list_local_agents))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/transcript", get(session_transcript))
        .route("/api/sessions/{id}/stats", get(session_stats))
        .route("/api/sessions/{id}/export", get(session_export))
        // Session management & chat enhancement endpoints (DW-427)
        // NOTE: static segment /search must be registered before parameterised {id} routes
        .route("/api/sessions/search", get(crate::session_handlers::search_sessions))
        .route("/api/sessions/{id}", axum::routing::delete(crate::session_handlers::delete_session))
        .route("/api/sessions/{id}/reset", post(crate::session_handlers::reset_session))
        .route("/api/sessions/{id}/stop", post(crate::session_handlers::stop_session_turn))
        .route("/api/sessions/{id}/model", post(crate::session_handlers::update_session_model))
        .route("/api/sessions/{id}/label", axum::routing::put(crate::session_handlers::update_session_label))
        .route("/api/sessions/{id}/context", get(crate::session_handlers::get_session_context))
        .route("/api/sessions/{id}/tools", get(crate::session_handlers::get_session_tool_calls))
        .route(
            "/api/sessions/{id}/messages/{msg_id}/retry",
            post(crate::session_handlers::retry_session_message),
        )
        .route(
            "/api/sessions/{id}/messages/{msg_id}",
            axum::routing::delete(crate::session_handlers::delete_session_message),
        )
        .route(
            "/api/sessions/{id}/upload",
            post(crate::session_handlers::upload_session_file)
                // Per-file cap + multipart framing headroom. The handler
                // streams fields to disk, so this bounds a request, not RAM.
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::session_handlers::MAX_FILE_UPLOAD_BYTES as usize + 1024 * 1024,
                )),
        )
        .route(
            "/api/sessions/{id}/files/{name}",
            get(crate::session_handlers::session_file_get),
        )
        .route(
            "/api/agents/{id}/attachments",
            post(crate::desk_attachments::upload)
                // The handler caps the file as it streams it to disk.
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/api/agents/{id}/attachments/{artifact}",
            get(crate::desk_attachments::download),
        )
        .route("/api/models/health", get(model_health))
        .route("/api/models/aliases", get(model_aliases))
        .route("/api/admin/reset-model", post(admin_reset_model))
        .route("/api/agents", get(list_agents))
        // Agent CRUD & detail endpoints (DW-421)
        .route("/api/agents", post(create_agent))
        .route(
            "/api/agents/{id}",
            get(get_agent).put(update_agent).patch(patch_agent).delete(delete_agent),
        )
        .route("/api/agents/{id}/tools", get(get_agent_tools))
        .route("/api/agents/{id}/identity/{filename}", get(get_agent_identity).put(put_agent_identity))
        .route(
            "/api/agents/{id}/identity/{filename}/revisions",
            get(agent_identity_revisions_list),
        )
        .route(
            "/api/agents/{id}/identity/{filename}/revisions/{rev}",
            get(agent_identity_revision_get),
        )
        .route(
            "/api/agents/{id}/identity/{filename}/revisions/{rev}/restore",
            post(agent_identity_revision_restore),
        )
        .route("/api/agents/{id}/config", get(agent_config_get).put(agent_config_put))
        .route("/api/agents/{id}/config/check", post(agent_config_check))
        .route("/api/agents/{id}/logins", get(crate::api_handlers::subscriptions::agent_logins))
        .route(
            "/api/agents/{id}/config/revisions",
            get(agent_config_revisions_list),
        )
        .route(
            "/api/agents/{id}/config/revisions/{rev}",
            get(agent_config_revision_get),
        )
        .route(
            "/api/agents/{id}/config/revisions/{rev}/restore",
            post(agent_config_revision_restore),
        )
        .route("/api/agents/{id}/vault", get(agent_vault_list))
        .route("/api/agents/{id}/vault/move", post(agent_vault_move))
        .route("/api/agents/{id}/vault/copy", post(agent_vault_copy))
        .route("/api/transfer/export", get(export_agents))
        .route(
            "/api/transfer/import",
            post(agent_import_upload)
                .layer(axum::extract::DefaultBodyLimit::max(AGENT_IMPORT_BODY_LIMIT)),
        )
        .route(
            "/api/transfer/import/{artifact_id}/apply",
            post(agent_import_apply),
        )
        .route(
            "/api/agents/{id}/vault/{*path}",
            post(agent_vault_set).delete(agent_vault_delete),
        )
        .route("/api/agents/{id}/skills", get(agent_skills_list))
        .route(
            "/api/agents/{id}/skills/{name}",
            get(agent_skill_get).put(agent_skill_put).delete(agent_skill_delete),
        )
        .route(
            "/api/agents/{id}/skills/{name}/config",
            get(agent_skill_config_get).put(agent_skill_config_put),
        )
        .route("/api/agents/{id}/stats", get(get_agent_stats))
        .route(
            "/api/agents/{id}/slack",
            get(agent_slack_status).put(agent_slack_connect).delete(agent_slack_disconnect),
        )
        .route(
            "/api/agents/{id}/picture",
            get(crate::api_handlers::pictures::agent_picture_get)
                .put(crate::api_handlers::pictures::agent_picture_put)
                .delete(crate::api_handlers::pictures::agent_picture_delete)
                .layer(axum::extract::DefaultBodyLimit::max(PICTURE_BODY_LIMIT)),
        )
        // Global (non-agent-scoped) skill endpoints
        .route("/api/skills", get(global_skills_list))
        .route("/api/skills/catalog", get(skill_catalog_list))
        .route("/api/skills/catalog/install", post(skill_catalog_install))
        .route(
            "/api/skills/{name}",
            get(global_skill_get).put(global_skill_put).delete(global_skill_delete),
        )
        .route(
            "/api/skills/{name}/config",
            get(global_skill_config_get).put(global_skill_config_put),
        )
        .route(
            "/api/wasm-skills",
            get(crate::api_handlers::wasm_skills::list_wasm_skills),
        )
        .route(
            "/api/wasm-skills/{name}",
            axum::routing::delete(crate::api_handlers::wasm_skills::delete_wasm_skill),
        )
        .route(
            "/api/wasm-skills/{name}/shares",
            get(crate::api_handlers::wasm_skills::get_wasm_skill_shares)
                .put(crate::api_handlers::wasm_skills::put_wasm_skill_shares),
        )
        .route(
            "/api/wasm-skills/{name}/visibility",
            axum::routing::put(crate::api_handlers::wasm_skills::put_wasm_skill_visibility),
        )
        .route(
            "/api/wasm-skills/{name}/owner",
            axum::routing::put(crate::api_handlers::wasm_skills::put_wasm_skill_owner),
        )
        .route(
            "/api/external-skills",
            get(crate::api_handlers::external_skills::list_external_skills),
        )
        .route(
            "/api/external-skills/{name}",
            axum::routing::delete(crate::api_handlers::external_skills::delete_external_skill),
        )
        .route(
            "/api/external-skills/{name}/shares",
            get(crate::api_handlers::external_skills::get_external_skill_shares)
                .put(crate::api_handlers::external_skills::put_external_skill_shares),
        )
        .route(
            "/api/external-skills/{name}/visibility",
            axum::routing::put(crate::api_handlers::external_skills::put_external_skill_visibility),
        )
        .route(
            "/api/external-skills/{name}/owner",
            axum::routing::put(crate::api_handlers::external_skills::put_external_skill_owner),
        )
        .route("/api/tools", get(list_tools))
        .route("/api/heartbeat", get(heartbeat_status))
        .route("/api/heartbeat/wake", post(heartbeat_wake))
        .route("/api/livez", get(livez))
        .route("/api/version", get(version_info))
        // Heartbeat check CRUD & history endpoints (DW-422)
        .route("/api/heartbeat/checks", get(list_heartbeat_checks).post(create_heartbeat_check))
        .route(
            "/api/heartbeat/checks/{id}",
            get(get_heartbeat_check).put(update_heartbeat_check).delete(delete_heartbeat_check),
        )
        .route("/api/heartbeat/checks/{id}/trigger", post(trigger_heartbeat_check))
        .route("/api/heartbeat/history", get(get_heartbeat_history))
        .route("/api/agents/{id}/heartbeat", get(agent_heartbeat_status))
        // Unauthenticated device pairing endpoints (self-service flow)
        .route(
            "/api/pair/device/request",
            post(pair_device_request_unauthenticated),
        )
        .route("/api/pair/device/status/{code}", get(pair_device_status))
        // Connecting a device from the browser: the signed-in person approves
        // it into their own account, or an admin assigns it to someone.
        .route(
            "/api/pair/device/pending/{code}",
            get(crate::api_handlers::device_pairing::pending),
        )
        .route(
            "/api/pair/device/approve-self",
            post(crate::api_handlers::device_pairing::approve_self),
        )
        .route(
            "/api/pair/device/reject-self",
            post(crate::api_handlers::device_pairing::reject_self),
        )
        .route(
            "/api/pair/device/approve",
            post(crate::api_handlers::device_pairing::assign),
        )
        .route(
            "/api/pair/device/rotate",
            post(pair_device_rotate_self),
        )
        .route(
            "/api/pair/device/revoke-self",
            post(pair_device_revoke_self),
        )
        .route(
            "/api/approvals/{request_id}/decision",
            post(crate::approvals::decide::decision),
        )
        // Pairing endpoints
        .route("/api/pair/pending", get(pair_list_pending))
        .route("/api/pair/approve/{code}", post(pair_approve))
        .route("/api/pair/reject/{code}", post(pair_reject))
        .route("/api/pair/allowlist", get(pair_list_allowlist))
        .route("/api/pair/device", post(pair_initiate_device))
        .route("/api/pair/devices", get(pair_list_devices))
        .route("/api/pair/devices/{id}/revoke", post(pair_revoke_device))
        .route("/api/pair/devices/{id}/rotate", post(pair_rotate_device))
        // The document store (see `store_api`). A collection is queried by
        // POSTing to it and a scope is batch-written by POSTing to it; the
        // static tails (`claim`) sit under the document so nothing shadows.
        // Pipelines (see `pipeline_api`): the catalogue, one definition, its
        // items and its events, and one item's costs and timings. Static
        // tails sit under the pipeline id.
        .route("/api/pipelines", get(crate::pipeline_api::catalogue))
        .route(
            "/api/pipelines/{scope}/{id}",
            get(crate::pipeline_api::get_definition),
        )
        .route(
            "/api/pipelines/{scope}/{id}/items",
            get(crate::pipeline_api::items),
        )
        .route(
            "/api/pipelines/{scope}/{id}/events",
            get(crate::pipeline_api::events),
        )
        .route(
            "/api/pipelines/{scope}/{id}/items/{item}/costs",
            get(crate::pipeline_api::item_costs),
        )
        .route(
            "/api/pipelines/{scope}/{id}/items/{item}/timings",
            get(crate::pipeline_api::item_timings),
        )
        .route("/api/store/{scope}", post(crate::store_api::batch))
        .route(
            "/api/store/{scope}/_collections",
            get(crate::store_api::collections),
        )
        .route(
            "/api/store/{scope}/{collection}/_describe",
            get(crate::store_api::described).put(crate::store_api::describe),
        )
        .route(
            "/api/store/{scope}/{collection}/{doc_id}/content",
            get(crate::store_api::content_list),
        )
        .route(
            "/api/store/{scope}/{collection}/{doc_id}/content/{name}",
            get(crate::store_api::content_get)
                .put(crate::store_api::content_put)
                .delete(crate::store_api::content_delete),
        )
        .route(
            "/api/store/{scope}/{collection}",
            get(crate::store_api::list).post(crate::store_api::query),
        )
        .route(
            "/api/store/{scope}/{collection}/{doc_id}",
            get(crate::store_api::get_doc)
                .put(crate::store_api::set_doc)
                .patch(crate::store_api::update_doc)
                .delete(crate::store_api::delete_doc),
        )
        .route(
            "/api/store/{scope}/{collection}/{doc_id}/claim",
            post(crate::store_api::claim).delete(crate::store_api::release),
        )
        // Owner-only vault endpoints
        .route("/api/vault", get(vault_list))
        // Credential catalog for the vault editor (path templates, no secrets).
        // Registered before the `{*path}` wildcard so it isn't shadowed.
        .route("/api/vault/catalog", get(vault_catalog))
        .route("/api/vault/rename", post(vault_rename))
        .route(
            "/api/vault/{*path}",
            get(vault_get).post(vault_set).delete(vault_delete),
        )
        // Artifacts v2 knowledge base (DW-553). `search` before the `{id}`
        // route so it isn't captured as an id.
        .route(
            "/api/artifacts",
            get(artifacts_list).post(artifacts_create),
        )
        .route("/api/artifacts/search", get(artifacts_search))
        .route(
            "/api/artifacts/{id}",
            get(artifacts_get)
                .put(artifacts_update)
                .delete(artifacts_delete),
        )
        // Cross-instance runtime artifact serving (Inc 2b). A distinct
        // 4-segment path serving the runtime `global_store`, not the
        // orchestrator KnowledgeStore above.
        .route(
            "/api/artifacts/runtime/{id}",
            get(artifacts_runtime_get),
        )
        .route(
            "/api/artifacts/runtime/{id}/table",
            get(artifacts_runtime_table),
        )
        // Workflow CRUD endpoints
        .route("/api/workflows/export", get(workflow_export))
        .route("/api/workflows", get(workflow_list))
        .route("/api/workflows/{id}/run", post(workflow_run))
        .route(
            "/api/workflows/{id}/source",
            get(workflow_get_source),
        )
        .route(
            "/api/workflows/{id}",
            axum::routing::put(workflow_put).delete(workflow_delete),
        )
        .route(
            "/api/workflows/{id}/revisions",
            get(workflow_revisions_list),
        )
        .route(
            "/api/workflows/{id}/revisions/{rev}",
            get(workflow_revision_get),
        )
        .route(
            "/api/workflows/{id}/revisions/{rev}/restore",
            post(workflow_revision_restore),
        )
        .route(
            "/api/workflows/{id}/shares",
            get(workflow_shares_get).put(workflow_shares_put),
        )
        .route(
            "/api/workflows/{id}/owner",
            axum::routing::put(workflow_owner_put),
        )
        // Playbook CRUD endpoints
        .route("/api/playbooks/export", get(playbook_export))
        .route("/api/playbooks", get(playbook_list))
        .route("/api/playbooks/{id}/source", get(playbook_get_source))
        .route(
            "/api/playbooks/{id}",
            axum::routing::put(playbook_put).delete(playbook_delete),
        )
        .route(
            "/api/playbooks/{id}/revisions",
            get(playbook_revisions_list),
        )
        .route(
            "/api/playbooks/{id}/revisions/{rev}",
            get(playbook_revision_get),
        )
        .route(
            "/api/playbooks/{id}/revisions/{rev}/restore",
            post(playbook_revision_restore),
        )
        .route(
            "/api/playbooks/{id}/shares",
            get(playbook_shares_get).put(playbook_shares_put),
        )
        .route(
            "/api/playbooks/{id}/owner",
            axum::routing::put(playbook_owner_put),
        )
        // Consolidation-template CRUD endpoints (member-owned)
        .route(
            "/api/consolidation-templates/parse-xlsx",
            post(consolidation_parse_xlsx),
        )
        .route(
            "/api/consolidation-templates/upload",
            post(consolidation_upload),
        )
        .route("/api/consolidation-templates", get(consolidation_list))
        .route(
            "/api/consolidation-templates/{id}/source",
            get(consolidation_get_source),
        )
        .route(
            "/api/consolidation-templates/{id}",
            axum::routing::put(consolidation_put).delete(consolidation_delete),
        )
        .route(
            "/api/consolidation-templates/{id}/revisions",
            get(consolidation_revisions_list),
        )
        .route(
            "/api/consolidation-templates/{id}/revisions/{rev}",
            get(consolidation_revision_get),
        )
        .route(
            "/api/consolidation-templates/{id}/revisions/{rev}/restore",
            post(consolidation_revision_restore),
        )
        // An agent's own environments (admin-only).
        .route(
            "/api/agents/{id}/environments",
            get(crate::api_handlers::environments::get_environments)
                .put(crate::api_handlers::environments::put_environments),
        )
        // Channel monitor CRUD endpoints
        .route("/api/monitors/export", get(monitor_export))
        .route("/api/monitors", get(monitor_list))
        .route("/api/monitors/{id}/source", get(monitor_get_source))
        .route(
            "/api/monitors/{id}",
            axum::routing::put(monitor_put).delete(monitor_delete),
        )
        .route("/api/monitors/{id}/activity", get(monitor_activity))
        .route("/api/monitors/{id}/revisions", get(monitor_revisions_list))
        .route(
            "/api/monitors/{id}/revisions/{rev}",
            get(monitor_revision_get),
        )
        .route(
            "/api/monitors/{id}/revisions/{rev}/restore",
            post(monitor_revision_restore),
        )
        .route(
            "/api/monitors/{id}/shares",
            get(monitor_shares_get).put(monitor_shares_put),
        )
        .route(
            "/api/monitors/{id}/owner",
            axum::routing::put(monitor_owner_put),
        )
        // Config, Policy, Engagement & Memory API (DW-425)
        .route("/api/config", get(crate::api_handlers::config::config_get).put(crate::api_handlers::config::config_put))
        .route("/api/config/schema", get(crate::api_handlers::config::config_schema))
        .route("/api/config/raw", get(config_raw_get).put(config_raw_put))
        // Config overlay (revisions-and-config-overlay design, §12–§13)
        .route("/api/config/overlay", get(config_overlay_get).put(config_overlay_put))
        .route("/api/config/effective", get(config_effective_get))
        .route("/api/config/export", get(config_export_get))
        .route("/api/config/overlay/revert", post(config_overlay_revert))
        .route("/api/config/overlay/clear", post(config_overlay_clear))
        .route(
            "/api/policy/rules",
            get(crate::api_handlers::policy::policy_list_rules).post(crate::api_handlers::policy::policy_create_rule),
        )
        .route(
            "/api/policy/rules/{name}",
            axum::routing::put(crate::api_handlers::policy::policy_update_rule).delete(crate::api_handlers::policy::policy_delete_rule),
        )
        .route(
            "/api/policy/proposals",
            get(crate::api_handlers::policy::policy_list_proposals).post(crate::api_handlers::policy::policy_create_proposal),
        )
        .route(
            "/api/policy/proposals/{id}/approve",
            post(crate::api_handlers::policy::policy_approve_proposal),
        )
        .route(
            "/api/policy/proposals/{id}/reject",
            post(crate::api_handlers::policy::policy_reject_proposal),
        )
        // Permission cascade — read + write API (phases 5 & 6)
        .route(
            "/api/permissions/rules",
            get(crate::api_handlers::permissions::list_rules)
                .post(crate::api_handlers::permissions::create_rule),
        )
        .route(
            "/api/permissions/rules/{id}",
            axum::routing::put(crate::api_handlers::permissions::update_rule)
                .delete(crate::api_handlers::permissions::delete_rule),
        )
        .route(
            "/api/permissions/cascade",
            get(crate::api_handlers::permissions::cascade),
        )
        .route(
            "/api/permissions/roles",
            get(crate::api_handlers::permissions::list_roles)
                .post(crate::api_handlers::permissions::create_role),
        )
        .route(
            "/api/permissions/roles/{id}",
            axum::routing::put(crate::api_handlers::permissions::update_role)
                .delete(crate::api_handlers::permissions::delete_role),
        )
        .route(
            "/api/permissions/roles/{role_id}/rules",
            get(crate::api_handlers::permissions::list_rules_for_role)
                .post(crate::api_handlers::permissions::grant_rule_to_role),
        )
        .route(
            "/api/permissions/roles/{role_id}/rules/{rule_id}",
            axum::routing::delete(crate::api_handlers::permissions::revoke_rule_from_role),
        )
        .route(
            "/api/permissions/identities/{identity_id}/roles",
            get(crate::api_handlers::permissions::list_identity_roles)
                .post(crate::api_handlers::permissions::assign_role_to_identity),
        )
        .route(
            "/api/permissions/identities/{identity_id}/roles/{role_id}",
            axum::routing::delete(crate::api_handlers::permissions::unassign_role_from_identity),
        )
        .route(
            "/api/permissions/audit",
            get(crate::api_handlers::permissions::list_audit),
        )
        .route(
            "/api/capabilities/matrix",
            get(crate::api_handlers::permissions::capabilities_matrix)
                .put(crate::api_handlers::permissions::save_capabilities_matrix),
        )
        .route(
            "/api/capabilities/audit",
            get(crate::api_handlers::permissions::capabilities_audit),
        )
        .route(
            "/api/capabilities/catalog",
            get(crate::api_handlers::permissions::capabilities_catalog),
        )
        .route(
            "/api/permissions/effective",
            get(crate::api_handlers::permissions::effective_permission),
        )
        .route(
            "/api/roles",
            get(crate::api_handlers::permissions::list_all_roles)
                .post(crate::api_handlers::permissions::create_custom_role),
        )
        .route(
            "/api/roles/{id}",
            axum::routing::put(crate::api_handlers::permissions::update_custom_role)
                .delete(crate::api_handlers::permissions::delete_custom_role),
        )
        .route(
            "/api/engagement/modes",
            get(crate::api_handlers::engagement::engagement_list_modes)
                .put(crate::api_handlers::engagement::engagement_set_mode)
                .delete(crate::api_handlers::engagement::engagement_delete_mode),
        )
        .route("/api/agents/{id}/memory/files", get(crate::api_handlers::memory::memory_list_files))
        .route(
            "/api/agents/{id}/memory/files/{*path}",
            get(crate::api_handlers::memory::memory_read_file).put(crate::api_handlers::memory::memory_write_file),
        )
        .route("/api/agents/{id}/memory/tasks", get(crate::api_handlers::memory::memory_list_tasks))
        .route("/api/agents/{id}/memory/stats", get(crate::api_handlers::memory::memory_stats))
        .route("/api/agents/{id}/memory/summary", get(crate::api_handlers::memory::memory_summary))
        // Auth endpoints (DW-466)
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/refresh", post(auth_refresh))
        // Public auth config — tells the frontend which login methods are available.
        .route("/api/auth/config", get(auth_config))
        // OIDC SSO — only functional when an OIDC provider is configured.
        .route("/api/auth/oidc/login", get(oidc_login))
        .route("/api/auth/oidc/callback", get(oidc_callback))
        // Google OAuth web-redirect callback — shares a process-wide
        // `OnceLock` store with `GoogleSetupTool` via `google_web_flow_store()`.
        // Only reachable in practice when `skills/google/oauth_client/redirect_uri`
        // is set in the vault; otherwise the agent uses device flow and never
        // creates entries in the store.
        .route(
            "/api/auth/google/callback",
            get(crate::google_oauth_callback::google_oauth_callback),
        )
        // FreeAgent OAuth web-redirect callback — shares a process-wide
        // `OnceLock` store with `FreeAgentSetupTool` via `freeagent_state_store()`.
        .route(
            "/api/auth/freeagent/callback",
            get(crate::freeagent_oauth_callback::freeagent_oauth_callback),
        )
        .route(
            "/api/auth/spotify/callback",
            get(crate::spotify_oauth_callback::spotify_oauth_callback),
        )
        // Identity & Team management (DW-472, DW-473)
        .route("/api/identities", get(crate::api_handlers::identity::list_identities).post(crate::api_handlers::identity::create_identity))
        .route("/api/identities/{id}", get(crate::api_handlers::identity::get_identity).put(crate::api_handlers::identity::update_identity).delete(crate::api_handlers::identity_lifecycle::delete_with_defaults))
        .route("/api/identities/{id}/holdings", get(crate::api_handlers::identity_lifecycle::holdings))
        .route("/api/identities/{id}/suspend", post(crate::api_handlers::identity_lifecycle::suspend))
        .route("/api/identities/{id}/resume", post(crate::api_handlers::identity_lifecycle::resume))
        .route("/api/identities/{id}/merge", post(crate::api_handlers::identity_lifecycle::merge))
        .route("/api/identities/{id}/transfer", post(crate::api_handlers::identity_lifecycle::transfer))
        .route("/api/identities/{id}/delete", post(crate::api_handlers::identity_lifecycle::delete))
        .route("/api/identity-lifecycle/operations", get(crate::api_handlers::identity_lifecycle::operations))
        .route("/api/identity-lifecycle/operations/{id}", get(crate::api_handlers::identity_lifecycle::operation))
        .route("/api/identity-lifecycle/orphans", get(crate::api_handlers::identity_lifecycle::orphans))
        .route("/api/identity-lifecycle/orphans/{id}/transfer", post(crate::api_handlers::identity_lifecycle::transfer_orphan))
        .route("/api/identity-lifecycle/orphans/{id}/delete", post(crate::api_handlers::identity_lifecycle::delete_orphan))
        .route("/api/identities/{id}/elevate", post(crate::api_handlers::identity::elevate_identity))
        .route("/api/identities/{id}/set-password", post(crate::api_handlers::identity::set_password))
        .route("/api/auth/link-device", post(crate::api_handlers::identity::link_device))
        .route("/api/identity/teams", get(crate::api_handlers::identity::list_teams).post(crate::api_handlers::identity::create_team))
        .route("/api/identity/teams/{id}", get(crate::api_handlers::identity::get_team).put(crate::api_handlers::identity::update_team).delete(crate::api_handlers::identity::delete_team))
        .route("/api/identity/teams/{id}/members", post(crate::api_handlers::identity::add_team_member))
        .route("/api/identity/teams/{id}/members/{member_id}", axum::routing::delete(crate::api_handlers::identity::remove_team_member))
        .route("/api/identity/teams/{id}/join-requests", post(crate::api_handlers::identity::request_join_team))
        .route("/api/identity/teams/{id}/join-requests/{applicant_id}/approve", post(crate::api_handlers::identity::approve_join_request))
        .route("/api/identity/teams/{id}/join-requests/{applicant_id}/reject", post(crate::api_handlers::identity::reject_join_request))
        // Owner-scoped scheduler API (members see own + public; admins all)
        // ── Lessons-learned admin API (SCOPE_ADMIN on every route) ──
        .route("/api/lessons", get(crate::api_handlers::lessons::list_lessons))
        .route(
            "/api/lessons/metrics",
            get(crate::api_handlers::lessons::lessons_metrics),
        )
        .route(
            "/api/lessons/recurrences",
            get(crate::api_handlers::lessons::lessons_recurrences),
        )
        .route(
            "/api/lessons/cleanup",
            post(crate::api_handlers::lessons::lessons_cleanup),
        )
        .route(
            "/api/lessons/backfill",
            post(crate::api_handlers::lessons::lessons_backfill),
        )
        .route(
            "/api/lessons/jobs/{id}",
            get(crate::api_handlers::lessons::lessons_job_status),
        )
        .route(
            "/api/lessons/{id}",
            axum::routing::delete(crate::api_handlers::lessons::delete_lesson),
        )
        .route("/api/schedules", get(schedules_list).post(schedule_create))
        .route(
            "/api/schedules/{id}",
            get(schedule_get).put(schedule_update).delete(schedule_delete),
        )
        .route("/api/schedules/{id}/run", post(schedule_run))
        .route("/api/schedules/{id}/history", get(schedule_history))
        // Per-user settings (DW-505)
        .route("/api/me", get(get_me))
        .route(
            "/api/me/picture",
            get(crate::api_handlers::pictures::my_picture_get)
                .put(crate::api_handlers::pictures::my_picture_put)
                .delete(crate::api_handlers::pictures::my_picture_delete)
                .layer(axum::extract::DefaultBodyLimit::max(PICTURE_BODY_LIMIT)),
        )
        .route("/api/me/capabilities", get(get_my_capabilities))
        .route(
            "/api/me/relationship",
            get(crate::api_handlers::relationship::my_relationship),
        )
        .route("/api/me/integrations", get(crate::api_handlers::my_settings::get_integrations))
        .route("/api/me/vault", get(crate::api_handlers::my_settings::list_user_vault))
        .route("/api/me/secret", post(crate::api_handlers::my_settings::set_user_secret).delete(crate::api_handlers::my_settings::delete_user_secret))
        .route("/api/me/vault/move", post(crate::api_handlers::my_settings::move_user_vault))
        .route("/api/me/vault/{*path}", post(crate::api_handlers::my_settings::set_user_vault).delete(crate::api_handlers::my_settings::delete_user_vault))
        .route("/api/me/subscriptions", get(crate::api_handlers::subscriptions::my_subscriptions))
        .route("/api/me/subscriptions/{kind}/login", post(crate::api_handlers::subscriptions::save_login).delete(crate::api_handlers::subscriptions::remove_login))
        .route("/api/me/subscriptions/chatgpt/device", post(crate::api_handlers::subscriptions::start_device_login).get(crate::api_handlers::subscriptions::current_device_login))
        .route("/api/me/subscriptions/chatgpt/device/{flow_id}", get(crate::api_handlers::subscriptions::get_device_login).delete(crate::api_handlers::subscriptions::cancel_device_login))
        .route("/api/me/remote-mcp-tokens", get(crate::api_handlers::remote_mcp_tokens::list_my_tokens).post(crate::api_handlers::remote_mcp_tokens::create_my_token))
        .route("/api/me/remote-mcp-tokens/{id}", axum::routing::delete(crate::api_handlers::remote_mcp_tokens::revoke_my_token))
        .route("/api/remote-mcp-tokens", get(crate::api_handlers::remote_mcp_tokens::list_all_tokens))
        .route("/api/remote-mcp-tokens/{id}", axum::routing::delete(crate::api_handlers::remote_mcp_tokens::revoke_any_token))
        // ── Agent training corpora (DW-539) ──
        .route(
            "/api/agents/{id}/training/corpora",
            post(crate::training_handlers::upload_corpus)
                .get(crate::training_handlers::list_corpora)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::training_handlers::TRAINING_UPLOAD_BODY_LIMIT,
                )),
        )
        .route(
            "/api/agents/{id}/training/corpora/{name}",
            get(crate::training_handlers::get_corpus)
                .delete(crate::training_handlers::delete_corpus),
        )
        .route(
            "/api/agents/{id}/training/documents/{doc_id}",
            get(crate::training_handlers::get_document)
                .delete(crate::training_handlers::delete_document),
        )
        .route(
            "/api/agents/{id}/training/documents/{doc_id}/download",
            get(crate::training_handlers::download_document),
        )
        // ── Agent self-learning (plans + curated items) ──
        .route(
            "/api/agents/{id}/self-learning/plans",
            get(crate::self_learning_handlers::list_plans)
                .post(crate::self_learning_handlers::upsert_plan),
        )
        .route(
            "/api/agents/{id}/self-learning/plans/{plan_id}",
            get(crate::self_learning_handlers::get_plan)
                .delete(crate::self_learning_handlers::delete_plan),
        )
        .route(
            "/api/agents/{id}/self-learning/plans/{plan_id}/run",
            post(crate::self_learning_handlers::run_plan),
        )
        .route(
            "/api/agents/{id}/self-learning/items",
            get(crate::self_learning_handlers::list_items),
        )
        .route(
            "/api/agents/{id}/self-learning/items/{item_id}/approve",
            post(crate::self_learning_handlers::approve_item),
        )
        .route(
            "/api/agents/{id}/self-learning/items/{item_id}/reject",
            post(crate::self_learning_handlers::reject_item),
        )
        .route(
            "/api/agents/{id}/self-learning/items/{item_id}/reverify",
            post(crate::self_learning_handlers::reverify_item),
        )
        .route(
            "/api/agents/{id}/self-learning/items/{item_id}",
            axum::routing::delete(crate::self_learning_handlers::delete_item),
        )
        .route(
            "/api/agents/{id}/settings",
            get(crate::api_handlers::settings::effective_settings),
        )
        // ── Agent know-how ──
        .route(
            "/api/agents/{id}/know-how",
            get(crate::api_handlers::know_how::list_entries),
        )
        .route(
            "/api/agents/{id}/know-how/{entry}/pin",
            post(crate::api_handlers::know_how::pin_entry),
        )
        .route(
            "/api/agents/{id}/know-how/{entry}/retire",
            post(crate::api_handlers::know_how::retire_entry),
        )
        .route(
            "/api/agents/{id}/know-how/{entry}",
            axum::routing::delete(crate::api_handlers::know_how::delete_entry),
        )
        .route(
            "/api/identities/{id}/know-how",
            axum::routing::delete(crate::api_handlers::know_how::erase_identity),
        )
        // ── Deep-expertise knowledge tree ──
        .route(
            "/api/agents/{id}/deep-expertise/nodes",
            get(crate::self_learning_handlers::list_nodes),
        )
        .route(
            "/api/agents/{id}/deep-expertise/nodes/{node_id}",
            get(crate::self_learning_handlers::get_node)
                .delete(crate::self_learning_handlers::delete_node),
        )
        .route(
            "/api/agents/{id}/deep-expertise/nodes/{node_id}/approve",
            post(crate::self_learning_handlers::approve_node),
        )
        .route(
            "/api/agents/{id}/deep-expertise/nodes/{node_id}/reject",
            post(crate::self_learning_handlers::reject_node),
        )
        // ── Stats / analytics ──
        .route("/api/stats/overview", get(crate::api_handlers::stats::get_overview))
        .route("/api/stats/cost/breakdown", get(crate::api_handlers::stats::get_cost_breakdown))
        .route("/api/stats/identities", get(crate::api_handlers::stats::list_identities))
        .route("/api/stats/identities/{id}", get(crate::api_handlers::stats::get_identity_stats))
        .route("/api/stats/identities/{id}/tasks", get(crate::api_handlers::stats::get_identity_tasks))
        .route("/api/stats/agents", get(crate::api_handlers::stats::list_agents))
        .route("/api/stats/agents/{id}", get(crate::api_handlers::stats::get_agent_stats))
        .route("/api/stats/agents/{id}/tasks", get(crate::api_handlers::stats::get_agent_tasks))
        .route("/api/stats/tasks/{task_id}", get(crate::api_handlers::stats::get_task_trace))
        .route("/api/stats/models", get(crate::api_handlers::stats::get_models))
        .route("/api/stats/tools", get(crate::api_handlers::stats::get_tools))
        .route("/api/stats/efficiency", get(crate::api_handlers::stats::get_efficiency))
        .route("/api/budget/status", get(crate::api_handlers::budget::get_status))
        .route("/api/budget/overrides", axum::routing::put(crate::api_handlers::budget::put_override))
        .route("/api/budget/overrides/clear", axum::routing::post(crate::api_handlers::budget::clear_override))
        .with_state(Arc::clone(&state))
        .merge(desk_routes(state))
        .fallback(static_fallback)
        // Audit F2 (2026-05-15).
        .layer(axum::middleware::from_fn(
            crate::security::security_headers_middleware,
        ))
        // Audit F3 (2026-05-15); per-route override on /api/sessions/{id}/upload.
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
}

/// Desk's routes beside the gateway's: its hub link and its conversations.
/// They read conversations from the Postgres-backed hub, so a hub without
/// it serves neither.
fn desk_routes(state: Arc<GatewayState>) -> Router {
    #[cfg(feature = "postgres")]
    {
        crate::desk_conversations::router(Arc::clone(&state)).merge(
            Router::new()
                .route("/ws/desk", get(crate::desk_surface::link::handler))
                .with_state(state),
        )
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = state;
        Router::new()
    }
}

/// A 403 for a connection whose person is suspended. Not a 401: the
/// credential is good, and a client counting 401s would stop reconnecting
/// for good, where this one connects again once the person is reactivated.
fn suspended_refusal(
    state: &GatewayState,
    identity_id: Option<&str>,
) -> Option<axum::response::Response> {
    let person = crate::standing::suspended(state.identity_store.as_ref(), identity_id?)?;
    Some(
        (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "suspended",
                "message": crate::standing::refusal(&person),
            })),
        )
            .into_response(),
    )
}

/// Who a WebSocket handshake's token says is connecting: a paired device
/// (and the person it is linked to), or a signed-in person.
#[derive(Debug, Clone)]
pub(crate) struct WsCaller {
    pub(crate) device_id: Option<String>,
    pub(crate) scopes: Option<Vec<String>>,
    pub(crate) identity_id: Option<String>,
    /// Which slot of a device's token chain matched, for `connected`.
    pub(crate) token_match: Option<String>,
    pub(crate) identity_role: String,
}

/// Authenticate a WebSocket handshake's `token`: a device pairing token, then
/// a JWT. The refusal is the response the upgrade is answered with: 403 for a
/// suspended person, 503 when the pairing store could not be read, else 401.
pub(crate) async fn authenticate_ws(
    state: &GatewayState,
    token: &str,
) -> Result<WsCaller, axum::response::Response> {
    // The admin token names nobody, so it never opens a WebSocket; it stays
    // valid for REST APIs, MCP and admin endpoints.
    let mut pairing_store_error = false;

    if let Some(ref pairing) = state.device_pairing {
        match pairing
            .validate_token_with_match_async(token.to_string())
            .await
        {
            Ok(Some((device, kind))) => {
                let identity_id = state
                    .identity_store
                    .as_ref()
                    .and_then(|store| identity_for_device(store.as_ref(), &device.id));
                if let Some(refused) = suspended_refusal(state, identity_id.as_deref()) {
                    return Err(refused);
                }
                return Ok(WsCaller {
                    device_id: Some(device.id),
                    scopes: Some(device.scopes),
                    identity_id,
                    token_match: Some(
                        match kind {
                            workforce_pairing::TokenMatch::Current => "current",
                            workforce_pairing::TokenMatch::Previous => "previous",
                        }
                        .to_string(),
                    ),
                    // Paired devices are the primary user → Owner trust, no wrap.
                    identity_role: "manager".to_string(),
                });
            }
            Ok(None) => {}
            Err(e) => {
                // Store failure ≠ bad token, and the JWT strategy below has
                // no dependency on the pairing store — a valid JWT must still
                // connect. Record the failure and only surface it after JWT
                // has had its chance.
                warn!(
                    token_prefix = %token_prefix_hint(token),
                    error = %e,
                    "Pairing store error during WebSocket auth — failing closed"
                );
                pairing_store_error = true;
            }
        }
    }

    if let Some(ref jwt_config) = state.jwt_config {
        if let Ok(claims) = crate::jwt::validate_token(jwt_config, token) {
            if let Some(refused) = suspended_refusal(state, claims.identity_id.as_deref()) {
                return Err(refused);
            }
            let identity_id = claims.identity_id.clone();
            let legacy_unscoped = claims.scopes.is_empty();
            let claims = crate::jwt::current_claims(state.identity_store.as_ref(), claims);
            return Ok(WsCaller {
                device_id: claims.device_id,
                scopes: (!legacy_unscoped).then_some(claims.scopes),
                identity_id,
                token_match: None,
                identity_role: claims.role.as_str().to_string(),
            });
        }
    }

    if pairing_store_error {
        // The token wasn't a valid JWT and the pairing store couldn't be
        // consulted, so its validity is unknown. A 401 here would make the
        // iOS client count an auth strike (three disable reconnection);
        // 503 keeps it on the plain retry-with-backoff path.
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "service_unavailable",
                "message": "Authentication backend unavailable — retry shortly."
            })),
        )
            .into_response());
    }

    warn!(
        token_prefix = %token_prefix_hint(token),
        "WebSocket auth failed — invalid or expired token"
    );

    Err((
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": "unauthorized",
            "message": "Authentication required. Provide a valid gateway token (?token=...) or pair your device via /api/pair/device/request."
        })),
    )
        .into_response())
}

/// WebSocket upgrade handler.
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    // The conversation this connection anchors on: iOS when the client names
    // no channel, otherwise the conversational channel it names.
    let anchor_channel = match query.channel.as_deref() {
        None => IOS_CHANNEL.to_string(),
        Some(channel) if workforce_common::channels::is_conversational(channel) => {
            channel.to_string()
        }
        Some(channel) => return invalid_ws_channel(channel),
    };
    let rich = workforce_common::rich::RichFeatures::from_names(query.rich.as_deref());
    let accept =
        workforce_gateway_client::protocol::MediaAccept::from_names(query.accept.as_deref());
    let declared =
        query.desk == Some(1) && workforce_common::channels::is_desk_conversation(&anchor_channel);
    let caller = match authenticate_ws(&state, query.token.as_deref().unwrap_or_default()).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let agent_id = query.agent_id;
    ws.on_upgrade(move |socket| {
        handle_ws(
            socket,
            state,
            anchor_channel,
            rich,
            accept,
            declared,
            agent_id,
            caller,
        )
    })
    .into_response()
}

/// Whether the broadcast forwarder should deliver `msg` on a connection.
///
/// Targeted messages (voice announcements) are routed ONLY by their target:
/// the connection must have registered the same voice channel via
/// `voice_listen` and pass the identity gate. In particular they never ride
/// the "session-less passes to everyone" path, which would read one user's
/// reminder to every connected client.
///
/// A message addressed to a person alone (a device request) reaches every
/// connection of that person — `conn_owner`, the identity or else the device
/// its sessions are owned by — and no other. A Desk delivery signal is for
/// the person's hub links (`/ws/desk`) and never reaches a chat connection.
fn forward_admits(
    msg: &BroadcastMessage,
    conn_session: Option<&str>,
    conn_voice_channel: Option<&str>,
    conn_identity: Option<&str>,
    conn_owner: Option<&str>,
) -> bool {
    if msg.for_links_only() {
        return false;
    }
    match (
        msg.target_channel.as_deref(),
        msg.target_identity.as_deref(),
    ) {
        (Some(target), Some(target_identity)) => {
            conn_voice_channel == Some(target)
                && crate::voice_announce::identity_admits(conn_identity, target_identity)
        }
        // No owner on a targeted message means nothing can be said about who
        // may hear it, so nobody does — a targeted broadcast must never fall
        // back to the "session-less reaches everyone" path.
        (Some(_), None) => false,
        (None, Some(owner)) => conn_owner == Some(owner),
        (None, None) => msg.session_id.is_empty() || conn_session == Some(msg.session_id.as_str()),
    }
}

/// What a Desk connection is told it may use: structured turns, and the
/// hub link with its conversation drain.
const DESK_PLACE: &str = "desk.place";
const DESK_STREAM: &str = "desk.stream";
const DESK_DELIVERIES: &str = "desk.deliveries";

/// Frames only a Desk connection is sent; any other connection is sent the
/// frames it always was.
const DESK_ONLY_FRAMES: &[&str] = &[
    crate::broadcast::TURN_END,
    crate::broadcast::TURN_SNAPSHOT,
    crate::broadcast::DESK_DELIVERED,
];

/// What a declared Desk connection is offered, on its `connected`. The hub
/// link and the drain are for a person's conversations and need the
/// conversation routes, so only a connection that is a person's, on a hub
/// that serves them, is offered deliveries; and the link's floor and usage
/// signals where the hub keeps a floor.
pub(crate) fn desk_features(of_a_person: bool) -> Vec<String> {
    let deliveries = cfg!(feature = "postgres") && of_a_person;
    let signals = deliveries.then(floor_signals).into_iter().flatten();
    [DESK_PLACE, DESK_STREAM]
        .into_iter()
        .chain(deliveries.then_some(DESK_DELIVERIES))
        .chain(signals)
        .map(str::to_string)
        .collect()
}

/// The features of the link's signals, when this hub keeps a floor.
fn floor_signals() -> Vec<&'static str> {
    #[cfg(feature = "postgres")]
    if crate::floor::installed().is_some() {
        use workforce_common::channels::desk::DeskSignal;
        return [DeskSignal::Floor, DeskSignal::Usage]
            .map(DeskSignal::feature)
            .to_vec();
    }
    Vec::new()
}

/// The Desk turn a chat frame is: the one a declared Desk connection sent,
/// checked against the channel it is anchored on, else the one an older
/// Desk's message on a Desk channel implies. `None` off Desk. The refusal is
/// what the client is told.
fn desk_turn_of(
    declared: bool,
    sent: Option<DeskTurn>,
    anchor_channel: &str,
    turn_ref: Option<&str>,
) -> Result<Option<DeskTurn>, String> {
    if turn_ref
        .is_some_and(|r| r.chars().count() > workforce_gateway_client::protocol::TURN_REF_MAX)
    {
        return Err(format!(
            "turn_ref is longer than {} characters",
            workforce_gateway_client::protocol::TURN_REF_MAX
        ));
    }
    match sent.filter(|_| declared) {
        Some(turn) => turn
            .validated(anchor_channel)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(DeskTurn::legacy(anchor_channel)),
    }
}

/// The `turn_ref` a frame that is not a [`ClientMessage`] still names, so
/// the turn it would have started can be ended.
fn unread_turn_ref(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("turn_ref")?
        .as_str()
        .filter(|r| {
            !r.is_empty() && r.chars().count() <= workforce_gateway_client::protocol::TURN_REF_MAX
        })
        .map(str::to_string)
}

/// The frame a connection is sent for `msg`. A connection that did not
/// declare itself Desk's (iOS, the web UI, an old Desk) gets exactly the
/// frame it always did: no turn ref, no reply id, and none of Desk's own
/// frames.
fn server_frame(msg: BroadcastMessage, declared: bool) -> Option<ServerMessage> {
    if !declared && DESK_ONLY_FRAMES.contains(&msg.msg_type.as_str()) {
        return None;
    }
    let id = msg
        .id
        .filter(|_| declared || msg.msg_type == crate::broadcast::USER_MESSAGE);
    let snapshot = msg.snapshot.filter(|_| declared);
    Some(ServerMessage {
        content: Some(msg.content),
        session_id: (!msg.session_id.is_empty()).then_some(msg.session_id),
        error: None,
        agent_id: msg.agent_id,
        context_tokens: msg.context_tokens,
        context_max: msg.context_max,
        team_id: None,
        sender_role: None,
        next_speaker: None,
        speakable: msg.speakable,
        token_match: None,
        request_id: msg.request_id,
        tool_name: msg.tool_name,
        prompt_options: msg.prompt_options,
        attachments: msg.attachments,
        code_citations: msg.code_citations,
        connect_prompt: msg.connect_prompt,
        route: msg.route,
        desk: msg.desk,
        id,
        author: msg.author,
        features: None,
        turn_ref: msg.turn_ref.filter(|_| declared),
        status: snapshot.as_ref().map(|s| s.status),
        since: snapshot.as_ref().map(|s| s.since.to_rfc3339()),
        text: snapshot.map(|s| s.text),
        msg_type: msg.msg_type,
    })
}

#[cfg(test)]
mod desk_frame_tests {
    use super::*;

    fn stamped(msg: BroadcastMessage) -> BroadcastMessage {
        BroadcastMessage {
            turn_ref: Some("ref-1".into()),
            id: Some("reply-1".into()),
            ..msg
        }
    }

    fn sent(msg: BroadcastMessage, declared: bool) -> Option<String> {
        server_frame(msg, declared).map(|frame| serde_json::to_string(&frame).unwrap())
    }

    /// What iOS, the web UI and an old Desk were sent before Desk had frames
    /// of its own, byte for byte, whatever a turn now carries for Desk.
    #[test]
    fn an_undeclared_connection_is_sent_exactly_what_it_always_was() {
        let prompt = BroadcastMessage::permission_prompt(
            "s1",
            "scout",
            "r1",
            "slack.send",
            "Post to a channel",
            workforce_core::policy::PermissionPromptOptions::ask(true),
        );
        let image = BroadcastMessage::image(
            "s1",
            Some("scout".into()),
            Some("a chart".into()),
            vec![Attachment {
                filename: "c.png".into(),
                mime_type: "image/png".into(),
                data: "iVBO".into(),
                ..Attachment::default()
            }],
        );
        let user_message = BroadcastMessage {
            id: Some("m1".into()),
            author: Some(serde_json::json!({"kind": "you", "id": "you", "name": "You"})),
            ..BroadcastMessage::new("s1", crate::broadcast::USER_MESSAGE, "hi")
        };
        let golden = [
            (
                BroadcastMessage::silent("s1", "stream_start", ""),
                r#"{"type":"stream_start","content":"","session_id":"s1","speakable":false}"#,
            ),
            (
                BroadcastMessage::new("s1", "stream_delta", "Hel"),
                r#"{"type":"stream_delta","content":"Hel","session_id":"s1"}"#,
            ),
            (
                BroadcastMessage::silent("s1", "stream_end", ""),
                r#"{"type":"stream_end","content":"","session_id":"s1","speakable":false}"#,
            ),
            (
                BroadcastMessage::new("s1", "message", "Done."),
                r#"{"type":"message","content":"Done.","session_id":"s1"}"#,
            ),
            (
                BroadcastMessage::silent("s1", "typing", ""),
                r#"{"type":"typing","content":"","session_id":"s1","speakable":false}"#,
            ),
            (
                prompt,
                r#"{"type":"permission_prompt","content":"Post to a channel","session_id":"s1","agent_id":"scout","speakable":false,"request_id":"r1","tool_name":"slack.send","prompt_options":{"allow_once":true,"deny_once":true,"allow_always":true,"deny_always":true}}"#,
            ),
            (
                image,
                r#"{"type":"image","content":"a chart","session_id":"s1","agent_id":"scout","speakable":false,"attachments":[{"filename":"c.png","mime_type":"image/png","data":"iVBO"}]}"#,
            ),
            (
                BroadcastMessage::new("", "pong", ""),
                r#"{"type":"pong","content":"","session_id":null}"#,
            ),
        ];
        for (msg, expected) in golden {
            assert_eq!(sent(stamped(msg), false).as_deref(), Some(expected));
        }
        assert_eq!(
            sent(user_message, false).as_deref(),
            Some(
                r#"{"type":"user_message","content":"hi","session_id":"s1","id":"m1","author":{"id":"you","kind":"you","name":"You"}}"#
            ),
            "a DM's user_message keeps its id"
        );
        for desk_only in DESK_ONLY_FRAMES {
            assert_eq!(
                sent(BroadcastMessage::new("s1", *desk_only, ""), false),
                None
            );
        }
    }

    #[test]
    fn a_desk_connection_is_sent_the_turn_ref_and_reply_id() {
        let frame = server_frame(
            stamped(BroadcastMessage::new("s1", "stream_delta", "Hel")),
            true,
        )
        .unwrap();
        assert_eq!(
            (frame.turn_ref.as_deref(), frame.id.as_deref()),
            (Some("ref-1"), Some("reply-1"))
        );
        assert!(server_frame(BroadcastMessage::new("s1", "turn_end", ""), true).is_some());
    }

    #[test]
    fn a_snapshot_and_a_turn_end_reach_desk_alone_with_their_ids() {
        use workforce_common::channels::desk::{TurnEnd, TurnPhase, TurnSnapshot};
        let snapshot = BroadcastMessage::turn_snapshot(
            "s1",
            TurnSnapshot {
                stamp: TurnStamp {
                    turn_ref: Some("ref-1".into()),
                    reply_id: "reply-1".into(),
                },
                since: "2026-10-29T09:00:00Z".parse().unwrap(),
                status: TurnPhase::Writing,
                text: "Hel".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(server_frame(snapshot.clone(), true).unwrap()).unwrap(),
            serde_json::json!({
                "type": "turn_snapshot", "content": "", "session_id": "s1", "speakable": false,
                "id": "reply-1", "turn_ref": "ref-1", "status": "writing",
                "since": "2026-10-29T09:00:00+00:00", "text": "Hel",
            })
        );
        assert!(server_frame(snapshot, false).is_none());
        let end = BroadcastMessage::turn_end(
            "s1",
            TurnEnd {
                turn_ref: Some("ref-1".into()),
                reply_id: Some("reply-1".into()),
            },
        );
        assert_eq!(
            serde_json::to_value(server_frame(end.clone(), true).unwrap()).unwrap(),
            serde_json::json!({
                "type": "turn_end", "content": "", "session_id": "s1", "speakable": false,
                "id": "reply-1", "turn_ref": "ref-1",
            })
        );
        assert!(server_frame(end, false).is_none());
    }

    #[test]
    fn a_desk_connection_s_turn_is_checked_or_inferred() {
        let room = format!("desk:{}", uuid::Uuid::from_u128(5));
        let sent = DeskTurn {
            place: DeskPlace::Dm,
            ..DeskTurn::legacy("desk").unwrap()
        };
        assert_eq!(
            desk_turn_of(true, Some(sent.clone()), "desk", Some("ref-1")),
            Ok(Some(sent.clone()))
        );
        assert_eq!(
            desk_turn_of(true, Some(sent.clone()), &room, None),
            Err("the turn's place is not this conversation's".into())
        );
        assert_eq!(
            desk_turn_of(false, Some(sent), &room, None),
            Ok(DeskTurn::legacy(&room)),
            "an undeclared connection's turn is inferred from its channel"
        );
        assert_eq!(desk_turn_of(false, None, "ios", None), Ok(None));
        assert!(desk_turn_of(true, None, "desk", Some(&"r".repeat(65))).is_err());
    }

    #[test]
    fn a_busy_reply_from_the_owning_instance_reads_as_busy() {
        let reply = |applied, refused: Option<&str>| TurnControlReply {
            applied,
            refused: refused.map(str::to_string),
        };
        assert_eq!(applied_by(&reply(true, None)), Applied::Injected);
        assert_eq!(applied_by(&reply(false, Some(REFUSED_BUSY))), Applied::Busy);
        assert_eq!(applied_by(&reply(false, None)), Applied::NotLive);
    }
}

#[cfg(test)]
mod voice_forward_tests {
    use super::*;

    #[test]
    fn targeted_announcements_reach_only_a_matching_voice_connection() {
        let msg = BroadcastMessage::voice_announce("voice-pe", "uid-1", "hi");
        // The registered bridge whose identity matches.
        assert!(forward_admits(
            &msg,
            None,
            Some("voice-pe"),
            Some("uid-1"),
            Some("uid-1")
        ));
        // An ordinary session client never sees it — this is the "session-less
        // passes to everyone" hole the target fields exist to close.
        assert!(!forward_admits(
            &msg,
            Some("sess-1"),
            None,
            Some("uid-1"),
            Some("uid-1")
        ));
        // A bridge on a different channel doesn't.
        assert!(!forward_admits(
            &msg,
            None,
            Some("kitchen"),
            Some("uid-1"),
            Some("uid-1")
        ));
        // A bridge holding another user's identity doesn't.
        assert!(!forward_admits(
            &msg,
            None,
            Some("voice-pe"),
            Some("uid-2"),
            Some("uid-2")
        ));
        // A paired-but-unlinked device doesn't: pairing alone is not
        // authorization for someone's reminders.
        assert!(!forward_admits(
            &msg,
            None,
            Some("voice-pe"),
            None,
            Some("device-1")
        ));
    }

    /// A targeted message that names no owner is undeliverable, not
    /// broadcast-to-all — the one way targeted routing could fail open.
    #[test]
    fn a_targeted_message_without_an_owner_reaches_nobody() {
        let mut orphan = BroadcastMessage::voice_announce("voice-pe", "uid-1", "hi");
        orphan.target_identity = None;
        assert!(!forward_admits(
            &orphan,
            None,
            Some("voice-pe"),
            Some("uid-1"),
            Some("uid-1")
        ));
        assert!(!forward_admits(
            &orphan,
            None,
            Some("voice-pe"),
            None,
            Some("device-1")
        ));
        assert!(!forward_admits(&orphan, Some("sess-1"), None, None, None));
    }

    #[test]
    fn untargeted_broadcasts_keep_their_session_routing() {
        let pong = BroadcastMessage::new("", "pong", "");
        assert!(forward_admits(&pong, None, None, None, None));
        let sess = BroadcastMessage::new("sess-1", "stream_delta", "hi");
        assert!(forward_admits(&sess, Some("sess-1"), None, None, None));
        assert!(!forward_admits(&sess, Some("sess-2"), None, None, None));
        assert!(!forward_admits(&sess, None, None, None, None));
    }

    /// A device request is its person's: it reaches each of their
    /// connections, whatever session it is in, and nobody else's.
    #[test]
    fn a_device_request_reaches_only_its_persons_connections() {
        let msg = BroadcastMessage::node_invoke("uid-1", "{}");
        assert!(forward_admits(
            &msg,
            None,
            None,
            Some("uid-1"),
            Some("uid-1")
        ));
        assert!(forward_admits(
            &msg,
            Some("sess-9"),
            None,
            Some("uid-1"),
            Some("uid-1")
        ));
        assert!(!forward_admits(
            &msg,
            None,
            None,
            Some("uid-2"),
            Some("uid-2")
        ));
        assert!(!forward_admits(
            &msg,
            Some("sess-1"),
            None,
            None,
            Some("device-2")
        ));
        assert!(!forward_admits(&msg, None, None, None, None));

        let signal = BroadcastMessage::desk_delivered(
            "uid-1",
            workforce_common::channels::desk::DeskDelivered {
                agent_id: "scout".into(),
                channel: "desk".into(),
                session_id: "sess-1".into(),
                message_id: "m1".into(),
                kind: workforce_common::channels::desk::DeliveryKind::Job,
                place: workforce_common::channels::desk::DeskPlace::Dm,
                spoken: false,
            },
        );
        assert!(
            !forward_admits(&signal, Some("sess-1"), None, Some("uid-1"), Some("uid-1")),
            "a Desk delivery signal is for the hub link, never a chat connection"
        );
        for signal in [
            workforce_common::channels::desk::DeskSignal::Floor,
            workforce_common::channels::desk::DeskSignal::Usage,
        ] {
            for owner in [Some("uid-1"), None] {
                assert!(
                    !forward_admits(
                        &BroadcastMessage::desk_signal(signal, owner),
                        None,
                        None,
                        Some("uid-1"),
                        Some("uid-1")
                    ),
                    "a floor or usage signal is for the hub link: {signal:?} {owner:?}"
                );
            }
        }

        let unlinked = BroadcastMessage::node_invoke("device-1", "{}");
        assert!(forward_admits(
            &unlinked,
            None,
            None,
            None,
            Some("device-1")
        ));
    }
}

/// The refusal of a `/ws` handshake naming a channel that is not
/// conversational: a 400 — not a 401, which the iOS client counts as an auth
/// strike — rather than quietly joining the iOS conversation.
fn invalid_ws_channel(channel: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({
            "error": "invalid_channel",
            "message": format!(
                "Unknown channel '{channel}'. A WebSocket anchors on one of: {}, \
                 or a desk room as desk:<slug> (1-64 of a-z, 0-9 and -).",
                workforce_common::channels::CONVERSATIONAL_CHANNELS.join(", ")
            ),
        })),
    )
        .into_response()
}

/// The session a connection on `channel` anchors on for `owner` and
/// `agent_id`. A dormant conversation rolls over at resume, so the client is
/// handed a fresh session instead of one whose history would replay
/// weeks-old exchanges.
pub(crate) fn anchor_session(
    service: &AgentService,
    channel: &str,
    owner: &str,
    agent_id: &str,
) -> eyre::Result<workforce_common::session::Session> {
    let session = service.sessions.get_or_create(channel, owner, agent_id)?;
    service.roll_stale_for_resume(channel, session)
}

/// Handle an authenticated WebSocket connection.
async fn handle_ws(
    socket: WebSocket,
    state: Arc<GatewayState>,
    anchor_channel: String,
    rich: workforce_common::rich::RichFeatures,
    accept: workforce_gateway_client::protocol::MediaAccept,
    declared: bool,
    agent_id: Option<String>,
    caller: WsCaller,
) {
    let WsCaller {
        device_id,
        scopes: device_scopes,
        identity_id,
        token_match,
        identity_role,
    } = caller;
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = state.broadcast_tx.subscribe();

    info!("WebSocket client connected");

    // Per-connection session tracking — shared between send/recv tasks so the
    // broadcast forwarder only forwards messages for this client's session.
    let conn_session: Arc<tokio::sync::Mutex<Option<String>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let conn_session_send = conn_session.clone();

    // Voice-bridge channel this connection registered via `voice_listen`,
    // shared the same way so the forwarder can route targeted announcements.
    let conn_voice: Arc<tokio::sync::Mutex<Option<String>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let conn_voice_send = conn_voice.clone();
    let conn_identity_send = identity_id.clone();
    let conn_owner_send = identity_id.clone().or_else(|| device_id.clone());

    let resolved_id = agent_id
        .clone()
        .unwrap_or_else(|| state.registry.default_id().to_string());
    let resolved_id = resolved_id.as_str();

    // Eagerly resolve the connection's anchor session so the client receives
    // a session_id on connect, and so per-message dispatch has a stable
    // fallback. Only resolved for identified users — anonymous connections
    // don't have a deterministic owner.
    let session_owner_ref = identity_id.as_deref().or(device_id.as_deref());
    let initial_session_id = session_owner_ref.and_then(|owner| {
        state.registry.get(resolved_id).and_then(|service| {
            anchor_session(&service, &anchor_channel, owner, resolved_id)
                .ok()
                .map(|s| s.id)
        })
    });
    if let Some(ref sid) = initial_session_id {
        *conn_session.lock().await = Some(sid.clone());
    }
    let anchored_session_id = initial_session_id.clone();

    let conn_agent = agent_id.clone();
    // WELCOME.md is the agent's own directive for greeting a new session,
    // read by the turn loop; it is never client copy.
    let connected_msg = ServerMessage {
        msg_type: "connected".to_string(),
        content: Some(String::new()),
        session_id: initial_session_id,
        error: None,
        agent_id,
        context_tokens: None,
        context_max: None,
        team_id: None,
        sender_role: None,
        next_speaker: None,
        speakable: true,
        token_match,
        request_id: None,
        tool_name: None,
        prompt_options: None,
        attachments: None,
        code_citations: None,
        connect_prompt: None,
        route: None,
        desk: None,
        id: None,
        author: None,
        features: declared.then(|| desk_features(identity_id.is_some())),
        turn_ref: None,
        status: None,
        since: None,
        text: None,
    };
    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&connected_msg).unwrap().into(),
        ))
        .await;

    // A Desk connection hears its session's progress whichever instance runs
    // the turn, and, joining mid-turn, is sent the turn as it stands: the
    // turn's relay puts a snapshot between two of its events, so the deltas
    // after it carry on from its text.
    let mut _watching = None;
    if let (true, Some(sid), Some(service)) = (
        declared,
        anchored_session_id.as_deref(),
        state.registry.get(resolved_id),
    ) {
        if let Some(nats) = state.nats.as_ref() {
            _watching = Some(
                state
                    .progress_forwarders
                    .watch(
                        nats,
                        workforce_nats::subjects::agent_progress(resolved_id, sid),
                        sid,
                        state.broadcast_tx.clone(),
                    )
                    .await,
            );
        }
        steer(state.nats.as_ref(), &service, sid, TurnControl::Snapshot).await;
    }

    // Spawn broadcast forwarder — only forward messages for this connection's session.
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            // Filter: targeted announcements go only to the voice-bridge
            // connection(s) that registered their channel; otherwise only
            // forward messages for this connection's session. Session-less
            // messages (pong, etc.) pass through to everyone.
            let my_session = conn_session_send.lock().await.clone();
            // Only consulted for a targeted message: this loop runs per
            // streamed token, and announcements are rare.
            let my_voice = match msg.target_channel {
                Some(_) => conn_voice_send.lock().await.clone(),
                None => None,
            };
            if !forward_admits(
                &msg,
                my_session.as_deref(),
                my_voice.as_deref(),
                conn_identity_send.as_deref(),
                conn_owner_send.as_deref(),
            ) {
                continue;
            }
            // Only the files this client shows, shaped as it expects them;
            // a frame that only carried files it doesn't is not sent.
            let attachments = match msg.attachments {
                Some(files) => match accept.shape(files) {
                    Some(shown) => Some(shown),
                    None if msg.msg_type == "image" => continue,
                    None => None,
                },
                None => None,
            };
            let Some(server_msg) = server_frame(BroadcastMessage { attachments, ..msg }, declared)
            else {
                continue;
            };
            if sender
                .send(Message::Text(
                    serde_json::to_string(&server_msg).unwrap().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Handle incoming messages
    let registry = Arc::clone(&state.registry);
    let nats = state.nats.clone();
    let broadcast_tx = state.broadcast_tx.clone();
    let node_invoke_pending_recv = Arc::clone(&state.node_invoke_pending);
    let conn_session_recv = conn_session.clone();
    let conn_voice_recv = conn_voice.clone();
    let ws_device_id = device_id;
    let ws_device_scopes = device_scopes;
    let ws_identity_id = identity_id;
    let ws_identity_role = identity_role;
    let ws_identity_store = state.identity_store.clone();

    let mut recv_task = tokio::spawn(async move {
        // Held for the life of the connection; a replacement or the task
        // ending (including abort) drops it and deregisters the listener.
        let mut voice_registration: Option<crate::voice_announce::VoiceListenerGuard> = None;
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    let client_msg: ClientMessage = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("Invalid WS message: {}", e);
                            // A Desk turn that can't be read still ends: its
                            // client is told why, under the ref it sent.
                            if let Some(turn_ref) = unread_turn_ref(&text) {
                                let session =
                                    conn_session_recv.lock().await.clone().unwrap_or_default();
                                let _ = broadcast_tx.send(BroadcastMessage {
                                    turn_ref: Some(turn_ref),
                                    ..BroadcastMessage::new(
                                        session,
                                        "error",
                                        format!("That message couldn't be read: {e}"),
                                    )
                                });
                            }
                            continue;
                        }
                    };
                    // A connection opened before its person was suspended
                    // carries nothing more from them: no message, stop or
                    // approval answer.
                    if ws_identity_id.as_deref().is_some_and(|id| {
                        crate::standing::suspended(ws_identity_store.as_ref(), id).is_some()
                    }) {
                        info!("Closing a WebSocket whose person is suspended");
                        break;
                    }

                    match client_msg.msg_type.as_str() {
                        "chat" => {
                            let turn_ref = client_msg.turn_ref.clone();
                            let refused = |session: String, why: String| BroadcastMessage {
                                turn_ref: turn_ref.clone(),
                                ..BroadcastMessage::new(session, "error", why)
                            };
                            // Scope enforcement for device tokens
                            if let Some(ref scopes) = ws_device_scopes {
                                if !crate::jwt::has_scope(scopes, crate::jwt::SCOPE_CHAT) {
                                    let _ = broadcast_tx.send(refused(
                                        String::new(),
                                        "Access denied: device does not have 'chat' scope".into(),
                                    ));
                                    continue;
                                }
                            }
                            let desk_turn = match desk_turn_of(
                                declared,
                                client_msg.desk,
                                &anchor_channel,
                                turn_ref.as_deref(),
                            ) {
                                Ok(turn) => turn,
                                Err(refusal) => {
                                    let session =
                                        conn_session_recv.lock().await.clone().unwrap_or_default();
                                    let _ = broadcast_tx.send(refused(session, refusal));
                                    continue;
                                }
                            };
                            let raw_content = client_msg.content.unwrap_or_default();
                            // Channel is metadata for downstream tooling only —
                            // session lookup uses session_id, not channel.
                            let channel = client_msg
                                .channel
                                .as_deref()
                                .unwrap_or(&anchor_channel)
                                .to_string();

                            // Sentinel: trust-aware content wrapping
                            let mut gateway_trust: Option<workforce_common::trust::TrustLevel> =
                                None;
                            let content = if ws_device_id.is_none() && ws_device_scopes.is_none() {
                                // Admin token — Owner trust, no wrapping
                                raw_content.clone()
                            } else {
                                // Device token — classify trust level
                                let source = ContentSource::Gateway {
                                    device_id: ws_device_id.clone(),
                                };
                                let ws_role = Some(ws_identity_role.as_str());
                                let trust = classify_trust_with_role(
                                    &source,
                                    None,
                                    &state.sentinel,
                                    true,
                                    ws_role,
                                );
                                tracing::info!(target: "sentinel", trust = %trust, source = "gateway", "Trust classified");
                                gateway_trust = Some(trust);
                                raw_content.clone()
                            };
                            let gateway_trust = crate::desk_surface::hand_off_trust(
                                desk_turn.as_ref(),
                                gateway_trust,
                            );
                            if gateway_trust.is_some_and(|trust| trust < TrustLevel::Owner) {
                                let scan =
                                    workforce_agent::security::external_content::scan_for_injection(
                                        &raw_content,
                                    );
                                if scan.suspicious {
                                    tracing::warn!(
                                        channel = %channel,
                                        score = scan.score,
                                        hostile = scan.hostile,
                                        "Suspicious patterns in gateway message"
                                    );
                                }
                            }

                            let (content, _captured_gateway_secrets) =
                                workforce_common::redact::tokenize_secrets(
                                    &content,
                                    &channel,
                                    workforce_common::secret_marker::SecretSource::Unknown,
                                );

                            // Resolve which agent to talk to
                            let agent_id_str = frame_agent(
                                client_msg.agent_id.as_deref(),
                                conn_agent.as_deref(),
                                &registry,
                            )
                            .to_string();
                            let service = match registry.get(&agent_id_str) {
                                Some(s) => s,
                                None => {
                                    let _ = broadcast_tx.send(refused(
                                        String::new(),
                                        format!("Unknown agent: {}", agent_id_str),
                                    ));
                                    continue;
                                }
                            };

                            let device_surface_id =
                                ws_device_id.as_deref().map(|d| format!("ios_device:{d}"));
                            let surface_id_refs: Vec<&str> =
                                device_surface_id.as_deref().into_iter().collect();
                            let decision =
                                state.access_gate.check(crate::access_gate::AccessRequest {
                                    agent_id: &agent_id_str,
                                    identity_id: ws_identity_id.as_deref(),
                                    surface_ids: &surface_id_refs,
                                    is_external: false,
                                });
                            if let workforce_common::settings::AccessDecision::Deny {
                                message,
                                reason,
                            } = decision
                            {
                                info!(
                                    agent = %agent_id_str,
                                    identity = ?ws_identity_id,
                                    device = ?ws_device_id,
                                    reason = %reason,
                                    "Access denied by agent access policy"
                                );
                                let deny_session = conn_session_recv
                                    .lock()
                                    .await
                                    .clone()
                                    .or_else(|| client_msg.session_id.clone())
                                    .unwrap_or_default();
                                let _ = broadcast_tx.send(refused(
                                    deny_session,
                                    message.unwrap_or_else(|| {
                                        "Access denied by this agent's policy.".to_string()
                                    }),
                                ));
                                continue;
                            }

                            let scope_policy = service
                                .scope_gate
                                .read()
                                .clone()
                                .and_then(|gate| gate.lookup(&agent_id_str));
                            if let Some(scope) = scope_policy {
                                if !content.trim().is_empty() {
                                    let outcome = crate::decide::evaluate_scope(
                                        &scope,
                                        service.llm.as_ref(),
                                        &crate::credentials::Caller::for_service(
                                            &service,
                                            ws_identity_id.clone(),
                                        ),
                                        &service.request_subject(ws_identity_id.clone(), "gateway"),
                                        &content,
                                        "gateway",
                                        &agent_id_str,
                                    )
                                    .await;
                                    if let crate::scope_gate::ScopeOutcome::Reject { canned } =
                                        outcome
                                    {
                                        let deny_session = conn_session_recv
                                            .lock()
                                            .await
                                            .clone()
                                            .or_else(|| client_msg.session_id.clone())
                                            .unwrap_or_default();
                                        let _ = broadcast_tx.send(refused(deny_session, canned));
                                        continue;
                                    }
                                }
                            }

                            let session_owner = match ws_identity_id
                                .as_deref()
                                .or(ws_device_id.as_deref())
                            {
                                Some(o) => o,
                                None => {
                                    let _ = broadcast_tx.send(refused(
                                        String::new(),
                                        "Unauthenticated WebSocket message rejected".into(),
                                    ));
                                    warn!("WS message rejected: no identity or device in scope");
                                    continue;
                                }
                            };

                            // Anchored-session pattern: each WS connection has one
                            // canonical session resolved at connect. A client-supplied
                            // `session_id` is honoured only when the caller owns it;
                            // otherwise we fall back to the anchor instead of
                            // fabricating a new session under a different
                            // (channel, owner) tuple — that fabrication was the bug
                            // that split iOS conversations across multiple sessions.
                            let fallback_session_id = match anchored_session_id.as_deref() {
                                Some(anchor) => anchor.to_string(),
                                None => {
                                    let resolved = anchor_session(
                                        &service,
                                        &anchor_channel,
                                        session_owner,
                                        &agent_id_str,
                                    );
                                    match resolved {
                                        Ok(s) => s.id.clone(),
                                        Err(e) => {
                                            warn!("Failed to resolve fallback session: {}", e);
                                            continue;
                                        }
                                    }
                                }
                            };

                            let effective_session_id = match client_msg
                                .session_id
                                .as_deref()
                                .filter(|s| !s.is_empty())
                            {
                                None => fallback_session_id,
                                // A Desk turn is placed in its connection's
                                // conversation, so it runs in no other.
                                Some(sid) => {
                                    let honoured = owned_session(
                                        service.sessions.as_ref(),
                                        sid,
                                        session_owner,
                                    )
                                    .is_some_and(|s| !declared || s.channel == anchor_channel);
                                    if honoured {
                                        sid.to_string()
                                    } else {
                                        warn!(
                                            requested = sid,
                                            anchor = %fallback_session_id,
                                            owner = session_owner,
                                            "Client requested a session it may not use here — using anchor"
                                        );
                                        fallback_session_id
                                    }
                                }
                            };

                            // Track this connection's active session
                            *conn_session_recv.lock().await = Some(effective_session_id.clone());

                            // Store voice mode / TTS provider in session metadata.
                            //
                            // `voice_mode` latches on: the iOS client sends it
                            // only while its input mode is voice, and text and
                            // speech share one eternal session, so once set it
                            // keeps reading "this is a voice conversation" —
                            // which is what the rollover caps, the code-context
                            // gate and the digest want. `voice_turn` is instead
                            // rewritten on every inbound message, so it reads
                            // "this message was spoken". Model selection keys on
                            // that one, so typing after speaking drops back off
                            // `[llm] voice_model`.
                            let voice_turn = client_msg.voice_mode.unwrap_or(false)
                                || desk_turn.as_ref().is_some_and(|turn| turn.spoken);
                            let desk = desk_turn.map(|turn| {
                                workforce_common::channels::desk::DeskTurnContext::new(
                                    turn,
                                    &anchor_channel,
                                    ws_device_id.as_deref(),
                                    voice_turn,
                                    turn_ref.clone(),
                                    uuid::Uuid::new_v4().to_string(),
                                )
                            });
                            let stamp = desk.as_ref().map(TurnStamp::from);
                            // Which voice BRIDGE (if any) this turn was spoken
                            // on, decided here and carried with the turn. The
                            // session cannot answer it: one WebSocket anchor
                            // session is shared by every surface a person uses
                            // with an agent, so its voice flag says "one of my
                            // turns was spoken", not "this one was". A
                            // scheduled job stamped off the latter would be
                            // permanently mis-addressed. The conversational
                            // channels are excluded — each is a screen the
                            // reply already reaches.
                            let voice_channel = (voice_turn
                                && !workforce_common::channels::is_conversational(&channel))
                            .then(|| channel.clone());
                            if let Ok(Some(mut session)) =
                                service.sessions.get(&effective_session_id)
                            {
                                // What this connection renders, rewritten
                                // per message like `voice_turn`: the session
                                // may last have been spoken to elsewhere.
                                let mut changed = session.set_rendered_rich(rich);
                                let turn_changed = session.is_voice_turn() != voice_turn;
                                if turn_changed
                                    || client_msg.voice_mode.is_some()
                                    || client_msg.tts_provider.is_some()
                                {
                                    // New sessions start with Null metadata — promote to object
                                    if session.metadata.is_null() {
                                        session.metadata = serde_json::json!({});
                                    }
                                    if let Some(obj) = session.metadata.as_object_mut() {
                                        obj.insert(
                                            "voice_turn".into(),
                                            serde_json::json!(voice_turn),
                                        );
                                        if let Some(vm) = client_msg.voice_mode {
                                            obj.insert("voice_mode".into(), serde_json::json!(vm));
                                        }
                                        if let Some(ref tp) = client_msg.tts_provider {
                                            obj.insert(
                                                "tts_provider".into(),
                                                serde_json::json!(tp),
                                            );
                                        }
                                        changed = true;
                                    }
                                }
                                if changed {
                                    let _ = service.sessions.save(&session);
                                }
                            }

                            let answers_approvals = client_msg.answers_approvals.unwrap_or(false);
                            let has_attachments = client_msg
                                .attachments
                                .as_ref()
                                .is_some_and(|a| !a.is_empty());
                            // A follow-up joins the live turn, wherever it runs.
                            // Attachments never do: an injected message is
                            // text-only, so absorbing one would drop its files.
                            // One from another Desk place waits for the live
                            // turn instead, and the client queues it.
                            if !has_attachments {
                                let steered = steer(
                                    nats.as_ref(),
                                    &service,
                                    &effective_session_id,
                                    followup_injection(
                                        &content,
                                        session_owner,
                                        desk.as_ref().map(|d| d.turn.place.clone()),
                                    ),
                                )
                                .await;
                                if steered == Applied::Busy {
                                    let _ = broadcast_tx.send(refused(
                                        effective_session_id.clone(),
                                        REFUSED_BUSY.into(),
                                    ));
                                    continue;
                                }
                                if steered.applied() {
                                    let _ = broadcast_tx.send(BroadcastMessage::new(
                                        effective_session_id.clone(),
                                        "injected",
                                        "Follow-up absorbed into active run".to_string(),
                                    ));
                                    continue;
                                }
                            }

                            // A `typing` opens a model call on clients, so a
                            // follow-up steered into a live call must not send one.
                            let _ = broadcast_tx.send(
                                BroadcastMessage::new(
                                    effective_session_id.clone(),
                                    "typing",
                                    String::new(),
                                )
                                .stamped(stamp.as_ref()),
                            );

                            // Drain gate: reject new tasks during graceful shutdown.
                            if state.drain_token.is_cancelled() {
                                let _ = broadcast_tx.send(
                                    BroadcastMessage::new(
                                        effective_session_id.clone(),
                                        "error",
                                        "Server is shutting down — please reconnect",
                                    )
                                    .stamped(stamp.as_ref()),
                                );
                                continue;
                            }

                            if let Some(ref nats) = nats {
                                // ── NATS mode: publish request, subscribe to progress ──

                                let nats_attachments: Vec<NatsChatAttachment> = client_msg
                                    .attachments
                                    .map(|atts| {
                                        atts.into_iter()
                                            .map(|a| NatsChatAttachment {
                                                filename: a.filename,
                                                mime_type: a.mime_type,
                                                data: a.data,
                                                artifact_id: a.artifact_id,
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();

                                let chat_subject = workforce_nats::subjects::agent_chat(
                                    &agent_id_str,
                                    &effective_session_id,
                                );
                                let display_content = if content != raw_content {
                                    Some(raw_content.clone())
                                } else {
                                    None
                                };
                                let request = NatsChatRequest {
                                    session_id: effective_session_id.clone(),
                                    channel,
                                    user: session_owner.to_string(),
                                    content,
                                    display_content,
                                    attachments: nats_attachments,
                                    voice_channel: voice_channel.clone(),
                                    answers_approvals,
                                    desk: desk.clone(),
                                };
                                let envelope = Envelope::new(&chat_subject, "gateway", request);

                                // Normalize the reply for TTS only when
                                // *this* message was spoken (DW-571). The
                                // session-level flag latches on for the
                                // life of an iOS session, so reading it
                                // here reshaped typed replies for speech
                                // long after the user stopped talking.
                                let tts_normalize = voice_turn;
                                let forwarding = state
                                    .progress_forwarders
                                    .expect(
                                        nats,
                                        workforce_nats::subjects::agent_progress(
                                            &agent_id_str,
                                            &effective_session_id,
                                        ),
                                        &effective_session_id,
                                        broadcast_tx.clone(),
                                        tts_normalize,
                                        stamp.clone(),
                                    )
                                    .await;

                                // Publish the chat request
                                match envelope.to_bytes() {
                                    Ok(bytes) => {
                                        if let Err(e) = nats.publish(chat_subject, bytes).await {
                                            warn!("Failed to publish chat request to NATS: {}", e);
                                            state
                                                .progress_forwarders
                                                .withdraw(&effective_session_id, forwarding);
                                            let _ = broadcast_tx.send(
                                                BroadcastMessage::new(
                                                    effective_session_id.clone(),
                                                    "error",
                                                    format!("Failed to route message: {}", e),
                                                )
                                                .stamped(stamp.as_ref()),
                                            );
                                            continue;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to serialize chat request: {}", e);
                                        state
                                            .progress_forwarders
                                            .withdraw(&effective_session_id, forwarding);
                                        continue;
                                    }
                                }
                            } else {
                                // ── Standalone mode: direct call (no NATS) ──
                                // A Desk turn's frames close with its end,
                                // however it went.
                                let turn_end = |session_id: &str| {
                                    desk.as_ref().map(|desk| {
                                        BroadcastMessage::turn_end(
                                            session_id,
                                            crate::desk_surface::turn_end(
                                                &service, session_id, desk,
                                            ),
                                        )
                                    })
                                };

                                // Convert protocol attachments to session attachments
                                let session_attachments = client_msg.attachments.map(|atts| {
                                    atts.into_iter()
                                        .map(|a| workforce_common::session::SessionAttachment {
                                            filename: a.filename,
                                            mime_type: a.mime_type,
                                            data: a.data,
                                            artifact_id: a.artifact_id,
                                        })
                                        .collect()
                                });

                                let (mut live, inbox) = match service
                                    .begin_live_turn(
                                        &effective_session_id,
                                        SelfServe::answerable_if(answers_approvals),
                                        desk.as_ref(),
                                    )
                                    .await
                                {
                                    Ok(begun) => begun,
                                    Err(busy) => {
                                        let refused = BroadcastMessage::new(
                                            effective_session_id.clone(),
                                            "error",
                                            busy.to_string(),
                                        )
                                        .stamped(stamp.as_ref());
                                        std::iter::once(refused)
                                            .chain(turn_end(&effective_session_id))
                                            .for_each(|frame| {
                                                let _ = broadcast_tx.send(frame);
                                            });
                                        continue;
                                    }
                                };

                                let display = if content != raw_content {
                                    Some(raw_content.as_str())
                                } else {
                                    None
                                };

                                // Process message with progress events
                                match service
                                    .process_message_with_progress(
                                        &channel,
                                        session_owner,
                                        None,
                                        &content,
                                        None,
                                        Some(&effective_session_id),
                                        session_attachments,
                                        None,
                                        None,
                                        None,
                                        Some(inbox),
                                        display,
                                        None, // no ticket_id for gateway
                                        None, // no step_max_rounds for gateway
                                        None, // no step_model for gateway
                                        None, // no context_eviction_interval
                                        None, // no context_eviction_keep_last
                                        None, // no tool_whitelist
                                        None, // no persona override
                                        None, // no workspace_id — gateway is not Slack
                                        gateway_trust,
                                        Some(live.cancel_token()),
                                        voice_channel.as_deref(),
                                        desk.as_ref(),
                                    )
                                    .await
                                {
                                    Ok(progress) => {
                                        let session_id = progress.session_id.clone();
                                        // A sealed session gets redirected to its rollover
                                        // successor inside the run; re-point this connection's
                                        // broadcast filter to the session actually used, or the
                                        // streamed response (tagged with the successor id) is
                                        // dropped. No-op when no rollover occurred.
                                        *conn_session_recv.lock().await = Some(session_id.clone());

                                        // If the run rolled the thread over, tell the client to
                                        // adopt the successor id so future sends and scroll-back
                                        // target the live session directly instead of relying on
                                        // the server-side redirect. Tagged with the successor so it
                                        // passes the (just re-pointed) broadcast filter.
                                        if session_id != effective_session_id {
                                            let _ = broadcast_tx.send(BroadcastMessage::silent(
                                                session_id.clone(),
                                                "session_rollover",
                                                session_id.clone(),
                                            ));
                                        }

                                        let (tx, sid) = (broadcast_tx.clone(), session_id.clone());
                                        // Spoken-turn only — see the NATS
                                        // branch above (DW-571).
                                        let tts_normalize = voice_turn;
                                        let service = Arc::clone(&service);
                                        let (desk, stamp) = (desk.clone(), stamp.clone());
                                        tokio::spawn(async move {
                                            let forward = {
                                                let (tx, sid) = (tx.clone(), sid.clone());
                                                let mentions = mentions_for(&service, &sid);
                                                let stamp = stamp.clone();
                                                move |rx| {
                                                    forward_local_progress(
                                                        rx,
                                                        tx,
                                                        sid,
                                                        tts_normalize,
                                                        mentions,
                                                        stamp,
                                                    )
                                                }
                                            };
                                            let conducted = crate::live_turns::conduct(
                                                &mut live, &service, progress, forward,
                                            )
                                            .await;
                                            let end = desk.as_ref().map(|desk| {
                                                BroadcastMessage::turn_end(
                                                    &sid,
                                                    crate::desk_surface::turn_end(
                                                        &service, &sid, desk,
                                                    ),
                                                )
                                            });
                                            conducted
                                                .closing(&service)
                                                .into_iter()
                                                .filter_map(|progress| {
                                                    chat_progress_to_broadcast(
                                                        progress, &sid, false,
                                                    )
                                                })
                                                .map(|message| message.stamped(stamp.as_ref()))
                                                .chain(end)
                                                .for_each(|message| {
                                                    let _ = tx.send(message);
                                                });
                                            drop(live);
                                        });
                                    }
                                    Err(_) if live.is_cancelled() => {}
                                    Err(e) => {
                                        // A Desk turn's failure is its own
                                        // session's; anyone else's goes as ever.
                                        let session = match desk {
                                            Some(_) => effective_session_id.clone(),
                                            None => String::new(),
                                        };
                                        let failed =
                                            BroadcastMessage::new(session, "error", e.to_string())
                                                .stamped(stamp.as_ref());
                                        std::iter::once(failed)
                                            .chain(turn_end(&effective_session_id))
                                            .for_each(|frame| {
                                                let _ = broadcast_tx.send(frame);
                                            });
                                    }
                                }
                            }
                        }
                        "ping" => {
                            let _ =
                                broadcast_tx.send(BroadcastMessage::new("", "pong", String::new()));
                        }
                        // A voice bridge declaring itself: targeted
                        // announcements (scheduled reminders) for its channel
                        // are forwarded on this connection from now on.
                        "voice_listen" => {
                            let Some(channel) =
                                client_msg.channel.as_deref().filter(|c| !c.is_empty())
                            else {
                                warn!("voice_listen without a channel — ignoring");
                                continue;
                            };
                            *conn_voice_recv.lock().await = Some(channel.to_string());
                            voice_registration.replace(
                                state
                                    .voice_listeners
                                    .register(channel, ws_identity_id.clone()),
                            );
                        }
                        "cancel" => {
                            let Some(sid) = client_msg.session_id.as_deref() else {
                                continue;
                            };
                            let Some(service) = registry.get(frame_agent(
                                client_msg.agent_id.as_deref(),
                                conn_agent.as_deref(),
                                &registry,
                            )) else {
                                continue;
                            };
                            if !ws_identity_id
                                .as_deref()
                                .or(ws_device_id.as_deref())
                                .is_some_and(|owner| {
                                    owns_session(service.sessions.as_ref(), sid, owner)
                                })
                            {
                                warn!(session_id = %sid, "Cancel for a non-owned session — ignoring");
                                continue;
                            }
                            // Silence the stopped turn first: the client has
                            // already finalized it, and its tail would land on
                            // the reply to the message sent next. Awaited
                            // inline, so that message is read only once the
                            // turn has settled.
                            state.progress_forwarders.stop(sid);
                            let settled = steer(nats.as_ref(), &service, sid, TurnControl::Cancel)
                                .await
                                .applied();
                            info!(session_id = %sid, settled, "Cancelling active turn");
                        }
                        "node_invoke_result" => {
                            // Client responding to a node invoke request.
                            // Parse the result and fulfill the pending oneshot.
                            if let Ok(result) = serde_json::from_str::<
                                workforce_gateway_client::protocol::NodeInvokeResult,
                            >(&text)
                            {
                                let invoke_id = result.invoke_id.clone();
                                let sender = ws_identity_id.as_deref().or(ws_device_id.as_deref());
                                if !node_invoke_pending_recv.fulfil(sender, result).await {
                                    warn!(invoke_id = %invoke_id, "No pending invoke of this person's for result");
                                }
                            }
                        }
                        "permission_response" => {
                            // Typed-button reply to a `permission_prompt` event.
                            // Reaches the live turn the same way as the
                            // free-text `approve <id>` flow but
                            // carries the typed PermissionChoice the runner
                            // uses to decide whether to persist a User-scope rule.
                            let request_id = match client_msg.request_id.as_deref() {
                                Some(r) if !r.is_empty() => r.to_string(),
                                _ => continue,
                            };
                            let Some(choice) = client_msg.choice else {
                                continue;
                            };
                            let owner = match ws_identity_id.as_deref().or(ws_device_id.as_deref())
                            {
                                Some(o) => o.to_string(),
                                None => continue,
                            };
                            let target_sid = match client_msg.session_id.clone() {
                                Some(s) => Some(s),
                                None => conn_session_recv.lock().await.clone(),
                            };
                            let Some(sid) = target_sid else { continue };
                            let Some(service) = registry.get(frame_agent(
                                client_msg.agent_id.as_deref(),
                                conn_agent.as_deref(),
                                &registry,
                            )) else {
                                continue;
                            };
                            match crate::prompt_answer::answer(
                                nats.as_ref(),
                                &service,
                                &owner,
                                &sid,
                                &request_id,
                                choice,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(crate::prompt_answer::Unanswered::NotYours) => warn!(
                                    session_id = %sid,
                                    "permission_response for a non-owned session — dropping"
                                ),
                                Err(crate::prompt_answer::Unanswered::NotLive) => warn!(
                                    session_id = %sid,
                                    request_id = %request_id,
                                    "permission_response for inactive session — dropping"
                                ),
                            }
                        }
                        _ => {}
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut send_task => {
            recv_task.abort();
        }
        _ = &mut recv_task => {
            send_task.abort();
        }
    }

    info!("WebSocket client disconnected");
}

/// What a Desk overseer's answer mentions to act on in place, worked out
/// from its text when its `stream_end` / `message` goes out.
pub type Mentions = Arc<
    dyn Fn(String) -> futures::future::BoxFuture<'static, Option<serde_json::Value>> + Send + Sync,
>;

/// The mentions of a turn in `sid` of `service`'s, when it is the Desk
/// overseer answering in its DM.
pub(crate) fn mentions_for(service: &AgentService, sid: &str) -> Option<Mentions> {
    #[cfg(feature = "postgres")]
    {
        crate::desk_crew::mentions_for(service, sid)
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (service, sid);
        None
    }
}

/// Forward NATS [`ChatProgress`] events to the broadcast channel until `Done`.
/// Broadcast a standalone turn's progress to this instance's WebSocket
/// clients, ticking the in-flight tool line while a quiet tool runs.
pub(crate) async fn forward_local_progress(
    mut progress_rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
    tx: broadcast::Sender<BroadcastMessage>,
    sid: String,
    tts_normalize: bool,
    mentions: Option<Mentions>,
    stamp: Option<TurnStamp>,
) {
    // The Desk turn the frames belong to, named again by each snapshot.
    let mut stamp = stamp;
    let mut delivery = crate::chat_worker::AnswerDelivery::default();
    // What has streamed of the answer, for what it mentions.
    let mut said = String::new();
    // The in-flight `tool_start` line the
    // heartbeat ticks while a black-box tool
    // (knowledge.ask) or quiet advisor round
    // emits nothing. Any other broadcast
    // supersedes it, so it never ticks over
    // live streamed output.
    let mut base: Option<String> = None;
    let mut activity_started = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::interval(crate::slack_manager::STATUS_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // skip immediate first tick
    loop {
        let event = tokio::select! {
            maybe = progress_rx.recv() => match maybe {
                Some(ev) => ev,
                None => break,
            },
            _ = heartbeat.tick() => {
                if let Some(b) = base.as_ref() {
                    let _ = tx.send(BroadcastMessage::silent(
                        sid.clone(),
                        "tool_start",
                        crate::slack_manager::render_status(
                            b,
                            activity_started,
                        ),
                    ).stamped(stamp.as_ref()));
                }
                continue;
            }
        };
        delivery.note(&event);
        let broadcast = match event {
            ProgressEvent::ToolStart { name, detail, .. } => {
                let content = if detail.is_empty() {
                    name
                } else {
                    format!("{}: {}", name, detail)
                };
                Some(BroadcastMessage::silent(sid.clone(), "tool_start", content))
            }
            ProgressEvent::ToolEnd {
                name,
                success,
                duration_ms,
                ..
            } => Some(BroadcastMessage::silent(
                sid.clone(),
                "tool_end",
                format!(
                    "{} ({}ms, {})",
                    name,
                    duration_ms,
                    if success { "ok" } else { "failed" }
                ),
            )),
            ProgressEvent::LlmCall { .. } => Some(BroadcastMessage::silent(
                sid.clone(),
                "typing",
                String::new(),
            )),
            ProgressEvent::StreamStart => Some(BroadcastMessage::silent(
                sid.clone(),
                "stream_start",
                String::new(),
            )),
            ProgressEvent::StreamDelta { text } => {
                if mentions.is_some() {
                    said.push_str(&text);
                }
                let content = if tts_normalize {
                    crate::formatting::normalize_delta_for_tts(&text)
                } else {
                    text
                };
                Some(BroadcastMessage::new(sid.clone(), "stream_delta", content))
            }
            ProgressEvent::StreamEnd => {
                let mut end = BroadcastMessage::silent(sid.clone(), "stream_end", String::new());
                if let Some(mentions) = &mentions {
                    end.desk = mentions(said.clone()).await;
                }
                Some(end)
            }
            // Answer complete; the
            // annotations after it stream
            // as ordinary text and this
            // stream is not reconciled
            // against a final answer, so
            // there is nothing to settle.
            ProgressEvent::AnswerSettled => None,
            ProgressEvent::StreamDiscard { reason } => {
                said.clear();
                reason.retracts().then(|| {
                    BroadcastMessage::silent(sid.clone(), "stream_discard", reason.as_str())
                })
            }
            ProgressEvent::Response { ref text } => {
                if !delivery.delivers_response() {
                    // Response was already delivered via
                    // StreamDelta events; skip to avoid
                    // duplicate display on clients.
                    None
                } else {
                    let rendered = workforce_common::secret_marker::render_markers_as_preview(text);
                    let text = if workforce_common::security_scanner::contains_credentials(
                        &rendered,
                    ) {
                        tracing::warn!(target: "sentinel", "Credential detected in response — redacting");
                        workforce_common::security_scanner::redact_credentials(&rendered)
                    } else {
                        rendered
                    };
                    let content = if tts_normalize {
                        crate::formatting::normalize_for_tts(&text)
                    } else {
                        text
                    };
                    // Non-streamed response (e.g. slash
                    // commands) — send as a regular message.
                    let mut message = BroadcastMessage::new(sid.clone(), "message", content);
                    if let Some(mentions) = &mentions {
                        message.desk = mentions(message.content.clone()).await;
                    }
                    Some(message)
                }
            }
            ProgressEvent::CompactStart => Some(BroadcastMessage::new(
                sid.clone(),
                "compact_start",
                String::new(),
            )),
            ProgressEvent::CompactEnd => Some(BroadcastMessage::new(
                sid.clone(),
                "compact_end",
                String::new(),
            )),
            ProgressEvent::ContextUpdate {
                estimated_tokens,
                context_max,
            } => {
                let mut msg = BroadcastMessage::new(sid.clone(), "context_update", String::new());
                msg.context_tokens = Some(estimated_tokens);
                msg.context_max = Some(context_max);
                Some(msg)
            }
            ProgressEvent::ApprovalRequired {
                request_id,
                agent_id,
                tool_name,
                detail: _,
                reason,
                options,
                ..
            } => Some(BroadcastMessage::permission_prompt(
                sid.clone(),
                agent_id,
                request_id,
                tool_name,
                reason,
                options,
            )),
            // A follow-up's `injected` is the gateway's, sent once as it is
            // steered in, as it is for a turn on another instance.
            ProgressEvent::Done { .. } | ProgressEvent::InjectedMessage { .. } => None,
            ProgressEvent::RateLimited { wait_secs, attempt } => Some(BroadcastMessage::new(
                sid.clone(),
                "rate_limited",
                rate_limited_line(wait_secs, attempt),
            )),
            ProgressEvent::SlowModelFailover {
                model,
                elapsed_secs,
            } => Some(BroadcastMessage::new(
                sid.clone(),
                "slow_model_failover",
                slow_model_line(&model, elapsed_secs),
            )),
            ProgressEvent::SubActivity { source, detail } => Some(BroadcastMessage::silent(
                sid.clone(),
                "tool_start",
                format!("{source} ▸ {detail}"),
            )),
            ProgressEvent::SecretConsumed { .. } => None,
            ProgressEvent::ApprovalResolved { .. } => None,
            ProgressEvent::ConnectRequired { prompt } => {
                Some(BroadcastMessage::connect_prompt(sid.clone(), prompt))
            }
            // Periodic progress check-in: deliver as a
            // durable message so gateway/iOS users hear
            // it too. (The Pause button is Slack-only;
            // here a reply interrupts the run.)
            ProgressEvent::CheckIn { note } => {
                Some(BroadcastMessage::new(sid.clone(), "message", note))
            }
            ProgressEvent::Snapshot(snapshot) => {
                stamp = Some(snapshot.stamp.clone());
                Some(BroadcastMessage::turn_snapshot(sid.clone(), snapshot))
            }
            ProgressEvent::File(file) => Some(crate::desk_attachments::frame(
                &sid,
                std::slice::from_ref(&file),
                file.caption.clone(),
            )),
        };

        // Track the heartbeat's in-flight
        // line off the broadcast itself (no
        // re-formatting); only a tool_start
        // is tickable, reset on change.
        match broadcast.as_ref() {
            Some(msg) if msg.msg_type == "tool_start" => {
                if base.as_deref() != Some(msg.content.as_str()) {
                    activity_started = tokio::time::Instant::now();
                }
                base = Some(msg.content.clone());
            }
            _ => base = None,
        }

        if let Some(msg) = broadcast {
            let _ = tx.send(msg.stamped(stamp.as_ref()));
        }
    }
}

/// The NATS progress forwarder of each session on this instance. Every
/// connection shares one broadcast channel, so one forwarder serves all of a
/// session's requests in flight here, ending after one `Done` per request,
/// and the Desk connections watching it, for as long as they watch. It also
/// keeps the ids of the Desk turn the session is streaming, which its frames
/// carry.
#[derive(Default)]
pub struct ProgressForwarders(parking_lot::Mutex<HashMap<String, Forwarder>>);

struct Forwarder {
    generation: u64,
    pending: usize,
    watchers: usize,
    stamp: Option<TurnStamp>,
    task: Option<AbortHandle>,
    /// A stopped turn's tail is not passed on while the session is watched,
    /// until that turn ends or a request is made here.
    silenced: bool,
}

/// Why a forwarder is joined: a request published here, or a Desk
/// connection watching the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    Request,
    Watch,
}

/// A Desk connection's hold on its session's progress: the session's frames
/// reach this instance for as long as it is held, whichever instance's
/// request they answer.
pub(crate) struct WatchGuard {
    forwarders: Arc<ProgressForwarders>,
    session_id: String,
    generation: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.forwarders.unwatch(&self.session_id, self.generation);
    }
}

impl ProgressForwarders {
    /// Count one more request on `session_id`, the Desk turn `stamp` names
    /// if it is one, subscribing a forwarder to `subject` when none runs.
    /// Called before the request is published, so none of its progress can
    /// slip past.
    async fn expect(
        self: &Arc<Self>,
        nats: &async_nats::Client,
        subject: String,
        session_id: &str,
        tx: broadcast::Sender<BroadcastMessage>,
        tts_normalize: bool,
        stamp: Option<TurnStamp>,
    ) -> u64 {
        let generation = self
            .join(nats, subject, session_id, tx, tts_normalize, Join::Request)
            .await;
        self.set_stamp(session_id, stamp);
        generation
    }

    /// Hold `session_id`'s progress on this instance for a Desk connection.
    pub(crate) async fn watch(
        self: &Arc<Self>,
        nats: &async_nats::Client,
        subject: String,
        session_id: &str,
        tx: broadcast::Sender<BroadcastMessage>,
    ) -> WatchGuard {
        let generation = self
            .join(nats, subject, session_id, tx, false, Join::Watch)
            .await;
        WatchGuard {
            forwarders: Arc::clone(self),
            session_id: session_id.to_string(),
            generation,
        }
    }

    async fn join(
        self: &Arc<Self>,
        nats: &async_nats::Client,
        subject: String,
        session_id: &str,
        tx: broadcast::Sender<BroadcastMessage>,
        tts_normalize: bool,
        join: Join,
    ) -> u64 {
        let (generation, lead) = self.admit(session_id, join);
        if lead {
            match nats.subscribe(subject).await {
                Ok(sub) => {
                    let task = tokio::spawn(forward_nats_progress(
                        sub,
                        tx,
                        session_id.to_string(),
                        tts_normalize,
                        Arc::clone(self),
                        generation,
                    ));
                    self.attach(session_id, generation, task.abort_handle());
                }
                Err(e) => {
                    warn!(session_id, error = %e, "Failed to subscribe to progress");
                    match join {
                        Join::Request => self.withdraw(session_id, generation),
                        Join::Watch => self.unwatch(session_id, generation),
                    };
                }
            }
        }
        generation
    }

    fn admit(&self, session_id: &str, join: Join) -> (u64, bool) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut forwarders = self.0.lock();
        let (forwarder, lead) = match forwarders.get_mut(session_id) {
            Some(forwarder) => (forwarder, false),
            None => {
                let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let forwarder = forwarders
                    .entry(session_id.to_string())
                    .or_insert(Forwarder {
                        generation,
                        pending: 0,
                        watchers: 0,
                        stamp: None,
                        task: None,
                        silenced: false,
                    });
                (forwarder, true)
            }
        };
        match join {
            Join::Request => {
                forwarder.pending += 1;
                forwarder.silenced = false;
            }
            Join::Watch => forwarder.watchers += 1,
        }
        (forwarder.generation, lead)
    }

    fn attach(&self, session_id: &str, generation: u64, task: AbortHandle) {
        match self.0.lock().get_mut(session_id) {
            Some(forwarder) if forwarder.generation == generation => forwarder.task = Some(task),
            _ => task.abort(),
        }
    }

    /// One request is over — its `Done` arrived or it was never published.
    /// `true` once the forwarder carries no request and no watcher, which
    /// ends it.
    fn withdraw(&self, session_id: &str, generation: u64) -> bool {
        self.leave(session_id, generation, Join::Request)
    }

    fn unwatch(&self, session_id: &str, generation: u64) -> bool {
        self.leave(session_id, generation, Join::Watch)
    }

    fn leave(&self, session_id: &str, generation: u64, join: Join) -> bool {
        let mut forwarders = self.0.lock();
        let Some(forwarder) = forwarders
            .get_mut(session_id)
            .filter(|f| f.generation == generation)
        else {
            return true;
        };
        match join {
            Join::Request => forwarder.pending = forwarder.pending.saturating_sub(1),
            Join::Watch => forwarder.watchers = forwarder.watchers.saturating_sub(1),
        }
        if forwarder.pending > 0 || forwarder.watchers > 0 {
            return false;
        }
        if let Some(task) = forwarders.remove(session_id).and_then(|f| f.task) {
            task.abort();
        }
        true
    }

    /// The Desk turn `session_id` is streaming, as its frames name it.
    fn stamp(&self, session_id: &str) -> Option<TurnStamp> {
        self.0
            .lock()
            .get(session_id)
            .and_then(|forwarder| forwarder.stamp.clone())
    }

    fn set_stamp(&self, session_id: &str, stamp: Option<TurnStamp>) {
        if let Some(forwarder) = self.0.lock().get_mut(session_id) {
            forwarder.stamp = stamp;
        }
    }

    /// Silence the turn `session_id` is streaming: its requests here are
    /// over, and its tail is not passed on. A session Desk watches keeps its
    /// forwarder, so the next turn, from any instance, is still heard.
    fn stop(&self, session_id: &str) {
        let mut forwarders = self.0.lock();
        match forwarders.get_mut(session_id) {
            Some(forwarder) if forwarder.watchers > 0 => {
                forwarder.pending = 0;
                forwarder.silenced = true;
            }
            _ => {
                if let Some(task) = forwarders.remove(session_id).and_then(|f| f.task) {
                    task.abort();
                }
            }
        }
    }

    /// Whether a frame of `session_id`'s stream is passed on: any, unless a
    /// stopped turn is silenced, whose end (`ends`) is passed on and lifts
    /// the silence.
    fn passes(&self, session_id: &str, ends: bool) -> bool {
        match self.0.lock().get_mut(session_id) {
            Some(forwarder) if forwarder.silenced => {
                forwarder.silenced = !ends;
                ends
            }
            _ => true,
        }
    }
}

/// How long the gateway waits for a turn's owner to apply a control message —
/// past [`crate::hub_core::live_turns::SETTLE_WAIT`], so a stop that settles in time is
/// always acknowledged.
const CONTROL_WAIT: Duration =
    Duration::from_secs(crate::hub_core::live_turns::SETTLE_WAIT.as_secs() + 2);

/// Steer a live turn wherever it runs: over NATS when the hub has a bus, since
/// the turn may be on another instance, else in this process.
pub(crate) async fn steer(
    nats: Option<&async_nats::Client>,
    service: &AgentService,
    session_id: &str,
    control: TurnControl,
) -> Applied {
    match nats {
        Some(nats) => control_turn(nats, service.agent_id.as_str(), session_id, &control).await,
        None => service.live_turns.apply(session_id, control.into()).await,
    }
}

/// Whether `owner` owns `session_id`. A WebSocket frame that names a session
/// acts in it (a new turn, a stop, an approval answer) only when its sender
/// is the person that session's turns run as; an unknown or unreadable
/// session is nobody's.
pub(crate) fn owns_session(
    sessions: &dyn workforce_common::session::SessionStore,
    session_id: &str,
    owner: &str,
) -> bool {
    owned_session(sessions, session_id, owner).is_some()
}

/// `session_id`, when `owner` owns it ([`owns_session`]).
fn owned_session(
    sessions: &dyn workforce_common::session::SessionStore,
    session_id: &str,
    owner: &str,
) -> Option<workforce_common::session::Session> {
    match sessions.get(session_id) {
        Ok(session) => session.filter(|s| s.owner == owner),
        Err(e) => {
            warn!(session_id, error = %e, "Session lookup failed");
            None
        }
    }
}

/// Ask the instance running the session's turn to apply `control`. Only that
/// instance listens, so with no live turn the request fails at once.
async fn control_turn(
    nats: &async_nats::Client,
    agent_id: &str,
    session_id: &str,
    control: &TurnControl,
) -> Applied {
    let subject = workforce_nats::subjects::turn_control(agent_id, session_id);
    let payload = match serde_json::to_vec(control) {
        Ok(payload) => payload,
        Err(e) => {
            warn!(error = %e, "Encoding turn control failed");
            return Applied::NotLive;
        }
    };
    let request = async_nats::Request::new()
        .payload(payload.into())
        .timeout(Some(CONTROL_WAIT));
    match nats.send_request(subject, request).await {
        Ok(reply) => serde_json::from_slice::<TurnControlReply>(&reply.payload)
            .map_or(Applied::NotLive, |reply| applied_by(&reply)),
        Err(e) if e.kind() == async_nats::RequestErrorKind::NoResponders => Applied::NotLive,
        Err(e) => {
            warn!(session_id, error = %e, "Turn control request failed");
            Applied::NotLive
        }
    }
}

/// What the owning instance's reply says became of a control.
fn applied_by(reply: &TurnControlReply) -> Applied {
    match (reply.applied, reply.refused.as_deref()) {
        (true, _) => Applied::Injected,
        (false, Some(REFUSED_BUSY)) => Applied::Busy,
        (false, _) => Applied::NotLive,
    }
}

/// A chat message offered to a live turn: an `approve <id>` / `reject <id>`
/// answers one of its prompts, anything else is a follow-up from `place`.
fn followup_injection(content: &str, user: &str, place: Option<DeskPlace>) -> TurnControl {
    let trimmed = content.trim();
    let lowered = trimmed.to_lowercase();
    let verdict = [("approve ", true), ("reject ", false)]
        .into_iter()
        .find_map(|(verb, approved)| lowered.starts_with(verb).then_some(approved));
    let request_id = trimmed
        .split_once(' ')
        .map(|(_, rest)| rest.trim())
        .filter(|id| !id.is_empty());
    match verdict.zip(request_id) {
        Some((approved, request_id)) => TurnControl::Approval {
            request_id: request_id.to_string(),
            approved,
            user: user.to_string(),
            choice: None,
        },
        None => TurnControl::FollowUp {
            text: content.to_string(),
            user: user.to_string(),
            place,
        },
    }
}

/// The agent a frame addresses: its own `agent_id`, else the agent the
/// connection was opened for, else the default.
fn frame_agent<'a>(
    frame_agent: Option<&'a str>,
    conn_agent: Option<&'a str>,
    registry: &'a AgentRegistry,
) -> &'a str {
    frame_agent.or(conn_agent).unwrap_or(registry.default_id())
}

/// Forward a session's NATS progress to this instance's WebSocket clients
/// until every request it carries has sent its `Done`.
async fn forward_nats_progress(
    mut sub: async_nats::Subscriber,
    tx: broadcast::Sender<BroadcastMessage>,
    session_id: String,
    tts_normalize: bool,
    forwarders: Arc<ProgressForwarders>,
    generation: u64,
) {
    while let Some(msg) = sub.next().await {
        // An event a later build sends and this one cannot read is skipped,
        // never the end of the stream.
        let progress: ChatProgress = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                debug!(session_id = %session_id, error = %e, "Skipping an unreadable progress event");
                continue;
            }
        };

        let done = matches!(progress, ChatProgress::Done { .. });
        let passes = forwarders.passes(
            &session_id,
            done || matches!(progress, ChatProgress::TurnEnd(_)),
        );
        let stamp = match &progress {
            ChatProgress::Snapshot(snapshot) => {
                forwarders.set_stamp(&session_id, Some(snapshot.stamp.clone()));
                Some(snapshot.stamp.clone())
            }
            ChatProgress::TurnEnd(_) => {
                forwarders.set_stamp(&session_id, None);
                None
            }
            _ => forwarders.stamp(&session_id),
        };

        if let Some(broadcast) =
            chat_progress_to_broadcast(progress, &session_id, tts_normalize).filter(|_| passes)
        {
            let _ = tx.send(broadcast.stamped(stamp.as_ref()));
        }

        if done && forwarders.withdraw(&session_id, generation) {
            break;
        }
    }
}

/// Convert a [`ChatProgress`] event into a [`BroadcastMessage`] for WebSocket delivery.
pub(crate) fn chat_progress_to_broadcast(
    progress: ChatProgress,
    session_id: &str,
    tts_normalize: bool,
) -> Option<BroadcastMessage> {
    match progress {
        ChatProgress::Typing => Some(BroadcastMessage::new(session_id, "typing", String::new())),
        ChatProgress::StreamStart => Some(BroadcastMessage::new(
            session_id,
            "stream_start",
            String::new(),
        )),
        ChatProgress::StreamDelta { text } => {
            let content = if tts_normalize {
                crate::formatting::normalize_delta_for_tts(&text)
            } else {
                text
            };
            Some(BroadcastMessage::new(session_id, "stream_delta", content))
        }
        ChatProgress::StreamEnd => Some(BroadcastMessage::new(
            session_id,
            "stream_end",
            String::new(),
        )),
        ChatProgress::StreamDiscard { reason } => Some(BroadcastMessage::silent(
            session_id,
            "stream_discard",
            reason.as_str(),
        )),
        ChatProgress::Message { text } => {
            let rendered = workforce_common::secret_marker::render_markers_as_preview(&text);
            let text = if workforce_common::security_scanner::contains_credentials(&rendered) {
                tracing::warn!(target: "sentinel", "Credential detected in message — redacting");
                workforce_common::security_scanner::redact_credentials(&rendered)
            } else {
                rendered
            };
            let content = if tts_normalize {
                crate::formatting::normalize_for_tts(&text)
            } else {
                text
            };
            Some(BroadcastMessage::new(session_id, "message", content))
        }
        ChatProgress::ToolStart { name, detail } => {
            let content = if detail.is_empty() {
                name
            } else {
                format!("{}: {}", name, detail)
            };
            Some(BroadcastMessage::new(session_id, "tool_start", content))
        }
        ChatProgress::ToolEnd {
            name,
            success,
            duration_ms,
            ..
        } => Some(BroadcastMessage::new(
            session_id,
            "tool_end",
            format!(
                "{} ({}ms, {})",
                name,
                duration_ms,
                if success { "ok" } else { "failed" }
            ),
        )),
        ChatProgress::CompactStart => Some(BroadcastMessage::new(
            session_id,
            "compact_start",
            String::new(),
        )),
        ChatProgress::CompactEnd => Some(BroadcastMessage::new(
            session_id,
            "compact_end",
            String::new(),
        )),
        ChatProgress::ContextUpdate {
            estimated_tokens,
            context_max,
        } => {
            let mut msg = BroadcastMessage::new(session_id, "context_update", String::new());
            msg.context_tokens = Some(estimated_tokens);
            msg.context_max = Some(context_max);
            Some(msg)
        }
        ChatProgress::ApprovalRequired {
            request_id,
            agent_id,
            tool_name,
            detail: _,
            reason,
            options,
            ..
        } => Some(BroadcastMessage::permission_prompt(
            session_id, agent_id, request_id, tool_name, reason, options,
        )),
        ChatProgress::CodeCitations { citations } => {
            Some(BroadcastMessage::code_citations(session_id, citations))
        }
        ChatProgress::ConnectPrompt { prompt } => {
            Some(BroadcastMessage::connect_prompt(session_id, prompt))
        }
        ChatProgress::Route { line } => Some(BroadcastMessage::route(session_id, line)),
        ChatProgress::Done { .. } => None,
        ChatProgress::Error { message } => {
            Some(BroadcastMessage::new(session_id, "error", message))
        }
        ChatProgress::RateLimited { wait_secs, attempt } => Some(BroadcastMessage::new(
            session_id,
            "rate_limited",
            rate_limited_line(wait_secs, attempt),
        )),
        ChatProgress::SlowModelFailover {
            model,
            elapsed_secs,
        } => Some(BroadcastMessage::new(
            session_id,
            "slow_model_failover",
            slow_model_line(&model, elapsed_secs),
        )),
        ChatProgress::Snapshot(snapshot) => {
            Some(BroadcastMessage::turn_snapshot(session_id, snapshot))
        }
        ChatProgress::File(file) => Some(crate::desk_attachments::frame(
            session_id,
            std::slice::from_ref(&file),
            file.caption.clone(),
        )),
        ChatProgress::TurnEnd(end) => Some(BroadcastMessage::turn_end(session_id, end)),
    }
}

/// What a `rate_limited` frame says.
fn rate_limited_line(wait_secs: u64, attempt: u32) -> String {
    match attempt {
        0 => format!("pacing under provider rate limit, ~{wait_secs}s wait"),
        _ => format!("rate-limited, retry #{attempt} in {wait_secs}s"),
    }
}

/// What a `slow_model_failover` frame says.
fn slow_model_line(model: &str, elapsed_secs: u64) -> String {
    format!("{model} produced nothing for {elapsed_secs}s — switching model")
}

// ── Local Agent WebSocket (DW-324) ─────────────────────────────────────────

/// WebSocket upgrade handler for local agent connections.
///
/// A node acts for one person, so it authenticates as their paired device:
/// the device's linked identity is who it is bound to. The gateway token
/// names nobody and is refused, as on `/ws`; so is a device without the
/// `agents` scope, one linked to no one, or one whose person is suspended.
async fn ws_local_agent_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let token = query.token.unwrap_or_default();
    let refused = |status: StatusCode, error: &str, message: &str| {
        (
            status,
            axum::Json(serde_json::json!({ "error": error, "message": message })),
        )
            .into_response()
    };

    if crate::auth::constant_time_token_eq(&token, &state.auth_token) {
        return refused(
            StatusCode::FORBIDDEN,
            "unbound",
            "A local agent acts for one person: connect with a device token paired to them, \
             not the gateway token.",
        );
    }
    let Some(ref pairing) = state.device_pairing else {
        return refused(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Device pairing is not configured on this hub.",
        );
    };
    let device =
        match pairing.validate_token_async(token.to_string()).await {
            Ok(Some(device)) => device,
            Ok(None) => return refused(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication required. Provide a valid device token (?token=...) or pair your \
                 device via /api/pair/device/request.",
            ),
            Err(e) => {
                warn!(
                    error = %e,
                    "Pairing store error during local agent WebSocket auth — failing closed"
                );
                return refused(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service_unavailable",
                    "Authentication backend unavailable — retry shortly.",
                );
            }
        };
    if !crate::jwt::has_scope(&device.scopes, crate::jwt::SCOPE_AGENTS) {
        return refused(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This device was paired without the 'agents' scope, which a local agent needs.",
        );
    }
    let Some(identity_id) = state
        .identity_store
        .as_ref()
        .and_then(|store| identity_for_device(store.as_ref(), &device.id))
    else {
        return refused(
            StatusCode::FORBIDDEN,
            "unbound",
            "This device is linked to no person, so no one's turns could use it. Pair it to a \
             person first.",
        );
    };
    if let Some(refused) = suspended_refusal(&state, Some(&identity_id)) {
        return refused;
    }
    ws.on_upgrade(move |socket| handle_local_agent_ws(socket, state, identity_id))
        .into_response()
}

/// Handle a local agent WebSocket connection, bound to `identity_id`.
///
/// Protocol:
/// 1. Agent sends `local_agent_hello` as its first message, with the tools
///    it offers
/// 2. Gateway registers the agent for its person — or answers
///    `local_agent_refused` and closes, for an unusable id or one another
///    person's node holds
/// 3. Write loop: forwards serialized JSON messages from the registry to the WS
/// 4. Read loop: receives `local_exec_result` / `local_exec_progress` /
///    `node_tool_result` / `node_tools` / `pong`
/// 5. On disconnect: deregisters this connection, failing its waiting calls
async fn handle_local_agent_ws(socket: WebSocket, state: Arc<GatewayState>, identity_id: String) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Step 1: Wait for the local_agent_hello handshake
    let hello: LocalAgentHello = loop {
        match ws_receiver.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<LocalAgentHello>(&text) {
                Ok(h) if h.msg_type == "local_agent_hello" => break h,
                Ok(_) => {
                    warn!("Local agent sent wrong message type, expected local_agent_hello");
                    continue;
                }
                Err(e) => {
                    warn!("Failed to parse local_agent_hello: {}", e);
                    continue;
                }
            },
            Some(Ok(Message::Close(_))) | None => {
                info!("Local agent disconnected before hello");
                return;
            }
            _ => continue,
        }
    };

    let agent_id = hello.agent_node_id.clone();
    info!(
        agent_id = %agent_id,
        name = %hello.name,
        identity = %identity_id,
        capabilities = ?hello.capabilities,
        tools = hello.tools.len(),
        "Local agent connected"
    );

    // Step 2: Create mpsc channel for sending requests to this agent
    let (request_tx, mut request_rx) = tokio::sync::mpsc::channel::<String>(64);

    // Step 3: Register in the local agent registry
    let registered = state
        .local_agents
        .register(
            crate::local_agent_registry::NodeHello {
                id: hello.agent_node_id,
                name: hello.name,
                capabilities: hello.capabilities,
                tools: hello.tools,
            },
            identity_id,
            request_tx,
        )
        .await;
    let connection = match registered {
        Ok(connection) => connection,
        Err(refusal) => {
            warn!(agent_id = %agent_id, %refusal, "Local agent refused");
            let refused = workforce_gateway_client::protocol::LocalAgentRefused {
                msg_type: "local_agent_refused".to_string(),
                message: refusal.to_string(),
            };
            let _ = ws_sender
                .send(Message::Text(
                    serde_json::to_string(&refused).unwrap().into(),
                ))
                .await;
            let _ = ws_sender.send(Message::Close(None)).await;
            return;
        }
    };

    // Send acknowledgement
    let ack = serde_json::json!({
        "type": "local_agent_ack",
        "agent_node_id": agent_id,
    });
    let _ = ws_sender
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await;

    // Step 4: Spawn write loop — reads from mpsc, sends to WS
    let agent_id_write = agent_id.clone();
    let mut write_task = tokio::spawn(async move {
        while let Some(msg) = request_rx.recv().await {
            if ws_sender.send(Message::Text(msg.into())).await.is_err() {
                warn!(agent_id = %agent_id_write, "Failed to send to local agent WS");
                break;
            }
        }
    });

    // Step 5: Spawn read loop — receives results from the agent
    let local_agents_read = Arc::clone(&state.local_agents);
    let agent_id_read = agent_id.clone();
    let mut read_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    let parsed: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(agent_id = %agent_id_read, "Invalid JSON from local agent: {}", e);
                            continue;
                        }
                    };
                    receive_from_node(&local_agents_read, &agent_id_read, connection, parsed).await;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut write_task => {
            read_task.abort();
        }
        _ = &mut read_task => {
            write_task.abort();
        }
    }

    // Step 6: Deregister on disconnect
    state.local_agents.deregister(&agent_id, connection).await;
    info!(agent_id = %agent_id, "Local agent disconnected");
}

/// Act on one message from node `agent_id`'s `connection`.
async fn receive_from_node(
    registry: &crate::local_agent_registry::LocalAgentRegistry,
    agent_id: &str,
    connection: u64,
    message: serde_json::Value,
) {
    use workforce_gateway_client::protocol::{NodeToolResult, NodeTools};
    fn parsed<T: serde::de::DeserializeOwned>(
        agent_id: &str,
        kind: &str,
        message: serde_json::Value,
    ) -> Option<T> {
        serde_json::from_value(message)
            .inspect_err(|e| warn!(agent_id, kind, "Failed to parse a local agent message: {e}"))
            .ok()
    }
    let kind = message
        .get("type")
        .and_then(|t| t.as_str())
        .map(str::to_owned);
    match kind.as_deref() {
        Some(kind @ "local_exec_result") => {
            if let Some(result) = parsed::<LocalExecResult>(agent_id, kind, message) {
                registry.fulfill_result(agent_id, connection, result).await;
            }
        }
        Some(kind @ "node_tool_result") => {
            if let Some(result) = parsed::<NodeToolResult>(agent_id, kind, message) {
                registry
                    .fulfill_tool_result(agent_id, connection, result)
                    .await;
            }
        }
        Some(kind @ "node_tools") => {
            if let Some(update) = parsed::<NodeTools>(agent_id, kind, message) {
                registry.set_tools(agent_id, connection, update.tools).await;
            }
        }
        // Progress is informational, and a pong is a keepalive.
        Some("local_exec_progress" | "pong") => {}
        Some(other) => {
            tracing::debug!(agent_id, msg_type = %other, "Unknown message type from local agent");
        }
        None => warn!(agent_id, "Local agent message missing 'type' field"),
    }
}

/// List connected local agents (REST API).
async fn list_local_agents(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let agents = match (caller.is_admin, caller.identity_id.as_deref()) {
        (true, _) => state.local_agents.list().await,
        (false, Some(person)) => state.local_agents.list_for(person).await,
        (false, None) => Vec::new(),
    };
    Ok(Json(serde_json::json!({ "local_agents": agents })))
}

// ── REST endpoints ──────────────────────────────────────────────────────────
// These use the default agent from the registry for backward compatibility.

/// Resolve the default `AgentService` and verify the calling principal is
/// allowed to read the named session.
///
/// Returns `Unauthorized` for callers that supplied no recognised token,
/// `NotFound` for callers that authed but lack ownership (so anonymous
/// enumeration of session IDs reveals nothing about which IDs exist).
/// Emits a `session_read` audit event in either branch (DW-527).
async fn authorise_session_read(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    session_id: &str,
) -> Result<Arc<AgentService>, StatusCode> {
    use workforce_memory::access_audit::{emit, AccessAuditEvent, AuditDecision, AuditOperation};

    let caller = resolve_caller_context(state, headers, query_token).await;
    let requester = caller
        .identity_id
        .as_deref()
        .or(caller.device_id.as_deref());
    if !caller.is_authenticated() {
        emit(AccessAuditEvent::new(
            AuditOperation::SessionRead,
            AuditDecision::DeniedUnscoped,
            requester,
            None,
            Some(session_id),
        ));
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !caller.has_scope(crate::jwt::SCOPE_SESSIONS) {
        return Err(StatusCode::FORBIDDEN);
    }
    // Sessions live in per-agent stores, so search every agent for the one
    // holding this id — not just the default agent. Without this, any session
    // owned by a non-default agent 404s on transcript/stats/export even though
    // it shows up in the (all-agent) list. `can_see_owner` below still does the
    // real authorisation, so this search is safe for non-admins too.
    let (service, owner) = state
        .registry
        .all_services()
        .into_iter()
        .find_map(|svc| match svc.sessions.get(session_id) {
            Ok(Some(s)) => Some((svc, s.owner)),
            _ => None,
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    if !caller.can_see_owner(&owner) {
        emit(AccessAuditEvent::new(
            AuditOperation::SessionRead,
            AuditDecision::DeniedOwnerMismatch,
            requester,
            Some(&owner),
            Some(session_id),
        ));
        return Err(StatusCode::NOT_FOUND);
    }
    emit(AccessAuditEvent::new(
        AuditOperation::SessionRead,
        AuditDecision::Allow,
        requester,
        Some(&owner),
        Some(session_id),
    ));
    Ok(service)
}

/// REST chat endpoint (non-WS fallback). Requires authentication; the
/// authenticated identity (or paired-device id) is the session owner.
async fn chat_handler(
    headers: axum::http::HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, axum::response::Response> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let owner = caller
        .identity_id
        .as_deref()
        .or(caller.device_id.as_deref())
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?
        .to_string();
    if !caller.has_scope(crate::jwt::SCOPE_CHAT) {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if let Err(retry_after) = check_action_rate_limit(state.action_limiter.as_ref(), &caller) {
        return Err(rate_limited_response(retry_after));
    }

    let service = match req.agent_id.as_deref() {
        Some(id) => state
            .registry
            .get(id)
            .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?
            .clone(),
        None => state
            .default_service()
            .ok_or_else(|| StatusCode::SERVICE_UNAVAILABLE.into_response())?
            .clone(),
    };
    let agent_id = req
        .agent_id
        .as_deref()
        .unwrap_or_else(|| state.registry.default_id());
    if let Some(refusal) =
        rest_chat_refusal(&state, agent_id, &service, &caller, &req.message).await
    {
        let (status, said) = match refusal {
            RestChatRefusal::Denied(said) => (StatusCode::FORBIDDEN, said),
            RestChatRefusal::OutOfScope(said) => (StatusCode::UNPROCESSABLE_ENTITY, said),
        };
        return Err((status, Json(serde_json::json!({ "error": said }))).into_response());
    }
    let channel = req.channel.as_deref().unwrap_or("rest");
    let task_id = req
        .turn_id
        .as_deref()
        .map(|turn| chat_task_id(&owner, turn))
        .transpose()
        .map_err(|reason| (StatusCode::BAD_REQUEST, reason).into_response())?;
    let config_before = service.config_applied.read().clone();
    let started = chrono::Utc::now();

    let scan = workforce_agent::security::external_content::scan_for_injection(&req.message);
    if scan.suspicious {
        tracing::warn!(
            score = scan.score,
            hostile = scan.hostile,
            "Suspicious patterns in REST chat message"
        );
    }

    match service
        .process_turn(
            channel,
            &owner,
            &req.message,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            task_id.as_deref(),
            Some(rest_chat_trust(&state, &caller)),
            None,
            crate::serve::RunAuthor::Other,
        )
        .await
    {
        Ok(turn) => {
            // Resolved AFTER the turn: a stale-resume or watermark rollover
            // inside `process_turn` changes the thread's active session,
            // and the client must be handed the id that will receive its
            // next message (transcript reads walk the whole chain anyway).
            let session = service
                .sessions
                .get_or_create(channel, &owner, &service.agent_id.to_string())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
            let config_after = service.config_applied.read().clone();
            let (response, outcome) = match turn {
                crate::serve::TurnOutcome::Answered(reply) => (reply, ChatOutcome::Answered),
                crate::serve::TurnOutcome::Stopped(stop) => (stop.reason(), ChatOutcome::Refused),
                crate::serve::TurnOutcome::Skipped(skip) => {
                    (skip.to_string(), ChatOutcome::Refused)
                }
            };
            Ok(Json(ChatResponse {
                response,
                capped: crate::serve::round_capped_since(&session, started),
                session_id: session.id,
                task_id,
                outcome,
                config: config_before.filter(|before| Some(before) == config_after.as_ref()),
            }))
        }
        Err(e) => {
            warn!("Chat error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// The longest `turn_id` a REST chat caller may name a turn with.
const MAX_TURN_ID_CHARS: usize = 64;

/// The task id a REST chat turn named `turn` by `owner` is filed under. The
/// prefix keeps a caller's name for a turn inside their own namespace, apart
/// from the ids the scheduler and workflows file theirs under.
fn chat_task_id(owner: &str, turn: &str) -> Result<String, &'static str> {
    let named = !turn.is_empty()
        && turn.chars().count() <= MAX_TURN_ID_CHARS
        && turn
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    match named {
        true => Ok(format!("chat:{owner}:{turn}")),
        false => Err("turn_id must be 1-64 letters, digits, '-', '_' or '.'"),
    }
}

/// The trust a REST chat message enters with: the role-to-trust mapping the
/// WebSocket and MCP paths use, applied to the identity on record now rather
/// than to the role a token was minted with. An active person is trusted as
/// their role is; a paired device speaks as a member, whoever it is linked
/// to, as everywhere else on the REST surface; anyone else (no identity on
/// record, a suspended one) stays untrusted.
fn rest_chat_trust(state: &GatewayState, caller: &CallerContext) -> TrustLevel {
    use workforce_identity::types::{IdentityRole, IdentityStatus};
    caller
        .identity_id
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .zip(state.identity_store.as_ref())
        .and_then(|(id, store)| store.get_by_id(id).ok().flatten())
        .filter(|identity| identity.status == IdentityStatus::Active)
        .map_or(TrustLevel::Untrusted, |identity| {
            let role = match caller.device_id {
                Some(_) => IdentityRole::Member,
                None => identity.role,
            };
            classify_trust_with_role(
                &ContentSource::Gateway { device_id: None },
                None,
                &state.sentinel,
                true,
                Some(role.as_str()),
            )
        })
}

/// Why a REST chat message is turned away before it reaches the agent, and
/// what its sender is told.
enum RestChatRefusal {
    /// The agent's access policy does not admit the sender.
    Denied(String),
    /// The agent's scope filter turned this message away.
    OutOfScope(String),
}

/// Why `caller` may not put `message` to the agent `agent_id` over REST, if
/// they may not: the agent's access policy and its scope filter, as the
/// WebSocket path applies them.
async fn rest_chat_refusal(
    state: &GatewayState,
    agent_id: &str,
    service: &Arc<crate::serve::AgentService>,
    caller: &CallerContext,
    message: &str,
) -> Option<RestChatRefusal> {
    let surface_id = caller
        .device_id
        .as_deref()
        .map(|d| format!("ios_device:{d}"));
    if let workforce_common::settings::AccessDecision::Deny { message, reason } =
        state.access_gate.check(crate::access_gate::AccessRequest {
            agent_id,
            identity_id: caller.identity_id.as_deref(),
            surface_ids: surface_id.as_deref().as_slice(),
            is_external: false,
        })
    {
        info!(
            agent = %agent_id,
            identity = ?caller.identity_id,
            device = ?caller.device_id,
            reason = %reason,
            "Access denied by agent access policy"
        );
        return Some(RestChatRefusal::Denied(message.unwrap_or_else(|| {
            "Access denied by this agent's policy.".to_string()
        })));
    }
    let scope = service
        .scope_gate
        .read()
        .clone()
        .and_then(|gate| gate.lookup(agent_id))
        .filter(|_| !message.trim().is_empty())?;
    match crate::decide::evaluate_scope(
        &scope,
        service.llm.as_ref(),
        &crate::credentials::Caller::for_service(service, caller.identity_id.clone()),
        &service.request_subject(caller.identity_id.clone(), "gateway"),
        message,
        "gateway",
        agent_id,
    )
    .await
    {
        crate::scope_gate::ScopeOutcome::Reject { canned } => {
            Some(RestChatRefusal::OutOfScope(canned))
        }
        _ => None,
    }
}

async fn session_transcript(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Path(session_id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let service =
        authorise_session_read(&state, &headers, params.token.as_deref(), &session_id).await?;
    let limit = params.limit.unwrap_or(50).min(200);
    let before = params.before.as_deref();
    let before_session = params.before_session.as_deref();
    // Walk the thread chain so a rolled-over conversation reads as one. The
    // top-level `session_id` echoes the page's starting session; the per-entry
    // `session_id` is the authoritative cursor key.
    match service
        .sessions
        .read_thread_page(&session_id, before_session, before, limit)
    {
        Ok(page) => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "entries": page.messages,
            "has_more": page.has_more,
        }))),
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to read transcript");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Get session statistics.
async fn session_stats(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Path(session_id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let service =
        authorise_session_read(&state, &headers, query.token.as_deref(), &session_id).await?;
    match service.sessions.session_stats(&session_id) {
        Ok(stats) => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "stats": stats,
        }))),
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to get session stats");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Export a full session as a single JSON document.
async fn session_export(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Path(session_id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let service =
        authorise_session_read(&state, &headers, query.token.as_deref(), &session_id).await?;
    match service.sessions.export_session(&session_id) {
        Ok(export) => {
            let value =
                serde_json::to_value(&export).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(value))
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to export session");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// List available agents.
async fn list_agents(
    headers: axum::http::HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<workforce_gateway_client::AgentsResponse>, StatusCode> {
    // Resolve the caller across every auth mode the gateway accepts — admin
    // token, JWT, and paired-device (`wfp_*`) token. The JWT-only
    // `extract_caller_identity` used previously could not resolve a paired
    // device, so an iOS client came through as anonymous and the visibility
    // filter hid every agent. `resolve_caller_context` is the same resolver
    // the chat ingress and session endpoints use, and it populates `device_id`.
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;

    // A rejected or uncheckable credential must not degrade to the anonymous
    // empty roster — HTTP 200 `{"agents": []}` is indistinguishable from
    // "connected, nothing configured" to the iOS app, which is exactly how a
    // stranded device stayed invisible for weeks. Only credential-less
    // callers proceed as anonymous (and see nothing — F1).
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => return Err(StatusCode::UNAUTHORIZED),
        Some(AuthFailure::StoreError) => return Err(StatusCode::SERVICE_UNAVAILABLE),
        None => {}
    }

    let viewer = AgentViewer::new(&state, caller);
    let faces = picture_faces(&state).await;

    let agents = state
        .registry
        .agent_ids()
        .iter()
        .filter_map(|id| {
            let id_str = id.to_string();
            let svc = state.registry.get(&id_str)?;
            let agent_config = svc.config.agent_by_id(&id_str);
            let (eff_owner_id, eff_owner_type) = svc.effective_owner();

            let vis = viewer.visibility(&id_str, eff_owner_id.as_deref(), &eff_owner_type);
            if !vis.visible {
                return None;
            }

            // Owner UUIDs are management metadata. Disclose them only on the
            // ownership path (admin / owner / team member); a caller who sees
            // the agent purely via its access policy must not learn the owner
            // id or type (audit F1 leaked owner UUIDs to unprivileged callers).
            let (owner_id, owner_type) = if vis.disclose_owner {
                (
                    eff_owner_id,
                    Some(format!("{:?}", eff_owner_type).to_lowercase()),
                )
            } else {
                (None, None)
            };

            let model = svc.current_default_model.read().clone();
            let provider = svc.provider_kind.read().canonical_name().to_string();
            Some(workforce_gateway_client::AgentInfo {
                id: id_str.clone(),
                name: svc.display_name(),
                model: Some(model).filter(|m| !m.is_empty()),
                provider: Some(provider),
                capabilities: agent_config
                    .map(|a| a.capabilities.clone())
                    .unwrap_or_default(),
                default: id_str == state.registry.default_id(),
                owner_id,
                owner_type,
                picture: faces.get(&id_str).map(crate::pictures::Face::info),
            })
        })
        .collect();
    Ok(Json(workforce_gateway_client::AgentsResponse { agents }))
}

/// Every agent's picture, by agent id; none, said in the log, when the
/// store can't be read (the roster still answers).
async fn picture_faces(state: &GatewayState) -> HashMap<String, crate::pictures::Face> {
    let Some(store) = state.pictures.as_ref() else {
        return HashMap::new();
    };
    crate::pictures::faces(store.as_ref(), crate::pictures::Kind::Agent)
        .await
        .unwrap_or_else(|e| {
            warn!(target: "pictures", error = %format!("{e:#}"), "couldn't read agents' pictures");
            HashMap::new()
        })
}

/// The picture `owner` is shown with; none, said in the log, when the
/// store can't be read.
async fn picture_info(
    state: &GatewayState,
    owner: &crate::pictures::Owner,
) -> Option<workforce_gateway_client::PictureInfo> {
    let store = state.pictures.as_ref()?;
    match crate::pictures::face(store.as_ref(), owner).await {
        Ok(face) => face.map(|f| f.info()),
        Err(e) => {
            warn!(target: "pictures", %owner, error = %format!("{e:#}"), "couldn't read a picture");
            None
        }
    }
}

/// Authenticated caller's identity for ownership-aware filtering and session
/// access control. Resolves uniformly across the auth modes the gateway
/// accepts (admin token, JWT, paired-device token).
///
/// `identity_id` is the canonical user ID when the caller is linked to a
/// human identity (Keycloak / Slack-auto-provisioning). `device_id` is the
/// paired-device ID for unlinked devices. Sessions on disk store either as
/// `owner`, so both are checked.
/// Why a presented credential failed to resolve to a principal. Carried on
/// [`CallerContext`] so handlers can tell "no credential" (legitimately
/// anonymous) from "credential rejected" (401) from "couldn't check" (503).
/// Collapsing all three into anonymous is how a stranded iOS device silently
/// saw an empty agent roster for 17 days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// A credential was presented but matched no admin token, JWT, or
    /// paired device — the caller's token is dead; reject with 401.
    InvalidCredentials,
    /// The pairing store errored while validating, so the credential's
    /// validity is unknown — fail with 503, never fail-open to anonymous.
    StoreError,
}

#[derive(Debug, Default, Clone)]
pub struct CallerContext {
    pub identity_id: Option<String>,
    pub device_id: Option<String>,
    /// Caller may see other users' sessions. Reserved for the static
    /// gateway token and for JWT callers with the SystemAdmin role. All
    /// other principals (Manager, Member, paired devices) stay scoped
    /// to their own owner. Management-only endpoints (Stats, etc.) gate
    /// on `claims.role.can_manage_teams()` independently, so Managers
    /// retain access there.
    pub is_admin: bool,
    /// Scope claims this token may exercise. Populated from the JWT's
    /// `scopes` field or the paired device's `approved_devices.scopes`
    /// column. Admins ignore this (their `is_admin` flag bypasses the
    /// gate); for everyone else, an empty vec denies all scope-gated
    /// routes — pair the device with the narrowest set it needs.
    pub scopes: Vec<String>,
    /// Set when a credential was presented but did not authenticate. `None`
    /// for both successful auth and credential-less anonymous callers.
    pub auth_failure: Option<AuthFailure>,
}

/// A JWT carries identity_id and (optionally) device_id directly. Only
/// SystemAdmin inherits `is_admin` for cross-user session visibility —
/// Managers see only their own sessions, matching the user-facing
/// requirement.
impl From<crate::jwt::Claims> for CallerContext {
    fn from(claims: crate::jwt::Claims) -> Self {
        Self {
            identity_id: claims.identity_id,
            device_id: claims.device_id,
            is_admin: claims.role.can_manage_system(),
            scopes: claims.scopes,
            auth_failure: None,
        }
    }
}

impl CallerContext {
    /// True if `owner` matches this caller's identity_id or device_id.
    /// Admin tokens are NOT auto-owners — they may view but cannot act-as.
    pub(crate) fn owns(&self, owner: &str) -> bool {
        self.identity_id.as_deref() == Some(owner) || self.device_id.as_deref() == Some(owner)
    }

    /// True if the caller may see a session with this owner in listings.
    /// Admins see all; everyone else sees only what they own.
    pub(crate) fn can_see_owner(&self, owner: &str) -> bool {
        self.is_admin || self.owns(owner)
    }

    /// True when the caller carries a recognised principal (identity, device,
    /// or admin). Anonymous callers fail this check; downstream handlers
    /// should treat them as 401.
    pub(crate) fn is_authenticated(&self) -> bool {
        self.is_admin || self.identity_id.is_some() || self.device_id.is_some()
    }

    /// True if the caller may exercise a route that requires `required`.
    /// Admins bypass the gate; everyone else must carry the scope explicitly
    /// (or the wildcard `*`). Mirrors `can_see_owner`'s admin-bypass shape.
    pub(crate) fn has_scope(&self, required: &str) -> bool {
        self.is_admin || crate::jwt::has_scope(&self.scopes, required)
    }
}

// ──────────────── Cross-instance runtime artifact serving (Inc 2b) ────────────
//
// Serves the runtime `workforce_agent::artifacts::global_store()` — distinct from
// the orchestrator `KnowledgeStore` behind `/api/artifacts/{id}`. The generic
// `/api/artifacts/runtime/{id}` route enforces USER scope only: the gateway
// caller carries no session, so a session-scoped artifact (no `user_id`) is
// served on the session route instead.

#[derive(serde::Deserialize)]
struct RuntimeArtifactQuery {
    token: Option<String>,
    agent_id: Option<String>,
}

/// Authorise a USER-scoped runtime artifact read for an already-authenticated
/// caller (the handler rejects anonymous callers with 401 first). Fail-closed
/// and anti-enumeration: every deny is `NOT_FOUND` so a non-owner can't learn
/// the artifact exists. No admin bypass — admins read their own like anyone.
///
/// `user_id` is matched first so a user-owned upload that *also* carries a
/// `session_id` still serves here (the doc's "servable on either route"); a
/// pure session-scoped artifact (`user_id == None`, `session_id == Some`) is
/// denied because the generic caller can't prove session ownership.
fn authorize_runtime_read(
    caller: &CallerContext,
    descriptor: &workforce_agent::artifacts::ArtifactDescriptor,
) -> Result<(), StatusCode> {
    match descriptor.user_id.as_deref() {
        Some(owner) if caller.owns(owner) => Ok(()),
        Some(_) => Err(StatusCode::NOT_FOUND),
        None if descriptor.session_id.is_some() => Err(StatusCode::NOT_FOUND),
        None => Ok(()),
    }
}

/// Dispatch an authorised read to the storage backend: an S3-style store hands
/// back a presigned URL we 307-redirect to (never buffering bytes); a streaming
/// store (Fs / in-memory) reads the body and serves it inline with the
/// persisted filename/mime. Factored out so it is unit-testable against a real
/// `FsArtifactStore` and a fake S3 without touching the process-global store.
pub(crate) fn serve_runtime_artifact(
    store: &dyn workforce_agent::artifacts::ArtifactStore,
    agent_id: &str,
    art_id: &workforce_agent::artifacts::ArtifactId,
    descriptor: &workforce_agent::artifacts::ArtifactDescriptor,
    expiry: std::time::Duration,
) -> Result<axum::response::Response, StatusCode> {
    use workforce_agent::artifacts::disposition::content_disposition_attachment;

    match store
        .presigned_get(agent_id, art_id, expiry)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        // Presigned URL is an unauthenticated bearer link once issued — never
        // log the `Location`.
        Some(url) => Ok(axum::response::Redirect::temporary(&url).into_response()),
        None => {
            // Fs-backed bodies stream straight from disk — a 1 GiB uploaded
            // video must not materialise in hub memory to be downloaded.
            let mut response = match store.body_file_path(agent_id, art_id) {
                Ok(Some(path)) => {
                    let file = std::fs::File::open(&path).map_err(|_| StatusCode::NOT_FOUND)?;
                    let len = file.metadata().map(|m| m.len()).ok();
                    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
                    let mut response =
                        axum::response::Response::new(axum::body::Body::from_stream(stream));
                    if let Some(len) = len {
                        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                            response
                                .headers_mut()
                                .insert(axum::http::header::CONTENT_LENGTH, v);
                        }
                    }
                    response
                }
                _ => store
                    .read(agent_id, art_id)
                    .map_err(|_| StatusCode::NOT_FOUND)?
                    .into_bytes()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .into_response(),
            };
            let content_type = descriptor
                .media_type
                .as_deref()
                .unwrap_or_else(|| descriptor.mime.as_str());
            let content_type = HeaderValue::from_str(content_type)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let resp_headers = response.headers_mut();
            resp_headers.insert(axum::http::header::CONTENT_TYPE, content_type);
            if let Some(filename) = descriptor.filename.as_deref() {
                let disposition = HeaderValue::from_str(&content_disposition_attachment(filename))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                resp_headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
            }
            Ok(response)
        }
    }
}

/// `GET /api/artifacts/runtime/{id}?agent_id=…` — serve a runtime artifact
/// produced on any hub instance. Authenticated, USER-scoped (see
/// [`authorize_runtime_read`]), S3 → 307 redirect / Fs → streamed bytes.
async fn artifacts_runtime_get(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Query(q): Query<RuntimeArtifactQuery>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let store =
        workforce_agent::artifacts::global_store().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let agent_id = q.agent_id.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let art_id =
        workforce_agent::artifacts::ArtifactId::parse(&id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let descriptor = store
        .metadata(agent_id, &art_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_runtime_read(&caller, &descriptor)?;
    serve_runtime_artifact(
        store.as_ref(),
        agent_id,
        &art_id,
        &descriptor,
        std::time::Duration::from_secs(300),
    )
}

/// `GET /api/artifacts/runtime/{id}/table` — one filtered, sorted page of a
/// staged table, which a rich-block client pages an artifact-backed
/// `wf-table` and a `wf-form` typeahead through. Authorised exactly like the
/// artifact's bytes ([`artifacts_runtime_get`]).
async fn artifacts_runtime_table(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Query(q): Query<RuntimeArtifactQuery>,
    Query(page): Query<workforce_agent::artifacts::table_page::PageRequest>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, StatusCode> {
    use workforce_agent::artifacts::table_page::{table_page, PageRefusal};
    let caller = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let store =
        workforce_agent::artifacts::global_store().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let agent_id = q.agent_id.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let art_id =
        workforce_agent::artifacts::ArtifactId::parse(&id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let descriptor = store
        .metadata(agent_id, &art_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_runtime_read(&caller, &descriptor)?;
    let body =
        table_page(store.as_ref(), agent_id, &art_id, &descriptor, &page).map_err(|refusal| {
            match refusal {
                PageRefusal::NotFound => StatusCode::NOT_FOUND,
                PageRefusal::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                PageRefusal::Unavailable => StatusCode::INTERNAL_SERVER_ERROR,
            }
        })?;
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// Resolve the calling principal from request context. Honours all three
/// auth strategies the gateway accepts so REST endpoints can apply per-user
/// filtering regardless of how the client authenticated.
// ───────────────────────── Artifacts v2 API (DW-553) ─────────────────────────

#[derive(serde::Deserialize)]
struct ArtifactsQuery {
    token: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    tag: Option<String>,
    visibility: Option<String>,
    q: Option<String>,
}

#[derive(serde::Deserialize)]
struct ArtifactCreateBody {
    id: Option<String>,
    title: Option<String>,
    content: Option<String>,
    artifact_type: Option<String>,
    visibility: Option<String>,
    tags: Option<Vec<String>>,
    token: Option<String>,
}

#[derive(serde::Deserialize)]
struct ArtifactUpdateBody {
    content: Option<String>,
    visibility: Option<String>,
    token: Option<String>,
}

/// Map a gateway caller to an artifact-store ACL principal + owner key.
/// `None` when the request carries no usable identity (and isn't the owner
/// token), which the handlers turn into `401`.
fn artifact_principal(
    ctx: &CallerContext,
) -> Option<(String, workforce_orchestrator::artifact_store::Caller)> {
    use workforce_orchestrator::artifact_store::Caller;
    let key = ctx.identity_id.clone().or_else(|| ctx.device_id.clone());
    if ctx.is_admin {
        Some((key.unwrap_or_else(|| "owner".to_string()), Caller::admin()))
    } else {
        let key = key?;
        Some((key.clone(), Caller::user(key)))
    }
}

fn artifact_to_json(
    a: &workforce_orchestrator::artifact_store::Artifact,
    include_content: bool,
) -> serde_json::Value {
    let mut j = serde_json::json!({
        "id": a.id,
        "title": a.title,
        "artifact_type": a.artifact_type.to_string(),
        "visibility": a.visibility.to_string(),
        "tags": a.tags,
        "status": a.status.to_string(),
        "version": a.version,
        "owner_id": a.owner_id,
        "created_by": a.created_by,
        "last_modified_by": a.last_modified_by,
        "created_at": a.created_at.to_rfc3339(),
        "updated_at": a.updated_at.to_rfc3339(),
    });
    if include_content {
        j["content"] = a.content.clone().into();
    }
    j
}

fn artifact_filter(
    type_: Option<&str>,
    visibility: Option<&str>,
    tag: Option<String>,
) -> workforce_orchestrator::artifact_store::ArtifactFilter {
    use workforce_orchestrator::artifact_store::{ArtifactFilter, ArtifactType, Visibility};
    ArtifactFilter {
        artifact_type: type_.map(|s| s.parse::<ArtifactType>().unwrap()),
        visibility: visibility.map(|s| s.parse::<Visibility>().unwrap()),
        tag: tag.filter(|t| !t.is_empty()),
    }
}

async fn artifacts_list(
    headers: axum::http::HeaderMap,
    Query(q): Query<ArtifactsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    let (_, caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    let filter = artifact_filter(q.type_.as_deref(), q.visibility.as_deref(), q.tag);
    let items = store
        .list(&caller, &filter)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "artifacts": items.iter().map(|a| artifact_to_json(a, false)).collect::<Vec<_>>(),
    })))
}

async fn artifacts_search(
    headers: axum::http::HeaderMap,
    Query(q): Query<ArtifactsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    let (_, caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    let query = q.q.clone().unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }
    let filter = artifact_filter(q.type_.as_deref(), q.visibility.as_deref(), q.tag);
    let hits = store
        .search(&query, &caller, &filter, 30)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "results": hits.iter().map(|h| {
            let mut j = artifact_to_json(&h.artifact, false);
            j["score"] = h.score.into();
            j
        }).collect::<Vec<_>>(),
    })))
}

async fn artifacts_get(
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ArtifactsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    let (_, caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    match store
        .read(&id, &caller)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(a) => Ok(Json(artifact_to_json(&a, true))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn artifacts_create(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ArtifactCreateBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use workforce_orchestrator::artifact_store::{Artifact, ArtifactType, Visibility};
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, body.token.as_deref()).await;
    let (owner, _caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    let id = body
        .id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let artifact_type = body
        .artifact_type
        .as_deref()
        .unwrap_or("notes")
        .parse::<ArtifactType>()
        .unwrap();
    let visibility = body
        .visibility
        .as_deref()
        .unwrap_or("private")
        .parse::<Visibility>()
        .unwrap();
    let title = body.title.unwrap_or_else(|| id.clone());
    let content = body.content.unwrap_or_default();
    let a = Artifact::new(
        id,
        artifact_type,
        title,
        content,
        owner.clone(),
        owner,
        visibility,
        body.tags.unwrap_or_default(),
    );
    store.create(&a).await.map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(artifact_to_json(&a, true)))
}

async fn artifacts_update(
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ArtifactUpdateBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use workforce_orchestrator::artifact_store::Visibility;
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, body.token.as_deref()).await;
    let (owner, caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    if let Some(vis) = body.visibility.as_deref() {
        let v = vis.parse::<Visibility>().unwrap();
        store
            .set_visibility(&id, v, &caller)
            .await
            .map_err(|_| StatusCode::FORBIDDEN)?;
    }
    if let Some(content) = body.content.as_deref() {
        store
            .update(&id, content, &owner, &caller)
            .await
            .map_err(|_| StatusCode::FORBIDDEN)?;
    }
    match store
        .read(&id, &caller)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(a) => Ok(Json(artifact_to_json(&a, true))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn artifacts_delete(
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ArtifactsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<StatusCode, StatusCode> {
    let store = state
        .artifact_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ctx = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    let (_, caller) = artifact_principal(&ctx).ok_or(StatusCode::UNAUTHORIZED)?;
    store
        .delete(&id, &caller)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resolve_caller_context(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> CallerContext {
    let token = match query_token.map(str::to_owned).or_else(|| {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_owned)
    }) {
        Some(t) if !t.is_empty() => t,
        _ => return CallerContext::default(),
    };

    // Admin token grants cross-user visibility but no specific identity.
    if crate::auth::constant_time_token_eq(&token, &state.auth_token) {
        return CallerContext {
            is_admin: true,
            scopes: vec![crate::jwt::SCOPE_WILDCARD.to_string()],
            ..Default::default()
        };
    }

    if let Some(ref jwt_config) = state.jwt_config {
        if let Ok(claims) = crate::jwt::validate_token(jwt_config, &token) {
            return crate::jwt::current_claims(state.identity_store.as_ref(), claims).into();
        }
    }

    // Paired-device token (wfp_*): validate and resolve the linked identity.
    let token_hint = token_prefix_hint(&token);
    if let Some(ref pairing) = state.device_pairing {
        match pairing.validate_token_async(token).await {
            Ok(Some(device)) => {
                let identity_id = state
                    .identity_store
                    .as_ref()
                    .and_then(|store| identity_for_device(store.as_ref(), &device.id));
                return CallerContext {
                    identity_id,
                    device_id: Some(device.id),
                    is_admin: false,
                    scopes: device.scopes,
                    auth_failure: None,
                };
            }
            Ok(None) => {}
            Err(e) => {
                warn!(
                    token_prefix = %token_hint,
                    error = %e,
                    "Pairing store error while validating device token — failing closed"
                );
                return CallerContext {
                    auth_failure: Some(AuthFailure::StoreError),
                    ..Default::default()
                };
            }
        }
    }

    // info, not warn: routine token expiry (web JWTs especially) lands here
    // on every request until the client refreshes, across every handler that
    // resolves a caller — warn would flood prod logs with non-actionable
    // noise while staying visible enough to diagnose a stranded client.
    info!(
        token_prefix = %token_hint,
        "Presented credential failed authentication (invalid or expired token)"
    );
    CallerContext {
        auth_failure: Some(AuthFailure::InvalidCredentials),
        ..Default::default()
    }
}

/// First eight characters of a token for log correlation — long enough to
/// tell tokens apart, far too short to replay. Char-wise so an arbitrary
/// (attacker-supplied) header can't panic a byte slice mid-codepoint.
fn token_prefix_hint(token: &str) -> String {
    token.chars().take(8).collect()
}

/// Canonical channel name for iOS / paired-device chat conversations, and the
/// channel a `/ws` connection anchors on when it names none.
pub const IOS_CHANNEL: &str = "ios";

/// `GET /api/me` — return the calling principal's identity summary.
///
/// Works for any auth method the gateway accepts: JWT bearer, paired-device
/// token (`wfp_*`), or admin token. The iOS app calls this immediately after
/// pairing succeeds to retrieve its `identity_id` (which is otherwise only
/// known to the server via the `("ios_device", device_id)` link), so it can
/// satisfy `isLoggedIn` without a separate JWT login flow.
///
/// Returns 401 if the caller is anonymous, 503 if no identity store is
/// configured, 404 if the caller resolves to an identity that no longer
/// exists.
async fn get_me(
    headers: axum::http::HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let identity_id = caller
        .identity_id
        .as_deref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let store = state
        .identity_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let uuid = uuid::Uuid::parse_str(identity_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let identity = store
        .get_by_id(uuid)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "id": identity.id.to_string(),
        "display_name": identity.display_name,
        "email": identity.email,
        "role": identity.role.as_str(),
        "status": identity.status.as_str(),
        "device_id": caller.device_id,
        "linked_ios_device": linked_ios_device(&identity, caller.device_id.as_deref()),
        "picture": picture_info(&state, &crate::pictures::Owner::identity(identity.id.to_string())).await,
    })))
}

/// The iPhone `/api/me` reports as this person's: the calling device when it
/// is one of theirs, else their first. A person may hold several, each under
/// its own `ios_device:{id}` key, or one under the bare key pairing used to
/// write.
fn linked_ios_device<'a>(
    identity: &'a workforce_identity::types::Identity,
    calling_device: Option<&str>,
) -> Option<&'a str> {
    let kind = workforce_pairing::types::DEFAULT_DEVICE_KIND;
    let mut theirs = identity
        .linked_ids
        .iter()
        .filter(|(key, _)| {
            key.as_str() == kind
                || key
                    .strip_prefix(kind)
                    .is_some_and(|rest| rest.starts_with(':'))
        })
        .map(|(_, id)| id.as_str())
        .collect::<Vec<_>>();
    theirs.sort_unstable();
    calling_device
        .and_then(|d| theirs.iter().copied().find(|id| *id == d))
        .or_else(|| theirs.first().copied())
}

/// `GET /api/me/capabilities` — the authenticated caller's role and the
/// per-capability decision view from the documented authorization matrix.
/// Self-scoped: it resolves the caller's own identity and reports only their
/// capabilities (booleans, never secrets). The exportable audit matrix the
/// design doc calls for; `identity.whoami` is its agent-tool twin.
async fn get_my_capabilities(
    headers: axum::http::HeaderMap,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let identity_id = caller
        .identity_id
        .as_deref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let store = state
        .identity_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let uuid = uuid::Uuid::parse_str(identity_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let identity = store
        .get_by_id(uuid)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let summary = crate::authz::capability_summary(
        identity.effective_role_key(),
        identity.status,
        &crate::authz::capability_matrix(),
    );
    Ok(Json(serde_json::json!({
        "identity_id": identity.id.to_string(),
        "display_name": identity.display_name,
        "role": summary.role,
        "capabilities": summary.capabilities,
    })))
}

/// True for channels representing a direct human-driven conversation.
/// Filters the iOS / web session-list views so they don't show
/// workflow-step sessions, slack-bot routing sessions, etc.
fn is_conversational_channel(channel: &str) -> bool {
    workforce_common::channels::is_conversational(channel)
}

// ── Agent CRUD & Detail Endpoints (DW-421) ────────────────────────────────

/// Identity files that can be read/written via the API. HEARTBEAT.md is the
/// agent's check-in list, which Desk edits beside the others.
const ALLOWED_IDENTITY_FILES: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "TOOLS.md",
    "IDENTITY.md",
    "WELCOME.md",
    "HEARTBEAT.md",
];

/// Helper: resolve an agent service by ID, returning an appropriate error.
pub(crate) fn resolve_agent(
    state: &GatewayState,
    id: &str,
) -> Result<Arc<crate::serve::AgentService>, (StatusCode, Json<serde_json::Value>)> {
    state.registry.get(id).ok_or_else(|| not_found_agent(id))
}

pub(crate) fn not_found_agent(id: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": format!("agent '{}' not found", id)})),
    )
}

/// Team IDs (as strings) that `caller_identity` belongs to. Returns an
/// empty vec when the caller has no identity, no team store is wired, or
/// the identity isn't a valid UUID. Shared between `list_agents` and the
/// per-agent visibility predicate so both apply the same rule.
fn caller_team_ids(state: &GatewayState, caller_identity: Option<&str>) -> Vec<String> {
    crate::team_reach::TeamReach::new(
        state.team_store.clone(),
        caller_identity.map(str::to_string),
    )
    .team_ids()
    .to_vec()
}

/// Single source of truth for "can this caller see this agent". Admin
/// bypass, direct-owner match (User-typed), and team-member match
/// (Team-typed). Pure: testable without `GatewayState`. Used by both
/// `list_agents` (filters listings) and `caller_can_access_agent` (gates
/// per-agent reads — closes the IDOR from audit F1).
fn caller_can_see_agent_owner(
    caller: &CallerContext,
    owner_id: Option<&str>,
    owner_type: &workforce_common::settings::OwnerType,
    caller_team_ids: &[String],
) -> bool {
    if caller.is_admin {
        return true;
    }
    let Some(owner_id) = owner_id else {
        return false;
    };
    if caller.owns(owner_id) {
        return true;
    }
    *owner_type == workforce_common::settings::OwnerType::Team
        && caller_team_ids.iter().any(|t| t == owner_id)
}

/// Outcome of the `/api/agents` roster-visibility rule for one agent.
struct RosterVisibility {
    /// Whether the caller may see the agent in the listing at all.
    visible: bool,
    /// Whether the agent's owner id/type may be disclosed to this caller.
    /// True only on the ownership path (admin / owner / team); access-policy
    /// visibility must not leak owner UUIDs (audit F1).
    disclose_owner: bool,
}

/// Roster-visibility rule for a single agent, shared between the handler and
/// its tests. Pure: testable without `GatewayState`.
///
/// An agent is visible when the caller owns it (admin / direct owner / team —
/// via [`caller_can_see_agent_owner`]) OR when the caller is **authenticated**
/// and the agent's access policy admits them (`access_allows`). The
/// `is_authenticated()` guard on the access path is load-bearing: it keeps an
/// anonymous caller from seeing Open/Blocklist agents and re-opening the F1
/// roster leak. The access-policy path also admits the read-only
/// `/api/pipelines` routes (through [`AgentViewer`]), which deliberately show
/// an agent's pipeline definitions and items to everyone the agent admits; it
/// is NOT applied to the per-agent management reads gated by
/// [`caller_can_access_agent`].
fn roster_visibility(
    caller: &CallerContext,
    owner_id: Option<&str>,
    owner_type: &workforce_common::settings::OwnerType,
    caller_team_ids: &[String],
    access_allows: bool,
) -> RosterVisibility {
    let owner_visible = caller_can_see_agent_owner(caller, owner_id, owner_type, caller_team_ids);
    let access_visible = caller.is_authenticated() && access_allows;
    RosterVisibility {
        visible: owner_visible || access_visible,
        disclose_owner: owner_visible,
    }
}

/// A caller judged against the `/api/agents` roster rule: their teams and
/// paired-device surface are resolved once, then each agent is visible by
/// ownership or by its access policy.
pub(crate) struct AgentViewer<'a> {
    state: &'a GatewayState,
    caller: CallerContext,
    team_ids: Vec<String>,
    device_surface_id: Option<String>,
}

impl<'a> AgentViewer<'a> {
    pub(crate) fn new(state: &'a GatewayState, caller: CallerContext) -> Self {
        Self {
            team_ids: caller_team_ids(state, caller.identity_id.as_deref()),
            device_surface_id: caller
                .device_id
                .as_deref()
                .map(|d| format!("ios_device:{d}")),
            state,
            caller,
        }
    }

    fn visibility(
        &self,
        agent_id: &str,
        owner_id: Option<&str>,
        owner_type: &workforce_common::settings::OwnerType,
    ) -> RosterVisibility {
        roster_visibility(
            &self.caller,
            owner_id,
            owner_type,
            &self.team_ids,
            self.access_allows(agent_id),
        )
    }

    /// The caller as the capability gate resolves it: live role and status
    /// from the identity store, a paired device as a member.
    pub(crate) fn principal(&self) -> crate::authz::Principal {
        gateway_principal(&self.caller, self.state.identity_store.as_ref())
    }

    /// Whether the roster shows `agent_id` to this caller. An unregistered
    /// agent never shows: the access gate admits any agent it holds no
    /// policy for, and removing an agent leaves its policy behind.
    pub(crate) fn can_see(&self, agent_id: &str) -> bool {
        self.state.registry.get(agent_id).is_some_and(|service| {
            let (owner_id, owner_type) = service.effective_owner();
            self.visibility(agent_id, owner_id.as_deref(), &owner_type)
                .visible
        })
    }

    /// The agent's access-policy verdict for this caller, with the same
    /// `ios_device:<id>` surface fallback the chat ingress applies.
    fn access_allows(&self, agent_id: &str) -> bool {
        let surface_id = self.device_surface_id.as_deref();
        matches!(
            self.state
                .access_gate
                .check(crate::access_gate::AccessRequest {
                    agent_id,
                    identity_id: self.caller.identity_id.as_deref(),
                    surface_ids: surface_id.as_slice(),
                    is_external: false,
                }),
            workforce_common::settings::AccessDecision::Allow
        )
    }
}

/// True if `caller` may read the agent's config. Wraps the pure
/// predicate with the team_store lookup.
pub(crate) fn caller_can_access_agent(
    state: &GatewayState,
    caller: &CallerContext,
    service: &crate::serve::AgentService,
) -> bool {
    let (owner_id, owner_type) = service.effective_owner();
    let team_ids = caller_team_ids(state, caller.identity_id.as_deref());
    caller_can_see_agent_owner(caller, owner_id.as_deref(), &owner_type, &team_ids)
}

/// Compose the read-side authorization preamble for `/api/agents/{id}` family
/// endpoints: resolve caller, require auth + `agents` scope, resolve the
/// target agent, and verify access. Mirrors `authorise_session_action` in
/// `crate::session_handlers`. Non-owners get 404 (not 403) so they cannot
/// enumerate agent IDs by status code — same shape as the session-IDOR fix
/// from 2026-05-14 (audit F2).
async fn authorise_agent_read(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    agent_id: &str,
) -> Result<Arc<crate::serve::AgentService>, (StatusCode, Json<serde_json::Value>)> {
    authorise_agent_reader(state, headers, query_token, agent_id)
        .await
        .map(|(service, _)| service)
}

/// [`authorise_agent_read`], keeping the caller it resolved.
pub(crate) async fn authorise_agent_reader(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    agent_id: &str,
) -> Result<(Arc<crate::serve::AgentService>, CallerContext), (StatusCode, Json<serde_json::Value>)>
{
    let caller = resolve_caller_context(state, headers, query_token).await;
    if !caller.is_authenticated() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        ));
    }
    if !caller.has_scope(crate::jwt::SCOPE_AGENTS) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "scope 'agents' required"})),
        ));
    }
    let service = resolve_agent(state, agent_id)?;
    if !caller_can_access_agent(state, &caller, &service) {
        return Err(not_found_agent(agent_id));
    }
    Ok((service, caller))
}

/// `GET /api/agents/{id}` — Full agent detail.
async fn get_agent(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let service = authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;
    let agent_config = service.config.agent_by_id(&id);

    // Discover identity files present on disk
    let mut identity_files = Vec::new();
    for &filename in ALLOWED_IDENTITY_FILES {
        let path = service.workspace.join(filename);
        if path.exists() {
            identity_files.push(filename.to_string());
        }
    }

    let session_count = service
        .sessions
        .list(None, 1000)
        .map(|s| s.len())
        .unwrap_or(0);

    let response = workforce_gateway_client::AgentDetailResponse {
        id: id.clone(),
        name: service.display_name(),
        model: agent_config.and_then(|a| a.model.clone()),
        capabilities: agent_config
            .map(|a| a.capabilities.clone())
            .unwrap_or_default(),
        default: id == state.registry.default_id(),
        identity_files,
        tool_count: service.tools.len(),
        session_count,
        workspace: Some(service.workspace.display().to_string()),
        owner_id: {
            let (oid, _) = service.effective_owner();
            oid
        },
        owner_type: {
            let (_, ot) = service.effective_owner();
            Some(format!("{:?}", ot).to_lowercase())
        },
    };

    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// `GET /api/agents/{id}/tools` — Agent tools with full JSON schemas.
async fn get_agent_tools(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let service = authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;

    let defs = service.tools.definitions();

    let tools: Vec<serde_json::Value> = defs
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "description": d.description,
                "input_schema": d.input_schema,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "agent_id": id,
        "total": tools.len(),
        "tools": tools,
    })))
}

/// `GET /api/agents/{id}/identity/{filename}` — Read an identity file.
async fn get_agent_identity(
    Path((id, filename)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let service = resolve_agent(&state, &id)?;

    // Whitelist validation — prevent path traversal
    if !ALLOWED_IDENTITY_FILES.contains(&filename.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("invalid identity file '{}'. Allowed: {:?}", filename, ALLOWED_IDENTITY_FILES)
            })),
        ));
    }

    match workforce_common::config_files::read(&service.workspace.join(&filename)) {
        Ok(Some(content)) => Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            content,
        )
            .into_response()),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("identity file '{}' not found", filename)})),
        )),
        Err(e) => {
            warn!(agent_id = %id, file = %filename, error = %e, "Failed to read identity file");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read identity file"})),
            ))
        }
    }
}

/// Capture file-canonical revisions (identity files, per-agent config.toml)
/// around a guarded write. Any out-of-band drift on disk (agent self-edits,
/// container edits) is recorded first as an unattributed revision so the
/// pre-edit state is never lost, then the new content as the caller's edit.
/// Errors when the store is present but the insert fails — a change that
/// can't be audited doesn't happen. No-op (with nothing recorded) when
/// Postgres is unavailable.
async fn record_file_revision(
    state: &GatewayState,
    kind: &str,
    ref_id: &str,
    path: &std::path::Path,
    new_content: &str,
    edit: workforce_orchestrator::workflow_store::EditContext<'_>,
) -> Result<(), String> {
    crate::agent_config_write::record_revision(
        state.def_store.as_deref(),
        kind,
        ref_id,
        path,
        new_content,
        edit,
    )
    .await
}

/// `PUT /api/agents/{id}/identity/{filename}` — Write an identity file.
async fn put_agent_identity(
    Path((id, filename)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &id)?;

    // Whitelist validation
    if !ALLOWED_IDENTITY_FILES.contains(&filename.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("invalid identity file '{}'. Allowed: {:?}", filename, ALLOWED_IDENTITY_FILES)
            })),
        ));
    }

    let path = service.workspace.join(&filename);

    // Ensure the workspace directory exists
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!(agent_id = %id, error = %e, "Failed to create workspace directory");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to create workspace directory"})),
            ));
        }
    }

    // Revision capture BEFORE the overwrite — who/when + rollback content.
    let edit = workforce_orchestrator::workflow_store::EditContext::new(
        "web",
        caller.identity_id.as_deref(),
    );
    let ref_id = format!("{id}/{filename}");
    if let Err(e) = record_file_revision(
        &state,
        workforce_orchestrator::workflow_store::KIND_IDENTITY_FILE,
        &ref_id,
        &path,
        &body,
        edit,
    )
    .await
    {
        warn!(agent_id = %id, file = %filename, error = %e, "Identity revision capture failed — refusing write");
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": format!("revision capture failed: {e}")})),
        ));
    }

    let written = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            workforce_common::config_files::replace(&path, body.as_bytes()).map(|()| body.len())
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
    };
    match written {
        Ok(bytes) => {
            info!(agent_id = %id, file = %filename, "Identity file updated via API");
            Ok(Json(serde_json::json!({
                "updated": filename,
                "agent_id": id,
                "bytes": bytes,
            })))
        }
        Err(e) => {
            warn!(agent_id = %id, file = %filename, error = %e, "Failed to write identity file");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to write identity file"})),
            ))
        }
    }
}

/// `GET /api/agents/{id}/identity/{filename}/revisions` — revision metadata,
/// newest first. Admin-gated like the identity read itself.
async fn agent_identity_revisions_list(
    Path((id, filename)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let _service = resolve_agent(&state, &id)?;
    if !ALLOWED_IDENTITY_FILES.contains(&filename.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("invalid identity file '{}'. Allowed: {:?}", filename, ALLOWED_IDENTITY_FILES)
            })),
        ));
    }
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let ref_id = format!("{id}/{filename}");
    let revisions = store
        .list_revisions(ws::KIND_IDENTITY_FILE, &ref_id, None)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

/// `GET /api/agents/{id}/identity/{filename}/revisions/{rev}` — one revision
/// with its full content (for the diff view).
async fn agent_identity_revision_get(
    Path((id, filename, rev)): Path<(String, String, i32)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let _service = resolve_agent(&state, &id)?;
    if !ALLOWED_IDENTITY_FILES.contains(&filename.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("invalid identity file '{}'. Allowed: {:?}", filename, ALLOWED_IDENTITY_FILES)
            })),
        ));
    }
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let ref_id = format!("{id}/{filename}");
    let revision = store
        .get_revision(ws::KIND_IDENTITY_FILE, &ref_id, None, rev)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("revision {rev} not found")})),
        ))?;
    Ok(Json(serde_json::json!(revision)))
}

/// `POST /api/agents/{id}/identity/{filename}/revisions/{rev}/restore` —
/// write revision `rev`'s content back to disk through the guarded path and
/// record a NEW `restore` revision (history is never rewritten).
async fn agent_identity_revision_restore(
    Path((id, filename, rev)): Path<(String, String, i32)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &id)?;
    if !ALLOWED_IDENTITY_FILES.contains(&filename.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("invalid identity file '{}'. Allowed: {:?}", filename, ALLOWED_IDENTITY_FILES)
            })),
        ));
    }
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let ref_id = format!("{id}/{filename}");
    let revision = store
        .get_revision(ws::KIND_IDENTITY_FILE, &ref_id, None, rev)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("revision {rev} not found")})),
        ))?;
    let content = revision.content.unwrap_or_default();

    let path = service.workspace.join(&filename);
    let edit = ws::EditContext::restore(caller.identity_id.as_deref(), rev);
    let ref_id = format!("{id}/{filename}");
    if let Err(e) = record_file_revision(
        &state,
        workforce_orchestrator::workflow_store::KIND_IDENTITY_FILE,
        &ref_id,
        &path,
        &content,
        edit,
    )
    .await
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": format!("revision capture failed: {e}")})),
        ));
    }
    if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
        warn!(agent_id = %id, file = %filename, error = %e, "Failed to restore identity file");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to write identity file"})),
        ));
    }
    info!(agent_id = %id, file = %filename, from_rev = rev, "Identity file restored via API");
    Ok(Json(serde_json::json!({
        "restored": filename,
        "agent_id": id,
        "from_rev": rev,
    })))
}

#[derive(Deserialize)]
struct StatsQuery {
    /// Period: "1d", "7d", "30d", "90d". Default: "7d".
    period: Option<String>,
    /// Auth token from query string.
    token: Option<String>,
}

/// `GET /api/agents/{id}/stats` — Per-agent usage statistics.
async fn get_agent_stats(
    Path(id): Path<String>,
    Query(query): Query<StatsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let service = authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;

    let period_str = query.period.as_deref().unwrap_or("7d");
    let days: i64 = match period_str {
        "1d" => 1,
        "7d" => 7,
        "30d" => 30,
        "90d" => 90,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid period — use 1d, 7d, 30d, or 90d"})),
            ))
        }
    };

    let now = chrono::Utc::now();
    let from = now - chrono::Duration::days(days);

    let summary = service.cost_tracker.summary(from, now);

    // Filter to just this agent's cost
    let agent_cost = summary.by_agent.get(&id).copied().unwrap_or(0.0);

    // Build per-model breakdown for this agent from in-memory records
    // Use the full summary which already has the data we need, but filter agent-specific
    // We need to access the records directly — the summary doesn't filter by agent for by_model.
    // Instead, use the summary data: agent total + overall model breakdown as context.

    Ok(Json(serde_json::json!({
        "agent_id": id,
        "period": period_str,
        "total_cost_usd": agent_cost,
        "total_input_tokens": summary.total_input_tokens,
        "total_output_tokens": summary.total_output_tokens,
        "record_count": summary.record_count,
        "by_model": summary.by_model,
    })))
}

/// `POST /api/agents` — Create a new agent.
async fn create_agent(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<workforce_gateway_client::CreateAgentRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;

    // Validate ID format (alphanumeric + hyphens + underscores)
    if body.id.is_empty()
        || !body
            .id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "invalid agent ID — use alphanumeric, hyphens, underscores only"}),
            ),
        ));
    }

    // Check if agent already exists
    if state.registry.get(&body.id).is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("agent '{}' already exists", body.id)})),
        ));
    }

    if let Some(scope) = body.agent_recall_scope.as_deref() {
        if !matches!(scope, "shared" | "isolated") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("invalid agent_recall_scope '{scope}' — shared | isolated")
                })),
            ));
        }
    }

    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(&state)?;

    // Build the raw agent entry and write it to the OVERLAY — the base
    // workforce.toml is infra-owned (read-only in production) and the next
    // deploy would clobber it anyway. The overlay lives in the data dir and
    // survives deploys.
    let mut agent = toml::Table::new();
    agent.insert("id".to_string(), toml::Value::String(body.id.clone()));
    if let Some(ref name) = body.name {
        agent.insert("name".to_string(), toml::Value::String(name.clone()));
    }
    if let Some(ref model) = body.model {
        agent.insert("model".to_string(), toml::Value::String(model.clone()));
    }
    if let Some(ref prompt) = body.system_prompt {
        agent.insert(
            "system_prompt".to_string(),
            toml::Value::String(prompt.clone()),
        );
    }
    if !body.capabilities.is_empty() {
        agent.insert(
            "capabilities".to_string(),
            toml::Value::Array(
                body.capabilities
                    .iter()
                    .map(|c| toml::Value::String(c.clone()))
                    .collect(),
            ),
        );
    }
    if body.default {
        agent.insert("default".to_string(), toml::Value::Boolean(true));
    }
    if let Some(ref oid) = body.owner_id {
        agent.insert("owner_id".to_string(), toml::Value::String(oid.clone()));
        let ot = body.owner_type.as_deref().unwrap_or("user");
        agent.insert(
            "owner_type".to_string(),
            toml::Value::String(ot.to_string()),
        );
    }
    if let Some(scope) = body.agent_recall_scope.as_deref() {
        let mut memory = toml::Table::new();
        memory.insert(
            "agent_recall_scope".to_string(),
            toml::Value::String(scope.to_string()),
        );
        agent.insert("memory".to_string(), toml::Value::Table(memory));
    }

    if let Err(e) = crate::config_writer::overlay_upsert_agent(
        &config_path,
        &data_dir,
        agent,
        caller.identity_id.as_deref(),
        "web",
        state.def_store.as_deref(),
        &lock_manager,
    )
    .await
    {
        // F8 (2026-05-15): log paths server-side but don't echo FS layout
        // back to the client.
        warn!(agent_id = %body.id, error = %e, "Failed to write agent to config overlay");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to save agent: {e}")})),
        ));
    }

    // Wake the watcher and wait (bounded) for the spawn, so creation returns
    // once the agent actually resolves — the UI navigates straight to a live
    // agent instead of racing the watcher's poll interval.
    let live = match &state.overlay_kick {
        Some(kick) => {
            kick.notify_one();
            wait_until(std::time::Duration::from_secs(20), || {
                state.registry.get(&body.id).is_some()
            })
            .await
        }
        None => false,
    };

    info!(agent_id = %body.id, live, "Agent created via API (overlay)");

    Ok(Json(serde_json::json!({
        "created": body.id,
        "live": live,
        "restart_required": false,
        "message": if live {
            "Agent created and live."
        } else {
            "Agent saved to the config overlay — the hub loads it live within a few seconds."
        },
    })))
}

/// Poll `cond` every 150ms until it holds or `timeout` elapses.
async fn wait_until(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// The longest display name an agent may be given.
const AGENT_NAME_MAX_CHARS: usize = 100;

/// A rename's name, trimmed: 1–100 characters.
fn agent_name(name: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let name = name.trim();
    match name.chars().count() {
        1..=AGENT_NAME_MAX_CHARS => Ok(name.to_string()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("an agent's name is 1 to {AGENT_NAME_MAX_CHARS} characters")
            })),
        )),
    }
}

/// Writes a new display name and instructions to the agent's config overlay
/// entry, where every later build of it reads them (a restart included), and
/// shows the name at once. An empty `system_prompt` removes the instructions.
async fn persist_name_and_prompt(
    state: &GatewayState,
    caller: &CallerContext,
    id: &str,
    name: Option<String>,
    system_prompt: Option<String>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.is_none() && system_prompt.is_none() {
        return Ok(());
    }
    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(state)?;
    let written = name.clone();
    crate::config_writer::overlay_update_agent_fields(
        &config_path,
        &data_dir,
        id,
        move |entry| {
            if let Some(name) = written {
                entry.insert("name".to_string(), toml::Value::String(name));
            }
            match system_prompt {
                Some(prompt) if prompt.trim().is_empty() => {
                    entry.remove("system_prompt");
                }
                Some(prompt) => {
                    entry.insert("system_prompt".to_string(), toml::Value::String(prompt));
                }
                None => {}
            }
        },
        caller.identity_id.as_deref(),
        "web",
        state.def_store.as_deref(),
        &lock_manager,
    )
    .await
    .map_err(|e| {
        warn!(agent_id = %id, error = %e, "Failed to save an agent's name to the config overlay");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "saving the agent failed"})),
        )
    })?;
    if let (Some(name), Some(service)) = (name, state.registry.get(id)) {
        *service.name_override.write() = Some(name);
    }
    if let Some(kick) = &state.overlay_kick {
        kick.notify_one();
    }
    Ok(())
}

/// `PUT /api/agents/{id}` — Update an existing agent.
async fn update_agent(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<workforce_gateway_client::UpdateAgentRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    apply_agent_update(&state, &caller, &id, &body).await?;
    Ok(Json(serde_json::json!({
        "updated": id,
        "applied": {
            "model": body.model,
            "name": body.name,
            "system_prompt": body.system_prompt.as_ref().map(|_| "(set)"),
            "capabilities": body.capabilities,
            "default": body.default,
            "owner_id": body.owner_id,
            "owner_type": body.owner_type,
        },
        "note": "Name, owner and model changes are applied live. Other changes require restart.",
    })))
}

/// `PATCH /api/agents/{id}` — the update `PUT` makes (a rename, with the
/// instructions that name the agent), answered with the agent as
/// `GET /api/agents` lists it.
async fn patch_agent(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<workforce_gateway_client::UpdateAgentRequest>,
) -> Result<Json<workforce_gateway_client::AgentInfo>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    apply_agent_update(&state, &caller, &id, &body).await?;
    let service = resolve_agent(&state, &id)?;
    let (owner_id, owner_type) = service.effective_owner();
    let model = service.current_default_model.read().clone();
    let provider = service.provider_kind.read().canonical_name().to_string();
    Ok(Json(workforce_gateway_client::AgentInfo {
        id: id.clone(),
        name: service.display_name(),
        model: Some(model).filter(|m| !m.is_empty()),
        provider: Some(provider),
        capabilities: service
            .config
            .agent_by_id(&id)
            .map(|a| a.capabilities.clone())
            .unwrap_or_default(),
        default: id == state.registry.default_id(),
        owner_id,
        owner_type: Some(format!("{owner_type:?}").to_lowercase()),
        picture: picture_info(&state, &crate::pictures::Owner::agent(&id)).await,
    }))
}

async fn apply_agent_update(
    state: &Arc<GatewayState>,
    caller: &CallerContext,
    id: &str,
    body: &workforce_gateway_client::UpdateAgentRequest,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let id = id.to_string();
    let state = Arc::clone(state);
    let caller = caller.clone();

    // Verify agent exists
    let _service = resolve_agent(&state, &id)?;

    let name = body.name.as_deref().map(agent_name).transpose()?;
    persist_name_and_prompt(&state, &caller, &id, name, body.system_prompt.clone()).await?;

    // Live model override (no restart required)
    if let Some(ref model) = body.model {
        if let Some(service) = state.registry.get(&id) {
            let mut active = service.active_model.write();
            *active = if model.is_empty() {
                None
            } else {
                Some(model.clone())
            };
        }
    }

    if body.owner_id.is_some() || body.owner_type.is_some() {
        // Snapshot the prior owner before applying the override so the
        // audit event below can record from → to. (F10, 2026-05-15.)
        let prior_owner = state
            .registry
            .get(&id)
            .map(|s| s.effective_owner())
            .and_then(|(oid, _)| oid);

        // Apply live override so the change is visible immediately
        if let Some(service) = state.registry.get(&id) {
            let ot = body
                .owner_type
                .as_deref()
                .map(|s| match s {
                    "team" => workforce_common::settings::OwnerType::Team,
                    _ => workforce_common::settings::OwnerType::User,
                })
                .unwrap_or_default();
            let oid = body.owner_id.clone().filter(|s| !s.is_empty());
            *service.owner_override.write() = Some((oid, ot));
        }

        // Persist to the config overlay (the base file is infra-owned). The
        // raw entry is pulled from overlay/base tables — never the expanded
        // struct — so unexpanded ${VAR} values survive the round-trip. Uses
        // the hub-level config path: the target service's own config_path is
        // None for live-spawned and startup "additional" agents.
        if let Ok((config_path, data_dir, lock_manager)) = resolve_overlay_ctx(&state) {
            let owner_id = body.owner_id.clone();
            let owner_type = body.owner_type.clone();
            if let Err(e) = crate::config_writer::overlay_update_agent_fields(
                &config_path,
                &data_dir,
                &id,
                move |entry| match owner_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(oid) => {
                        entry.insert("owner_id".to_string(), toml::Value::String(oid.to_string()));
                        entry.insert(
                            "owner_type".to_string(),
                            toml::Value::String(
                                owner_type.as_deref().unwrap_or("user").to_string(),
                            ),
                        );
                    }
                    None => {
                        entry.remove("owner_id");
                        entry.remove("owner_type");
                    }
                },
                caller.identity_id.as_deref(),
                "web",
                state.def_store.as_deref(),
                &lock_manager,
            )
            .await
            {
                warn!(error = %e, "Failed to persist owner change to config overlay");
            }
            if let Some(kick) = &state.overlay_kick {
                kick.notify_one();
            }
        }

        // Audit F10 (2026-05-15) — AccessAuditEvent carries the new owner;
        // the prior owner is on the tracing line below.
        let new_owner = body.owner_id.clone().filter(|s| !s.is_empty());
        workforce_memory::access_audit::emit(
            workforce_memory::access_audit::AccessAuditEvent::new(
                workforce_memory::access_audit::AuditOperation::AgentOwnerChange,
                workforce_memory::access_audit::AuditDecision::Allow,
                caller
                    .identity_id
                    .as_deref()
                    .or(caller.device_id.as_deref()),
                new_owner.as_deref(),
                Some(&id),
            ),
        );
        info!(
            target: "audit.agent_owner_change",
            agent_id = %id,
            from_owner = ?prior_owner,
            to_owner = ?new_owner,
            to_owner_type = ?body.owner_type,
            caller_identity = ?caller.identity_id,
            "Agent owner reassigned"
        );
    }

    info!(agent_id = %id, "Agent updated via API");
    Ok(())
}

/// `DELETE /api/agents/{id}` — Delete an agent.
async fn delete_agent(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;

    // Verify agent exists
    let _service = resolve_agent(&state, &id)?;

    // Cannot delete the default agent
    if id == state.registry.default_id() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "cannot delete the default agent"})),
        ));
    }

    // Cannot delete if only one agent
    if state.registry.len() <= 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "cannot delete the last remaining agent"})),
        ));
    }

    // Drop any overlay entry and tombstone the id in agents_remove — a
    // base-defined agent stays deleted across infra deploys. Uses the
    // hub-level config path: the target service's own config_path is None
    // for live-spawned and startup "additional" agents.
    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(&state)?;
    if let Err(e) = crate::config_writer::overlay_remove_agent(
        &config_path,
        &data_dir,
        &id,
        caller.identity_id.as_deref(),
        "web",
        state.def_store.as_deref(),
        &lock_manager,
    )
    .await
    {
        warn!(agent_id = %id, error = %e, "Failed to remove agent from config overlay");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to remove agent: {e}")})),
        ));
    }

    // Wake the watcher and wait (bounded) for the teardown so the agent list
    // the UI refetches no longer contains the deleted agent.
    let gone = match &state.overlay_kick {
        Some(kick) => {
            kick.notify_one();
            wait_until(std::time::Duration::from_secs(10), || {
                state.registry.get(&id).is_none()
            })
            .await
        }
        None => false,
    };

    // A new agent given this id later starts with no picture, and with
    // nothing anyone always allowed this one.
    if let Some(store) = state.pictures.as_ref() {
        if let Err(e) = store.forget(&crate::pictures::Owner::agent(&id)).await {
            warn!(agent_id = %id, error = %format!("{e:#}"), "Failed to remove the agent's pictures");
        }
    }
    #[cfg(feature = "postgres")]
    if let Some(store) = crate::surface_allows::store() {
        if let Err(e) = store.forget_agent(&id).await {
            warn!(agent_id = %id, error = %format!("{e:#}"), "Failed to remove the agent's always-allows");
        }
    }

    info!(agent_id = %id, gone, "Agent removed via API (overlay)");

    Ok(Json(serde_json::json!({
        "deleted": id,
        "live": gone,
        "restart_required": false,
        "message": if gone {
            "Agent removed."
        } else {
            "Agent removed from the config overlay — the hub unloads it live within a few seconds."
        },
    })))
}

#[derive(Deserialize)]
struct ToolsQuery {
    agent_id: Option<String>,
    prefix: Option<String>,
    /// `scope=global` returns the deduped union of tools across every
    /// loaded agent instead of a single agent's registry.
    scope: Option<String>,
    /// Bearer token, accepted via query as well as the `Authorization` header.
    token: Option<String>,
}

/// Collapse tool definitions to one entry per name, picked deterministically
/// (a name can be registered on several agents with agent-specific
/// descriptions, so we keep the lexicographically-smallest description to
/// keep the catalogue stable across requests).
fn dedup_tools_by_name(
    mut defs: Vec<workforce_agent::tools::ToolDefinition>,
) -> Vec<workforce_agent::tools::ToolDefinition> {
    defs.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.description.cmp(&b.description))
    });
    defs.dedup_by(|a, b| a.name == b.name);
    defs
}

/// Where a tool comes from, for the tools-listing UI. `kind` is `builtin` or
/// `mcp`; `label` is the friendly integration/group name; `server` is the MCP
/// server id (or `null` for built-ins).
fn tool_provenance(name: &str) -> serde_json::Value {
    // MCP tools are registered as `mcp.<server_id>.<tool>` (see
    // `mcp_servers::tool::McpTool::new`). Anything else is a built-in.
    if let Some(rest) = name.strip_prefix("mcp.") {
        if let Some((server, _tool)) = rest.split_once('.') {
            return serde_json::json!({
                "kind": "mcp",
                "label": mcp_server_label(server),
                "server": server,
            });
        }
    }
    serde_json::json!({
        "kind": "builtin",
        "label": builtin_label(name),
        "server": serde_json::Value::Null,
    })
}

/// Friendly display name for an MCP server id. The registry only carries the
/// server id on each tool, so we derive a Title-Cased label from it (e.g.
/// `hubspot` → "Hubspot", `google_drive` → "Google Drive").
fn mcp_server_label(server_id: &str) -> String {
    title_case_segments(server_id)
}

/// Friendly group label for a built-in tool, derived from its top-level
/// prefix: `vault.*` → "Vault", `metabase.*` → "Metabase", `filesystem.*` →
/// "Filesystem". Tools without a `.`-prefix use the whole name.
fn builtin_label(name: &str) -> String {
    let prefix = name.split(['.', '_']).next().unwrap_or(name);
    title_case_segments(prefix)
}

/// Title-case an id whose words are separated by `_`, `-`, or `.`.
fn title_case_segments(id: &str) -> String {
    let words: Vec<String> = id
        .split(['_', '-', '.'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        id.to_string()
    } else {
        words.join(" ")
    }
}

/// `GET /api/tools` — list the tools available to an agent (the `agent_id`
/// agent, or the default when omitted), or the deduped union across every
/// loaded agent with `?scope=global`. Admin-only: the catalogue reveals which
/// skills / MCP servers an agent has wired.
async fn list_tools(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ToolsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;

    let mut defs = if query.scope.as_deref() == Some("global") {
        dedup_tools_by_name(
            state
                .registry
                .all_services()
                .iter()
                .flat_map(|service| service.tools.definitions())
                .collect(),
        )
    } else {
        match state.registry.resolve(query.agent_id.as_deref()) {
            Some(s) => s.tools.definitions(),
            None => return Ok(Json(serde_json::json!({ "total": 0, "tools": [] }))),
        }
    };

    defs.sort_by(|a, b| a.name.cmp(&b.name));

    if let Some(ref prefix) = query.prefix {
        let prefix_lower = prefix.to_lowercase();
        defs.retain(|d| d.name.to_lowercase().starts_with(&prefix_lower));
    }

    let tools: Vec<serde_json::Value> = defs
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "description": d.description,
                "provenance": tool_provenance(&d.name),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "total": tools.len(),
        "tools": tools,
    })))
}

#[cfg(test)]
mod tool_union_tests {
    use super::{dedup_tools_by_name, tool_provenance};
    use workforce_agent::tools::ToolDefinition;

    fn td(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({}),
        }
    }

    #[test]
    fn dedups_by_name_to_one_sorted_deterministic_entry() {
        let out = dedup_tools_by_name(vec![
            td("b.tool", "z"),
            td("a.tool", "m"),
            td("b.tool", "a"),
            td("a.tool", "m"),
        ]);
        let pairs: Vec<_> = out
            .iter()
            .map(|d| (d.name.as_str(), d.description.as_str()))
            .collect();
        assert_eq!(pairs, vec![("a.tool", "m"), ("b.tool", "a")]);
    }

    #[test]
    fn dedups_empty_input() {
        assert!(dedup_tools_by_name(Vec::new()).is_empty());
    }

    #[test]
    fn provenance_for_builtin_derives_label_from_prefix() {
        let p = tool_provenance("vault.delete");
        assert_eq!(p["kind"], "builtin");
        assert_eq!(p["label"], "Vault");
        assert!(p["server"].is_null());

        let p = tool_provenance("metabase.card_query");
        assert_eq!(p["kind"], "builtin");
        assert_eq!(p["label"], "Metabase");

        let p = tool_provenance("filesystem.read");
        assert_eq!(p["label"], "Filesystem");
    }

    #[test]
    fn provenance_for_builtin_handles_underscore_prefix() {
        // `tool_retrieve`-style names split on the first `_` too.
        let p = tool_provenance("tool_retrieve");
        assert_eq!(p["kind"], "builtin");
        assert_eq!(p["label"], "Tool");
    }

    #[test]
    fn provenance_for_mcp_extracts_server_and_label() {
        let p = tool_provenance("mcp.hubspot.create_contact");
        assert_eq!(p["kind"], "mcp");
        assert_eq!(p["server"], "hubspot");
        assert_eq!(p["label"], "Hubspot");
    }

    #[test]
    fn provenance_for_mcp_title_cases_multiword_server() {
        let p = tool_provenance("mcp.google_drive.search");
        assert_eq!(p["kind"], "mcp");
        assert_eq!(p["server"], "google_drive");
        assert_eq!(p["label"], "Google Drive");
    }

    #[test]
    fn provenance_bare_mcp_prefix_without_server_is_builtin() {
        // `mcp.list_remote_tools` is a built-in probe, not a server tool —
        // it has no `mcp.<server>.<tool>` shape, so it stays builtin.
        let p = tool_provenance("mcp.list_remote_tools");
        assert_eq!(p["kind"], "builtin");
    }
}

#[derive(Deserialize)]
struct SessionListQuery {
    agent_id: Option<String>,
    channel: Option<String>,
    /// Bearer token, accepted via query param so REST clients that can't set
    /// headers (rare) can still authenticate. Same as `Authorization: Bearer`.
    token: Option<String>,
    /// Legacy admin flag: include non-conversational channels in the
    /// listing. Today admin callers see every channel by default — this
    /// flag is retained for backwards compatibility with older callers
    /// but is effectively a no-op when set to its old default `false`,
    /// because the admin path already includes everything.
    #[serde(default)]
    include_all_channels: bool,
    /// Admin-only escape hatch to hide non-conversational sessions
    /// (workflow:*, scheduler:*, slack-routing). Defaults to `false` —
    /// admins see every session unless they ask not to.
    #[serde(default)]
    conversational_only: bool,
}

/// `GET /api/sessions` — list sessions visible to the authenticated caller.
///
/// Visibility rules:
///  * Admin tokens see every session across every registered agent.
///    They see every channel by default; pass `?conversational_only=true`
///    to hide workflow / scheduler / slack-routing sessions.
///  * Identity / device callers see only sessions whose `owner` matches
///    their `identity_id` or `device_id`, and only on conversational
///    channels (`ios`, `web`, `terminal`). This is what stops the iOS
///    session list from showing other users' Slack DMs or bot-owned
///    sessions.
///  * SubAgent sessions are always hidden (internal orchestration).
///
/// Each session carries hydrated `owner_display_name`, `owner_kind`,
/// `agent_display_name`, and a derived `session_category` ("user" or
/// "system") so the UI can filter user vs system sessions without
/// re-implementing the classification rules.
///
/// Result is sorted by `updated_at` descending and capped at 1000 for
/// admins (cross-agent), 50 for per-caller listings.
async fn list_sessions(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use crate::api_handlers::identity_display::{
        classify_session, resolve_agents, resolve_principals, PrincipalKind,
    };

    let caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_SESSIONS,
    )
    .await?;

    // Collect from one agent or every agent depending on whether the
    // caller scoped the request. `all_services` is bounded by the
    // configured agent count (single digits in practice).
    let services: Vec<Arc<crate::serve::AgentService>> = match query.agent_id.as_deref() {
        Some(id) => state.registry.get(id).into_iter().collect(),
        None if caller.is_admin => state.registry.all_services(),
        None => state.registry.default_agent().into_iter().collect(),
    };
    if services.is_empty() {
        return Ok(Json(serde_json::json!({ "sessions": [] })));
    }

    // Higher cap on the all-agent admin view so the UI's "group by user / by
    // agent" modes see full groups rather than a truncated slice. The per-agent
    // `list` limit must match the cap, otherwise a busy agent's newest sessions
    // could be dropped before the global sort ever sees them.
    let cap = if caller.is_admin && query.agent_id.is_none() {
        1000
    } else {
        50
    };

    let raw: Vec<workforce_common::session::Session> = services
        .into_iter()
        .flat_map(|svc| {
            svc.sessions
                .list(query.channel.as_deref(), cap)
                .unwrap_or_default()
        })
        .collect();

    // Admin default: see everything. The legacy `include_all_channels`
    // flag is honoured for backwards-compat but no longer needed.
    let hide_non_conversational = if caller.is_admin {
        query.conversational_only && !query.include_all_channels
    } else {
        true
    };
    let mut filtered: Vec<workforce_common::session::Session> = raw
        .into_iter()
        .filter(|s| {
            s.kind != workforce_common::session::SessionKind::SubAgent
                && s.owner != "subagent"
                && (!hide_non_conversational || is_conversational_channel(&s.channel))
                && caller.can_see_owner(&s.owner)
        })
        .collect();
    filtered.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    filtered.truncate(cap);

    let principals = resolve_principals(
        state.identity_store.as_ref(),
        filtered.iter().map(|s| s.owner.as_str()),
    );
    let agents = resolve_agents(
        &state.registry,
        filtered.iter().map(|s| s.agent_id.as_str()),
    );

    let sessions: Vec<serde_json::Value> = filtered
        .into_iter()
        .map(|s| {
            let principal = principals
                .get(s.owner.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    use crate::api_handlers::identity_display::PrincipalRef;
                    PrincipalRef {
                        id: s.owner.clone(),
                        display_name: s.owner.clone(),
                        kind: PrincipalKind::User,
                    }
                });
            let agent_display = agents
                .get(s.agent_id.as_str())
                .map(|a| a.display_name.clone())
                .unwrap_or_else(|| s.agent_id.clone());
            let category = classify_session(&s.owner, &s.channel);
            let channel_display = crate::api_handlers::channel_display::parse_channel_display(
                &s.channel,
                s.label.as_deref(),
                &s.metadata,
            );
            let mut entry = serde_json::json!({
                "id": s.id,
                "label": s.label,
                "channel": s.channel,
                "channel_display": channel_display,
                "is_claude_chat": s.is_claude_chat(),
                "owner": s.owner,
                "owner_display_name": principal.display_name,
                "owner_kind": principal.kind,
                "agent_id": s.agent_id,
                "agent_display_name": agent_display,
                "session_category": category,
                "kind": s.kind,
                "messages": s.message_count(),
                "total_tokens": s.total_tokens,
                "created_at": s.created_at.to_rfc3339(),
                "updated_at": s.updated_at.to_rfc3339(),
            });
            if let Some(url) = s.metadata.get("slack_thread_url").and_then(|v| v.as_str()) {
                entry["thread_url"] = serde_json::json!(url);
            }
            if let Some(ch) = s.metadata.get("slack_channel_id").and_then(|v| v.as_str()) {
                entry["slack_channel_id"] = serde_json::json!(ch);
            }
            entry
        })
        .collect();

    Ok(Json(serde_json::json!({ "sessions": sessions })))
}

/// Get model health status (degraded models, failure counts).
async fn model_health(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_MODELS,
    )
    .await?;
    let service = match state.default_service() {
        Some(s) => s,
        None => {
            return Ok(Json(serde_json::json!({
                "enabled": false,
                "models": {},
            })))
        }
    };
    Ok(match service.health_tracker {
        Some(ref tracker) => Json(serde_json::json!({
            "enabled": true,
            "models": tracker.status().await,
        })),
        None => Json(serde_json::json!({
            "enabled": false,
            "models": {},
        })),
    })
}

/// Get configured model aliases.
async fn model_aliases(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_MODELS,
    )
    .await?;
    let service = match state.default_service() {
        Some(s) => s,
        None => {
            return Ok(Json(serde_json::json!({
                "default_model": "",
                "failover_chain": [],
                "aliases": {},
            })))
        }
    };
    Ok(Json(serde_json::json!({
        "default_model": service.effective_llm.default_model,
        "failover_chain": service.effective_llm.failover,
        "aliases": service.effective_llm.aliases,
    })))
}

/// Admin endpoint: reset the active model override for an agent.
///
/// Clears both the in-memory override and the vault entry, restoring
/// the configured default model. Use this to recover from a bad model switch.
async fn admin_reset_model(
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    // Constant-time + empty-token-safe (audit F2): an unset `auth_token` must
    // not authenticate a tokenless caller.
    if !crate::auth::constant_time_token_eq(
        query.token.as_deref().unwrap_or_default(),
        &state.auth_token,
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let service = match state.registry.resolve(query.agent_id.as_deref()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "agent not found"})),
            )
                .into_response();
        }
    };

    let default_model = service.reset_model();

    Json(serde_json::json!({
        "agent": service.agent_id.to_string(),
        "model": default_model,
        "message": "Model reset to default",
    }))
    .into_response()
}

/// Get heartbeat status and recent history. Discloses the schedule and
/// the response text of recent runs — gated on `SCOPE_HEARTBEAT` so it
/// can't be used as a free-attendance oracle. For pure liveness probes
/// (load balancers etc.) see [`livez`].
/// The items of a check-in list: its Markdown list items.
fn checklist_items(list: &str) -> usize {
    list.lines()
        .map(str::trim_start)
        .filter(|l| {
            l.starts_with("- ")
                || l.starts_with("* ")
                || l.split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
        .count()
}

#[cfg(test)]
mod checklist_tests {
    #[test]
    fn a_check_in_list_counts_its_list_items_only() {
        let list = "# Check-in list\n\nLook over:\n- the build\n  * nested ones count too\n1. Acme's inbox\n12. the calendar\nnot an item\n2025. was a year\n";
        assert_eq!(super::checklist_items(list), 5);
        assert_eq!(super::checklist_items(""), 0);
    }
}

/// `GET /api/agents/{id}/heartbeat`: one agent's own check-ins, as its own
/// `[heartbeat]` over the install's says: on or off, how often, its quiet
/// hours, how many things are on its check-in list (HEARTBEAT.md), when it
/// last went through it, and whether that needed the person.
async fn agent_heartbeat_status(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (service, _caller) =
        authorise_agent_reader(&state, &headers, query.token.as_deref(), &id).await?;
    let own = workforce_common::settings::AgentWorkspaceConfig::load_with_text(&service.workspace)
        .ok()
        .flatten()
        .and_then(|(config, _)| config.heartbeat);
    let effective = service.config.heartbeat.for_agent(own.as_ref());
    let list = workforce_common::config_files::read(&service.workspace.join("HEARTBEAT.md"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let last_checked = crate::heartbeat::HeartbeatState::load(&service.workspace).last_heartbeat;
    let handle = service.heartbeat_handle.read().clone();
    let last = match handle {
        Some(handle) => handle.last().await,
        None => None,
    };
    Ok(Json(serde_json::json!({
        "agent_id": id,
        "enabled": effective.enabled,
        "interval_secs": effective.interval_secs,
        "quiet_start": effective.quiet_start,
        "quiet_end": effective.quiet_end,
        "items": checklist_items(&list),
        "last_checked": last_checked.map(|t| t.to_rfc3339()),
        "needed_you": last.as_ref().map(|r| r.actionable),
        "last_error": last.and_then(|r| r.error),
    })))
}

async fn heartbeat_status(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_HEARTBEAT,
    )
    .await?;
    let service = match state.default_service() {
        Some(s) => s,
        None => {
            return Ok(Json(serde_json::json!({
                "enabled": false,
                "message": "No agent available",
            })))
        }
    };
    let config = &service.config.heartbeat;
    let handle = service.heartbeat_handle.read().clone();

    let Some(handle) = handle else {
        return Ok(Json(serde_json::json!({
            "enabled": false,
            "message": "Heartbeat not configured",
        })));
    };

    let history = handle.history().await;
    let last = handle.last().await;
    let render = |r: &crate::hub_core::heartbeats::HeartbeatRecord| {
        serde_json::json!({
            "timestamp": r.timestamp.to_rfc3339(),
            "actionable": r.actionable,
            "delivered": r.delivered,
            "response": r.response,
        })
    };
    Ok(Json(serde_json::json!({
        "enabled": config.enabled,
        "interval_secs": config.interval_secs,
        "active_start": config.active_start,
        "active_end": config.active_end,
        "channels": config.channels,
        "deliver": format!("{:?}", config.deliver),
        "last": last.as_ref().map(render),
        "history_count": history.len(),
        "history": history.iter().rev().take(10).map(render).collect::<Vec<_>>(),
    })))
}

/// Trigger an immediate heartbeat wake.
async fn heartbeat_wake(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_HEARTBEAT,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let handle = service.heartbeat_handle.read().clone();

    let Some(handle) = handle else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !handle.wake().await {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "Heartbeat wake triggered",
    })))
}

/// Public, unauthenticated liveness probe. Returns `{"ok": true}` and
/// nothing else — no schedule, no history, no agent identity, nothing an
/// attacker can use to time activity or fingerprint the deployment. Wire
/// load balancers and k8s `livenessProbe` at this endpoint, not at the
/// scope-gated [`heartbeat_status`].
async fn livez() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Deployed build provenance: crate version, full git SHA, image build date.
/// Auth-gated (unlike [`livez`]) so anonymous callers can't fingerprint the
/// deployment; any logged-in UI user can read it — it backs the sidebar
/// version footer.
async fn version_info(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_CHAT,
    )
    .await?;
    Ok(Json(crate::build_info::as_json()))
}

// ── Heartbeat Check CRUD & History Endpoints (DW-422) ─────────────────────

/// `GET /api/heartbeat/checks` — List all heartbeat checks.
async fn list_heartbeat_checks(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_HEARTBEAT,
    )
    .await?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    match workforce_common::heartbeat_check::list_heartbeat_checks(&service.workspace) {
        Ok(checks) => {
            let checks_json: Vec<serde_json::Value> = checks
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "name": c.meta.name,
                        "enabled": c.meta.enabled,
                        "schedule": c.meta.schedule,
                        "priority": c.meta.priority,
                        "tags": c.meta.tags,
                    })
                })
                .collect();
            Ok(Json(serde_json::json!({ "checks": checks_json })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to list heartbeat checks: {e}")})),
        )),
    }
}

/// Request body for creating a heartbeat check.
#[derive(Debug, Deserialize)]
struct CreateHeartbeatCheckRequest {
    id: String,
    name: String,
    body: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default = "default_heartbeat_priority")]
    priority: u32,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    cooldown_secs: Option<u64>,
}

fn default_heartbeat_priority() -> u32 {
    5
}

/// `POST /api/heartbeat/checks` — Create a new heartbeat check.
async fn create_heartbeat_check(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateHeartbeatCheckRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::HeartbeatManage,
    )
    .await?;
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    // Validate ID format
    if body.id.is_empty()
        || !body
            .id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "invalid check ID — use alphanumeric, hyphens, underscores only"}),
            ),
        ));
    }

    // Check uniqueness
    if workforce_common::heartbeat_check::load_heartbeat_check(&service.workspace, &body.id).is_ok()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                serde_json::json!({"error": format!("heartbeat check '{}' already exists", body.id)}),
            ),
        ));
    }

    let mut check = workforce_common::heartbeat_check::HeartbeatCheck {
        id: body.id.clone(),
        meta: workforce_common::heartbeat_check::HeartbeatCheckMeta {
            name: body.name,
            enabled: true,
            schedule: body.schedule,
            priority: body.priority,
            tags: body.tags,
            timezone: body.timezone,
            cooldown_secs: body.cooldown_secs,
            take_action: false,
            tools: None,
            creator: None,
            creator_sig: None,
            deliver_to: None,
        },
        body: body.body,
    };
    crate::heartbeat_signature::stamp_creator(
        service.check_signer.as_deref(),
        &mut check,
        caller.identity_id.as_deref(),
    );

    match workforce_common::heartbeat_check::save_heartbeat_check(&service.workspace, &check) {
        Ok(()) => {
            info!(check_id = %check.id, "Heartbeat check created via API");
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "created": true,
                    "id": check.id,
                    "name": check.meta.name,
                })),
            ))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to create heartbeat check: {e}")})),
        )),
    }
}

/// `GET /api/heartbeat/checks/{id}` — Get full detail of a heartbeat check.
async fn get_heartbeat_check(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_HEARTBEAT,
    )
    .await?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    match workforce_common::heartbeat_check::load_heartbeat_check(&service.workspace, &id) {
        Ok(check) => Ok(Json(serde_json::json!({
            "id": check.id,
            "name": check.meta.name,
            "enabled": check.meta.enabled,
            "schedule": check.meta.schedule,
            "priority": check.meta.priority,
            "tags": check.meta.tags,
            "timezone": check.meta.timezone,
            "cooldown_secs": check.meta.cooldown_secs,
            "body": check.body,
        }))),
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("heartbeat check '{}' not found", id)})),
        )),
    }
}

/// Request body for updating a heartbeat check (partial update).
#[derive(Debug, Deserialize)]
struct UpdateHeartbeatCheckRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// `PUT /api/heartbeat/checks/{id}` — Update an existing heartbeat check.
async fn update_heartbeat_check(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateHeartbeatCheckRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::HeartbeatManage,
    )
    .await?;
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    // Load existing check
    let mut check = workforce_common::heartbeat_check::load_heartbeat_check(
        &service.workspace,
        &id,
    )
    .map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("heartbeat check '{}' not found", id)})),
        )
    })?;

    // Merge only provided fields
    if let Some(name) = body.name {
        check.meta.name = name;
    }
    if let Some(body_text) = body.body {
        check.body = body_text;
    }
    if let Some(schedule) = body.schedule {
        check.meta.schedule = Some(schedule);
    }
    if let Some(priority) = body.priority {
        check.meta.priority = priority;
    }
    if let Some(enabled) = body.enabled {
        check.meta.enabled = enabled;
    }
    if let Some(tags) = body.tags {
        check.meta.tags = tags;
    }
    crate::heartbeat_signature::stamp_creator(
        service.check_signer.as_deref(),
        &mut check,
        caller.identity_id.as_deref(),
    );

    match workforce_common::heartbeat_check::save_heartbeat_check(&service.workspace, &check) {
        Ok(()) => {
            info!(check_id = %id, "Heartbeat check updated via API");
            Ok(Json(serde_json::json!({
                "updated": true,
                "id": check.id,
                "name": check.meta.name,
            })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to save heartbeat check: {e}")})),
        )),
    }
}

/// `DELETE /api/heartbeat/checks/{id}` — Delete a heartbeat check.
async fn delete_heartbeat_check(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::HeartbeatManage,
    )
    .await?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    match workforce_common::heartbeat_check::delete_heartbeat_check(&service.workspace, &id) {
        Ok(()) => {
            info!(check_id = %id, "Heartbeat check deleted via API");
            Ok(Json(serde_json::json!({
                "deleted": true,
                "id": id,
            })))
        }
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("heartbeat check '{}' not found", id)})),
        )),
    }
}

/// `POST /api/heartbeat/checks/{id}/trigger` — Trigger a specific heartbeat check.
async fn trigger_heartbeat_check(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::HeartbeatManage,
    )
    .await?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    // Verify the check exists first
    if workforce_common::heartbeat_check::load_heartbeat_check(&service.workspace, &id).is_err() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("heartbeat check '{}' not found", id)})),
        ));
    }

    let handle = {
        let guard = service.heartbeat_handle.read();
        guard.clone()
    };

    match handle {
        Some(handle) => {
            if handle.wake_check(id.clone()).await {
                info!(check_id = %id, "Heartbeat check triggered via API");
                Ok(Json(serde_json::json!({
                    "triggered": true,
                    "check_id": id,
                })))
            } else {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        serde_json::json!({"error": "failed to send trigger command — heartbeat runner may have stopped"}),
                    ),
                ))
            }
        }
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "heartbeat runner is not active"})),
        )),
    }
}

/// `GET /api/heartbeat/history` — Get heartbeat run history.
async fn get_heartbeat_history(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_HEARTBEAT,
    )
    .await?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no agent available"})),
        )
    })?;

    let handle = {
        let guard = service.heartbeat_handle.read();
        guard.clone()
    };

    match handle {
        Some(handle) => {
            let history = handle.history().await;
            let records: Vec<serde_json::Value> = history
                .iter()
                .rev()
                .map(|r| {
                    serde_json::json!({
                        "check_id": r.check_id,
                        "check_name": r.check_name,
                        "timestamp": r.timestamp.to_rfc3339(),
                        "actionable": r.actionable,
                        "delivered": r.delivered,
                        "response": r.response,
                        "duration_ms": r.duration_ms,
                        "error": r.error,
                    })
                })
                .collect();
            Ok(Json(serde_json::json!({ "history": records })))
        }
        None => {
            // No heartbeat runner active, return empty history
            Ok(Json(serde_json::json!({ "history": [] })))
        }
    }
}

// ── Pairing endpoints ─────────────────────────────────────────────────────

/// Axum middleware that gates a sub-router behind the `admin` scope. Used
/// to wrap the admin router when it is nested into the gateway at
/// `/api/admin`. Admins (static gateway token, SystemAdmin JWT) bypass the
/// scope predicate via `CallerContext::has_scope`'s admin shortcut.
pub async fn require_admin_auth(
    State(state): State<Arc<GatewayState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    Ok(next.run(admit_admin(&state, request).await?).await)
}

/// `request` from an admin, carrying the caller as an extension so a handler
/// can act as the identity behind it, and the gateway so it can apply the
/// gateway's rules to that caller.
async fn admit_admin(
    state: &Arc<GatewayState>,
    mut request: axum::extract::Request,
) -> Result<axum::extract::Request, StatusCode> {
    let caller = require_scope(state, request.headers(), None, crate::jwt::SCOPE_ADMIN).await?;
    request.extensions_mut().insert(caller);
    request.extensions_mut().insert(Arc::clone(state));
    Ok(request)
}

/// Standalone admin-port auth (audit F3, 2026-06-16). The admin router binds
/// `0.0.0.0:8090` and was previously mounted with CORS but **no auth**,
/// exposing cross-tenant session reads (`/api/sessions/history`) and
/// destructive redaction (`/api/sessions/redact`) to anything that could route
/// to the host (it sits on a flat shared VPC). This gates the whole admin
/// surface EXCEPT the unauthenticated operational endpoints infra
/// scrapes/probes: `/api/metrics` (the Datadog OpenMetrics scrape),
/// `/api/health`, and `/api/ready`. Paths are matched at the served prefix
/// (`/api/...` on the standalone port).
pub async fn require_admin_auth_except_operational(
    State(state): State<Arc<GatewayState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    if matches!(
        request.uri().path(),
        "/api/metrics" | "/api/health" | "/api/ready"
    ) {
        return Ok(next.run(request).await);
    }
    Ok(next.run(admit_admin(&state, request).await?).await)
}

/// Reason a scope-gated route rejected a caller. Threaded through `?` in
/// handlers via `From` impls — the same helper works for both
/// `Result<_, StatusCode>` and `Result<_, (StatusCode, Json<Value>)>` styles.
pub(crate) enum ScopeRejection {
    Unauthenticated,
    MissingScope,
}

impl From<ScopeRejection> for StatusCode {
    fn from(r: ScopeRejection) -> Self {
        match r {
            ScopeRejection::Unauthenticated => StatusCode::UNAUTHORIZED,
            ScopeRejection::MissingScope => StatusCode::FORBIDDEN,
        }
    }
}

impl From<ScopeRejection> for (StatusCode, Json<serde_json::Value>) {
    fn from(r: ScopeRejection) -> Self {
        match r {
            ScopeRejection::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "unauthorized"})),
            ),
            ScopeRejection::MissingScope => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "forbidden"})),
            ),
        }
    }
}

impl IntoResponse for ScopeRejection {
    fn into_response(self) -> axum::response::Response {
        let body: (StatusCode, Json<serde_json::Value>) = self.into();
        body.into_response()
    }
}

/// Resolve the caller, require authentication, and require `scope`. The
/// returned `CallerContext` carries identity/device/scope info for any
/// downstream ownership checks. Admin callers (static gateway token,
/// SystemAdmin JWT) bypass the scope gate via `CallerContext::has_scope`.
pub(crate) async fn require_scope(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    scope: &str,
) -> Result<CallerContext, ScopeRejection> {
    let caller = resolve_caller_context(state, headers, query_token).await;
    if !caller.is_authenticated() {
        return Err(ScopeRejection::Unauthenticated);
    }
    if !caller.has_scope(scope) {
        return Err(ScopeRejection::MissingScope);
    }
    Ok(caller)
}

/// Resolve a gateway [`CallerContext`] to the engine's [`Principal`] — the one
/// place the REST surface turns "how the caller authenticated" into "what role
/// + live status the capability matrix scores".
///
/// * Static gateway token / owner master auth (`is_admin` with **no**
///   `identity_id`) → trusted `SystemAdmin`/`Active`: the master token bypasses
///   the role/status machinery by design.
/// * A paired-device token (`device_id` set) → always a non-admin Member,
///   regardless of the role of the identity it is linked to. This preserves the
///   gateway's standing invariant that paired devices never inherit admin
///   authority (see [`CallerContext::is_admin`]) and matches the pre-migration
///   behaviour, where these routes rejected device tokens (`check_owner_auth`
///   does not resolve devices; `require_scope` gated on the device's own
///   scopes). Admin actions go through a JWT or the static token, never a device.
/// * A JWT caller carrying an `identity_id` → a **live** `get_by_id` lookup so
///   role *and* status are current — a suspended admin scores as a non-admin
///   Member even with a structurally-valid JWT, closing the suspended-admin-on-
///   REST residual. `CallerContext::is_admin` is *not* trusted here: it is
///   `role.can_manage_system()` with no status check, so honouring it would
///   reopen exactly that hole.
/// * No store, an unresolvable id, or no principal at all → non-admin Member.
pub(crate) fn gateway_principal(
    caller: &CallerContext,
    identity_store: Option<&Arc<dyn workforce_identity::store::IdentityStore>>,
) -> crate::authz::Principal {
    use workforce_identity::types::{IdentityRole, IdentityStatus};

    let member = |identity_id| crate::authz::Principal {
        identity_id,
        role: IdentityRole::Member,
        role_key: "member".to_string(),
        status: IdentityStatus::Active,
    };

    // A paired device never carries admin authority over the REST surface, even
    // when linked to an admin identity — resolve it as a Member up front.
    if caller.device_id.is_some() {
        return member(
            caller
                .identity_id
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        );
    }

    match caller.identity_id.as_deref() {
        Some(id) => {
            let resolved = identity_store
                .zip(uuid::Uuid::parse_str(id).ok())
                .and_then(|(store, uid)| store.get_by_id(uid).ok().flatten());
            match resolved {
                Some(identity) => crate::authz::Principal {
                    identity_id: Some(identity.id),
                    role: identity.role,
                    role_key: identity.effective_role_key().to_string(),
                    status: identity.status,
                },
                None => member(uuid::Uuid::parse_str(id).ok()),
            }
        }
        None if caller.is_admin => crate::authz::Principal {
            identity_id: None,
            role: IdentityRole::SystemAdmin,
            role_key: "system_admin".to_string(),
            status: IdentityStatus::Active,
        },
        None => member(None),
    }
}

/// Resolve the caller and enforce a capability for an admin **read** route —
/// no rate limit (admin-UI reads poll). 403 on a hard `Deny`, 401 when no
/// principal is present at all. The returned [`CallerContext`] carries the
/// identity/device info downstream filtering needs. This is the single
/// capability-engine entry point that retires `require_scope(SCOPE_ADMIN)` and
/// the read half of `check_owner_auth` on migrated routes.
pub(crate) async fn require_capability_read(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    cap: crate::authz::Capability,
) -> Result<CallerContext, (StatusCode, Json<serde_json::Value>)> {
    let caller = resolve_caller_context(state, headers, query_token).await;
    if !caller.is_authenticated() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        ));
    }
    if !caller_holds(state, &caller, cap) {
        crate::permissions_audit::emit_capability_deny(
            caller.identity_id.as_deref(),
            cap.key(),
            cap.key(),
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "forbidden"})),
        ));
    }
    Ok(caller)
}

/// Whether the capability engine lets `caller` exercise `cap`: anything but
/// a hard `Deny`, an `Ask` leaving the confirm to the route's own flow.
pub(crate) fn caller_holds(
    state: &GatewayState,
    caller: &CallerContext,
    cap: crate::authz::Capability,
) -> bool {
    let principal = gateway_principal(caller, state.identity_store.as_ref());
    crate::authz::enforce(&principal, cap, &crate::authz::capability_matrix()).is_ok()
}

/// Enforce a capability for an admin **write** route: [`require_capability_read`]
/// plus the per-identity action rate limit (parity with `require_admin_action`,
/// which it replaces on migrated write routes). Returns the resolved caller for
/// audit attribution and write-actor selection.
pub(crate) async fn require_capability_action(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    cap: crate::authz::Capability,
) -> Result<CallerContext, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_read(state, headers, query_token, cap).await?;
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(too_many_requests_err)?;
    Ok(caller)
}

/// List all pending pairing requests (channel + device + node).
async fn pair_list_pending(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let mut result = serde_json::json!({
        "channel": [],
        "device": [],
    });

    if let Some(ref pairing) = state.channel_pairing {
        if let Ok(reqs) = pairing.list_pending_async().await {
            result["channel"] = serde_json::to_value(reqs).unwrap_or_default();
        }
    }
    if let Some(ref pairing) = state.device_pairing {
        if let Ok(reqs) = pairing.list_pending_async().await {
            result["device"] = serde_json::to_value(reqs).unwrap_or_default();
        }
    }
    Ok(Json(result))
}

/// Approve a pairing code (works for channel, device, or node).
async fn pair_approve(
    Path(code): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    // Try channel pairing first
    if let Some(ref pairing) = state.channel_pairing {
        if let Ok(req) = pairing
            .approve_code_async(code.clone(), "admin".to_string())
            .await
        {
            return Ok(Json(serde_json::json!({
                "type": "channel",
                "approved": true,
                "platform": req.platform,
                "sender_id": req.sender_id,
                "sender_name": req.sender_name,
            })));
        }
    }

    // Try device pairing. This older route links by the device's own
    // `identity_hint`; the person-confirmed paths are approve-self and the
    // admin assign below.
    if let Some(ref pairing) = state.device_pairing {
        if let Ok(approval) = crate::device_link::approve_and_link(
            pairing,
            state.identity_store.as_deref(),
            &code,
            "admin",
            crate::device_link::LinkChoice::FromHint,
        )
        .await
        {
            crate::device_link::audit_device_pair(
                "admin_hint",
                _caller.identity_id.as_deref(),
                &approval,
            );
            let device = &approval.device;
            let mut response = serde_json::json!({
                "type": "device",
                "approved": true,
                "device_name": device.device_name,
                "device_id": device.id,
                "token": approval.token,
                "scopes": device.scopes,
            });
            if let Some((identity_id, kind)) = approval.link {
                response["linked_identity_id"] = serde_json::json!(identity_id);
                response["linked_via"] = serde_json::json!(kind);
            }
            return Ok(Json(response));
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Reject a pairing code.
async fn pair_reject(
    Path(code): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    if let Some(ref pairing) = state.channel_pairing {
        if pairing.reject_code_async(code.clone()).await.is_ok() {
            return Ok(Json(serde_json::json!({ "rejected": true })));
        }
    }
    if let Some(ref pairing) = state.device_pairing {
        if pairing.reject_async(code.clone()).await.is_ok() {
            return Ok(Json(serde_json::json!({ "rejected": true })));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

/// List channel allowlist.
async fn pair_list_allowlist(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    if let Some(ref pairing) = state.channel_pairing {
        if let Ok(entries) = pairing.list_allowlist_async(None).await {
            return Ok(Json(serde_json::json!({ "allowlist": entries })));
        }
    }
    Ok(Json(serde_json::json!({ "allowlist": [] })))
}

#[derive(Deserialize)]
struct InitiateDeviceRequest {
    device_name: String,
    public_key: Option<String>,
    client_id: Option<String>,
    /// Caller-supplied identity hint — email or identity UUID. The gateway
    /// uses this on approval to auto-link the new device to that identity
    /// (mirroring the Slack / Keycloak auto-match flows). When set the
    /// device authenticates as that user from day one and no separate JWT
    /// login is required.
    #[serde(default, alias = "email", alias = "identity_id")]
    identity_hint: Option<String>,
    /// What kind of device this is — becomes the `Identity.linked_ids` key.
    /// Omitted means `ios_device`, so existing clients are unaffected.
    #[serde(default, alias = "kind")]
    device_kind: Option<String>,
}

pub(crate) use crate::device_link::identity_for_device;

/// Validate a caller-supplied device kind. `None` stays `None` (meaning
/// `ios_device`); a malformed kind is a 400 rather than a silent default,
/// because linking under a namespace nothing looks up would present as the
/// device pairing successfully and then never resolving to its owner.
fn resolve_device_kind(raw: Option<&str>) -> Result<Option<String>, StatusCode> {
    match raw {
        None => Ok(None),
        Some(raw) => workforce_pairing::types::normalise_device_kind(raw)
            .map(Some)
            .ok_or(StatusCode::BAD_REQUEST),
    }
}

/// Normalise an `identity_hint` claimed at pairing-initiation time. Trims
/// whitespace, lowercases email addresses (RFC 5321 local-part is technically
/// case-sensitive but practically we treat email case-insensitively, matching
/// the rest of the codebase), and rejects empty strings.
fn normalise_identity_hint(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains('@') {
                s.to_ascii_lowercase()
            } else {
                s
            }
        })
}

/// Unauthenticated device pairing initiation (self-service flow).
/// Devices call this to create a pairing request without needing an admin token.
async fn pair_device_request_unauthenticated(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<InitiateDeviceRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if check_rate_limit(
        state.pair_rate_limiter.as_ref(),
        &client_ip(&state.trusted_proxies, &headers, Some(peer)),
    )
    .is_err()
    {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let identity_hint = normalise_identity_hint(req.identity_hint);
    let device_kind = match resolve_device_kind(req.device_kind.as_deref()) {
        Ok(kind) => kind,
        Err(status) => return Err(status),
    };
    match pairing
        .initiate_async(
            req.device_name.clone(),
            crate::jwt::device_scopes(),
            req.public_key,
            req.client_id,
            identity_hint,
            device_kind,
        )
        .await
    {
        Ok(pairing_req) => Ok(Json(serde_json::json!({
            "code": pairing_req.code,
            "device_name": pairing_req.device_name,
            "approve_url": pair_approve_url(&state, &headers, &pairing_req.code),
            "expires_at": pairing_req.expires_at.to_rfc3339(),
        }))),
        Err(e) => {
            tracing::warn!("Device pairing initiation failed: {e}");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Where the device's person approves `code`: the hub's `/pair` page.
///
/// The base is `[hub].public_url`; failing that, the origin SSO redirects back
/// to; failing that, the host the device reached the hub at.
fn pair_approve_url(state: &GatewayState, headers: &axum::http::HeaderMap, code: &str) -> String {
    let configured = state
        .default_service()
        .and_then(|service| service.config.hub.public_url.clone())
        .filter(|url| !url.trim().is_empty());
    #[cfg(feature = "oidc")]
    let configured = configured.or_else(|| {
        state
            .oidc_provider
            .as_ref()
            .map(|oidc| oidc.post_logout_redirect_uri())
    });
    let base = configured.unwrap_or_else(|| {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|h| h.to_str().ok())
            .filter(|p| *p == "https" || *p == "http")
            .unwrap_or("http");
        format!("{scheme}://{host}")
    });
    format!("{}/pair?code={code}", base.trim_end_matches('/'))
}

/// The account an approved request's device was connected to, as the device
/// shows it ("connected as …"): its name and email, or `null` when the device
/// is anonymous.
async fn approved_identity(
    state: &GatewayState,
    pairing: &DevicePairingService,
    req: &workforce_pairing::types::DevicePairingRequest,
) -> serde_json::Value {
    let Some(store) = state.identity_store.as_ref() else {
        return serde_json::Value::Null;
    };
    let Ok(Some(device)) = pairing.device_for_request_async(req).await else {
        return serde_json::Value::Null;
    };
    identity_for_device(store.as_ref(), &device.id)
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        .and_then(|id| store.get_by_id(id).ok().flatten())
        .map_or(serde_json::Value::Null, |identity| {
            serde_json::json!({
                "display_name": identity.display_name,
                "email": identity.email,
            })
        })
}

/// Poll device pairing status by code (unauthenticated).
/// Returns the current status. On approval, returns the token once (one-time retrieval).
async fn pair_device_status(
    Path(code): Path<String>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if check_rate_limit(
        state.pair_rate_limiter.as_ref(),
        &client_ip(&state.trusted_proxies, &headers, Some(peer)),
    )
    .is_err()
    {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;

    let req = pairing
        .check_status_async(code)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // F9 (2026-05-15): expired codes return 404 — the SAME shape as a
    // never-existed code — so external probes cannot distinguish "code
    // was issued but timed out" from "code never existed". Clients that
    // were polling a pending code see 404 and treat it identically to
    // any other terminal state.
    if (req.status == workforce_pairing::types::PairingStatus::Pending
        && req.expires_at < chrono::Utc::now())
        || req.status == workforce_pairing::types::PairingStatus::Expired
    {
        return Err(StatusCode::NOT_FOUND);
    }

    match req.status {
        workforce_pairing::types::PairingStatus::Pending => {
            Ok(Json(serde_json::json!({ "status": "pending" })))
        }
        workforce_pairing::types::PairingStatus::Approved => {
            if let Some(token) = req.pending_token.clone() {
                // One-time token retrieval — clear it after returning
                let _ = pairing.clear_pending_token_async(req.id.clone()).await;
                Ok(Json(serde_json::json!({
                    "status": "approved",
                    "token": token,
                    "scopes": req.scopes,
                    "identity": approved_identity(&state, pairing, &req).await,
                })))
            } else {
                // Token already retrieved
                Ok(Json(serde_json::json!({
                    "status": "approved",
                    "identity": approved_identity(&state, pairing, &req).await,
                })))
            }
        }
        workforce_pairing::types::PairingStatus::Rejected => {
            Ok(Json(serde_json::json!({ "status": "rejected" })))
        }
        // Handled above — kept exhaustive so a new variant trips the compiler.
        workforce_pairing::types::PairingStatus::Expired => Err(StatusCode::NOT_FOUND),
    }
}

/// Initiate a device pairing request.
async fn pair_initiate_device(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<InitiateDeviceRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let identity_hint = normalise_identity_hint(req.identity_hint);
    let device_kind = match resolve_device_kind(req.device_kind.as_deref()) {
        Ok(kind) => kind,
        Err(status) => return Err(status),
    };
    match pairing
        .initiate_async(
            req.device_name.clone(),
            crate::jwt::device_scopes(),
            req.public_key,
            req.client_id,
            identity_hint,
            device_kind,
        )
        .await
    {
        Ok(pairing_req) => Ok(Json(serde_json::json!({
            "code": pairing_req.code,
            "device_name": pairing_req.device_name,
            "expires_at": pairing_req.expires_at.to_rfc3339(),
        }))),
        Err(e) => {
            warn!("Device pairing initiation failed: {e}");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// List paired devices.
async fn pair_list_devices(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    if let Some(ref pairing) = state.device_pairing {
        if let Ok(devices) = pairing.list_devices_async().await {
            // Each device with the person it belongs to, so a person's devices
            // can be found and revoked.
            let devices: Vec<serde_json::Value> = devices
                .into_iter()
                .map(|device| {
                    let owner = state.identity_store.as_ref().and_then(|store| {
                        crate::device_link::device_owner(store.as_ref(), &device.id)
                    });
                    let mut entry = serde_json::to_value(&device).unwrap_or_default();
                    entry["owner"] = owner.as_ref().map_or(
                        serde_json::Value::Null,
                        crate::device_link::identity_summary,
                    );
                    entry
                })
                .collect();
            return Ok(Json(serde_json::json!({ "devices": devices })));
        }
    }
    Ok(Json(serde_json::json!({ "devices": [] })))
}

/// Revoke a paired device.
async fn pair_revoke_device(
    Path(device_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let owner = state
        .identity_store
        .as_ref()
        .and_then(|store| identity_for_device(store.as_ref(), &device_id))
        .unwrap_or_else(|| device_id.clone());
    match pairing.revoke_async(device_id.clone()).await {
        Ok(()) => {
            retarget_device(&state, &owner, &device_id).await;
            Ok(Json(serde_json::json!({ "revoked": true })))
        }
        Err(e) => {
            warn!("Device revocation failed: {e}");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// A device unpaired itself (Desk's "Disconnect"): only the device whose
/// token the request carries.
async fn pair_device_revoke_self(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (device, _) = pairing
        .validate_token_with_match_async(token.to_string())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let owner = state
        .identity_store
        .as_ref()
        .and_then(|store| identity_for_device(store.as_ref(), &device.id))
        .unwrap_or_else(|| device.id.clone());
    pairing.revoke_async(device.id.clone()).await.map_err(|e| {
        warn!("Device self-revocation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    retarget_device(&state, &owner, &device.id).await;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

/// What `owner` had delivered to Desk rooms on the unpaired `device` goes to
/// their DM instead, with every agent.
async fn retarget_device(state: &GatewayState, owner: &str, device: &str) {
    for service in state.registry.all_services() {
        crate::desk_surface::retarget(&service, owner, device).await;
    }
}

/// Rotate a paired device's token.
async fn pair_rotate_device(
    Path(device_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    match pairing.rotate_token_async(device_id).await {
        Ok(token) => Ok(Json(serde_json::json!({
            "rotated": true,
            "token": token,
        }))),
        Err(e) => {
            warn!("Device token rotation failed: {e}");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Device-authenticated token rotation (self-service).
/// The device sends its current `wfp_*` token via Bearer auth and gets a new token back.
///
/// Refuses to rotate when the presented token matched via the
/// `prev_token_hash` grace slot. Rotation is the ONLY operation that retires
/// a token, so gating it on a `Current` match is what bounds the grace
/// window to one generation: a holder of the previous token can keep
/// authenticating but can never advance the chain and orphan the current
/// token some other holder may have persisted. The client sees
/// `409 CONFLICT`, skips rotation for the session, and continues using the
/// token it holds.
async fn pair_device_rotate_self(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pairing = state.device_pairing.as_ref().ok_or(StatusCode::NOT_FOUND)?;

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let (device, kind) = pairing
        .validate_token_with_match_async(token.to_string())
        .await
        // 503 like every other pairing-store failure: the token's validity
        // is unknown, and the client should retry rather than treat it as
        // a rotation failure.
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !crate::jwt::has_scope(&device.scopes, crate::jwt::SCOPE_PAIR) {
        return Err(StatusCode::FORBIDDEN);
    }

    if kind == workforce_pairing::TokenMatch::Previous {
        warn!(
            device_id = %device.id,
            "Rejecting rotation: client presented previous (recovery-slot) token"
        );
        return Err(StatusCode::CONFLICT);
    }

    match pairing.rotate_token_async(device.id).await {
        Ok(new_token) => Ok(Json(serde_json::json!({
            "rotated": true,
            "token": new_token,
        }))),
        Err(e) => {
            warn!("Device self-rotation failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── Owner-only vault endpoints ────────────────────────────────────────────

/// Strict owner-only auth check.
/// Accepts the static admin token or a valid JWT with SystemAdmin role.
pub(crate) fn check_owner_auth(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> bool {
    if let Some(t) = query_token {
        if crate::auth::constant_time_token_eq(t, &state.auth_token) {
            return true;
        }
    }
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            if crate::auth::constant_time_token_eq(token, &state.auth_token) {
                return true;
            }
            if let Some(ref cfg) = state.jwt_config {
                if let Ok(claims) = crate::jwt::validate_token(cfg, token) {
                    return claims.role.can_manage_system();
                }
            }
        }
    }
    false
}

/// Choose the audit actor for an admin-surface vault write.
///
/// Per-user entries (physical path begins `users/<uid>/`) must be attributed
/// to that owner: the admin UI reconstructs an entry's physical path from its
/// `created_by` tag (`vaultEntryOwnerId`), so a per-user secret stored for
/// another user has to record *that* user, not the acting admin. Bare/shared
/// writes are attributed to the acting admin's own identity (so the owner
/// column shows their name rather than the generic owner-token label), falling
/// back to `RestOwner` only when the caller authenticated with the static
/// owner token and carries no identity.
/// A physical `users/{uid}/<logical>` path whose logical half may only live
/// in a shared tier.
fn per_user_shared_only(path: &str) -> bool {
    path.strip_prefix("users/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, logical)| crate::vault_policy::is_shared_only(logical))
}

/// Who a vault write through the REST API is recorded as: the signed-in user,
/// or the owner token. Per-user entries still name their owner as
/// `created_by` through the vault's owner rule.
fn caller_actor(caller: &CallerContext) -> workforce_common::settings::VaultActor<'_> {
    caller
        .identity_id
        .as_deref()
        .map(workforce_common::settings::VaultActor::User)
        .unwrap_or(workforce_common::settings::VaultActor::RestOwner)
}

/// The response to a failed vault write: 503 while the vault is frozen for a
/// migration (retry shortly), otherwise 500 carrying the cause.
pub(crate) fn vault_write_rejection(error: &eyre::Report) -> (StatusCode, Json<serde_json::Value>) {
    let frozen = error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::vault::VaultError>(),
            Some(crate::vault::VaultError::Frozen { .. })
        )
    });
    if frozen {
        warn!(error = %error, "Vault write refused while the vault is frozen");
        reject_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "The vault is read-only while it moves to a new store. Try again in a few minutes.",
        )
    } else {
        tracing::error!(error = %error, "Vault write failed");
        reject_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("The vault change failed: {error}"),
        )
    }
}

/// A login write the guarded store refused, as the response that says why.
pub(crate) fn login_refusal(
    refusal: crate::hub_core::logins::store::LoginRefusal,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::hub_core::logins::store::LoginRefusal;
    match refusal {
        LoginRefusal::Invalid(_) => reject_json(StatusCode::BAD_REQUEST, &refusal.to_string()),
        LoginRefusal::Duplicate(_) | LoginRefusal::CopyForbidden => {
            reject_json(StatusCode::CONFLICT, &refusal.to_string())
        }
        LoginRefusal::SharedOnly | LoginRefusal::InstallOnly => {
            reject_json(StatusCode::FORBIDDEN, &refusal.to_string())
        }
        LoginRefusal::Vault(e) => vault_write_rejection(&e),
    }
}

/// Every vault the hub holds, where a copy of a login may already sit.
pub(crate) fn login_vaults(state: &GatewayState) -> Vec<Arc<crate::vault::Vault>> {
    state.registry.login_vaults(state.vault.as_ref())
}

fn hub_vault(
    state: &GatewayState,
) -> Result<&Arc<crate::vault::Vault>, (StatusCode, Json<serde_json::Value>)> {
    state
        .vault
        .as_ref()
        .ok_or_else(|| reject_json(StatusCode::SERVICE_UNAVAILABLE, "vault not configured"))
}

#[derive(Debug, Deserialize)]
struct VaultListQuery {
    token: Option<String>,
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VaultSetBody {
    value: String,
}

/// List vault keys (never values). Returns both the legacy flat `keys`
/// list and a richer `entries` list with tier + owner-display info for the
/// admin UI; old callers consume `keys`, new callers consume `entries`.
async fn vault_list(
    Query(query): Query<VaultListQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedRead,
    )
    .await
    .map_err(|(s, _)| s)?;
    let vault = state
        .vault
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let keys: Vec<String> = vault
        .list(query.prefix.as_deref())
        .into_iter()
        .filter(|path| !crate::hub_core::logins::device_login::is_record(path))
        .collect();
    let resolve_display = crate::vault_view::actor_display(state.identity_store.clone());
    let entries = global_vault_entries_for_admin(vault, query.prefix.as_deref(), &resolve_display);
    Ok(Json(
        serde_json::json!({ "keys": keys, "entries": entries }),
    ))
}

/// Project the global vault into admin-view entries. Per-user (`users/...`)
/// rows are labelled `global_per_user`; bare rows are `global`. Skips the
/// WASM-skill private namespace, admin-managed via separate tools, and the
/// ChatGPT sign-ins in flight.
fn global_vault_entries_for_admin(
    vault: &crate::vault::Vault,
    prefix: Option<&str>,
    resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<crate::vault_view::VaultEntryView> {
    use workforce_common::settings::VaultSource;
    vault
        .list(prefix)
        .into_iter()
        .filter(|p| !p.starts_with("wasm-skills/"))
        .filter(|p| !crate::hub_core::logins::device_login::is_record(p))
        .filter_map(|raw| {
            let entry = vault.get_entry(&raw)?;
            let (logical_path, tier) = if let Some(rest) = raw.strip_prefix("users/") {
                let mut parts = rest.splitn(2, '/');
                let _uid = parts.next()?;
                let logical = parts.next()?.to_string();
                (logical, VaultSource::GlobalPerUser)
            } else {
                (raw.clone(), VaultSource::Global)
            };
            Some(crate::vault_view::VaultEntryView::from_entry(
                logical_path,
                tier,
                entry,
                resolve_display,
            ))
        })
        .collect()
}

/// Get a single secret value by dot-notation path.
async fn vault_get(
    Path(path): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedRead,
    )
    .await
    .map_err(|(s, _)| s)?;
    let vault = state
        .vault
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match vault
        .get(&path)
        .filter(|_| !crate::hub_core::logins::device_login::is_record(&path))
    {
        Some(value) => Ok(Json(serde_json::json!({ "path": path, "value": value }))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Set a secret value by dot-notation path.
async fn vault_set(
    Path(path): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<VaultSetBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    if per_user_shared_only(&path) {
        return Err(reject_json(
            StatusCode::FORBIDDEN,
            &crate::vault_policy::shared_only_rejection_message(&path),
        ));
    }
    crate::hub_core::logins::store::set(
        hub_vault(&state)?,
        &path,
        &body.value,
        || login_vaults(&state),
        caller_actor(&caller),
    )
    .await
    .map_err(login_refusal)?;
    info!(path = %path, "Vault secret stored via API");
    Ok(Json(serde_json::json!({ "stored": path })))
}

/// Delete a secret by dot-notation path.
async fn vault_delete(
    Path(path): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedDelete,
    )
    .await?;
    match crate::hub_core::logins::store::delete(hub_vault(&state)?, &path, caller_actor(&caller))
        .await
    {
        Ok(true) => Ok(Json(serde_json::json!({ "deleted": path }))),
        Ok(false) => Err(reject_json(
            StatusCode::NOT_FOUND,
            &format!("key '{path}' not found"),
        )),
        Err(e) => Err(vault_write_rejection(&e)),
    }
}

#[derive(Debug, Deserialize)]
struct VaultRenameBody {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

/// `POST /api/vault/rename` — rename a global-vault secret in one write; the
/// value never leaves the server. A rename writes one path and deletes
/// another, so it needs both capabilities.
async fn vault_rename(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<VaultRenameBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedDelete,
    )
    .await?;
    if body.from.is_empty() || body.to.is_empty() || body.from == body.to {
        return Err(reject_json(
            StatusCode::BAD_REQUEST,
            "rename needs two different, non-empty paths",
        ));
    }
    if per_user_shared_only(&body.to) {
        return Err(reject_json(
            StatusCode::FORBIDDEN,
            &crate::vault_policy::shared_only_rejection_message(&body.to),
        ));
    }
    let vault = hub_vault(&state)?;
    vault.sync().map_err(|e| vault_write_rejection(&e))?;
    let value = vault
        .get(&body.from)
        .ok_or_else(|| reject_json(StatusCode::NOT_FOUND, "no value at the source path"))?;
    if !body.overwrite && vault.get(&body.to).is_some() {
        return Err(reject_json(
            StatusCode::CONFLICT,
            "a value already exists at the destination; pass overwrite=true to replace it",
        ));
    }
    crate::hub_core::logins::store::relocate(
        (vault, &body.from),
        (vault, &body.to),
        &value,
        || login_vaults(&state),
        caller_actor(&caller),
    )
    .await
    .map_err(login_refusal)?;
    info!(from = %body.from, to = %body.to, "Vault secret renamed via API");
    Ok(Json(
        serde_json::json!({ "renamed": body.from, "to": body.to }),
    ))
}

// ── Workflow CRUD ─────────────────────────────────────────────────

/// Query carrying the optional `owner` that, with the bare path id, addresses a
/// specific definition. Absent `owner` = a system/global definition (bare key).
#[derive(serde::Deserialize)]
struct DefRefQuery {
    token: Option<String>,
    owner: Option<String>,
}

/// The share store behind every definition-access decision here.
fn stored_shares(state: &GatewayState) -> StoredShares {
    StoredShares::new(state.def_store.clone(), state.team_store.clone())
}

/// Whether the caller holds administrator standing *now*. A JWT carries the
/// role it was minted with, so the live identity decides
/// ([`crate::authz::is_effective_admin`]): a suspended administrator is the
/// member they have become. This is the flag that reaches past another member's
/// row and past a skill's owner, so it is resolved, never claimed.
pub(crate) fn caller_is_admin(
    identity_store: Option<&Arc<dyn workforce_identity::store::IdentityStore>>,
    caller: &CallerContext,
) -> bool {
    let principal = gateway_principal(caller, identity_store);
    crate::authz::is_effective_admin(principal.role, principal.status)
}

/// The caller as the definition rules see them.
pub(crate) fn def_writer(
    identity_store: Option<&Arc<dyn workforce_identity::store::IdentityStore>>,
    caller: &CallerContext,
) -> DefWriter {
    DefWriter {
        identity_id: caller.identity_id.clone(),
        is_admin: caller_is_admin(identity_store, caller),
    }
}

/// The `{owner}/{id}` keys of definitions of `kind` shared to the caller — the
/// set [`crate::workflow_runtime::WorkflowRuntime::list_definitions_visible`]
/// filters a listing by.
async fn caller_listing(
    state: &GatewayState,
    caller: &CallerContext,
    kind: &str,
) -> crate::resource_access::DefListing {
    crate::resource_access::DefListing::resolve(
        state.def_store.as_ref(),
        def_writer(state.identity_store.as_ref(), caller),
        crate::team_reach::TeamReach::new(state.team_store.clone(), caller.identity_id.clone()),
        kind,
    )
    .await
}

/// Resolve a definition's `(owner, visibility)` across kinds via the owning
/// runtime — the shared entry point for the kind-agnostic share endpoints.
async fn definition_ownership_for(
    state: &GatewayState,
    kind: &str,
    id: &str,
    owner_q: Option<&str>,
) -> Option<(Option<String>, workforce_core::access::Visibility)> {
    use workforce_orchestrator::workflow_store as ws;
    match kind {
        ws::KIND_WORKFLOW => {
            state
                .workflow_runtime
                .as_ref()?
                .definition_ownership(id, owner_q)
                .await
        }
        ws::KIND_PLAYBOOK => {
            state
                .playbook_runtime
                .as_ref()?
                .definition_ownership(id, owner_q)
                .await
        }
        ws::KIND_MONITOR => {
            state
                .monitor_engine
                .as_ref()?
                .definitions()
                .definition_ownership(id, owner_q)
                .await
        }
        ws::KIND_CONSOLIDATION_TEMPLATE => {
            state
                .consolidation_runtime
                .as_ref()?
                .definition_ownership(id, owner_q)
                .await
        }
        _ => None,
    }
}

/// The member who owns the definition a request addresses. One that is not
/// loaded is a `404`; one that belongs to the whole install has no owner to
/// reach past, share from or hand over, so it is a `400`.
async fn definition_member_owner(
    state: &GatewayState,
    kind: &str,
    id: &str,
    owner_q: Option<&str>,
) -> Result<String, StatusCode> {
    definition_ownership_for(state, kind, id, owner_q)
        .await
        .ok_or(StatusCode::NOT_FOUND)?
        .0
        .ok_or(StatusCode::BAD_REQUEST)
}

/// Re-read the definitions of `kind` so a change of who owns one takes effect
/// here without waiting for the store poll, exactly as every other definition
/// write does. A monitor also reconciles its persona layer: a transferred
/// definition is tainted, and a tainted persona must stop driving its channel
/// at once rather than at the next sweep. Consolidation templates have no
/// write surface at all, so there is nothing of theirs to reload.
async fn reload_definitions_for(state: &GatewayState, kind: &str) {
    use workforce_orchestrator::workflow_store as ws;
    let reloaded = match kind {
        ws::KIND_WORKFLOW => match state.workflow_runtime.as_ref() {
            Some(runtime) => runtime.reload_definitions().await.map(drop),
            None => Ok(()),
        },
        ws::KIND_PLAYBOOK => match state.playbook_runtime.as_ref() {
            Some(runtime) => runtime.reload().await.map(drop),
            None => Ok(()),
        },
        ws::KIND_MONITOR => match state.monitor_engine.as_ref() {
            Some(engine) => {
                let reloaded = engine.definitions().reload().await.map(drop);
                engine.reconcile_personas().await;
                reloaded
            }
            None => Ok(()),
        },
        _ => Ok(()),
    };
    if let Err(e) = reloaded {
        warn!(kind, error = %e, "Reloading definitions after an ownership transfer failed");
    }
}

/// Authorize reaching a definition's grantee set and return its owner — the
/// shared rule ([`crate::def_write_auth::authorize_share_manage`]): the owner,
/// or a system administrator overriding them. System/global definitions are
/// not shareable. `op` is `None` to read the set and `Some(_)` to change it, so
/// an administrator's read is not recorded as a change they did not make.
async fn authorize_share_access(
    state: &GatewayState,
    caller: &CallerContext,
    kind: &str,
    id: &str,
    owner_q: Option<&str>,
    op: Option<crate::def_write_auth::DefOp>,
) -> Result<String, StatusCode> {
    let owner = definition_member_owner(state, kind, id, owner_q).await?;
    let writer = def_writer(state.identity_store.as_ref(), caller);
    let shares = stored_shares(state);
    match op {
        Some(op) => {
            crate::def_write_auth::authorize_share_manage(&writer, &shares, kind, id, &owner, op)
                .await
        }
        None => {
            crate::def_write_auth::authorize_share_read(&writer, &shares, kind, id, &owner).await
        }
    }
    .map(|()| owner)
    .map_err(|_| StatusCode::FORBIDDEN)
}

#[derive(serde::Deserialize)]
struct SharesBody {
    shares: Vec<workforce_core::access::GrantEntry>,
}

/// `GET /api/{kind}s/{id}/shares?owner=` — the definition's grantees.
async fn def_shares_get(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    kind: &str,
    id: &str,
    query: &DefRefQuery,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(state, headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let owner =
        authorize_share_access(state, &caller, kind, id, query.owner.as_deref(), None).await?;
    let store = state
        .def_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let stored = store
        .list_shares(kind, id, &owner)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let shares = crate::resource_access::grant_entries(&stored, Some(&owner));
    Ok(Json(serde_json::json!({ "shares": shares })))
}

/// `PUT /api/{kind}s/{id}/shares?owner=` — replace the definition's grantee set.
///
/// A refused grantee set says which entry is wrong, as the skills' own share
/// route does; every other refusal keeps the bare status it has always carried.
async fn def_shares_put(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    kind: &str,
    id: &str,
    query: &DefRefQuery,
    body: SharesBody,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let bare = |status: StatusCode| (status, String::new());
    let caller = resolve_caller_context(state, headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(bare(StatusCode::UNAUTHORIZED));
    }
    let owner = authorize_share_access(
        state,
        &caller,
        kind,
        id,
        query.owner.as_deref(),
        Some(crate::def_write_auth::DefOp::Share),
    )
    .await
    .map_err(bare)?;
    let shares = crate::resource_access::validate_grants(body.shares, Some(&owner))
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let store = state
        .def_store
        .as_ref()
        .ok_or_else(|| bare(StatusCode::SERVICE_UNAVAILABLE))?;
    store
        .set_shares(
            kind,
            id,
            &owner,
            &shares,
            caller.identity_id.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(|_| bare(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(
        serde_json::json!({ "shared": id, "count": shares.len() }),
    ))
}

#[derive(serde::Deserialize)]
struct OwnerBody {
    owner: String,
    /// Leave the outgoing owner a `manage` grant, written inside the transfer's
    /// own transaction. Absent means no, which is the transfer's plain meaning.
    #[serde(default)]
    keep_manage: bool,
}

/// `PUT /api/{kind}s/{id}/owner?owner=` — hand the definition to another
/// member. `?owner=` names the row being transferred *from*, exactly as it
/// names the row being shared; the body names who it goes to.
///
/// The standing, the move and the audit are all
/// [`crate::def_write_auth::transfer_definition`]'s, so this surface cannot
/// forget any of them. What it adds is the HTTP vocabulary, the synchronous
/// reload every other definition write does, and the consequences the
/// requester cannot otherwise see.
async fn def_owner_put(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    kind: &str,
    id: &str,
    query: &DefRefQuery,
    body: OwnerBody,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let bare = |status: StatusCode| (status, String::new());
    let caller = resolve_caller_context(state, headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(bare(StatusCode::UNAUTHORIZED));
    }
    let from = definition_member_owner(state, kind, id, query.owner.as_deref())
        .await
        .map_err(bare)?;
    let defs = state
        .def_store
        .clone()
        .ok_or_else(|| bare(StatusCode::SERVICE_UNAVAILABLE))?;
    let done = crate::def_write_auth::transfer_definition(
        &def_writer(state.identity_store.as_ref(), &caller),
        &stored_shares(state),
        &crate::def_write_auth::TransferStores {
            defs,
            identities: state.identity_store.clone(),
            teams: state.team_store.clone(),
        },
        crate::def_write_auth::DefTransfer {
            kind,
            id,
            from: &from,
            to: &body.owner,
            keep: workforce_core::access::OutgoingAccess::keeping_manage(body.keep_manage),
        },
    )
    .await
    .map_err(transfer_refusal)?;
    reload_definitions_for(state, kind).await;
    let by_owner = caller.identity_id.as_deref() == Some(from.as_str());
    Ok(Json(serde_json::json!({
        "transferred": id,
        "from": done.from,
        "to": done.to,
        "to_display": done.to_display,
        "trigger_paths": done.moved.trigger_paths,
        "notes": crate::def_write_auth::transfer_notes(id, &done, by_owner),
    })))
}

/// A refused transfer in HTTP terms. Every refusal that is the requester's to
/// act on carries the sentence that says how.
fn transfer_refusal(denied: crate::def_write_auth::TransferDenied) -> (StatusCode, String) {
    use crate::def_write_auth::TransferDenied as D;
    let status = match &denied {
        D::Refused(crate::def_write_auth::DefWriteDenied::Unauthenticated) => {
            StatusCode::UNAUTHORIZED
        }
        D::Refused(_) => StatusCode::FORBIDDEN,
        D::Target(_) => StatusCode::BAD_REQUEST,
        D::NotFound { .. } => StatusCode::NOT_FOUND,
        D::Occupied { .. } => StatusCode::CONFLICT,
        D::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, denied.to_string())
}

/// The definition `/owner` route's status vocabulary, pinned where CI can see
/// it: the move itself needs Postgres, but which refusal is the requester's to
/// correct and which is the hub's is a total function over the refusal, and a
/// transfer refused as a `409` rather than a `500` is the difference between a
/// name the requester can change and an incident.
#[cfg(test)]
mod transfer_refusal_tests {
    use super::*;
    use crate::def_write_auth::{DefWriteDenied, TransferDenied as D};

    #[test]
    fn every_refused_transfer_carries_its_own_status_and_its_reason() {
        let cases = [
            (
                D::Refused(DefWriteDenied::Unauthenticated),
                StatusCode::UNAUTHORIZED,
            ),
            (
                D::Refused(DefWriteDenied::NotOwner {
                    id: "triage".into(),
                    owner: "alice".into(),
                }),
                StatusCode::FORBIDDEN,
            ),
            (
                D::Refused(DefWriteDenied::ReservedOwner {
                    owner: "__system__".into(),
                }),
                StatusCode::FORBIDDEN,
            ),
            (
                D::Target("nobody holds that name".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                D::NotFound {
                    word: "workflow".into(),
                    id: "triage".into(),
                },
                StatusCode::NOT_FOUND,
            ),
            (
                D::Occupied {
                    word: "workflow".into(),
                    id: "triage".into(),
                    holder: "Bob".into(),
                },
                StatusCode::CONFLICT,
            ),
            (
                D::Storage("connection reset".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (denied, expected) in cases {
            let reason = denied.to_string();
            let (status, message) = transfer_refusal(denied);
            assert_eq!(status, expected, "{reason}");
            assert_eq!(message, reason, "a refusal must say why, not only that");
            assert!(!message.is_empty());
        }
    }
}

async fn workflow_owner_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OwnerBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_owner_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        &query,
        body,
    )
    .await
}

async fn playbook_owner_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OwnerBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_owner_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        &query,
        body,
    )
    .await
}

async fn monitor_owner_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OwnerBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_owner_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        &query,
        body,
    )
    .await
}

async fn workflow_shares_get(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    def_shares_get(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        &query,
    )
    .await
}

async fn workflow_shares_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SharesBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_shares_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        &query,
        body,
    )
    .await
}

async fn playbook_shares_get(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    def_shares_get(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        &query,
    )
    .await
}

async fn playbook_shares_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SharesBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_shares_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        &query,
        body,
    )
    .await
}

async fn monitor_shares_get(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    def_shares_get(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        &query,
    )
    .await
}

async fn monitor_shares_put(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SharesBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    def_shares_put(
        &state,
        &headers,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        &query,
        body,
    )
    .await
}

/// Read/run authorization on a resolved `(owner, visibility)` under the
/// shared rule ([`may_read_def`]).
async fn authorize_def_read(
    state: &GatewayState,
    caller: &CallerContext,
    kind: &str,
    bare_id: &str,
    owner: Option<String>,
    visibility: workforce_core::access::Visibility,
) -> Result<(Option<String>, workforce_core::access::Visibility), StatusCode> {
    may_read_def(
        &def_writer(state.identity_store.as_ref(), caller),
        &stored_shares(state),
        kind,
        bare_id,
        owner.as_deref(),
        visibility,
        &crate::team_reach::TeamReach::new(state.team_store.clone(), caller.identity_id.clone()),
    )
    .await
    .then_some((owner, visibility))
    .ok_or(StatusCode::FORBIDDEN)
}

/// Resolve the row a write (`PUT`/`DELETE`/restore) addresses and authorize
/// it under the shared rule ([`authorize_def_write`]). An explicit `owner_q`
/// addresses that definition's row — so an edit updates the loaded row rather
/// than forking a new one under the caller (the monitor duplicate-row bug).
/// A refusal carries the rule's message, which names the row's owner.
async fn resolve_def_write(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    token: Option<&str>,
    kind: &str,
    bare_id: &str,
    owner_q: Option<&str>,
    op: crate::def_write_auth::DefOp,
) -> Result<DefWriteGrant, (StatusCode, String)> {
    let caller = resolve_caller_context(state, headers, token).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }
    resolve_def_write_as(state, &caller, kind, bare_id, owner_q, op).await
}

/// [`resolve_def_write`] for a handler that has already resolved its caller —
/// the monitor writes, which need the same caller for the binding gate.
async fn resolve_def_write_as(
    state: &GatewayState,
    caller: &CallerContext,
    kind: &str,
    bare_id: &str,
    owner_q: Option<&str>,
    op: crate::def_write_auth::DefOp,
) -> Result<DefWriteGrant, (StatusCode, String)> {
    authorize_def_write(
        &def_writer(state.identity_store.as_ref(), caller),
        &stored_shares(state),
        kind,
        bare_id,
        owner_q,
        op,
    )
    .await
    .map_err(|denied| (StatusCode::FORBIDDEN, denied.to_string()))
}

/// The visibility a `PUT` should persist ([`write_visibility`] over the stored
/// row's visibility).
async fn resolve_put_visibility(
    state: &GatewayState,
    ctx: &DefWriteGrant,
    kind: &str,
    id: &str,
    requested: Option<&str>,
) -> Result<workforce_core::access::Visibility, String> {
    let stored = definition_ownership_for(state, kind, id, ctx.owner.as_deref())
        .await
        .map(|(_, visibility)| visibility);
    write_visibility(ctx, requested, stored)
}

async fn authorize_workflow_access(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<(Option<String>, workforce_core::access::Visibility), StatusCode> {
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, visibility) = runtime
        .definition_ownership(bare_id, owner_q)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_def_read(
        state,
        caller,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        bare_id,
        owner,
        visibility,
    )
    .await
}

/// Playbook sibling of [`authorize_workflow_access`].
async fn authorize_playbook_access(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<(Option<String>, workforce_core::access::Visibility), StatusCode> {
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, visibility) = runtime
        .definition_ownership(bare_id, owner_q)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_def_read(
        state,
        caller,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        bare_id,
        owner,
        visibility,
    )
    .await
}

async fn workflow_get_source(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, _) =
        authorize_workflow_access(&state, &id, query.owner.as_deref(), &caller).await?;

    match runtime.get_definition_source(&id, owner.as_deref()) {
        Ok(toml) => Ok(([(axum::http::header::CONTENT_TYPE, "text/toml")], toml).into_response()),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// List the workflows visible to the caller: an admin sees all; a member sees
/// global definitions plus their own. Each entry's `id` is the addressing key
/// the run endpoint expects.
async fn workflow_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let listing = caller_listing(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
    )
    .await;
    let defs = runtime.list_definitions_visible(&listing).await;
    let defs = runtime.annotate_automation(defs).await;
    let workflows = runtime.summaries_json(&defs).await;
    Ok(Json(serde_json::json!({ "workflows": workflows })))
}

#[derive(serde::Deserialize)]
struct WorkflowRunBody {
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

/// Run a workflow the caller may access (global, or their own; admins run any),
/// scoped to the runner's identity so per-user resolution uses the runner.
async fn workflow_run(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<WorkflowRunBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }
    let runtime = state.workflow_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "workflow runtime unavailable".to_string(),
    ))?;

    let (owner, _) = authorize_workflow_access(&state, &id, query.owner.as_deref(), &caller)
        .await
        .map_err(|s| (s, "not authorized to run this workflow".to_string()))?;

    // Conservative abuse ceiling: cap concurrent runs per definition owner.
    if runtime.owner_run_quota_exceeded(owner.as_deref()) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many concurrent runs for this workflow's owner".to_string(),
        ));
    }

    // Address by the internal composite key (owner-namespaced for member defs).
    let key = crate::workflow_runtime::def_key(owner.as_deref(), &id);
    // Interactive run: execute as the runner so per-user resolution uses their
    // identity. `start_workflow` captures this ambient scope across its spawn.
    let start = Arc::clone(runtime).start_workflow(
        &key,
        body.payload,
        body.channel_id,
        body.thread_id,
        None,
        None,
    );
    let result = match caller.identity_id.clone() {
        Some(uid) => {
            workforce_agent::runner::CURRENT_USER_ID
                .scope(uid, start)
                .await
        }
        None => start.await,
    };

    match result {
        Ok(instance_id) => Ok(Json(serde_json::json!({
            "instance_id": instance_id,
            "workflow_id": id,
            "status": "running",
        }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

async fn workflow_export(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    match runtime.export_all_sources() {
        Ok(map) => Ok(Json(serde_json::json!(map))),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(serde::Deserialize)]
struct DefPutQuery {
    token: Option<String>,
    /// When renaming a definition, the previous id whose file should be removed
    /// after the new definition saves successfully.
    from: Option<String>,
    /// `"private"` (owner only) or `"global"` (anyone may run). Defaults to
    /// `global` for system/admin definitions, `private` for member definitions.
    visibility: Option<String>,
    /// The owner whose definition this write addresses — set so an edit updates
    /// the loaded row rather than forking one under the caller. Absent for a
    /// create (under the caller) or an admin write to the system row.
    owner: Option<String>,
    /// Who automated runs of a member-owned workflow execute as: `owner`, or
    /// `me` (the saver, who thereby consents). Absent leaves it unchanged.
    /// Workflows only; the other definition kinds refuse it.
    run_as: Option<String>,
}

impl DefPutQuery {
    fn refuse_run_as(&self) -> Result<(), (StatusCode, String)> {
        match self.run_as {
            None => Ok(()),
            Some(_) => Err((
                StatusCode::BAD_REQUEST,
                "run_as applies to workflows only".to_string(),
            )),
        }
    }
}

async fn workflow_put(
    Path(id): Path<String>,
    Query(query): Query<DefPutQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Save,
    )
    .await?;
    let visibility = resolve_put_visibility(
        &state,
        &ctx,
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        query.visibility.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let runtime = state.workflow_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "workflow runtime unavailable".to_string(),
    ))?;
    let rename_from = query.from.as_deref().filter(|from| *from != id);
    let run_as =
        run_as_write(&ctx, query.run_as.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // A rename inserts a fresh row, so an unchanged run identity is carried over.
    let run_as = match (run_as, rename_from) {
        (RunAsWrite::Unchanged, Some(from)) => runtime
            .definition_run_as(from, ctx.owner.as_deref())
            .await
            .map_or(RunAsWrite::Unchanged, RunAsWrite::Identity),
        (run_as, _) => run_as,
    };
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    // Return the error verbatim so the UI can show it (e.g. an id mismatch).
    let warnings = runtime
        .save_definition_source(&id, &body, ctx.owner.as_deref(), visibility, run_as, edit)
        .await
        .map_err(|e| {
            warn!(error = %e, "Failed to save workflow");
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

    // Rename: the new definition saved under `id`; drop the old one (same owner).
    if let Some(from) = query.from.as_deref() {
        if from != id && ctx.authority.controls_access() {
            if let Err(e) = runtime
                .delete_definition(from, ctx.owner.as_deref(), edit)
                .await
            {
                warn!(error = %e, old_id = %from, new_id = %id,
                    "Saved renamed workflow but failed to delete old definition");
            }
        }
    }

    Ok(Json(
        serde_json::json!({ "saved": id, "warnings": warnings }),
    ))
}

async fn workflow_delete(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Delete,
    )
    .await?;
    let runtime = state.workflow_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "workflow runtime unavailable".to_string(),
    ))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    match runtime
        .delete_definition(&id, ctx.owner.as_deref(), edit)
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({ "deleted": id }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

// ── Playbook CRUD endpoints (mirror of the workflow ones) ─────────

async fn playbook_export(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    match runtime.export_all_sources() {
        Ok(map) => Ok(Json(serde_json::json!(map))),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// List the playbooks visible to the caller: an admin sees all; a member sees
/// global definitions plus their own. Each entry's `id` is the bare id and
/// `owner` addresses a member definition for the source/delete endpoints.
async fn playbook_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let listing = caller_listing(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
    )
    .await;
    let defs = runtime.list_definitions_visible(&listing).await;
    let playbooks: Vec<_> = defs
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "description": d.description,
                "steps": d.step_count,
                "owner": d.owner,
                "visibility": d.visibility,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "playbooks": playbooks })))
}

async fn playbook_get_source(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, _) =
        authorize_playbook_access(&state, &id, query.owner.as_deref(), &caller).await?;

    match runtime.get_definition_source(&id, owner.as_deref()) {
        Ok(toml) => Ok(([(axum::http::header::CONTENT_TYPE, "text/toml")], toml).into_response()),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

async fn playbook_put(
    Path(id): Path<String>,
    Query(query): Query<DefPutQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    query.refuse_run_as()?;
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Save,
    )
    .await?;
    let visibility = resolve_put_visibility(
        &state,
        &ctx,
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        query.visibility.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let runtime = state.playbook_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "playbook runtime unavailable".to_string(),
    ))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    // Return the error verbatim so the UI can show it (e.g. an id mismatch).
    if let Err(e) = runtime
        .save_definition_source(&id, &body, ctx.owner.as_deref(), visibility, edit)
        .await
    {
        warn!(error = %e, "Failed to save playbook");
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    // Rename: the new definition saved under `id`; drop the old one (same owner).
    if let Some(from) = query.from.as_deref() {
        if from != id && ctx.authority.controls_access() {
            if let Err(e) = runtime
                .delete_definition(from, ctx.owner.as_deref(), edit)
                .await
            {
                warn!(error = %e, old_id = %from, new_id = %id,
                    "Saved renamed playbook but failed to delete old definition");
            }
        }
    }

    Ok(Json(serde_json::json!({ "saved": id })))
}

async fn playbook_delete(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Delete,
    )
    .await?;
    let runtime = state.playbook_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "playbook runtime unavailable".to_string(),
    ))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    match runtime
        .delete_definition(&id, ctx.owner.as_deref(), edit)
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({ "deleted": id }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

// ── Definition revision history (workflows + playbooks) ──────────

/// Authorize revision access when the live definition is gone (deleted —
/// that's what tombstones are for): admins may view any namespace; everyone
/// else only the owner namespace that is their own identity. Administrator
/// standing is the live one ([`caller_is_admin`]), as it is for a live row, so
/// a suspended administrator reads no more history than they read rows.
fn authorize_deleted_def_access(
    state: &GatewayState,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<Option<String>, StatusCode> {
    if caller_is_admin(state.identity_store.as_ref(), caller) {
        return Ok(owner_q.map(str::to_string));
    }
    match owner_q {
        Some(o) if caller.identity_id.as_deref() == Some(o) => Ok(Some(o.to_string())),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

/// Resolve and authorize the owner namespace for a revision read: live
/// definitions use the normal access check (global / own / admin); deleted
/// ones fall back to [`authorize_deleted_def_access`].
async fn revision_read_owner_workflow(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<Option<String>, StatusCode> {
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match runtime.definition_ownership(bare_id, owner_q).await {
        Some((owner, visibility)) => authorize_def_read(
            state,
            caller,
            workforce_orchestrator::workflow_store::KIND_WORKFLOW,
            bare_id,
            owner,
            visibility,
        )
        .await
        .map(|(o, _)| o),
        None => authorize_deleted_def_access(state, owner_q, caller),
    }
}

/// Playbook sibling of [`revision_read_owner_workflow`].
async fn revision_read_owner_playbook(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<Option<String>, StatusCode> {
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match runtime.definition_ownership(bare_id, owner_q).await {
        Some((owner, visibility)) => authorize_def_read(
            state,
            caller,
            workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
            bare_id,
            owner,
            visibility,
        )
        .await
        .map(|(o, _)| o),
        None => authorize_deleted_def_access(state, owner_q, caller),
    }
}

/// `GET /api/workflows/{id}/revisions` — revision metadata, newest first.
async fn workflow_revisions_list(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner = revision_read_owner_workflow(&state, &id, query.owner.as_deref(), &caller).await?;
    let revisions = runtime
        .list_revisions(&id, owner.as_deref())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

/// `GET /api/workflows/{id}/revisions/{rev}` — one revision with content.
async fn workflow_revision_get(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .workflow_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner = revision_read_owner_workflow(&state, &id, query.owner.as_deref(), &caller).await?;
    let revision = runtime
        .get_revision(&id, owner.as_deref(), rev)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!(revision)))
}

/// `POST /api/workflows/{id}/revisions/{rev}/restore` — restore = save a NEW
/// revision whose content equals `rev` (history is never rewritten). Same
/// authorization as PUT; also revives a deleted definition.
async fn workflow_revision_restore(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Restore,
    )
    .await?;
    let runtime = state.workflow_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "workflow runtime unavailable".to_string(),
    ))?;

    let revision = runtime
        .get_revision(&id, ctx.owner.as_deref(), rev)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("revision {rev} not found")))?;
    if revision.deleted {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot restore a delete tombstone — pick a content revision".to_string(),
        ));
    }
    let content = revision.content.unwrap_or_default();

    // Keep the live definition's visibility; a deleted one revives with the
    // owner-appropriate default (system → global, member → private).
    let visibility = match runtime
        .definition_ownership(&id, ctx.owner.as_deref())
        .await
    {
        Some((_, v)) => v,
        None => resolve_visibility(None, ctx.owner.as_deref())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
    };

    let edit =
        workforce_orchestrator::workflow_store::EditContext::restore(ctx.actor.as_deref(), rev);
    let warnings = runtime
        .save_definition_source(
            &id,
            &content,
            ctx.owner.as_deref(),
            visibility,
            RunAsWrite::Unchanged,
            edit,
        )
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "restored": id, "from_rev": rev, "warnings": warnings }),
    ))
}

/// `GET /api/playbooks/{id}/revisions` — revision metadata, newest first.
async fn playbook_revisions_list(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner = revision_read_owner_playbook(&state, &id, query.owner.as_deref(), &caller).await?;
    let revisions = runtime
        .list_revisions(&id, owner.as_deref())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

/// `GET /api/playbooks/{id}/revisions/{rev}` — one revision with content.
async fn playbook_revision_get(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .playbook_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner = revision_read_owner_playbook(&state, &id, query.owner.as_deref(), &caller).await?;
    let revision = runtime
        .get_revision(&id, owner.as_deref(), rev)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!(revision)))
}

/// `POST /api/playbooks/{id}/revisions/{rev}/restore` — playbook sibling of
/// [`workflow_revision_restore`].
async fn playbook_revision_restore(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Restore,
    )
    .await?;
    let runtime = state.playbook_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "playbook runtime unavailable".to_string(),
    ))?;

    let revision = runtime
        .get_revision(&id, ctx.owner.as_deref(), rev)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("revision {rev} not found")))?;
    if revision.deleted {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot restore a delete tombstone — pick a content revision".to_string(),
        ));
    }
    let content = revision.content.unwrap_or_default();

    let visibility = match runtime
        .definition_ownership(&id, ctx.owner.as_deref())
        .await
    {
        Some((_, v)) => v,
        None => resolve_visibility(None, ctx.owner.as_deref())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
    };

    let edit =
        workforce_orchestrator::workflow_store::EditContext::restore(ctx.actor.as_deref(), rev);
    runtime
        .save_definition_source(&id, &content, ctx.owner.as_deref(), visibility, edit)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({ "restored": id, "from_rev": rev })))
}

// ── Consolidation-template endpoints (member-owned; mirror playbooks) ──

/// Consolidation-template sibling of [`authorize_playbook_access`].
async fn authorize_consolidation_access(
    state: &GatewayState,
    id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<(Option<String>, workforce_core::access::Visibility), StatusCode> {
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, visibility) = runtime
        .definition_ownership(id, owner_q)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_def_read(
        state,
        caller,
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
        id,
        owner,
        visibility,
    )
    .await
}

/// Consolidation sibling of [`revision_read_owner_playbook`].
async fn revision_read_owner_consolidation(
    state: &GatewayState,
    id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<Option<String>, StatusCode> {
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match runtime.definition_ownership(id, owner_q).await {
        Some((owner, visibility)) => authorize_def_read(
            state,
            caller,
            workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
            id,
            owner,
            visibility,
        )
        .await
        .map(|(o, _)| o),
        None => authorize_deleted_def_access(state, owner_q, caller),
    }
}

/// `GET /api/consolidation-templates` — templates visible to the caller (admin:
/// all; member: global + own). Each entry's `name` is the template JSON's
/// optional `name`, else the id.
async fn consolidation_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let listing = caller_listing(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
    )
    .await;
    let templates = runtime
        .list_definitions_visible(&listing)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "templates": templates })))
}

/// `GET /api/consolidation-templates/{id}/source` (+ optional `?owner=`) — the
/// stored template JSON.
async fn consolidation_get_source(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (owner, _) =
        authorize_consolidation_access(&state, &id, query.owner.as_deref(), &caller).await?;

    match runtime.get_definition_source(&id, owner.as_deref()).await {
        Ok(source) => Ok((
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            source,
        )
            .into_response()),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// `PUT /api/consolidation-templates/{id}` (+ optional `?from=&visibility=`) —
/// body is the template JSON text; validated (must parse as JSON with a `lines`
/// array) and saved under the caller's namespace.
async fn consolidation_put(
    Path(id): Path<String>,
    Query(query): Query<DefPutQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    query.refuse_run_as()?;
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Save,
    )
    .await?;
    let visibility = resolve_put_visibility(
        &state,
        &ctx,
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
        &id,
        query.visibility.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let runtime = state.consolidation_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "consolidation runtime unavailable".to_string(),
    ))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    // Return the error verbatim so the UI can show it (e.g. invalid JSON).
    if let Err(e) = runtime
        .save_definition_source(&id, &body, ctx.owner.as_deref(), visibility, edit)
        .await
    {
        warn!(error = %e, "Failed to save consolidation template");
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    // Rename: the new definition saved under `id`; drop the old one (same owner).
    if let Some(from) = query.from.as_deref() {
        if from != id && ctx.authority.controls_access() {
            if let Err(e) = runtime
                .delete_definition(from, ctx.owner.as_deref(), edit)
                .await
            {
                warn!(error = %e, old_id = %from, new_id = %id,
                    "Saved renamed consolidation template but failed to delete old definition");
            }
        }
    }

    Ok(Json(serde_json::json!({ "saved": id })))
}

/// `DELETE /api/consolidation-templates/{id}`.
async fn consolidation_delete(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Delete,
    )
    .await?;
    let runtime = state.consolidation_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "consolidation runtime unavailable".to_string(),
    ))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    match runtime
        .delete_definition(&id, ctx.owner.as_deref(), edit)
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({ "deleted": id }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

/// `GET /api/consolidation-templates/{id}/revisions` — revision metadata.
async fn consolidation_revisions_list(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner =
        revision_read_owner_consolidation(&state, &id, query.owner.as_deref(), &caller).await?;
    let revisions = runtime
        .list_revisions(&id, owner.as_deref())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

/// `GET /api/consolidation-templates/{id}/revisions/{rev}` — one revision.
async fn consolidation_revision_get(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = state
        .consolidation_runtime
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let owner =
        revision_read_owner_consolidation(&state, &id, query.owner.as_deref(), &caller).await?;
    let revision = runtime
        .get_revision(&id, owner.as_deref(), rev)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!(revision)))
}

/// `POST /api/consolidation-templates/{id}/revisions/{rev}/restore` — restore =
/// save a NEW revision whose content equals `rev` (mirrors the playbook one).
async fn consolidation_revision_restore(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Restore,
    )
    .await?;
    let runtime = state.consolidation_runtime.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "consolidation runtime unavailable".to_string(),
    ))?;

    let revision = runtime
        .get_revision(&id, ctx.owner.as_deref(), rev)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("revision {rev} not found")))?;
    if revision.deleted {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot restore a delete tombstone — pick a content revision".to_string(),
        ));
    }
    let content = revision.content.unwrap_or_default();

    let visibility = match runtime
        .definition_ownership(&id, ctx.owner.as_deref())
        .await
    {
        Some((_, v)) => v,
        None => resolve_visibility(None, ctx.owner.as_deref())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
    };

    let edit =
        workforce_orchestrator::workflow_store::EditContext::restore(ctx.actor.as_deref(), rev);
    runtime
        .save_definition_source(&id, &content, ctx.owner.as_deref(), visibility, edit)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({ "restored": id, "from_rev": rev })))
}

#[derive(serde::Deserialize)]
struct ConsolidationParseBody {
    artifact_id: String,
    token: Option<String>,
}

/// `POST /api/consolidation-templates/parse-xlsx` — body `{ artifact_id }` (an
/// `art_<hex>` id pointing at an uploaded xlsx staged as raw bytes in the FS
/// artifact store, `global_store()`). Reads those bytes, parses the
/// consolidation grid, and returns a `ConsolidationTemplate` JSON the caller can
/// then PUT.
///
/// Artifacts are keyed by the owning agent; the staging agent is taken from
/// `?agent_id=`, falling back to the default agent. The gateway carries no
/// session/user task-local, so the read takes the artifact store's unrestricted
/// server-side branch.
async fn consolidation_parse_xlsx(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ConsolidationParseBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let token = body.token.as_deref().or(query.token.as_deref());
    let caller = resolve_caller_context(&state, &headers, token).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }

    let agent = state
        .registry
        .resolve(query.agent_id.as_deref())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no agent".to_string()))?;
    let bytes = read_staged_bytes(agent.agent_id.as_str(), &body.artifact_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let template =
        parse_xlsx_template(&bytes).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(template))
}

/// Read an uploaded artifact's raw bytes by id from the shared FS artifact store.
fn read_staged_bytes(agent_id: &str, artifact_id: &str) -> eyre::Result<Vec<u8>> {
    use workforce_agent::artifacts::{global_store, resolve_bytes_input, BytesRequest};
    let store = global_store().ok_or_else(|| eyre::eyre!("artifact store is not configured"))?;
    let resolved = resolve_bytes_input(
        store.as_ref(),
        agent_id,
        BytesRequest {
            artifact_id: Some(artifact_id),
            ..Default::default()
        },
    )?;
    Ok(resolved.bytes)
}

/// `POST /api/consolidation-templates/upload` — multipart `file` field holding an
/// .xlsx. Stages the raw bytes in the shared artifact store under the resolved
/// agent and returns `{ "artifact_id": "art_…" }` for a follow-up `parse-xlsx`.
async fn consolidation_upload(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }
    let agent = state
        .registry
        .resolve(query.agent_id.as_deref())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no agent".to_string()))?;

    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        if field.name() == Some("file") {
            bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
                    .to_vec(),
            );
            break;
        }
    }
    let bytes = bytes.ok_or((StatusCode::BAD_REQUEST, "missing `file` field".to_string()))?;
    let artifact_id = stage_uploaded_xlsx(agent.agent_id.as_str(), bytes)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({ "artifact_id": artifact_id })))
}

/// Stage raw upload bytes in the shared artifact store, returning the `art_…` id
/// that [`consolidation_parse_xlsx`] reads back.
fn stage_uploaded_xlsx(agent_id: &str, bytes: Vec<u8>) -> eyre::Result<String> {
    stage_bytes("consolidation_upload", agent_id, bytes)
}

const AGENT_IMPORT_BODY_LIMIT: usize = 512 * 1024 * 1024;
const TRANSFER_STAGING_AGENT: &str = "__transfer__";

fn transfer_err(
    status: StatusCode,
    message: impl ToString,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": message.to_string() })),
    )
}

fn transfer_install_id() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "workforce".to_string())
}

/// The owned stores a transfer handler borrows into a `TransferContext`.
struct TransferDeps {
    config: workforce_common::settings::WorkforceConfig,
    config_path: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    identity: Arc<dyn workforce_identity::store::IdentityStore>,
    memory: Arc<dyn workforce_memory::store::MemoryStore>,
    locks: Arc<workforce_common::locking::LockManager>,
}

fn transfer_deps(
    state: &GatewayState,
) -> Result<TransferDeps, (StatusCode, Json<serde_json::Value>)> {
    let (config_path, data_dir, locks) = resolve_overlay_ctx(state)?;
    let config = workforce_common::settings::WorkforceConfig::load(&config_path)
        .map_err(|e| transfer_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let identity = state.identity_store.clone().ok_or_else(|| {
        transfer_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity store unavailable",
        )
    })?;
    let memory = state
        .default_service()
        .and_then(|service| service.memory_store.clone())
        .ok_or_else(|| transfer_err(StatusCode::SERVICE_UNAVAILABLE, "memory store unavailable"))?;
    Ok(TransferDeps {
        config,
        config_path,
        data_dir,
        identity,
        memory,
        locks,
    })
}

impl TransferDeps {
    fn context<'a>(
        &'a self,
        state: &'a GatewayState,
        actor: Option<String>,
    ) -> crate::transfer::TransferContext<'a> {
        crate::transfer::TransferContext {
            data_dir: self.data_dir.clone(),
            base_config_path: self.config_path.clone(),
            config: &self.config,
            identity_store: self.identity.as_ref(),
            memory_store: self.memory.as_ref(),
            lock_manager: self.locks.as_ref(),
            def_store: state.def_store.as_deref(),
            actor,
            install_id: transfer_install_id(),
            slack_team_id: None,
        }
    }
}

#[derive(serde::Deserialize)]
struct AgentExportQuery {
    token: Option<String>,
    agents: String,
}

/// `GET /api/transfer/export?agents=a,b` — streams a `.zip` bundle of the named
/// agents' config, transcripts and memories. Egresses cross-user data, so it is
/// gated by the dedicated [`Capability::AgentExport`].
async fn export_agents(
    Query(query): Query<AgentExportQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    require_named_admin(
        require_capability_action(
            &state,
            &headers,
            query.token.as_deref(),
            crate::authz::Capability::AgentExport,
        )
        .await?,
    )?;
    let agents: Vec<String> = query
        .agents
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if agents.is_empty() {
        return Err(transfer_err(StatusCode::BAD_REQUEST, "no agents specified"));
    }
    let deps = transfer_deps(&state)?;
    let ctx = deps.context(&state, None);
    let (cursor, _counts) =
        crate::transfer::export_bundle(&ctx, &agents, std::io::Cursor::new(Vec::new()))
            .await
            .map_err(|e| transfer_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let mut response = axum::response::Response::new(axum::body::Body::from(cursor.into_inner()));
    let resp_headers = response.headers_mut();
    resp_headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/zip"),
    );
    resp_headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"workforce-export.zip\""),
    );
    Ok(response)
}

/// `POST /api/transfer/import` — multipart `file` field holding a bundle `.zip`.
/// Stages the bytes in the artifact store and returns `{ artifact_id, plan }`;
/// the plan is the dry-run merge the operator approves before `…/apply`.
async fn agent_import_upload(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_named_admin(
        require_capability_action(
            &state,
            &headers,
            query.token.as_deref(),
            crate::authz::Capability::AgentImport,
        )
        .await?,
    )?;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| transfer_err(StatusCode::BAD_REQUEST, e))?
    {
        if field.name() == Some("file") {
            bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| transfer_err(StatusCode::BAD_REQUEST, e))?
                    .to_vec(),
            );
            break;
        }
    }
    let bytes =
        bytes.ok_or_else(|| transfer_err(StatusCode::BAD_REQUEST, "missing `file` field"))?;
    let deps = transfer_deps(&state)?;
    let ctx = deps.context(&state, None);
    let plan = crate::transfer::plan_import(
        &ctx,
        std::io::Cursor::new(bytes.as_slice()),
        &crate::transfer::ImportOptions::default(),
    )
    .map_err(|e| transfer_err(StatusCode::BAD_REQUEST, e))?;
    let artifact_id = stage_bytes("agent_import", TRANSFER_STAGING_AGENT, bytes)
        .map_err(|e| transfer_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(
        serde_json::json!({ "artifact_id": artifact_id, "plan": plan }),
    ))
}

/// `POST /api/transfer/import/{artifact_id}/apply` — applies the staged bundle
/// with the operator-approved [`ImportOptions`], then kicks a registry reload so
/// imported agents (and their sessions) go live.
async fn agent_import_apply(
    Path(artifact_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(options): Json<crate::transfer::ImportOptions>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let admin_id = require_named_admin(
        require_capability_action(
            &state,
            &headers,
            query.token.as_deref(),
            crate::authz::Capability::AgentImport,
        )
        .await?,
    )?;
    let bytes = read_staged_bytes(TRANSFER_STAGING_AGENT, &artifact_id)
        .map_err(|e| transfer_err(StatusCode::NOT_FOUND, e))?;
    let deps = transfer_deps(&state)?;
    let ctx = deps.context(&state, Some(admin_id));
    let outcome = crate::transfer::apply_import(&ctx, std::io::Cursor::new(bytes), &options)
        .await
        .map_err(|e| transfer_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if let Some(kick) = &state.overlay_kick {
        kick.notify_one();
    }
    Ok(Json(
        serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({})),
    ))
}

/// Stage raw upload bytes in the shared artifact store, returning the `art_…`
/// id a follow-up request reads back. `context` tags the artifact's provenance
/// and the artifact inherits the store's upload TTL.
fn stage_bytes(context: &str, agent_id: &str, bytes: Vec<u8>) -> eyre::Result<String> {
    use workforce_agent::artifacts::{global_store, ArtifactBody, ArtifactDescriptor, ContentType};
    let store = global_store().ok_or_else(|| eyre::eyre!("artifact store is not configured"))?;
    let descriptor =
        ArtifactDescriptor::from_context(context, ContentType::OctetStream, agent_id.to_string());
    let handle = store.write(agent_id, descriptor, ArtifactBody::Bytes(bytes))?;
    Ok(handle.id.as_str().to_string())
}

/// Parse xlsx bytes into a [`ConsolidationTemplate`](workforce_skills::adapters::xero::report::ConsolidationTemplate)
/// JSON. Reads the first non-empty sheet's rows as trimmed strings via the
/// shared bounded reader (`workforce_agent::spreadsheet`), then delegates to
/// the pure [`rows_to_template`]. An unreadable/empty workbook yields an
/// empty template; it never panics.
fn parse_xlsx_template(bytes: &[u8]) -> eyre::Result<serde_json::Value> {
    use workforce_agent::spreadsheet::{display_cell, Workbook};

    let mut workbook = Workbook::open(bytes)?;
    let names = workbook.sheet_names();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for name in &names {
        let Ok(grid) = workbook.read_grid(name, |c| display_cell(c).trim().to_string()) else {
            continue;
        };
        if grid.rows.is_empty() {
            continue;
        }
        rows = grid.rows;
        break;
    }
    Ok(rows_to_template(rows))
}

/// Build a `ConsolidationTemplate` JSON from a spreadsheet grid (pure; the
/// parser's testable core). Contract: header row =
/// `["Line", "Kind", <entity>, <entity>, …]`; each data row =
/// `[label, kind, <entity1 source or blank>, …]`. Produces one `lines` entry per
/// data row (order preserved) and `map[entity][source] = label` for every
/// non-blank entity cell. Defensive throughout — missing cells, an empty grid,
/// or an unrecognised kind never panic (kind coerces to `line`).
fn rows_to_template(rows: Vec<Vec<String>>) -> serde_json::Value {
    use workforce_skills::adapters::xero::report::{
        ConsolidationTemplate, TemplateLine, TemplateLineKind,
    };

    let mut template = ConsolidationTemplate::default();
    let Some((header, data)) = rows.split_first() else {
        return serde_json::to_value(&template).unwrap_or_else(|_| serde_json::json!({}));
    };
    // Entity names are the header cells past the Line/Kind columns; a blank
    // header cell can't name an entity column, so it's skipped.
    let entities: Vec<(usize, String)> = header
        .iter()
        .enumerate()
        .skip(2)
        .filter(|(_, name)| !name.trim().is_empty())
        .map(|(i, name)| (i, name.trim().to_string()))
        .collect();

    for row in data {
        let label = row.first().map(|s| s.trim()).unwrap_or("").to_string();
        // A row with no canonical label carries nothing mappable — skip it.
        if label.is_empty() {
            continue;
        }
        let kind = row
            .get(1)
            .map(|k| parse_line_kind(k))
            .unwrap_or(TemplateLineKind::Line);
        template.lines.push(TemplateLine {
            label: label.clone(),
            kind,
            components: Vec::new(),
        });
        for (col, entity) in &entities {
            let source = row.get(*col).map(|s| s.trim()).unwrap_or("");
            if source.is_empty() {
                continue;
            }
            template
                .map
                .entry(entity.clone())
                .or_default()
                .insert(source.to_string(), label.clone());
        }
    }
    serde_json::to_value(&template).unwrap_or_else(|_| serde_json::json!({}))
}

/// Coerce a free-text kind cell to a [`TemplateLineKind`]; anything outside
/// `{header, line, subtotal}` (case-insensitive) — including blank — becomes
/// `Line`, so the output always deserializes as a `ConsolidationTemplate`.
fn parse_line_kind(raw: &str) -> workforce_skills::adapters::xero::report::TemplateLineKind {
    use workforce_skills::adapters::xero::report::TemplateLineKind;
    match raw.trim().to_lowercase().as_str() {
        "header" => TemplateLineKind::Header,
        "subtotal" => TemplateLineKind::Subtotal,
        _ => TemplateLineKind::Line,
    }
}

// ── Channel monitor endpoints ────────────────────────────────────

fn monitor_engine(state: &GatewayState) -> Result<&Arc<dyn crate::hosts::MonitorHost>, StatusCode> {
    state
        .monitor_engine
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// The membership gate every monitor write on this surface passes through: the
/// same rule the agent tool applies, over the engine's own surfaces and the
/// caller's linked platform ids. A REST request carries no platform id of its
/// own, so membership is answered through their linked account alone.
async fn gate_monitor_binding(
    state: &GatewayState,
    engine: &dyn crate::hosts::MonitorHost,
    caller: &CallerContext,
    def: &workforce_orchestrator::monitor::MonitorDefinition,
    stored: Option<&workforce_orchestrator::monitor::MonitorDefinition>,
) -> Result<(), (StatusCode, String)> {
    let store = state.identity_store.clone();
    crate::monitor::BindingAccess::new(
        Some(Arc::clone(engine.surfaces())),
        crate::monitor::linked_platform_ids(move || store.clone()),
    )
    .authorize(
        &crate::monitor::BindingCaller {
            is_admin: caller_is_admin(state.identity_store.as_ref(), caller),
            identity: caller.identity_id.clone(),
            turn_platform_id: None,
        },
        def,
        stored,
    )
    .await
    .map_err(|denied| (StatusCode::FORBIDDEN, denied.to_string()))
}

/// Monitor sibling of [`authorize_workflow_access`].
async fn authorize_monitor_access(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<(Option<String>, workforce_core::access::Visibility), StatusCode> {
    let runtime = state
        .monitor_engine
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?
        .definitions();
    let (owner, visibility) = runtime
        .definition_ownership(bare_id, owner_q)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_def_read(
        state,
        caller,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        bare_id,
        owner,
        visibility,
    )
    .await
}

/// Monitor sibling of [`revision_read_owner_workflow`].
async fn revision_read_owner_monitor(
    state: &GatewayState,
    bare_id: &str,
    owner_q: Option<&str>,
    caller: &CallerContext,
) -> Result<Option<String>, StatusCode> {
    let runtime = state
        .monitor_engine
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?
        .definitions();
    match runtime.definition_ownership(bare_id, owner_q).await {
        Some((owner, visibility)) => authorize_def_read(
            state,
            caller,
            workforce_orchestrator::workflow_store::KIND_MONITOR,
            bare_id,
            owner,
            visibility,
        )
        .await
        .map(|(o, _)| o),
        None => authorize_deleted_def_access(state, owner_q, caller),
    }
}

async fn monitor_export(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    match monitor_engine(&state)?.definitions().export_all_sources() {
        Ok(map) => Ok(Json(serde_json::json!(map))),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn monitor_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let listing = caller_listing(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
    )
    .await;
    let defs = monitor_engine(&state)?.list_visible(&listing).await;
    Ok(Json(serde_json::json!({ "monitors": defs })))
}

async fn monitor_get_source(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = monitor_engine(&state)?.definitions();
    let (owner, _) = authorize_monitor_access(&state, &id, query.owner.as_deref(), &caller).await?;
    match runtime.get_definition_source(&id, owner.as_deref()) {
        Ok(toml) => Ok(([(axum::http::header::CONTENT_TYPE, "text/toml")], toml).into_response()),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

async fn monitor_put(
    Path(id): Path<String>,
    Query(query): Query<DefPutQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    query.refuse_run_as()?;
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }
    let ctx = resolve_def_write_as(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Save,
    )
    .await?;
    let visibility = resolve_put_visibility(
        &state,
        &ctx,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        query.visibility.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let engine =
        monitor_engine(&state).map_err(|s| (s, "monitor engine unavailable".to_string()))?;
    let runtime = engine.definitions();
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());

    // The stored definition answers both what this write is changing (the
    // binding gate) and whether it is a resume (the catch-up policy).
    let previous = runtime.stored_definition(&id, ctx.owner.as_deref()).await;
    let parsed = workforce_orchestrator::monitor::MonitorDefinition::from_toml(&body).ok();

    if let Some(def) = &parsed {
        engine
            .preflight(def)
            .await
            .map_err(|refused| (StatusCode::BAD_REQUEST, refused))?;
        gate_monitor_binding(&state, engine.as_ref(), &caller, def, previous.as_ref()).await?;
        if let workforce_orchestrator::monitor::Action::Persona { agent, .. } = &def.monitor.action
        {
            if state.registry.get(agent).is_none() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("unknown agent '{agent}' — bind it to the channel first"),
                ));
            }
        }
    }

    if let Err(e) = runtime
        .save_definition_source(&id, &body, ctx.owner.as_deref(), visibility, edit)
        .await
    {
        warn!(error = %e, "Failed to save monitor");
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }
    if let Some(from) = query.from.as_deref() {
        if from != id && ctx.authority.controls_access() {
            if let Err(e) = runtime
                .delete_definition(from, ctx.owner.as_deref(), edit)
                .await
            {
                warn!(error = %e, old_id = %from, new_id = %id,
                    "Saved renamed monitor but failed to delete old definition");
            }
        }
    }
    if let Some(def) = &parsed {
        engine
            .apply_catch_up_policy(
                &workforce_orchestrator::monitor::MonitorKey::new(&id, ctx.owner.as_deref()),
                def,
                previous.as_ref(),
            )
            .await;
        engine.probe(def, ctx.owner.as_deref()).await;
    }
    engine.reconcile_personas().await;
    let summary = engine.summary_of(&id, ctx.owner.as_deref()).await;
    Ok(Json(serde_json::json!({
        "saved": id,
        "agent": summary.as_ref().and_then(|s| s.agent.clone()),
        "status": summary.as_ref().map(|s| s.status),
        "block": summary.and_then(|s| s.block),
    })))
}

async fn monitor_delete(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ctx = resolve_def_write(
        &state,
        &headers,
        query.token.as_deref(),
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Delete,
    )
    .await?;
    let engine =
        monitor_engine(&state).map_err(|s| (s, "monitor engine unavailable".to_string()))?;
    let edit =
        workforce_orchestrator::workflow_store::EditContext::new("web", ctx.actor.as_deref());
    match engine.delete_monitor(&id, ctx.owner.as_deref(), edit).await {
        Ok(()) => Ok(Json(serde_json::json!({ "deleted": id }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

async fn monitor_revisions_list(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = monitor_engine(&state)?.definitions();
    let owner = revision_read_owner_monitor(&state, &id, query.owner.as_deref(), &caller).await?;
    let revisions = runtime
        .list_revisions(&id, owner.as_deref())
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

async fn monitor_revision_get(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let runtime = monitor_engine(&state)?.definitions();
    let owner = revision_read_owner_monitor(&state, &id, query.owner.as_deref(), &caller).await?;
    let revision = runtime
        .get_revision(&id, owner.as_deref(), rev)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!(revision)))
}

async fn monitor_revision_restore(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }
    let ctx = resolve_def_write_as(
        &state,
        &caller,
        workforce_orchestrator::workflow_store::KIND_MONITOR,
        &id,
        query.owner.as_deref(),
        crate::def_write_auth::DefOp::Restore,
    )
    .await?;
    let engine =
        monitor_engine(&state).map_err(|s| (s, "monitor engine unavailable".to_string()))?;
    let runtime = engine.definitions();
    let revision = runtime
        .get_revision(&id, ctx.owner.as_deref(), rev)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("revision {rev} not found")))?;
    if revision.deleted {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot restore a delete tombstone — pick a content revision".to_string(),
        ));
    }
    let content = revision.content.unwrap_or_default();
    // A restore is a save, and may restore a different binding or delivery
    // target: it passes the same membership gate and the same catch-up policy.
    let previous = runtime.stored_definition(&id, ctx.owner.as_deref()).await;
    let parsed = workforce_orchestrator::monitor::MonitorDefinition::from_toml(&content).ok();
    if let Some(def) = &parsed {
        engine
            .preflight(def)
            .await
            .map_err(|refused| (StatusCode::BAD_REQUEST, refused))?;
        gate_monitor_binding(&state, engine.as_ref(), &caller, def, previous.as_ref()).await?;
    }
    let visibility = match runtime
        .definition_ownership(&id, ctx.owner.as_deref())
        .await
    {
        Some((_, v)) => v,
        None => resolve_visibility(None, ctx.owner.as_deref())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
    };
    let edit =
        workforce_orchestrator::workflow_store::EditContext::restore(ctx.actor.as_deref(), rev);
    runtime
        .save_definition_source(&id, &content, ctx.owner.as_deref(), visibility, edit)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Some(def) = &parsed {
        engine
            .apply_catch_up_policy(
                &workforce_orchestrator::monitor::MonitorKey::new(&id, ctx.owner.as_deref()),
                def,
                previous.as_ref(),
            )
            .await;
    }
    Ok(Json(serde_json::json!({ "restored": id, "from_rev": rev })))
}

/// `GET /api/monitors/{id}/activity` — recent dispatches for the monitor.
async fn monitor_activity(
    Path(id): Path<String>,
    Query(query): Query<DefRefQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let engine = monitor_engine(&state)?;
    let (owner, _) = authorize_monitor_access(&state, &id, query.owner.as_deref(), &caller).await?;
    let activity = engine
        .store()
        .recent_activity(&id, owner.as_deref(), 50)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(serde_json::json!({ "activity": activity })))
}

// ── Agent config.toml endpoints ──────────────────────────────────

/// The agent's workspace config.toml as it stands, `None` when there is none.
fn read_agent_config(
    id: &str,
    workspace: &std::path::Path,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    match workforce_common::config_files::read(
        &workspace.join(workforce_common::config_files::WORKSPACE_CONFIG),
    ) {
        Ok(content) => Ok(content),
        Err(e) => {
            warn!(agent_id = %id, error = %e, "Failed to read agent config.toml");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read config.toml"})),
            ))
        }
    }
}

/// The entity tag a config.toml body is served with and checked against.
pub(crate) fn config_etag(content: &str) -> String {
    format!(
        "\"{}\"",
        workforce_orchestrator::workflow_store::content_hash(content)
    )
}

/// Whether a write's `If-Match` names the config.toml on disk (`None`: there
/// is none). `*` names any existing file. No `If-Match` writes
/// unconditionally.
pub(crate) fn if_match_holds(headers: &axum::http::HeaderMap, current: Option<&str>) -> bool {
    let Some(expected) = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    let current_hash = config_etag(current.unwrap_or_default());
    let tag = |t: &str| {
        t.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    };
    expected.split(',').any(|t| match t.trim() {
        "*" => current.is_some(),
        t => tag(t) == tag(&current_hash),
    })
}

/// What a person reads when a settings change failed on the hub's side.
const SAVE_FAILED: &str = "Saving the change failed; try again.";

/// A config.toml write that did not land, as the response that says why: in
/// plain words for whoever made the change (Desk shows `error` as it is),
/// the details in the log and `findings`.
pub(crate) fn config_write_refused(
    id: &str,
    refused: crate::agent_config_write::Refused,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::agent_config_write::Refused;
    match refused {
        Refused::Stale { current_hash } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "These settings were changed somewhere else while you were editing \
                          them. Reload them and make your change again.",
                "current_hash": format!("\"{current_hash}\""),
            })),
        ),
        Refused::Unloadable(refusal) => config_write_unloadable(id, refusal),
        Refused::Revision(e) => {
            warn!(agent_id = %id, error = %e, "Config revision capture failed — refusing write");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": SAVE_FAILED})),
            )
        }
        Refused::Io(e) => {
            warn!(agent_id = %id, error = %e, "Failed to write agent config.toml");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": SAVE_FAILED})),
            )
        }
    }
}

/// A config.toml write the agent would not load, as a 400 in plain words:
/// a setting's own plain refusal as it is, anything else generically, with
/// what was wrong in `findings` (a body that doesn't read as one finding).
fn config_write_unloadable(
    id: &str,
    refusal: crate::workspace_admission::ConfigRefusal,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::workspace_admission::ConfigRefusal;
    info!(agent_id = %id, refusal = %refusal, "Agent config.toml write refused");
    let (error, findings) = match refusal {
        ConfigRefusal::Said(said) => (format!("That change couldn’t be saved. {said}"), None),
        ConfigRefusal::Refused(findings) => (
            "That change couldn’t be saved: the agent can’t run with those settings.".to_string(),
            Some(findings),
        ),
        ConfigRefusal::Parse(detail) => (
            "That change couldn’t be saved: the agent can’t read those settings.".to_string(),
            Some(vec![crate::subscription::Finding::refused(
                workforce_common::config_files::WORKSPACE_CONFIG,
                detail,
            )]),
        ),
    };
    let mut body = serde_json::json!({ "error": error });
    if let Some(findings) = findings {
        body["findings"] = serde_json::json!(findings);
    }
    (StatusCode::BAD_REQUEST, Json(body))
}

/// A config the agent would not load, as a 400.
fn config_refused(
    refusal: crate::workspace_admission::ConfigRefusal,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut body = serde_json::json!({"error": refusal.to_string()});
    if let crate::workspace_admission::ConfigRefusal::Refused(findings) = &refusal {
        body["findings"] = serde_json::json!(findings);
    }
    (StatusCode::BAD_REQUEST, Json(body))
}

/// `GET /api/agents/{id}/config` — Read agent workspace config.toml, tagged
/// with the `ETag` a later `If-Match` write names it by.
async fn agent_config_get(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let service = resolve_agent(&state, &id)?;
    let content = read_agent_config(&id, &service.workspace)?.unwrap_or_default();
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/toml; charset=utf-8".to_string(),
            ),
            (axum::http::header::ETAG, config_etag(&content)),
            // A compressing proxy (Caddy's `encode`) otherwise suffixes the
            // ETag with its coding, so no `If-Match` echoing it ever matches.
            (
                axum::http::header::CACHE_CONTROL,
                "no-transform".to_string(),
            ),
        ],
        content,
    )
        .into_response())
}

/// `POST /api/agents/{id}/config/check` — what the agent would make of a
/// config.toml body, without writing it: every finding, refused ones
/// included, or 400 when it does not parse. Same auth as the write.
async fn agent_config_check(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use crate::workspace_admission::ConfigRefusal;
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    let service = resolve_agent(&state, &id)?;
    match crate::workspace_admission::admit_workspace(&service, &body) {
        Ok(findings) | Err(ConfigRefusal::Refused(findings)) => {
            Ok(Json(serde_json::json!({ "findings": findings })))
        }
        Err(other) => Err(config_refused(other)),
    }
}

/// `PUT /api/agents/{id}/config` — Write agent workspace config.toml, only if
/// the agent would load it: a refused body leaves the file and its revision
/// history as they were.
async fn agent_config_put(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "You aren’t allowed to change this agent’s settings.",
            })),
        ));
    }
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let service = state.registry.get(&id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("There’s no agent called {id}.")})),
        )
    })?;
    let edit = workforce_orchestrator::workflow_store::EditContext::new(
        "web",
        caller.identity_id.as_deref(),
    );
    let saved = crate::agent_config_write::save(
        &service,
        &id,
        state.def_store.as_deref(),
        &body,
        edit,
        |current| if_match_holds(&headers, current),
    )
    .await
    .map_err(|refused| config_write_refused(&id, refused))?;
    Ok(Json(serde_json::json!({
        "saved": true,
        "restart_required": saved.restart_required,
        "findings": saved.findings,
        "etag": config_etag(&body),
    })))
}

/// `GET /api/agents/{id}/config/revisions` — revision metadata for the
/// agent's workspace config.toml, newest first. Same scope as the config read.
async fn agent_config_revisions_list(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let _service = resolve_agent(&state, &id)?;
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let revisions = store
        .list_revisions(ws::KIND_AGENT_CONFIG, &id, None)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;
    Ok(Json(serde_json::json!({ "revisions": revisions })))
}

/// `GET /api/agents/{id}/config/revisions/{rev}` — one revision with content.
async fn agent_config_revision_get(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let _service = resolve_agent(&state, &id)?;
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let revision = store
        .get_revision(ws::KIND_AGENT_CONFIG, &id, None, rev)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("revision {rev} not found")})),
        ))?;
    Ok(Json(serde_json::json!(revision)))
}

/// `POST /api/agents/{id}/config/revisions/{rev}/restore` — write revision
/// `rev`'s content back to the workspace config.toml, hot-reload the agent,
/// and record a NEW `restore` revision (history is never rewritten). Same
/// auth as the config write.
async fn agent_config_revision_restore(
    Path((id, rev)): Path<(String, i32)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_orchestrator::workflow_store as ws;
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let service = resolve_agent(&state, &id)?;
    let store = state.def_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "revision history requires Postgres"})),
    ))?;
    let revision = store
        .get_revision(ws::KIND_AGENT_CONFIG, &id, None, rev)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("revision {rev} not found")})),
        ))?;
    let content = revision.content.unwrap_or_default();
    let edit = ws::EditContext::restore(caller.identity_id.as_deref(), rev);
    let saved =
        crate::agent_config_write::save(&service, &id, Some(store), &content, edit, |_| true)
            .await
            .map_err(|refused| match refused {
                crate::agent_config_write::Refused::Unloadable(refusal) => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!("revision {rev} would not load: {refusal}"),
                    })),
                ),
                refused => config_write_refused(&id, refused),
            })?;
    info!(agent_id = %id, from_rev = rev, "Agent config.toml restored via API");
    Ok(Json(serde_json::json!({
        "restored": id,
        "from_rev": rev,
        "restart_required": saved.restart_required,
    })))
}

// ── Per-agent vault endpoints ────────────────────────────────────

/// `GET /api/agents/{id}/vault` — List keys + projected entries in agent vault.
async fn agent_vault_list(
    Path(id): Path<String>,
    Query(query): Query<VaultListQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_capability_read(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedRead,
    )
    .await?;
    let service = resolve_agent(&state, &id)?;
    let vault = service.vault.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "vault not configured for this agent"})),
        )
    })?;
    let keys = vault.list(query.prefix.as_deref());
    let resolve_display = crate::vault_view::actor_display(state.identity_store.clone());
    let entries =
        agent_vault_entries_for_admin(vault, &id, query.prefix.as_deref(), &resolve_display);
    Ok(Json(
        serde_json::json!({ "keys": keys, "entries": entries }),
    ))
}

/// Project an agent vault into admin-view entries. Per-user (`users/...`)
/// rows in an agent vault are tier `agent_per_user`; bare rows are
/// `agent_specific`. Each entry carries the agent_id so the UI can
/// disambiguate when the table renders entries from multiple agents.
fn agent_vault_entries_for_admin(
    vault: &crate::vault::Vault,
    agent_id: &str,
    prefix: Option<&str>,
    resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<crate::vault_view::VaultEntryView> {
    use workforce_common::settings::VaultSource;
    vault
        .list(prefix)
        .into_iter()
        .filter(|p| !p.starts_with("wasm-skills/"))
        .filter_map(|raw| {
            let entry = vault.get_entry(&raw)?;
            let (logical_path, tier) = if let Some(rest) = raw.strip_prefix("users/") {
                let mut parts = rest.splitn(2, '/');
                let _uid = parts.next()?;
                let logical = parts.next()?.to_string();
                (logical, VaultSource::AgentPerUser)
            } else {
                (raw.clone(), VaultSource::AgentSpecific)
            };
            Some(crate::vault_view::VaultEntryView::from_entry_with_agent(
                logical_path,
                tier,
                Some(agent_id.to_string()),
                entry,
                resolve_display,
            ))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct AgentVaultMoveBody {
    path: String,
    from_tier: String,
    to_tier: String,
    /// Optional destination path. When set and different from `path`, the entry
    /// is renamed (and may also change tier). When omitted, the path is kept and
    /// only the tier changes. A same-tier + same-path request is a no-op error.
    #[serde(default)]
    to_path: Option<String>,
    /// Required for moves that touch a per-user tier — identifies which
    /// user's entry to move. Optional for shared-tier-only moves.
    user_id: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

/// `POST /api/agents/{id}/vault/move` — Move a value between any two of the
/// four tiers (T1/T3 live in the agent vault; T2/T4 in the global vault).
/// Owner-token gated; the request body declares which user's entry to move
/// when a per-user tier is involved.
async fn agent_vault_move(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<AgentVaultMoveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    let from = parse_tier_param(&body.from_tier)?;
    let to = parse_tier_param(&body.to_tier)?;
    let dest_path = body.to_path.clone().unwrap_or_else(|| body.path.clone());
    if from == to && dest_path == body.path {
        return Err(reject_json(
            StatusCode::BAD_REQUEST,
            "no-op move: same tier and same path (set to_path to rename)",
        ));
    }

    let service = resolve_agent(&state, &id)?;
    let agent_vault = service.vault.clone().ok_or_else(|| {
        reject_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault not configured for this agent",
        )
    })?;
    let store = crate::user_config::LayeredUserVault::new(agent_vault, state.vault.clone());

    let planned = run_with_optional_identity(body.user_id.clone(), || {
        use workforce_common::settings::SecretStore;
        store.sync().map_err(MoveOutcome::Backend)?;
        let source_value = store
            .inspect(&body.path)
            .into_iter()
            .find(|(t, _)| *t == from)
            .map(|(_, v)| v)
            .ok_or(MoveOutcome::SourceMissing)?;
        let dest_exists = store.inspect(&dest_path).into_iter().any(|(t, _)| t == to);
        if dest_exists && !body.overwrite {
            return Err(MoveOutcome::DestinationExists);
        }
        let source = store
            .write_target(&body.path, from)
            .map_err(MoveOutcome::Backend)?;
        let target = store
            .write_target(&dest_path, to)
            .map_err(MoveOutcome::Backend)?;
        Ok((source_value, source, target))
    });
    let outcome = match planned {
        Ok((value, (source, source_key), (target, target_key))) => {
            crate::hub_core::logins::store::relocate(
                (&source, &source_key),
                (&target, &target_key),
                &value,
                || login_vaults(&state),
                caller_actor(&caller),
            )
            .await
            .map_err(MoveOutcome::Refused)
        }
        Err(outcome) => Err(outcome),
    };
    match outcome {
        Ok(()) => {
            info!(
                agent_id = %id,
                vault_path = %body.path,
                to_path = %dest_path,
                from = from.label(),
                to = to.label(),
                user_id = body.user_id.as_deref().unwrap_or("-"),
                "Vault entry moved"
            );
            Ok(Json(serde_json::json!({
                "moved": body.path, "to_path": dest_path,
                "from": from.label(), "to": to.label(),
            })))
        }
        Err(MoveOutcome::SourceMissing) => Err(reject_json(
            StatusCode::NOT_FOUND,
            "no value at source tier",
        )),
        Err(MoveOutcome::DestinationExists) => Err(reject_json(
            StatusCode::CONFLICT,
            "destination tier already has a value; pass overwrite=true to replace",
        )),
        Err(MoveOutcome::Backend(e)) => Err(vault_write_rejection(&e)),
        Err(MoveOutcome::Refused(refusal)) => Err(login_refusal(refusal)),
    }
}

enum MoveOutcome {
    SourceMissing,
    DestinationExists,
    Backend(eyre::Report),
    Refused(crate::hub_core::logins::store::LoginRefusal),
}

use crate::vault_copy::{
    copied_count, copy_credential, copy_outcome_json, dedup_destinations, logical_path_error,
    run_with_optional_identity, CopyRequest,
};

fn parse_tier_param(
    s: &str,
) -> Result<workforce_common::settings::VaultSource, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::settings::VaultSource;
    match s {
        "agent_per_user" | "agent_user_scoped" => Ok(VaultSource::AgentPerUser),
        "global_per_user" | "global_user_scoped" | "user" => Ok(VaultSource::GlobalPerUser),
        "agent_specific" | "agent_shared_bare" | "agent_shared" => Ok(VaultSource::AgentSpecific),
        "global" | "global_shared_bare" => Ok(VaultSource::Global),
        other => Err(reject_json(
            StatusCode::BAD_REQUEST,
            &format!("unknown vault tier: {other}"),
        )),
    }
}

fn reject_json(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

#[derive(Debug, Deserialize)]
struct AgentVaultCopyBody {
    path: String,
    from_tier: String,
    to_tier: String,
    /// Source user — required when `from_tier` is per-user, ignored otherwise.
    #[serde(default)]
    user_id: Option<String>,
    to_user_ids: Vec<String>,
    #[serde(default)]
    overwrite: bool,
}

fn resolve_source_user(
    from_tier: workforce_common::settings::VaultSource,
    user_id: Option<&str>,
) -> Result<Option<&str>, (StatusCode, Json<serde_json::Value>)> {
    match (from_tier.is_per_user(), user_id) {
        (true, None) => Err(reject_json(
            StatusCode::BAD_REQUEST,
            "user_id (source user) is required when from_tier is per-user",
        )),
        (true, some) => Ok(some),
        (false, _) => Ok(None),
    }
}

fn validate_logical_path(path: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match logical_path_error(path) {
        Some(msg) => Err(reject_json(StatusCode::BAD_REQUEST, msg)),
        None => Ok(()),
    }
}

/// Require an attributable system-admin identity. The shared gateway/owner
/// token authenticates as an admin but carries no `identity_id`; this action
/// propagates a credential across users, so it is restricted to a named
/// SystemAdmin (JWT) for a non-repudiable audit trail.
fn require_named_admin(
    caller: CallerContext,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    caller.identity_id.ok_or_else(|| {
        reject_json(
            StatusCode::FORBIDDEN,
            "this action requires an authenticated system admin (JWT); the shared gateway token is not permitted",
        )
    })
}

/// `POST /api/agents/{id}/vault/copy` — Authenticated SystemAdmin only (the
/// shared gateway token is rejected). Copy a credential from a source (any
/// tier) into one or more destination users' per-user slots within the agent
/// vault, preserving the source. The value never appears in the response.
async fn agent_vault_copy(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<AgentVaultCopyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let admin_id = require_named_admin(
        require_capability_action(
            &state,
            &headers,
            query.token.as_deref(),
            crate::authz::Capability::VaultSharedWrite,
        )
        .await?,
    )?;
    let from = parse_tier_param(&body.from_tier)?;
    let to = parse_tier_param(&body.to_tier)?;
    if !to.is_per_user() {
        return Err(reject_json(
            StatusCode::BAD_REQUEST,
            "destination tier must be per-user (agent_per_user or global_per_user)",
        ));
    }
    validate_logical_path(&body.path)?;
    crate::hub_core::logins::store::refuse_copy(&body.path).map_err(login_refusal)?;
    let from_user = resolve_source_user(from, body.user_id.as_deref())?;
    let to_users = dedup_destinations(&body.to_user_ids);
    if to_users.is_empty() {
        return Err(reject_json(
            StatusCode::BAD_REQUEST,
            "to_user_ids must contain at least one user",
        ));
    }

    let service = resolve_agent(&state, &id)?;
    let agent_vault = service.vault.clone().ok_or_else(|| {
        reject_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault not configured for this agent",
        )
    })?;
    let store: Arc<dyn workforce_common::settings::SecretStore> = Arc::new(
        crate::user_config::LayeredUserVault::new(agent_vault, state.vault.clone()),
    );

    let req = CopyRequest {
        path: &body.path,
        from_tier: from,
        to_tier: to,
        from_user,
        to_users: &to_users,
        overwrite: body.overwrite,
    };
    let outcomes = copy_credential(store.as_ref(), &req).ok_or_else(|| {
        reject_json(
            StatusCode::NOT_FOUND,
            "no value at the source tier for the given user",
        )
    })?;

    let copied = copied_count(&outcomes);
    info!(
        agent_id = %id,
        actor = %admin_id,
        vault_path = %body.path,
        from = from.label(),
        to = to.label(),
        source_user = from_user.unwrap_or("-"),
        destinations = to_users.len(),
        copied,
        "Vault credential copied across users"
    );
    Ok(Json(serde_json::json!({
        "agent_id": id,
        "path": body.path,
        "from_tier": from.label(),
        "to_tier": to.label(),
        "source_user": from_user,
        "copied": copied,
        "results": outcomes.iter().map(copy_outcome_json).collect::<Vec<_>>(),
    })))
}

/// `POST /api/agents/{id}/vault/{*path}` — Set a secret in agent vault.
async fn agent_vault_set(
    Path((id, path)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<VaultSetBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    let service = resolve_agent(&state, &id)?;
    if per_user_shared_only(&path) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": crate::vault_policy::shared_only_rejection_message(&path)
            })),
        ));
    }
    let vault = service.vault.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "vault not configured for this agent"})),
        )
    })?;
    crate::hub_core::logins::store::set(
        vault,
        &path,
        &body.value,
        || login_vaults(&state),
        caller_actor(&caller),
    )
    .await
    .map_err(login_refusal)?;
    info!(agent_id = %id, path = %path, "Agent vault secret stored via API");
    Ok(Json(serde_json::json!({ "stored": path })))
}

/// `DELETE /api/agents/{id}/vault/{*path}` — Delete a secret from agent vault.
async fn agent_vault_delete(
    Path((id, path)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedDelete,
    )
    .await?;
    let service = resolve_agent(&state, &id)?;
    let vault = service.vault.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "vault not configured for this agent"})),
        )
    })?;
    match crate::hub_core::logins::store::delete(vault, &path, caller_actor(&caller)).await {
        Ok(true) => Ok(Json(serde_json::json!({ "deleted": path }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("key '{}' not found", path)})),
        )),
        Err(e) => Err(vault_write_rejection(&e)),
    }
}

// ── Agent skills CRUD endpoints ──────────────────────────────────

/// `GET /api/agents/{id}/skills` — List skills in agent workspace.
async fn agent_skills_list(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    let service = resolve_agent(&state, &id)?;
    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // 1. The live SKILL.md skills the caller may use
    for skill in crate::external_skills::of(&service)
        .snapshot()
        .iter()
        .filter(|skill| {
            caller
                .access(*skill)
                .allows(workforce_core::access::AccessLevel::Run)
        })
    {
        let meta = &skill.discovered.skill.meta;
        seen_names.insert(meta.name.clone());
        skills.push(serde_json::json!({
            "name": meta.name,
            "description": meta.description,
            "type": "external",
            "editable": true,
            "available": skill.discovered.is_available(),
        }));
    }

    // 2. Builtin adapter skills — (display_name, directory_name, description)
    let builtins = [
        (
            "github",
            "github",
            "GitHub integration — PRs, issues, repos",
        ),
        (
            "google",
            "google",
            "Google Workspace — Calendar, Drive, Docs",
        ),
        ("linear", "linear", "Linear project management"),
        ("slack", "slack-integration", "Slack messaging"),
        ("browser", "browser", "Web browsing and screenshots"),
        ("email", "email", "Email sending and reading"),
    ];
    for (name, dir_name, desc) in builtins {
        let has_config = service
            .skills_dir
            .join(dir_name)
            .join("config.toml")
            .exists();
        seen_names.insert(dir_name.to_string());
        skills.push(serde_json::json!({
            "name": name,
            "dir_name": dir_name,
            "description": desc,
            "type": "builtin",
            "editable": false,
            "available": true,
            "has_config": has_config,
        }));
    }

    // 3. Fallback: scan skills/ directory for any skill dirs not already listed
    let skills_dir = &service.skills_dir;
    if skills_dir.is_dir() {
        let mut entries = match tokio::fs::read_dir(&skills_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!(agent_id = %id, error = %e, "Failed to read skills directory");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "failed to read skills directory"})),
                ));
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                    if seen_names.contains(name) {
                        continue;
                    }
                    let has_skill_md = entry_path.join("SKILL.md").exists();
                    let has_config = entry_path.join("config.toml").exists();
                    if (has_skill_md || has_config)
                        && crate::external_skills::may_read_instructions(&caller, &entry_path)
                    {
                        skills.push(serde_json::json!({
                            "name": name,
                            "dir_name": name,
                            "type": "external",
                            "editable": true,
                            "available": true,
                            "has_config": has_config,
                        }));
                    }
                }
            }
        }
    }

    Ok(Json(serde_json::json!({ "skills": skills })))
}

/// `GET /api/agents/{id}/skills/{name}` — Read SKILL.md content.
async fn agent_skill_get(
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    validate_skill_name(&name)?;
    let service = resolve_agent(&state, &id)?;
    let skill_dir = service.skills_dir.join(&name);
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    if !crate::external_skills::may_read_instructions(&caller, &skill_dir) {
        return Err(skill_not_found(&name));
    }
    match tokio::fs::read_to_string(skill_dir.join("SKILL.md")).await {
        Ok(content) => Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            content,
        )
            .into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(skill_not_found(&name)),
        Err(e) => {
            warn!(agent_id = %id, skill = %name, error = %e, "Failed to read SKILL.md");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read SKILL.md"})),
            ))
        }
    }
}

/// `PUT /api/agents/{id}/skills/{name}` — Write/create SKILL.md.
async fn agent_skill_put(
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    validate_skill_name(&name)?;
    let service = resolve_agent(&state, &id)?;
    let skill_dir = service.skills_dir.join(&name);

    // Ensure skill directory exists
    if let Err(e) = tokio::fs::create_dir_all(&skill_dir).await {
        warn!(agent_id = %id, skill = %name, error = %e, "Failed to create skill directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to create skill directory"})),
        ));
    }

    let skill_path = skill_dir.join("SKILL.md");
    match crate::external_skills::of(&service)
        .write(move || workforce_common::config_files::replace(&skill_path, body.as_bytes()))
        .await
    {
        Ok(()) => {
            info!(agent_id = %id, skill = %name, "Skill SKILL.md written via API");
            Ok(Json(serde_json::json!({"saved": name})))
        }
        Err(e) => {
            warn!(agent_id = %id, skill = %name, error = %e, "Failed to write SKILL.md");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to write SKILL.md"})),
            ))
        }
    }
}

/// `DELETE /api/agents/{id}/skills/{name}` — Delete a skill directory.
async fn agent_skill_delete(
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    validate_skill_name(&name)?;
    let service = resolve_agent(&state, &id)?;
    let skill_dir = service.skills_dir.join(&name);

    if !skill_dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("skill '{}' not found", name)})),
        ));
    }

    match crate::external_skills::of(&service)
        .write({
            let name = name.clone();
            move || workforce_skills::installer::remove_skill_dir(&name, &skill_dir)
        })
        .await
    {
        Ok(()) => {
            info!(agent_id = %id, skill = %name, "Skill directory deleted via API");
            Ok(Json(serde_json::json!({"deleted": name})))
        }
        Err(e) => {
            warn!(agent_id = %id, skill = %name, error = %e, "Failed to delete skill directory");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to delete skill directory"})),
            ))
        }
    }
}

/// `GET /api/agents/{id}/skills/{name}/config` — Read a skill's config.toml.
async fn agent_skill_config_get(
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid skill name"})),
        ));
    }
    let service = resolve_agent(&state, &id)?;
    let skill_dir = service.skills_dir.join(&name);
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    if !crate::external_skills::may_read_config(&caller, &skill_dir) {
        return Err(skill_not_found(&name));
    }
    match tokio::fs::read_to_string(skill_dir.join("config.toml")).await {
        Ok(content) => Ok((
            [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
            content,
        )
            .into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Return empty string — skill exists but has no config yet
            Ok((
                [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
                String::new(),
            )
                .into_response())
        }
        Err(e) => {
            warn!(agent_id = %id, skill = %name, error = %e, "Failed to read skill config.toml");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read config.toml"})),
            ))
        }
    }
}

/// `PUT /api/agents/{id}/skills/{name}/config` — Write a skill's config.toml.
async fn agent_skill_config_put(
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid skill name"})),
        ));
    }
    let service = resolve_agent(&state, &id)?;
    let skill_dir = service.skills_dir.join(&name);

    if let Err(e) = tokio::fs::create_dir_all(&skill_dir).await {
        warn!(agent_id = %id, skill = %name, error = %e, "Failed to create skill directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to create skill directory"})),
        ));
    }

    let config_path = skill_dir.join("config.toml");
    match crate::external_skills::of(&service)
        .write(move || workforce_common::config_files::replace(&config_path, body.as_bytes()))
        .await
    {
        Ok(()) => {
            info!(agent_id = %id, skill = %name, "Skill config.toml written via API");
            reload_slack_after_config_write(&state, &name).await;
            Ok(Json(
                serde_json::json!({"saved": format!("{}/config.toml", name)}),
            ))
        }
        Err(e) => {
            warn!(agent_id = %id, skill = %name, error = %e, "Failed to write skill config.toml");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to write config.toml"})),
            ))
        }
    }
}

/// A written Slack skill config takes effect on every replica at once. A
/// config Slack can't load leaves the running connections as they were.
async fn reload_slack_after_config_write(state: &GatewayState, skill: &str) {
    if skill != "slack-integration" {
        return;
    }
    let Some(slack) = state.slack.get() else {
        return;
    };
    match slack.reload(None).await {
        Ok(result) => info!(
            workspaces = result.workspaces,
            "Slack reloaded after its config was written"
        ),
        Err(e) => warn!(error = %e, "Slack config written, but reloading Slack failed"),
    }
}

fn slack_slot(
    state: &GatewayState,
) -> Result<&Arc<crate::slack_onboarding::SlackControl>, (StatusCode, Json<serde_json::Value>)> {
    state.slack.get().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Slack is not running on this hub"})),
        )
    })
}

fn slack_refusal(
    agent_id: &str,
    refusal: crate::slack_onboarding::SlackRefusal,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::slack_onboarding::SlackRefusal;
    let status = match &refusal {
        SlackRefusal::Invalid(_) => StatusCode::BAD_REQUEST,
        SlackRefusal::Conflict(_) => StatusCode::CONFLICT,
        SlackRefusal::Rejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
        SlackRefusal::Failed(e) => {
            warn!(agent_id = %agent_id, error = %e, "Slack connection change failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (
        status,
        Json(serde_json::json!({"error": refusal.to_string()})),
    )
}

/// `GET /api/agents/{id}/slack` — the agent's Slack workspaces and whether
/// each is connected.
async fn agent_slack_status(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<crate::slack_onboarding::SlackStatus>, (StatusCode, Json<serde_json::Value>)> {
    authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;
    slack_slot(&state)?
        .status(&id)
        .await
        .map(Json)
        .map_err(|refusal| slack_refusal(&id, refusal))
}

/// `PUT /api/agents/{id}/slack` — connect the agent to Slack (or rotate its
/// tokens): the same flow as the agent's `slack.connect` tool.
async fn agent_slack_connect(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<crate::slack_onboarding::ConnectRequest>,
) -> Result<Json<crate::slack_onboarding::Connection>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;
    slack_slot(&state)?
        .connect(&id, body, caller_actor(&caller))
        .await
        .map(Json)
        .map_err(|refusal| slack_refusal(&id, refusal))
}

/// `DELETE /api/agents/{id}/slack` — disconnect the agent from Slack.
async fn agent_slack_disconnect(
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<crate::slack_onboarding::Disconnection>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_capability_action(
        &state,
        &headers,
        query.token.as_deref(),
        crate::authz::Capability::VaultSharedWrite,
    )
    .await?;
    authorise_agent_read(&state, &headers, query.token.as_deref(), &id).await?;
    slack_slot(&state)?
        .disconnect(&id, None, caller_actor(&caller))
        .await
        .map(Json)
        .map_err(|refusal| slack_refusal(&id, refusal))
}

// ── Global (non-agent-scoped) skill endpoints ──────────────────────────

/// Validate a skill name to prevent path traversal.
fn validate_skill_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid skill name"})),
        ));
    }
    Ok(())
}

/// The not-found reply of every skill read, also given for a skill the caller may not read.
fn skill_not_found(name: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": format!("skill '{}' not found", name)})),
    )
}

/// Resolve a file within a skill directory, checking the skill roots in precedence order.
fn resolve_skill_file(
    skills_dir: &std::path::Path,
    skill_name: &str,
    file_name: &str,
) -> Option<std::path::PathBuf> {
    crate::external_skills::skill_roots(skills_dir)
        .into_iter()
        .map(|root| root.join(skill_name).join(file_name))
        .find(|path| path.exists())
}

/// `GET /api/skills` — List all skills (built-in + external) with metadata.
async fn global_skills_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    let skills = crate::skill_loader::global_skills_list(&state.skills_dir, &caller);
    Ok(Json(serde_json::json!({ "skills": skills })))
}

/// `GET /api/skills/catalog` — List skills available to install across the
/// configured `[[skills.catalogs]]`, annotated with local install state.
async fn skill_catalog_list(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<crate::skill_catalog::CatalogListing>, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    state.skill_catalog.list(None).await.map(Json).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })
}

#[derive(serde::Deserialize)]
struct CatalogInstallRequest {
    catalog: String,
    name: String,
}

/// `POST /api/skills/catalog/install` — Install a skill from a named catalog, or
/// update an installed one to the catalog's version; live on every agent at once.
/// Owner-scoped (writes to the skills / WASM stores), mirroring the skill CRUD
/// write endpoints.
async fn skill_catalog_install(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<CatalogInstallRequest>,
) -> Result<Json<crate::skill_catalog::InstallOutcome>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    match state
        .skill_catalog
        .install(
            &crate::skill_catalog::InstallAuthority::Operator,
            Some(&req.catalog),
            &req.name,
        )
        .await
    {
        Ok(outcome) => {
            info!(
                catalog = %req.catalog,
                skill = %req.name,
                replaced = outcome.replaced,
                active = outcome.active,
                "Skill installed from catalog"
            );
            Ok(Json(outcome))
        }
        Err(e) => {
            warn!(catalog = %req.catalog, skill = %req.name, error = %e, "Catalog install failed");
            Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            ))
        }
    }
}

/// `GET /api/vault/catalog` — Flattened credential catalog (path templates with
/// scope + managed_by + `{var}` segments) that drives the vault editor's path
/// dropdown. Returns no secret values, only the known path shapes.
async fn vault_catalog(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    let mut entries = crate::skill_loader::credential_catalog();
    // Refine per_user_allowed against the authoritative server gate so the
    // editor never offers a per-user option the /api/me/vault endpoint rejects.
    for e in &mut entries {
        let rep = crate::skill_loader::representative_path(&e.template);
        e.per_user_allowed =
            e.per_user_allowed && crate::api_handlers::my_settings::is_allowed_path(&rep);
    }
    Ok(Json(serde_json::json!({ "entries": entries })))
}

/// `GET /api/skills/{name}` — Read a skill's SKILL.md content.
async fn global_skill_get(
    Path(name): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    validate_skill_name(&name)?;
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    let skill_path = resolve_skill_file(&state.skills_dir, &name, "SKILL.md")
        .filter(|path| {
            path.parent()
                .is_some_and(|dir| crate::external_skills::may_read_instructions(&caller, dir))
        })
        .ok_or_else(|| skill_not_found(&name))?;
    match tokio::fs::read_to_string(&skill_path).await {
        Ok(content) => Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            content,
        )
            .into_response()),
        Err(e) => {
            warn!(skill = %name, error = %e, "Failed to read SKILL.md");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read SKILL.md"})),
            ))
        }
    }
}

/// `PUT /api/skills/{name}` — Write/create a skill's SKILL.md.
async fn global_skill_put(
    Path(name): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    validate_skill_name(&name)?;
    let skill_dir = state.skills_dir.join(&name);
    if let Err(e) = tokio::fs::create_dir_all(&skill_dir).await {
        warn!(skill = %name, error = %e, "Failed to create skill directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to create skill directory"})),
        ));
    }
    let skill_path = skill_dir.join("SKILL.md");
    match state
        .external_skills
        .write(move || workforce_common::config_files::replace(&skill_path, body.as_bytes()))
        .await
    {
        Ok(()) => {
            info!(skill = %name, "Skill SKILL.md written via global API");
            Ok(Json(serde_json::json!({"saved": name})))
        }
        Err(e) => {
            warn!(skill = %name, error = %e, "Failed to write SKILL.md");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to write SKILL.md"})),
            ))
        }
    }
}

/// `DELETE /api/skills/{name}` — Delete a skill directory.
async fn global_skill_delete(
    Path(name): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    validate_skill_name(&name)?;
    let skill_dir = state.skills_dir.join(&name);
    if !skill_dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("skill '{}' not found", name)})),
        ));
    }
    match state
        .external_skills
        .write({
            let name = name.clone();
            move || workforce_skills::installer::remove_skill_dir(&name, &skill_dir)
        })
        .await
    {
        Ok(()) => {
            info!(skill = %name, "Skill deleted via global API");
            Ok(Json(serde_json::json!({"deleted": name})))
        }
        Err(e) => {
            warn!(skill = %name, error = %e, "Failed to delete skill");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to delete skill"})),
            ))
        }
    }
}

/// `GET /api/skills/{name}/config` — Read a skill's config.toml.
async fn global_skill_config_get(
    Path(name): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let caller_ctx = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_AGENTS,
    )
    .await?;
    validate_skill_name(&name)?;
    let caller = crate::api_handlers::external_skills::external_caller(&state, &caller_ctx);
    let config_path = resolve_skill_file(&state.skills_dir, &name, "config.toml");
    match config_path {
        Some(path)
            if !path
                .parent()
                .is_some_and(|dir| crate::external_skills::may_read_config(&caller, dir)) =>
        {
            Err(skill_not_found(&name))
        }
        Some(path) => match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok((
                [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
                content,
            )
                .into_response()),
            Err(e) => {
                warn!(skill = %name, error = %e, "Failed to read skill config.toml");
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "failed to read config.toml"})),
                ))
            }
        },
        None => {
            let body = crate::skill_loader::builtin_config_template(&name).unwrap_or("");
            Ok((
                [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
                body.to_string(),
            )
                .into_response())
        }
    }
}

/// `PUT /api/skills/{name}/config` — Write a skill's config.toml.
async fn global_skill_config_put(
    Path(name): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    validate_skill_name(&name)?;
    let skill_dir = state.skills_dir.join(&name);
    if let Err(e) = tokio::fs::create_dir_all(&skill_dir).await {
        warn!(skill = %name, error = %e, "Failed to create skill directory");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to create skill directory"})),
        ));
    }
    let config_path = skill_dir.join("config.toml");
    match state
        .external_skills
        .write(move || workforce_common::config_files::replace(&config_path, body.as_bytes()))
        .await
    {
        Ok(()) => {
            info!(skill = %name, "Skill config.toml written via global API");
            reload_slack_after_config_write(&state, &name).await;
            Ok(Json(
                serde_json::json!({"saved": format!("{}/config.toml", name)}),
            ))
        }
        Err(e) => {
            warn!(skill = %name, error = %e, "Failed to write skill config.toml");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to write config.toml"})),
            ))
        }
    }
}

// ── Raw config file endpoints ───────────────────────────────────────────

/// `GET /api/config/raw` — Return the raw workforce.toml content as text/toml.
async fn config_raw_get(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;

    let config_path = resolve_config_path(&state)?;

    match std::fs::read_to_string(&config_path) {
        Ok(content) => Ok((
            [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
            content,
        )
            .into_response()),
        Err(e) => {
            warn!(path = %config_path.display(), error = %e, "Failed to read raw config file");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"error": format!("failed to read config file: {}", config_path.display())}),
                ),
            ))
        }
    }
}

/// `PUT /api/config/raw` — Write raw TOML body to workforce.toml (owner-only).
async fn config_raw_put(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }

    let config_path = resolve_config_path(&state)?;

    // Validate that the body is valid TOML before writing
    if let Err(e) = body.parse::<toml::Value>() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid TOML: {e}")})),
        ));
    }

    if let Err(e) = std::fs::write(&config_path, body.as_bytes()) {
        warn!(path = %config_path.display(), error = %e, "Failed to write raw config file");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to write configuration file"})),
        ));
    }

    info!("Raw config file updated via API");
    Ok(Json(serde_json::json!({
        "updated": true,
        "message": "Configuration file saved. Restart may be required for changes to take effect."
    })))
}

/// Resolve the config file path from GatewayState or the default agent service.
fn resolve_config_path(
    state: &GatewayState,
) -> Result<std::path::PathBuf, (StatusCode, Json<serde_json::Value>)> {
    // Try GatewayState first
    if let Some(ref p) = state.config_path {
        return Ok(p.clone());
    }
    // Fall back to the default service's config_path
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no default agent available"})),
        )
    })?;
    service.config_path.clone().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "no config path configured"})),
        )
    })
}

// ── Config overlay endpoints (revisions-and-config-overlay design, §12–§13) ─

/// Base path + data dir + lock manager for overlay operations.
fn resolve_overlay_ctx(
    state: &GatewayState,
) -> Result<
    (
        std::path::PathBuf,
        std::path::PathBuf,
        Arc<workforce_common::locking::LockManager>,
    ),
    (StatusCode, Json<serde_json::Value>),
> {
    let config_path = resolve_config_path(state)?;
    let service = state.default_service().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no default agent available"})),
        )
    })?;
    let data_dir = std::path::PathBuf::from(&service.config.hub.data_dir);
    Ok((config_path, data_dir, Arc::clone(&service.lock_manager)))
}

/// `GET /api/config/overlay` — the raw overlay TOML (empty when absent).
async fn config_overlay_get(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let (_, data_dir, _) = resolve_overlay_ctx(&state)?;
    let content = read_overlay_file(&data_dir)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
        content,
    )
        .into_response())
}

/// Overlay file contents; absent = empty, but real I/O failures surface as
/// 500 instead of masquerading as an empty overlay.
fn read_overlay_file(
    data_dir: &std::path::Path,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let path = data_dir.join(workforce_common::config_overlay::OVERLAY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read config overlay");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to read config overlay"})),
            ))
        }
    }
}

/// `PUT /api/config/overlay` — replace the whole overlay with the submitted
/// TOML (admin). The write is allowlist-gated and validated through the boot
/// pipeline by [`crate::config_writer::mutate_overlay`]; a non-overlayable key
/// (e.g. `[permissions]`, `[postgres]`) is a 400 naming it, so the UI can tell
/// the admin exactly which sections stay infra-owned.
async fn config_overlay_put(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::config_overlay as overlay;
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(&state)?;

    let parsed = body.parse::<toml::Table>().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid TOML: {e}")})),
        )
    })?;

    crate::config_writer::mutate_overlay(
        &config_path,
        &data_dir,
        crate::config_writer::OverlayWrite {
            actor: caller.identity_id.as_deref(),
            via: "web",
            action: "replace",
            key_path: "*",
        },
        state.def_store.as_deref(),
        &lock_manager,
        move |ov| {
            // Agent entries stay owned by the surgical agent writers (they
            // enforce the default/last-agent guard and avoid lost updates);
            // the editor replaces config keys but inherits the live agent set.
            let mut next = parsed;
            overlay::preserve_agent_keys(&mut next, ov);
            *ov = next;
            Ok(())
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    info!("Config overlay replaced via API");
    Ok(Json(serde_json::json!({
        "saved": true,
        "restart_required": true,
    })))
}

/// `GET /api/config/effective` — the merged (base ⊕ overlay) raw config plus
/// per-key provenance: which key paths the overlay sets, and which of those
/// have **converged** with the base (redundant overrides an infra deploy
/// caught up with — flagged so operators clear them instead of letting them
/// silently shadow future base edits). Values stay raw/unexpanded — secrets
/// never serialize here.
async fn config_effective_get(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::config_overlay as overlay;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let (config_path, _, _) = resolve_overlay_ctx(&state)?;
    let raw = overlay::load_raw(&config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let effective_toml =
        toml::to_string_pretty(&toml::Value::Table(raw.merged())).unwrap_or_default();
    let overlay_keys: Vec<serde_json::Value> = raw
        .overlay
        .as_ref()
        .map(|ov| {
            overlay::overlay_key_paths(ov)
                .into_iter()
                .map(|key_path| {
                    let converged = overlay::key_path_converged(&raw.base, ov, &key_path);
                    serde_json::json!({ "key_path": key_path, "converged": converged })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(serde_json::json!({
        "effective_toml": effective_toml,
        "overlay_present": raw.overlay.is_some(),
        "overlay_keys": overlay_keys,
        "stripped": raw.stripped,
        "overlay_error": raw.overlay_error,
    })))
}

#[derive(Deserialize)]
struct ConfigExportQuery {
    token: Option<String>,
    /// `full` (default) = merged base ⊕ overlay rendered as a complete
    /// workforce.toml ready to commit to the infra repo; `fragment` = the
    /// overlay alone.
    mode: Option<String>,
}

/// `GET /api/config/export?mode=full|fragment` — the promote flow: export
/// full → commit to infra → deploy → clear the overlay (in that order).
/// Raw values only; `${VAR}` / vault references stay unexpanded.
async fn config_export_get(
    Query(query): Query<ConfigExportQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::config_overlay as overlay;
    let _caller = require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let (config_path, data_dir, _) = resolve_overlay_ctx(&state)?;
    let content = match query.mode.as_deref().unwrap_or("full") {
        "fragment" => workforce_common::config_files::read(
            &data_dir.join(workforce_common::config_overlay::OVERLAY_FILE),
        )
        .ok()
        .flatten()
        .unwrap_or_default(),
        "full" => {
            let raw = overlay::load_raw(&config_path).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
            })?;
            toml::to_string_pretty(&toml::Value::Table(raw.merged())).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
            })?
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": format!("invalid mode '{other}' — full | fragment")}),
                ),
            ))
        }
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/toml; charset=utf-8")],
        content,
    )
        .into_response())
}

#[derive(Deserialize)]
struct OverlayRevertBody {
    /// e.g. `agents.maple`, `agents_remove.maple`, `llm.routing.enabled`.
    key_path: String,
}

/// `POST /api/config/overlay/revert` — remove one key path from the overlay
/// so the base value takes effect again (revert-to-base).
async fn config_overlay_revert(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OverlayRevertBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::config_overlay as overlay;
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(&state)?;

    // Pre-check for a clean 404; the mutation re-applies under the lock.
    let mut probe = crate::config_writer::load_overlay_table(&data_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    if !overlay::remove_overlay_path(&mut probe, &body.key_path) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"error": format!("overlay key '{}' not found", body.key_path)}),
            ),
        ));
    }
    crate::config_writer::mutate_overlay(
        &config_path,
        &data_dir,
        crate::config_writer::OverlayWrite {
            actor: caller.identity_id.as_deref(),
            via: "web",
            action: "revert",
            key_path: &body.key_path,
        },
        state.def_store.as_deref(),
        &lock_manager,
        |ov| {
            overlay::remove_overlay_path(ov, &body.key_path);
            Ok(())
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    info!(key_path = %body.key_path, "Config overlay key reverted to base");
    Ok(Json(serde_json::json!({
        "reverted": body.key_path,
        "restart_required": true,
    })))
}

/// `POST /api/config/overlay/clear` — empty the overlay. The final step of
/// the promote flow, run AFTER the new base (with the former overrides
/// baked in) is deployed.
async fn config_overlay_clear(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (config_path, data_dir, lock_manager) = resolve_overlay_ctx(&state)?;
    crate::config_writer::mutate_overlay(
        &config_path,
        &data_dir,
        crate::config_writer::OverlayWrite {
            actor: caller.identity_id.as_deref(),
            via: "web",
            action: "clear",
            key_path: "*",
        },
        state.def_store.as_deref(),
        &lock_manager,
        |ov| {
            ov.clear();
            Ok(())
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    info!("Config overlay cleared");
    Ok(Json(serde_json::json!({
        "cleared": true,
        "restart_required": true,
    })))
}

// ── Auth login/refresh (DW-466) ─────────────────────────────────────────────

/// Extract the client IP for rate-limiter keying. Honours
/// `X-Forwarded-For` only when the direct connection peer is in
/// `trusted` (audit F12, 2026-05-15). Without a configured allowlist
/// or a known peer, falls back to loopback so all unverified traffic
/// shares one rate-limit bucket.
fn client_ip(
    trusted: &crate::trusted_proxy::TrustedProxyConfig,
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> std::net::IpAddr {
    let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let connect_info = peer.map(axum::extract::ConnectInfo);
    crate::trusted_proxy::extract_client_ip(&connect_info, xff, trusted)
}

/// Identity key used by the per-caller action limiter. Prefers
/// `identity_id` → `device_id` → a single `admin` bucket for the
/// static-token path. Returns `None` for unauthenticated callers so
/// handlers can't accidentally rate-limit them all into one shared
/// bucket — anyone who reaches this is a programmer error.
fn caller_action_key(caller: &CallerContext) -> Option<String> {
    caller
        .identity_id
        .clone()
        .or_else(|| caller.device_id.clone())
        .or_else(|| caller.is_admin.then(|| "admin".to_string()))
}

/// Check the per-identity action limiter. `None` limiter or unauthenticated
/// caller both skip the check — unauthenticated requests must already be
/// rejected by the handler before reaching this, so this is a safety
/// fall-open rather than a bucket-collapse.
pub(crate) fn check_action_rate_limit(
    limiter: Option<&crate::rate_limit::SharedActionLimiter>,
    caller: &CallerContext,
) -> Result<(), u64> {
    match (limiter, caller_action_key(caller)) {
        (Some(lim), Some(key)) => lim.check_and_record(&key),
        _ => Ok(()),
    }
}

/// Build a 429 JSON error body with `retry_after_secs` in the payload.
/// Use this when the handler's error type carries a JSON body.
pub(crate) fn too_many_requests_err(retry_after: u64) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": "rate limited",
            "retry_after_secs": retry_after,
        })),
    )
}

/// Build a 429 `Response` with the `Retry-After` header set per RFC 7231.
/// Use this for handlers whose error type is `Response` (e.g. `chat_handler`).
pub(crate) fn rate_limited_response(retry_after: u64) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("retry-after", retry_after.to_string())],
        Json(serde_json::json!({
            "error": "rate limited",
            "retry_after_secs": retry_after,
        })),
    )
        .into_response()
}

/// Compose admin-auth + per-identity rate limit for write handlers
/// (`/api/agents` CRUD, `/api/vault/*` writes, etc.). Returns the resolved
/// caller context for downstream use. F4 (2026-05-15).
pub(crate) async fn require_admin_action(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> Result<CallerContext, (StatusCode, Json<serde_json::Value>)> {
    if !check_owner_auth(state, headers, query_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized — owner token required"})),
        ));
    }
    let caller = resolve_caller_context(state, headers, query_token).await;
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(too_many_requests_err)?;
    Ok(caller)
}

/// Check a rate limiter, returning `Err(retry_after_secs)` if the caller
/// should be rejected with 429. `None` skips the check — handy because
/// every limiter on `GatewayState` is optional (test harnesses and
/// minimal embed configurations omit them).
fn check_rate_limit<K: crate::rate_limit::RateLimitKey>(
    limiter: Option<&crate::rate_limit::SharedRateLimiter<K>>,
    key: &K,
) -> Result<(), u64> {
    match limiter {
        Some(limiter) => limiter.check(key),
        None => Ok(()),
    }
}

async fn auth_login(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    axum::Json(req): axum::Json<crate::jwt::LoginRequest>,
) -> Result<Json<crate::jwt::LoginResponse>, (StatusCode, Json<crate::jwt::ErrorBody>)> {
    let ip = client_ip(&state.trusted_proxies, &headers, Some(peer));
    let key = crate::rate_limit::CredentialKey::new(ip, &req.email);
    if let Err(retry_after) = check_rate_limit(state.rate_limiter.as_ref(), &key) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(crate::jwt::ErrorBody {
                error: format!("Too many login attempts. Try again in {retry_after} seconds."),
            }),
        ));
    }

    let jwt_config = state.jwt_config.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::jwt::ErrorBody {
                error: "Auth not configured".to_string(),
            }),
        )
    })?;
    let auth_state = Arc::new(crate::jwt::JwtAuthState {
        config: jwt_config.clone(),
        static_token: Some(state.auth_token.clone()),
        device_pairing: state.device_pairing.clone(),
        identity_store: state.identity_store.clone(),
    });
    let result = crate::jwt::login_handler(State(auth_state), axum::Json(req)).await;

    if matches!(&result, Err((status, _)) if *status == StatusCode::UNAUTHORIZED) {
        if let Some(ref limiter) = state.rate_limiter {
            limiter.record_failure(key);
        }
    }

    result
}

async fn auth_refresh(
    State(state): State<Arc<GatewayState>>,
    request: axum::extract::Request,
) -> Result<Json<crate::jwt::RefreshResponse>, (StatusCode, Json<crate::jwt::ErrorBody>)> {
    let jwt_config = state.jwt_config.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::jwt::ErrorBody {
                error: "Auth not configured".to_string(),
            }),
        )
    })?;
    let auth_state = Arc::new(crate::jwt::JwtAuthState {
        config: jwt_config.clone(),
        static_token: Some(state.auth_token.clone()),
        device_pairing: state.device_pairing.clone(),
        identity_store: state.identity_store.clone(),
    });
    crate::jwt::refresh_handler(State(auth_state), request).await
}

// ── Public auth config + OIDC SSO (DW-466 follow-up) ────────────────────────

/// Public endpoint describing which login methods this hub supports. Used by
/// the login page to decide whether to render the "Login with OIDC" button.
/// Must never return secrets or internal URLs.
async fn auth_config(
    #[cfg_attr(not(feature = "oidc"), allow(unused_variables))] State(state): State<
        Arc<GatewayState>,
    >,
) -> Json<serde_json::Value> {
    #[cfg(feature = "oidc")]
    if let Some(ref provider) = state.oidc_provider {
        return Json(serde_json::json!({
            "oidc": {
                "display_name": provider.display_name(),
                "end_session_endpoint": provider.end_session_endpoint().map(|u| u.as_str()),
                "post_logout_redirect_uri": provider.post_logout_redirect_uri(),
            }
        }));
    }
    Json(serde_json::json!({}))
}

// OIDC login initiation only redirects the user to the IdP; it verifies no
// user-supplied secret, so it is not gated by the credential rate limiter
// (which would otherwise let failed password logins block the SSO button).
#[cfg(feature = "oidc")]
async fn oidc_login(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let provider = state.oidc_provider.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "OIDC not configured"})),
        )
    })?;
    let url = provider.initiate().await;
    Ok(Json(
        serde_json::json!({ "authorization_url": url.as_str() }),
    ))
}

#[cfg(not(feature = "oidc"))]
async fn oidc_login(
    State(_state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "OIDC not compiled in"})),
    ))
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

// Not gated by the credential rate limiter: the callback is protected by the
// single-use authorization code and the CSRF `state` nonce, which `complete`
// validates locally before any token exchange. The only pre-exchange key
// available here is the client IP, which collapses to one bucket behind a
// proxy and would re-introduce the "everyone is blocked" failure.
#[cfg(feature = "oidc")]
async fn oidc_callback(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<OidcCallbackQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let redirect_to = |hash: String| -> axum::response::Response {
        axum::response::Redirect::to(&format!("/#{}", hash)).into_response()
    };

    // IdP itself reported an error (user denied, invalid request, etc.).
    if let Some(err) = query.error.as_deref() {
        warn!(
            error = %err,
            desc = %query.error_description.as_deref().unwrap_or(""),
            "OIDC callback: IdP returned error"
        );
        let hash: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("error", "oidc_idp_error")
            .append_pair("detail", err)
            .finish();
        return redirect_to(hash);
    }

    let provider = match state.oidc_provider.as_ref() {
        Some(p) => p,
        None => return redirect_to("error=oidc_not_configured".to_string()),
    };

    let code = match query.code {
        Some(c) => c,
        None => return redirect_to("error=oidc_missing_code".to_string()),
    };
    let state_param = match query.state {
        Some(s) => s,
        None => return redirect_to("error=oidc_missing_state".to_string()),
    };

    let login_result = match provider.complete(&code, &state_param).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "OIDC callback: complete() failed");
            return redirect_to("error=oidc_token_exchange_failed".to_string());
        }
    };

    let identity_store = match state.identity_store.as_ref() {
        Some(s) => s,
        None => return redirect_to("error=identity_store_unavailable".to_string()),
    };

    let resolved = match provider.resolve_identity(identity_store.as_ref(), &login_result.claims) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "OIDC callback: identity resolution failed");
            return redirect_to("error=oidc_identity_rejected".to_string());
        }
    };
    let identity = resolved.into_identity();

    let jwt_config = match state.jwt_config.as_ref() {
        Some(c) => c,
        None => return redirect_to("error=auth_not_configured".to_string()),
    };

    let workforce_token = match crate::jwt::generate_token_with_identity(
        jwt_config,
        &identity.display_name,
        identity.role,
        Some(identity.id.to_string()),
    ) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "OIDC callback: failed to mint workforce JWT");
            return redirect_to("error=jwt_mint_failed".to_string());
        }
    };

    let hash: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", &workforce_token)
        .append_pair("identity_id", &identity.id.to_string())
        .append_pair("display_name", &identity.display_name)
        .append_pair("role", identity.role.as_str())
        .append_pair("id_token", &login_result.raw_id_token)
        .finish();
    redirect_to(hash)
}

#[cfg(not(feature = "oidc"))]
async fn oidc_callback(
    State(_state): State<Arc<GatewayState>>,
    Query(_query): Query<OidcCallbackQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::response::Redirect::to("/#error=oidc_not_compiled_in").into_response()
}

// ── Owner-scoped scheduler view ─────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ScheduleQuery {
    token: Option<String>,
}

/// A job is visible to a caller when they're an admin, they own it, or it's a
/// non-reminder global job. Reminders are always private to their owner.
fn schedule_visible(job: &ScheduledJob, caller: &CallerContext) -> bool {
    caller.is_admin
        || job.owner().is_some_and(|o| caller.owns(o))
        || (!job.is_reminder && job.effective_visibility() == VISIBILITY_GLOBAL)
}

/// Mutations (edit/delete/run/visibility) require admin or ownership — a global
/// job is visible to everyone but writable only by its owner.
fn authorize_schedule_mutation(
    job: &ScheduledJob,
    caller: &CallerContext,
) -> Result<(), StatusCode> {
    if caller.is_admin || job.owner().is_some_and(|o| caller.owns(o)) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// The owner namespace a write addresses: admins write system/global jobs
/// (owner `None`); everyone else owns under their identity or paired device.
fn caller_owner_key(caller: &CallerContext) -> Option<String> {
    if caller.is_admin {
        None
    } else {
        caller
            .identity_id
            .clone()
            .or_else(|| caller.device_id.clone())
    }
}

/// Validate and default a requested visibility. Owned jobs default to private,
/// ownerless (system/admin) jobs to global; reminders can never be global.
fn resolve_schedule_visibility(
    requested: Option<&str>,
    owner: Option<&str>,
    is_reminder: bool,
) -> Result<&'static str, StatusCode> {
    let visibility = match requested {
        None if owner.is_none() => VISIBILITY_GLOBAL,
        None => VISIBILITY_PRIVATE,
        Some(VISIBILITY_PRIVATE) => VISIBILITY_PRIVATE,
        Some(VISIBILITY_GLOBAL) => VISIBILITY_GLOBAL,
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    if is_reminder && visibility == VISIBILITY_GLOBAL {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(visibility)
}

/// Locate the scheduler holding `id` along with a clone of the job.
async fn find_scheduled_job(
    state: &GatewayState,
    id: &str,
) -> Option<(Arc<dyn crate::hosts::SchedulerHost>, ScheduledJob)> {
    for svc in state.registry.all_services() {
        if let Some(scheduler) = svc.scheduler.clone() {
            if let Some(job) = scheduler.get_job(id).await {
                return Some((scheduler, job));
            }
        }
    }
    None
}

/// Serialize a job with its owner's display name resolved for the UI, optionally
/// tagging the owning agent.
fn schedule_json(
    job: &ScheduledJob,
    agent_id: Option<&str>,
    principals: &HashMap<String, PrincipalRef>,
) -> serde_json::Value {
    let mut value = crate::api::serialize_job(job);
    if let Some(agent_id) = agent_id {
        value["agent_id"] = serde_json::json!(agent_id);
    }
    match job.owner() {
        Some(owner) => {
            let principal = principals.get(owner);
            value["owner_display_name"] = serde_json::json!(principal
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| owner.to_string()));
            value["owner_kind"] =
                serde_json::json!(principal.map(|p| p.kind).unwrap_or(PrincipalKind::User));
        }
        None => {
            value["owner_display_name"] = serde_json::Value::Null;
            value["owner_kind"] = serde_json::Value::Null;
        }
    }
    value
}

/// `GET /api/schedules` — list the jobs visible to the caller across every
/// agent scheduler: admins see all; everyone else sees their own plus public
/// (non-reminder global) jobs.
async fn schedules_list(
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut visible: Vec<(ScheduledJob, String, Option<f64>)> = Vec::new();
    for svc in state.registry.all_services() {
        if let Some(ref scheduler) = svc.scheduler {
            let agent_id = svc.agent_id.to_string();
            for job in scheduler.list_jobs().await {
                if schedule_visible(&job, &caller) {
                    // A global job is listed for everyone; what it costs is
                    // its owner's and an admin's to see.
                    let spent = authorize_schedule_mutation(&job, &caller)
                        .ok()
                        .and_then(|()| scheduler.job_spend(&job.id));
                    visible.push((job, agent_id.clone(), spent));
                }
            }
        }
    }
    let principals = resolve_principals(
        state.identity_store.as_ref(),
        visible.iter().filter_map(|(job, ..)| job.owner()),
    );
    let jobs: Vec<_> = visible
        .iter()
        .map(|(job, agent_id, spent)| {
            let mut value = schedule_json(job, Some(agent_id), &principals);
            value["spent_usd"] = serde_json::json!(spent);
            value
        })
        .collect();
    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

#[derive(serde::Deserialize)]
struct ScheduleCreateBody {
    name: String,
    #[serde(default)]
    description: Option<String>,
    message: String,
    schedule: Schedule,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    /// When the job's instruction tells the agent to post its result itself,
    /// set this so the scheduler doesn't also deliver the agent's reply
    /// (avoids a duplicate post + a chatty "Done" confirmation).
    #[serde(default)]
    agent_owns_delivery: bool,
}

/// `POST /api/schedules` — create a job owned by the caller (admins create
/// system/global jobs). Private by default for members.
async fn schedule_create(
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ScheduleCreateBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let owner = caller_owner_key(&caller);
    let visibility =
        resolve_schedule_visibility(body.visibility.as_deref(), owner.as_deref(), false)?;
    let scheduler = state
        .default_service()
        .and_then(|svc| svc.scheduler.clone())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let payload = JobPayload::AgentTurn {
        message: body.message,
        channel: None,
        channel_id: body.channel_id,
        thread_id: body.thread_id,
        model: body.model,
        user_id: None,
        max_rounds: None,
        agent_owns_delivery: body.agent_owns_delivery,
        tools: None,
    };
    let id = match scheduler
        .add_job_owned(
            &body.name,
            body.description.as_deref(),
            body.schedule,
            payload,
            owner.clone(),
            Some(visibility.to_string()),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            ));
        }
    };
    let principals = resolve_principals(state.identity_store.as_ref(), owner.as_deref());
    let job = scheduler
        .get_job(&id)
        .await
        .map(|job| schedule_json(&job, None, &principals));
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "created": true, "id": id, "job": job })),
    ))
}

/// `GET /api/schedules/{id}` — fetch a single job the caller may see.
async fn schedule_get(
    Path(id): Path<String>,
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (_, job) = find_scheduled_job(&state, &id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    if !schedule_visible(&job, &caller) {
        return Err(StatusCode::FORBIDDEN);
    }
    let principals = resolve_principals(state.identity_store.as_ref(), job.owner());
    Ok(Json(schedule_json(&job, None, &principals)))
}

/// `PUT /api/schedules/{id}` — edit fields, visibility, session mode and/or
/// tool list on a job the caller owns (or any job, for admins).
async fn schedule_update(
    Path(id): Path<String>,
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(edit): Json<crate::hub_core::schedules::JobEdit>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (scheduler, job) = find_scheduled_job(&state, &id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_schedule_mutation(&job, &caller)?;
    let editor = caller
        .identity_id
        .as_deref()
        .or(caller.device_id.as_deref())
        .unwrap_or(crate::hub_core::schedules::OWNER_TOKEN_EDITOR);
    let job = match scheduler.update_job(&id, edit, editor).await {
        Ok(job) => job.ok_or(StatusCode::NOT_FOUND)?,
        Err(refused) => {
            tracing::warn!(job_id = %id, error = %refused, "Schedule update not applied");
            return Ok(crate::api::edit_refused(refused));
        }
    };
    let principals = resolve_principals(state.identity_store.as_ref(), job.owner());
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "updated": true, "job": schedule_json(&job, None, &principals) })),
    ))
}

/// `DELETE /api/schedules/{id}` — remove a job the caller owns (or any, admin).
async fn schedule_delete(
    Path(id): Path<String>,
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (scheduler, job) = find_scheduled_job(&state, &id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_schedule_mutation(&job, &caller)?;
    scheduler.remove_job(&id).await;
    Ok(Json(serde_json::json!({ "deleted": true, "id": id })))
}

/// `POST /api/schedules/{id}/run` — run a job now. Owner/admin only.
async fn schedule_run(
    Path(id): Path<String>,
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (scheduler, job) = find_scheduled_job(&state, &id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_schedule_mutation(&job, &caller)?;
    let record = scheduler
        .run_job_now(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({
        "triggered": true,
        "run": crate::api::serialize_run_record(&record),
    })))
}

/// `GET /api/schedules/{id}/history` — run history for a job the caller may see.
async fn schedule_history(
    Path(id): Path<String>,
    Query(query): Query<ScheduleQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (scheduler, job) = find_scheduled_job(&state, &id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    if !schedule_visible(&job, &caller) {
        return Err(StatusCode::FORBIDDEN);
    }
    let history: Vec<_> = scheduler
        .job_run_history(&id)
        .await
        .iter()
        .map(crate::api::serialize_run_record)
        .collect();
    Ok(Json(serde_json::json!({ "history": history })))
}

#[cfg(test)]
mod schedule_auth_tests {
    use super::*;
    use chrono::Utc;

    fn member(id: &str) -> CallerContext {
        CallerContext {
            identity_id: Some(id.to_string()),
            ..Default::default()
        }
    }

    fn admin() -> CallerContext {
        CallerContext {
            is_admin: true,
            ..Default::default()
        }
    }

    fn job(creator: Option<&str>, visibility: Option<&str>, payload: JobPayload) -> ScheduledJob {
        let is_reminder = matches!(payload, JobPayload::Reminder { .. });
        ScheduledJob {
            id: "j".to_string(),
            name: "j".to_string(),
            description: None,
            schedule: Schedule::Cron {
                expr: "0 9 * * *".to_string(),
                tz: None,
            },
            payload,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
            next_run: None,
            run_count: 0,
            is_reminder,
            delivery_failures: 0,
            creator_identity_id: creator.map(str::to_string),
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility: visibility.map(str::to_string),
            max_runs: None,
            budget_usd: None,
            expires_at: None,
            deliver_to: None,
            edited_by: None,
            paused_by_suspension: false,
        }
    }

    fn turn() -> JobPayload {
        JobPayload::AgentTurn {
            message: "m".to_string(),
            channel: None,
            channel_id: None,
            thread_id: None,
            model: None,
            user_id: None,
            max_rounds: None,
            agent_owns_delivery: false,
            tools: None,
        }
    }

    fn reminder(user: &str) -> JobPayload {
        JobPayload::Reminder {
            text: "r".to_string(),
            user_id: user.to_string(),
        }
    }

    #[test]
    fn admin_sees_and_mutates_everything() {
        let private = job(Some("alice"), None, turn());
        assert!(schedule_visible(&private, &admin()));
        assert!(authorize_schedule_mutation(&private, &admin()).is_ok());
    }

    #[test]
    fn member_sees_and_mutates_own_private() {
        let mine = job(Some("alice"), None, turn());
        assert!(schedule_visible(&mine, &member("alice")));
        assert!(authorize_schedule_mutation(&mine, &member("alice")).is_ok());
    }

    #[test]
    fn member_cannot_see_or_mutate_others_private() {
        let theirs = job(Some("bob"), None, turn());
        assert!(!schedule_visible(&theirs, &member("alice")));
        assert_eq!(
            authorize_schedule_mutation(&theirs, &member("alice")),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn member_sees_but_cannot_mutate_public() {
        let public = job(Some("bob"), Some(VISIBILITY_GLOBAL), turn());
        assert!(schedule_visible(&public, &member("alice")));
        assert_eq!(
            authorize_schedule_mutation(&public, &member("alice")),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn ownerless_job_is_visible_but_member_cannot_mutate() {
        let system = job(None, None, JobPayload::MonitorSweep);
        assert!(schedule_visible(&system, &member("alice")));
        assert_eq!(
            authorize_schedule_mutation(&system, &member("alice")),
            Err(StatusCode::FORBIDDEN)
        );
        assert!(authorize_schedule_mutation(&system, &admin()).is_ok());
    }

    #[test]
    fn reminders_stay_private_even_if_marked_global() {
        let others = job(Some("bob"), Some(VISIBILITY_GLOBAL), reminder("bob"));
        assert!(!schedule_visible(&others, &member("alice")));
        let mine = job(Some("alice"), None, reminder("alice"));
        assert!(schedule_visible(&mine, &member("alice")));
    }

    #[test]
    fn visibility_defaults_by_owner() {
        assert_eq!(
            resolve_schedule_visibility(None, Some("alice"), false),
            Ok(VISIBILITY_PRIVATE)
        );
        assert_eq!(
            resolve_schedule_visibility(None, None, false),
            Ok(VISIBILITY_GLOBAL)
        );
    }

    #[test]
    fn visibility_rejects_global_reminder_and_unknown_values() {
        assert_eq!(
            resolve_schedule_visibility(Some(VISIBILITY_GLOBAL), Some("alice"), true),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            resolve_schedule_visibility(Some("team"), Some("alice"), false),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn admin_owner_key_is_system_member_key_is_identity() {
        assert_eq!(caller_owner_key(&admin()), None);
        assert_eq!(
            caller_owner_key(&member("alice")),
            Some("alice".to_string())
        );
    }

    #[test]
    fn device_only_caller_owns_its_jobs() {
        let device = CallerContext {
            device_id: Some("dev-1".to_string()),
            ..Default::default()
        };
        assert_eq!(caller_owner_key(&device), Some("dev-1".to_string()));

        let mine = job(Some("dev-1"), None, turn());
        assert!(schedule_visible(&mine, &device));
        assert!(authorize_schedule_mutation(&mine, &device).is_ok());

        let theirs = job(Some("dev-2"), None, turn());
        assert!(!schedule_visible(&theirs, &device));
        assert_eq!(
            authorize_schedule_mutation(&theirs, &device),
            Err(StatusCode::FORBIDDEN)
        );
    }
}

#[cfg(test)]
mod chat_progress_tests {
    use super::*;

    /// The `stream_discard` broadcasts a local run's discard for `reason` makes.
    async fn local_discards(
        reason: workforce_agent::runner::StreamDiscardReason,
    ) -> Vec<BroadcastMessage> {
        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(8);
        let (tx, mut rx) = broadcast::channel(16);
        progress_tx
            .send(ProgressEvent::StreamDiscard { reason })
            .await
            .unwrap();
        drop(progress_tx);
        forward_local_progress(progress_rx, tx, "s-1".into(), false, None, None).await;
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|message| message.msg_type == "stream_discard")
            .collect()
    }

    /// A local run absorbing a follow-up sends no frame of its own: the
    /// gateway said `injected` once when it steered the follow-up in.
    #[tokio::test]
    async fn a_local_run_absorbing_a_follow_up_sends_no_second_injected() {
        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(8);
        let (tx, mut rx) = broadcast::channel(16);
        progress_tx
            .send(ProgressEvent::InjectedMessage { count: 1 })
            .await
            .unwrap();
        drop(progress_tx);
        forward_local_progress(progress_rx, tx, "s-1".into(), false, None, None).await;
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).all(|m| m.msg_type != "injected"));
    }

    /// A file a live Desk turn delivered is the `image` frame that shows it,
    /// named by the turn, from a local turn and over NATS alike.
    #[tokio::test]
    async fn a_delivered_file_is_an_image_frame_of_its_turn() {
        let file = workforce_common::channels::desk::DeskFile {
            artifact_id: "art_1".into(),
            agent_id: "scout".into(),
            filename: "report.pdf".into(),
            media_type: "application/pdf".into(),
            size: 9,
            width: None,
            height: None,
            caption: Some("the report".into()),
        };
        let stamp = TurnStamp {
            turn_ref: Some("ref-1".into()),
            reply_id: "reply-1".into(),
        };
        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(8);
        let (tx, mut rx) = broadcast::channel(16);
        progress_tx
            .send(ProgressEvent::File(file.clone()))
            .await
            .unwrap();
        drop(progress_tx);
        forward_local_progress(progress_rx, tx, "s-1".into(), false, None, Some(stamp)).await;
        let local = rx.try_recv().unwrap();
        let nats = chat_progress_to_broadcast(ChatProgress::File(file), "s-1", false).unwrap();
        for frame in [&local, &nats] {
            assert_eq!(
                (frame.msg_type.as_str(), frame.content.as_str()),
                ("image", "the report")
            );
            let sent = frame.attachments.as_ref().unwrap();
            assert_eq!(
                (
                    sent[0].artifact_id.as_deref(),
                    sent[0].size,
                    sent[0].data.as_str()
                ),
                (Some("art_1"), Some(9), "")
            );
        }
        assert_eq!(
            (local.turn_ref.as_deref(), local.id.as_deref()),
            (Some("ref-1"), Some("reply-1"))
        );
    }

    #[tokio::test]
    async fn an_interrupted_discard_broadcasts_stream_discard() {
        let discards =
            local_discards(workforce_agent::runner::StreamDiscardReason::Interrupted).await;
        assert_eq!(discards.len(), 1);
        assert_eq!(discards[0].content, "interrupted");
        assert!(!discards[0].speakable);
    }

    #[tokio::test]
    async fn a_superseded_discard_broadcasts_nothing() {
        assert!(
            local_discards(workforce_agent::runner::StreamDiscardReason::Superseded)
                .await
                .is_empty()
        );
    }

    #[test]
    fn a_nats_discard_broadcasts_stream_discard() {
        let message = chat_progress_to_broadcast(
            ChatProgress::StreamDiscard {
                reason: workforce_agent::runner::StreamDiscardReason::Retry,
            },
            "s-1",
            true,
        )
        .unwrap();
        assert_eq!(message.msg_type, "stream_discard");
        assert_eq!(message.content, "retry");
        assert!(!message.speakable);
    }

    #[test]
    fn route_broadcast_follows_answer_and_is_silent() {
        let citation = workforce_core::code_citation::CodeCitation {
            repo: "workforce".into(),
            owner: String::new(),
            repo_name: String::new(),
            commit: "abc123".into(),
            path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 2,
            kind: "fn".into(),
            symbol: "main".into(),
        };
        let line = "Claude Code · sonnet · Alice's plan".to_string();
        let delivered: Vec<BroadcastMessage> = std::iter::once(ChatProgress::StreamEnd)
            .chain(crate::chat_worker::after_answer(
                vec![citation],
                Some(line.clone()),
            ))
            .filter_map(|progress| chat_progress_to_broadcast(progress, "s-1", true))
            .collect();
        let types: Vec<&str> = delivered.iter().map(|m| m.msg_type.as_str()).collect();
        assert_eq!(types, ["stream_end", "code_citations", "route"]);
        let route = &delivered[2];
        assert!(!route.speakable, "the route line is never read aloud");
        assert_eq!(
            route.content, "",
            "a client that knows no route shows nothing"
        );
        assert_eq!(route.route.as_deref(), Some(line.as_str()));
        assert_eq!(route.session_id, "s-1");
    }
}

#[cfg(test)]
mod vault_admin_tests {
    use super::*;
    use workforce_common::settings::VaultActor;

    #[test]
    fn per_user_writes_of_shared_only_paths_are_refused_at_the_admin_surface() {
        assert!(per_user_shared_only(
            "users/alice/skills/jira/default_workspace"
        ));
        assert!(per_user_shared_only(
            "users/alice/skills/google/oauth_client/client_id"
        ));
        assert!(!per_user_shared_only("users/alice/skills/jira/site"));
        assert!(!per_user_shared_only("skills/jira/default_workspace"));
        assert!(!per_user_shared_only("users/"));
    }

    #[test]
    fn writes_are_recorded_as_the_caller() {
        let caller = CallerContext {
            identity_id: Some("kings".to_string()),
            ..Default::default()
        };
        assert_eq!(caller_actor(&caller).audit_string(), "user:kings");
        assert!(matches!(
            caller_actor(&CallerContext::default()),
            VaultActor::RestOwner
        ));
    }

    #[test]
    fn a_refused_config_write_is_said_in_plain_words_with_the_details_in_findings() {
        use crate::agent_config_write::Refused;
        use crate::workspace_admission::ConfigRefusal;
        let said = |refused| {
            let (status, Json(body)) = config_write_refused("scout", refused);
            (status, body)
        };
        let jargon = ["config", "toml", "revision", "[", "_", "refused"];
        let plain = |body: &serde_json::Value| {
            let error = body["error"].as_str().unwrap().to_string();
            assert!(
                !jargon.iter().any(|j| error.to_lowercase().contains(j)),
                "{error}"
            );
            error
        };

        let (status, body) = said(Refused::Stale {
            current_hash: "abc".into(),
        });
        assert_eq!(status, StatusCode::CONFLICT);
        plain(&body);
        assert_eq!(body["current_hash"], "\"abc\"");

        let (status, body) = said(Refused::Revision("pool timed out".into()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(plain(&body), "Saving the change failed; try again.");
        let (status, body) = said(Refused::Io("disk full".into()));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(plain(&body), "Saving the change failed; try again.");

        let (status, body) = said(Refused::Unloadable(ConfigRefusal::Said(
            "How much the overseer does on its own must be watch, suggest or act.".into(),
        )));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            plain(&body),
            "That change couldn’t be saved. How much the overseer does on its own must be \
             watch, suggest or act."
        );
        assert!(body.get("findings").is_none(), "{body}");

        let finding = crate::subscription::Finding::refused(
            "codex:gpt-6.1-sol",
            "\"codex:gpt-6.1-sol\" routes to codex: set [coding_session.executor]".into(),
        );
        let (status, body) = said(Refused::Unloadable(ConfigRefusal::Refused(vec![
            finding.clone()
        ])));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(plain(&body).starts_with("That change couldn’t be saved: "));
        assert_eq!(body["findings"], serde_json::json!([finding]));

        let (status, body) = said(Refused::Unloadable(ConfigRefusal::Parse(
            "invalid config.toml: TOML parse error at line 1".into(),
        )));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(plain(&body).starts_with("That change couldn’t be saved: "));
        assert!(body["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("line 1"));
    }

    #[test]
    fn a_frozen_vault_answers_503_and_other_failures_500() {
        let frozen = eyre::Report::from(crate::vault::VaultError::Frozen {
            scope: "global".into(),
        })
        .wrap_err("storing secret");
        let (status, Json(body)) = vault_write_rejection(&frozen);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("read-only"));

        let (status, Json(body)) = vault_write_rejection(&eyre::eyre!("disk full"));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["error"].as_str().unwrap().contains("disk full"));
    }
}

#[cfg(test)]
mod session_visibility_tests {
    //! Ownership-aware session listing tests.
    //!  * `CallerContext` gates which sessions a paired-device iOS user
    //!    can see (no Slack-bot sessions, no other users' chats).
    //!  * `is_conversational_channel` filters out workflow / slack-routing
    //!    channels so they never reach the mobile session browser.
    use super::*;
    use crate::serve::Compose as _;

    fn caller(identity_id: Option<&str>, device_id: Option<&str>) -> CallerContext {
        CallerContext {
            identity_id: identity_id.map(str::to_owned),
            device_id: device_id.map(str::to_owned),
            is_admin: false,
            scopes: vec![crate::jwt::SCOPE_WILDCARD.to_string()],
            auth_failure: None,
        }
    }

    #[test]
    fn caller_owns_session_only_when_identity_or_device_matches() {
        let c = caller(Some("user-1"), None);
        assert!(c.owns("user-1"));
        assert!(!c.owns("user-2"));
        assert!(!c.owns("U0FAKEUSER1")); // Slack bot user id
    }

    #[test]
    fn admin_can_see_any_owner_but_does_not_own() {
        let admin = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(admin.can_see_owner("user-1"));
        assert!(admin.can_see_owner("U0FAKEUSER1"));
        // Admin may view but cannot act-as.
        assert!(!admin.owns("user-1"));
    }

    #[test]
    fn jwt_admin_flag_tracks_system_admin_only() {
        use workforce_identity::types::IdentityRole;
        assert!(IdentityRole::SystemAdmin.can_manage_system());
        assert!(!IdentityRole::Manager.can_manage_system());
        assert!(!IdentityRole::Member.can_manage_system());
    }

    /// The definition rules see the caller as the *live* identity store does,
    /// not as their token claims: that flag now reaches past another member's
    /// row, so a suspended administrator must not still carry it.
    #[test]
    fn def_writer_reads_administrator_standing_from_the_live_identity() {
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

        let store = Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
        let made = |name: &str, role, status| {
            let mut who = Identity::new(name, role);
            who.status = status;
            store.create(&who).unwrap();
            who.id.to_string()
        };
        let alice = made("Alice", IdentityRole::Member, IdentityStatus::Active);
        let root = made("Root", IdentityRole::SystemAdmin, IdentityStatus::Active);
        let gone = made("Gone", IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        let identities: Arc<dyn IdentityStore> = store;
        let writer = |identity: Option<&str>, is_admin| {
            def_writer(
                Some(&identities),
                &CallerContext {
                    identity_id: identity.map(str::to_owned),
                    is_admin,
                    ..Default::default()
                },
            )
        };

        // The JWT claim is SystemAdmin in all three member cases; only the
        // live role and status decide.
        assert!(!writer(Some(&alice), true).is_admin);
        assert!(writer(Some(&root), true).is_admin);
        assert!(!writer(Some(&gone), true).is_admin);
        // The static gateway token is the operator, with nobody to attribute to.
        assert_eq!(
            writer(None, true),
            DefWriter {
                identity_id: None,
                is_admin: true
            }
        );
        // A device token linked to an administrator is never one over REST.
        assert!(
            !def_writer(
                Some(&identities),
                &CallerContext {
                    identity_id: Some(root.clone()),
                    device_id: Some("device-1".into()),
                    is_admin: true,
                    ..Default::default()
                },
            )
            .is_admin
        );
        assert_eq!(
            writer(Some(&alice), false),
            DefWriter {
                identity_id: Some(alice),
                is_admin: false
            }
        );
    }

    #[test]
    fn anonymous_caller_cannot_see_anything() {
        let anon = CallerContext::default();
        assert!(!anon.can_see_owner("user-1"));
        assert!(!anon.owns("user-1"));
    }

    #[test]
    fn is_authenticated_only_when_principal_present() {
        assert!(!CallerContext::default().is_authenticated());
        assert!(caller(Some("user-1"), None).is_authenticated());
        assert!(caller(None, Some("device-abc")).is_authenticated());
        assert!(CallerContext {
            is_admin: true,
            ..Default::default()
        }
        .is_authenticated());
    }

    #[test]
    fn device_matched_caller_sees_device_owned_session() {
        // A paired device with no linked identity owns sessions by device id.
        let c = caller(None, Some("device-abc"));
        assert!(c.owns("device-abc"));
        assert!(!c.owns("device-xyz"));
    }

    #[test]
    fn conversational_channel_classification() {
        assert!(is_conversational_channel("ios"));
        assert!(is_conversational_channel("web"));
        assert!(is_conversational_channel("terminal"));
        assert!(is_conversational_channel("desk"));
        assert!(is_conversational_channel("desk:release"));
        assert!(!is_conversational_channel("slack:DM:U0FAKEUSER1"));
        assert!(!is_conversational_channel("slack:allan:D0FAKEDMAA1"));
        assert!(!is_conversational_channel("workflow:abc:design"));
        assert!(!is_conversational_channel("rest"));
        assert!(!is_conversational_channel("gateway"));
        assert!(!is_conversational_channel("clawdy"));
        assert!(!is_conversational_channel("ws"));
    }

    // ── F1: agent-read access predicate ───────────────────────────────
    //
    // Pin the rule shared by `/api/agents/{id}`, `/tools`, `/stats`:
    // non-owners get filtered out (the handler converts to 404 so they
    // cannot enumerate IDs by status code — same shape as session IDOR).
    use workforce_common::settings::OwnerType;

    #[test]
    fn admin_sees_any_agent() {
        let admin = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(caller_can_see_agent_owner(
            &admin,
            Some("alice"),
            &OwnerType::User,
            &[]
        ));
        // Even unowned agents are visible to admin.
        assert!(caller_can_see_agent_owner(
            &admin,
            None,
            &OwnerType::User,
            &[]
        ));
    }

    #[test]
    fn user_owner_sees_their_own_agent() {
        let c = caller(Some("alice"), None);
        assert!(caller_can_see_agent_owner(
            &c,
            Some("alice"),
            &OwnerType::User,
            &[]
        ));
    }

    #[test]
    fn user_cannot_see_other_users_agent() {
        let c = caller(Some("alice"), None);
        assert!(!caller_can_see_agent_owner(
            &c,
            Some("bob"),
            &OwnerType::User,
            &[]
        ));
    }

    #[test]
    fn unowned_agent_hidden_from_non_admin() {
        let c = caller(Some("alice"), None);
        assert!(!caller_can_see_agent_owner(&c, None, &OwnerType::User, &[]));
    }

    #[test]
    fn anonymous_caller_sees_no_agents_f1_regression() {
        // audit F1 regression: an anonymous caller (no token) must resolve to
        // is_admin=false in `list_agents` and therefore see NO agents — not the
        // full roster. Before the fix, `is_admin = caller_identity_id.is_none()
        // || …` collapsed a tokenless caller to admin. `list_agents` now derives
        // the caller via `resolve_caller_context`, which yields is_admin=false
        // and no identity/device for an anonymous caller; the access-path guard
        // is pinned separately by `anonymous_never_sees_open_agent_via_access_path_f1`.
        let anon = CallerContext::default();
        assert!(!anon.is_admin, "anonymous caller must never be admin");
        // Unowned agents (the prod shape) are invisible to anonymous …
        assert!(!caller_can_see_agent_owner(
            &anon,
            None,
            &OwnerType::User,
            &[]
        ));
        // … and so are owned agents (no owner-UUID enumeration either).
        assert!(!caller_can_see_agent_owner(
            &anon,
            Some("some-owner-uuid"),
            &OwnerType::User,
            &[]
        ));
    }

    #[test]
    fn team_member_sees_team_owned_agent() {
        let c = caller(Some("alice"), None);
        let teams = vec!["team-engineering".to_string()];
        assert!(caller_can_see_agent_owner(
            &c,
            Some("team-engineering"),
            &OwnerType::Team,
            &teams
        ));
    }

    #[test]
    fn non_team_member_cannot_see_team_owned_agent() {
        let c = caller(Some("alice"), None);
        let teams = vec!["team-marketing".to_string()];
        assert!(!caller_can_see_agent_owner(
            &c,
            Some("team-engineering"),
            &OwnerType::Team,
            &teams
        ));
    }

    #[test]
    fn paired_device_sees_device_owned_agent() {
        // Devices without a linked identity own agents by device id.
        let c = caller(None, Some("device-abc"));
        assert!(caller_can_see_agent_owner(
            &c,
            Some("device-abc"),
            &OwnerType::User,
            &[]
        ));
        assert!(!caller_can_see_agent_owner(
            &c,
            Some("device-xyz"),
            &OwnerType::User,
            &[]
        ));
    }

    // ── roster_visibility: ownership OR (authenticated && access policy) ──

    #[test]
    fn anonymous_never_sees_open_agent_via_access_path_f1() {
        // F1 regression on the access path: an Open agent's gate returns
        // Allow to ANYONE (`access_allows = true`), so the `is_authenticated()`
        // guard is the only thing stopping an anonymous caller from seeing the
        // full Open roster. Pin it.
        let anon = CallerContext::default();
        let vis = roster_visibility(&anon, None, &OwnerType::User, &[], true);
        assert!(!vis.visible, "anonymous must not see an Open agent");
        assert!(!vis.disclose_owner);
    }

    #[test]
    fn authenticated_device_sees_open_agent_with_owner_scrubbed() {
        // The iOS case: a paired device (authenticated, non-owner) sees an
        // owner-less Open agent, but its owner metadata is not disclosed.
        let c = caller(None, Some("device-abc"));
        let vis = roster_visibility(&c, None, &OwnerType::User, &[], true);
        assert!(vis.visible, "authenticated device must see an Open agent");
        assert!(
            !vis.disclose_owner,
            "owner metadata must be scrubbed on the access-only path"
        );
    }

    #[test]
    fn authenticated_caller_hidden_when_access_denied() {
        // Allowlist non-member / Blocklist member → gate denies → hidden even
        // though the caller is authenticated.
        let c = caller(Some("alice"), None);
        let vis = roster_visibility(&c, None, &OwnerType::User, &[], false);
        assert!(!vis.visible);
    }

    #[test]
    fn owner_sees_agent_and_owner_is_disclosed() {
        // Ownership path: visible regardless of access verdict, and owner
        // metadata is disclosed to the owner.
        let c = caller(Some("alice"), None);
        let vis = roster_visibility(&c, Some("alice"), &OwnerType::User, &[], false);
        assert!(vis.visible);
        assert!(vis.disclose_owner);
    }

    #[test]
    fn admin_sees_unowned_agent_and_owner_is_disclosed() {
        let admin = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        let vis = roster_visibility(&admin, None, &OwnerType::User, &[], false);
        assert!(vis.visible);
        assert!(vis.disclose_owner, "admin is on the ownership path");
    }

    #[test]
    fn a_team_member_sees_a_locked_team_agent() {
        let alice = caller(Some("alice"), None);
        let teams = vec!["team-finance".to_string()];
        let vis = roster_visibility(
            &alice,
            Some("team-finance"),
            &OwnerType::Team,
            &teams,
            false,
        );
        assert!(vis.visible);
    }

    // ── AgentViewer: the roster rule against a live gateway state ──

    use workforce_common::settings::{AccessDecision, AccessMode, AgentAccessConfig};

    fn gateway() -> Arc<GatewayState> {
        crate::remote_mcp::testing::fixture(false).state
    }

    #[test]
    fn a_rest_turn_is_filed_under_its_askers_own_name_for_it() {
        assert_eq!(
            chat_task_id("alice", "run-1.q_7").as_deref(),
            Ok("chat:alice:run-1.q_7")
        );
        for turn in ["", "a b", "scheduler:job-1", "a/b", &"x".repeat(65)] {
            assert!(
                chat_task_id("alice", turn).is_err(),
                "{turn:?} was accepted"
            );
        }
    }

    #[test]
    fn a_rest_message_is_trusted_as_its_senders_live_role_maps_and_no_further() {
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};
        let state = gateway();
        let store = state.identity_store.as_ref().unwrap();
        let person = |role, status| {
            let identity = Identity {
                status,
                ..Identity::new("Someone", role)
            };
            store.create(&identity).unwrap();
            identity.id.to_string()
        };
        for (role, trusted) in [
            (IdentityRole::SystemAdmin, TrustLevel::Owner),
            (IdentityRole::Manager, TrustLevel::Owner),
            (IdentityRole::Member, TrustLevel::Paired),
        ] {
            let id = person(role, IdentityStatus::Active);
            let elsewhere = classify_trust_with_role(
                &ContentSource::Gateway { device_id: None },
                None,
                &state.sentinel,
                true,
                Some(role.as_str()),
            );
            assert_eq!(elsewhere, trusted);
            assert_eq!(rest_chat_trust(&state, &caller(Some(&id), None)), trusted);
            assert_eq!(
                rest_chat_trust(&state, &caller(Some(&id), Some("device"))),
                TrustLevel::Paired,
                "a paired device speaks as a member, whoever it is linked to"
            );
        }
        let stranger = uuid::Uuid::new_v4().to_string();
        let suspended = person(IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        for untrusted in [
            caller(Some(&stranger), None),
            caller(Some(&suspended), None),
            caller(Some("not-an-identity"), None),
            caller(None, Some("device")),
            CallerContext::default(),
        ] {
            assert_eq!(rest_chat_trust(&state, &untrusted), TrustLevel::Untrusted);
        }
    }

    /// A gateway over one agent on a mock model, with Alice and Bob as
    /// members, and what signs each of them in.
    async fn rest_chat(
        data: &std::path::Path,
    ) -> (
        Arc<GatewayState>,
        Arc<AgentService>,
        [(workforce_identity::types::Identity, axum::http::HeaderMap); 2],
    ) {
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};
        let (service, _llm) = crate::serve::testing::service_on_mock_llm(data).await;
        let fixture = crate::remote_mcp::testing::fixture(false);
        let jwt = crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600)
            .unwrap();
        let people = ["Alice", "Bob"].map(|name| {
            let person = Identity::new(name, IdentityRole::Member);
            fixture.identities.create(&person).unwrap();
            let token = crate::jwt::generate_token_with_identity(
                &jwt,
                name,
                person.role,
                Some(person.id.to_string()),
            )
            .unwrap();
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            (person, headers)
        });
        let agent = service.agent_id.to_string();
        let state = Arc::new(GatewayState {
            registry: Arc::new(AgentRegistry::from_parts(
                [(agent.clone(), Arc::clone(&service))].into(),
                agent,
            )),
            telemetry_store: Some(Arc::clone(&service.telemetry_store)),
            jwt_config: Some(jwt),
            ..Arc::try_unwrap(fixture.state)
                .ok()
                .expect("the fixture's only handle")
        });
        (state, service, people)
    }

    async fn post_chat(
        state: &Arc<GatewayState>,
        headers: &axum::http::HeaderMap,
        request: ChatRequest,
    ) -> Result<ChatResponse, StatusCode> {
        let anyone = Query(WsQuery {
            token: None,
            agent_id: None,
            channel: None,
            rich: None,
            desk: None,
            accept: None,
        });
        chat_handler(
            headers.clone(),
            anyone,
            State(Arc::clone(state)),
            Json(request),
        )
        .await
        .map(|Json(chat)| chat)
        .map_err(|refused| refused.status())
    }

    fn asking(message: &str, channel: Option<&str>, turn_id: Option<&str>) -> ChatRequest {
        ChatRequest {
            message: message.into(),
            channel: channel.map(String::from),
            session_id: None,
            agent_id: None,
            turn_id: turn_id.map(String::from),
        }
    }

    /// A suspended person keeps the member reads they have become entitled
    /// to, but nothing new runs as them: no REST turn, and no WebSocket,
    /// whose every frame steers or starts something.
    #[tokio::test]
    async fn a_suspended_person_starts_nothing_through_rest_or_a_websocket() {
        use workforce_identity::types::IdentityStatus;
        let tmp = tempfile::tempdir().unwrap();
        let (state, service, [(alice, _), (mut bob, as_bob)]) = rest_chat(tmp.path()).await;
        let store = state.identity_store.clone().expect("an identity store");
        service.set_identity_store(Arc::clone(&store));
        bob.status = IdentityStatus::Suspended;
        store.update(&bob).unwrap();

        let chat = post_chat(&state, &as_bob, asking("hello", None, None))
            .await
            .unwrap();
        assert_eq!(chat.outcome, ChatOutcome::Refused);
        assert_eq!(
            chat.response,
            "Bob is suspended, so nothing can run as them."
        );
        let command = post_chat(&state, &as_bob, asking("/status", None, None))
            .await
            .unwrap();
        assert_eq!(
            (command.outcome, command.response.as_str()),
            (
                ChatOutcome::Refused,
                "Bob is suspended, so nothing can run as them."
            ),
            "a slash command is refused too"
        );
        assert!(suspended_refusal(&state, Some(&bob.id.to_string())).is_some());
        assert!(suspended_refusal(&state, Some(&alice.id.to_string())).is_none());

        bob.status = IdentityStatus::Active;
        store.update(&bob).unwrap();
        assert!(suspended_refusal(&state, Some(&bob.id.to_string())).is_none());
    }

    /// A client's `rich` declaration is recorded on the session its message
    /// runs on (names this hub does not know dropped), and a submitted form
    /// — its summary and fenced values, as Desk sends it — is the turn's user
    /// message byte for byte.
    #[tokio::test]
    async fn a_rich_clients_turn_carries_its_declaration_and_a_form_submission_arrives_intact() {
        use futures::{SinkExt, StreamExt};
        use workforce_common::session::MessageRole;
        let tmp = tempfile::tempdir().unwrap();
        let (state, service, [(_, as_alice), _]) = rest_chat(tmp.path()).await;
        let token = as_alice["authorization"]
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/ws?token={token}&channel=desk&rich=tables,forms,video"
        ))
        .await
        .unwrap();
        async fn next_frame(
            socket: &mut (impl futures::Stream<
                Item = Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            > + Unpin),
        ) -> serde_json::Value {
            loop {
                let frame = socket.next().await.unwrap().unwrap();
                if let Ok(text) = frame.to_text() {
                    return serde_json::from_str(text).unwrap();
                }
            }
        }
        let connected = next_frame(&mut socket).await;
        let session_id = connected["session_id"].as_str().unwrap().to_string();

        let submission = "Submitted \"New lead\"\n- Company: Acme Ltd\n- Status: Open\n\n\
                          ```json\n{\"company\":\"Acme Ltd\",\"status\":\"open\",\"value\":1000,\
                          \"tags\":[\"warm\",\"q3\"],\"consent\":true}\n```";
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"type": "chat", "content": submission})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while next_frame(&mut socket).await["type"] != "stream_end" {}
        })
        .await
        .expect("the turn answers");

        let asked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let session = service.sessions.get(&session_id).unwrap().unwrap();
                if let Some(asked) = session
                    .messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                {
                    return (session.rendered_rich(), asked.content.clone());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the question is kept");
        assert_eq!(asked.0.names(), ["tables", "forms"]);
        assert_eq!(asked.1, submission);
    }

    /// A token minted while its person was an administrator claims only
    /// the role they have now.
    #[tokio::test]
    async fn the_identity_api_reads_the_callers_role_now_not_from_their_token() {
        use workforce_identity::types::{IdentityRole, IdentityStatus};
        let tmp = tempfile::tempdir().unwrap();
        let (state, _, [(mut alice, _), _]) = rest_chat(tmp.path()).await;
        let minted_as_admin = crate::jwt::generate_token_with_identity(
            state.jwt_config.as_ref().unwrap(),
            "Alice",
            IdentityRole::SystemAdmin,
            Some(alice.id.to_string()),
        )
        .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {minted_as_admin}").parse().unwrap(),
        );
        let role = |state: &GatewayState| {
            crate::api_handlers::identity::require_auth(state, &headers)
                .unwrap()
                .role
        };
        assert_eq!(role(&state), IdentityRole::Member);

        let store = state.identity_store.clone().unwrap();
        alice.set_role(IdentityRole::SystemAdmin);
        store.update(&alice).unwrap();
        assert_eq!(role(&state), IdentityRole::SystemAdmin);
        alice.status = IdentityStatus::Suspended;
        store.update(&alice).unwrap();
        assert_eq!(
            role(&state),
            IdentityRole::Member,
            "a suspended administrator is the member they have become"
        );
    }

    /// Every REST route reads the caller from the live identity: a token
    /// minted while its person was an administrator sees only their own
    /// sessions once they are a member or suspended, and carries no admin
    /// scope.
    #[tokio::test]
    async fn a_demoted_or_suspended_administrators_token_is_no_administrators_over_rest() {
        use workforce_identity::types::{IdentityRole, IdentityStatus};
        let tmp = tempfile::tempdir().unwrap();
        let (state, _, [(mut alice, _), (bob, _)]) = rest_chat(tmp.path()).await;
        let minted_as_admin = crate::jwt::generate_token_with_identity(
            state.jwt_config.as_ref().unwrap(),
            "Alice",
            IdentityRole::SystemAdmin,
            Some(alice.id.to_string()),
        )
        .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {minted_as_admin}").parse().unwrap(),
        );
        let bobs = bob.id.to_string();
        let caller = || resolve_caller_context(&state, &headers, None);

        let member = caller().await;
        assert!(!member.is_admin && !member.can_see_owner(&bobs));
        assert!(!member.has_scope(crate::jwt::SCOPE_ADMIN));
        assert!(member.has_scope(crate::jwt::SCOPE_CHAT));

        let store = state.identity_store.clone().unwrap();
        alice.set_role(IdentityRole::SystemAdmin);
        store.update(&alice).unwrap();
        assert!(caller().await.can_see_owner(&bobs));

        alice.status = IdentityStatus::Suspended;
        store.update(&alice).unwrap();
        let suspended = caller().await;
        assert!(!suspended.is_admin && !suspended.can_see_owner(&bobs));
    }

    #[tokio::test]
    async fn a_named_rest_turn_is_filed_where_only_its_asker_can_read_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, service, [(alice, as_alice), (_, as_bob)]) = rest_chat(tmp.path()).await;

        let asked = asking("how many orders shipped?", Some("eval:t1"), Some("t1"));
        let chat = post_chat(&state, &as_alice, asked).await.unwrap();
        assert_eq!(chat.outcome, ChatOutcome::Answered);
        assert!(chat.response.starts_with("done"), "{}", chat.response);
        assert!(!chat.capped);
        let task_id = chat.task_id.expect("a named turn has a task id");
        assert_eq!(task_id, format!("chat:{}:t1", alice.id));

        let trace_as = |headers: &axum::http::HeaderMap| {
            crate::api_handlers::stats::get_task_trace(
                State(Arc::clone(&state)),
                headers.clone(),
                Path(task_id.clone()),
            )
        };
        let Json(trace) = trace_as(&as_alice)
            .await
            .map_err(|(status, _)| status)
            .unwrap();
        assert_eq!(trace.task.identity_id, Some(alice.id.to_string()));
        assert_eq!((trace.task.input_tokens, trace.task.output_tokens), (10, 5));
        assert!(trace.task.rounds >= 1);
        assert_eq!(
            trace_as(&as_bob)
                .await
                .map(drop)
                .map_err(|(status, _)| status),
            Err(StatusCode::FORBIDDEN),
            "another person read the turn"
        );

        let own_name_again = asking("and cancelled?", Some("eval:t2"), Some("t1"));
        let chat = post_chat(&state, &as_bob, own_name_again).await.unwrap();
        assert_ne!(chat.task_id.as_deref(), Some(task_id.as_str()));
        let Json(trace) = trace_as(&as_alice)
            .await
            .map_err(|(status, _)| status)
            .unwrap();
        assert_eq!(trace.task.identity_id, Some(alice.id.to_string()));

        let chat = post_chat(&state, &as_alice, asking("and last week?", None, None))
            .await
            .unwrap();
        assert_eq!(chat.task_id, None);
        let said = service.sessions.get(&chat.session_id).unwrap().unwrap();
        let asked = said
            .messages
            .iter()
            .find(|message| message.content == "and last week?")
            .expect("the question is in its session");
        assert_eq!(
            asked.trust_level,
            Some(TrustLevel::Paired),
            "a member's REST message is a paired person's"
        );
    }

    #[tokio::test]
    async fn rest_chat_keeps_to_the_agents_access_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, service, [(alice, as_alice), (_, as_bob)]) = rest_chat(tmp.path()).await;
        state.access_gate.upsert(
            service.agent_id.to_string(),
            allowlist(&[&alice.id.to_string()]),
        );
        assert_eq!(
            post_chat(&state, &as_bob, asking("hello", None, None))
                .await
                .map(drop),
            Err(StatusCode::FORBIDDEN)
        );
        assert!(service.sessions.list(None, 10).unwrap().is_empty());
        post_chat(&state, &as_alice, asking("hello", None, None))
            .await
            .unwrap();
        let unsigned = axum::http::HeaderMap::new();
        assert_eq!(
            post_chat(&state, &unsigned, asking("hello", None, None))
                .await
                .map(drop),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn a_rest_turn_says_which_config_it_ran_under() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, service, [(_, as_alice), _]) = rest_chat(tmp.path()).await;
        let chat = post_chat(&state, &as_alice, asking("hello", None, None))
            .await
            .unwrap();
        assert_eq!(chat.config, None, "the agent has no config.toml");

        let edited = "[know_how]\nenabled = false\n";
        std::fs::write(service.workspace.join("config.toml"), edited).unwrap();
        let chat = post_chat(&state, &as_alice, asking("hello", None, None))
            .await
            .unwrap();
        assert_eq!(chat.config, None, "the edit has not reached the agent");

        service.reload_config().await.unwrap();
        let chat = post_chat(&state, &as_alice, asking("hello", None, None))
            .await
            .unwrap();
        assert_eq!(
            chat.config,
            Some(workforce_orchestrator::workflow_store::content_hash(edited))
        );
    }

    #[test]
    fn a_turn_is_capped_only_by_a_cap_it_hit_itself() {
        let mut session = workforce_common::session::Session::new("rest", "alice", "scout");
        let before = chrono::Utc::now() - chrono::Duration::seconds(1);
        assert!(!crate::serve::round_capped_since(&session, before));
        session.metadata = serde_json::json!({
            "round_cap_continuation": {"rounds": 24, "at": chrono::Utc::now().to_rfc3339()}
        });
        assert!(crate::serve::round_capped_since(&session, before));
        let later = chrono::Utc::now() + chrono::Duration::seconds(1);
        assert!(!crate::serve::round_capped_since(&session, later));
    }

    fn allowlist(allow: &[&str]) -> AgentAccessConfig {
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: allow.iter().map(|id| id.to_string()).collect(),
            ..Default::default()
        }
    }

    fn sees_unowned(state: &GatewayState, caller: CallerContext, agent_id: &str) -> bool {
        AgentViewer::new(state, caller)
            .visibility(agent_id, None, &OwnerType::User)
            .visible
    }

    #[test]
    fn an_unregistered_agent_is_never_visible_though_the_gate_allows_it() {
        let state = gateway();
        assert_eq!(
            state.access_gate.check(crate::access_gate::AccessRequest {
                agent_id: "ghost",
                identity_id: Some("alice"),
                ..Default::default()
            }),
            AccessDecision::Allow
        );
        assert!(!AgentViewer::new(&state, caller(Some("alice"), None)).can_see("ghost"));
    }

    #[test]
    fn viewer_shows_an_open_agent_to_a_member_without_its_owner() {
        let state = gateway();
        let vis = AgentViewer::new(&state, caller(Some("alice"), None)).visibility(
            "atlas",
            None,
            &OwnerType::User,
        );
        assert!(vis.visible);
        assert!(!vis.disclose_owner);
    }

    #[test]
    fn viewer_hides_an_allowlisted_agent_from_a_member_outside_the_list() {
        let state = gateway();
        state
            .access_gate
            .upsert("quill".into(), allowlist(&["alice"]));
        assert!(!sees_unowned(&state, caller(Some("bob"), None), "quill"));
    }

    #[test]
    fn viewer_shows_an_allowlisted_agent_to_a_member_on_the_list() {
        let state = gateway();
        state
            .access_gate
            .upsert("quill".into(), allowlist(&["alice"]));
        assert!(sees_unowned(&state, caller(Some("alice"), None), "quill"));
    }

    #[test]
    fn viewer_hides_a_blocklisted_agent_from_a_blocked_member() {
        let state = gateway();
        state.access_gate.upsert(
            "atlas".into(),
            AgentAccessConfig {
                mode: AccessMode::Blocklist,
                block: vec!["bob".into()],
                ..Default::default()
            },
        );
        assert!(!sees_unowned(&state, caller(Some("bob"), None), "atlas"));
        assert!(sees_unowned(&state, caller(Some("alice"), None), "atlas"));
    }

    #[test]
    fn viewer_matches_a_paired_device_by_its_surface_id() {
        let state = gateway();
        state
            .access_gate
            .upsert("quill".into(), allowlist(&["ios_device:device-abc"]));
        assert!(sees_unowned(
            &state,
            caller(None, Some("device-abc")),
            "quill"
        ));
        assert!(!sees_unowned(
            &state,
            caller(None, Some("device-xyz")),
            "quill"
        ));
    }

    #[test]
    fn viewer_shows_a_locked_agent_to_its_owner() {
        let state = gateway();
        state
            .access_gate
            .upsert("quill".into(), allowlist(&["bob"]));
        let vis = AgentViewer::new(&state, caller(Some("alice"), None)).visibility(
            "quill",
            Some("alice"),
            &OwnerType::User,
        );
        assert!(vis.visible);
        assert!(vis.disclose_owner);
    }

    #[test]
    fn viewer_hides_an_open_agent_from_an_anonymous_caller() {
        assert!(!sees_unowned(&gateway(), CallerContext::default(), "atlas"));
    }

    #[tokio::test]
    async fn can_see_follows_the_registry_and_the_access_policy() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = data_dir.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let service = AgentService::from_config(config, None)
            .await
            .expect("test service");
        let state = gateway();
        state.registry.insert("atlas", Arc::clone(&service));
        state.registry.insert("quill", service);
        state
            .access_gate
            .upsert("quill".into(), allowlist(&["alice"]));

        let bob = AgentViewer::new(&state, caller(Some("bob"), None));
        assert!(bob.can_see("atlas"));
        assert!(!bob.can_see("quill"));
        let alice = AgentViewer::new(&state, caller(Some("alice"), None));
        assert!(alice.can_see("atlas"));
        assert!(alice.can_see("quill"));
    }
}

#[cfg(test)]
mod caller_scope_tests {
    //! Scope enforcement on `CallerContext::has_scope`. The predicate gates
    //! every REST handler via `require_scope`; the tests pin the four
    //! decision paths: admin bypass, wildcard, explicit match, and the new
    //! empty-deny semantic (security finding 4.1.3).
    use super::*;

    fn device(scopes: &[&str]) -> CallerContext {
        CallerContext {
            identity_id: None,
            device_id: Some("device-abc".into()),
            is_admin: false,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            auth_failure: None,
        }
    }

    #[test]
    fn admin_bypasses_every_scope_check() {
        let admin = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(admin.has_scope(crate::jwt::SCOPE_CHAT));
        assert!(admin.has_scope(crate::jwt::SCOPE_ADMIN));
        assert!(admin.has_scope("anything-else"));
    }

    #[test]
    fn wildcard_scope_matches_every_required() {
        let c = device(&[crate::jwt::SCOPE_WILDCARD]);
        assert!(c.has_scope(crate::jwt::SCOPE_CHAT));
        assert!(c.has_scope(crate::jwt::SCOPE_ADMIN));
        assert!(c.has_scope(crate::jwt::SCOPE_SESSIONS));
    }

    #[test]
    fn explicit_scope_matches_itself_only() {
        let c = device(&[crate::jwt::SCOPE_CHAT, crate::jwt::SCOPE_SESSIONS]);
        assert!(c.has_scope(crate::jwt::SCOPE_CHAT));
        assert!(c.has_scope(crate::jwt::SCOPE_SESSIONS));
        assert!(!c.has_scope(crate::jwt::SCOPE_ADMIN));
        assert!(!c.has_scope(crate::jwt::SCOPE_HEARTBEAT));
    }

    /// Security-critical: empty scopes deny by default. Before the
    /// `jwt::has_scope` flip this returned true for every required scope,
    /// silently granting full access to every CLI- or legacy-iOS-paired
    /// device. The V003 migration backfills existing rows to `["*"]`.
    #[test]
    fn empty_scopes_deny_every_required() {
        let c = device(&[]);
        assert!(!c.has_scope(crate::jwt::SCOPE_CHAT));
        assert!(!c.has_scope(crate::jwt::SCOPE_ADMIN));
        assert!(!c.has_scope(crate::jwt::SCOPE_WILDCARD));
    }
}

#[cfg(test)]
mod livez_tests {
    //! Pins the public liveness probe's body so a future "let's add a bit
    //! more info" PR can't accidentally leak runtime state into the
    //! unauthenticated endpoint (security finding 4.2.3).
    use super::*;

    #[tokio::test]
    async fn livez_returns_only_ok_true() {
        let Json(body) = livez().await;
        assert_eq!(body, serde_json::json!({ "ok": true }));
    }
}

#[cfg(test)]
mod client_ip_tests {
    //! F12 (2026-05-15): client_ip honours XFF only when the direct
    //! connection peer is a configured trusted proxy. Without a known
    //! peer or with the empty allowlist, falls back to loopback so all
    //! unverified traffic shares one rate-limit bucket.
    use super::*;
    use crate::trusted_proxy::TrustedProxyConfig;
    use std::net::{IpAddr, SocketAddr};

    fn headers_with_xff(xff: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", xff.parse().unwrap());
        h
    }

    fn peer(addr: &str) -> SocketAddr {
        format!("{addr}:1234").parse().unwrap()
    }

    #[test]
    fn empty_allowlist_ignores_xff() {
        let cfg = TrustedProxyConfig::default();
        let headers = headers_with_xff("203.0.113.7");
        let ip = client_ip(&cfg, &headers, Some(peer("10.0.0.1")));
        assert_eq!(ip, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_peer_honours_leftmost_xff() {
        let cfg = TrustedProxyConfig::from_strings(&["10.0.0.0/8".to_string()]).unwrap();
        let headers = headers_with_xff("203.0.113.7, 10.20.0.5");
        let ip = client_ip(&cfg, &headers, Some(peer("10.20.0.5")));
        assert_eq!(ip, "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn untrusted_peer_ignores_xff() {
        // Attacker claim: "I'm 10.0.0.1 forwarding for 1.2.3.4". Actual
        // peer is 198.51.100.5 (not in 10.0.0.0/8) → spoofed XFF dropped.
        let cfg = TrustedProxyConfig::from_strings(&["10.0.0.0/8".to_string()]).unwrap();
        let headers = headers_with_xff("1.2.3.4");
        let ip = client_ip(&cfg, &headers, Some(peer("198.51.100.5")));
        assert_eq!(ip, "198.51.100.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn missing_peer_falls_back_to_loopback() {
        let cfg = TrustedProxyConfig::from_strings(&["10.0.0.0/8".to_string()]).unwrap();
        let headers = headers_with_xff("203.0.113.7");
        let ip = client_ip(&cfg, &headers, None);
        assert_eq!(ip, IpAddr::from([127, 0, 0, 1]));
    }

    #[test]
    fn malformed_xff_falls_back_to_peer() {
        let cfg = TrustedProxyConfig::from_strings(&["10.0.0.0/8".to_string()]).unwrap();
        let headers = headers_with_xff("not-an-ip");
        let ip = client_ip(&cfg, &headers, Some(peer("10.20.0.5")));
        assert_eq!(ip, "10.20.0.5".parse::<IpAddr>().unwrap());
    }
}

#[cfg(test)]
mod body_limit_tests {
    //! F3 (2026-05-15): router-level body-size cap. A 2 MB POST should be
    //! rejected by the tower layer before it ever reaches a handler.
    use axum::body::Body;
    use axum::extract::DefaultBodyLimit;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    async fn echo(body: axum::body::Bytes) -> String {
        format!("{}", body.len())
    }

    fn router_with_default_limit() -> Router {
        Router::new()
            .route("/echo", post(echo))
            .layer(DefaultBodyLimit::max(1024 * 1024))
    }

    #[tokio::test]
    async fn oversized_post_returns_413() {
        let app = router_with_default_limit();
        let payload = vec![b'A'; 2 * 1024 * 1024]; // 2 MiB
        let req = Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(payload))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn modest_post_succeeds() {
        let app = router_with_default_limit();
        let payload = vec![b'A'; 1024]; // 1 KiB
        let req = Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(payload))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn per_route_override_lifts_limit() {
        // Mirrors the upload-route override pattern: 64 MiB on the route,
        // 1 MiB on the rest.
        let app = Router::new()
            .route(
                "/upload",
                post(echo).layer(DefaultBodyLimit::max(8 * 1024 * 1024)),
            )
            .route("/normal", post(echo))
            .layer(DefaultBodyLimit::max(1024 * 1024));

        // 2 MiB to /upload — allowed by the per-route override.
        let payload = vec![b'A'; 2 * 1024 * 1024];
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(Body::from(payload))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Same payload to /normal — rejected by the router-level limit.
        let payload = vec![b'A'; 2 * 1024 * 1024];
        let req = Request::builder()
            .method("POST")
            .uri("/normal")
            .body(Body::from(payload))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}

#[cfg(test)]
mod cors_layer_tests {
    //! DW-517: replaces `CorsLayer::permissive()` with an explicit allow-list.
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn ping() -> &'static str {
        "ok"
    }

    fn preflight(origin: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method(Method::OPTIONS)
            .uri("/p")
            .header("Origin", origin)
            .header("Access-Control-Request-Method", "GET")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn router_with(layer: CorsLayer) -> Router {
        Router::new().route("/p", get(ping)).layer(layer)
    }

    #[tokio::test]
    async fn empty_allow_list_emits_no_cors_headers() {
        let app = router_with(build_cors_layer(true, &[]));
        let resp = app
            .oneshot(preflight("https://attacker.example"))
            .await
            .unwrap();
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn cors_disabled_emits_no_cors_headers() {
        let app = router_with(build_cors_layer(
            false,
            &["https://app.example.com".to_string()],
        ));
        let resp = app
            .oneshot(preflight("https://app.example.com"))
            .await
            .unwrap();
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn explicit_origin_is_echoed_for_listed_origin_only() {
        let app = router_with(build_cors_layer(
            true,
            &["https://app.example.com".to_string()],
        ));

        let listed = app
            .clone()
            .oneshot(preflight("https://app.example.com"))
            .await
            .unwrap();
        assert_eq!(
            listed
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );

        let unlisted = app
            .oneshot(preflight("https://attacker.example"))
            .await
            .unwrap();
        assert!(unlisted
            .headers()
            .get("access-control-allow-origin")
            .is_none());
    }

    #[tokio::test]
    async fn wildcard_origin_disables_credentials() {
        let app = router_with(build_cors_layer(true, &["*".to_string()]));
        let resp = app
            .oneshot(preflight("https://anywhere.example"))
            .await
            .unwrap();
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert!(resp
            .headers()
            .get("access-control-allow-credentials")
            .is_none());
    }
}

#[cfg(test)]
mod pairing_auto_link_tests {
    //! Behaviour tests for the pairing-time identity hint that lets the
    //! iOS app skip a separate JWT login. These cover the three resolution
    //! paths in [`auto_link_device_to_identity`] and the input-normalisation
    //! rules.
    use super::*;
    use crate::device_link::{auto_link_device_to_identity, device_link_key};
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::store::IdentityStore;
    use workforce_identity::types::*;

    fn store() -> SqliteIdentityStore {
        SqliteIdentityStore::in_memory().expect("in-memory identity store")
    }

    #[test]
    fn normalises_email_to_lowercase() {
        assert_eq!(
            normalise_identity_hint(Some("  Kings@EXAMPLE.com ".into())),
            Some("kings@example.com".into())
        );
    }

    #[test]
    fn preserves_uuid_case() {
        // UUIDs have a canonical lowercase representation but the parser
        // accepts mixed case; we don't lowercase non-email hints because
        // they may be opaque identifiers in other systems.
        let uuid = "F1706D96-86C8-492A-A88B-B259ECB08A25";
        assert_eq!(
            normalise_identity_hint(Some(uuid.into())),
            Some(uuid.to_string())
        );
    }

    #[test]
    fn drops_empty_and_whitespace_hints() {
        assert_eq!(normalise_identity_hint(None), None);
        assert_eq!(normalise_identity_hint(Some(String::new())), None);
        assert_eq!(normalise_identity_hint(Some("   ".into())), None);
    }

    #[test]
    fn auto_link_resolves_existing_identity_by_uuid() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member);
        let id_str = identity.id.to_string();
        store.create(&identity).unwrap();

        let result =
            auto_link_device_to_identity(&store, &id_str, "device-1", "ios_device").unwrap();
        let (linked_id, kind) = result.expect("link succeeded");
        assert_eq!(linked_id, id_str);
        assert_eq!(kind, "id");

        let after = store.get_by_id(identity.id).unwrap().unwrap();
        assert_eq!(
            after
                .linked_ids
                .get(&device_link_key("ios_device", "device-1"))
                .map(String::as_str),
            Some("device-1")
        );
    }

    #[test]
    fn auto_link_resolves_existing_identity_by_email() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member).with_email("kings@example.com");
        let id_str = identity.id.to_string();
        store.create(&identity).unwrap();

        let result =
            auto_link_device_to_identity(&store, "kings@example.com", "device-2", "ios_device")
                .unwrap();
        let (linked_id, kind) = result.expect("link succeeded");
        assert_eq!(linked_id, id_str);
        assert_eq!(kind, "email");
    }

    #[test]
    fn auto_link_provisions_new_identity_when_email_unknown() {
        let store = store();
        let result =
            auto_link_device_to_identity(&store, "fresh@example.com", "device-3", "ios_device")
                .unwrap();
        let (id_str, kind) = result.expect("link succeeded");
        assert_eq!(kind, "auto_provisioned");

        let uuid = uuid::Uuid::parse_str(&id_str).unwrap();
        let created = store.get_by_id(uuid).unwrap().expect("identity created");
        assert_eq!(created.email.as_deref(), Some("fresh@example.com"));
        assert_eq!(created.status, IdentityStatus::AutoProvisioned);
        assert_eq!(created.provisioning_source, ProvisioningSource::IosAuto);
        assert_eq!(
            created
                .linked_ids
                .get(&device_link_key("ios_device", "device-3"))
                .map(String::as_str),
            Some("device-3")
        );
    }

    /// A person provisioned by a device's pairing has it linked under its own
    /// kind, so the phone they pair next does not replace it.
    #[test]
    fn auto_link_provisions_under_the_devices_own_kind() {
        let store = store();
        let (id_str, _) =
            auto_link_device_to_identity(&store, "fresh@example.com", "desk-1", "desktop_device")
                .unwrap()
                .expect("link succeeded");
        auto_link_device_to_identity(&store, "fresh@example.com", "phone-1", "ios_device").unwrap();

        let created = store
            .get_by_id(uuid::Uuid::parse_str(&id_str).unwrap())
            .unwrap()
            .expect("identity created");
        assert_eq!(
            created
                .linked_ids
                .get(&device_link_key("desktop_device", "desk-1"))
                .map(String::as_str),
            Some("desk-1")
        );
        assert_eq!(
            created
                .linked_ids
                .get(&device_link_key("ios_device", "phone-1"))
                .map(String::as_str),
            Some("phone-1")
        );
        assert_eq!(
            identity_for_device(&store, "desk-1").as_deref(),
            Some(id_str.as_str())
        );
    }

    #[test]
    fn auto_link_returns_none_for_unparsable_uuid_without_at_sign() {
        // A hint with no '@' that isn't a UUID can't be email-resolved or
        // safely auto-provisioned (we don't want to create identities from
        // arbitrary opaque strings) — return None and let the admin link
        // the device manually.
        let store = store();
        let result =
            auto_link_device_to_identity(&store, "some-random-string", "device-4", "ios_device")
                .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn auto_link_refuses_suspended_identity() {
        let store = store();
        let identity = Identity::new("blocked", IdentityRole::Member)
            .with_email("blocked@example.com")
            .with_status(IdentityStatus::Suspended);
        store.create(&identity).unwrap();

        let result =
            auto_link_device_to_identity(&store, "blocked@example.com", "device-5", "ios_device")
                .unwrap();
        assert!(
            result.is_none(),
            "suspended identities must not auto-link to a new device"
        );
    }

    /// The bug this fixes: pairing a voice satellite used to evict the phone,
    /// because every device linked under the hardcoded `ios_device` key. A
    /// device of a different kind must take its own slot and leave the phone
    /// exactly where it was.
    #[test]
    fn devices_of_different_kinds_coexist_on_one_identity() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member).with_email("kings@example.com");
        let id_uuid = identity.id;
        store.create(&identity).unwrap();

        auto_link_device_to_identity(&store, "kings@example.com", "phone-1", "ios_device").unwrap();
        auto_link_device_to_identity(&store, "kings@example.com", "bridge-1", "voice_device")
            .unwrap();

        let after = store.get_by_id(id_uuid).unwrap().unwrap();
        assert_eq!(
            after
                .linked_ids
                .get(&device_link_key("ios_device", "phone-1"))
                .map(String::as_str),
            Some("phone-1"),
            "pairing a voice device must not unlink the phone"
        );
        assert_eq!(
            after
                .linked_ids
                .get(&device_link_key("voice_device", "bridge-1"))
                .map(String::as_str),
            Some("bridge-1")
        );
    }

    /// A device must resolve to its owner whatever kind it paired as; a lookup
    /// fixed to `ios_device` would authenticate phones and leave every other
    /// surface anonymous — which presented as the bridge having no chat history.
    #[test]
    fn a_device_resolves_to_its_owner_whatever_kind_it_is() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member).with_email("kings@example.com");
        let id_uuid = identity.id.to_string();
        store.create(&identity).unwrap();

        auto_link_device_to_identity(&store, "kings@example.com", "bridge-1", "voice_device")
            .unwrap();

        assert_eq!(
            identity_for_device(&store, "bridge-1").as_deref(),
            Some(id_uuid.as_str())
        );
        assert!(identity_for_device(&store, "never-paired").is_none());
    }

    /// A person's second iPhone (or iPad) joins the first instead of
    /// replacing it: each device is linked under its own key, so the first
    /// one's token never goes on working anonymously.
    #[test]
    fn devices_of_the_same_kind_coexist_on_one_identity() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member).with_email("kings@example.com");
        let id_str = identity.id.to_string();
        store.create(&identity).unwrap();

        for phone in ["phone-1", "phone-2"] {
            auto_link_device_to_identity(&store, "kings@example.com", phone, "ios_device").unwrap();
        }

        for phone in ["phone-1", "phone-2"] {
            assert_eq!(
                identity_for_device(&store, phone).as_deref(),
                Some(id_str.as_str()),
                "{phone} stays linked"
            );
        }
    }

    /// Devices linked before per-device keys, under one kind's single slot,
    /// still resolve, and a re-link moves them to their own key.
    #[test]
    fn a_device_linked_the_old_way_still_resolves_and_moves_to_its_own_key() {
        let store = store();
        let identity = Identity::new("Kings", IdentityRole::Member)
            .with_email("kings@example.com")
            .with_linked_id("ios_device", "phone-1");
        let id_uuid = identity.id;
        store.create(&identity).unwrap();
        assert_eq!(
            identity_for_device(&store, "phone-1"),
            Some(id_uuid.to_string())
        );

        auto_link_device_to_identity(&store, "kings@example.com", "phone-1", "ios_device").unwrap();

        let after = store.get_by_id(id_uuid).unwrap().unwrap();
        assert!(!after.linked_ids.contains_key("ios_device"));
        assert_eq!(
            after
                .linked_ids
                .get(&device_link_key("ios_device", "phone-1"))
                .map(String::as_str),
            Some("phone-1")
        );
    }
}

#[cfg(test)]
mod vault_copy_tests {
    use super::*;
    use workforce_common::settings::VaultSource;

    #[test]
    fn named_admin_required_rejects_shared_token() {
        let jwt_admin = CallerContext {
            identity_id: Some("admin-1".to_string()),
            is_admin: true,
            ..Default::default()
        };
        assert_eq!(require_named_admin(jwt_admin).unwrap(), "admin-1");

        let shared_token = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(require_named_admin(shared_token).is_err());
    }

    #[test]
    fn source_user_required_only_for_per_user_source() {
        assert_eq!(
            resolve_source_user(VaultSource::AgentPerUser, Some("alice")).unwrap(),
            Some("alice")
        );
        assert_eq!(
            resolve_source_user(VaultSource::AgentSpecific, None).unwrap(),
            None
        );
        assert_eq!(
            resolve_source_user(VaultSource::AgentSpecific, Some("ignored")).unwrap(),
            None
        );
        assert!(resolve_source_user(VaultSource::AgentPerUser, None).is_err());
    }

    #[test]
    fn logical_path_validation() {
        assert!(validate_logical_path("skills/xero/secret").is_ok());
        for bad in ["users/alice/p", "a/../b", "wasm-skills/x"] {
            assert!(
                validate_logical_path(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }
}

#[cfg(test)]
mod consolidation_template_tests {
    use super::*;
    use workforce_skills::adapters::xero::report::ConsolidationTemplate;

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|s| s.to_string()).collect()
    }

    /// The grid contract: lines preserve row order with their kind; every
    /// non-blank entity cell maps that source label to the canonical label; the
    /// output deserializes back into a `ConsolidationTemplate`.
    #[test]
    fn rows_to_template_builds_lines_and_map() {
        let grid = vec![
            row(&["Line", "Kind", "uk", "us"]),
            row(&["Income", "header", "", ""]),
            row(&["Revenue", "line", "UK Sales", "Product Revenue"]),
            row(&["Services", "", "Consulting", ""]),
            row(&["Total Income", "subtotal", "", ""]),
        ];
        let value = rows_to_template(grid);
        let template: ConsolidationTemplate = serde_json::from_value(value).unwrap();

        use workforce_skills::adapters::xero::report::TemplateLineKind;
        let labels: Vec<_> = template.lines.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Income", "Revenue", "Services", "Total Income"]
        );
        assert_eq!(template.lines[0].kind, TemplateLineKind::Header);
        // Blank kind coerces to Line.
        assert_eq!(template.lines[2].kind, TemplateLineKind::Line);
        assert_eq!(template.lines[3].kind, TemplateLineKind::Subtotal);

        // Map: per entity, source label → canonical label; blank cells absent.
        assert_eq!(
            template.map["uk"]["UK Sales"], "Revenue",
            "uk source maps to canonical"
        );
        assert_eq!(template.map["uk"]["Consulting"], "Services");
        assert_eq!(template.map["us"]["Product Revenue"], "Revenue");
        assert!(
            !template.map["us"].contains_key("Consulting"),
            "blank us cell must not appear in the map"
        );
    }

    /// End-to-end through the bounded reader: an empty first sheet is
    /// skipped, cells arrive trimmed, and the grid feeds the pure core.
    #[test]
    fn parse_xlsx_template_reads_the_first_non_empty_sheet() {
        let mut wb = rust_xlsxwriter::Workbook::new();
        wb.add_worksheet();
        let ws = wb.add_worksheet();
        ws.write_string(0, 0, "Line").unwrap();
        ws.write_string(0, 1, "Kind").unwrap();
        ws.write_string(0, 2, "uk").unwrap();
        ws.write_string(1, 0, " Revenue ").unwrap();
        ws.write_string(1, 1, "line").unwrap();
        ws.write_string(1, 2, "UK Sales").unwrap();
        let bytes = wb.save_to_buffer().unwrap();

        let template: ConsolidationTemplate =
            serde_json::from_value(parse_xlsx_template(&bytes).unwrap()).unwrap();
        let labels: Vec<_> = template.lines.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, vec!["Revenue"], "cell text arrives trimmed");
        assert_eq!(template.map["uk"]["UK Sales"], "Revenue");
    }

    /// Defensive edges: an empty grid yields an empty template; a header-only
    /// grid yields no lines; rows with no label and an unrecognised kind degrade
    /// safely (kind → line, label-less row skipped).
    #[test]
    fn rows_to_template_is_defensive() {
        let empty: ConsolidationTemplate =
            serde_json::from_value(rows_to_template(vec![])).unwrap();
        assert!(empty.lines.is_empty() && empty.map.is_empty());

        let header_only: ConsolidationTemplate =
            serde_json::from_value(rows_to_template(vec![row(&["Line", "Kind", "uk"])])).unwrap();
        assert!(header_only.lines.is_empty());

        // A label-less row is skipped; a ragged/short row and a bogus kind don't
        // panic and the kind falls back to Line.
        let grid = vec![
            row(&["Line", "Kind", "uk"]),
            row(&["", "line", "Orphan"]),
            row(&["Cash"]),
            row(&["Odd", "weird-kind", "Bank"]),
        ];
        let template: ConsolidationTemplate =
            serde_json::from_value(rows_to_template(grid)).unwrap();
        use workforce_skills::adapters::xero::report::TemplateLineKind;
        let labels: Vec<_> = template.lines.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, vec!["Cash", "Odd"]);
        assert_eq!(template.lines[1].kind, TemplateLineKind::Line);
        assert_eq!(template.map["uk"]["Bank"], "Odd");
        // The orphan source under a label-less row never reached the map.
        assert!(!template.map["uk"].contains_key("Orphan"));
    }

    #[test]
    fn parse_line_kind_coerces_unknown_to_line() {
        use workforce_skills::adapters::xero::report::TemplateLineKind;
        assert_eq!(parse_line_kind("Header"), TemplateLineKind::Header);
        assert_eq!(parse_line_kind(" SUBTOTAL "), TemplateLineKind::Subtotal);
        assert_eq!(parse_line_kind("line"), TemplateLineKind::Line);
        assert_eq!(parse_line_kind(""), TemplateLineKind::Line);
        assert_eq!(parse_line_kind("nonsense"), TemplateLineKind::Line);
    }
}

#[cfg(test)]
mod gateway_principal_tests {
    //! Unit coverage for [`gateway_principal`] — the REST surface's single
    //! caller → [`crate::authz::Principal`] resolution. Complements the
    //! route-level tests in `tests/api/authz_routes.rs`: here we pin the
    //! resolution rules directly (static-token trust, live-status lookup,
    //! deny-by-default) without the axum plumbing.
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::store::IdentityStore;
    use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

    fn store_with(identities: &[Identity]) -> Arc<dyn workforce_identity::store::IdentityStore> {
        let store = SqliteIdentityStore::in_memory().unwrap();
        for i in identities {
            store.create(i).unwrap();
        }
        Arc::new(store)
    }

    #[test]
    fn static_token_resolves_to_active_system_admin() {
        // is_admin with no identity_id == the static gateway / owner master
        // token, which is trusted outright.
        let caller = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        let p = gateway_principal(&caller, None);
        assert_eq!(p.identity_id, None);
        assert_eq!(p.role, IdentityRole::SystemAdmin);
        assert_eq!(p.status, IdentityStatus::Active);
        assert!(crate::authz::enforce(
            &p,
            crate::authz::Capability::PermissionsManage,
            &crate::authz::CapabilityMatrix::default()
        )
        .is_ok());
    }

    #[test]
    fn jwt_admin_resolves_via_live_lookup() {
        let admin = Identity::new("admin", IdentityRole::SystemAdmin);
        let store = store_with(std::slice::from_ref(&admin));
        let caller = CallerContext {
            identity_id: Some(admin.id.to_string()),
            is_admin: true,
            ..Default::default()
        };
        let p = gateway_principal(&caller, Some(&store));
        assert_eq!(p.role, IdentityRole::SystemAdmin);
        assert_eq!(p.status, IdentityStatus::Active);
        assert!(crate::authz::enforce(
            &p,
            crate::authz::Capability::VaultSharedDelete,
            &crate::authz::CapabilityMatrix::default()
        )
        .is_ok());
    }

    #[test]
    fn suspended_admin_jwt_resolves_to_non_admin_via_live_status() {
        // The residual the slice closes: the JWT still says SystemAdmin
        // (caller.is_admin == true) but the live record is Suspended — the
        // engine must score it as a non-admin and deny.
        let suspended = Identity::new("admin", IdentityRole::SystemAdmin)
            .with_status(IdentityStatus::Suspended);
        let store = store_with(std::slice::from_ref(&suspended));
        let caller = CallerContext {
            identity_id: Some(suspended.id.to_string()),
            is_admin: true,
            ..Default::default()
        };
        let p = gateway_principal(&caller, Some(&store));
        assert_eq!(p.status, IdentityStatus::Suspended);
        assert_eq!(
            crate::authz::enforce(
                &p,
                crate::authz::Capability::PermissionsManage,
                &crate::authz::CapabilityMatrix::default()
            ),
            Err(crate::authz::Decision::Deny)
        );
    }

    #[test]
    fn member_jwt_resolves_to_member() {
        let member = Identity::new("member", IdentityRole::Member);
        let store = store_with(std::slice::from_ref(&member));
        let caller = CallerContext {
            identity_id: Some(member.id.to_string()),
            ..Default::default()
        };
        let p = gateway_principal(&caller, Some(&store));
        assert_eq!(p.role, IdentityRole::Member);
        assert_eq!(
            crate::authz::enforce(
                &p,
                crate::authz::Capability::RolesAssign,
                &crate::authz::CapabilityMatrix::default()
            ),
            Err(crate::authz::Decision::Deny)
        );
    }

    #[test]
    fn unresolvable_identity_falls_back_to_member() {
        // A JWT identity_id with no store, or one absent from the store, must
        // resolve to a non-admin Member — deny-by-default, never trust the
        // JWT's own admin flag.
        let caller = CallerContext {
            identity_id: Some(uuid::Uuid::new_v4().to_string()),
            is_admin: true,
            ..Default::default()
        };
        // No store wired.
        let p = gateway_principal(&caller, None);
        assert_eq!(p.role, IdentityRole::Member);
        // Store wired but missing this id.
        let store = store_with(&[]);
        let p = gateway_principal(&caller, Some(&store));
        assert_eq!(p.role, IdentityRole::Member);
        assert!(crate::authz::enforce(
            &p,
            crate::authz::Capability::AgentsManage,
            &crate::authz::CapabilityMatrix::default()
        )
        .is_err());
    }

    #[test]
    fn anonymous_caller_resolves_to_member() {
        let p = gateway_principal(&CallerContext::default(), None);
        assert_eq!(p.identity_id, None);
        assert_eq!(p.role, IdentityRole::Member);
    }

    #[test]
    fn paired_device_linked_to_admin_resolves_to_member() {
        // A device token carries the linked identity's id but must NEVER inherit
        // its admin authority on the REST surface — the device=non-admin
        // invariant the rest of the gateway already holds.
        let admin = Identity::new("admin", IdentityRole::SystemAdmin);
        let store = store_with(std::slice::from_ref(&admin));
        let caller = CallerContext {
            identity_id: Some(admin.id.to_string()),
            device_id: Some("ios-device-1".to_string()),
            ..Default::default()
        };
        let p = gateway_principal(&caller, Some(&store));
        assert_eq!(p.role, IdentityRole::Member);
        assert_eq!(
            crate::authz::enforce(
                &p,
                crate::authz::Capability::VaultSharedDelete,
                &crate::authz::CapabilityMatrix::default()
            ),
            Err(crate::authz::Decision::Deny),
            "a device linked to an admin must not pass admin capabilities"
        );
    }
}

#[cfg(test)]
mod runtime_artifact_tests {
    use super::*;
    use workforce_agent::artifacts::{
        ArtifactBody, ArtifactDescriptor, ArtifactHandle, ArtifactId, ArtifactStore, ContentType,
        FsArtifactStore,
    };

    fn caller(id: &str) -> CallerContext {
        CallerContext {
            identity_id: Some(id.to_string()),
            ..Default::default()
        }
    }

    fn descriptor(user_id: Option<&str>, session_id: Option<&str>) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: None,
            ttl_seconds: 3600,
            agent_id: "agent-a".into(),
            session_id: session_id.map(str::to_string),
            user_id: user_id.map(str::to_string),
            filename: None,
            media_type: None,
        }
    }

    #[test]
    fn user_match_is_allowed() {
        let d = descriptor(Some("u1"), None);
        assert!(authorize_runtime_read(&caller("u1"), &d).is_ok());
    }

    #[test]
    fn user_mismatch_is_not_found() {
        let d = descriptor(Some("u1"), None);
        assert_eq!(
            authorize_runtime_read(&caller("u2"), &d),
            Err(StatusCode::NOT_FOUND)
        );
    }

    #[test]
    fn user_owned_with_session_still_serves_for_owner() {
        // An upload stamped with both user_id and session_id is servable here:
        // user_id matches first, so the session arm never runs.
        let d = descriptor(Some("u1"), Some("sess-1"));
        assert!(authorize_runtime_read(&caller("u1"), &d).is_ok());
    }

    #[test]
    fn user_owned_with_session_denied_for_non_owner() {
        let d = descriptor(Some("u1"), Some("sess-1"));
        assert_eq!(
            authorize_runtime_read(&caller("u2"), &d),
            Err(StatusCode::NOT_FOUND)
        );
    }

    #[test]
    fn both_none_is_allowed() {
        let d = descriptor(None, None);
        assert!(authorize_runtime_read(&caller("anyone"), &d).is_ok());
    }

    #[test]
    fn session_scoped_without_user_is_not_found() {
        // The generic caller can't prove session ownership; served on the
        // session route instead. The hole to avoid: do NOT treat it as
        // owner-less/allow.
        let d = descriptor(None, Some("sess-1"));
        assert_eq!(
            authorize_runtime_read(&caller("u1"), &d),
            Err(StatusCode::NOT_FOUND)
        );
    }

    #[test]
    fn admin_gets_no_bypass() {
        // No admin concept on this byte-serving route: an admin still only
        // reads what it owns.
        let admin = CallerContext {
            is_admin: true,
            identity_id: Some("admin".into()),
            ..Default::default()
        };
        let d = descriptor(Some("someone-else"), None);
        assert_eq!(
            authorize_runtime_read(&admin, &d),
            Err(StatusCode::NOT_FOUND)
        );
    }

    async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec()
    }

    #[tokio::test]
    async fn fs_store_streams_bytes_with_disposition() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let d = ArtifactDescriptor {
            producer: "test".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: None,
            ttl_seconds: 3600,
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: Some("u1".into()),
            filename: Some("report.bin".into()),
            media_type: Some("application/octet-stream".into()),
        };
        let handle = store
            .write("agent-a", d.clone(), ArtifactBody::Bytes(vec![1, 2, 3, 4]))
            .unwrap();
        let descriptor = store.metadata("agent-a", &handle.id).unwrap().unwrap();

        let response = serve_runtime_artifact(
            store.as_ref(),
            "agent-a",
            &handle.id,
            &descriptor,
            std::time::Duration::from_secs(300),
        )
        .expect("serve");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
            Some("attachment; filename=\"report.bin\"")
        );
        assert_eq!(body_bytes(response).await, vec![1, 2, 3, 4]);
    }

    struct FakeS3;

    impl ArtifactStore for FakeS3 {
        fn write(
            &self,
            _agent_id: &str,
            _descriptor: ArtifactDescriptor,
            _body: ArtifactBody,
        ) -> eyre::Result<ArtifactHandle> {
            unreachable!("not exercised by the serving path")
        }

        fn read(&self, _agent_id: &str, _id: &ArtifactId) -> eyre::Result<ArtifactBody> {
            unreachable!("presigned path must not read bytes")
        }

        fn metadata(
            &self,
            _agent_id: &str,
            _id: &ArtifactId,
        ) -> eyre::Result<Option<ArtifactDescriptor>> {
            Ok(None)
        }

        fn purge_expired(&self) -> eyre::Result<usize> {
            Ok(0)
        }

        fn presigned_get(
            &self,
            _agent_id: &str,
            _id: &ArtifactId,
            _expiry: std::time::Duration,
        ) -> eyre::Result<Option<String>> {
            Ok(Some("https://example.invalid/presigned".to_string()))
        }
    }

    #[tokio::test]
    async fn presigned_store_redirects() {
        let id = ArtifactId::fresh();
        let response = serve_runtime_artifact(
            &FakeS3,
            "agent-a",
            &id,
            &descriptor(Some("u1"), None),
            std::time::Duration::from_secs(300),
        )
        .expect("serve");
        assert!(
            response.status().is_redirection(),
            "{:?}",
            response.status()
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("https://example.invalid/presigned")
        );
    }

    #[test]
    fn shared_session_owner_is_admin_only() {
        // A shared session's owner (`shared:{thread}`) matches no human's id, so
        // the owner-scoped read path admits only admins — members read the
        // conversation in Slack, never its raw transcript via the API.
        let member = CallerContext {
            identity_id: Some("alice".to_string()),
            ..Default::default()
        };
        assert!(!member.can_see_owner("shared:thread-1"));
        let admin = CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(admin.can_see_owner("shared:thread-1"));
    }
}

#[cfg(test)]
mod turn_control_tests {
    use super::*;

    /// The owner, an administrator's identity and another member, for every
    /// frame that names a session: `chat`, `cancel`, `permission_response`.
    #[test]
    fn a_websocket_frame_acts_only_in_its_senders_own_session() {
        use workforce_common::session::{InMemorySessionStore, SessionStore};
        let store = InMemorySessionStore::default();
        let session = store.get_or_create(IOS_CHANNEL, "alice", "agent").unwrap();
        assert!(owns_session(&store, &session.id, "alice"));
        assert!(!owns_session(&store, &session.id, "admin-identity"));
        assert!(!owns_session(&store, &session.id, "bob"));
        assert!(!owns_session(&store, "no-such-session", "alice"));
    }

    #[test]
    fn followup_injection_answers_prompts_and_forwards_everything_else() {
        assert_eq!(
            followup_injection("  Approve req-1 ", "u1", None),
            TurnControl::Approval {
                request_id: "req-1".into(),
                approved: true,
                user: "u1".into(),
                choice: None,
            }
        );
        assert!(matches!(
            followup_injection("reject req-2", "u1", None),
            TurnControl::Approval { approved: false, request_id, .. } if request_id == "req-2"
        ));
        assert_eq!(
            followup_injection("approve ", "u1", None),
            TurnControl::FollowUp {
                text: "approve ".into(),
                user: "u1".into(),
                place: None,
            },
            "no request id is a plain follow-up"
        );
        assert_eq!(
            followup_injection("approved, ship it", "u1", Some(DeskPlace::Dm)),
            TurnControl::FollowUp {
                text: "approved, ship it".into(),
                user: "u1".into(),
                place: Some(DeskPlace::Dm),
            }
        );
    }

    /// A Desk connection watching a session keeps its forwarder through
    /// every request's end, so it hears the next turn too, from any
    /// instance; the forwarder ends with the last watcher and request.
    #[test]
    fn a_watched_session_s_forwarder_outlives_its_requests() {
        let forwarders = ProgressForwarders::default();
        let (generation, lead) = forwarders.admit("s1", Join::Watch);
        assert!(lead);
        assert_eq!(forwarders.admit("s1", Join::Request), (generation, false));
        assert!(
            !forwarders.withdraw("s1", generation),
            "the watcher keeps it after the request's Done"
        );
        forwarders.set_stamp(
            "s1",
            Some(TurnStamp {
                turn_ref: Some("ref-1".into()),
                reply_id: "reply-1".into(),
            }),
        );
        assert_eq!(
            forwarders.stamp("s1").and_then(|s| s.turn_ref),
            Some("ref-1".into())
        );
        assert!(
            forwarders.unwatch("s1", generation),
            "the last watcher ends it"
        );
        assert_eq!(forwarders.stamp("s1"), None);
    }

    /// Stopping a turn on a session Desk watches silences that turn's tail
    /// and keeps the watch: the turn's end, and every turn after, is heard.
    #[test]
    fn a_stopped_turn_is_silenced_without_ending_the_watch() {
        let forwarders = ProgressForwarders::default();
        let (generation, _) = forwarders.admit("s1", Join::Watch);
        forwarders.admit("s1", Join::Request);
        forwarders.stop("s1");
        assert!(!forwarders.passes("s1", false), "the stopped turn's tail");
        assert!(forwarders.passes("s1", true), "its end");
        assert!(forwarders.passes("s1", false), "the next turn");
        forwarders.stop("s1");
        forwarders.admit("s1", Join::Request);
        assert!(forwarders.passes("s1", false), "a request made here");
        assert!(
            !forwarders.withdraw("s1", generation),
            "still watched after the stop"
        );
        assert!(forwarders.unwatch("s1", generation));
    }

    #[test]
    fn a_session_forwarder_ends_after_every_request_it_carries() {
        let forwarders = ProgressForwarders::default();
        let (generation, lead) = forwarders.admit("s1", Join::Request);
        assert!(lead);
        assert_eq!(
            forwarders.admit("s1", Join::Request),
            (generation, false),
            "joins the running forwarder"
        );
        assert!(
            !forwarders.withdraw("s1", generation),
            "one request still out"
        );
        assert!(forwarders.withdraw("s1", generation));
        let (next, lead) = forwarders.admit("s1", Join::Request);
        assert!(
            lead && next != generation,
            "a fresh forwarder after the last Done"
        );
        assert!(
            forwarders.withdraw("s1", generation),
            "a stale generation ends at once"
        );
        forwarders.stop("s1");
        assert!(
            forwarders.withdraw("s1", next),
            "a stopped session's forwarder ends"
        );
    }
}

#[cfg(test)]
mod fallback_tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn call(method: Method, path: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let app: Router = Router::new()
            .route("/api/known", get(|| async { "known" }))
            .fallback(static_fallback);
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = to_bytes(response.into_body(), 1 << 22)
            .await
            .unwrap()
            .to_vec();
        (status, content_type, body)
    }

    #[tokio::test]
    async fn an_api_or_websocket_path_no_route_matches_is_a_json_404() {
        for (method, path) in [
            (Method::GET, "/api/desk/providers"),
            (Method::POST, "/api/desk/providers/anthropic/connect"),
            (Method::DELETE, "/api/nothing/here"),
            (Method::GET, "/api"),
            (Method::GET, "/ws/elsewhere"),
        ] {
            let (status, content_type, body) = call(method.clone(), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(content_type.as_deref(), Some("application/json"), "{path}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({ "error": "not found" })
            );
        }
        let (status, _, body) = call(Method::GET, "/api/known").await;
        assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"known"[..]));
    }

    #[tokio::test]
    async fn any_other_path_keeps_the_dashboards_spa_fallback() {
        for path in ["/agents/allan", "/apiary", "/wsx", "/"] {
            let (status, content_type, _) = call(Method::GET, path).await;
            let content_type = content_type.unwrap_or_default();
            // The UI may not be built in this checkout: either way it is
            // the dashboard's answer, never the API's JSON 404.
            assert!(
                (status == StatusCode::OK && content_type.starts_with("text/html"))
                    || (status == StatusCode::NOT_FOUND && content_type == "text/plain"),
                "{path}: {status} {content_type}"
            );
        }
        let (status, _, _) = call(Method::POST, "/agents/allan").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
}

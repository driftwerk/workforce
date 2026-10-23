//! Agent-callable tools wrapping [`super::manager::McpServerManager`].
//!
//! These tools extend **the calling agent's own toolset** by
//! connecting it to upstream MCP servers — the upstream's tools
//! become available as `mcp.<server>.*` and are callable just like
//! built-in tools. They are NOT for setting up Claude Desktop or
//! any other external MCP client; the agent is the consumer.
//!
//! Surface: `mcp.list_presets` (catalog) → `mcp.install` (start
//! setup) → `mcp.authorize` (OAuth flow) → `mcp.list` / `mcp.show`
//! / `mcp.update` (introspect + refresh) → `mcp.uninstall` (remove).
//!
//! Default policy is `allow` for all — routine ops, matching the
//! posture of `projects.*`. Operators override with a higher-
//! priority deny / require_approval rule in their policy YAML.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::{McpAuthConfig, McpServerConfig};
use workforce_mcp_store::InstalledVia;

use super::manager::{InstallReport, McpServerManager};
use super::presets;

/// Register the manager tools on `tools`.
///
/// `public_url` is the configured `[hub].public_url`; passed
/// through so the catalog + install responses can embed a
/// fully-resolved OAuth callback URL in the per-preset setup
/// walkthrough.
pub fn register_manager_tools(
    manager: Arc<McpServerManager>,
    tools: Arc<ToolRegistry>,
    public_url: Option<String>,
) {
    let public_url = public_url.map(Arc::new);
    tools.register(Arc::new(McpListPresetsTool {
        public_url: public_url.clone(),
    }));
    tools.register(Arc::new(McpListTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpShowTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpInstallTool {
        manager: Arc::clone(&manager),
        public_url: public_url.clone(),
    }));
    tools.register(Arc::new(McpUninstallTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpUpdateTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpSetToolsTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpListRemoteToolsTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpAuthorizeTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpWorkspaceAddTool {
        manager: Arc::clone(&manager),
    }));
    tools.register(Arc::new(McpWorkspaceListTool));
}

// ── mcp.list_presets ──────────────────────────────────────────

struct McpListPresetsTool {
    public_url: Option<Arc<String>>,
}

#[async_trait]
impl Tool for McpListPresetsTool {
    fn name(&self) -> &str {
        "mcp.list_presets"
    }
    fn description(&self) -> &str {
        "List the curated MCP-server presets this agent can connect to. Use this when a user asks 'what can you connect to?' or 'how do I set up <vendor>?'. Each preset includes a `setup_steps` walkthrough — read those aloud verbatim to guide the user from nothing to a working install. The tools the connected server exposes will become available to YOU (this agent) as `mcp.<server>.*`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let public_url = self.public_url.as_deref().map(String::as_str);
        let entries: Vec<Value> = presets::ALL
            .iter()
            .map(|p| {
                let suggested_redirect_uri = public_url.and_then(|base| {
                    p.oauth_provider_name().map(|provider| {
                        format!(
                            "{}/api/auth/{provider}/callback",
                            base.trim_end_matches('/')
                        )
                    })
                });
                json!({
                    "id": p.id,
                    "label": p.label,
                    "endpoint": p.endpoint,
                    "auth_kind": p.auth_kind(),
                    "required_vault_paths": p.required_vault_paths(),
                    "oauth_app_registration_url": p.oauth_app_registration_url(),
                    "suggested_redirect_uri": suggested_redirect_uri,
                    "setup_steps": p.setup_steps(public_url),
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({"presets": entries}),
            started.elapsed().as_millis() as u64,
        ))
    }
}

// ── mcp.list ───────────────────────────────────────────────────

struct McpListTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpListTool {
    fn name(&self) -> &str {
        "mcp.list"
    }
    fn description(&self) -> &str {
        "List MCP servers currently connected to **this agent** — their endpoint, auth kind, and tool count. Use this before mcp.install to avoid duplicates. Tools from each server are exposed to you as `mcp.<server>.*`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let summaries = self.manager.list();
        Ok(ToolResult::success(
            json!({"servers": summaries}),
            started.elapsed().as_millis() as u64,
        ))
    }
}

// ── mcp.show ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ShowInput {
    server: String,
}

struct McpShowTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpShowTool {
    fn name(&self) -> &str {
        "mcp.show"
    }
    fn description(&self) -> &str {
        "Show full detail for one MCP server connected to this agent — config, summary, install provenance, and the `mcp.<server>.*` tool count."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {
                "server": {"type": "string", "description": "server_id (e.g. \"linear\")"}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: ShowInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let result = self.manager.show(&parsed.server);
        let elapsed = started.elapsed().as_millis() as u64;
        match result {
            Ok(Some(detail)) => Ok(ToolResult::success(json!(detail), elapsed)),
            Ok(None) => Ok(ToolResult::failure(
                format!("MCP server `{}` is not installed", parsed.server),
                elapsed,
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed)),
        }
    }
}

// ── mcp.install ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InstallInput {
    /// `"linear" | "github" | "atlassian"`.  Mutually exclusive with `custom`.
    #[serde(default)]
    preset: Option<String>,
    /// Custom server config (use any vendor's MCP endpoint).
    #[serde(default)]
    custom: Option<CustomInstall>,
    /// Optional override for the server_id key. Defaults to the
    /// preset name; custom installs require it.
    #[serde(default)]
    server_id: Option<String>,
    /// When the calling user has no token for this MCP server, fall
    /// back to the shadowed built-in skill. `None` keeps the preset
    /// / custom default of `false`.
    #[serde(default)]
    fallback_to_builtin: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CustomInstall {
    endpoint: String,
    #[serde(default = "default_transport")]
    transport: String,
    auth: McpAuthConfig,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default = "default_refresh")]
    refresh_interval_secs: u64,
    #[serde(default)]
    shadows: Vec<String>,
    #[serde(default)]
    fallback_to_builtin: bool,
    /// Glob patterns (`*`, `?`, `[...]`) matched against each upstream
    /// tool's raw name. Non-empty = allowlist: only matching tools are
    /// registered, curating a large server's surface so fewer schemas
    /// load into every request. Empty = register all.
    #[serde(default)]
    tools_allow: Vec<String>,
    /// Glob patterns whose matches are dropped even if they matched
    /// `tools_allow` (deny wins). Subtract a few tools from an allowed
    /// surface, e.g. allow `issue_*`, deny `issue_delete`.
    #[serde(default)]
    tools_deny: Vec<String>,
    /// Run OAuth 2.1 / OIDC discovery (RFC 8414 + RFC 9728) against
    /// `endpoint` to resolve the authorization/token endpoints
    /// automatically, instead of relying on a built-in provider or a
    /// hand-written `discovery` block. Use for any OAuth-protected
    /// server that isn't a preset (Keycloak / Okta / Auth0 …).
    /// Requires `auth.kind = "oauth2"` and `[hub].public_url`.
    #[serde(default)]
    discover: bool,
    /// OIDC issuer / discovery base URL, when it differs from `endpoint`.
    /// Keycloak and similar front the MCP API on one host
    /// (`api.example/mcp`) but serve their `.well-known` on the realm
    /// (`auth.example/realms/x`). Set this to that realm URL so
    /// `discover: true` resolves the endpoints from there. Omit when the
    /// MCP server serves its own discovery doc.
    #[serde(default)]
    issuer: Option<String>,
}

fn default_transport() -> String {
    "streamable-http".to_string()
}
fn default_refresh() -> u64 {
    3600
}

struct McpInstallTool {
    manager: Arc<McpServerManager>,
    public_url: Option<Arc<String>>,
}

#[async_trait]
impl Tool for McpInstallTool {
    fn name(&self) -> &str {
        "mcp.install"
    }
    fn description(&self) -> &str {
        "Connect this agent to an upstream MCP server. The server's tools become available to YOU as `mcp.<server>.*` and are callable just like built-in tools — install is hot, no hub restart. Use `preset` for one of the curated catalog entries (linear, github, atlassian) or `custom` for any URL.\n\nFor an OAuth/OIDC-protected server that isn't a preset (Keycloak/Okta/Auth0/etc.), pass `custom` with `auth.kind=\"oauth2\"` and `discover: true` — the hub resolves the OAuth endpoints AND registers a client itself. Two things to get right:\n• ISSUER: discovery runs against `endpoint` by default. If that's just the MCP API and discovery fails (no `.well-known` there), the OIDC issuer is on a different host — ASK the user for the issuer/realm URL (Keycloak looks like `https://auth.<host>/realms/<realm>`) and retry with `issuer` set to it.\n• SCOPES: set `auth.scopes`. If the user doesn't specify, SUGGEST a sensible default and confirm — for OIDC/Keycloak that's typically `[\"openid\",\"profile\",\"email\",\"offline_access\"]` (offline_access enables token refresh).\nIf you can't discover and can't get an issuer, you can instead skip discovery (omit `discover`) and provide endpoints directly in `auth.discovery` ({authorization_endpoint, token_endpoint}), but that path needs a pre-seeded OAuth client in the vault.\n\nIf credentials need seeding, the response includes a structured `setup_steps` walkthrough — read those to the user verbatim, then call mcp.install again once they're done. (If you're unsure which preset fits, call mcp.list_presets first.)"
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "preset": {"type": "string", "enum": ["linear", "github", "atlassian"]},
                "server_id": {"type": "string"},
                "fallback_to_builtin": {
                    "type": "boolean",
                    "description": "When the calling user has no token for this MCP server, fall back to the shadowed built-in skill if it is loaded. Default false."
                },
                "custom": {
                    "type": "object",
                    "required": ["endpoint", "auth"],
                    "properties": {
                        "endpoint": {"type": "string"},
                        "transport": {"type": "string", "default": "streamable-http"},
                        "auth": {"type": "object"},
                        "allowed_hosts": {"type": "array", "items": {"type": "string"}},
                        "refresh_interval_secs": {"type": "integer", "minimum": 60},
                        "shadows": {"type": "array", "items": {"type": "string"}},
                        "fallback_to_builtin": {"type": "boolean"},
                        "tools_allow": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns (*, ?, [...]) matched against each upstream tool's raw name. Non-empty = allowlist: only matching tools are registered, so fewer schemas load into every request on a large server. Empty = register all."},
                        "tools_deny": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns whose matches are dropped even if matched by tools_allow (deny wins). E.g. allow [\"issue_*\"], deny [\"issue_delete\"]."},
                        "discover": {"type": "boolean", "description": "Run OAuth 2.1/OIDC discovery (RFC 8414/9728) against `endpoint` (or `issuer` if set) to auto-resolve the authorization/token endpoints AND auto-register an OAuth client. Use for any OAuth-protected server that isn't a preset (Keycloak/Okta/Auth0). Requires auth.kind=\"oauth2\" and [hub].public_url. Default false."},
                        "issuer": {"type": "string", "description": "OIDC issuer / discovery base URL, when it differs from `endpoint`. Keycloak fronts the MCP API on one host (api.example/mcp) but serves .well-known on the realm (auth.example/realms/x) — set that realm URL here so discover:true resolves from the right place. If discovery fails because the endpoint serves no .well-known doc, ask the user for this issuer URL and retry."}
                    }
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: InstallInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;

        let (server_id, mut config, preset, discover, issuer) =
            match (parsed.preset.as_deref(), parsed.custom) {
                (Some(p), None) => {
                    let Some(preset) = presets::lookup(p) else {
                        return Ok(ToolResult::failure(
                            format!(
                                "unknown preset `{p}`; known: {}",
                                presets::ALL
                                    .iter()
                                    .map(|x| x.id)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            elapsed(),
                        ));
                    };
                    let id = parsed.server_id.unwrap_or_else(|| preset.id.to_string());
                    (id, preset.to_config(), Some(preset), false, None)
                }
                (None, Some(custom)) => {
                    let id = match parsed.server_id {
                        Some(id) => id,
                        None => {
                            return Ok(ToolResult::failure(
                                "custom install requires `server_id`",
                                elapsed(),
                            ));
                        }
                    };
                    let discover = custom.discover;
                    let issuer = custom.issuer;
                    let cfg = McpServerConfig {
                        endpoint: custom.endpoint,
                        transport: custom.transport,
                        auth: custom.auth,
                        allowed_hosts: custom.allowed_hosts,
                        refresh_interval_secs: custom.refresh_interval_secs,
                        shadows: custom.shadows,
                        fallback_to_builtin: custom.fallback_to_builtin,
                        tools_allow: custom.tools_allow,
                        tools_deny: custom.tools_deny,
                        // Ungrouped on install; group later via mcp.workspace_add.
                        workspace: None,
                        env: None,
                        default: false,
                    };
                    (id, cfg, None, discover, issuer)
                }
                _ => {
                    return Ok(ToolResult::failure(
                        "specify exactly one of `preset` or `custom`",
                        elapsed(),
                    ));
                }
            };
        if let Some(fb) = parsed.fallback_to_builtin {
            config.fallback_to_builtin = fb;
        }

        let public_url = self.public_url.as_deref().map(String::as_str);
        // Discovery-based OAuth install: curated dynamic presets carry a
        // templated endpoint (`endpoint_var`); `custom` + `discover` runs the
        // same path against a fixed endpoint. Both resolve the OAuth endpoints
        // from the server's well-knowns and reuse any operator-seeded client.
        let install = if let Some(p) = preset.filter(|p| p.is_dynamic()) {
            self.manager
                .install_dynamic(
                    &server_id,
                    config,
                    p.endpoint_var.map(|v| v.vault_path),
                    public_url,
                    None,
                    InstalledVia::Agent,
                )
                .await
        } else if discover {
            self.manager
                .install_dynamic(
                    &server_id,
                    config,
                    None,
                    public_url,
                    issuer.as_deref(),
                    InstalledVia::Agent,
                )
                .await
        } else {
            self.manager
                .install(&server_id, config, InstalledVia::Agent)
                .await
        };
        match install {
            Ok(report) => {
                let report = report.with_preset_setup_steps(preset.map(|p| p.id), public_url);
                Ok(ToolResult::success(install_report_json(&report), elapsed()))
            }
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

fn install_report_json(report: &InstallReport) -> Value {
    serde_json::to_value(report).expect("InstallReport is plain serde-derive — infallible")
}

// ── mcp.uninstall ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UninstallInput {
    server: String,
    #[serde(default)]
    purge_tokens: bool,
}

struct McpUninstallTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpUninstallTool {
    fn name(&self) -> &str {
        "mcp.uninstall"
    }
    fn description(&self) -> &str {
        "Disconnect an MCP server from this agent. Stops the refresh task, removes every `mcp.<server>.*` tool from your toolset, drops the persistence row. Pass `purge_tokens: true` to also wipe per-user OAuth tokens (operator-supplied client credentials are preserved)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {
                "server": {"type": "string"},
                "purge_tokens": {"type": "boolean", "default": false}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: UninstallInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        match self
            .manager
            .uninstall(&parsed.server, parsed.purge_tokens)
            .await
        {
            Ok(report) => Ok(ToolResult::success(json!(report), elapsed())),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.update ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UpdateInput {
    server: String,
}

struct McpUpdateTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpUpdateTool {
    fn name(&self) -> &str {
        "mcp.update"
    }
    fn description(&self) -> &str {
        "Re-fetch tools/list from one MCP server connected to this agent and reconcile your `mcp.<server>.*` toolset against it. Use after the vendor adds new tools, instead of waiting for the next periodic refresh tick."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {"server": {"type": "string"}},
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: UpdateInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        match self.manager.refresh(&parsed.server).await {
            Ok(report) => Ok(ToolResult::success(json!(report), elapsed())),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.set_tools ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SetToolsInput {
    server: String,
    /// Glob patterns (`*`, `?`, `[...]`) matched against each upstream
    /// tool's raw name. Non-empty = allowlist: only matching tools stay
    /// registered. Empty (with empty `tools_deny`) clears the filter and
    /// re-registers the server's full surface.
    #[serde(default)]
    tools_allow: Vec<String>,
    /// Glob patterns whose matches are dropped even if matched by
    /// `tools_allow` (deny wins).
    #[serde(default)]
    tools_deny: Vec<String>,
}

struct McpSetToolsTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpSetToolsTool {
    fn name(&self) -> &str {
        "mcp.set_tools"
    }
    fn description(&self) -> &str {
        "Curate which of an installed MCP server's tools stay registered, applied immediately (hot, no restart). Review the server's `mcp.<server>.*` tools (they're already in your available toolset, and the install report lists them by name), decide which to keep, then pass `tools_allow` (an allowlist of glob patterns over each tool's raw upstream name — the part after `mcp.<server>.`, e.g. [\"issue_*\", \"*_get\"]) and/or `tools_deny` (subtract specific tools; deny wins). This re-applies on the already-installed server using its stored endpoint/auth — no re-authorization. For an OAuth server it respawns under the calling user's credentials, so you must be authorized to it (a failed respawn briefly drops the server). Use it to shrink a large server (hundreds of tools) down to the handful the agent actually needs, so fewer schemas load into every request. The response's `registered_tools` confirms what remains. Pass both lists empty to clear the filter and restore the full surface."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {
                "server": {"type": "string", "description": "server_id of an already-installed MCP server (e.g. \"harbor-staging\")."},
                "tools_allow": {"type": "array", "items": {"type": "string"}, "description": "Allowlist glob patterns over raw upstream tool names. Non-empty = only matching tools registered. Empty = allow all (then apply deny)."},
                "tools_deny": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns whose matches are dropped even if matched by tools_allow."}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: SetToolsInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        match self
            .manager
            .set_tool_filter(&parsed.server, parsed.tools_allow, parsed.tools_deny)
            .await
        {
            Ok(report) => Ok(ToolResult::success(json!(report), elapsed())),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.list_remote_tools ──────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListRemoteToolsInput {
    server: String,
    /// Optional substring to drill into a family and list matching names.
    #[serde(default)]
    contains: Option<String>,
}

struct McpListRemoteToolsTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpListRemoteToolsTool {
    fn name(&self) -> &str {
        "mcp.list_remote_tools"
    }
    fn description(&self) -> &str {
        "Survey an installed MCP server's FULL tool surface WITHOUT registering anything — the discovery step before curating a large server with mcp.set_tools. Returns a family summary: tool-name prefixes with counts (e.g. shift_* (124), worker_* (89)), so you can see the categories even for a server with thousands of tools, and even one that failed to hydrate because it exceeds the tool cap. Pass `contains` to drill into one family and get the actual tool names (e.g. contains=\"shift\" → shift_create, shift_cancel, …). `total_tools` is the full count; `truncated: true` means the server is larger than the survey limit and the list is a floor. Workflow: list_remote_tools (review families) → list_remote_tools contains=\"X\" (see names) → mcp.set_tools tools_allow=[\"X_*\", …] (register the curated subset, applied hot)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {
                "server": {"type": "string", "description": "server_id of an installed MCP server (e.g. \"harbor-staging\")."},
                "contains": {"type": "string", "description": "Optional substring; when given, returns the names of tools whose name contains it (drill into a family) instead of just the family summary."}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: ListRemoteToolsInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        match self
            .manager
            .probe_remote_tools(&parsed.server, parsed.contains.as_deref())
            .await
        {
            Ok(survey) => Ok(ToolResult::success(json!(survey), elapsed())),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.authorize ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AuthorizeInput {
    server: String,
}

struct McpAuthorizeTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpAuthorizeTool {
    fn name(&self) -> &str {
        "mcp.authorize"
    }
    fn description(&self) -> &str {
        "Start an OAuth web flow for an MCP server connected to this agent. Returns the authorization URL the user should open in a browser; once they consent, tokens persist and the server's `mcp.<server>.*` tools become callable. Call this after mcp.install reports `needs_credentials` and the user has seeded the vault paths from the `setup_steps` walkthrough."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server"],
            "properties": {"server": {"type": "string"}},
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: AuthorizeInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        let uid = current_user_id().unwrap_or_default();
        if uid.is_empty() {
            return Ok(ToolResult::failure(
                "mcp.authorize requires an active user context — call from a Slack thread or `/cc` chat, not a workflow",
                elapsed(),
            ));
        }
        match self.manager.authorize_start(&parsed.server, &uid).await {
            Ok(start) => Ok(ToolResult::success(json!(start), elapsed())),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.workspace_add ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorkspaceAddInput {
    server: String,
    workspace: String,
    /// The variant label within the workspace (`staging`/`prod`/…). Accepts the
    /// legacy `env` key so older callers keep working after the rename.
    #[serde(alias = "env")]
    variant: String,
}

struct McpWorkspaceAddTool {
    manager: Arc<McpServerManager>,
}

#[async_trait]
impl Tool for McpWorkspaceAddTool {
    fn name(&self) -> &str {
        "mcp.workspace_add"
    }
    fn description(&self) -> &str {
        "Group an installed MCP server into a variant-gated WORKSPACE so it stops being auto-reached. A workspace (e.g. \"harbor\") holds one server per VARIANT (e.g. \"staging\", \"prod\"); only ONE variant is reachable at a time. Adding a server makes it DORMANT — its tools are hidden and not callable until that variant becomes active, which happens only when (a) a human runs `cmd: use <workspace> <variant>` for their session, or (b) the variant is the workspace default. This tool always adds as non-default; you CANNOT make a variant the default or activate one yourself (both are human-only `cmd:` actions), because pointing a prod investigation at staging is exactly the mistake gating prevents. Use this to put a staging/demo server behind the gate. To make a variant the default, tell the human to run `cmd: mcp default <workspace> <variant>`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["server", "workspace", "variant"],
            "properties": {
                "server": {"type": "string", "description": "server_id of an already-installed MCP server (e.g. \"harbor-staging\")."},
                "workspace": {"type": "string", "description": "Workspace (group) name, e.g. \"harbor\". Created implicitly if new."},
                "variant": {"type": "string", "description": "Variant label within the workspace, e.g. \"staging\", \"prod\", \"demo\". (The legacy key `env` is still accepted.)"}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: WorkspaceAddInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), 0)),
        };
        let elapsed = || started.elapsed().as_millis() as u64;
        match self.manager.set_workspace_membership(
            &parsed.server,
            Some(&parsed.workspace),
            Some(&parsed.variant),
        ) {
            Ok(()) => Ok(ToolResult::success(
                json!({
                    "server": parsed.server,
                    "workspace": parsed.workspace,
                    "variant": parsed.variant,
                    "default": false,
                    "state": "dormant",
                    "note": format!(
                        "`{}` is now the `{}` member of workspace `{}` and is DORMANT. \
                         To make it the always-on default, a human runs \
                         `cmd: mcp default {} {}`. To point just their session at it, a human \
                         runs `cmd: use {} {}`.",
                        parsed.server, parsed.variant, parsed.workspace,
                        parsed.workspace, parsed.variant, parsed.workspace, parsed.variant
                    ),
                }),
                elapsed(),
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed())),
        }
    }
}

// ── mcp.workspace_list ─────────────────────────────────────────

struct McpWorkspaceListTool;

#[async_trait]
impl Tool for McpWorkspaceListTool {
    fn name(&self) -> &str {
        "mcp.workspace_list"
    }
    fn description(&self) -> &str {
        "List MCP workspaces and, for each, which variant is currently ACTIVE for this session and which are DORMANT. Use this to see what variants exist (e.g. a staging server you could offer to check) and which one your gated MCP tools are hitting right now. You cannot switch — a human runs `cmd: use <workspace> <variant>` to change the active variant for the session, or `cmd: mcp default <workspace> <variant>` to change the persistent default. For a fuller picture (including installed-but-unauthorised servers and the fix), call `env.status`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let views: Vec<Value> = workforce_agent::llm::mcp_workspace::workspace_summary()
            .into_iter()
            .map(|v| {
                json!({
                    "workspace": v.workspace,
                    "active_variant": v.active_env,
                    "members": v.members.into_iter().map(|m| json!({
                        "variant": m.env,
                        "server": m.server_id,
                        "default": m.default,
                        "active": m.active,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({"workspaces": views}),
            started.elapsed().as_millis() as u64,
        ))
    }
}

fn current_user_id() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// Default policies for the six manager tools — all `allow` by
/// default, matching the `projects.*` posture for routine ops.
/// Operators can add a higher-priority deny / require_approval
/// rule in their policy YAML.
pub fn default_policies() -> Vec<workforce_policy::rules::PolicyRule> {
    use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

    const TOOL_NAMES: &[&str] = &[
        "mcp.list",
        "mcp.list_presets",
        "mcp.show",
        "mcp.install",
        "mcp.uninstall",
        "mcp.update",
        "mcp.authorize",
        "mcp.workspace_add",
        "mcp.workspace_list",
    ];
    TOOL_NAMES
        .iter()
        .map(|tool| PolicyRule {
            name: format!("{tool}-allow-default"),
            scope: PolicyScope::Global,
            tool: (*tool).to_string(),
            action: RuleAction::Allow,
            conditions: Vec::new(),
            priority: 10,
            constitutional: true,
        })
        .collect()
}

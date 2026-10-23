//! `McpServer` (the live, in-memory representation of one
//! configured MCP server) and `spawn_all` — the entry point
//! `serve.rs` calls at startup to build the registry from
//! `WorkforceConfig.mcp.servers`.

use std::sync::Arc;

use eyre::{eyre, Result};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use url::Url;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::Tool;
use workforce_common::cost::CostTracker;
use workforce_common::settings::{McpAuthBinding, McpAuthConfig, McpServerConfig, WorkforceConfig};
use workforce_mcp_client::{
    AuthProvider, McpClient, McpError, McpLimits, McpToolDef, DEFAULT_MAX_CONTENT_BYTES,
};

use super::auth::{LayeredAuth, PatAuth, TokenStoreAuth};
use super::discovery::DiscoveryIdentity;
use super::refresh;
use super::registry::{McpServerRegistry, ServerSummary};
use super::tool::McpTool;
use super::tool_allowlist::ToolAllowlist;
use crate::user_config::LayeredUserVault;
use crate::vault::Vault;
use workforce_oauth_client::{OAuthClient, OAuthClientCreds};

/// What this server shadows and how to probe whether the active user
/// can actually reach it. Computed once at install and consulted by
/// [`super::access::McpToolAccess`] on every LLM round.
#[derive(Debug, Clone)]
pub struct ShadowPlan {
    pub patterns: Vec<String>,
    pub fallback_to_builtin: bool,
    pub token_probe: TokenProbe,
}

/// How to test whether the active user has a working credential for
/// this MCP server. Mirrors the resolution shape used by
/// [`super::auth::PatAuth`] and [`super::auth::TokenStoreAuth`] so
/// the visibility check stays in step with the actual auth path.
#[derive(Debug, Clone)]
pub enum TokenProbe {
    Pat { vault_path: String },
    Oauth { binding: McpAuthBinding },
}

impl ShadowPlan {
    fn from_config(cfg: &McpServerConfig) -> Self {
        let token_probe = match &cfg.auth {
            McpAuthConfig::Pat { vault_path } => TokenProbe::Pat {
                vault_path: vault_path.clone(),
            },
            McpAuthConfig::Oauth2 { binding, .. } => TokenProbe::Oauth { binding: *binding },
        };
        Self {
            patterns: cfg.shadows.clone(),
            fallback_to_builtin: cfg.fallback_to_builtin,
            token_probe,
        }
    }
}

/// One configured MCP server kept alive while installed. Holds the
/// `McpClient` (shared with every `McpTool` registered for this
/// server) and the `JoinHandle` for the refresh task. On uninstall
/// the manager calls [`Self::shutdown`] to abort the refresh task
/// and then drops the `Arc<McpServer>`; the underlying `McpClient`
/// stays alive until every in-flight `tools/call` finishes (Arc
/// semantics) so callers don't see torn responses.
pub struct McpServer {
    pub endpoint: String,
    pub auth_kind: &'static str,
    pub tool_count: parking_lot::Mutex<usize>,
    refresh_task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    client: Arc<McpClient>,
    shadow_plan: ShadowPlan,
    allowlist: ToolAllowlist,
}

impl McpServer {
    pub fn summary(&self, id: &str) -> ServerSummary {
        ServerSummary {
            id: id.to_string(),
            endpoint: self.endpoint.clone(),
            tool_count: *self.tool_count.lock(),
            auth_kind: self.auth_kind,
        }
    }

    pub fn shadow_plan(&self) -> &ShadowPlan {
        &self.shadow_plan
    }

    /// Per-server tool allow/deny filter. The manager applies it on an
    /// on-demand `mcp.update` refresh so a denied tool can't reappear,
    /// mirroring the initial registration and the periodic refresh tick.
    pub(super) fn allowlist(&self) -> &ToolAllowlist {
        &self.allowlist
    }

    /// Shared client. The manager uses this to issue an immediate
    /// `tools/list` from `mcp.update` without waiting for the next
    /// refresh tick.
    pub fn client(&self) -> Arc<McpClient> {
        Arc::clone(&self.client)
    }

    /// Abort the periodic refresh task. Idempotent — a second call
    /// is a no-op.
    pub fn shutdown(&self) {
        if let Some(handle) = self.refresh_task.lock().take() {
            handle.abort();
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build every server in the config, register their tools, spawn
/// their refresh loops, populate the registry. Failures on
/// individual servers are logged and skipped — the hub starts
/// regardless.
pub async fn spawn_all(
    config: &WorkforceConfig,
    agent_id: &str,
    tools: Arc<ToolRegistry>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    cost: Option<Arc<CostTracker>>,
) -> Arc<McpServerRegistry> {
    let registry = Arc::new(McpServerRegistry::new());
    if config.mcp.servers.is_empty() {
        return registry;
    }
    info!(
        count = config.mcp.servers.len(),
        "starting MCP servers from [mcp.servers.*] config"
    );

    let http = Arc::new(reqwest::Client::new());
    for (id, server_config) in &config.mcp.servers {
        match start_one(
            id,
            agent_id,
            server_config,
            Arc::clone(&http),
            Arc::clone(&tools),
            agent_vault.clone(),
            global_vault.clone(),
            cost.clone(),
        )
        .await
        {
            Ok(server) => {
                registry.insert(id, Arc::new(server));
            }
            Err(e) => {
                error!(server = %id, error = %e, "MCP server initialise failed; skipping");
            }
        }
    }
    registry
}

/// Validate config, build the auth provider + discovery identity, and
/// construct an (un-initialised) [`McpClient`]. Shared by [`start_one`]
/// and the discovery probe (`McpServerManager::probe_remote_tools`) so
/// both reach a server through exactly the same auth/host-allow path.
pub(super) fn build_client(
    id: &str,
    cfg: &McpServerConfig,
    http: Arc<reqwest::Client>,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> Result<(Arc<McpClient>, DiscoveryIdentity)> {
    if cfg.transport != "streamable-http" {
        return Err(eyre!(
            "unsupported MCP transport `{}` for server `{id}` (only \"streamable-http\" is supported in v1)",
            cfg.transport
        ));
    }
    let endpoint = Url::parse(&cfg.endpoint)
        .map_err(|e| eyre!("invalid endpoint URL for server `{id}`: {e}"))?;
    if !cfg.allowed_hosts.is_empty() {
        let host = endpoint
            .host_str()
            .ok_or_else(|| eyre!("server `{id}` endpoint has no host"))?;
        if !cfg.allowed_hosts.iter().any(|h| h == host) {
            return Err(eyre!(
                "server `{id}` endpoint host `{host}` is not in [allowed_hosts]"
            ));
        }
    }
    let auth = build_auth(id, &cfg.auth, &http, agent_vault, global_vault)?;
    let discovery = DiscoveryIdentity::from_config(id, &cfg.auth, global_vault.clone());
    let client = Arc::new(McpClient::new(
        endpoint,
        http,
        auth,
        McpLimits {
            max_content_bytes: DEFAULT_MAX_CONTENT_BYTES,
            ..McpLimits::default()
        },
    ));
    Ok((client, discovery))
}

/// Turn a `tools/list` error into operator-facing guidance. The over-cap
/// case is actionable — review + curate — so it gets a pointed message
/// instead of a generic failure; everything else passes through.
fn map_tools_list_err(id: &str, e: McpError) -> eyre::Report {
    match &e {
        McpError::ToolCountExceeded { cap, .. } => eyre!(
            "server `{id}` advertises more than the {cap}-tool registration cap, \
             so none were registered. Review its surface with `mcp.list_remote_tools` \
             and curate a subset with `mcp.set_tools` (tools_allow), or trim the server."
        ),
        McpError::RawToolLimitExceeded { limit } => eyre!(
            "server `{id}` advertised more than {limit} raw tools and was refused. \
             This is far beyond any sane server; check the endpoint is correct."
        ),
        _ => eyre!("tools/list failed for server `{id}`: {e}"),
    }
}

/// Initialise one MCP server, register its tools, spawn its refresh
/// task. Reused by both the legacy `spawn_all` startup path and by
/// `McpServerManager::install` for runtime additions.
#[allow(clippy::too_many_arguments)]
pub(super) async fn start_one(
    id: &str,
    agent_id: &str,
    cfg: &McpServerConfig,
    http: Arc<reqwest::Client>,
    tools: Arc<ToolRegistry>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    cost: Option<Arc<CostTracker>>,
) -> Result<McpServer> {
    let (client, discovery) = build_client(id, cfg, http, &agent_vault, &global_vault)?;

    // Curate the surface at FETCH time so a large server's tools never all
    // land in the registry (nor inline in every large-context request),
    // and — crucially — so a server over the registration cap can still
    // register its allowed subset (the cap counts only kept tools). The
    // same `keep` predicate runs in the refresh loop and on `mcp.update`,
    // so a denied tool can't slip back in.
    let allowlist = ToolAllowlist::from_config(id, &cfg.tools_allow, &cfg.tools_deny);

    let (init, upstream_tools) = discovery
        .run(async {
            let init = client
                .initialize()
                .await
                .map_err(|e| eyre!("initialize failed for server `{id}`: {e}"))?;
            let tools = client
                .tools_list_filtered(|n| allowlist.keeps_name(n))
                .await
                .map_err(|e| map_tools_list_err(id, e))?;
            Ok::<_, eyre::Report>((init, tools))
        })
        .await?;
    info!(
        server = %id,
        protocol_version = %init.protocol_version,
        server_name = ?init.server_info.as_ref().map(|i| &i.name),
        tools = upstream_tools.len(),
        filtered = !allowlist.is_noop(),
        "MCP server initialised"
    );

    let tool_count = register_tools(id, agent_id, &client, &tools, &upstream_tools, &cost);
    // Record (or clear) this server's MCP-workspace membership so the tool
    // filter + call guard know whether it's gated. A server with `workspace`
    // set is dormant until its env is the workspace's active one; without it
    // the server is ungrouped and always active (clears any stale membership
    // from a prior grouping when the server is respawned after re-grouping).
    match (&cfg.workspace, &cfg.env) {
        (Some(ws), env) => workforce_agent::llm::mcp_workspace::set_member(
            id,
            ws,
            env.as_deref().unwrap_or_default(),
            cfg.default,
        ),
        (None, _) => workforce_agent::llm::mcp_workspace::clear_member(id),
    }
    if !cfg.shadows.is_empty() {
        info!(
            server = %id,
            patterns = ?cfg.shadows,
            fallback = cfg.fallback_to_builtin,
            "MCP server registered shadow plan; per-user resolution in McpToolAccess"
        );
    }

    let refresh_task = refresh::spawn(
        id.to_string(),
        agent_id.to_string(),
        Arc::clone(&client),
        Arc::clone(&tools),
        upstream_tools,
        cfg.refresh_interval_secs,
        cost,
        discovery,
        allowlist.clone(),
    );

    Ok(McpServer {
        endpoint: cfg.endpoint.clone(),
        auth_kind: auth_kind_label(&cfg.auth),
        tool_count: parking_lot::Mutex::new(tool_count),
        refresh_task: parking_lot::Mutex::new(Some(refresh_task)),
        client,
        shadow_plan: ShadowPlan::from_config(cfg),
        allowlist,
    })
}

fn build_auth(
    id: &str,
    auth: &McpAuthConfig,
    http: &Arc<reqwest::Client>,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> Result<Arc<dyn AuthProvider>> {
    let layered = layered_store(agent_vault, global_vault).ok_or_else(|| {
        eyre!("server `{id}` requires at least one vault (agent or global) for auth")
    })?;
    match auth {
        McpAuthConfig::Pat { vault_path } => {
            build_pat_auth(id, vault_path, agent_vault, global_vault)
        }
        McpAuthConfig::Oauth2 {
            binding,
            provider,
            shared_pat_fallback,
            discovery,
            ..
        } => {
            let oauth = build_oauth_auth(
                id,
                *binding,
                provider,
                discovery.as_ref(),
                http,
                &layered,
                global_vault,
            )?;
            match shared_pat_fallback {
                Some(path) => Ok(Arc::new(LayeredAuth::new(vec![
                    oauth,
                    build_pat_auth(id, path, agent_vault, global_vault)?,
                ]))),
                None => Ok(oauth),
            }
        }
    }
}

/// Build the layered `SecretStore` used by MCP auth and OAuth client-cred
/// reads. Returns `None` when neither vault is configured (system has no
/// secrets store at all).
pub(super) fn layered_store(
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> Option<Arc<dyn workforce_common::settings::SecretStore>> {
    agent_vault.clone().map(|agent| {
        let layered: Arc<dyn workforce_common::settings::SecretStore> =
            Arc::new(LayeredUserVault::new(agent, global_vault.clone()));
        layered
    })
}

fn build_pat_auth(
    id: &str,
    vault_path: &str,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> Result<Arc<dyn AuthProvider>> {
    let agent = agent_vault
        .clone()
        .ok_or_else(|| eyre!("server `{id}` requires a vault for PAT auth"))?;
    let layered = LayeredUserVault::new(agent, global_vault.clone());
    Ok(Arc::new(PatAuth::new(layered, vault_path)))
}

fn build_oauth_auth(
    id: &str,
    binding: McpAuthBinding,
    provider: &str,
    discovery: Option<&workforce_common::settings::DiscoveredMetadata>,
    http: &Arc<reqwest::Client>,
    layered: &Arc<dyn workforce_common::settings::SecretStore>,
    global_vault: &Option<Arc<Vault>>,
) -> Result<Arc<dyn AuthProvider>> {
    let global = global_vault
        .clone()
        .ok_or_else(|| eyre!("server `{id}` requires a global vault for OAuth auth"))?;
    let creds = oauth_creds_from_vault(id, layered.as_ref())?;
    let provider_impl = super::oauth_bridge::provider_for_config(provider, discovery)?;
    let oauth_client = Arc::new(OAuthClient::new(
        provider_impl,
        creds,
        reqwest::Client::clone(http),
    ));
    Ok(Arc::new(TokenStoreAuth::new(
        id,
        binding,
        oauth_client,
        global,
    )))
}

/// Read OAuth client credentials for an MCP server (`mcp/v1/<id>/client_id`,
/// `client_secret`, `redirect_uri`). Resolution goes through the layered
/// store: a per-agent override (T3 `agent_specific`) wins over the org-wide
/// default (T4 `global`). T1/T2 (per-user) are skipped — an OAuth client
/// belongs to the agent or to the org, never to one human.
///
/// `client_secret` is optional — public clients authenticate via PKCE alone.
pub(super) fn oauth_creds_from_vault(
    id: &str,
    store: &dyn workforce_common::settings::SecretStore,
) -> Result<OAuthClientCreds> {
    let base = format!("mcp/v1/{id}");
    let pick = |path: String| {
        store
            .inspect(&path)
            .into_iter()
            .find(|(t, _)| {
                matches!(
                    t,
                    workforce_common::settings::VaultSource::AgentSpecific
                        | workforce_common::settings::VaultSource::Global
                )
            })
            .map(|(_, v)| v)
    };
    let client_id = pick(format!("{base}/client_id"))
        .ok_or_else(|| eyre!("missing OAuth credential at vault path `{base}/client_id`"))?;
    Ok(OAuthClientCreds {
        client_id,
        client_secret: pick(format!("{base}/client_secret")),
        redirect_uri: pick(format!("{base}/redirect_uri")),
    })
}

fn auth_kind_label(auth: &McpAuthConfig) -> &'static str {
    match auth {
        McpAuthConfig::Pat { .. } => "pat",
        McpAuthConfig::Oauth2 {
            shared_pat_fallback: Some(_),
            ..
        } => "oauth2+pat",
        McpAuthConfig::Oauth2 { .. } => "oauth2",
    }
}

fn register_tools(
    server_id: &str,
    agent_id: &str,
    client: &Arc<McpClient>,
    registry: &Arc<ToolRegistry>,
    upstream: &[McpToolDef],
    cost: &Option<Arc<CostTracker>>,
) -> usize {
    let mut registered = 0usize;
    for def in upstream {
        let tool = McpTool::new(
            server_id,
            agent_id,
            Arc::clone(client),
            def.clone(),
            cost.clone(),
        );
        let name = tool.name().to_string();
        if registry.get(&name).is_some() {
            warn!(
                server = %server_id,
                tool = %name,
                "MCP tool name collides with an existing tool; skipping registration"
            );
            continue;
        }
        registry.register(Arc::new(tool));
        registered += 1;
    }
    info!(server = %server_id, registered, "MCP tools registered");
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    fn layered(
        agent: Arc<Vault>,
        global: Option<Arc<Vault>>,
    ) -> Arc<dyn workforce_common::settings::SecretStore> {
        Arc::new(LayeredUserVault::new(agent, global))
    }

    #[test]
    fn oauth_creds_from_vault_prefers_agent_specific_over_global() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("mcp/v1/hubspot/client_id", "agent-id").unwrap();
        agent
            .set("mcp/v1/hubspot/client_secret", "agent-secret")
            .unwrap();
        global.set("mcp/v1/hubspot/client_id", "global-id").unwrap();
        global
            .set("mcp/v1/hubspot/client_secret", "global-secret")
            .unwrap();
        let store = layered(agent, Some(global));
        let creds = oauth_creds_from_vault("hubspot", store.as_ref()).unwrap();
        assert_eq!(creds.client_id, "agent-id");
        assert_eq!(creds.client_secret.as_deref(), Some("agent-secret"));
    }

    #[test]
    fn oauth_creds_from_vault_falls_back_to_global() {
        let agent = new_vault();
        let global = new_vault();
        global.set("mcp/v1/hubspot/client_id", "global-id").unwrap();
        global
            .set("mcp/v1/hubspot/client_secret", "global-secret")
            .unwrap();
        global
            .set("mcp/v1/hubspot/redirect_uri", "https://hub/callback")
            .unwrap();
        let store = layered(agent, Some(global));
        let creds = oauth_creds_from_vault("hubspot", store.as_ref()).unwrap();
        assert_eq!(creds.client_id, "global-id");
        assert_eq!(creds.client_secret.as_deref(), Some("global-secret"));
        assert_eq!(creds.redirect_uri.as_deref(), Some("https://hub/callback"));
    }

    #[test]
    fn oauth_creds_from_vault_ignores_per_user_paths() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set(
                "users/u-alice/mcp/v1/hubspot/client_id",
                "alice-shouldnt-leak",
            )
            .unwrap();
        global.set("mcp/v1/hubspot/client_id", "global-id").unwrap();
        let store = layered(agent, Some(global));
        let creds = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("u-alice".to_string(), || {
                oauth_creds_from_vault("hubspot", store.as_ref()).unwrap()
            });
        assert_eq!(
            creds.client_id, "global-id",
            "per-user entries must not satisfy OAuth client cred reads"
        );
    }

    #[test]
    fn oauth_creds_from_vault_errors_when_client_id_missing() {
        let agent = new_vault();
        let global = new_vault();
        let store = layered(agent, Some(global));
        let err = oauth_creds_from_vault("hubspot", store.as_ref()).unwrap_err();
        assert!(err.to_string().contains("client_id"));
    }
}

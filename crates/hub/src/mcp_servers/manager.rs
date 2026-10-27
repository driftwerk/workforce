//! [`McpServerManager`] — the live owner of an agent's MCP server
//! installs. Replaces the v1 [`super::server::spawn_all`] flow
//! (which built a registry then dropped it) so the hub can support
//! agent-driven and UI-driven install / uninstall / re-authorize
//! at runtime, with no hub restart required.
//!
//! Owned per-`AgentService`. Holds the live in-memory
//! [`McpServerRegistry`] (the spawned servers) plus shared
//! `Arc<dyn McpServerStore>` and `Arc<dyn OAuthStateStore>` so its
//! mutations survive restarts.
//!
//! Three install paths funnel through [`Self::install`]:
//! 1. **Hydration on hub boot** — [`Self::hydrate_from_store`] reads
//!    every persisted record for this agent and re-spawns it.
//! 2. **TOML import** — [`Self::import_from_toml`] copies
//!    `[mcp.servers.*]` entries that aren't already in the store
//!    so ops-as-code deployments still work.
//! 3. **Runtime install** — agent tool / REST API call →
//!    [`Self::install`] writes to the store and spawns the server
//!    in one shot.
//!
//! Uninstall mirrors install in reverse: pull the entry from the
//! registry, abort its refresh task, unregister every
//! `mcp.<id>.<*>` tool from the agent's `ToolRegistry`, unregister
//! the runtime-essential prefix, optionally purge OAuth tokens,
//! drop the store row.

use std::sync::Arc;

use eyre::{eyre, Result};
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::cost::CostTracker;
use workforce_common::redact::redact_url_str;
use workforce_common::settings::{DiscoveredMetadata, McpAuthConfig, McpConfig, McpServerConfig};
use workforce_mcp_client::{McpToolDef, ToolNameInfo, ToolNameSurvey};
use workforce_mcp_store::{InstalledVia, McpServerRecord, McpServerStore};
use workforce_oauth_client::{
    discover, register_client, ClientRegistrationRequest, OAuthClient, OAuthStateStore,
};

use super::oauth_bridge::provider_for_config;
use super::registry::{McpServerRegistry, ServerSummary};
use super::server::{build_client, start_one};
use super::tool_allowlist::validate_patterns;
use crate::hosts::{McpHost, ServerStatus};
use crate::vault::Vault;

/// One tool-name family (the prefix before the first `_`) and how many
/// tools share it. The category-summary view the agent reviews before
/// curating an allowlist.
#[derive(Debug, Clone, Serialize)]
pub struct ToolFamily {
    pub prefix: String,
    pub count: usize,
}

/// Result of [`McpServerManager::probe_remote_tools`] — a cheap survey of
/// a server's tool surface that registers nothing, so a server far too
/// large to register can still be reviewed and curated.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteToolSurvey {
    pub server_id: String,
    pub total_tools: usize,
    /// Paging stopped at `max_raw_tools`; the survey is a floor, not the
    /// whole surface.
    pub truncated: bool,
    /// Number of distinct families found.
    pub family_count: usize,
    /// Families sorted by count (desc) — capped to the largest few.
    pub families: Vec<ToolFamily>,
    /// Names matching the `contains` drill-down (empty when none given).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched: Vec<ToolNameInfo>,
    /// The `matched` list was capped.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub matched_truncated: bool,
}

/// Largest few families to return (keeps the default review payload tiny
/// even for a server with thousands of tools across many families).
const MAX_SURVEY_FAMILIES: usize = 60;
/// Cap on names returned for a `contains` drill-down.
const MAX_SURVEY_MATCHES: usize = 500;

/// Aggregate a raw name survey into the family summary + optional
/// `contains` drill-down the agent reviews. A family is the prefix before
/// the first `_` (`order_create` → `order`); names without `_` are their
/// own family.
fn build_survey(
    server_id: &str,
    survey: ToolNameSurvey,
    contains: Option<&str>,
) -> RemoteToolSurvey {
    use std::collections::BTreeMap;
    let total_tools = survey.names.len();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in &survey.names {
        let prefix = t.name.split_once('_').map(|(p, _)| p).unwrap_or(&t.name);
        *counts.entry(prefix.to_string()).or_default() += 1;
    }
    let family_count = counts.len();
    let mut families: Vec<ToolFamily> = counts
        .into_iter()
        .map(|(prefix, count)| ToolFamily { prefix, count })
        .collect();
    families.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.prefix.cmp(&b.prefix)));
    families.truncate(MAX_SURVEY_FAMILIES);

    let (matched, matched_truncated) = match contains {
        Some(sub) if !sub.is_empty() => {
            let mut m: Vec<ToolNameInfo> = survey
                .names
                .into_iter()
                .filter(|t| t.name.contains(sub))
                .collect();
            let trunc = m.len() > MAX_SURVEY_MATCHES;
            m.truncate(MAX_SURVEY_MATCHES);
            (m, trunc)
        }
        _ => (Vec::new(), false),
    };

    RemoteToolSurvey {
        server_id: server_id.to_string(),
        total_tools,
        truncated: survey.truncated,
        family_count,
        families,
        matched,
        matched_truncated,
    }
}

/// Default OAuth callback path mounted by the hub axum router.
/// Vendors are configured to redirect users here after consent;
/// `<provider>` is the segment that lets one hub serve multiple
/// providers from a single mounted route.
pub const DEFAULT_CALLBACK_PATH: &str = "/api/auth/{provider}/callback";

#[derive(Clone)]
pub struct McpServerManager {
    agent_id: String,
    store: Arc<dyn McpServerStore>,
    registry: Arc<McpServerRegistry>,
    tools: Arc<ToolRegistry>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    oauth_state: Arc<dyn OAuthStateStore>,
    cost: Option<Arc<CostTracker>>,
    http: Arc<reqwest::Client>,
}

/// Outcome of an `install` call. Two shapes:
/// - **`Installed`** — server is live and tools are registered.
/// - **`NeedsCredentials`** — config was persisted but the server
///   couldn't spawn yet because OAuth client credentials or tokens
///   are missing. Caller should set the listed vault paths and
///   then call `mcp.authorize` to walk the OAuth flow.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InstallReport {
    Installed {
        server_id: String,
        tool_count: usize,
        registered_tools: Vec<String>,
        auth_kind: &'static str,
    },
    NeedsCredentials {
        server_id: String,
        missing: Vec<String>,
        instructions: String,
        /// Per-preset setup walkthrough — populated by the agent
        /// tool / REST API layer when the install was preset-driven
        /// (the manager itself doesn't know which preset the caller
        /// used, so emits this empty and lets the caller enrich).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        setup_steps: Vec<super::presets::SetupStep>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    pub server_id: String,
    pub removed_tools: Vec<String>,
    pub purged_tokens: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthorizeStart {
    pub server_id: String,
    pub provider: String,
    pub authorization_url: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerDetail {
    pub summary: ServerSummary,
    pub config: Value,
    pub installed_via: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshReport {
    pub server_id: String,
    pub tool_count: usize,
}

impl McpServerManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_id: impl Into<String>,
        store: Arc<dyn McpServerStore>,
        tools: Arc<ToolRegistry>,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        oauth_state: Arc<dyn OAuthStateStore>,
        cost: Option<Arc<CostTracker>>,
        http: Arc<reqwest::Client>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_id: agent_id.into(),
            store,
            registry: Arc::new(McpServerRegistry::new()),
            tools,
            agent_vault,
            global_vault,
            oauth_state,
            cost,
            http,
        })
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn registry(&self) -> Arc<McpServerRegistry> {
        Arc::clone(&self.registry)
    }

    /// Re-spawn every persisted server for this agent. Called once
    /// at hub startup so live state matches the store. Failures on
    /// individual servers are logged and skipped — same isolation
    /// rule v1's `spawn_all` already enforces.
    pub async fn hydrate_from_store(&self) -> Result<()> {
        let records = self
            .store
            .list(&self.agent_id)
            .map_err(|e| eyre!("listing mcp_servers for hydration: {e}"))?;
        if records.is_empty() {
            return Ok(());
        }
        info!(
            agent = %self.agent_id,
            count = records.len(),
            "hydrating MCP servers from store"
        );
        for record in records {
            let cfg: McpServerConfig = match serde_json::from_value(record.config.clone()) {
                Ok(c) => c,
                Err(e) => {
                    warn!(server = %record.server_id, error = %e, "skipping malformed mcp store row");
                    continue;
                }
            };
            if let Err(e) = self.spawn_in_registry(&record.server_id, &cfg).await {
                warn!(server = %record.server_id, error = %e, "hydrate skipped server");
            }
        }
        Ok(())
    }

    /// Reconcile `[mcp.servers.*]` TOML entries into the store. New
    /// entries are inserted as `InstalledVia::Toml`; entries already
    /// in the store are left alone (operator-edited or
    /// agent-installed config wins). After import, hydrated entries
    /// will be live in the registry; entries newly imported here
    /// are spawned inline so a fresh hub picks them up first boot.
    pub async fn import_from_toml(&self, toml: &McpConfig) -> Result<()> {
        for (id, cfg) in &toml.servers {
            if self
                .store
                .get(&self.agent_id, id)
                .map_err(|e| eyre!("checking mcp_store for `{id}`: {e}"))?
                .is_some()
            {
                continue;
            }
            let config_json = serde_json::to_value(cfg)
                .map_err(|e| eyre!("serialising TOML config for `{id}`: {e}"))?;
            let record = McpServerRecord::new(&self.agent_id, id, config_json, InstalledVia::Toml);
            self.store
                .put(&record)
                .map_err(|e| eyre!("persisting TOML import for `{id}`: {e}"))?;
            // Spawn now if the registry doesn't already have it
            // (hydrate_from_store may have already handled it on a
            // re-import — but TOML imports are skipped above when
            // the row exists, so this is a fresh add).
            if self.registry.get(id).is_none() {
                if let Err(e) = self.spawn_in_registry(id, cfg).await {
                    warn!(server = %id, error = %e, "TOML-imported server failed to spawn");
                }
            }
        }
        Ok(())
    }

    /// Install (or update) a server. Persists the config to the
    /// store, then spawns it; for OAuth servers without seeded
    /// credentials returns a `NeedsCredentials` report instead of
    /// failing — the caller is expected to seed the vault and call
    /// `authorize_start` to complete setup.
    pub async fn install(
        &self,
        server_id: &str,
        config: McpServerConfig,
        installed_via: InstalledVia,
    ) -> Result<InstallReport> {
        validate_patterns(&config.tools_allow, &config.tools_deny)
            .map_err(|e| eyre!("server `{server_id}`: {e}"))?;
        let config_json = serde_json::to_value(&config)
            .map_err(|e| eyre!("serialising config for `{server_id}`: {e}"))?;
        let record = McpServerRecord::new(&self.agent_id, server_id, config_json, installed_via);
        self.store
            .put(&record)
            .map_err(|e| eyre!("persisting install for `{server_id}`: {e}"))?;

        // If the server is already installed, swap it out cleanly:
        // shutdown the existing one, unregister its tools, then
        // start the new config. A common case is `mcp.install` being
        // called a second time to update endpoint or refresh
        // interval.
        if self.registry.get(server_id).is_some() {
            self.unwire_locally(server_id);
        }

        match self.spawn_in_registry(server_id, &config).await {
            Ok(report) => Ok(report),
            Err(e) => {
                // Differentiate "needs credentials" from real failures
                // so the agent / UI can guide the operator instead of
                // spinning on a transient error.
                if let Some(report) = self.diagnose_missing_credentials(server_id, &config) {
                    Ok(report)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Install a self-hosted OAuth server that advertises its endpoints
    /// via discovery and self-registers its client (RFC 8414 + 7591),
    /// e.g. Metabase. Resolves the templated endpoint from the vault,
    /// discovers metadata, registers a public PKCE client once (skipped
    /// if a `client_id` already exists), derives the redirect URI from
    /// `public_url`, caches the metadata on the config, then installs.
    ///
    /// `base_url_var` is the vault path holding the per-instance base
    /// URL substituted for [`super::presets::BASE_URL_TOKEN`] in
    /// `config.endpoint` (`None` for a fixed endpoint). Returns
    /// `NeedsCredentials` (persisting nothing) when that value is unset.
    ///
    /// Step order is self-healing: a failure after registration but
    /// before the row is persisted leaves the `client_id` in the vault,
    /// so a re-install skips registration and completes the rest.
    pub async fn install_dynamic(
        &self,
        server_id: &str,
        mut config: McpServerConfig,
        base_url_var: Option<&str>,
        public_url: Option<&str>,
        discovery_issuer: Option<&str>,
        installed_via: InstalledVia,
    ) -> Result<InstallReport> {
        validate_patterns(&config.tools_allow, &config.tools_deny)
            .map_err(|e| eyre!("server `{server_id}`: {e}"))?;
        let global = self
            .global_vault
            .clone()
            .ok_or_else(|| eyre!("dynamic OAuth install requires a global vault"))?;

        if let Some(path) = base_url_var {
            let Some(base_url) = global.get(path) else {
                return Ok(InstallReport::NeedsCredentials {
                    server_id: server_id.to_string(),
                    missing: vec![path.to_string()],
                    instructions: format!(
                        "Set `{path}` to your instance's base URL, then reinstall the server."
                    ),
                    setup_steps: Vec::new(),
                });
            };
            config.endpoint = config.endpoint.replace(
                super::presets::BASE_URL_TOKEN,
                base_url.trim_end_matches('/'),
            );
        }

        // Scopes come from the preset config, never from discovery's
        // `scopes_supported`: a preset author requests exactly what the
        // service needs (Metabase requests none and derives them at
        // consent time). Auto-requesting every advertised scope would
        // over-ask.
        let (provider, scopes) = match &config.auth {
            McpAuthConfig::Oauth2 {
                provider, scopes, ..
            } => (provider.clone(), scopes.clone()),
            McpAuthConfig::Pat { .. } => {
                return Err(eyre!("install_dynamic requires OAuth2 auth"));
            }
        };
        let public_url = public_url.ok_or_else(|| {
            eyre!("dynamic OAuth install requires `[hub].public_url` to derive the redirect URI")
        })?;
        let redirect_uri = format!(
            "{}/api/auth/{provider}/callback",
            public_url.trim_end_matches('/')
        );

        // Discover against the OAuth issuer when it differs from the MCP
        // endpoint — e.g. a Keycloak-fronted server whose API lives at
        // `api.example` but whose OIDC issuer is `auth.example/realms/x`.
        // Without `issuer`, discovery targets the MCP endpoint (correct for
        // servers that serve their own `.well-known`, like Metabase).
        let discovery_target = discovery_issuer.unwrap_or(&config.endpoint);
        let metadata = discover(&self.http, discovery_target).await.map_err(|e| {
            eyre!(
                "OAuth discovery failed for `{server_id}` at {}: {e}",
                redact_url_str(discovery_target)
            )
        })?;

        // Obtain an OAuth client for this server. A fresh one is needed when
        // none is stored yet, or when the instance now points at a different
        // authorization server (a client_id minted by the old issuer would be
        // rejected). When a usable client is already seeded, reuse it as-is.
        let issuer_changed = self
            .cached_authorization_endpoint(server_id)
            .is_some_and(|prev| prev != metadata.authorization_endpoint);
        let base = format!("mcp/v1/{server_id}");
        if global.get(&format!("{base}/client_id")).is_none() || issuer_changed {
            // Prefer RFC 7591 dynamic registration when the authorization
            // server both advertises it and accepts anonymous registration.
            // Many IdPs (Keycloak / Okta / Auth0) advertise a registration
            // endpoint but gate it behind an initial-access-token, or don't
            // offer one at all — there we fall back to an operator-registered
            // client and return a precise "register a client" walkthrough
            // instead of failing hard.
            let registered = match &metadata.registration_endpoint {
                Some(registration_endpoint) => {
                    let scope = (!scopes.is_empty()).then(|| scopes.join(" "));
                    let request = ClientRegistrationRequest::public_pkce(
                        format!("workforce ({})", self.agent_id),
                        redirect_uri.clone(),
                        scope,
                    );
                    match register_client(&self.http, registration_endpoint, &request).await {
                        Ok(reg) => Some(reg),
                        Err(e) => {
                            warn!(
                                server = %server_id,
                                endpoint = %redact_url_str(registration_endpoint),
                                error = %e,
                                "dynamic client registration failed; falling back to an operator-registered client"
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            let Some(registered) = registered else {
                // No usable client and we can't self-register. Persist nothing
                // (mirrors the missing-base-url path) and tell the operator
                // exactly what to register and where to seed it.
                return Ok(InstallReport::NeedsCredentials {
                    server_id: server_id.to_string(),
                    missing: vec![format!("{base}/client_id")],
                    instructions: format!(
                        "`{server_id}`'s authorization server does not allow anonymous client \
                         registration. Register an OAuth client there with redirect URI \
                         `{redirect_uri}` (a public client + PKCE needs no secret; for a \
                         confidential client also set `{base}/client_secret`), set \
                         `{base}/client_id` to the new client's id, then reinstall."
                    ),
                    setup_steps: Vec::new(),
                });
            };
            global
                .set(&format!("{base}/client_id"), &registered.client_id)
                .map_err(|e| eyre!("persisting client_id for `{server_id}`: {e}"))?;
            // Mirror the new client's auth method: a public client returns
            // no secret, so any stale secret from a prior issuer is cleared.
            match registered.client_secret {
                Some(secret) => global
                    .set(&format!("{base}/client_secret"), &secret)
                    .map_err(|e| eyre!("persisting client_secret for `{server_id}`: {e}"))?,
                None => {
                    let _ = global.delete(&format!("{base}/client_secret"));
                }
            }
            if let Some(token) = registered.registration_access_token {
                global
                    .set(&format!("{base}/registration_access_token"), &token)
                    .map_err(|e| eyre!("persisting registration token for `{server_id}`: {e}"))?;
            }
        }
        global
            .set(&format!("{base}/redirect_uri"), &redirect_uri)
            .map_err(|e| eyre!("persisting redirect_uri for `{server_id}`: {e}"))?;

        if let McpAuthConfig::Oauth2 { discovery, .. } = &mut config.auth {
            *discovery = Some(DiscoveredMetadata {
                authorization_endpoint: metadata.authorization_endpoint,
                token_endpoint: metadata.token_endpoint,
                registration_endpoint: metadata.registration_endpoint,
                scopes_supported: metadata.scopes_supported,
            });
        }

        self.install(server_id, config, installed_via).await
    }

    /// Patch just the per-server tool allow/deny lists on an already
    /// installed server and re-apply them hot. Reuses the stored config
    /// (endpoint, auth, cached discovery metadata) so there's no re-auth
    /// and no re-discovery — the agent reviews the full `mcp.<id>.*`
    /// surface, picks the keepers, and calls this; `install` swaps the
    /// running server in place, unregistering the now-denied tools and
    /// registering only the curated set. Takes effect immediately, no
    /// hub restart.
    pub async fn set_tool_filter(
        &self,
        server_id: &str,
        tools_allow: Vec<String>,
        tools_deny: Vec<String>,
    ) -> Result<InstallReport> {
        let record = self
            .store
            .get(&self.agent_id, server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
            .ok_or_else(|| eyre!("server `{server_id}` is not installed on this agent"))?;
        let installed_via = record.installed_via;
        let mut config: McpServerConfig = serde_json::from_value(record.config)
            .map_err(|e| eyre!("stored config for `{server_id}` is malformed: {e}"))?;
        config.tools_allow = tools_allow;
        config.tools_deny = tools_deny;
        // `install` validates the globs, persists, and hot-swaps the
        // running server (unwire old tools → spawn with the new filter).
        self.install(server_id, config, installed_via).await
    }

    /// Place a server into (or remove it from) an MCP **workspace**. A pure
    /// metadata change: persisted to the store and applied to the in-memory
    /// membership registry WITHOUT respawning the server (its tool surface is
    /// unchanged), so it's safe even when the surface is over the registration
    /// cap. `workspace`+`env` set membership (the member starts dormant —
    /// `default` stays false); both `None` ungroups it (→ always active). The
    /// default lever is the human-gated [`Self::set_workspace_default`].
    pub fn set_workspace_membership(
        &self,
        server_id: &str,
        workspace: Option<&str>,
        env: Option<&str>,
    ) -> Result<()> {
        let mut record = self
            .store
            .get(&self.agent_id, server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
            .ok_or_else(|| eyre!("server `{server_id}` is not installed on this agent"))?;
        let mut config: McpServerConfig = serde_json::from_value(record.config.clone())
            .map_err(|e| eyre!("stored config for `{server_id}` is malformed: {e}"))?;
        match (workspace, env) {
            (Some(ws), Some(env)) => {
                if ws.trim().is_empty() || env.trim().is_empty() {
                    return Err(eyre!("workspace and env must be non-empty"));
                }
                config.workspace = Some(ws.to_string());
                config.env = Some(env.to_string());
                workforce_agent::llm::mcp_workspace::set_member(server_id, ws, env, config.default);
            }
            (None, None) => {
                config.workspace = None;
                config.env = None;
                config.default = false;
                workforce_agent::llm::mcp_workspace::clear_member(server_id);
            }
            _ => {
                return Err(eyre!(
                    "both `workspace` and `env` are required (or neither, to ungroup)"
                ))
            }
        }
        record.config = serde_json::to_value(&config)
            .map_err(|e| eyre!("serializing config for `{server_id}`: {e}"))?;
        self.store
            .put(&record)
            .map_err(|e| eyre!("persisting workspace membership for `{server_id}`: {e}"))?;
        Ok(())
    }
    /// Survey an installed server's tool surface *without registering
    /// anything* — the discovery step that makes a too-large server
    /// curatable. Reads the stored config (so it works on a dormant
    /// server that failed to hydrate over-cap) and reaches the server
    /// under the calling user's scope, just like a tool call. Returns a
    /// family summary (and, when `contains` is given, the matching names)
    /// so the agent can build a sensible `tools_allow` for `set_tool_filter`.
    pub async fn probe_remote_tools(
        &self,
        server_id: &str,
        contains: Option<&str>,
    ) -> Result<RemoteToolSurvey> {
        let record = self
            .store
            .get(&self.agent_id, server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
            .ok_or_else(|| eyre!("server `{server_id}` is not installed on this agent"))?;
        let cfg: McpServerConfig = serde_json::from_value(record.config)
            .map_err(|e| eyre!("stored config for `{server_id}` is malformed: {e}"))?;
        let (client, discovery) = build_client(
            server_id,
            &cfg,
            Arc::clone(&self.http),
            &self.agent_vault,
            &self.global_vault,
        )?;
        let survey = discovery
            .run(async {
                client
                    .initialize()
                    .await
                    .map_err(|e| eyre!("initialize failed for `{server_id}`: {e}"))?;
                client
                    .list_tool_names()
                    .await
                    .map_err(|e| eyre!("listing tools for `{server_id}`: {e}"))
            })
            .await?;
        Ok(build_survey(server_id, survey, contains))
    }

    /// Authorization endpoint cached on a server's stored config from a
    /// prior discovery, if any. Used to detect that a self-hosted
    /// instance has been repointed at a different OAuth issuer.
    fn cached_authorization_endpoint(&self, server_id: &str) -> Option<String> {
        let record = self.store.get(&self.agent_id, server_id).ok().flatten()?;
        let cfg: McpServerConfig = serde_json::from_value(record.config).ok()?;
        match cfg.auth {
            McpAuthConfig::Oauth2 { discovery, .. } => discovery.map(|d| d.authorization_endpoint),
            McpAuthConfig::Pat { .. } => None,
        }
    }

    /// Stop and forget a server. Aborts the refresh task,
    /// unregisters every `mcp.<id>.*` tool, drops the store row.
    /// `purge_tokens` additionally wipes per-user OAuth tokens
    /// from the vault.
    pub async fn uninstall(&self, server_id: &str, purge_tokens: bool) -> Result<UninstallReport> {
        // Store-delete first so a partial failure can't leave the
        // registry empty while the row remains — next hub start
        // would otherwise re-spawn an unwanted server.
        self.store
            .delete(&self.agent_id, server_id)
            .map_err(|e| eyre!("deleting mcp_store row for `{server_id}`: {e}"))?;
        let removed_tools = self.unwire_locally(server_id);
        let purged = if purge_tokens {
            self.purge_oauth_tokens(server_id)
        } else {
            false
        };
        info!(
            agent = %self.agent_id,
            server = %server_id,
            removed = removed_tools.len(),
            purged_tokens = purged,
            "MCP server uninstalled"
        );
        Ok(UninstallReport {
            server_id: server_id.to_string(),
            removed_tools,
            purged_tokens: purged,
        })
    }

    pub fn list(&self) -> Vec<ServerSummary> {
        self.registry.summaries()
    }
    pub fn show(&self, server_id: &str) -> Result<Option<ServerDetail>> {
        let Some(record) = self
            .store
            .get(&self.agent_id, server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
        else {
            return Ok(None);
        };
        let summary = self
            .registry
            .get(server_id)
            .map(|s| s.summary(server_id))
            .unwrap_or(ServerSummary {
                id: server_id.to_string(),
                endpoint: record
                    .config
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tool_count: 0,
                auth_kind: "unknown",
            });
        Ok(Some(ServerDetail {
            summary,
            config: record.config,
            installed_via: record.installed_via.as_str().to_string(),
        }))
    }

    /// Re-fetch `tools/list` from upstream now (without waiting for
    /// the next refresh tick) and reconcile against the agent's
    /// `ToolRegistry`. Returns the server's current tool count.
    pub async fn refresh(&self, server_id: &str) -> Result<RefreshReport> {
        let server = self
            .registry
            .get(server_id)
            .ok_or_else(|| eyre!("mcp server `{server_id}` is not installed"))?;
        let client = server.client();
        // Filter at fetch time so `mcp.update` honours the allow/deny
        // lists (else it would re-register denied tools) and stays under
        // the cap for an over-cap server.
        let latest: Vec<McpToolDef> = client
            .tools_list_filtered(|n| server.allowlist().keeps_name(n))
            .await
            .map_err(|e| eyre!("tools/list refresh failed for `{server_id}`: {e}"))?;
        let prefix = format!("mcp.{server_id}.");
        let existing: Vec<String> = self
            .tools
            .names()
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .collect();
        let latest_qualified: std::collections::HashSet<String> = latest
            .iter()
            .map(|d| format!("{prefix}{}", d.name))
            .collect();
        for name in &existing {
            if !latest_qualified.contains(name) {
                self.tools.unregister(name);
            }
        }
        for def in &latest {
            let qualified = format!("{prefix}{}", def.name);
            if self.tools.get(&qualified).is_some() {
                continue;
            }
            let tool = super::tool::McpTool::new(
                server_id,
                &self.agent_id,
                Arc::clone(&client),
                def.clone(),
                self.cost.clone(),
            );
            self.tools.register(Arc::new(tool));
        }
        let tool_count = latest.len();
        *server.tool_count.lock() = tool_count;
        Ok(RefreshReport {
            server_id: server_id.to_string(),
            tool_count,
        })
    }

    /// Start a UI/agent-driven OAuth web flow for a server whose
    /// auth is configured as `Oauth2`. Returns the
    /// `authorization_url` for the caller to open in a browser.
    /// `uid` is the user the eventual tokens will be filed against.
    pub async fn authorize_start(&self, server_id: &str, uid: &str) -> Result<AuthorizeStart> {
        let record = self
            .store
            .get(&self.agent_id, server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
            .ok_or_else(|| eyre!("mcp server `{server_id}` is not installed"))?;
        let cfg: McpServerConfig = serde_json::from_value(record.config)
            .map_err(|e| eyre!("malformed config for `{server_id}`: {e}"))?;
        let (provider, scopes, discovery) = match cfg.auth {
            McpAuthConfig::Oauth2 {
                provider,
                scopes,
                discovery,
                ..
            } => (provider, scopes, discovery),
            McpAuthConfig::Pat { .. } => {
                return Err(eyre!(
                    "server `{server_id}` uses PAT auth — no `mcp.authorize` flow needed"
                ));
            }
        };
        let layered = super::server::layered_store(&self.agent_vault, &self.global_vault)
            .ok_or_else(|| eyre!("authorise requires at least one vault (agent or global)"))?;
        let creds =
            super::server::oauth_creds_from_vault(server_id, layered.as_ref()).map_err(|e| {
                eyre!(
                    "missing OAuth client creds for `{server_id}`: {e} \
                     (set them at `mcp/v1/{server_id}/{{client_id,client_secret,redirect_uri}}` at \
                     tier 'agent_specific' or 'global' and retry)"
                )
            })?;
        let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
            eyre!("vault `mcp/v1/{server_id}/redirect_uri` is required for OAuth flow")
        })?;
        let provider_impl = provider_for_config(&provider, discovery.as_ref())?;
        let client = OAuthClient::new(provider_impl, creds, reqwest::Client::clone(&self.http));
        let scope_refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        let started = client.start_web_flow(&redirect_uri, &scope_refs);
        // Persist (state → flow context). `workspace` carries the
        // agent_id so the callback handler can dispatch to the
        // correct manager when multiple agents share one hub.
        // `provider` carries `mcp:<server_id>` so the callback can
        // resolve back to this server (the OAuth provider name
        // alone wouldn't tell us which install to wire tokens for).
        self.oauth_state
            .insert(
                started.state.clone(),
                format!("mcp:{server_id}:{provider}"),
                uid.to_string(),
                self.agent_id.clone(),
                started.verifier,
            )
            .map_err(|e| eyre!("persisting OAuth state for `{server_id}`: {e}"))?;
        Ok(AuthorizeStart {
            server_id: server_id.to_string(),
            provider,
            authorization_url: started.authorization_url,
            expires_in_secs: workforce_oauth_client::STATE_TTL.as_secs(),
        })
    }

    /// Complete a previously-started OAuth flow. Looks up the
    /// pending state, exchanges `code` + `verifier` for tokens, and
    /// persists them at the per-user vault path the
    /// [`super::auth::TokenStoreAuth`] reader uses on `tools/call`.
    /// On success, re-spawns the server if it isn't already live —
    /// covers the "install returned NeedsCredentials, then user
    /// authorised" path.
    ///
    /// Callers who already hold a [`workforce_oauth_client::StateEntry`]
    /// (e.g. the axum callback route) should use
    /// [`Self::complete_with_entry`] instead so the state is consumed
    /// exactly once.
    pub async fn authorize_complete(&self, state: &str, code: &str) -> Result<String> {
        let entry = self
            .oauth_state
            .take(state)
            .map_err(|e| eyre!("retrieving OAuth state: {e}"))?
            .ok_or_else(|| eyre!("OAuth state `{state}` is unknown or expired"))?;
        self.complete_with_entry(entry, code).await
    }

    /// Same as [`Self::authorize_complete`] but takes an
    /// already-consumed `StateEntry` directly. The callback route
    /// uses this to consume the state from the shared
    /// `OAuthStateStore` exactly once, then dispatch to the right
    /// per-agent manager.
    pub async fn complete_with_entry(
        &self,
        entry: workforce_oauth_client::StateEntry,
        code: &str,
    ) -> Result<String> {
        if entry.workspace != self.agent_id {
            return Err(eyre!(
                "OAuth state belongs to a different agent (`{}`)",
                entry.workspace
            ));
        }
        let (server_id, provider) = parse_state_provider(&entry.provider)?;
        let record = self
            .store
            .get(&self.agent_id, &server_id)
            .map_err(|e| eyre!("loading mcp_store row for `{server_id}`: {e}"))?
            .ok_or_else(|| eyre!("OAuth callback for unknown server `{server_id}`"))?;
        let cfg: McpServerConfig = serde_json::from_value(record.config)
            .map_err(|e| eyre!("malformed stored config for `{server_id}`: {e}"))?;

        let global_vault = self
            .global_vault
            .clone()
            .ok_or_else(|| eyre!("authorise_complete requires a global vault"))?;
        let layered = super::server::layered_store(&self.agent_vault, &self.global_vault)
            .ok_or_else(|| eyre!("authorise_complete requires at least one vault"))?;
        let creds = super::server::oauth_creds_from_vault(&server_id, layered.as_ref())?;
        let redirect_uri = creds
            .redirect_uri
            .clone()
            .ok_or_else(|| eyre!("vault `mcp/v1/{server_id}/redirect_uri` missing on callback"))?;
        let discovery = match &cfg.auth {
            McpAuthConfig::Oauth2 { discovery, .. } => discovery.as_ref(),
            McpAuthConfig::Pat { .. } => None,
        };
        let provider_impl = provider_for_config(&provider, discovery)?;
        let client = OAuthClient::new(provider_impl, creds, reqwest::Client::clone(&self.http));
        let tokens = client
            .exchange_web_code(code, &entry.verifier, &redirect_uri)
            .await
            .map_err(|e| eyre!("OAuth code exchange failed for `{server_id}`: {e}"))?;

        // Persist tokens at the path TokenStoreAuth reads from on
        // `tools/call`. `binding=User` (the default) writes under
        // `users/{uid}/mcp/v1/<server_id>/...`; `binding=Shared`
        // writes the shared org-wide path. We mirror the logic in
        // `super::auth::TokenStoreAuth` here so the locations
        // match exactly.
        persist_tokens(&server_id, &cfg.auth, &entry.uid, &tokens, &global_vault)?;

        // Spawn the server now that it can authenticate (if it wasn't
        // already running — install may have returned NeedsCredentials
        // earlier, or the operator may be re-authing an expired session).
        // The spawn's `tools/list` resolves a `binding=User` token from
        // the `CURRENT_USER_ID` task-local, which this OAuth callback
        // (a browser redirect, not an agent turn) lacks — so scope it to
        // the user who just authorised, else no tools register.
        if self.registry.get(&server_id).is_none() {
            let spawn = self.spawn_in_registry(&server_id, &cfg);
            if let Err(e) = workforce_agent::runner::CURRENT_USER_ID
                .scope(entry.uid.clone(), spawn)
                .await
            {
                warn!(server = %server_id, error = %e, "post-authorise spawn failed");
            }
        }
        Ok(server_id)
    }

    // ── internals ─────────────────────────────────────────────

    async fn spawn_in_registry(
        &self,
        server_id: &str,
        cfg: &McpServerConfig,
    ) -> Result<InstallReport> {
        let server = start_one(
            server_id,
            &self.agent_id,
            cfg,
            Arc::clone(&self.http),
            Arc::clone(&self.tools),
            self.agent_vault.clone(),
            self.global_vault.clone(),
            self.cost.clone(),
        )
        .await?;
        let auth_kind = server.auth_kind;
        let tool_count = *server.tool_count.lock();
        let prefix = format!("mcp.{server_id}.");
        let registered_tools: Vec<String> = self
            .tools
            .names()
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .collect();
        self.registry.insert(server_id, Arc::new(server));
        Ok(InstallReport::Installed {
            server_id: server_id.to_string(),
            tool_count,
            registered_tools,
            auth_kind,
        })
    }

    /// Local-state cleanup — registry entry, tool registrations,
    /// runtime-essential prefix. Returns the names of tools removed.
    /// Does NOT touch the store or the vault.
    fn unwire_locally(&self, server_id: &str) -> Vec<String> {
        if let Some(server) = self.registry.remove(server_id) {
            server.shutdown();
            // Drop our extra Arc explicitly so the connection only
            // lives until the last in-flight `tools/call` finishes
            // (each McpTool holds its own Arc<McpClient>).
            drop(server);
        }
        let prefix = format!("mcp.{server_id}.");
        let to_remove: Vec<String> = self
            .tools
            .names()
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .collect();
        for name in &to_remove {
            self.tools.unregister(name);
        }
        // Drop any MCP-workspace membership. On a hot-swap re-install, the
        // subsequent `start_one` re-records it from the new config; on a true
        // uninstall it stays cleared.
        workforce_agent::llm::mcp_workspace::clear_member(server_id);
        to_remove
    }

    fn purge_oauth_tokens(&self, server_id: &str) -> bool {
        // Operator-supplied app credentials (`client_id`,
        // `client_secret`, `redirect_uri`) are NOT purged here —
        // they're shared across re-installs.
        const TOKEN_SUFFIXES: &[&str] = &[
            "access_token",
            "refresh_token",
            "expires_at",
            "scopes",
            "email",
        ];
        let Some(global) = self.global_vault.clone() else {
            return false;
        };
        let shared_paths = TOKEN_SUFFIXES
            .iter()
            .map(|s| format!("mcp/v1/{server_id}/{s}"));
        let user_segment = format!("/mcp/v1/{server_id}/");
        let user_paths = global.list(Some("users/")).into_iter().filter(move |k| {
            TOKEN_SUFFIXES
                .iter()
                .any(|t| k.ends_with(&format!("{user_segment}{t}")))
        });
        let mut purged = false;
        for key in shared_paths.chain(user_paths) {
            match global.delete(&key) {
                Ok(true) => purged = true,
                Ok(false) => {}
                Err(e) => warn!(path = %key, error = %e, "failed to purge OAuth token"),
            }
        }
        purged
    }

    /// Inspect `cfg` against the vault to decide whether the
    /// missing piece is operator-supplied client credentials or a
    /// pending OAuth flow. Used by `install` to differentiate a
    /// "needs setup" outcome from a real spawn failure.
    fn diagnose_missing_credentials(
        &self,
        server_id: &str,
        cfg: &McpServerConfig,
    ) -> Option<InstallReport> {
        let McpAuthConfig::Oauth2 { discovery, .. } = &cfg.auth else {
            return None;
        };
        let global = self.global_vault.clone()?;
        let base = format!("mcp/v1/{server_id}");
        // Dynamically-registered (discovery) servers are public PKCE
        // clients with a self-registered id and a derived redirect_uri —
        // no operator-supplied client_secret.
        let required: &[&str] = if discovery.is_some() {
            &["client_id", "redirect_uri"]
        } else {
            &["client_id", "client_secret", "redirect_uri"]
        };
        let mut missing = Vec::new();
        for key in required {
            let path = format!("{base}/{key}");
            if global.get(&path).is_none() {
                missing.push(path);
            }
        }
        if missing.is_empty() {
            // Creds are present but tokens haven't been issued —
            // that's a "ready to authorize" state.
            Some(InstallReport::NeedsCredentials {
                server_id: server_id.to_string(),
                missing: Vec::new(),
                instructions: format!(
                    "OAuth client credentials are seeded. Call `mcp.authorize {server_id}` to start the consent flow."
                ),
                setup_steps: Vec::new(),
            })
        } else {
            Some(InstallReport::NeedsCredentials {
                server_id: server_id.to_string(),
                missing: missing.clone(),
                instructions: format!(
                    "Set the listed vault paths (operator-supplied OAuth app credentials) and then call `mcp.authorize {server_id}`. \
                     Missing: {}.",
                    missing.join(", ")
                ),
                setup_steps: Vec::new(),
            })
        }
    }
}

impl InstallReport {
    /// If this is `NeedsCredentials` and was preset-driven, attach
    /// the preset's setup walkthrough so the caller (agent tool /
    /// REST API) can hand the user a complete checklist. No-op for
    /// `Installed` or non-preset installs.
    pub fn with_preset_setup_steps(
        mut self,
        preset_id: Option<&str>,
        public_url: Option<&str>,
    ) -> Self {
        if let (InstallReport::NeedsCredentials { setup_steps, .. }, Some(pid)) =
            (&mut self, preset_id)
        {
            if let Some(preset) = super::presets::lookup(pid) {
                *setup_steps = preset.setup_steps(public_url);
            }
        }
        self
    }
}

/// Persist tokens at the same paths `super::auth::TokenStoreAuth`
/// reads from on `tools/call`. Shared binding writes under
/// `mcp/v1/<id>/...`; user binding writes under
/// `users/<uid>/mcp/v1/<id>/...`.
fn persist_tokens(
    server_id: &str,
    auth: &McpAuthConfig,
    uid: &str,
    tokens: &workforce_oauth_client::StoredTokens,
    vault: &Arc<Vault>,
) -> Result<()> {
    let base = match auth {
        McpAuthConfig::Oauth2 { binding, .. } => match binding {
            workforce_common::settings::McpAuthBinding::User => {
                format!("users/{uid}/mcp/v1/{server_id}")
            }
            workforce_common::settings::McpAuthBinding::Shared => {
                format!("mcp/v1/{server_id}")
            }
        },
        McpAuthConfig::Pat { .. } => return Err(eyre!("PAT auth doesn't persist tokens")),
    };
    vault
        .set(&format!("{base}/access_token"), &tokens.access_token)
        .map_err(|e| eyre!("vault set access_token: {e}"))?;
    vault
        .set(&format!("{base}/refresh_token"), &tokens.refresh_token)
        .map_err(|e| eyre!("vault set refresh_token: {e}"))?;
    vault
        .set(
            &format!("{base}/expires_at"),
            &tokens.expires_at.to_string(),
        )
        .map_err(|e| eyre!("vault set expires_at: {e}"))?;
    if !tokens.scopes.is_empty() {
        vault
            .set(&format!("{base}/scopes"), &tokens.scopes)
            .map_err(|e| eyre!("vault set scopes: {e}"))?;
    }
    if let Some(email) = &tokens.email {
        vault
            .set(&format!("{base}/email"), email)
            .map_err(|e| eyre!("vault set email: {e}"))?;
    }
    Ok(())
}

/// Decode the composite `provider` string we stuff into the
/// `OAuthStateStore` on `authorize_start`. Format: `mcp:<id>:<provider>`.
fn parse_state_provider(s: &str) -> Result<(String, String)> {
    let mut parts = s.splitn(3, ':');
    let kind = parts.next().unwrap_or("");
    if kind != "mcp" {
        return Err(eyre!("unexpected OAuth state provider tag `{s}`"));
    }
    let id = parts
        .next()
        .ok_or_else(|| eyre!("OAuth state missing server_id segment"))?;
    let provider = parts
        .next()
        .ok_or_else(|| eyre!("OAuth state missing provider segment"))?;
    Ok((id.to_string(), provider.to_string()))
}

impl McpHost for McpServerManager {
    /// Set the persistent default (auto-active) env for a workspace — the
    /// human-gated lever behind `cmd: mcp default <ws> <env>`. Rewrites every
    /// member record so `default = (env == default_env)` and updates the live
    /// membership registry; no respawn (default only affects gating). Errors
    /// if the workspace or env doesn't exist.
    fn set_workspace_default(&self, workspace: &str, default_env: &str) -> Result<()> {
        let records = self
            .store
            .list(&self.agent_id)
            .map_err(|e| eyre!("listing mcp_store rows: {e}"))?;
        let mut members = 0usize;
        let mut found_env = false;
        for mut record in records {
            let mut config: McpServerConfig = match serde_json::from_value(record.config.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if config.workspace.as_deref() != Some(workspace) {
                continue;
            }
            members += 1;
            let should = config.env.as_deref() == Some(default_env);
            if should {
                found_env = true;
            }
            if config.default != should {
                config.default = should;
                record.config = serde_json::to_value(&config)
                    .map_err(|e| eyre!("serializing config for `{}`: {e}", record.server_id))?;
                self.store
                    .put(&record)
                    .map_err(|e| eyre!("persisting default for `{}`: {e}", record.server_id))?;
            }
        }
        if members == 0 {
            return Err(eyre!("no MCP workspace named `{workspace}`"));
        }
        if !found_env {
            return Err(eyre!(
                "workspace `{workspace}` has no `{default_env}` environment"
            ));
        }
        workforce_agent::llm::mcp_workspace::set_workspace_default(workspace, default_env);
        Ok(())
    }

    /// Status of every **installed** server (the persisted store, not just the
    /// live registry), joined with connection state. This is the join the
    /// environment state board needs: a server can be installed (store row
    /// written by [`Self::install`]) yet not connected — most importantly when
    /// it is awaiting OAuth authorisation, the exact state that reads as
    /// "configured but not usable". `list()` (registry only) can't see those, so
    /// the board would otherwise report an unauthorised server as "not
    /// installed". Reading the store is cheap and side-effect-free.
    fn server_statuses(&self) -> Vec<ServerStatus> {
        let records = match self.store.list(&self.agent_id) {
            Ok(r) => r,
            Err(e) => {
                warn!(agent = %self.agent_id, error = %e, "server_statuses: store list failed");
                return Vec::new();
            }
        };
        records
            .into_iter()
            .map(|rec| {
                let cfg: Option<McpServerConfig> = serde_json::from_value(rec.config).ok();
                let connected = self.registry.get(&rec.server_id).is_some();
                // Installed + uses OAuth + not connected ⇒ it almost certainly
                // just needs `mcp.authorize`. (A PAT server that won't connect is
                // a different fault — bad endpoint/token — so don't claim auth.)
                let is_oauth = cfg
                    .as_ref()
                    .map(|c| matches!(c.auth, McpAuthConfig::Oauth2 { .. }))
                    .unwrap_or(false);
                ServerStatus {
                    workspace: cfg.as_ref().and_then(|c| c.workspace.clone()),
                    env: cfg.as_ref().and_then(|c| c.env.clone()),
                    needs_auth: !connected && is_oauth,
                    connected,
                    server_id: rec.server_id,
                }
            })
            .collect()
    }
}

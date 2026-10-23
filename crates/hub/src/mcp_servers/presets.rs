//! Curated preset catalog. One source of truth for both the
//! agent-tool surface (`mcp.install preset:linear`) and the REST
//! API used by the Settings UI.
//!
//! Adding a preset is a single edit here — both surfaces pick it
//! up automatically.

use serde::Serialize;
use workforce_common::settings::{McpAuthBinding, McpAuthConfig, McpServerConfig};

use crate::skill_loader::{CatalogEntry, CredManaged, CredScope};

/// One step the operator should follow to set up a preset MCP
/// server. Surfaced by `mcp.list_presets` (agent tool) and
/// `GET /api/mcp/presets` (REST) so Allan can walk users through
/// the flow verbatim and the UI can render guidance next to each
/// vault input.
#[derive(Debug, Clone, Serialize)]
pub struct SetupStep {
    pub title: String,
    pub instructions: String,
    /// External link the operator should open (e.g. the vendor's
    /// OAuth-app registration page).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The vault path this step's value lands at, when applicable.
    /// UI renders the matching credential input alongside the
    /// instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<String>,
}

impl SetupStep {
    fn new(title: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            instructions: instructions.into(),
            url: None,
            vault_path: None,
        }
    }

    fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    fn with_vault_path(mut self, path: impl Into<String>) -> Self {
        self.vault_path = Some(path.into());
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PresetAuth {
    Pat {
        vault_path: &'static str,
    },
    Oauth2 {
        provider: &'static str,
        scopes: &'static [&'static str],
        oauth_app_registration_url: &'static str,
        /// When set, the preset installs as per-user OAuth with a
        /// shared static-PAT fallback at this vault path: each call
        /// uses the caller's own token first, then this shared key,
        /// then fails as unconfigured. `None` is plain per-user OAuth.
        shared_pat_fallback: Option<&'static str>,
    },
    /// OAuth 2.1 for a self-hosted server that advertises its endpoints
    /// via RFC 8414 discovery and issues client credentials via RFC 7591
    /// dynamic registration — so the operator registers no app and seeds
    /// no client_id/secret. Used by Metabase and any future self-hosted
    /// service with an OAuth MCP server.
    Oauth2Dynamic {
        provider: &'static str,
        scopes: &'static [&'static str],
    },
}

/// Token in a templated [`Preset::endpoint`] replaced by the per-instance
/// base URL at install time.
pub const BASE_URL_TOKEN: &str = "{base_url}";

/// Declares that a preset's `endpoint` is templated on a per-deployment
/// base URL read from `vault_path` (e.g. a self-hosted instance URL).
#[derive(Debug, Clone, Copy)]
pub struct EndpointVar {
    pub vault_path: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    /// Endpoint URL. May contain [`BASE_URL_TOKEN`] when `endpoint_var`
    /// is set, resolved per-install from the vault.
    pub endpoint: &'static str,
    pub transport: &'static str,
    pub auth: PresetAuth,
    pub shadows: &'static [&'static str],
    /// `Some` for self-hosted presets whose endpoint is per-deployment.
    pub endpoint_var: Option<EndpointVar>,
}

impl Preset {
    pub fn to_config(&self) -> McpServerConfig {
        let auth = match self.auth {
            PresetAuth::Pat { vault_path } => McpAuthConfig::Pat {
                vault_path: vault_path.to_string(),
            },
            PresetAuth::Oauth2 {
                provider,
                scopes,
                shared_pat_fallback,
                ..
            } => McpAuthConfig::Oauth2 {
                binding: McpAuthBinding::User,
                provider: provider.to_string(),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                shared_pat_fallback: shared_pat_fallback.map(String::from),
                discovery: None,
            },
            PresetAuth::Oauth2Dynamic { provider, scopes } => McpAuthConfig::Oauth2 {
                binding: McpAuthBinding::User,
                provider: provider.to_string(),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                shared_pat_fallback: None,
                // Populated by the install-time discovery + registration
                // step before the row is persisted.
                discovery: None,
            },
        };
        McpServerConfig {
            endpoint: self.endpoint.to_string(),
            transport: self.transport.to_string(),
            auth,
            allowed_hosts: Vec::new(),
            refresh_interval_secs: 3600,
            shadows: self.shadows.iter().map(|s| s.to_string()).collect(),
            fallback_to_builtin: false,
            tools_allow: Vec::new(),
            tools_deny: Vec::new(),
            // Presets install ungrouped (always active); group via mcp.workspace_add.
            workspace: None,
            env: None,
            default: false,
        }
    }

    /// Dynamic presets self-register their OAuth client and discover
    /// endpoints at install time, rather than reusing operator-seeded
    /// static credentials.
    pub fn is_dynamic(&self) -> bool {
        matches!(self.auth, PresetAuth::Oauth2Dynamic { .. })
    }

    /// Resolve the (possibly templated) endpoint against a per-instance
    /// `base_url`. A no-op when the preset has no `endpoint_var`.
    pub fn resolve_endpoint(&self, base_url: &str) -> String {
        self.endpoint
            .replace(BASE_URL_TOKEN, base_url.trim_end_matches('/'))
    }

    pub fn auth_kind(&self) -> &'static str {
        match self.auth {
            PresetAuth::Pat { .. } => "pat",
            PresetAuth::Oauth2 {
                shared_pat_fallback: Some(_),
                ..
            } => "oauth2+pat",
            PresetAuth::Oauth2 { .. } => "oauth2",
            PresetAuth::Oauth2Dynamic { .. } => "oauth2_dynamic",
        }
    }

    /// Vault paths the operator must seed for the install to work. PAT
    /// presets need one (the bearer token); static OAuth presets need
    /// three (client_id, client_secret, redirect_uri); dynamic OAuth
    /// presets need only the per-instance base URL — the client is
    /// self-registered and the redirect URI derived from `public_url`.
    pub fn required_vault_paths(&self) -> Vec<String> {
        match self.auth {
            PresetAuth::Pat { vault_path } => vec![vault_path.to_string()],
            PresetAuth::Oauth2 {
                shared_pat_fallback,
                ..
            } => {
                let base = format!("mcp/v1/{}", self.id);
                let mut paths = vec![
                    format!("{base}/client_id"),
                    format!("{base}/client_secret"),
                    format!("{base}/redirect_uri"),
                ];
                paths.extend(shared_pat_fallback.map(String::from));
                paths
            }
            PresetAuth::Oauth2Dynamic { .. } => self
                .endpoint_var
                .map(|v| vec![v.vault_path.to_string()])
                .unwrap_or_default(),
        }
    }

    pub fn oauth_app_registration_url(&self) -> Option<&'static str> {
        match self.auth {
            PresetAuth::Oauth2 {
                oauth_app_registration_url,
                ..
            } => Some(oauth_app_registration_url),
            PresetAuth::Pat { .. } | PresetAuth::Oauth2Dynamic { .. } => None,
        }
    }

    /// OAuth provider name (matches `oauth_bridge::provider_for_name`).
    /// Used by the UI to compute the OAuth callback URL.
    pub fn oauth_provider_name(&self) -> Option<&'static str> {
        match self.auth {
            PresetAuth::Oauth2 { provider, .. } | PresetAuth::Oauth2Dynamic { provider, .. } => {
                Some(provider)
            }
            PresetAuth::Pat { .. } => None,
        }
    }

    /// Hand-curated walkthrough — every step the operator needs to
    /// take, in order, to go from "nothing" to a working install.
    /// Allan reads these aloud verbatim when guiding a user;
    /// the Settings UI renders each step's `instructions` and `url`
    /// next to the matching credential input.
    ///
    /// `public_url` is the configured `[hub].public_url` base — when
    /// present, the OAuth "register your app" step embeds the
    /// fully-qualified callback URL the operator should paste into
    /// their vendor app config. When absent, the step reminds them
    /// to configure it.
    pub fn setup_steps(&self, public_url: Option<&str>) -> Vec<SetupStep> {
        let id = self.id;
        let label = self.label;
        let redirect_blurb = match (public_url, self.oauth_provider_name()) {
            (Some(base), Some(provider)) => {
                let callback = format!(
                    "{}/api/auth/{provider}/callback",
                    base.trim_end_matches('/')
                );
                format!(
                    "Set the redirect URL on the vendor app to exactly: `{callback}`. \
                     (This value will be pre-filled in the credential form below.)"
                )
            }
            (None, Some(provider)) => format!(
                "Set the redirect URL on the vendor app to \
                 `<HUB_PUBLIC_URL>/api/auth/{provider}/callback`. \
                 Tip: configure `[hub].public_url` in workforce.toml to make this auto-fill."
            ),
            // PAT presets don't reach this branch (they don't call
            // `redirect_blurb`); placeholder kept for completeness.
            (_, None) => String::new(),
        };
        match self.auth {
            PresetAuth::Pat { vault_path } => vec![
                SetupStep::new(
                    format!("Generate a {label} access token"),
                    "Open the vendor's developer settings and create a new \
                     personal-access-token / API key with read access. Copy the \
                     resulting token — you'll paste it next.",
                ),
                SetupStep::new(
                    "Paste the token into the vault",
                    format!(
                        "Set the vault path `{vault_path}` to the token from the \
                         previous step. The Settings UI form below has a row for it; \
                         from Slack, ask Allan: 'set vault {vault_path} to <token>'."
                    ),
                )
                .with_vault_path(vault_path),
                SetupStep::new(
                    "Install the server",
                    format!(
                        "Click 'Install' on the {label} card in the Settings UI, or \
                         from Slack: 'Allan, install the {label} MCP server'."
                    ),
                ),
            ],
            PresetAuth::Oauth2 {
                scopes,
                shared_pat_fallback,
                ..
            } => {
                let app_url = self.oauth_app_registration_url().unwrap_or("");
                let scope_list = scopes.join(", ");
                let base = format!("mcp/v1/{id}");
                let mut steps = vec![
                    SetupStep::new(
                        format!("Register an OAuth app on {label}"),
                        format!(
                            "Open the {label} developer console and create a new \
                             OAuth application. {redirect_blurb} \
                             Required scopes: {scope_list}."
                        ),
                    )
                    .with_url(app_url),
                    SetupStep::new(
                        "Copy the client ID and client secret",
                        "After saving the OAuth app, copy the generated \
                         `client_id` and `client_secret` values. You'll paste them next.",
                    ),
                    SetupStep::new(
                        "Paste credentials into the vault",
                        "Set the three vault paths below: client_id, client_secret, \
                         and redirect_uri (pre-filled when `[hub].public_url` is \
                         configured). The Settings UI form does this in one click; \
                         from Slack, paste each value into the corresponding path.",
                    )
                    .with_vault_path(format!("{base}/client_id")),
                    SetupStep::new(
                        "Install the server".to_string(),
                        format!(
                            "Click 'Install' on the {label} card in the Settings UI, or \
                             from Slack: 'Allan, install the {label} MCP server'. \
                             The first install records the config; if credentials \
                             aren't seeded yet you'll see this same step list again."
                        ),
                    ),
                    SetupStep::new(
                        "Authorize on the vendor's consent screen".to_string(),
                        format!(
                            "Click 'Authorize' on the card (or from Slack: \
                             'Allan, authorize {id}'). A new tab opens to {label}'s \
                             consent screen. Pick the workspace you want to connect, \
                             approve, and the page redirects back to the hub. Tokens \
                             persist automatically; tools become available as \
                             `mcp.{id}.*` immediately."
                        ),
                    ),
                ];
                if let Some(token_path) = shared_pat_fallback {
                    steps.insert(
                        0,
                        SetupStep::new(
                            format!("Choose how to connect {label}"),
                            format!(
                                "{label} offers two setup paths — use either or both. \
                                 Option 1 (per-user): each user authorises individually; \
                                 required when a client mandates per-user access — follow the \
                                 OAuth steps below. Option 2 (shared): one organisation-wide \
                                 key everyone shares; simplest — skip the OAuth/Authorize steps \
                                 and use the final shared-key step instead. A user's own token \
                                 always takes precedence over the shared key."
                            ),
                        ),
                    );
                    steps.push(
                        SetupStep::new(
                            format!("Option 2 (shared): set an organisation-wide {label} key"),
                            format!(
                                "Create a {label} personal API key with the analytics scopes \
                                 and set the vault path `{token_path}`. Every user without their \
                                 own authorisation falls back to this key, with no per-user \
                                 Authorize step. Leave it unset to require per-user OAuth for all."
                            ),
                        )
                        .with_vault_path(token_path),
                    );
                }
                steps
            }
            PresetAuth::Oauth2Dynamic { .. } => {
                let base_step = match self.endpoint_var {
                    Some(var) => SetupStep::new(
                        format!("Set your {label} URL"),
                        format!(
                            "Set the vault path `{}` to your instance's base URL \
                             (e.g. `{}`). From Slack: 'set vault {} to <url>'.",
                            var.vault_path, var.placeholder, var.vault_path
                        ),
                    )
                    .with_vault_path(var.vault_path),
                    None => SetupStep::new(
                        format!("Configure {label}"),
                        "No instance URL is required for this server.",
                    ),
                };
                let authorize_blurb = match public_url {
                    Some(_) => String::new(),
                    None => " Tip: configure `[hub].public_url` in workforce.toml first so \
                              the callback URL resolves."
                        .to_string(),
                };
                vec![
                    base_step,
                    SetupStep::new(
                        "Install the server".to_string(),
                        format!(
                            "Click 'Install' on the {label} card (or from Slack: \
                             'Allan, install the {label} MCP server'). The hub discovers \
                             {label}'s OAuth endpoints and registers itself automatically — \
                             no client ID or secret to copy.{authorize_blurb}"
                        ),
                    ),
                    SetupStep::new(
                        "Authorize on the consent screen".to_string(),
                        format!(
                            "Click 'Authorize' on the card (or from Slack: \
                             'Allan, authorize {id}'). A tab opens to {label}'s consent \
                             screen; approve, and tools become available as `mcp.{id}.*`."
                        ),
                    ),
                ]
            }
        }
    }
}

pub const ALL: &[Preset] = &[
    Preset {
        id: "linear",
        label: "Linear",
        endpoint: "https://mcp.linear.app/mcp",
        transport: "streamable-http",
        auth: PresetAuth::Oauth2 {
            provider: "linear",
            scopes: &["read", "issues:create", "comments:create"],
            oauth_app_registration_url: "https://linear.app/settings/api/applications/new",
            shared_pat_fallback: None,
        },
        shadows: &["linear_*"],
        endpoint_var: None,
    },
    Preset {
        id: "github",
        label: "GitHub",
        endpoint: "https://api.githubcopilot.com/mcp",
        transport: "streamable-http",
        auth: PresetAuth::Pat {
            vault_path: "mcp/v1/github/token",
        },
        shadows: &[],
        endpoint_var: None,
    },
    Preset {
        id: "atlassian",
        label: "Atlassian (Jira + Confluence)",
        endpoint: "https://mcp.atlassian.com/v1/sse",
        transport: "streamable-http",
        auth: PresetAuth::Oauth2 {
            provider: "atlassian",
            scopes: &["read:jira-work", "read:confluence-content.all"],
            oauth_app_registration_url:
                "https://developer.atlassian.com/console/myapps/create-3lo-app/",
            shared_pat_fallback: None,
        },
        shadows: &["jira_*"],
        endpoint_var: None,
    },
    Preset {
        id: "hubspot",
        label: "HubSpot",
        endpoint: "https://mcp.hubspot.com",
        transport: "streamable-http",
        auth: PresetAuth::Oauth2 {
            provider: "hubspot",
            // HubSpot's MCP server determines scopes at consent time
            // from the available tools + user's existing permissions,
            // so the OAuth request is sent without an explicit scope set.
            scopes: &[],
            oauth_app_registration_url: "https://app.hubspot.com/l/mcp-auth-apps/",
            shared_pat_fallback: None,
        },
        shadows: &[],
        endpoint_var: None,
    },
    Preset {
        id: "posthog",
        label: "PostHog",
        endpoint: "https://mcp.posthog.com/mcp",
        transport: "streamable-http",
        auth: PresetAuth::Oauth2 {
            provider: "posthog",
            scopes: &[
                "openid",
                "profile",
                "email",
                "insight:read",
                "insight:write",
                "query:read",
                "query:write",
                "dashboard:read",
                "dashboard:write",
                "session_recording:read",
                "session_recording:write",
                "web_analytics:read",
                "web_analytics:write",
                "person:read",
                "person:write",
                "event_definition:read",
                "event_definition:write",
                "cohort:read",
                "cohort:write",
                "feature_flag:read",
                "feature_flag:write",
                "experiment:read",
                "experiment:write",
                "survey:read",
                "survey:write",
                "error_tracking:read",
                "error_tracking:write",
                "annotation:read",
                "annotation:write",
                "notebook:read",
                "notebook:write",
            ],
            oauth_app_registration_url: "https://posthog.com/docs/api/oauth",
            shared_pat_fallback: Some("mcp/v1/posthog/token"),
        },
        shadows: &[],
        endpoint_var: None,
    },
    Preset {
        id: "metabase",
        label: "Metabase (self-hosted)",
        endpoint: "{base_url}/api/mcp",
        transport: "streamable-http",
        auth: PresetAuth::Oauth2Dynamic {
            provider: "metabase",
            // Metabase derives scopes from the connecting user's
            // permissions at consent time; the request sends none.
            scopes: &[],
        },
        shadows: &["metabase.*"],
        endpoint_var: Some(EndpointVar {
            vault_path: "skills/metabase/site_url",
            label: "Metabase site URL (instance origin)",
            placeholder: "https://metabase.example.com",
        }),
    },
    Preset {
        id: "granola",
        label: "Granola",
        // Granola's hosted MCP server (AI meeting notes: search, transcripts,
        // folders, action items). Auth host differs from the MCP host
        // (mcp-auth.granola.ai), reached via the RFC 9728 protected-resource
        // cascade in `oauth_client::discovery::discover` — no special handling.
        endpoint: "https://mcp.granola.ai/mcp",
        transport: "streamable-http",
        // Fully dynamic: the AS advertises RFC 8414 discovery + RFC 7591
        // registration (registration_endpoint, public clients via
        // token_endpoint_auth_method "none", PKCE S256), so the operator
        // seeds nothing and registers no app — same flow as Metabase.
        auth: PresetAuth::Oauth2Dynamic {
            provider: "granola",
            // `mcp` is the resource scope the protected-resource metadata
            // advertises; `offline_access` is required for a refresh token
            // (DiscoveredProvider sets no extra authorize params). Requires a
            // paid Granola plan. NOT live-verified — if WorkOS rejects `mcp`
            // as invalid_scope at consent, drop to `&["offline_access"]`.
            scopes: &["mcp", "offline_access"],
        },
        shadows: &[],
        endpoint_var: None,
    },
];

pub fn lookup(id: &str) -> Option<&'static Preset> {
    ALL.iter().find(|p| p.id == id)
}

/// Static OAuth client credentials an operator seeds for a preset — offered in
/// the vault editor's dropdown.
const MCP_OAUTH_CLIENT_KEYS: &[(&str, &str)] = &[
    ("client_id", "OAuth client ID"),
    ("client_secret", "OAuth client secret"),
    ("redirect_uri", "OAuth redirect URI"),
];

/// Runtime OAuth material written by the install/authorize flow under
/// `mcp/v1/<id>/`. Recognised by the catalog (never badged unrecognised) but
/// hidden from the dropdown — operators never set these by hand.
const MCP_RUNTIME_TOKEN_KEYS: &[(&str, &str)] = &[
    (
        "access_token",
        "Short-lived OAuth access token (auto-managed)",
    ),
    ("refresh_token", "OAuth refresh token (auto-managed)"),
    ("expires_at", "Access-token expiry timestamp (auto-managed)"),
];

/// Material a dynamic (self-registering) preset writes for itself, in addition
/// to the runtime tokens above — recognised but never hand-entered.
const MCP_DYNAMIC_RUNTIME_KEYS: &[(&str, &str)] = &[
    (
        "client_id",
        "Self-registered OAuth client ID (auto-managed)",
    ),
    (
        "client_secret",
        "Self-registered OAuth client secret (auto-managed)",
    ),
    ("redirect_uri", "OAuth redirect URI (auto-managed)"),
    (
        "registration_access_token",
        "Dynamic-registration management token (auto-managed)",
    ),
];

fn mcp_entry(
    id: &str,
    path: String,
    label: &str,
    description: &str,
    addable: bool,
    managed_by: CredManaged,
    scope: CredScope,
) -> CatalogEntry {
    CatalogEntry {
        id: path.clone(),
        skill: Some(id.to_string()),
        group: id.to_string(),
        label: format!("{id} — {label}"),
        template: path,
        variables: Vec::new(),
        source: "mcp",
        addable,
        scope: scope.as_str(),
        managed_by: managed_by.as_str(),
        per_user_allowed: scope.allows_per_user(),
        required: false,
        description: description.to_string(),
        multiline: false,
    }
}

fn mcp_runtime_entries(id: &str, keys: &[(&str, &str)]) -> Vec<CatalogEntry> {
    keys.iter()
        .map(|&(key, desc)| {
            mcp_entry(
                id,
                format!("mcp/v1/{id}/{key}"),
                desc,
                desc,
                false,
                CredManaged::Oauth,
                CredScope::Either,
            )
        })
        .collect()
}

fn mcp_pat_entry(id: &str, vault_path: &str) -> CatalogEntry {
    mcp_entry(
        id,
        vault_path.to_string(),
        "MCP access token (API key)",
        "MCP server access token (personal access token)",
        true,
        CredManaged::Manual,
        CredScope::Shared,
    )
}

fn mcp_oauth_entries(id: &str, shared_pat_fallback: Option<&str>) -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = MCP_OAUTH_CLIENT_KEYS
        .iter()
        .map(|&(key, label)| {
            mcp_entry(
                id,
                format!("mcp/v1/{id}/{key}"),
                label,
                label,
                true,
                CredManaged::Oauth,
                CredScope::Shared,
            )
        })
        .collect();
    if let Some(token) = shared_pat_fallback {
        entries.push(mcp_entry(
            id,
            token.to_string(),
            "Shared PAT fallback (API key)",
            "Shared personal-access-token fallback used when a caller has no OAuth token",
            true,
            CredManaged::Manual,
            CredScope::Shared,
        ));
    }
    entries.extend(mcp_runtime_entries(id, MCP_RUNTIME_TOKEN_KEYS));
    entries
}

/// Self-registered material (client creds + tokens) a dynamic OAuth server
/// writes for itself — all recognised but never hand-entered.
fn mcp_dynamic_runtime_entries(id: &str) -> Vec<CatalogEntry> {
    let mut entries = mcp_runtime_entries(id, MCP_DYNAMIC_RUNTIME_KEYS);
    entries.extend(mcp_runtime_entries(id, MCP_RUNTIME_TOKEN_KEYS));
    entries
}

fn preset_catalog_entries(preset: &Preset) -> Vec<CatalogEntry> {
    match preset.auth {
        PresetAuth::Pat { vault_path } => vec![mcp_pat_entry(preset.id, vault_path)],
        PresetAuth::Oauth2 {
            shared_pat_fallback,
            ..
        } => mcp_oauth_entries(preset.id, shared_pat_fallback),
        // The only operator-seeded value is the instance base URL, which reuses
        // the built-in skill's path (e.g. skills/metabase/site_url) catalogued
        // there — so emit only the runtime material this preset self-registers.
        PresetAuth::Oauth2Dynamic { .. } => mcp_dynamic_runtime_entries(preset.id),
    }
}

/// Catalog entries for one installed MCP server, derived from its persisted
/// [`McpAuthConfig`] (server id from the install). Lets the vault editor offer
/// credentials for custom (non-preset) servers, not just the curated presets.
pub fn config_catalog_entries(id: &str, auth: &McpAuthConfig) -> Vec<CatalogEntry> {
    match auth {
        McpAuthConfig::Pat { vault_path } => vec![mcp_pat_entry(id, vault_path)],
        // A server that self-registers its client (RFC 7591 dynamic
        // registration, signalled by a discovered `registration_endpoint`)
        // writes client_id/secret itself — so those are runtime material, not
        // hand-entered. Mirrors the static `Oauth2Dynamic` preset treatment.
        McpAuthConfig::Oauth2 {
            shared_pat_fallback,
            discovery,
            ..
        } => {
            let self_registering = discovery
                .as_ref()
                .is_some_and(|d| d.registration_endpoint.is_some());
            if self_registering {
                mcp_dynamic_runtime_entries(id)
            } else {
                mcp_oauth_entries(id, shared_pat_fallback.as_deref())
            }
        }
    }
}

/// Flatten every preset into catalog entries tagged `source: "mcp"`, so the
/// vault editor can offer MCP credentials and distinguish them from built-in
/// skills. Appended to [`crate::skill_loader::credential_catalog`].
pub fn mcp_credential_catalog() -> Vec<CatalogEntry> {
    ALL.iter().flat_map(preset_catalog_entries).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::DiscoveredMetadata;

    #[test]
    fn config_catalog_entries_cover_custom_servers() {
        // Custom PAT server (e.g. a self-hosted notion) → one offerable token
        // at its configured path.
        let pat = config_catalog_entries(
            "notion",
            &McpAuthConfig::Pat {
                vault_path: "mcp/v1/notion/token".to_string(),
            },
        );
        assert_eq!(pat.len(), 1);
        assert_eq!(pat[0].template, "mcp/v1/notion/token");
        assert_eq!(pat[0].source, "mcp");
        assert_eq!(pat[0].managed_by, "manual");
        assert!(pat[0].addable);

        // Static OAuth server (no self-registration) → offerable client creds +
        // a shared-PAT fallback + hidden runtime tokens.
        let oauth = config_catalog_entries(
            "acme",
            &McpAuthConfig::Oauth2 {
                binding: McpAuthBinding::User,
                provider: "acme".to_string(),
                scopes: vec![],
                shared_pat_fallback: Some("mcp/v1/acme/token".to_string()),
                discovery: None,
            },
        );
        let find = |t: &str| oauth.iter().find(|e| e.template == t).expect(t);
        assert!(find("mcp/v1/acme/client_id").addable);
        assert_eq!(find("mcp/v1/acme/client_id").managed_by, "oauth");
        assert!(find("mcp/v1/acme/token").addable);
        assert!(!find("mcp/v1/acme/access_token").addable);
        assert!(oauth.iter().all(|e| e.source == "mcp"));

        // Dynamic (self-registering) OAuth server → client creds are written by
        // the server itself, so they're recognised but NOT offerable.
        let dynamic = config_catalog_entries(
            "metabase-self",
            &McpAuthConfig::Oauth2 {
                binding: McpAuthBinding::User,
                provider: "metabase-self".to_string(),
                scopes: vec![],
                shared_pat_fallback: None,
                discovery: Some(DiscoveredMetadata {
                    authorization_endpoint: "https://mb.example.com/oauth/authorize".to_string(),
                    token_endpoint: "https://mb.example.com/oauth/token".to_string(),
                    registration_endpoint: Some(
                        "https://mb.example.com/oauth/register".to_string(),
                    ),
                    scopes_supported: vec![],
                }),
            },
        );
        assert!(dynamic.iter().all(|e| !e.addable));
        assert!(dynamic
            .iter()
            .any(|e| e.template == "mcp/v1/metabase-self/client_id"));
    }

    #[test]
    fn lookup_returns_known_presets() {
        assert!(lookup("linear").is_some());
        assert!(lookup("github").is_some());
        assert!(lookup("atlassian").is_some());
        assert!(lookup("posthog").is_some());
        assert!(lookup("metabase").is_some());
        assert!(lookup("granola").is_some());
        assert!(lookup("not-a-preset").is_none());
    }

    #[test]
    fn granola_preset_is_dynamic_with_zero_operator_setup() {
        let p = lookup("granola").unwrap();
        assert!(p.is_dynamic());
        assert_eq!(p.auth_kind(), "oauth2_dynamic");
        // Hosted (no endpoint_var) + dynamic registration → the operator
        // seeds nothing at all; the client self-registers at install time.
        assert!(p.required_vault_paths().is_empty());
        assert!(p.oauth_app_registration_url().is_none());
        assert_eq!(p.oauth_provider_name(), Some("granola"));
        assert!(p.shadows.is_empty());
        assert_eq!(p.endpoint, "https://mcp.granola.ai/mcp");
        match p.to_config().auth {
            McpAuthConfig::Oauth2 {
                binding,
                provider,
                scopes,
                shared_pat_fallback,
                ..
            } => {
                assert_eq!(binding, McpAuthBinding::User);
                assert_eq!(provider, "granola");
                assert_eq!(scopes, vec!["mcp", "offline_access"]);
                assert!(shared_pat_fallback.is_none());
            }
            other => panic!("expected Oauth2 config, got {other:?}"),
        }
    }

    #[test]
    fn granola_setup_steps_need_no_operator_input() {
        // First dynamic preset with `endpoint_var: None` — exercise the arm
        // of `setup_steps` that earlier only ran against Metabase (which has
        // an endpoint_var), since `GET /api/mcp/presets` serializes this for
        // every preset.
        let steps = lookup("granola")
            .unwrap()
            .setup_steps(Some("https://hub.example"));
        assert!(
            !steps.is_empty(),
            "dynamic preset still needs a walkthrough"
        );
        // Self-registering + hosted → nothing for the operator to seed or open.
        assert!(steps.iter().all(|s| s.vault_path.is_none()));
        assert!(steps.iter().all(|s| s.url.is_none()));
    }

    #[test]
    fn metabase_preset_is_dynamic_self_hosted_oauth() {
        let p = lookup("metabase").unwrap();
        assert!(p.is_dynamic());
        assert_eq!(p.auth_kind(), "oauth2_dynamic");
        // Dynamic registration → no operator app, only the instance URL.
        assert_eq!(p.required_vault_paths(), vec!["skills/metabase/site_url"]);
        assert!(p.oauth_app_registration_url().is_none());
        assert_eq!(p.oauth_provider_name(), Some("metabase"));
        assert_eq!(p.shadows, &["metabase.*"]);
    }

    #[test]
    fn metabase_endpoint_resolves_from_base_url() {
        let p = lookup("metabase").unwrap();
        assert_eq!(
            p.resolve_endpoint("https://mb.example.com/"),
            "https://mb.example.com/api/mcp"
        );
    }

    #[test]
    fn metabase_setup_steps_have_no_oauth_app_step() {
        let steps = lookup("metabase")
            .unwrap()
            .setup_steps(Some("https://hub.example"));
        assert!(steps
            .iter()
            .any(|s| s.vault_path.as_deref() == Some("skills/metabase/site_url")));
        for step in &steps {
            assert!(
                step.url.is_none(),
                "dynamic preset needs no external app-registration link: {:?}",
                step.title
            );
            assert!(
                !step.instructions.contains("client_id")
                    && !step.instructions.contains("client_secret"),
                "dynamic preset must not ask for client credentials: {:?}",
                step.instructions
            );
        }
    }

    #[test]
    fn linear_preset_yields_oauth_config() {
        let p = lookup("linear").unwrap();
        let cfg = p.to_config();
        assert_eq!(cfg.endpoint, "https://mcp.linear.app/mcp");
        match cfg.auth {
            McpAuthConfig::Oauth2 { provider, .. } => assert_eq!(provider, "linear"),
            _ => panic!("expected OAuth2"),
        }
    }

    #[test]
    fn github_preset_yields_pat_config() {
        let p = lookup("github").unwrap();
        let cfg = p.to_config();
        match cfg.auth {
            McpAuthConfig::Pat { vault_path } => {
                assert_eq!(vault_path, "mcp/v1/github/token")
            }
            _ => panic!("expected PAT"),
        }
    }

    #[test]
    fn required_vault_paths_match_auth_shape() {
        assert_eq!(
            lookup("linear").unwrap().required_vault_paths(),
            vec![
                "mcp/v1/linear/client_id",
                "mcp/v1/linear/client_secret",
                "mcp/v1/linear/redirect_uri",
            ]
        );
        assert_eq!(
            lookup("github").unwrap().required_vault_paths(),
            vec!["mcp/v1/github/token"]
        );
    }

    #[test]
    fn linear_setup_steps_embed_redirect_uri_when_provided() {
        let steps = lookup("linear")
            .unwrap()
            .setup_steps(Some("https://hub.example"));
        assert!(!steps.is_empty(), "OAuth preset must have walkthrough");
        let register_step = &steps[0];
        assert!(
            register_step
                .instructions
                .contains("https://hub.example/api/auth/linear/callback"),
            "register step should include the verbatim callback URL: {:?}",
            register_step.instructions
        );
        assert_eq!(
            register_step.url.as_deref(),
            Some("https://linear.app/settings/api/applications/new")
        );
    }

    #[test]
    fn linear_setup_steps_prompt_to_set_public_url_when_missing() {
        let steps = lookup("linear").unwrap().setup_steps(None);
        let first = &steps[0];
        assert!(
            first.instructions.contains("[hub].public_url"),
            "operator should be reminded to configure public_url: {:?}",
            first.instructions
        );
    }

    #[test]
    fn github_setup_steps_are_pat_only_no_oauth_steps() {
        let steps = lookup("github").unwrap().setup_steps(None);
        assert!(
            !steps.is_empty(),
            "PAT preset still needs a token-generation step"
        );
        for step in &steps {
            assert!(
                !step.instructions.to_lowercase().contains("oauth"),
                "PAT preset should not mention OAuth: {:?}",
                step.instructions
            );
        }
        // Token vault path is referenced.
        assert!(steps
            .iter()
            .any(|s| s.vault_path.as_deref() == Some("mcp/v1/github/token")));
    }

    #[test]
    fn atlassian_setup_steps_use_atlassian_app_url() {
        let steps = lookup("atlassian")
            .unwrap()
            .setup_steps(Some("https://hub.example"));
        let register_step = &steps[0];
        assert_eq!(
            register_step.url.as_deref(),
            Some("https://developer.atlassian.com/console/myapps/create-3lo-app/")
        );
        assert!(register_step
            .instructions
            .contains("https://hub.example/api/auth/atlassian/callback"));
    }

    #[test]
    fn posthog_preset_is_layered_oauth_with_shared_pat() {
        let p = lookup("posthog").unwrap();
        assert_eq!(p.auth_kind(), "oauth2+pat");
        match p.to_config().auth {
            McpAuthConfig::Oauth2 {
                binding,
                provider,
                shared_pat_fallback,
                ..
            } => {
                assert_eq!(binding, McpAuthBinding::User);
                assert_eq!(provider, "posthog");
                assert_eq!(shared_pat_fallback.as_deref(), Some("mcp/v1/posthog/token"));
            }
            _ => panic!("expected layered OAuth2"),
        }
    }

    #[test]
    fn posthog_required_vault_paths_append_shared_token() {
        assert_eq!(
            lookup("posthog").unwrap().required_vault_paths(),
            vec![
                "mcp/v1/posthog/client_id",
                "mcp/v1/posthog/client_secret",
                "mcp/v1/posthog/redirect_uri",
                "mcp/v1/posthog/token",
            ]
        );
    }

    #[test]
    fn posthog_setup_steps_present_both_options() {
        let steps = lookup("posthog")
            .unwrap()
            .setup_steps(Some("https://hub.example"));
        assert!(
            steps[0].instructions.contains("per-user") && steps[0].instructions.contains("shared"),
            "first step should frame the two options: {:?}",
            steps[0].instructions
        );
        assert!(
            steps
                .iter()
                .any(|s| s.vault_path.as_deref() == Some("mcp/v1/posthog/token")),
            "a step must target the shared-token vault path"
        );
    }
}

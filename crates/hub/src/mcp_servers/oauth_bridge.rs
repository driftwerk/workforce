//! OAuth-2.1 bridge for MCP. Maps the `provider` name from
//! `[mcp.servers.<id>].auth.provider` config to a concrete
//! [`OAuthProvider`] impl, plus the `PathLayout` MCP servers use.
//!
//! Adding a new provider is two lines: a struct implementing
//! [`OAuthProvider`] and a match arm in [`provider_for_name`].

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use workforce_common::settings::DiscoveredMetadata;
use workforce_oauth_client::{DiscoveredProvider, OAuthProvider, PathLayout};

/// Vault path layout for MCP OAuth credentials. `target` is the
/// MCP server id (e.g. `"linear"`); together with the `mcp/v1/`
/// prefix this yields paths like `mcp/v1/linear/access_token`.
/// When the underlying store is a `UserAwareVault::for_user(uid)`,
/// per-user persistence transparently lands at
/// `users/{uid}/mcp/v1/linear/access_token`.
#[derive(Debug)]
pub struct McpVaultLayout;

impl PathLayout for McpVaultLayout {
    fn path(&self, target: &str, suffix: &str) -> String {
        format!("mcp/v1/{target}/{suffix}")
    }
}

/// Resolve a `provider` string from config to an [`OAuthProvider`]
/// implementation. Unknown names return `Err` so misconfigured
/// servers fail loudly at startup rather than silently using
/// wrong endpoints.
///
/// Tests can register a custom provider for an arbitrary name via
/// [`register_test_provider`] (intended for the manager
/// integration tests in `crates/hub/tests/mcp_manager.rs` — the
/// OAuth `code` exchange goes through a wiremock server, so the
/// endpoints can't be the real Linear ones). The override map is a
/// process-wide static; tests that touch it must clean up via
/// `unregister_test_provider` when finished or use unique names.
pub fn provider_for_name(name: &str) -> Result<Arc<dyn OAuthProvider>> {
    if let Some(p) = test_overrides::lookup(name) {
        return Ok(p);
    }
    match name {
        "linear" => Ok(Arc::new(LinearOAuthProvider)),
        "atlassian" => Ok(Arc::new(AtlassianOAuthProvider)),
        "hubspot" => Ok(Arc::new(HubspotOAuthProvider)),
        "posthog" => Ok(Arc::new(PostHogOAuthProvider)),
        other => Err(eyre::eyre!(
            "unknown MCP OAuth provider `{other}` — built-in providers: linear, atlassian, hubspot, posthog. \
             For any other OAuth 2.1 / OIDC server, you have two options, neither needs a built-in provider: \
             (1) `mcp.install` custom with `discover: true` to auto-resolve the endpoints AND auto-register a \
             client — if the MCP endpoint doesn't serve a `.well-known` doc (e.g. Keycloak fronts the API on a \
             different host), also pass `issuer` with the OIDC realm URL (e.g. \
             `https://auth.example/realms/x`); or (2) skip discovery and provide the endpoints directly in \
             `auth.discovery` (`authorization_endpoint` + `token_endpoint`), which requires a pre-seeded \
             OAuth client in the vault at `mcp/v1/<server>/`."
        )),
    }
}

/// Resolve the [`OAuthProvider`] for a configured MCP server. When the
/// config carries discovered metadata (self-hosted servers using RFC 8414
/// discovery, e.g. Metabase) the endpoints come from that cache; otherwise
/// `name` selects a statically-defined provider.
pub fn provider_for_config(
    name: &str,
    discovery: Option<&DiscoveredMetadata>,
) -> Result<Arc<dyn OAuthProvider>> {
    match discovery {
        Some(meta) => Ok(Arc::new(DiscoveredProvider::new(
            name,
            &meta.authorization_endpoint,
            &meta.token_endpoint,
        ))),
        None => provider_for_name(name),
    }
}

/// Test hook — register a custom [`OAuthProvider`] under `name` so
/// `provider_for_name(name)` returns it instead of the built-in
/// implementation. Available outside `#[cfg(test)]` so integration
/// tests in sibling test crates (`crates/hub/tests/*.rs`) can use
/// it; production code never calls this.
pub fn register_test_provider(name: &str, provider: Arc<dyn OAuthProvider>) {
    test_overrides::insert(name, provider);
}

/// Reverse of [`register_test_provider`].
pub fn unregister_test_provider(name: &str) {
    test_overrides::remove(name);
}

mod test_overrides {
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock, RwLock};

    use workforce_oauth_client::OAuthProvider;

    fn map() -> &'static RwLock<HashMap<String, Arc<dyn OAuthProvider>>> {
        static MAP: OnceLock<RwLock<HashMap<String, Arc<dyn OAuthProvider>>>> = OnceLock::new();
        MAP.get_or_init(|| RwLock::new(HashMap::new()))
    }

    pub fn insert(name: &str, provider: Arc<dyn OAuthProvider>) {
        map()
            .write()
            .expect("oauth_bridge test override lock")
            .insert(name.to_string(), provider);
    }

    pub fn remove(name: &str) {
        map()
            .write()
            .expect("oauth_bridge test override lock")
            .remove(name);
    }

    pub fn lookup(name: &str) -> Option<Arc<dyn OAuthProvider>> {
        map()
            .read()
            .expect("oauth_bridge test override lock")
            .get(name)
            .cloned()
    }
}

struct LinearOAuthProvider;

#[async_trait]
impl OAuthProvider for LinearOAuthProvider {
    fn name(&self) -> &str {
        "linear"
    }
    fn authorization_endpoint(&self) -> &str {
        "https://linear.app/oauth/authorize"
    }
    fn token_endpoint(&self) -> &str {
        "https://api.linear.app/oauth/token"
    }
    fn default_scopes(&self) -> &[&str] {
        &["read", "write"]
    }
}

struct AtlassianOAuthProvider;

#[async_trait]
impl OAuthProvider for AtlassianOAuthProvider {
    fn name(&self) -> &str {
        "atlassian"
    }
    fn authorization_endpoint(&self) -> &str {
        "https://auth.atlassian.com/authorize"
    }
    fn token_endpoint(&self) -> &str {
        "https://auth.atlassian.com/oauth/token"
    }
    fn default_scopes(&self) -> &[&str] {
        &["read:jira-work", "read:confluence-content.all"]
    }
    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        // Atlassian's OAuth 2.1 (3LO) requires `audience=api.atlassian.com`
        // and `prompt=consent` so the consent screen always shows scope
        // changes — without these the authorize endpoint silently 400s.
        vec![("audience", "api.atlassian.com"), ("prompt", "consent")]
    }
}

struct HubspotOAuthProvider;

#[async_trait]
impl OAuthProvider for HubspotOAuthProvider {
    fn name(&self) -> &str {
        "hubspot"
    }
    fn authorization_endpoint(&self) -> &str {
        // From HubSpot's OAuth Authorization Server Metadata at
        // https://mcp.hubspot.com/.well-known/oauth-authorization-server
        "https://mcp.hubspot.com/oauth/authorize/user"
    }
    fn token_endpoint(&self) -> &str {
        "https://mcp.hubspot.com/oauth/v3/token"
    }
    fn default_scopes(&self) -> &[&str] {
        // HubSpot's MCP server reports `scopes_supported: []` — scopes
        // are derived at consent time from the tools available and the
        // user's existing HubSpot permissions, not declared up-front.
        &[]
    }
}

struct PostHogOAuthProvider;

#[async_trait]
impl OAuthProvider for PostHogOAuthProvider {
    fn name(&self) -> &str {
        "posthog"
    }
    fn authorization_endpoint(&self) -> &str {
        // From PostHog's OAuth Authorization Server Metadata at
        // https://oauth.posthog.com/.well-known/oauth-authorization-server
        // (mcp.posthog.com 302-redirects there). Trailing slash is
        // significant — the endpoints reject the unslashed form.
        "https://oauth.posthog.com/oauth/authorize/"
    }
    fn token_endpoint(&self) -> &str {
        "https://oauth.posthog.com/oauth/token/"
    }
    fn default_scopes(&self) -> &[&str] {
        // Conservative fallback; the preset passes an explicit
        // analytics scope set, so these only apply to a bare
        // hand-rolled `[mcp.servers.*]` config.
        &["openid", "profile", "email"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_layout_yields_versioned_paths() {
        let layout = McpVaultLayout;
        assert_eq!(
            layout.path("linear", "access_token"),
            "mcp/v1/linear/access_token"
        );
        assert_eq!(
            layout.path("github", "refresh_token"),
            "mcp/v1/github/refresh_token"
        );
    }

    #[test]
    fn provider_for_name_returns_linear() {
        let p = provider_for_name("linear").unwrap();
        assert_eq!(p.name(), "linear");
    }

    #[test]
    fn provider_for_name_returns_posthog() {
        let p = provider_for_name("posthog").unwrap();
        assert_eq!(p.name(), "posthog");
        assert!(p.authorization_endpoint().ends_with('/'));
        assert!(p.token_endpoint().ends_with('/'));
    }

    #[test]
    fn provider_for_name_rejects_unknown() {
        let err = provider_for_name("definitely-not-a-provider")
            .err()
            .expect("unknown provider must error");
        assert!(err.to_string().contains("unknown"));
    }
}

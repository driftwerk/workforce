//! Provider trait — endpoint URLs + scope defaults + optional userinfo.
//! Each integration (Google, Linear, Atlassian, …) implements this once
//! and reuses the generic [`crate::OAuthClient`] flow code.

use async_trait::async_trait;
use eyre::Result;

/// How the client authenticates itself at the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuth {
    /// `client_id` (+ `client_secret`) in the form body — the OAuth-2.1
    /// default used by Google and the MCP flow.
    RequestBody,
    /// HTTP Basic auth header (`client_id` as username, `client_secret`
    /// as password). FreeAgent documents only this scheme.
    Basic,
}

#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Stable provider identifier — used in vault paths, log fields,
    /// and the `OAuthStateStore` entry. Must be lowercase, no
    /// whitespace, suitable for filesystem-style paths.
    fn name(&self) -> &str;

    /// Authorization endpoint (where the user is redirected to).
    fn authorization_endpoint(&self) -> &str;

    /// Token endpoint (where `code → tokens` and `refresh_token → tokens`
    /// exchanges go).
    fn token_endpoint(&self) -> &str;

    /// Default scopes used when the caller's scope list is empty.
    fn default_scopes(&self) -> &[&str];

    /// Provider-specific authorize-URL query parameters appended in
    /// addition to the OAuth-2.1 standard set (client_id, redirect_uri,
    /// response_type, scope, code_challenge, code_challenge_method,
    /// state). Google needs `access_type=offline` + `prompt=consent`
    /// to issue refresh tokens; most other providers leave this empty.
    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        Vec::new()
    }

    /// Whether the provider implements PKCE. On by default; providers
    /// that don't (FreeAgent) return `false`, and the `code_challenge`
    /// authorize param and `code_verifier` exchange param are omitted.
    fn uses_pkce(&self) -> bool {
        true
    }

    /// Token-endpoint client authentication scheme. `RequestBody` by
    /// default (Google, MCP); `Basic` for providers that require HTTP
    /// Basic auth (FreeAgent).
    fn client_auth(&self) -> ClientAuth {
        ClientAuth::RequestBody
    }

    /// Best-effort userinfo lookup (typically returns the user's
    /// email) using the freshly-issued access token. Implementations
    /// that don't expose userinfo can return `Ok(None)`.
    async fn fetch_userinfo(
        &self,
        _http: &reqwest::Client,
        _access_token: &str,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

/// An [`OAuthProvider`] whose endpoints are supplied at runtime — e.g.
/// from RFC 8414 discovery against a self-hosted instance — rather than
/// hardcoded. Scopes always come from the caller's config, so
/// `default_scopes` is empty.
pub struct DiscoveredProvider {
    name: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

impl DiscoveredProvider {
    pub fn new(
        name: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            authorization_endpoint: authorization_endpoint.into(),
            token_endpoint: token_endpoint.into(),
        }
    }
}

#[async_trait]
impl OAuthProvider for DiscoveredProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }
    fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }
    fn default_scopes(&self) -> &[&str] {
        &[]
    }
}

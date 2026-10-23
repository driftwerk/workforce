//! [`workforce_mcp_client::AuthProvider`] implementations bridging
//! into workforce's secret stores.
//!
//! Two flavours match the two `McpAuthConfig` variants:
//!
//! - [`PatAuth`] reads a static bearer token from `LayeredUserVault`
//!   on every call so a per-user override at
//!   `users/{uid}/<vault_path>` wins over the agent-shared default.
//! - [`TokenStoreAuth`] delegates to a
//!   [`workforce_oauth_client::TokenStore`] which handles refresh
//!   and rotation transparently. Vault scoping (per-user vs
//!   shared) is decided at request time from the active
//!   `CURRENT_USER_ID` task-local and the configured
//!   [`McpAuthBinding`].

use std::sync::Arc;

use async_trait::async_trait;
use workforce_common::settings::{McpAuthBinding, SecretStore};
use workforce_mcp_client::{AuthProvider, McpError};
use workforce_oauth_client::{OAuthClient, PathLayout, TokenStore};

use super::oauth_bridge::McpVaultLayout;
use crate::user_config::{LayeredUserVault, UserAwareVault};
use crate::vault::Vault;

/// Resolves a personal-access-token credential from the vault on
/// every call. Cheap; the underlying store does its own caching
/// where appropriate. Per-user override at `users/{uid}/<path>`
/// wins via `LayeredUserVault`.
pub struct PatAuth {
    vault: LayeredUserVault,
    vault_path: String,
}

impl PatAuth {
    pub fn new(vault: LayeredUserVault, vault_path: impl Into<String>) -> Self {
        Self {
            vault,
            vault_path: vault_path.into(),
        }
    }
}

#[async_trait]
impl AuthProvider for PatAuth {
    async fn authorization_header(&self) -> Result<String, McpError> {
        match self.vault.get(&self.vault_path).filter(|t| !t.is_empty()) {
            Some(token) => Ok(format!("Bearer {token}")),
            None => Err(McpError::AuthExpired(format!(
                "no MCP credential at vault path `{}`",
                self.vault_path
            ))),
        }
    }
}

/// Tries each inner provider in order on every call and returns the
/// first that resolves a credential. Backs one MCP server with
/// layered auth — the caller's per-user OAuth token first, then a
/// shared static PAT — so a single server serves both per-user and
/// shared deployments. When every layer is unconfigured the last
/// layer's error propagates, surfacing as the usual "not authorised"
/// call failure.
pub struct LayeredAuth {
    layers: Vec<Arc<dyn AuthProvider>>,
}

impl LayeredAuth {
    pub fn new(layers: Vec<Arc<dyn AuthProvider>>) -> Self {
        Self { layers }
    }
}

#[async_trait]
impl AuthProvider for LayeredAuth {
    async fn authorization_header(&self) -> Result<String, McpError> {
        let mut last = None;
        for layer in &self.layers {
            match layer.authorization_header().await {
                Ok(header) => return Ok(header),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| McpError::AuthExpired("no auth layers configured".into())))
    }
}

/// OAuth-backed bearer source. Constructs a per-user-scoped
/// `TokenStore` lazily on each call so refresh + rotation
/// land at `users/{uid}/mcp/v1/<server_id>/...` (User binding)
/// or `mcp/v1/<server_id>/...` (Shared binding). A User binding reads only
/// the caller's own token, never a shared one. Refreshes are single-flight
/// across every hub through the vault's refresh lock.
pub struct TokenStoreAuth {
    server_id: String,
    binding: McpAuthBinding,
    oauth_client: Arc<OAuthClient>,
    global_vault: Arc<Vault>,
    layout: Arc<dyn PathLayout>,
}

impl TokenStoreAuth {
    pub fn new(
        server_id: impl Into<String>,
        binding: McpAuthBinding,
        oauth_client: Arc<OAuthClient>,
        global_vault: Arc<Vault>,
    ) -> Self {
        Self {
            server_id: server_id.into(),
            binding,
            oauth_client,
            global_vault,
            layout: Arc::new(McpVaultLayout),
        }
    }

    fn build_token_store(&self) -> Result<TokenStore, McpError> {
        let store: Arc<dyn SecretStore> = match self.binding {
            McpAuthBinding::User => {
                let uid = workforce_agent::runner::CURRENT_USER_ID
                    .try_with(|u| u.clone())
                    .ok()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| {
                        McpError::AuthExpired(format!(
                            "MCP server `{}` requires per-user auth but no current user identity is set",
                            self.server_id
                        ))
                    })?;
                Arc::new(UserAwareVault::for_user_strict(
                    Arc::clone(&self.global_vault),
                    uid,
                ))
            }
            McpAuthBinding::Shared => {
                Arc::new(UserAwareVault::global(Arc::clone(&self.global_vault)))
            }
        };
        Ok(TokenStore::new(
            store,
            self.server_id.clone(),
            Arc::clone(&self.layout),
            Arc::clone(&self.oauth_client),
        ))
    }
}

#[async_trait]
impl AuthProvider for TokenStoreAuth {
    async fn authorization_header(&self) -> Result<String, McpError> {
        let store = self.build_token_store()?;
        match store.access_token().await {
            Ok(token) => Ok(format!("Bearer {token}")),
            Err(e) => Err(McpError::AuthExpired(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubAuth(Result<String, String>);

    #[async_trait]
    impl AuthProvider for StubAuth {
        async fn authorization_header(&self) -> Result<String, McpError> {
            self.0.clone().map_err(McpError::AuthExpired)
        }
    }

    fn ok(h: &str) -> Arc<dyn AuthProvider> {
        Arc::new(StubAuth(Ok(h.to_string())))
    }
    fn err(msg: &str) -> Arc<dyn AuthProvider> {
        Arc::new(StubAuth(Err(msg.to_string())))
    }

    #[tokio::test]
    async fn layered_returns_first_success() {
        let auth = LayeredAuth::new(vec![ok("first"), ok("second")]);
        assert_eq!(auth.authorization_header().await.unwrap(), "first");
    }

    #[tokio::test]
    async fn layered_falls_through_to_next_on_error() {
        let auth = LayeredAuth::new(vec![err("no user token"), ok("shared")]);
        assert_eq!(auth.authorization_header().await.unwrap(), "shared");
    }

    #[tokio::test]
    async fn layered_propagates_last_error_when_all_fail() {
        let auth = LayeredAuth::new(vec![err("no user token"), err("no shared key")]);
        let e = auth.authorization_header().await.unwrap_err();
        assert!(e.to_string().contains("no shared key"));
    }
}

//! Spotify token storage, delegating to the generic `workforce-oauth-client`
//! [`GenericTokenStore`] for lazy refresh, rotated refresh-token persistence,
//! and single-flight refresh coordination.
//!
//! Unlike FreeAgent, which connects one company per workspace, a Spotify
//! connection is simply the person's own account — so there is one target,
//! [`ACCOUNT`], and the per-user `users/{uid}/` segment the layered vault adds
//! is what keeps two people's music apart.

use std::sync::Arc;

use workforce_common::settings::SecretStore;
use workforce_oauth_client::{
    OAuthClient, OAuthClientCreds, OAuthProvider, PathLayout, TokenStore as GenericTokenStore,
};

use super::provider::SpotifyProvider;

/// Vault prefix under which the connected account's tokens live.
pub const ACCOUNT_PREFIX: &str = "skills/spotify/account/";

/// The single connection target. Named rather than empty so the vault paths
/// read as sentences and leave room for a second account later.
pub const ACCOUNT: &str = "me";

/// Suffix marking the account as connected.
const REFRESH_SUFFIX: &str = "/refresh_token";

#[derive(Debug)]
struct SpotifyLayout;

impl PathLayout for SpotifyLayout {
    fn path(&self, target: &str, suffix: &str) -> String {
        format!("{ACCOUNT_PREFIX}{target}/{suffix}")
    }
}

/// Build the token store for the connected account.
pub fn build_token_store(
    store: Arc<dyn SecretStore>,
    creds: OAuthClientCreds,
    http: reqwest::Client,
) -> GenericTokenStore {
    token_store_with_provider(store, creds, http, Arc::new(SpotifyProvider::new()))
}

/// Build against an explicit provider — tests point one at a mock token
/// endpoint.
pub(crate) fn token_store_with_provider(
    store: Arc<dyn SecretStore>,
    creds: OAuthClientCreds,
    http: reqwest::Client,
    provider: Arc<dyn OAuthProvider>,
) -> GenericTokenStore {
    let oauth = Arc::new(OAuthClient::new(provider, creds, http));
    let layout: Arc<dyn PathLayout> = Arc::new(SpotifyLayout);
    GenericTokenStore::new(store, ACCOUNT.to_string(), layout, oauth)
}

/// Whether an account has been connected, by looking for a persisted refresh
/// token.
pub fn is_connected(store: &dyn SecretStore) -> bool {
    store
        .get(&format!("{ACCOUNT_PREFIX}{ACCOUNT}{REFRESH_SUFFIX}"))
        .is_some_and(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;

    #[test]
    fn tokens_are_namespaced_under_the_account() {
        let layout = SpotifyLayout;
        assert_eq!(
            layout.path(ACCOUNT, "refresh_token"),
            "skills/spotify/account/me/refresh_token"
        );
    }

    #[test]
    fn an_account_is_connected_once_a_refresh_token_is_stored() {
        assert!(!is_connected(InMem::from(&[]).as_ref()));
        assert!(is_connected(
            InMem::from(&[("skills/spotify/account/me/refresh_token", "rt")]).as_ref()
        ));
    }

    /// A blank token is what a half-finished authorization leaves behind, and
    /// reading it as "connected" sends every later call to a 401 instead of
    /// telling the person to connect.
    #[test]
    fn a_blank_refresh_token_is_not_a_connection() {
        assert!(!is_connected(
            InMem::from(&[("skills/spotify/account/me/refresh_token", "   ")]).as_ref()
        ));
    }
}

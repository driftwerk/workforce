//! Per-workspace access-token cache with lazy refresh, delegating to
//! `workforce-oauth-client`'s generic `TokenStore`.
//!
//! The migration to `oauth-client` fixes a pre-existing bug: the
//! original `refresh_now` here only persisted `access_token` and
//! `expires_at` and silently dropped any rotated refresh token Google
//! returned. OAuth 2.1 mandates RT rotation for many providers and
//! Google itself rotates in some flows; the generic `TokenStore` now
//! persists the rotated RT atomically (refresh-token-first, then
//! access-token, then expires-at, then scopes).
//!
//! Vault paths are unchanged — `skills/google/workspaces/<workspace>/...`
//! — so existing users see no behaviour change other than the bug fix.

use std::sync::Arc;

use eyre::Result;
use workforce_common::settings::SecretStore;
use workforce_oauth_client::{
    Clock, OAuthClient, OAuthClientCreds as OAuthClientCredsGeneric, PathLayout,
    TokenStore as GenericTokenStore,
};

use super::oauth::{google_provider, OAuthClientCreds};

pub use workforce_oauth_client::REFRESH_SLACK_SECS;

#[derive(Debug)]
struct GoogleLayout;

impl PathLayout for GoogleLayout {
    fn path(&self, target: &str, suffix: &str) -> String {
        format!("skills/google/workspaces/{target}/{suffix}")
    }
}

/// Per-workspace access-token cache with lazy refresh.
#[derive(Debug)]
pub struct TokenStore {
    inner: GenericTokenStore,
}

impl TokenStore {
    pub fn new(
        store: Arc<dyn SecretStore>,
        workspace: String,
        client: OAuthClientCreds,
        http: reqwest::Client,
    ) -> Self {
        let creds = OAuthClientCredsGeneric {
            client_id: client.client_id,
            client_secret: Some(client.client_secret),
            redirect_uri: client.redirect_uri,
        };
        let oauth_client = Arc::new(OAuthClient::new(google_provider(), creds, http));
        let layout: Arc<dyn PathLayout> = Arc::new(GoogleLayout);
        let inner = GenericTokenStore::new(store, workspace, layout, oauth_client);
        Self { inner }
    }

    /// Inject a custom clock — for unit tests.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.inner = self.inner.with_clock(clock);
        self
    }

    pub fn workspace(&self) -> &str {
        self.inner.target()
    }

    /// Return a valid access token, refreshing if missing, expired, or
    /// within `REFRESH_SLACK_SECS` of expiry.
    pub async fn access_token(&self) -> Result<String> {
        self.inner.access_token().await
    }

    /// Force a refresh regardless of cache state (used on 401 retries).
    pub async fn force_refresh(&self) -> Result<String> {
        self.inner.force_refresh().await
    }

    /// Sanity check: do we have a refresh token on disk?
    pub fn is_authenticated(&self) -> bool {
        self.inner.is_authenticated()
    }

    /// Human-readable status summary for the setup tool / CLI `list`.
    pub fn status(&self) -> TokenStatus {
        let s = self.inner.status();
        TokenStatus {
            workspace: s.target,
            email: s.email,
            has_refresh: s.has_refresh,
            expires_at: s.expires_at,
            scopes: s.scopes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenStatus {
    pub workspace: String,
    pub email: Option<String>,
    pub has_refresh: bool,
    pub expires_at: Option<u64>,
    pub scopes: Option<String>,
}

/// Return the list of configured workspace names by scanning the secret store.
pub fn list_workspaces(store: &dyn SecretStore) -> Vec<String> {
    let prefix = "skills/google/workspaces/";
    let suffix = "/refresh_token";
    let mut out: Vec<String> = store
        .list(Some(prefix))
        .into_iter()
        .filter_map(|k| {
            k.strip_prefix(prefix)
                .and_then(|s| s.strip_suffix(suffix))
                .map(|s| s.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Minimal in-memory SecretStore for unit tests.
    #[derive(Debug, Default)]
    struct MemStore {
        map: StdMutex<HashMap<String, String>>,
    }

    impl SecretStore for MemStore {
        fn get(&self, path: &str) -> Option<String> {
            self.map.lock().unwrap().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> Result<()> {
            self.map
                .lock()
                .unwrap()
                .insert(path.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, path: &str) -> Result<bool> {
            Ok(self.map.lock().unwrap().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let m = self.map.lock().unwrap();
            match prefix {
                Some(p) => m.keys().filter(|k| k.starts_with(p)).cloned().collect(),
                None => m.keys().cloned().collect(),
            }
        }
    }

    fn fixture() -> (Arc<MemStore>, TokenStore) {
        let store: Arc<MemStore> = Arc::new(MemStore::default());
        store
            .set("skills/google/workspaces/default/refresh_token", "rt-abc")
            .unwrap();
        store
            .set("skills/google/workspaces/default/access_token", "at-old")
            .unwrap();
        let far_future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 100_000;
        store
            .set(
                "skills/google/workspaces/default/expires_at",
                &far_future.to_string(),
            )
            .unwrap();

        let creds = OAuthClientCreds {
            client_id: "x".into(),
            client_secret: "y".into(),
            redirect_uri: None,
        };
        let ts = TokenStore::new(
            store.clone() as Arc<dyn SecretStore>,
            "default".into(),
            creds,
            reqwest::Client::new(),
        );
        (store, ts)
    }

    #[tokio::test]
    async fn access_token_returned_when_fresh() {
        let (_store, ts) = fixture();
        let tok = ts.access_token().await.unwrap();
        assert_eq!(tok, "at-old");
    }

    #[tokio::test]
    async fn status_reports_has_refresh() {
        let (_store, ts) = fixture();
        let s = ts.status();
        assert!(s.has_refresh);
        assert_eq!(s.workspace, "default");
    }

    #[tokio::test]
    async fn unauthenticated_when_no_refresh_token() {
        let store: Arc<MemStore> = Arc::new(MemStore::default());
        let creds = OAuthClientCreds {
            client_id: "x".into(),
            client_secret: "y".into(),
            redirect_uri: None,
        };
        let ts = TokenStore::new(
            store as Arc<dyn SecretStore>,
            "default".into(),
            creds,
            reqwest::Client::new(),
        );
        assert!(!ts.is_authenticated());
    }

    #[test]
    fn list_workspaces_finds_configured() {
        let store = MemStore::default();
        store
            .set("skills/google/workspaces/work/refresh_token", "r1")
            .unwrap();
        store
            .set("skills/google/workspaces/personal/refresh_token", "r2")
            .unwrap();
        store
            .set("skills/google/workspaces/personal/email", "me@x")
            .unwrap();
        let ws = list_workspaces(&store);
        assert_eq!(ws, vec!["personal", "work"]);
    }
}

//! Per-workspace FreeAgent token storage, delegating to the generic
//! `workforce-oauth-client` [`GenericTokenStore`] for lazy refresh, rotated
//! refresh-token persistence, and single-flight refresh coordination.

use std::sync::Arc;

use workforce_common::settings::SecretStore;
use workforce_oauth_client::{
    OAuthClient, OAuthClientCreds, OAuthProvider, PathLayout, TokenStore as GenericTokenStore,
};

use super::provider::FreeAgentProvider;

/// Vault prefix under which a connected FreeAgent company's tokens live.
/// The per-user `users/{uid}/` segment is added by the layered vault.
pub const WORKSPACE_PREFIX: &str = "skills/freeagent/workspaces/";

/// Suffix marking a workspace as connected.
const REFRESH_SUFFIX: &str = "/refresh_token";

#[derive(Debug)]
struct FreeAgentLayout;

impl PathLayout for FreeAgentLayout {
    fn path(&self, target: &str, suffix: &str) -> String {
        format!("{WORKSPACE_PREFIX}{target}/{suffix}")
    }
}

/// Build a token store for one workspace. The same UA-configured `http`
/// client must be used for both this (token endpoint) and the data client
/// so FreeAgent's mandatory `User-Agent` is present on every request.
pub fn build_token_store(
    store: Arc<dyn SecretStore>,
    workspace: &str,
    creds: OAuthClientCreds,
    http: reqwest::Client,
    sandbox: bool,
) -> GenericTokenStore {
    token_store_with_provider(
        store,
        workspace,
        creds,
        http,
        Arc::new(FreeAgentProvider::new(sandbox)),
    )
}

/// Build a token store against an explicit provider — the production path
/// passes [`FreeAgentProvider`]; tests inject a provider pointed at a mock
/// token endpoint.
pub(crate) fn token_store_with_provider(
    store: Arc<dyn SecretStore>,
    workspace: &str,
    creds: OAuthClientCreds,
    http: reqwest::Client,
    provider: Arc<dyn OAuthProvider>,
) -> GenericTokenStore {
    let oauth = Arc::new(OAuthClient::new(provider, creds, http));
    let layout: Arc<dyn PathLayout> = Arc::new(FreeAgentLayout);
    GenericTokenStore::new(store, workspace.to_string(), layout, oauth)
}

/// Connected workspace names, discovered by scanning the secret store for
/// persisted refresh tokens.
pub fn list_workspaces(store: &dyn SecretStore) -> Vec<String> {
    let mut out: Vec<String> = store
        .list(Some(WORKSPACE_PREFIX))
        .into_iter()
        .filter_map(|k| {
            k.strip_prefix(WORKSPACE_PREFIX)
                .and_then(|s| s.strip_suffix(REFRESH_SUFFIX))
                .map(str::to_string)
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

    #[derive(Default)]
    struct MemStore {
        map: StdMutex<HashMap<String, String>>,
    }

    impl SecretStore for MemStore {
        fn get(&self, p: &str) -> Option<String> {
            self.map.lock().unwrap().get(p).cloned()
        }
        fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
            self.map.lock().unwrap().insert(p.into(), v.into());
            Ok(())
        }
        fn delete(&self, p: &str) -> eyre::Result<bool> {
            Ok(self.map.lock().unwrap().remove(p).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let m = self.map.lock().unwrap();
            match prefix {
                Some(p) => m.keys().filter(|k| k.starts_with(p)).cloned().collect(),
                None => m.keys().cloned().collect(),
            }
        }
    }

    #[test]
    fn layout_namespaces_by_workspace() {
        assert_eq!(
            FreeAgentLayout.path("acme", "access_token"),
            "skills/freeagent/workspaces/acme/access_token"
        );
    }

    #[test]
    fn list_workspaces_finds_connected_only() {
        let store = MemStore::default();
        store
            .set("skills/freeagent/workspaces/acme/refresh_token", "r1")
            .unwrap();
        store
            .set("skills/freeagent/workspaces/acme/access_token", "a1")
            .unwrap();
        store
            .set("skills/freeagent/workspaces/widgets/refresh_token", "r2")
            .unwrap();
        // An access token without a refresh token is not "connected".
        store
            .set("skills/freeagent/workspaces/ghost/access_token", "a3")
            .unwrap();
        assert_eq!(list_workspaces(&store), vec!["acme", "widgets"]);
    }
}

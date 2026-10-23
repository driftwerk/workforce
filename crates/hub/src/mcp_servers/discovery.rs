//! Identity for a server's discovery calls (`initialize` + `tools/list`).
//!
//! Discovery is server-scoped, so it lacks the request-scoped
//! `CURRENT_USER_ID` a `binding=User` token read needs (hub startup and
//! the refresh timer both run with no user). We borrow the freshest
//! enrolled user's identity for those calls only — tool *calls* keep the
//! caller's own identity and never route through here.
//!
//! This assumes a server's tool *list* is the same whoever asks. If one
//! ever returns RBAC-filtered tools, another user may see a tool that
//! isn't theirs; their call still authenticates as them and the upstream
//! rejects it, so the worst case is a phantom tool — never a credential
//! or data leak.

use std::future::Future;
use std::sync::Arc;

use workforce_agent::runner::CURRENT_USER_ID;
use workforce_common::settings::{McpAuthBinding, McpAuthConfig};

use crate::vault::Vault;

/// Resolves the identity to authenticate a server's discovery calls with
/// when no request-scoped user is active. Cheap to clone; re-resolved on
/// every use so it tracks enrolment and token-freshness changes.
#[derive(Clone)]
pub(super) struct DiscoveryIdentity {
    server_id: String,
    needs_user: bool,
    global_vault: Option<Arc<Vault>>,
}

impl DiscoveryIdentity {
    pub(super) fn from_config(
        server_id: &str,
        auth: &McpAuthConfig,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        // Only pure per-user OAuth must borrow an identity to list: PAT and
        // Shared resolve tokenless, and a shared-PAT fallback already lists
        // via that static token.
        let needs_user = matches!(
            auth,
            McpAuthConfig::Oauth2 {
                binding: McpAuthBinding::User,
                shared_pat_fallback: None,
                ..
            }
        );
        Self {
            server_id: server_id.to_string(),
            needs_user,
            global_vault,
        }
    }

    fn resolve(&self) -> Option<String> {
        if !self.needs_user || ambient_user_present() {
            return None;
        }
        freshest_enrolled_user(&self.server_id, self.global_vault.as_deref()?)
    }

    /// Run a discovery future under the resolved identity, or as-is when
    /// none is needed (a request-scoped user is already active, or the
    /// server authenticates without one).
    pub(super) async fn run<F: Future>(&self, fut: F) -> F::Output {
        match self.resolve() {
            Some(uid) => CURRENT_USER_ID.scope(uid, fut).await,
            None => fut.await,
        }
    }
}

fn ambient_user_present() -> bool {
    CURRENT_USER_ID.try_with(|u| !u.is_empty()).unwrap_or(false)
}

/// The enrolled user whose token was most recently refreshed (max
/// `expires_at`, ties broken by uid for determinism) — the one most likely
/// to still hold a valid refresh token. `None` when nobody has authorised.
fn freshest_enrolled_user(server_id: &str, vault: &Vault) -> Option<String> {
    let suffix = format!("/mcp/v1/{server_id}/access_token");
    vault
        .list(Some("users/"))
        .into_iter()
        .filter_map(|key| {
            let uid = key.strip_prefix("users/")?.strip_suffix(&suffix)?;
            if uid.is_empty() || uid.contains('/') {
                return None;
            }
            let expires_at = vault
                .get(&format!("users/{uid}/mcp/v1/{server_id}/expires_at"))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            Some((expires_at, uid.to_string()))
        })
        .max()
        .map(|(_, uid)| uid)
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

    fn oauth_user(shared_pat_fallback: Option<String>) -> McpAuthConfig {
        McpAuthConfig::Oauth2 {
            binding: McpAuthBinding::User,
            provider: "p".into(),
            scopes: Vec::new(),
            shared_pat_fallback,
            discovery: None,
        }
    }

    fn enrol(vault: &Vault, uid: &str, server_id: &str, expires_at: &str) {
        vault
            .set(
                &format!("users/{uid}/mcp/v1/{server_id}/access_token"),
                "at",
            )
            .unwrap();
        vault
            .set(
                &format!("users/{uid}/mcp/v1/{server_id}/expires_at"),
                expires_at,
            )
            .unwrap();
    }

    #[test]
    fn pat_auth_needs_no_identity() {
        let auth = McpAuthConfig::Pat {
            vault_path: "x".into(),
        };
        let d = DiscoveryIdentity::from_config("s", &auth, Some(new_vault()));
        assert_eq!(d.resolve(), None);
    }

    #[test]
    fn shared_binding_needs_no_identity() {
        let auth = McpAuthConfig::Oauth2 {
            binding: McpAuthBinding::Shared,
            provider: "p".into(),
            scopes: Vec::new(),
            shared_pat_fallback: None,
            discovery: None,
        };
        let d = DiscoveryIdentity::from_config("s", &auth, Some(new_vault()));
        assert_eq!(d.resolve(), None);
    }

    #[test]
    fn shared_pat_fallback_needs_no_identity() {
        let vault = new_vault();
        enrol(&vault, "alice", "s", "100");
        let d = DiscoveryIdentity::from_config("s", &oauth_user(Some("mcp/x".into())), Some(vault));
        assert_eq!(d.resolve(), None);
    }

    #[test]
    fn picks_freshest_enrolled_user() {
        let vault = new_vault();
        enrol(&vault, "alice", "s", "100");
        enrol(&vault, "bob", "s", "200");
        let d = DiscoveryIdentity::from_config("s", &oauth_user(None), Some(vault));
        assert_eq!(d.resolve(), Some("bob".to_string()));
    }

    #[test]
    fn ignores_other_servers_enrolments() {
        let vault = new_vault();
        enrol(&vault, "alice", "other", "100");
        let d = DiscoveryIdentity::from_config("s", &oauth_user(None), Some(vault));
        assert_eq!(d.resolve(), None);
    }

    #[test]
    fn none_when_no_enrolments() {
        let d = DiscoveryIdentity::from_config("s", &oauth_user(None), Some(new_vault()));
        assert_eq!(d.resolve(), None);
    }

    #[tokio::test]
    async fn ambient_user_takes_precedence() {
        let vault = new_vault();
        enrol(&vault, "bob", "s", "200");
        let d = DiscoveryIdentity::from_config("s", &oauth_user(None), Some(vault));
        let resolved = CURRENT_USER_ID
            .scope("alice".to_string(), async { d.resolve() })
            .await;
        assert_eq!(resolved, None);
    }
}

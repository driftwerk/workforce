//! Helpers for resolving skill credentials from a layered `SecretStore`.
//!
//! Skills in workforce store credentials at two related shapes:
//!
//! - **Workspace path:** `skills/<skill>/<scope>/<name>/<field>` — used in
//!   multi-workspace mode (e.g. `skills/jira/workspaces/acme/api_token`).
//! - **Flat path:** `skills/<skill>/<field>` — the legacy single-tenant
//!   form (e.g. `skills/jira/api_token`).
//!
//! With a `LayeredUserVault` store, each path also has a per-user overlay at
//! `users/{uid}/<path>` that should win over the agent-shared bare path.
//! Naïvely chaining `store.get(workspace).or_else(|| store.get(flat))`
//! inverts the precedence in the cross-form case: if the agent has a
//! workspace-path value AND the user has a flat-path override, the agent
//! wins (because `LayeredUserVault::get` resolves the workspace path's
//! agent tier before the flat path is even considered).
//!
//! [`resolve_credential`] fixes this by checking the per-user tier at both
//! forms first, then falling back to the agent tier at both forms:
//! `user-workspace > user-flat > agent-workspace > agent-flat`.
//!
//! A credential is only ever sent to a host chosen by an authority over it.
//! Settings that travel with a credential — a host, a site, a port — resolve
//! through [`resolve_companion`], which reads the caller's own tiers only
//! when the credential itself came from them; a shared credential is never
//! paired with a host one user wrote. Configuration baked into a client that
//! every user shares must be read with [`shared_get`].

use workforce_common::settings::{SecretStore, VaultSource};

/// A credential value and whether one of the caller's own tiers supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub per_user: bool,
    /// The tier it came from, where the store says.
    pub tier: Option<VaultSource>,
}

/// The workspace and flat forms of a skill field.
pub fn field_paths(
    skill: &str,
    collection: &str,
    workspace: &str,
    field: &str,
) -> (String, String) {
    (
        format!("skills/{skill}/{collection}/{workspace}/{field}"),
        format!("skills/{skill}/{field}"),
    )
}

/// Resolve a credential with workspace-aware fallback to a flat path,
/// honouring per-user overlays at either form before falling back to the
/// agent tier. See module docs for the precedence rules.
pub fn resolve_credential(
    store: &dyn SecretStore,
    workspace_path: &str,
    flat_path: &str,
) -> Option<Resolved> {
    // The tier `value` was read from at `path`, by the store's own account.
    let tier = |path: &str, value: &str, per_user: bool| {
        store
            .inspect(path)
            .into_iter()
            .find(|(t, v)| t.is_per_user() == per_user && v == value)
            .map(|(t, _)| t)
    };
    let user = |path: &str| {
        store.get_user_override(path).map(|value| Resolved {
            tier: tier(path, &value, true),
            value,
            per_user: true,
        })
    };
    let shared = |path: &str| {
        store.get(path).map(|value| Resolved {
            tier: tier(path, &value, false),
            value,
            per_user: false,
        })
    };
    user(workspace_path)
        .or_else(|| user(flat_path))
        .or_else(|| shared(workspace_path))
        .or_else(|| shared(flat_path))
}

/// Where a credential is sent (a site, a host, a port), for the credential
/// `cred`: [`resolve_companion`]'s rule, except on a pinned store (a hub
/// running Desk), where an agent can write vault paths beside the person's
/// connection. There it is read only from the tier `cred` came from, or the
/// hub's shared `Global` tier, which only an administrator writes: never
/// from a tier an agent wrote without the credential.
pub fn companion_for(
    store: &dyn SecretStore,
    cred: &Resolved,
    workspace_path: &str,
    flat_path: &str,
) -> Option<String> {
    if !store.is_pinned() {
        return resolve_companion(store, cred.per_user, workspace_path, flat_path);
    }
    let at = |path: &str, tier: VaultSource| {
        store
            .inspect(path)
            .into_iter()
            .find(|(t, _)| *t == tier)
            .map(|(_, v)| v)
    };
    let own = cred.tier?;
    at(workspace_path, own)
        .or_else(|| at(flat_path, own))
        .or_else(|| at(workspace_path, VaultSource::Global))
        .or_else(|| at(flat_path, VaultSource::Global))
}

/// [`companion_for`] of a host, normalised to no trailing slash.
pub fn host_for(
    store: &dyn SecretStore,
    cred: &Resolved,
    workspace_path: &str,
    flat_path: &str,
) -> Option<String> {
    companion_for(store, cred, workspace_path, flat_path)
        .map(|url| url.trim_end_matches('/').to_string())
}

/// The address a loader builds a client with, for the credential at
/// `credential_path`: [`shared_get`] as before, except on a pinned store,
/// where it is read as [`companion_for`] reads it.
pub fn address_for(
    store: &dyn SecretStore,
    credential_path: &str,
    address_path: &str,
) -> Option<String> {
    if !store.is_pinned() {
        return shared_get(store, address_path);
    }
    let cred = resolve_at(store, credential_path)?;
    companion_for(store, &cred, address_path, address_path)
}

/// [`resolve_credential`] for a field that has a single form.
pub fn resolve_at(store: &dyn SecretStore, path: &str) -> Option<Resolved> {
    resolve_credential(store, path, path)
}

pub fn resolve_workspace_or_flat(
    store: &dyn SecretStore,
    workspace_path: &str,
    flat_path: &str,
) -> Option<String> {
    resolve_credential(store, workspace_path, flat_path).map(|r| r.value)
}

/// Read `path` from the shared tiers only. A store that reports no tiers
/// (a single vault, an in-memory store) has nothing per-user, so `get`
/// is its shared read.
pub fn shared_get(store: &dyn SecretStore, path: &str) -> Option<String> {
    let tiers = store.inspect(path);
    if tiers.is_empty() {
        return store.get(path);
    }
    tiers
        .into_iter()
        .find(|(tier, _)| !tier.is_per_user())
        .map(|(_, value)| value)
}

/// A setting that travels with a credential: the caller's own tiers are
/// consulted only when `per_user` says the credential came from them, then
/// the shared tiers.
pub fn resolve_companion(
    store: &dyn SecretStore,
    per_user: bool,
    workspace_path: &str,
    flat_path: &str,
) -> Option<String> {
    let shared = || shared_get(store, workspace_path).or_else(|| shared_get(store, flat_path));
    if !per_user {
        return shared();
    }
    store
        .get_user_override(workspace_path)
        .or_else(|| store.get_user_override(flat_path))
        .or_else(shared)
}

/// [`resolve_companion`] for a field that has a single form.
pub fn companion_at(store: &dyn SecretStore, per_user: bool, path: &str) -> Option<String> {
    resolve_companion(store, per_user, path, path)
}

/// A host companion, normalised to no trailing slash.
pub fn resolve_host(
    store: &dyn SecretStore,
    per_user: bool,
    workspace_path: &str,
    flat_path: &str,
) -> Option<String> {
    resolve_companion(store, per_user, workspace_path, flat_path)
        .map(|url| url.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::{InMem, LayeredInMem};

    const WS: &str = "skills/jira/workspaces/default/api_token";
    const FLAT: &str = "skills/jira/api_token";
    const WS_SITE: &str = "skills/jira/workspaces/default/site";
    const FLAT_SITE: &str = "skills/jira/site";

    /// What LayeredInMem's tiers are: the user's is AgentPerUser, the
    /// agent's AgentSpecific.
    fn resolved(value: &str, per_user: bool) -> Resolved {
        Resolved {
            tier: Some(match per_user {
                true => VaultSource::AgentPerUser,
                false => VaultSource::AgentSpecific,
            }),
            value: value.into(),
            per_user,
        }
    }

    #[test]
    fn field_paths_pair_the_workspace_and_flat_forms() {
        assert_eq!(
            field_paths("jira", "workspaces", "default", "api_token"),
            (WS.to_string(), FLAT.to_string())
        );
    }

    #[test]
    fn user_flat_beats_agent_workspace() {
        let store = LayeredInMem::new(&[(WS, "agent_ws_token")], &[(FLAT, "user_flat_token")]);
        assert_eq!(
            resolve_credential(&*store, WS, FLAT),
            Some(resolved("user_flat_token", true))
        );
    }

    #[test]
    fn user_workspace_beats_user_flat() {
        let store = LayeredInMem::new(&[], &[(WS, "user_ws_token"), (FLAT, "user_flat_token")]);
        assert_eq!(
            resolve_credential(&*store, WS, FLAT),
            Some(resolved("user_ws_token", true))
        );
    }

    #[test]
    fn agent_workspace_beats_agent_flat_when_no_user_override() {
        let store = LayeredInMem::new(&[(WS, "agent_ws_token"), (FLAT, "agent_flat_token")], &[]);
        assert_eq!(
            resolve_credential(&*store, WS, FLAT),
            Some(resolved("agent_ws_token", false))
        );
    }

    #[test]
    fn agent_flat_resolves_when_only_flat_set() {
        let store = LayeredInMem::new(&[(FLAT, "agent_flat_token")], &[]);
        assert_eq!(
            resolve_workspace_or_flat(&*store, WS, FLAT).as_deref(),
            Some("agent_flat_token")
        );
        assert_eq!(
            resolve_at(&*store, FLAT),
            Some(resolved("agent_flat_token", false))
        );
    }

    #[test]
    fn returns_none_when_nothing_set() {
        let store = LayeredInMem::new(&[], &[]);
        assert!(resolve_credential(&*store, WS, FLAT).is_none());
    }

    #[test]
    fn shared_get_ignores_a_per_user_shadow_and_reads_tierless_stores_plainly() {
        let store = LayeredInMem::new(
            &[(FLAT_SITE, "https://org.atlassian.net")],
            &[(FLAT_SITE, "https://attacker.example")],
        );
        assert_eq!(
            shared_get(&*store, FLAT_SITE).as_deref(),
            Some("https://org.atlassian.net")
        );
        let user_only = LayeredInMem::new(&[], &[(FLAT_SITE, "https://attacker.example")]);
        assert!(shared_get(&*user_only, FLAT_SITE).is_none());
        let tierless = InMem::from(&[(FLAT_SITE, "https://org.atlassian.net")]);
        assert_eq!(
            shared_get(&*tierless, FLAT_SITE).as_deref(),
            Some("https://org.atlassian.net")
        );
    }

    #[test]
    fn companion_follows_a_per_user_credential_to_the_users_host() {
        let store = LayeredInMem::new(
            &[(FLAT_SITE, "https://org.atlassian.net")],
            &[(FLAT, "user_tok"), (FLAT_SITE, "https://me.atlassian.net/")],
        );
        let credential = resolve_credential(&*store, WS, FLAT).unwrap();
        assert_eq!(
            resolve_host(&*store, credential.per_user, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://me.atlassian.net")
        );
        assert_eq!(
            companion_at(&*store, true, FLAT_SITE).as_deref(),
            Some("https://me.atlassian.net/")
        );
    }

    #[test]
    fn companion_falls_back_to_the_shared_host_when_the_user_has_none() {
        let store = LayeredInMem::new(
            &[(FLAT_SITE, "https://org.atlassian.net")],
            &[(FLAT, "user_tok")],
        );
        let credential = resolve_credential(&*store, WS, FLAT).unwrap();
        assert_eq!(
            resolve_companion(&*store, credential.per_user, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://org.atlassian.net")
        );
    }

    #[test]
    fn companion_never_pairs_a_shared_credential_with_a_per_user_host() {
        let store = LayeredInMem::new(
            &[
                (FLAT, "shared_tok"),
                (FLAT_SITE, "https://org.atlassian.net"),
            ],
            &[(FLAT_SITE, "https://attacker.example")],
        );
        let credential = resolve_credential(&*store, WS, FLAT).unwrap();
        assert!(!credential.per_user);
        assert_eq!(
            resolve_companion(&*store, credential.per_user, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://org.atlassian.net")
        );
        let no_shared_host = LayeredInMem::new(
            &[(FLAT, "shared_tok")],
            &[(FLAT_SITE, "https://attacker.example")],
        );
        assert!(resolve_companion(&*no_shared_host, false, WS_SITE, FLAT_SITE).is_none());
    }

    #[test]
    fn companion_prefers_the_workspace_form_within_a_tier() {
        let store = LayeredInMem::new(
            &[],
            &[
                (FLAT, "user_tok"),
                (WS_SITE, "https://ws.atlassian.net"),
                (FLAT_SITE, "https://flat.atlassian.net"),
            ],
        );
        assert_eq!(
            resolve_companion(&*store, true, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://ws.atlassian.net")
        );
    }

    /// LayeredInMem as a hub running Desk reads it.
    struct Pinned(std::sync::Arc<LayeredInMem>);

    impl SecretStore for Pinned {
        fn get(&self, p: &str) -> Option<String> {
            self.0.get(p)
        }
        fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
            self.0.set(p, v)
        }
        fn delete(&self, p: &str) -> eyre::Result<bool> {
            self.0.delete(p)
        }
        fn list(&self, p: Option<&str>) -> Vec<String> {
            self.0.list(p)
        }
        fn get_user_override(&self, p: &str) -> Option<String> {
            self.0.get_user_override(p)
        }
        fn inspect(&self, p: &str) -> Vec<(VaultSource, String)> {
            self.0.inspect(p)
        }
        fn is_pinned(&self) -> bool {
            true
        }
    }

    /// On a hub running Desk, a site an agent wrote at another tier than
    /// the person's key is never where the key is sent; the site kept with
    /// the key is.
    #[test]
    fn a_pinned_store_sends_a_key_only_where_its_own_tier_says() {
        // The person's key and site for this agent; the agent's own site
        // beside them at the agent's shared tier.
        let store = Pinned(LayeredInMem::new(
            &[(WS_SITE, "https://evil.example")],
            &[(WS, "user_token"), (WS_SITE, "https://acme.atlassian.net")],
        ));
        let cred = resolve_credential(&store, WS, FLAT).unwrap();
        assert_eq!(
            companion_for(&store, &cred, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://acme.atlassian.net")
        );
        // A key at the shared tier, a site only at the person's: none.
        let store = Pinned(LayeredInMem::new(
            &[(WS, "shared_token")],
            &[(WS_SITE, "https://evil.example")],
        ));
        let cred = resolve_credential(&store, WS, FLAT).unwrap();
        assert_eq!(companion_for(&store, &cred, WS_SITE, FLAT_SITE), None);
        assert_eq!(address_for(&store, WS, WS_SITE), None);
        // The person's key, a site only the agent wrote: none, where before
        // it would have been read.
        let store = Pinned(LayeredInMem::new(
            &[(WS_SITE, "https://evil.example")],
            &[(WS, "user_token")],
        ));
        let cred = resolve_credential(&store, WS, FLAT).unwrap();
        assert_eq!(companion_for(&store, &cred, WS_SITE, FLAT_SITE), None);
        assert_eq!(
            resolve_companion(&*store.0, true, WS_SITE, FLAT_SITE).as_deref(),
            Some("https://evil.example"),
            "an unpinned store keeps its rule"
        );
    }
}

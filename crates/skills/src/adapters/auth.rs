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

use workforce_common::settings::SecretStore;

/// A credential value and whether one of the caller's own tiers supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub per_user: bool,
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
    let user = |path: &str| {
        store.get_user_override(path).map(|value| Resolved {
            value,
            per_user: true,
        })
    };
    let shared = |path: &str| {
        store.get(path).map(|value| Resolved {
            value,
            per_user: false,
        })
    };
    user(workspace_path)
        .or_else(|| user(flat_path))
        .or_else(|| shared(workspace_path))
        .or_else(|| shared(flat_path))
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

    fn resolved(value: &str, per_user: bool) -> Resolved {
        Resolved {
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
}

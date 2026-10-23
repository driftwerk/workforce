//! Per-run secret policy and the deny-by-default scoped-secret resolver.
//!
//! A workflow run installs a [`SecretPolicy`] as a task-local for its duration.
//! At a skill sink, [`resolve_secret_markers`] resolves the run's declared
//! secret markers against the vault — and refuses everything else. The policy's
//! presence is the positive "this is a workflow run" signal; absence means a
//! genuinely interactive call, where the legacy per-skill resolver still
//! applies. This split is what stops a member-authored workflow from reaching an
//! undeclared key (deny-by-default), a non-owner from reading per-user secrets
//! (the per-user gate), or a shared infra credential outside the curated
//! namespace — see the workflow-ownership design doc.

use std::collections::HashMap;
use std::sync::Arc;

use eyre::{eyre, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use tracing::warn;

use crate::settings::SecretStore;
use crate::vault_scope::{self, is_per_user_scope};

/// Oracle-free error returned for every refusal, so a caller learns nothing
/// about vault membership. Specifics go to operator tracing only.
pub const SECRET_REFUSED: &str = "secret resolution refused — see operator logs for details";

/// One declared secret variable's resolution policy.
#[derive(Clone, Debug, Default)]
pub struct DeclaredSecret {
    /// Hosts the resolved value may egress to at a browser sink. Empty imposes
    /// no host restriction.
    pub allowed_hosts: Vec<String>,
}

/// The secret-resolution policy for one workflow run.
#[derive(Clone, Debug, Default)]
pub struct SecretPolicy {
    /// The identity whose per-user secrets (`agent_user`/`global_user`) this run
    /// may read — `Some(owner)` only when the executing identity owns the
    /// definition, `None` otherwise (a non-owner running a shared workflow never
    /// reaches the runner's per-user secrets). Resolution uses this identity
    /// explicitly, not the turn's `CURRENT_USER_ID` (which a workflow turn
    /// rebinds to a synthetic value).
    pub per_user_identity: Option<String>,
    /// When `Some`, references to shared scopes (`agent`/`global`) are
    /// restricted to keys under this operator-curated prefix. `None` for
    /// system/operator definitions, which may reference any shared key.
    pub shared_secret_prefix: Option<String>,
    /// The `(scope, key)` pairs declared as secret variables in the running
    /// definition. Only these resolve — everything else is refused.
    pub declared: HashMap<(String, String), DeclaredSecret>,
}

tokio::task_local! {
    /// Active per-run secret policy. Absent outside a workflow run.
    pub static SECRET_POLICY: SecretPolicy;
}

/// Whether a workflow-run secret policy is in scope right now.
pub fn in_secret_policy_scope() -> bool {
    SECRET_POLICY.try_with(|_| ()).is_ok()
}

/// The active policy cloned, if one is in scope. Capture before a `tokio::spawn`
/// and re-apply with [`scope_if_present`] so the policy survives the spawn —
/// task-locals don't cross a bare spawn boundary.
pub fn capture() -> Option<SecretPolicy> {
    SECRET_POLICY.try_with(|p| p.clone()).ok()
}

/// Run `fut` with `policy` installed as the task-local when `Some`; otherwise
/// run it unchanged. Boxed so both arms share one type at a spawn site.
pub fn scope_if_present<F>(
    policy: Option<SecretPolicy>,
    fut: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match policy {
        Some(p) => Box::pin(SECRET_POLICY.scope(p, fut)),
        None => Box::pin(fut),
    }
}

/// Run `f` with `policy` installed as the task-local when `Some`; otherwise run
/// it unchanged. The blocking counterpart of [`scope_if_present`], for
/// `spawn_blocking` bodies that reach a secret sink.
pub fn sync_scope_if_present<T>(policy: Option<SecretPolicy>, f: impl FnOnce() -> T) -> T {
    match policy {
        Some(p) => SECRET_POLICY.sync_scope(p, f),
        None => f(),
    }
}

static VAULT_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[\[vault:[^\[\]\s]+\]\]").expect("vault token regex"));

/// Resolve every `[[vault:...]]` marker in `text` against the run's secret
/// policy and vault `store`. `host` is the current navigation target host at a
/// browser sink (for egress enforcement); `None` for sinks without a known
/// destination.
///
/// Deny-by-default, fail-closed:
/// - With NO policy in scope (not a workflow run), `text` is returned unchanged
///   so a non-workflow caller can apply its own legacy resolver.
/// - With a policy in scope, EVERY marker is routed through the policy: a scoped
///   `[[vault:<scope>:<key>]]` resolves only if `(scope,key)` was declared, the
///   per-user gate and curated-prefix rules pass, the value exists at exactly
///   that tier, and `host` is permitted; a legacy `[[vault:<key>]]` (undeclared
///   by construction) is refused. Every failure returns [`SECRET_REFUSED`].
pub fn resolve_secret_markers(
    text: &str,
    store: &Arc<dyn SecretStore>,
    host: Option<&str>,
) -> Result<String> {
    if !text.contains("[[vault:") {
        return Ok(text.to_string());
    }
    let Ok(policy) = SECRET_POLICY.try_with(|p| p.clone()) else {
        return Ok(text.to_string());
    };
    let mut out = text.to_string();
    for token in VAULT_TOKEN.find_iter(text) {
        let token = token.as_str();
        let value = resolve_one(token, &policy, store, host)?;
        out = out.replace(token, &value);
    }
    Ok(out)
}

fn resolve_one(
    token: &str,
    policy: &SecretPolicy,
    store: &Arc<dyn SecretStore>,
    host: Option<&str>,
) -> Result<String> {
    let Some(marker) = vault_scope::parse_exact_scoped_marker(token) else {
        warn!(target: "sentinel.audit", token = %token, "secret refused: non-scoped marker in workflow run");
        return Err(eyre!("{SECRET_REFUSED}"));
    };
    let Some(declared) = policy
        .declared
        .get(&(marker.scope_name.clone(), marker.key.clone()))
    else {
        warn!(target: "sentinel.audit", scope = %marker.scope_name, "secret refused: undeclared");
        return Err(eyre!("{SECRET_REFUSED}"));
    };
    if !host_allowed(&declared.allowed_hosts, host) {
        warn!(target: "sentinel.audit", "secret refused: egress host not allowed");
        return Err(eyre!("{SECRET_REFUSED}"));
    }

    let resolved = if is_per_user_scope(marker.scope) {
        // Resolve the owner's per-user secret by its explicit physical path at
        // the bare tier — independent of the turn's `CURRENT_USER_ID`. `None`
        // identity means the executor doesn't own the definition.
        let Some(uid) = policy.per_user_identity.as_deref() else {
            warn!(target: "sentinel.audit", "secret refused: per-user scope but executor is not the owner");
            return Err(eyre!("{SECRET_REFUSED}"));
        };
        store.get_at_tier(
            &format!("users/{uid}/{}", marker.key),
            per_user_bare_tier(marker.scope),
        )
    } else {
        // Shared scope: member references are confined to the curated prefix.
        if let Some(prefix) = &policy.shared_secret_prefix {
            if !marker.key.starts_with(prefix.as_str()) {
                warn!(target: "sentinel.audit", "secret refused: shared key outside curated prefix");
                return Err(eyre!("{SECRET_REFUSED}"));
            }
        }
        store.get_at_tier(&marker.key, marker.scope)
    };

    resolved.ok_or_else(|| {
        warn!(target: "sentinel.audit", scope = %marker.scope_name, "secret refused: not found at tier");
        eyre!("{SECRET_REFUSED}")
    })
}

/// The bare tier a per-user scope reads from when resolved by explicit path:
/// `agent_user` → the agent vault, `global_user` → the global vault.
fn per_user_bare_tier(scope: crate::settings::VaultSource) -> crate::settings::VaultSource {
    use crate::settings::VaultSource;
    match scope {
        VaultSource::AgentPerUser => VaultSource::AgentSpecific,
        VaultSource::GlobalPerUser => VaultSource::Global,
        other => other,
    }
}

/// Egress host check with label-boundary matching: `host == h` or a true
/// subdomain `*.h` — never a bare suffix like `notexample.com` matching
/// `example.com`. An empty allowlist imposes no restriction; a non-empty one
/// with no known host refuses.
fn host_allowed(allowed: &[String], host: Option<&str>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(host) = host else {
        return false;
    };
    allowed
        .iter()
        .any(|h| host == h || host.ends_with(&format!(".{h}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{VaultActor, VaultEntry, VaultSource};
    use std::sync::Mutex;

    #[derive(Default)]
    struct TierStore(Mutex<HashMap<(String, VaultSource), String>>);

    impl TierStore {
        fn with(entries: &[(&str, VaultSource, &str)]) -> Arc<dyn SecretStore> {
            let s = TierStore::default();
            for (k, t, v) in entries {
                s.0.lock()
                    .unwrap()
                    .insert(((*k).to_string(), *t), (*v).to_string());
            }
            Arc::new(s)
        }
    }

    impl SecretStore for TierStore {
        fn get(&self, _: &str) -> Option<String> {
            None
        }
        fn set(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn list(&self, _: Option<&str>) -> Vec<String> {
            Vec::new()
        }
        fn get_at_tier(&self, path: &str, tier: VaultSource) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .get(&(path.to_string(), tier))
                .cloned()
        }
        fn inspect(&self, _: &str) -> Vec<(VaultSource, String)> {
            Vec::new()
        }
        fn set_at_tier(&self, _: &str, _: &str, _: VaultSource, _: VaultActor<'_>) -> Result<()> {
            unreachable!()
        }
        fn list_at_tier(&self, _: Option<&str>, _: VaultSource) -> Vec<(String, VaultEntry)> {
            Vec::new()
        }
    }

    fn declared(pairs: &[(&str, &str, &[&str])]) -> HashMap<(String, String), DeclaredSecret> {
        pairs
            .iter()
            .map(|(scope, key, hosts)| {
                (
                    (scope.to_string(), key.to_string()),
                    DeclaredSecret {
                        allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    async fn with_policy<F: std::future::Future<Output = ()>>(policy: SecretPolicy, f: F) {
        SECRET_POLICY.scope(policy, f).await;
    }

    #[tokio::test]
    async fn policy_survives_spawn_via_capture_then_scope_if_present() {
        // The whole feature hinges on the policy crossing the agent turn's
        // tokio::spawn. capture() in the parent + scope_if_present() in the
        // spawned task carry it; a bare spawn drops it (the contract that
        // motivates capture() — pin it so nobody "simplifies" it away).
        SECRET_POLICY
            .scope(
                SecretPolicy {
                    per_user_identity: Some("alice".into()),
                    ..Default::default()
                },
                async {
                    let captured = capture();
                    assert!(captured.is_some());
                    let survived = tokio::spawn(scope_if_present(captured, async {
                        in_secret_policy_scope()
                    }))
                    .await
                    .unwrap();
                    assert!(
                        survived,
                        "policy must survive capture → spawn → scope_if_present"
                    );
                },
            )
            .await;

        SECRET_POLICY
            .scope(SecretPolicy::default(), async {
                let dropped = tokio::spawn(async { in_secret_policy_scope() })
                    .await
                    .unwrap();
                assert!(
                    !dropped,
                    "a bare spawn must drop the policy — that is why capture() exists"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn no_policy_passes_markers_through() {
        // Outside a workflow run the resolver is a no-op (legacy path handles it).
        let store = TierStore::with(&[]);
        let out = resolve_secret_markers("[[vault:agent:k]]", &store, None).unwrap();
        assert_eq!(out, "[[vault:agent:k]]");
    }

    #[tokio::test]
    async fn undeclared_marker_refused() {
        let store = TierStore::with(&[("k", VaultSource::AgentSpecific, "v")]);
        with_policy(SecretPolicy::default(), async move {
            assert!(resolve_secret_markers("[[vault:agent:k]]", &store, None).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn legacy_marker_refused_in_workflow_run() {
        let store = TierStore::with(&[]);
        let policy = SecretPolicy {
            declared: declared(&[("agent", "k", &[])]),
            ..Default::default()
        };
        with_policy(policy, async move {
            // Legacy single-colon marker is never declared → refused.
            assert!(resolve_secret_markers("[[vault:k]]", &store, None).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn declared_shared_resolves_at_its_tier_only() {
        // Value only at Global; an `agent`-scoped declared marker must NOT find
        // it (single-tier pinning) but a `global` one must.
        let store = TierStore::with(&[("k", VaultSource::Global, "secret-v")]);
        let policy = SecretPolicy {
            declared: declared(&[("agent", "k", &[]), ("global", "k", &[])]),
            ..Default::default()
        };
        with_policy(policy, async move {
            assert!(resolve_secret_markers("[[vault:agent:k]]", &store, None).is_err());
            assert_eq!(
                resolve_secret_markers("[[vault:global:k]]", &store, None).unwrap(),
                "secret-v"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn per_user_gate_resolves_owner_by_explicit_path() {
        // The owner's secret lives at its explicit physical path on the bare
        // agent tier; the resolver reads it there using the policy identity.
        let store = TierStore::with(&[("users/alice/k", VaultSource::AgentSpecific, "mine")]);
        let decl = declared(&[("agent_user", "k", &[])]);
        // Owner runs own definition → identity present → resolves alice's secret.
        let allow = SecretPolicy {
            per_user_identity: Some("alice".to_string()),
            declared: decl.clone(),
            ..Default::default()
        };
        let store2 = Arc::clone(&store);
        with_policy(allow, async move {
            assert_eq!(
                resolve_secret_markers("[[vault:agent_user:k]]", &store2, None).unwrap(),
                "mine"
            );
        })
        .await;
        // Non-owner runs a shared definition → no identity → per-user refused.
        let deny = SecretPolicy {
            per_user_identity: None,
            declared: decl,
            ..Default::default()
        };
        with_policy(deny, async move {
            assert!(resolve_secret_markers("[[vault:agent_user:k]]", &store, None).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn shared_key_must_be_under_curated_prefix_for_members() {
        let store = TierStore::with(&[
            ("workflow_shared/ok", VaultSource::AgentSpecific, "ok-v"),
            ("prod_db_password", VaultSource::AgentSpecific, "leak"),
        ]);
        let policy = SecretPolicy {
            shared_secret_prefix: Some("workflow_shared/".to_string()),
            declared: declared(&[
                ("agent", "workflow_shared/ok", &[]),
                ("agent", "prod_db_password", &[]),
            ]),
            ..Default::default()
        };
        with_policy(policy, async move {
            assert_eq!(
                resolve_secret_markers("[[vault:agent:workflow_shared/ok]]", &store, None).unwrap(),
                "ok-v"
            );
            // Declared, present, but outside the curated namespace → refused.
            assert!(
                resolve_secret_markers("[[vault:agent:prod_db_password]]", &store, None).is_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn egress_label_boundary() {
        let store = TierStore::with(&[("k", VaultSource::AgentSpecific, "v")]);
        let policy = SecretPolicy {
            declared: declared(&[("agent", "k", &["example.com"])]),
            ..Default::default()
        };
        with_policy(policy, async move {
            // Exact host and true subdomain pass.
            assert!(
                resolve_secret_markers("[[vault:agent:k]]", &store, Some("example.com")).is_ok()
            );
            assert!(
                resolve_secret_markers("[[vault:agent:k]]", &store, Some("app.example.com"))
                    .is_ok()
            );
            // Sibling-suffix and unknown host refuse.
            assert!(
                resolve_secret_markers("[[vault:agent:k]]", &store, Some("notexample.com"))
                    .is_err()
            );
            assert!(resolve_secret_markers("[[vault:agent:k]]", &store, None).is_err());
        })
        .await;
    }
}

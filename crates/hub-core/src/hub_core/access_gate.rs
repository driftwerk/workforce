//! Cross-surface user-access gate.
//!
//! Single hot-reloadable registry of `[agents.X.access]` policies, consulted
//! by every user-facing ingress (Slack, gateway WebSocket). Surface-agnostic:
//! each ingress builds an [`AccessRequest`] with the canonical workforce
//! identity UUID and any surface-prefixed fallback ids, and the gate returns
//! an [`AccessDecision`].

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use workforce_common::sentinel_config::ExternalUsersConfig;
use workforce_common::settings::{AccessDecision, AgentAccessConfig};

#[derive(Debug, Default, Clone, Copy)]
pub struct AccessRequest<'a> {
    pub agent_id: &'a str,
    pub identity_id: Option<&'a str>,
    pub surface_ids: &'a [&'a str],
    /// Whether the sender is from outside the bot's home Slack workspace.
    /// Surface-agnostic: non-Slack ingresses pass `false`.
    pub is_external: bool,
}

pub struct AccessGate {
    policies: RwLock<Arc<HashMap<String, AgentAccessConfig>>>,
    external: ExternalUsersConfig,
}

impl AccessGate {
    pub fn new(initial: HashMap<String, AgentAccessConfig>) -> Arc<Self> {
        Self::with_external(initial, ExternalUsersConfig::default())
    }

    pub fn with_external(
        initial: HashMap<String, AgentAccessConfig>,
        external: ExternalUsersConfig,
    ) -> Arc<Self> {
        let normalized = initial
            .into_iter()
            .map(|(agent, mut policy)| {
                policy.normalize_bare_slack_ids(&agent);
                (agent, policy)
            })
            .collect();
        Arc::new(Self {
            policies: RwLock::new(Arc::new(normalized)),
            external,
        })
    }

    /// Whether the resolved agent allows external senders — a per-agent
    /// `allow_external_users` override falls back to the global default.
    fn external_allowed(policy: Option<&AgentAccessConfig>, global: bool) -> bool {
        policy
            .and_then(|p| p.allow_external_users)
            .unwrap_or(global)
    }

    /// Whether dispatch needs to classify the sender's origin before calling
    /// [`AccessGate::check`]. `false` when external senders are already allowed
    /// for `agent_id`, letting the caller skip the lookup entirely — no
    /// detection cost when the gate cannot deny.
    pub fn external_check_needed(&self, agent_id: &str) -> bool {
        !Self::external_allowed(self.snapshot().get(agent_id), self.external.allow)
    }

    pub fn upsert(&self, agent_id: String, mut policy: AgentAccessConfig) {
        policy.normalize_bare_slack_ids(&agent_id);
        let mut guard = self.policies.write();
        let mut next: HashMap<String, AgentAccessConfig> = (**guard).clone();
        next.insert(agent_id, policy);
        *guard = Arc::new(next);
    }

    pub fn remove(&self, agent_id: &str) {
        let mut guard = self.policies.write();
        if !guard.contains_key(agent_id) {
            return;
        }
        let mut next: HashMap<String, AgentAccessConfig> = (**guard).clone();
        next.remove(agent_id);
        *guard = Arc::new(next);
    }

    pub fn check(&self, req: AccessRequest<'_>) -> AccessDecision {
        let snap = self.snapshot();
        let policy = snap.get(req.agent_id);
        if req.is_external && !Self::external_allowed(policy, self.external.allow) {
            return AccessDecision::Deny {
                message: policy
                    .and_then(|p| p.deny_message.clone())
                    .or_else(|| self.external.deny_message.clone()),
                reason: "external_user_blocked",
            };
        }
        match policy {
            Some(policy) => policy.evaluate(req.identity_id, req.surface_ids),
            None => AccessDecision::Allow,
        }
    }

    pub fn snapshot(&self) -> Arc<HashMap<String, AgentAccessConfig>> {
        self.policies.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::AccessMode;

    fn allowlist_policy(allow: &[&str]) -> AgentAccessConfig {
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn req<'a>(
        agent: &'a str,
        identity: Option<&'a str>,
        surfaces: &'a [&'a str],
    ) -> AccessRequest<'a> {
        AccessRequest {
            agent_id: agent,
            identity_id: identity,
            surface_ids: surfaces,
            is_external: false,
        }
    }

    fn external_req(agent: &str) -> AccessRequest<'_> {
        AccessRequest {
            agent_id: agent,
            identity_id: None,
            surface_ids: &[],
            is_external: true,
        }
    }

    fn deny_external() -> ExternalUsersConfig {
        ExternalUsersConfig {
            allow: false,
            deny_message: Some("internal only".into()),
        }
    }

    fn allow_external_global() -> ExternalUsersConfig {
        ExternalUsersConfig {
            allow: true,
            deny_message: None,
        }
    }

    fn allow_external() -> AgentAccessConfig {
        AgentAccessConfig {
            allow_external_users: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn unknown_agent_allows() {
        let gate = AccessGate::new(HashMap::new());
        assert_eq!(
            gate.check(req("nonexistent", Some("uuid"), &[])),
            AccessDecision::Allow
        );
    }

    #[test]
    fn check_routes_to_policy() {
        let mut policies = HashMap::new();
        policies.insert("allan".into(), allowlist_policy(&["alice-uuid"]));
        let gate = AccessGate::new(policies);

        assert_eq!(
            gate.check(req("allan", Some("alice-uuid"), &[])),
            AccessDecision::Allow
        );
        assert!(matches!(
            gate.check(req("allan", Some("bob-uuid"), &[])),
            AccessDecision::Deny { .. }
        ));
    }

    #[test]
    fn upsert_hot_swap_visible_to_next_check() {
        let mut initial = HashMap::new();
        initial.insert("allan".into(), allowlist_policy(&["alice-uuid"]));
        let gate = AccessGate::new(initial);

        assert!(matches!(
            gate.check(req("allan", Some("bob-uuid"), &[])),
            AccessDecision::Deny { .. }
        ));

        gate.upsert(
            "allan".into(),
            allowlist_policy(&["alice-uuid", "bob-uuid"]),
        );

        assert_eq!(
            gate.check(req("allan", Some("bob-uuid"), &[])),
            AccessDecision::Allow
        );
    }

    #[test]
    fn upsert_for_new_agent() {
        let gate = AccessGate::new(HashMap::new());
        gate.upsert("new-agent".into(), allowlist_policy(&["alice"]));
        assert_eq!(
            gate.check(req("new-agent", Some("alice"), &[])),
            AccessDecision::Allow
        );
        assert!(matches!(
            gate.check(req("new-agent", Some("bob"), &[])),
            AccessDecision::Deny { .. }
        ));
    }

    #[test]
    fn remove_drops_policy() {
        let mut policies = HashMap::new();
        policies.insert("allan".into(), allowlist_policy(&["alice"]));
        let gate = AccessGate::new(policies);
        gate.remove("allan");
        assert_eq!(
            gate.check(req("allan", Some("bob"), &[])),
            AccessDecision::Allow,
            "policy removed → falls back to allow"
        );
    }

    #[test]
    fn remove_unknown_agent_is_noop() {
        let gate = AccessGate::new(HashMap::new());
        gate.remove("nope");
    }

    #[test]
    fn new_normalizes_bare_slack_ids() {
        let mut policies = HashMap::new();
        policies.insert("allan".into(), allowlist_policy(&["U01ABCDEFG"]));
        let gate = AccessGate::new(policies);
        assert_eq!(
            gate.check(req("allan", None, &["slack:U01ABCDEFG"])),
            AccessDecision::Allow,
            "bare slack id should match the prefixed surface id"
        );
    }

    #[test]
    fn upsert_normalizes_bare_slack_ids() {
        let gate = AccessGate::new(HashMap::new());
        gate.upsert("allan".into(), allowlist_policy(&["U01ABCDEFG"]));
        assert_eq!(
            gate.check(req("allan", None, &["slack:U01ABCDEFG"])),
            AccessDecision::Allow
        );
    }

    #[test]
    fn surface_ids_match_when_identity_absent() {
        let mut policies = HashMap::new();
        policies.insert("allan".into(), allowlist_policy(&["slack:U123"]));
        let gate = AccessGate::new(policies);
        assert_eq!(
            gate.check(req("allan", None, &["slack:U123"])),
            AccessDecision::Allow
        );
    }

    #[test]
    fn external_denied_by_default() {
        let gate = AccessGate::new(HashMap::new());
        assert!(gate.external_check_needed("any"));
        assert!(matches!(
            gate.check(external_req("any")),
            AccessDecision::Deny {
                reason: "external_user_blocked",
                ..
            }
        ));
    }

    #[test]
    fn external_deny_carries_global_message() {
        let gate = AccessGate::with_external(HashMap::new(), deny_external());
        assert_eq!(
            gate.check(external_req("unbound")),
            AccessDecision::Deny {
                message: Some("internal only".into()),
                reason: "external_user_blocked",
            }
        );
    }

    #[test]
    fn external_allowed_when_globally_allowed() {
        let gate = AccessGate::with_external(HashMap::new(), allow_external_global());
        assert!(!gate.external_check_needed("any"));
        assert_eq!(gate.check(external_req("any")), AccessDecision::Allow);
    }

    #[test]
    fn internal_sender_unaffected() {
        let gate = AccessGate::with_external(HashMap::new(), deny_external());
        assert_eq!(gate.check(req("unbound", None, &[])), AccessDecision::Allow);
    }

    #[test]
    fn per_agent_override_allows_external() {
        let mut policies = HashMap::new();
        policies.insert("support".into(), allow_external());
        let gate = AccessGate::with_external(policies, deny_external());
        assert!(!gate.external_check_needed("support"));
        assert_eq!(gate.check(external_req("support")), AccessDecision::Allow);
    }

    #[test]
    fn per_agent_override_denies_external_when_global_allows() {
        let mut policies = HashMap::new();
        policies.insert(
            "internal".into(),
            AgentAccessConfig {
                allow_external_users: Some(false),
                ..Default::default()
            },
        );
        let gate = AccessGate::with_external(policies, allow_external_global());
        assert!(gate.external_check_needed("internal"));
        assert!(matches!(
            gate.check(external_req("internal")),
            AccessDecision::Deny {
                reason: "external_user_blocked",
                ..
            }
        ));
    }

    #[test]
    fn allowed_external_still_subject_to_allowlist() {
        let mut policies = HashMap::new();
        policies.insert(
            "support".into(),
            AgentAccessConfig {
                mode: AccessMode::Allowlist,
                allow: vec!["slack:U999".into()],
                allow_external_users: Some(true),
                ..Default::default()
            },
        );
        let gate = AccessGate::with_external(policies, deny_external());
        assert!(matches!(
            gate.check(AccessRequest {
                agent_id: "support",
                identity_id: None,
                surface_ids: &["slack:U123"],
                is_external: true,
            }),
            AccessDecision::Deny {
                reason: "allowlist_no_match",
                ..
            }
        ));
        assert_eq!(
            gate.check(AccessRequest {
                agent_id: "support",
                identity_id: None,
                surface_ids: &["slack:U999"],
                is_external: true,
            }),
            AccessDecision::Allow
        );
    }
}

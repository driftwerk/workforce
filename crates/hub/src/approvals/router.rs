//! [`ApprovalRouter`] adapter for the hub.
//!
//! Pure resolution (`route`) is a single indexed store read plus set
//! algebra: most-specific approvers-bearing rule → expand
//! `ApproverRef`s to user ids → drop the requester (four-eyes) and the
//! Slack-unreachable → the eligibility/fallback table. Persistence
//! (`defer`) writes the durable core row before the runner returns the
//! synthetic pending result. No Slack dependency here.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use uuid::Uuid;

use workforce_agent::runner::{
    ApprovalDecision, ApprovalRoute, ApprovalRouteCtx, ApprovalRouter, PendingApprovalCore,
};
use workforce_identity::store::{IdentityFilter, IdentityStore};
use workforce_identity::team::TeamStore;
use workforce_identity::types::IdentityRole;
use workforce_permissions::store::PermissionStore;
use workforce_permissions::types::{ApproverRef, PermissionScope};

use super::store::PendingApprovalStore;

/// Same short TTL the cascade gate uses for its team-membership cache —
/// long enough that a multi-turn conversation doesn't hammer the
/// identity store, short enough that "add user to team" propagates
/// within a minute.
const TEAM_CACHE_TTL: Duration = Duration::from_secs(60);

pub struct HubApprovalRouter {
    permissions: Arc<dyn PermissionStore>,
    identity: Arc<dyn IdentityStore>,
    team_store: Option<Arc<dyn TeamStore>>,
    pending: Arc<dyn PendingApprovalStore>,
    /// Lifetime of a routed approval before the sweeper expires it.
    default_ttl: Duration,
    team_cache: Mutex<HashMap<Uuid, (Vec<String>, Instant)>>,
}

impl HubApprovalRouter {
    pub fn new(
        permissions: Arc<dyn PermissionStore>,
        identity: Arc<dyn IdentityStore>,
        pending: Arc<dyn PendingApprovalStore>,
        default_ttl: Duration,
    ) -> Self {
        Self {
            permissions,
            identity,
            team_store: None,
            pending,
            default_ttl,
            team_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_team_store(mut self, team_store: Arc<dyn TeamStore>) -> Self {
        self.team_store = Some(team_store);
        self
    }

    /// Resolve the requester's team UUIDs as strings — the shape the
    /// scope lookup expects. Mirrors `CascadePermissionGate::team_ids_for`
    /// (UUID-parse guard, fail-open to empty, short-TTL cache); a missing
    /// team store or non-UUID user simply yields no Team-scope rules.
    fn team_ids_for(&self, user_id: &str) -> Vec<String> {
        let Some(store) = self.team_store.as_ref() else {
            return Vec::new();
        };
        let Ok(uid) = Uuid::from_str(user_id) else {
            return Vec::new();
        };
        {
            let cache = self.team_cache.lock();
            if let Some((teams, deadline)) = cache.get(&uid) {
                if Instant::now() < *deadline {
                    return teams.clone();
                }
            }
        }
        let teams = match store.teams_for_member(uid) {
            Ok(rows) => rows.into_iter().map(|t| t.id.to_string()).collect(),
            Err(e) => {
                tracing::warn!(error = %e, user = %user_id, "team lookup failed; routing without Team-scope rules");
                Vec::new()
            }
        };
        self.team_cache
            .lock()
            .insert(uid, (teams.clone(), Instant::now() + TEAM_CACHE_TTL));
        teams
    }

    /// Expand `ApproverRef`s to distinct internal user ids, preserving
    /// first-seen order so route() is deterministic. Role refs resolve
    /// via `IdentityStore::list(role)`.
    fn expand_approvers(&self, refs: &[ApproverRef]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut push = |id: String, out: &mut Vec<String>| {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        };
        for r in refs {
            match r {
                ApproverRef::User { id } => push(id.clone(), &mut out),
                ApproverRef::Role { role } => {
                    let filter = IdentityFilter {
                        role: Some(IdentityRole::from_str_lossy(role)),
                        ..IdentityFilter::default()
                    };
                    match self.identity.list(&filter) {
                        Ok(ids) => {
                            for identity in ids {
                                push(identity.id.to_string(), &mut out);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, role = %role, "role expansion failed for approval routing")
                        }
                    }
                }
            }
        }
        out
    }

    /// Drop the requester (when self-approval is off) and any candidate
    /// without a linked Slack id (cannot DM), preserving order.
    fn filter_eligible(
        &self,
        candidates: Vec<String>,
        requester: &str,
        allow_self_approval: bool,
    ) -> Vec<String> {
        candidates
            .into_iter()
            .filter(|id| allow_self_approval || id != requester)
            .filter(|id| self.has_slack_link(id))
            .collect()
    }

    fn has_slack_link(&self, user_id: &str) -> bool {
        let Ok(uuid) = Uuid::from_str(user_id) else {
            return false;
        };
        matches!(
            self.identity.get_by_id(uuid),
            Ok(Some(identity)) if identity.linked_ids.get("slack").is_some_and(|s| !s.is_empty())
        )
    }
}

impl ApprovalRouter for HubApprovalRouter {
    fn route(&self, ctx: &ApprovalRouteCtx<'_>) -> ApprovalDecision {
        let team_ids = self.team_ids_for(ctx.user_id);
        let mut scopes: Vec<(PermissionScope, &str)> = vec![
            (PermissionScope::Agent, ctx.agent_id),
            (PermissionScope::User, ctx.user_id),
        ];
        scopes.extend(team_ids.iter().map(|t| (PermissionScope::Team, t.as_str())));

        let rule = match self
            .permissions
            .approval_routing_for(&scopes, ctx.tool_name)
        {
            Ok(Some(rule)) => rule,
            Ok(None) => return ApprovalDecision::SelfServe,
            Err(e) => {
                tracing::warn!(error = %e, tool = %ctx.tool_name, "approval routing lookup failed; self-serve");
                return ApprovalDecision::SelfServe;
            }
        };

        let candidates = self.expand_approvers(&rule.approvers);
        let eligible = self.filter_eligible(candidates, ctx.user_id, rule.allow_self_approval);

        if !eligible.is_empty() {
            ApprovalDecision::Routed(ApprovalRoute {
                approver_user_ids: eligible,
                allow_self_approval: rule.allow_self_approval,
                ttl: self.default_ttl,
            })
        } else if rule.allow_self_approval {
            ApprovalDecision::SelfServe
        } else {
            ApprovalDecision::DenyUnreachable
        }
    }

    fn defer(&self, record: &PendingApprovalCore) -> eyre::Result<()> {
        let (workspace_id, thread_ts) = parse_session_key(&record.session_key);
        let expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(record.ttl)
                .unwrap_or_else(|_| chrono::Duration::hours(24));
        self.pending.insert_core(
            record,
            workspace_id.as_deref(),
            thread_ts.as_deref(),
            expires_at,
        )
    }
}

/// Split a `slack:ws:channel:thread` session key into workspace + thread.
/// The channel is already on the core row; thread may be empty.
pub fn parse_session_key(session_key: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = session_key.splitn(4, ':').collect();
    let nonempty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    match parts.as_slice() {
        ["slack", ws, _chan, thread] => (nonempty(ws), nonempty(thread)),
        ["slack", ws, _chan] => (nonempty(ws), None),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::types::{Identity, IdentityRole};
    use workforce_permissions::sqlite::SqlitePermissionStore;
    use workforce_permissions::types::{PermissionOrigin, PermissionRule, PermissionState};

    use crate::approvals::store::SqlitePendingApprovalStore;

    fn identity(id: Uuid, role: IdentityRole, slack: Option<&str>) -> Identity {
        let mut ident = Identity::new(format!("user-{id}"), role);
        ident.id = id;
        if let Some(s) = slack {
            ident.linked_ids.insert("slack".to_string(), s.to_string());
        }
        ident
    }

    struct Fixture {
        router: HubApprovalRouter,
        permissions: Arc<dyn PermissionStore>,
    }

    fn fixture(identities: Vec<Identity>) -> Fixture {
        let permissions: Arc<dyn PermissionStore> =
            Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let id_store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        for i in &identities {
            id_store.create(i).unwrap();
        }
        let identity: Arc<dyn IdentityStore> = id_store;
        let pending: Arc<dyn PendingApprovalStore> =
            Arc::new(SqlitePendingApprovalStore::in_memory().unwrap());
        let router = HubApprovalRouter::new(
            Arc::clone(&permissions),
            identity,
            pending,
            Duration::from_secs(3600),
        );
        Fixture {
            router,
            permissions,
        }
    }

    fn rule_with_approvers(
        agent: &str,
        approvers: Vec<ApproverRef>,
        allow_self: bool,
    ) -> PermissionRule {
        PermissionRule::new(
            PermissionScope::Agent,
            agent,
            "vault.*",
            PermissionState::AlwaysAsk,
            PermissionOrigin::Admin,
        )
        .with_approvers(approvers)
        .with_allow_self_approval(allow_self)
    }

    fn ctx<'a>(user: &'a str, agent: &'a str, tool: &'a str) -> ApprovalRouteCtx<'a> {
        ApprovalRouteCtx {
            user_id: user,
            agent_id: agent,
            tool_name: tool,
        }
    }

    #[test]
    fn no_rule_is_self_serve() {
        let f = fixture(vec![]);
        let uid = Uuid::new_v4().to_string();
        assert!(matches!(
            f.router.route(&ctx(&uid, "atlas", "vault.delete")),
            ApprovalDecision::SelfServe
        ));
    }

    #[test]
    fn reachable_approver_routes() {
        let bob = Uuid::new_v4();
        let f = fixture(vec![identity(bob, IdentityRole::Manager, Some("U_BOB"))]);
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::User {
                    id: bob.to_string(),
                }],
                false,
            ))
            .unwrap();
        let alice = Uuid::new_v4().to_string();
        match f.router.route(&ctx(&alice, "atlas", "vault.delete")) {
            ApprovalDecision::Routed(route) => {
                assert_eq!(route.approver_user_ids, vec![bob.to_string()]);
                assert!(!route.allow_self_approval);
            }
            _ => panic!("expected Routed"),
        }
    }

    #[test]
    fn requester_excluded_when_self_approval_off() {
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let f = fixture(vec![
            identity(alice, IdentityRole::Manager, Some("U_ALICE")),
            identity(bob, IdentityRole::Manager, Some("U_BOB")),
        ]);
        // Role-based rule names every manager; alice (the requester) is one.
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::Role {
                    role: "manager".to_string(),
                }],
                false,
            ))
            .unwrap();
        match f
            .router
            .route(&ctx(&alice.to_string(), "atlas", "vault.delete"))
        {
            ApprovalDecision::Routed(route) => {
                assert_eq!(
                    route.approver_user_ids,
                    vec![bob.to_string()],
                    "requester dropped by four-eyes"
                );
            }
            _ => panic!("expected Routed"),
        }
    }

    #[test]
    fn requester_included_when_self_approval_on() {
        let alice = Uuid::new_v4();
        let f = fixture(vec![identity(
            alice,
            IdentityRole::Manager,
            Some("U_ALICE"),
        )]);
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::Role {
                    role: "manager".to_string(),
                }],
                true,
            ))
            .unwrap();
        match f
            .router
            .route(&ctx(&alice.to_string(), "atlas", "vault.delete"))
        {
            ApprovalDecision::Routed(route) => {
                assert_eq!(route.approver_user_ids, vec![alice.to_string()]);
            }
            _ => panic!("expected Routed"),
        }
    }

    #[test]
    fn unreachable_approver_dropped() {
        let bob = Uuid::new_v4();
        // bob has no slack link → not reachable.
        let f = fixture(vec![identity(bob, IdentityRole::Manager, None)]);
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::User {
                    id: bob.to_string(),
                }],
                false,
            ))
            .unwrap();
        let alice = Uuid::new_v4().to_string();
        // No reachable approver + self-approval off → deny-fast.
        assert!(matches!(
            f.router.route(&ctx(&alice, "atlas", "vault.delete")),
            ApprovalDecision::DenyUnreachable
        ));
    }

    #[test]
    fn empty_eligible_with_self_approval_falls_back_to_self_serve() {
        let bob = Uuid::new_v4();
        let f = fixture(vec![identity(bob, IdentityRole::Manager, None)]);
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::User {
                    id: bob.to_string(),
                }],
                true,
            ))
            .unwrap();
        let alice = Uuid::new_v4().to_string();
        assert!(matches!(
            f.router.route(&ctx(&alice, "atlas", "vault.delete")),
            ApprovalDecision::SelfServe
        ));
    }

    #[test]
    fn requester_only_candidate_self_off_denies() {
        let alice = Uuid::new_v4();
        let f = fixture(vec![identity(
            alice,
            IdentityRole::Manager,
            Some("U_ALICE"),
        )]);
        f.permissions
            .upsert_rule(&rule_with_approvers(
                "atlas",
                vec![ApproverRef::User {
                    id: alice.to_string(),
                }],
                false,
            ))
            .unwrap();
        assert!(matches!(
            f.router
                .route(&ctx(&alice.to_string(), "atlas", "vault.delete")),
            ApprovalDecision::DenyUnreachable
        ));
    }

    #[test]
    fn defer_persists_core_with_parsed_session() {
        let bob = Uuid::new_v4();
        let f = fixture(vec![identity(bob, IdentityRole::Manager, Some("U_BOB"))]);
        let core = PendingApprovalCore {
            request_id: Uuid::new_v4().to_string(),
            agent_id: "atlas".to_string(),
            session_key: "slack:T123:C456:THREAD9".to_string(),
            channel_id: "C456".to_string(),
            requester_user_id: "alice".to_string(),
            tool_name: "vault.delete".to_string(),
            tool_input: serde_json::json!({"id": 1}),
            reason: "r".to_string(),
            allow_self_approval: false,
            approver_user_ids: vec![bob.to_string()],
            ttl: Duration::from_secs(60),
        };
        f.router.defer(&core).unwrap();
        // Read it back through the same pending store via a fresh handle.
        let row = f
            .router
            .pending
            .get(&core.request_id)
            .unwrap()
            .expect("persisted");
        assert_eq!(row.workspace_id.as_deref(), Some("T123"));
        assert_eq!(row.thread_ts.as_deref(), Some("THREAD9"));
        assert_eq!(row.channel_id.as_deref(), Some("C456"));
        assert!(row.expires_at > Utc::now());
    }

    #[test]
    fn session_key_parsing_handles_missing_thread() {
        assert_eq!(
            parse_session_key("slack:T1:C1:"),
            (Some("T1".to_string()), None)
        );
        assert_eq!(
            parse_session_key("slack:T1:C1:TS"),
            (Some("T1".to_string()), Some("TS".to_string()))
        );
        assert_eq!(parse_session_key("cli"), (None, None));
    }
}

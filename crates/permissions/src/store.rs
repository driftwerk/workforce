use eyre::Result;
use uuid::Uuid;

use crate::types::{
    PermissionFilter, PermissionRoleAssignment, PermissionRoleDef, PermissionRule, PermissionScope,
};

/// Scope specificity for routing tie-breaks: User > Agent > Team.
fn scope_specificity(scope: PermissionScope) -> u8 {
    match scope {
        PermissionScope::User => 2,
        PermissionScope::Agent => 1,
        PermissionScope::Team => 0,
    }
}

/// Persistence layer for the permission system. Mirrors
/// [`workforce_identity::store::IdentityStore`] in shape: synchronous
/// trait (matches the runner's hot path) with both SQLite and Postgres
/// implementations behind one interface.
pub trait PermissionStore: Send + Sync {
    // ── permission_rules ──
    fn upsert_rule(&self, rule: &PermissionRule) -> Result<()>;
    fn get_rule(&self, id: Uuid) -> Result<Option<PermissionRule>>;
    fn delete_rule(&self, id: Uuid) -> Result<bool>;
    fn list_rules(&self, filter: &PermissionFilter) -> Result<Vec<PermissionRule>>;

    /// Convenience read used by the cascade evaluator: every rule
    /// (any tool pattern) whose `(scope, scope_id)` matches one of the
    /// pairs supplied. Implementations are free to issue one query per
    /// pair or batch into a single `IN`; callers must not depend on
    /// ordering.
    fn rules_for_scopes(&self, scopes: &[(PermissionScope, &str)]) -> Result<Vec<PermissionRule>>;

    /// Most-specific rule (User > Agent > Team) whose (scope, scope_id) is in
    /// `scopes`, whose `tool_pattern` matches `tool_name`, and which carries a
    /// non-empty `approvers`. Ties broken by scope specificity then most-recent
    /// `updated_at`. Used by the approval router; independent of the cascade.
    fn approval_routing_for(
        &self,
        scopes: &[(PermissionScope, &str)],
        tool_name: &str,
    ) -> Result<Option<PermissionRule>> {
        Ok(self
            .rules_for_scopes(scopes)?
            .into_iter()
            .filter(|rule| {
                !rule.approvers.is_empty()
                    && workforce_policy::rules::tool_matches(&rule.tool_pattern, tool_name)
            })
            .max_by_key(|rule| (scope_specificity(rule.scope), rule.updated_at)))
    }

    /// Monotonic counter incremented on every successful write
    /// (`upsert_rule` / `delete_rule` / role-grant mutations). Cache
    /// invalidation strategy is "compare generation against the value
    /// captured when the cache entry was computed" — see the plan doc.
    fn current_generation(&self) -> Result<u64>;

    // ── permission_role / grants ──
    fn upsert_role(&self, role: &PermissionRoleDef) -> Result<()>;
    fn get_role(&self, id: Uuid) -> Result<Option<PermissionRoleDef>>;
    fn delete_role(&self, id: Uuid) -> Result<bool>;
    fn list_roles_for_team(&self, team_id: Uuid) -> Result<Vec<PermissionRoleDef>>;

    fn grant_rule_to_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<()>;
    fn revoke_rule_from_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<bool>;
    fn rules_for_role(&self, role_id: Uuid) -> Result<Vec<PermissionRule>>;

    fn assign_role_to_identity(&self, assignment: &PermissionRoleAssignment) -> Result<()>;
    fn unassign_role_from_identity(&self, identity_id: Uuid, role_id: Uuid) -> Result<bool>;
    fn roles_for_identity(&self, identity_id: Uuid) -> Result<Vec<PermissionRoleDef>>;
    /// How many roles each identity holding any is assigned.
    fn role_holders(&self) -> Result<std::collections::BTreeMap<Uuid, usize>>;
}

/// Every assignment counts once for its identity, and a deleted role's
/// assignments go with it.
#[cfg(test)]
pub(crate) fn role_holders_contract(store: &dyn PermissionStore) {
    let team = Uuid::new_v4();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let roles = [
        PermissionRoleDef::new(team, "operator"),
        PermissionRoleDef::new(team, "reviewer"),
    ];
    for role in &roles {
        store.upsert_role(role).unwrap();
    }
    for (identity_id, role) in [(alice, &roles[0]), (alice, &roles[1]), (bob, &roles[1])] {
        store
            .assign_role_to_identity(&PermissionRoleAssignment {
                identity_id,
                role_id: role.id,
                granted_by: None,
                granted_at: chrono::DateTime::from_timestamp_micros(
                    chrono::Utc::now().timestamp_micros(),
                )
                .unwrap(),
            })
            .unwrap();
    }
    assert_eq!(
        store.role_holders().unwrap(),
        std::collections::BTreeMap::from([(alice, 2), (bob, 1)])
    );
    store.delete_role(roles[1].id).unwrap();
    assert_eq!(
        store.role_holders().unwrap(),
        std::collections::BTreeMap::from([(alice, 1)])
    );
}

use chrono::Utc;
use uuid::Uuid;
use workforce_permissions::sqlite::SqlitePermissionStore;
use workforce_permissions::{
    PermissionFilter, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef,
    PermissionRule, PermissionScope, PermissionState, PermissionStore,
};

fn store() -> SqlitePermissionStore {
    SqlitePermissionStore::in_memory().expect("open sqlite")
}

fn admin_rule(
    scope: PermissionScope,
    scope_id: &str,
    tool: &str,
    state: PermissionState,
) -> PermissionRule {
    PermissionRule::new(scope, scope_id, tool, state, PermissionOrigin::Admin)
}

#[test]
fn rule_upsert_get_delete_round_trip() {
    let store = store();
    let team = Uuid::new_v4().to_string();
    let mut rule = admin_rule(
        PermissionScope::Team,
        &team,
        "memory.*",
        PermissionState::Ask,
    );

    store.upsert_rule(&rule).unwrap();
    let loaded = store.get_rule(rule.id).unwrap().expect("rule present");
    assert_eq!(loaded.scope, PermissionScope::Team);
    assert_eq!(loaded.scope_id, team);
    assert_eq!(loaded.tool_pattern, "memory.*");
    assert_eq!(loaded.state, PermissionState::Ask);
    assert_eq!(loaded.origin, PermissionOrigin::Admin);

    // Upsert on the same (scope, scope_id, tool_pattern) replaces in-place.
    rule.state = PermissionState::Deny;
    rule.updated_at = Utc::now();
    store.upsert_rule(&rule).unwrap();
    let again = store.get_rule(rule.id).unwrap().expect("rule present");
    assert_eq!(again.state, PermissionState::Deny);

    assert!(store.delete_rule(rule.id).unwrap());
    assert!(store.get_rule(rule.id).unwrap().is_none());
    assert!(!store.delete_rule(rule.id).unwrap());
}

#[test]
fn list_rules_filters_compose() {
    let store = store();
    let team_a = Uuid::new_v4().to_string();
    let team_b = Uuid::new_v4().to_string();

    store
        .upsert_rule(&admin_rule(
            PermissionScope::Team,
            &team_a,
            "memory.*",
            PermissionState::Ask,
        ))
        .unwrap();
    store
        .upsert_rule(&admin_rule(
            PermissionScope::Team,
            &team_a,
            "vault.*",
            PermissionState::Deny,
        ))
        .unwrap();
    store
        .upsert_rule(&admin_rule(
            PermissionScope::Team,
            &team_b,
            "memory.*",
            PermissionState::Allow,
        ))
        .unwrap();

    let only_team_a = store
        .list_rules(&PermissionFilter {
            scope: Some(PermissionScope::Team),
            scope_id: Some(team_a.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(only_team_a.len(), 2);

    let only_memory = store
        .list_rules(&PermissionFilter {
            tool_pattern: Some("memory.*".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(only_memory.len(), 2);

    let team_a_memory = store
        .list_rules(&PermissionFilter {
            scope: Some(PermissionScope::Team),
            scope_id: Some(team_a),
            tool_pattern: Some("memory.*".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(team_a_memory.len(), 1);
}

#[test]
fn list_rules_scopes_in_filter_returns_only_listed_scope_types() {
    let store = store();
    let team = uuid::Uuid::new_v4().to_string();
    let user = uuid::Uuid::new_v4().to_string();
    store
        .upsert_rule(&admin_rule(
            PermissionScope::Team,
            &team,
            "memory.*",
            PermissionState::Ask,
        ))
        .unwrap();
    store
        .upsert_rule(&admin_rule(
            PermissionScope::Agent,
            "atlas",
            "vault.*",
            PermissionState::Deny,
        ))
        .unwrap();
    store
        .upsert_rule(&admin_rule(
            PermissionScope::User,
            &user,
            "memory.recall",
            PermissionState::Allow,
        ))
        .unwrap();

    let admin_only = store
        .list_rules(&PermissionFilter {
            scopes: Some(vec![PermissionScope::Team, PermissionScope::Agent]),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(admin_only.len(), 2);
    assert!(admin_only.iter().all(|r| r.scope != PermissionScope::User));

    let empty = store
        .list_rules(&PermissionFilter {
            scopes: Some(vec![]),
            ..Default::default()
        })
        .unwrap();
    assert!(empty.is_empty(), "empty scopes filter returns nothing");
}

#[test]
fn rules_for_scopes_batches_lookup() {
    let store = store();
    let team = Uuid::new_v4().to_string();
    let agent = "atlas".to_string();
    let user = Uuid::new_v4().to_string();

    for (scope, sid, tool, state) in [
        (
            PermissionScope::Team,
            team.as_str(),
            "memory.*",
            PermissionState::Ask,
        ),
        (
            PermissionScope::Agent,
            agent.as_str(),
            "vault.*",
            PermissionState::AlwaysAsk,
        ),
        (
            PermissionScope::User,
            user.as_str(),
            "memory.*",
            PermissionState::Allow,
        ),
    ] {
        store
            .upsert_rule(&admin_rule(scope, sid, tool, state))
            .unwrap();
    }

    let rows = store
        .rules_for_scopes(&[
            (PermissionScope::Team, team.as_str()),
            (PermissionScope::Agent, agent.as_str()),
            (PermissionScope::User, user.as_str()),
        ])
        .unwrap();
    assert_eq!(rows.len(), 3);

    let empty = store.rules_for_scopes(&[]).unwrap();
    assert!(empty.is_empty());

    let only_user = store
        .rules_for_scopes(&[(PermissionScope::User, user.as_str())])
        .unwrap();
    assert_eq!(only_user.len(), 1);
    assert_eq!(only_user[0].state, PermissionState::Allow);
}

#[test]
fn generation_counter_bumps_on_every_write() {
    let store = store();
    let team = Uuid::new_v4().to_string();
    let baseline = store.current_generation().unwrap();
    assert_eq!(baseline, 0);

    let rule = admin_rule(
        PermissionScope::Team,
        &team,
        "memory.*",
        PermissionState::Ask,
    );
    store.upsert_rule(&rule).unwrap();
    let after_insert = store.current_generation().unwrap();
    assert_eq!(after_insert, 1);

    // Repeat upsert still bumps — the cache must invalidate even on
    // logically-no-op writes (state-change-only).
    let mut bumped = rule.clone();
    bumped.state = PermissionState::Deny;
    bumped.updated_at = Utc::now();
    store.upsert_rule(&bumped).unwrap();
    assert_eq!(store.current_generation().unwrap(), 2);

    store.delete_rule(rule.id).unwrap();
    assert_eq!(store.current_generation().unwrap(), 3);

    // Delete of a non-existent rule must not bump.
    store.delete_rule(Uuid::new_v4()).unwrap();
    assert_eq!(store.current_generation().unwrap(), 3);
}

#[test]
fn roles_and_grants_compose() {
    let store = store();
    let team_id = Uuid::new_v4();
    let identity_id = Uuid::new_v4();

    let role = PermissionRoleDef::new(team_id, "agent_operator")
        .with_description("can run all messaging tools");
    store.upsert_role(&role).unwrap();

    let rule = admin_rule(
        PermissionScope::Team,
        &team_id.to_string(),
        "messaging.*",
        PermissionState::Allow,
    );
    store.upsert_rule(&rule).unwrap();

    store.grant_rule_to_role(role.id, rule.id).unwrap();
    let granted = store.rules_for_role(role.id).unwrap();
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].tool_pattern, "messaging.*");

    let assignment = PermissionRoleAssignment {
        identity_id,
        role_id: role.id,
        granted_by: None,
        granted_at: Utc::now(),
    };
    store.assign_role_to_identity(&assignment).unwrap();

    let user_roles = store.roles_for_identity(identity_id).unwrap();
    assert_eq!(user_roles.len(), 1);
    assert_eq!(user_roles[0].name, "agent_operator");

    assert!(store.revoke_rule_from_role(role.id, rule.id).unwrap());
    assert!(store.rules_for_role(role.id).unwrap().is_empty());

    assert!(store
        .unassign_role_from_identity(identity_id, role.id)
        .unwrap());
    assert!(store.roles_for_identity(identity_id).unwrap().is_empty());

    // Cascading delete: removing the role drops dangling membership rows.
    store.assign_role_to_identity(&assignment).unwrap();
    assert!(store.delete_role(role.id).unwrap());
    assert!(store.roles_for_identity(identity_id).unwrap().is_empty());
}

#[test]
fn upsert_rule_replaces_on_unique_violation() {
    let store = store();
    let team = Uuid::new_v4().to_string();

    // Two PermissionRule instances with different IDs but the same
    // (scope_type, scope_id, tool_pattern) — the second must overwrite.
    let first = admin_rule(
        PermissionScope::Team,
        &team,
        "memory.*",
        PermissionState::Ask,
    );
    let mut second = admin_rule(
        PermissionScope::Team,
        &team,
        "memory.*",
        PermissionState::Deny,
    );
    second.id = first.id;

    store.upsert_rule(&first).unwrap();
    store.upsert_rule(&second).unwrap();
    let loaded = store.get_rule(first.id).unwrap().expect("present");
    assert_eq!(loaded.state, PermissionState::Deny);
}

#[test]
fn rule_with_conditions_round_trip() {
    let store = store();
    let team = Uuid::new_v4().to_string();
    let cond = serde_json::json!([
        {
            "field": "path",
            "operator": "starts_with",
            "value": "/tmp/",
            "action": "allow"
        }
    ]);
    let rule = admin_rule(
        PermissionScope::Team,
        &team,
        "filesystem.write",
        PermissionState::Ask,
    )
    .with_conditions(cond.clone());
    store.upsert_rule(&rule).unwrap();
    let loaded = store.get_rule(rule.id).unwrap().expect("present");
    assert_eq!(loaded.conditions.as_ref(), Some(&cond));
}

//! Migration helpers — translate legacy `workforce-policy` YAML rules
//! and approved `policy_proposals` into [`PermissionRule`] rows.
//!
//! Phase 7 of the User RBAC + Permission Cascade plan. The translation
//! is pure: callers feed in `workforce_policy::rules::PolicyRule`s,
//! receive a [`MigrationReport`] back, and decide whether to dry-run
//! (print + CSV) or apply (upsert into the store).
//!
//! Mapping summary:
//! - `RuleAction::{Allow,Deny,Ask,AlwaysAsk}` → `PermissionState::{Allow,Deny,Ask,AlwaysAsk}`
//! - `PolicyScope::Global` + `constitutional = true` → stays in YAML, NOT migrated.
//! - `PolicyScope::Global` + `constitutional = false` → no DB scope fits; reported as
//!   unmappable so the operator can decide whether to seed it as System or pin it to a team.
//! - `PolicyScope::Team{team}` → `PermissionScope::Team` with `scope_id = team`.
//! - `PolicyScope::Agent{agent}` → `PermissionScope::Agent` with `scope_id = agent`.
//! - `PolicyScope::Role{role}` → CSV row for operator review (plan calls for manual handling).

use chrono::Utc;
use uuid::Uuid;

use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

use crate::store::PermissionStore;
use crate::types::{PermissionOrigin, PermissionRule, PermissionScope, PermissionState};

/// Why a YAML rule was not converted into a [`PermissionRule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `constitutional = true` — System rules stay in YAML.
    Constitutional,
    /// `PolicyScope::Role` — plan calls for operator review; surfaced via the CSV.
    RoleScopeNeedsReview { role: String },
    /// `PolicyScope::Global` with `constitutional = false` — there's no DB scope
    /// that matches; operator should either promote to constitutional or pin to a team.
    UnmappableGlobal,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Constitutional => "constitutional",
            Self::RoleScopeNeedsReview { .. } => "role_scope_needs_review",
            Self::UnmappableGlobal => "unmappable_global_nonconstitutional",
        }
    }
}

/// One rule that wasn't translated, with enough context for the operator
/// to decide what to do with it.
#[derive(Debug, Clone)]
pub struct SkippedRule {
    pub name: String,
    pub reason: SkipReason,
    pub tool_pattern: String,
    pub action: String,
}

/// Output of [`translate_rules`]. The CSV lives separately so callers
/// can stream it to stdout or a file without coupling to a string format.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Rules ready to upsert into the [`PermissionStore`].
    pub migrated: Vec<PermissionRule>,
    /// Rules deliberately left in YAML (constitutional) or surfaced for
    /// manual review (`role` scope, ambiguous global).
    pub skipped: Vec<SkippedRule>,
    /// Lines (header included) for the operator-review CSV. Empty when
    /// no `Role`-scope rules were present.
    pub role_csv: Vec<String>,
}

impl MigrationReport {
    pub fn migrated_count(&self) -> usize {
        self.migrated.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    /// CSV as a single string. Convenient for writing to a file.
    pub fn role_csv_text(&self) -> String {
        self.role_csv.join("\n")
    }
}

/// Pure translator from legacy YAML policy rules to permission_rules.
/// Caller decides whether to dry-run (just inspect the report) or to
/// feed [`MigrationReport::migrated`] into [`apply_report`].
pub fn translate_rules(rules: &[PolicyRule], created_by: Option<Uuid>) -> MigrationReport {
    let mut report = MigrationReport::default();
    let mut role_rows: Vec<String> = Vec::new();

    for rule in rules {
        if rule.constitutional {
            report.skipped.push(SkippedRule {
                name: rule.name.clone(),
                reason: SkipReason::Constitutional,
                tool_pattern: rule.tool.clone(),
                action: action_label(&rule.action).into(),
            });
            continue;
        }

        let state = action_to_state(&rule.action);
        let now = Utc::now();
        match &rule.scope {
            PolicyScope::Team { team } => {
                report.migrated.push(PermissionRule {
                    id: Uuid::new_v4(),
                    scope: PermissionScope::Team,
                    scope_id: team.clone(),
                    tool_pattern: rule.tool.clone(),
                    state,
                    conditions: serialise_conditions(rule),
                    approvers: Vec::new(),
                    allow_self_approval: true,
                    origin: PermissionOrigin::Admin,
                    created_by,
                    created_at: now,
                    updated_at: now,
                });
            }
            PolicyScope::Agent { agent } => {
                report.migrated.push(PermissionRule {
                    id: Uuid::new_v4(),
                    scope: PermissionScope::Agent,
                    scope_id: agent.clone(),
                    tool_pattern: rule.tool.clone(),
                    state,
                    conditions: serialise_conditions(rule),
                    approvers: Vec::new(),
                    allow_self_approval: true,
                    origin: PermissionOrigin::Admin,
                    created_by,
                    created_at: now,
                    updated_at: now,
                });
            }
            PolicyScope::Role { role } => {
                report.skipped.push(SkippedRule {
                    name: rule.name.clone(),
                    reason: SkipReason::RoleScopeNeedsReview { role: role.clone() },
                    tool_pattern: rule.tool.clone(),
                    action: action_label(&rule.action).into(),
                });
                role_rows.push(format!(
                    "{},{},{},{},{}",
                    escape_csv(&rule.name),
                    escape_csv(role),
                    escape_csv(&rule.tool),
                    action_label(&rule.action),
                    rule.priority,
                ));
            }
            PolicyScope::Global => {
                report.skipped.push(SkippedRule {
                    name: rule.name.clone(),
                    reason: SkipReason::UnmappableGlobal,
                    tool_pattern: rule.tool.clone(),
                    action: action_label(&rule.action).into(),
                });
            }
        }
    }

    if !role_rows.is_empty() {
        report
            .role_csv
            .push("name,role,tool_pattern,action,priority".to_string());
        report.role_csv.extend(role_rows);
    }

    report
}

/// Translate an approved [`policy_proposals`] row (which is JSON of a
/// `PolicyRule`) into a [`PermissionRule`] ready to upsert. Returns
/// `Ok(None)` for proposals whose scope can't be mapped — the legacy
/// proposal stays in the `policy_proposals` table for audit; the
/// `PolicyEngine` keeps applying it in-memory.
pub fn translate_proposal(rule: &PolicyRule, created_by: Option<Uuid>) -> Option<PermissionRule> {
    if rule.constitutional {
        return None;
    }
    let state = action_to_state(&rule.action);
    let now = Utc::now();
    let (scope, scope_id) = match &rule.scope {
        PolicyScope::Team { team } => (PermissionScope::Team, team.clone()),
        PolicyScope::Agent { agent } => (PermissionScope::Agent, agent.clone()),
        // Role and Global scope proposals cannot land in permission_rules
        // without operator intervention — the plan defers these to manual
        // review. Returning `None` keeps the approval flow working (the
        // PolicyEngine still picks up the rule in-memory) while flagging
        // that the new store is intentionally not updated.
        PolicyScope::Role { .. } | PolicyScope::Global => return None,
    };
    Some(PermissionRule {
        id: Uuid::new_v4(),
        scope,
        scope_id,
        tool_pattern: rule.tool.clone(),
        state,
        conditions: serialise_conditions(rule),
        approvers: Vec::new(),
        allow_self_approval: true,
        origin: PermissionOrigin::Proposal,
        created_by,
        created_at: now,
        updated_at: now,
    })
}

/// Upsert every rule in `report.migrated` into the store. Returns the
/// number of writes performed. Callers should run this only after a
/// dry-run review.
pub fn apply_report(report: &MigrationReport, store: &dyn PermissionStore) -> eyre::Result<usize> {
    let mut written = 0usize;
    for rule in &report.migrated {
        store.upsert_rule(rule)?;
        written += 1;
    }
    Ok(written)
}

fn action_to_state(action: &RuleAction) -> PermissionState {
    match action {
        RuleAction::Allow => PermissionState::Allow,
        RuleAction::Deny => PermissionState::Deny,
        RuleAction::Ask => PermissionState::Ask,
        RuleAction::AlwaysAsk => PermissionState::AlwaysAsk,
    }
}

fn action_label(action: &RuleAction) -> &'static str {
    match action {
        RuleAction::Allow => "allow",
        RuleAction::Deny => "deny",
        RuleAction::Ask => "ask",
        RuleAction::AlwaysAsk => "always_ask",
    }
}

fn serialise_conditions(rule: &PolicyRule) -> Option<serde_json::Value> {
    if rule.conditions.is_empty() {
        None
    } else {
        serde_json::to_value(&rule.conditions).ok()
    }
}

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_policy::rules::{PolicyConfig, PolicyScope};

    fn parse_yaml(yaml: &str) -> PolicyConfig {
        PolicyConfig::from_yaml(yaml).expect("yaml parses")
    }

    #[test]
    fn constitutional_rules_skip_migration() {
        let yaml = r#"
rules:
  - name: forbid-vault-delete
    scope: global
    tool: vault.delete
    action: deny
    constitutional: true
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        assert_eq!(report.migrated.len(), 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Constitutional);
    }

    #[test]
    fn team_and_agent_rules_migrate_with_state_translation() {
        let yaml = r#"
rules:
  - name: team-mem-allow
    scope: { team: backend }
    tool: memory.*
    action: allow
  - name: agent-vault-ask
    scope: { agent: claude-pilot }
    tool: vault.read
    action: ask
  - name: agent-always-ask
    scope: { agent: claude-pilot }
    tool: vault.write
    action: always_ask
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        assert_eq!(report.migrated.len(), 3);
        assert!(report.skipped.is_empty());

        let team = report
            .migrated
            .iter()
            .find(|r| r.scope == PermissionScope::Team)
            .unwrap();
        assert_eq!(team.scope_id, "backend");
        assert_eq!(team.state, PermissionState::Allow);
        assert_eq!(team.tool_pattern, "memory.*");

        let ask = report
            .migrated
            .iter()
            .find(|r| r.tool_pattern == "vault.read")
            .unwrap();
        assert_eq!(ask.state, PermissionState::Ask);

        let always = report
            .migrated
            .iter()
            .find(|r| r.tool_pattern == "vault.write")
            .unwrap();
        assert_eq!(always.state, PermissionState::AlwaysAsk);
    }

    #[test]
    fn role_scope_rules_go_to_csv_for_review() {
        let yaml = r#"
rules:
  - name: dev-tool-allow
    scope: { role: developer }
    tool: code.*
    action: allow
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        assert!(report.migrated.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(matches!(
            report.skipped[0].reason,
            SkipReason::RoleScopeNeedsReview { ref role } if role == "developer"
        ));
        assert_eq!(report.role_csv.len(), 2, "header + one row");
        assert!(report.role_csv[0].starts_with("name,role,tool_pattern,action,priority"));
        assert!(report.role_csv[1].contains("developer"));
    }

    #[test]
    fn global_nonconstitutional_is_reported_as_unmappable() {
        // Operator wrote a global rule without `constitutional: true` —
        // we don't pretend to know whether it should become a System
        // rule or pin to a team.
        let yaml = r#"
rules:
  - name: global-ask
    scope: global
    tool: vault.*
    action: ask
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        assert!(report.migrated.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::UnmappableGlobal);
    }

    #[test]
    fn apply_report_writes_migrated_rules_and_skips_role_scope() {
        let yaml = r#"
rules:
  - name: t1
    scope: { team: backend }
    tool: memory.*
    action: ask
  - name: r1
    scope: { role: dev }
    tool: vault.*
    action: allow
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        let store = crate::sqlite::SqlitePermissionStore::in_memory().unwrap();
        let written = apply_report(&report, &store).unwrap();
        assert_eq!(written, 1, "only Team-scope migrates");
        let rules = store
            .list_rules(&crate::types::PermissionFilter::default())
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].scope, PermissionScope::Team);
        assert_eq!(rules[0].scope_id, "backend");
    }

    #[test]
    fn translate_proposal_maps_team_and_agent_scopes() {
        let rule = PolicyRule {
            name: "p1".into(),
            scope: PolicyScope::Team {
                team: "alpha".into(),
            },
            tool: "memory.recall".into(),
            action: RuleAction::Ask,
            conditions: vec![],
            priority: 0,
            constitutional: false,
        };
        let out = translate_proposal(&rule, None).expect("team-scope materialises");
        assert_eq!(out.scope, PermissionScope::Team);
        assert_eq!(out.origin, PermissionOrigin::Proposal);
        assert_eq!(out.state, PermissionState::Ask);

        let global = PolicyRule {
            scope: PolicyScope::Global,
            ..rule.clone()
        };
        assert!(translate_proposal(&global, None).is_none());

        let role_scope = PolicyRule {
            scope: PolicyScope::Role { role: "dev".into() },
            ..rule
        };
        assert!(translate_proposal(&role_scope, None).is_none());
    }

    #[test]
    fn conditions_are_preserved_when_present() {
        let yaml = r#"
rules:
  - name: cond
    scope: { team: backend }
    tool: filesystem.read
    action: ask
    conditions:
      - field: path
        operator: starts_with
        value: "/etc"
        action: deny
"#;
        let cfg = parse_yaml(yaml);
        let report = translate_rules(&cfg.rules, None);
        assert_eq!(report.migrated.len(), 1);
        let cond = report.migrated[0]
            .conditions
            .as_ref()
            .expect("conditions copied through");
        assert!(cond.is_array());
        assert_eq!(cond.as_array().unwrap().len(), 1);
    }
}

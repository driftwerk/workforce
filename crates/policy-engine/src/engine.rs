use parking_lot::RwLock;
use std::time::Instant;

use eyre::Result;
use tracing::info;
use workforce_core::policy::{PolicyDecision, PolicyRequest, PolicyResponse};

use crate::rules::{tool_matches, ConditionOperator, PolicyConfig, PolicyRule, PolicyScope};

/// Sort rules by priority descending, then by specificity descending.
fn sort_rules(rules: &mut [PolicyRule]) {
    rules.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.scope.specificity().cmp(&a.scope.specificity()))
    });
}

/// The policy engine evaluates requests against loaded rules.
///
/// Rules are composed hierarchically: global → team → role → agent.
/// The most specific matching rule wins.
///
/// Constitutional rules (loaded from config at startup) are immutable and always
/// checked first. Mutable rules can be added at runtime via `add_rules()`.
pub struct PolicyEngine {
    /// Immutable rules loaded from config — cannot be modified by agent proposals.
    constitutional_rules: Vec<PolicyRule>,
    /// Mutable rules added at runtime (e.g., approved policy proposals).
    mutable_rules: RwLock<Vec<PolicyRule>>,
}

impl PolicyEngine {
    /// Create a new policy engine from a configuration.
    ///
    /// All rules from config are marked constitutional (immutable).
    pub fn new(config: PolicyConfig) -> Self {
        let mut rules = config.rules;
        for r in &mut rules {
            r.constitutional = true;
        }
        sort_rules(&mut rules);
        Self {
            constitutional_rules: rules,
            mutable_rules: RwLock::new(vec![]),
        }
    }

    /// Create an empty policy engine (denies everything by default).
    pub fn empty() -> Self {
        Self {
            constitutional_rules: vec![],
            mutable_rules: RwLock::new(vec![]),
        }
    }

    /// Load rules from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let config = PolicyConfig::from_yaml(yaml)?;
        Ok(Self::new(config))
    }

    /// Add rules at runtime (e.g., from approved policy proposals).
    ///
    /// Only non-constitutional rules are accepted. Constitutional rules are silently
    /// filtered out to prevent agents from elevating their own rules.
    pub fn add_rules(&self, new_rules: Vec<PolicyRule>) {
        let filtered: Vec<PolicyRule> = new_rules
            .into_iter()
            .filter(|r| !r.constitutional)
            .collect();
        let mut rules = self.mutable_rules.write();
        rules.extend(filtered);
        sort_rules(&mut rules);
    }

    /// Append constitutional rules at construction time (before the engine is
    /// shared via `Arc`). Used to bootstrap built-in defaults (e.g. tool-level
    /// `require_approval` for project-mutating actions) on top of whatever
    /// the operator configured. Each rule is forced `constitutional = true`
    /// so it can't be removed by a later policy proposal.
    pub fn with_constitutional_rules(mut self, rules: Vec<PolicyRule>) -> Self {
        let mut new_rules = rules;
        for r in &mut new_rules {
            r.constitutional = true;
        }
        self.constitutional_rules.extend(new_rules);
        sort_rules(&mut self.constitutional_rules);
        self
    }

    /// Evaluate a policy request and return a decision.
    ///
    /// Constitutional rules are checked first. If a constitutional rule matches,
    /// it takes precedence. Otherwise, mutable rules are checked.
    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyResponse {
        let start = Instant::now();
        let agent_id = request.agent_id.as_str();
        let tool = &request.tool_name;

        let rule_matcher = |rule: &&PolicyRule| -> bool {
            // Check tool pattern matches
            if !tool_matches(&rule.tool, tool) {
                return false;
            }

            // Check scope matches
            match &rule.scope {
                PolicyScope::Global => true,
                PolicyScope::Team { team } => {
                    request.context.team_id.as_deref() == Some(team.as_str())
                }
                PolicyScope::Role { role } => {
                    request.context.role.as_deref() == Some(role.as_str())
                }
                PolicyScope::Agent { agent } => agent_id == agent.as_str(),
            }
        };

        // Constitutional rules take precedence — check them first.
        let mutable = self.mutable_rules.read();
        let matched = self
            .constitutional_rules
            .iter()
            .find(rule_matcher)
            .or_else(|| mutable.iter().find(rule_matcher));

        let elapsed_us = start.elapsed().as_micros() as u64;

        match matched {
            Some(rule) => {
                // If the rule has conditions, check them against the parameters
                let decision = if rule.conditions.is_empty() {
                    rule.action.to_decision(&rule.name)
                } else {
                    self.evaluate_conditions(rule, &request.parameters)
                };

                info!(
                    agent = agent_id,
                    tool = tool,
                    rule = %rule.name,
                    decision = ?decision,
                    elapsed_us = elapsed_us,
                    "Policy evaluated"
                );
                PolicyResponse {
                    decision,
                    matched_rule: Some(rule.name.clone()),
                    evaluation_time_us: elapsed_us,
                }
            }
            None => {
                // Default deny if no rule matches
                let decision = PolicyDecision::Deny {
                    reason: format!("No policy rule matches tool '{tool}' for agent '{agent_id}'"),
                };
                info!(
                    agent = agent_id,
                    tool = tool,
                    decision = ?decision,
                    elapsed_us = elapsed_us,
                    "Policy evaluated (default deny)"
                );
                PolicyResponse {
                    decision,
                    matched_rule: None,
                    evaluation_time_us: elapsed_us,
                }
            }
        }
    }

    /// Evaluate conditions on a rule against the request parameters.
    ///
    /// If any condition matches, its action is used.
    /// If no condition matches, the rule's default action is used.
    fn evaluate_conditions(
        &self,
        rule: &PolicyRule,
        parameters: &serde_json::Value,
    ) -> PolicyDecision {
        for condition in &rule.conditions {
            let field_value = get_nested_value(parameters, &condition.field);

            if evaluate_condition_op(&condition.operator, field_value, &condition.value) {
                return condition.action.to_decision(&rule.name);
            }
        }
        // No condition matched — use the rule's default action
        rule.action.to_decision(&rule.name)
    }

    /// Get the total number of loaded rules (constitutional + mutable).
    pub fn rule_count(&self) -> usize {
        self.constitutional_rules.len() + self.mutable_rules.read().len()
    }

    /// Get the number of constitutional (immutable) rules.
    pub fn constitutional_count(&self) -> usize {
        self.constitutional_rules.len()
    }

    /// Return a cloned list of all rules (constitutional + mutable).
    pub fn list_all_rules(&self) -> Vec<PolicyRule> {
        let mut rules = self.constitutional_rules.clone();
        rules.extend(self.mutable_rules.read().clone());
        rules
    }

    /// Remove a mutable rule by name. Returns true if found and removed.
    pub fn remove_mutable_rule(&self, name: &str) -> bool {
        let mut rules = self.mutable_rules.write();
        let before = rules.len();
        rules.retain(|r| r.name != name);
        rules.len() < before
    }
}

/// Extract a nested value from JSON using dot-notation (e.g., "recipient.domain").
fn get_nested_value<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for part in parts {
        match current.get(part) {
            Some(v) => current = v,
            None => return None,
        }
    }
    Some(current)
}

/// Evaluate a condition operator against a field value.
fn evaluate_condition_op(
    op: &ConditionOperator,
    field_value: Option<&serde_json::Value>,
    condition_value: &serde_json::Value,
) -> bool {
    match op {
        ConditionOperator::Equals => field_value == Some(condition_value),
        ConditionOperator::NotEquals => field_value != Some(condition_value),
        ConditionOperator::StartsWith => {
            match (
                field_value.and_then(|v| v.as_str()),
                condition_value.as_str(),
            ) {
                (Some(field), Some(prefix)) => field.starts_with(prefix),
                _ => false,
            }
        }
        ConditionOperator::Contains => {
            match (
                field_value.and_then(|v| v.as_str()),
                condition_value.as_str(),
            ) {
                (Some(field), Some(substr)) => field.contains(substr),
                _ => false,
            }
        }
        ConditionOperator::In => {
            if let Some(arr) = condition_value.as_array() {
                match field_value {
                    Some(v) => arr.contains(v),
                    None => false,
                }
            } else {
                false
            }
        }
        ConditionOperator::NotIn => {
            if let Some(arr) = condition_value.as_array() {
                match field_value {
                    Some(v) => !arr.contains(v),
                    None => true,
                }
            } else {
                true
            }
        }
        ConditionOperator::LessThan => {
            match (
                field_value.and_then(|v| v.as_f64()),
                condition_value.as_f64(),
            ) {
                (Some(a), Some(b)) => a < b,
                _ => false,
            }
        }
        ConditionOperator::GreaterThan => {
            match (
                field_value.and_then(|v| v.as_f64()),
                condition_value.as_f64(),
            ) {
                (Some(a), Some(b)) => a > b,
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleAction;
    use workforce_core::policy::PolicyContext;
    use workforce_core::types::AgentId;

    fn make_request(agent: &str, tool: &str) -> PolicyRequest {
        PolicyRequest {
            agent_id: AgentId::new(agent),
            tool_name: tool.to_string(),
            parameters: serde_json::Value::Null,
            context: PolicyContext::empty(),
        }
    }

    fn make_request_with_team(agent: &str, tool: &str, team: &str) -> PolicyRequest {
        PolicyRequest {
            agent_id: AgentId::new(agent),
            tool_name: tool.to_string(),
            parameters: serde_json::Value::Null,
            context: PolicyContext {
                node_id: None,
                team_id: Some(team.to_string()),
                task_id: None,
                role: None,
            },
        }
    }

    #[test]
    fn default_deny_when_no_rules() {
        let engine = PolicyEngine::empty();
        let response = engine.evaluate(&make_request("dev", "filesystem.read"));
        assert!(response.decision.is_denied());
        assert!(response.matched_rule.is_none());
    }

    #[test]
    fn allow_matching_rule() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "allow-read"
    scope: global
    tool: "filesystem.read"
    action: allow
"#,
        )
        .unwrap();

        let response = engine.evaluate(&make_request("dev", "filesystem.read"));
        assert!(response.decision.is_allowed());
        assert_eq!(response.matched_rule, Some("allow-read".to_string()));
    }

    #[test]
    fn wildcard_tool_matching() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "allow-all-filesystem"
    scope: global
    tool: "filesystem.*"
    action: allow
"#,
        )
        .unwrap();

        assert!(engine
            .evaluate(&make_request("dev", "filesystem.read"))
            .decision
            .is_allowed());
        assert!(engine
            .evaluate(&make_request("dev", "filesystem.write"))
            .decision
            .is_allowed());
        assert!(engine
            .evaluate(&make_request("dev", "exec.run"))
            .decision
            .is_denied());
    }

    #[test]
    fn specific_scope_overrides_global() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "global-deny-exec"
    scope: global
    tool: "exec.*"
    action: deny
  - name: "allow-exec-for-dev"
    scope:
      agent: "trusted-dev"
    tool: "exec.*"
    action: allow
"#,
        )
        .unwrap();

        // Global deny applies to other agents
        assert!(engine
            .evaluate(&make_request("random-agent", "exec.run"))
            .decision
            .is_denied());

        // Agent-specific allow overrides global deny
        assert!(engine
            .evaluate(&make_request("trusted-dev", "exec.run"))
            .decision
            .is_allowed());
    }

    #[test]
    fn team_scoped_rules() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "allow-git-for-dev-team"
    scope:
      team: "backend-dev"
    tool: "git.*"
    action: allow
"#,
        )
        .unwrap();

        // Agent with the team gets access
        assert!(engine
            .evaluate(&make_request_with_team("dev", "git.push", "backend-dev"))
            .decision
            .is_allowed());

        // Agent without the team is denied
        assert!(engine
            .evaluate(&make_request_with_team("dev", "git.push", "other-team"))
            .decision
            .is_denied());
    }

    #[test]
    fn require_approval_rule() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "approve-email-send"
    scope: global
    tool: "email.send"
    action: require_approval
"#,
        )
        .unwrap();

        let response = engine.evaluate(&make_request("assistant", "email.send"));
        assert!(response.decision.requires_approval());
    }

    #[test]
    fn priority_overrides_specificity() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "high-priority-deny"
    scope: global
    tool: "dangerous.*"
    action: deny
    priority: 100
  - name: "agent-specific-allow"
    scope:
      agent: "admin"
    tool: "dangerous.*"
    action: allow
    priority: 0
"#,
        )
        .unwrap();

        // Even though agent-specific is more specific, the high-priority global rule wins
        assert!(engine
            .evaluate(&make_request("admin", "dangerous.delete"))
            .decision
            .is_denied());
    }

    fn make_request_with_params(
        agent: &str,
        tool: &str,
        params: serde_json::Value,
    ) -> PolicyRequest {
        PolicyRequest {
            agent_id: AgentId::new(agent),
            tool_name: tool.to_string(),
            parameters: params,
            context: PolicyContext::empty(),
        }
    }

    #[test]
    fn condition_starts_with() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "restrict-filesystem-path"
    scope: global
    tool: "filesystem.write"
    action: deny
    conditions:
      - field: "path"
        operator: starts_with
        value: "/workspace"
        action: allow
"#,
        )
        .unwrap();

        // Path in /workspace → condition matches → allow
        let r = engine.evaluate(&make_request_with_params(
            "dev",
            "filesystem.write",
            serde_json::json!({"path": "/workspace/src/main.rs"}),
        ));
        assert!(r.decision.is_allowed());

        // Path outside → no condition matches → rule default (deny)
        let r = engine.evaluate(&make_request_with_params(
            "dev",
            "filesystem.write",
            serde_json::json!({"path": "/etc/passwd"}),
        ));
        assert!(r.decision.is_denied());
    }

    #[test]
    fn condition_in_list() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "restrict-email-recipients"
    scope: global
    tool: "email.send"
    action: require_approval
    conditions:
      - field: "recipient"
        operator: in
        value: ["alice@example.com", "bob@example.com"]
        action: allow
"#,
        )
        .unwrap();

        // Known recipient → allow
        let r = engine.evaluate(&make_request_with_params(
            "assistant",
            "email.send",
            serde_json::json!({"recipient": "alice@example.com"}),
        ));
        assert!(r.decision.is_allowed());

        // Unknown recipient → require approval (default)
        let r = engine.evaluate(&make_request_with_params(
            "assistant",
            "email.send",
            serde_json::json!({"recipient": "stranger@evil.com"}),
        ));
        assert!(r.decision.requires_approval());
    }

    #[test]
    fn condition_less_than() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "auto-approve-small-expenses"
    scope: global
    tool: "expenses.submit"
    action: require_approval
    conditions:
      - field: "amount"
        operator: less_than
        value: 50
        action: allow
"#,
        )
        .unwrap();

        // Small amount → allow
        let r = engine.evaluate(&make_request_with_params(
            "assistant",
            "expenses.submit",
            serde_json::json!({"amount": 25}),
        ));
        assert!(r.decision.is_allowed());

        // Large amount → require approval
        let r = engine.evaluate(&make_request_with_params(
            "assistant",
            "expenses.submit",
            serde_json::json!({"amount": 100}),
        ));
        assert!(r.decision.requires_approval());
    }

    #[test]
    fn condition_nested_field() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "check-nested"
    scope: global
    tool: "api.call"
    action: deny
    conditions:
      - field: "target.domain"
        operator: equals
        value: "internal.example.com"
        action: allow
"#,
        )
        .unwrap();

        let r = engine.evaluate(&make_request_with_params(
            "dev",
            "api.call",
            serde_json::json!({"target": {"domain": "internal.example.com"}}),
        ));
        assert!(r.decision.is_allowed());

        let r = engine.evaluate(&make_request_with_params(
            "dev",
            "api.call",
            serde_json::json!({"target": {"domain": "external.com"}}),
        ));
        assert!(r.decision.is_denied());
    }

    #[test]
    fn constitutional_rules_take_precedence() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "constitutional-deny-exec"
    scope: global
    tool: "exec.*"
    action: deny
    priority: 100
"#,
        )
        .unwrap();

        // Add a mutable rule that tries to allow exec
        engine.add_rules(vec![PolicyRule {
            name: "mutable-allow-exec".to_string(),
            scope: PolicyScope::Global,
            tool: "exec.*".to_string(),
            action: RuleAction::Allow,
            conditions: vec![],
            priority: 200, // Even higher priority
            constitutional: false,
        }]);

        // Constitutional deny still wins (checked first regardless of priority)
        let r = engine.evaluate(&make_request("dev", "exec.run"));
        assert!(r.decision.is_denied());
        assert_eq!(r.matched_rule, Some("constitutional-deny-exec".to_string()));
    }

    #[test]
    fn with_constitutional_rules_appends_and_forces_constitutional_flag() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "user-allow-all"
    scope: global
    tool: "*"
    action: allow
"#,
        )
        .unwrap()
        .with_constitutional_rules(vec![PolicyRule {
            name: "builtin-require-approval".to_string(),
            scope: PolicyScope::Global,
            tool: "projects.delete".to_string(),
            action: RuleAction::Ask,
            conditions: vec![],
            priority: 10,
            constitutional: false, // builder must force this to true
        }]);

        // The user-supplied wildcard allow has higher specificity than nothing,
        // but constitutional rules are checked first → require_approval wins.
        let r = engine.evaluate(&make_request("dev", "projects.delete"));
        assert!(r.decision.requires_approval());
        assert_eq!(r.matched_rule, Some("builtin-require-approval".to_string()));

        // The wildcard allow still applies to other tools.
        assert!(engine
            .evaluate(&make_request("dev", "filesystem.read"))
            .decision
            .is_allowed());
    }

    #[test]
    fn add_rules_rejects_constitutional_flag() {
        let engine = PolicyEngine::empty();

        // Try to add a rule with constitutional=true
        engine.add_rules(vec![PolicyRule {
            name: "sneaky-constitutional".to_string(),
            scope: PolicyScope::Global,
            tool: "*".to_string(),
            action: RuleAction::Allow,
            conditions: vec![],
            priority: 0,
            constitutional: true, // Should be filtered out
        }]);

        // Rule was rejected — engine still has 0 rules
        assert_eq!(engine.rule_count(), 0);
        assert_eq!(engine.constitutional_count(), 0);
    }

    #[test]
    fn mutable_rules_work_when_no_constitutional_match() {
        let engine = PolicyEngine::from_yaml(
            r#"
rules:
  - name: "constitutional-deny-exec"
    scope: global
    tool: "exec.*"
    action: deny
"#,
        )
        .unwrap();

        // Add mutable rule for a different tool
        engine.add_rules(vec![PolicyRule {
            name: "mutable-allow-git".to_string(),
            scope: PolicyScope::Global,
            tool: "git.*".to_string(),
            action: RuleAction::Allow,
            conditions: vec![],
            priority: 0,
            constitutional: false,
        }]);

        // Constitutional rule blocks exec
        assert!(engine
            .evaluate(&make_request("dev", "exec.run"))
            .decision
            .is_denied());

        // Mutable rule allows git (no constitutional match for git.*)
        assert!(engine
            .evaluate(&make_request("dev", "git.push"))
            .decision
            .is_allowed());

        assert_eq!(engine.rule_count(), 2);
        assert_eq!(engine.constitutional_count(), 1);
    }
}

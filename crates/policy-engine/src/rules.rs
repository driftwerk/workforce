use serde::{Deserialize, Serialize};
use workforce_core::policy::PolicyDecision;

/// A policy rule as defined in configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Human-readable name for this rule.
    pub name: String,
    /// Scope at which this rule applies (global, team, role, agent).
    pub scope: PolicyScope,
    /// The tool name pattern to match (supports wildcards: "filesystem.*").
    pub tool: String,
    /// The action to take when this rule matches.
    pub action: RuleAction,
    /// Optional conditions for more granular matching.
    #[serde(default)]
    pub conditions: Vec<RuleCondition>,
    /// Priority (higher = evaluated first). Default: 0.
    #[serde(default)]
    pub priority: i32,
    /// If true, this rule cannot be overridden or removed by agent proposals.
    /// Rules loaded from config at startup are constitutional; proposal-added rules are not.
    #[serde(default)]
    pub constitutional: bool,
}

/// The scope a policy rule applies to.
///
/// In YAML, use either:
/// - `scope: global`
/// - `scope: { team: "backend-dev" }`
/// - `scope: { role: "developer" }`
/// - `scope: { agent: "dev-1" }`
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PolicyScope {
    Global,
    Team { team: String },
    Role { role: String },
    Agent { agent: String },
}

impl PolicyScope {
    /// Returns the specificity level (higher = more specific).
    /// Used for composition: most specific matching rule wins.
    pub fn specificity(&self) -> u8 {
        match self {
            PolicyScope::Global => 0,
            PolicyScope::Team { .. } => 1,
            PolicyScope::Role { .. } => 2,
            PolicyScope::Agent { .. } => 3,
        }
    }
}

/// Custom deserialiser for PolicyScope.
/// Accepts either the string `"global"` or a map like `{ team: "x" }`.
impl<'de> Deserialize<'de> for PolicyScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        struct ScopeMap {
            team: Option<String>,
            role: Option<String>,
            agent: Option<String>,
        }

        struct ScopeVisitor;

        impl<'de> de::Visitor<'de> for ScopeVisitor {
            type Value = PolicyScope;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"global\" or a map with one of: team, role, agent")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<PolicyScope, E> {
                if value == "global" {
                    Ok(PolicyScope::Global)
                } else {
                    Err(E::custom(format!(
                        "unknown scope string: '{value}', expected 'global'"
                    )))
                }
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<PolicyScope, M::Error> {
                let scope_map: ScopeMap =
                    Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))?;
                if let Some(agent) = scope_map.agent {
                    Ok(PolicyScope::Agent { agent })
                } else if let Some(role) = scope_map.role {
                    Ok(PolicyScope::Role { role })
                } else if let Some(team) = scope_map.team {
                    Ok(PolicyScope::Team { team })
                } else {
                    Err(de::Error::custom(
                        "scope map must contain one of: team, role, agent",
                    ))
                }
            }
        }

        deserializer.deserialize_any(ScopeVisitor)
    }
}

/// The action a policy rule prescribes.
///
/// `Ask` and `AlwaysAsk` were added in the User RBAC + Permission
/// Cascade work; before that the runner only knew `Allow` / `Deny` /
/// `RequireApproval`. Both new variants currently emit
/// [`PolicyDecision::RequireApproval`] so the existing approval channel
/// keeps working unchanged — the distinction between them (whether the
/// user is allowed to save Always-Allow at runtime) is enforced by the
/// downstream cascade in `workforce-permissions`, not by the policy
/// engine itself.
///
/// `require_approval` is kept as a serde alias for `ask` so YAML rules
/// shipped before this change continue to deserialise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Deny,
    #[serde(alias = "require_approval")]
    Ask,
    AlwaysAsk,
}

impl RuleAction {
    pub fn to_decision(&self, rule_name: &str) -> PolicyDecision {
        match self {
            RuleAction::Allow => PolicyDecision::Allow,
            RuleAction::Deny => PolicyDecision::Deny {
                reason: format!("Denied by rule: {rule_name}"),
            },
            RuleAction::Ask => PolicyDecision::RequireApproval {
                reason: format!("Approval required by rule: {rule_name}"),
                allow_save_allow: true,
            },
            RuleAction::AlwaysAsk => PolicyDecision::RequireApproval {
                reason: format!("Approval required (always-ask) by rule: {rule_name}"),
                allow_save_allow: false,
            },
        }
    }

    /// True for states that need a runtime prompt. Both `Ask` and
    /// `AlwaysAsk` qualify; the surface layer (phase 4) uses the
    /// distinction to decide whether to render the "Always Allow"
    /// button.
    pub fn requires_prompt(&self) -> bool {
        matches!(self, Self::Ask | Self::AlwaysAsk)
    }
}

/// A condition within a policy rule for fine-grained matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleCondition {
    /// The parameter path to check (e.g., "path", "recipient.domain").
    pub field: String,
    /// The comparison operator.
    pub operator: ConditionOperator,
    /// The value to compare against.
    pub value: serde_json::Value,
    /// The action if this specific condition matches.
    pub action: RuleAction,
}

/// Comparison operators for rule conditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOperator {
    /// Exact equality.
    Equals,
    /// Not equal.
    NotEquals,
    /// String starts with.
    StartsWith,
    /// String contains.
    Contains,
    /// Value is in a list.
    In,
    /// Value is not in a list.
    NotIn,
    /// Numeric less than.
    LessThan,
    /// Numeric greater than.
    GreaterThan,
}

/// A collection of policy rules loaded from configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub rules: Vec<PolicyRule>,
}

impl PolicyConfig {
    /// Load policy rules from a YAML file.
    pub fn from_yaml(yaml: &str) -> eyre::Result<Self> {
        serde_yaml::from_str(yaml).map_err(|e| eyre::eyre!("Failed to parse policy YAML: {}", e))
    }

    /// Load policy rules from a YAML file path.
    pub fn from_file(path: &str) -> eyre::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("Failed to read {}: {}", path, e))?;
        Self::from_yaml(&content)
    }
}

/// Check if a tool name matches a pattern (supports trailing wildcard `*`).
pub fn tool_matches(pattern: &str, tool_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern == DESTRUCTIVE_PATTERN {
        return is_destructive(tool_name);
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        tool_name.starts_with(prefix)
            && tool_name.len() > prefix.len()
            && tool_name.as_bytes()[prefix.len()] == b'.'
    } else {
        pattern == tool_name
    }
}

/// Reserved tool pattern that matches any tool [`is_destructive`] classifies as
/// destructive — the target for an org-wide "confirm destructive actions"
/// baseline rule.
pub const DESTRUCTIVE_PATTERN: &str = "@destructive";

/// Best-effort classifier for destructive tool calls (delete / remove /
/// destroy …) behind [`DESTRUCTIVE_PATTERN`], so one baseline rule can gate
/// destructive actions across built-in, skill, and MCP tools — whose names are
/// the only signal common to all three. Tokenises on `_ . - : /` and matches a
/// token against a curated verb set; fused or camelCase names may slip through
/// and are refined by explicit rules.
pub fn is_destructive(tool_name: &str) -> bool {
    const DESTRUCTIVE_VERBS: &[&str] = &[
        "delete",
        "del",
        "destroy",
        "drop",
        "remove",
        "rm",
        "rmdir",
        "purge",
        "wipe",
        "erase",
        "truncate",
        "terminate",
        "kill",
        "revoke",
        "unlink",
        "deprovision",
        "prune",
        "obliterate",
        "expunge",
        "uninstall",
        "teardown",
        "overwrite",
    ];
    tool_name.split(['_', '.', '-', ':', '/']).any(|token| {
        DESTRUCTIVE_VERBS
            .iter()
            .any(|verb| token.eq_ignore_ascii_case(verb))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_matching_exact() {
        assert!(tool_matches("filesystem.read", "filesystem.read"));
        assert!(!tool_matches("filesystem.read", "filesystem.write"));
    }

    #[test]
    fn tool_matching_wildcard() {
        assert!(tool_matches("filesystem.*", "filesystem.read"));
        assert!(tool_matches("filesystem.*", "filesystem.write"));
        assert!(!tool_matches("filesystem.*", "exec.run"));
        assert!(!tool_matches("filesystem.*", "filesystem")); // Must have something after the dot
    }

    #[test]
    fn tool_matching_global_wildcard() {
        assert!(tool_matches("*", "anything"));
        assert!(tool_matches("*", "filesystem.read"));
    }

    #[test]
    fn destructive_classifier_flags_destructive_verbs() {
        assert!(is_destructive("vault.delete"));
        assert!(is_destructive("aws.s3.purge"));
        assert!(is_destructive("DELETE_ALL")); // case-insensitive
        assert!(is_destructive("jira_delete_issue"));
        assert!(is_destructive("linear_document_delete"));
        assert!(!is_destructive("metabase_card_get"));
        assert!(!is_destructive("memory.recall"));
        // Token equality keeps related-but-safe names out.
        assert!(!is_destructive("undelete"));
        assert!(!is_destructive("dropdown_list"));
    }

    #[test]
    fn destructive_pattern_matches_via_tool_matches() {
        assert!(tool_matches("@destructive", "vault.delete"));
        assert!(tool_matches(
            "@destructive",
            "mcp__workforce-local__linear_document_delete"
        ));
        assert!(!tool_matches("@destructive", "jira_get_issue"));
        // The reserved pattern is not treated as a literal tool name.
        assert!(!tool_matches("@destructive", "@destructive"));
    }

    #[test]
    fn scope_specificity_ordering() {
        assert!(
            PolicyScope::Agent {
                agent: "dev".to_string()
            }
            .specificity()
                > PolicyScope::Global.specificity()
        );
        assert!(
            PolicyScope::Role {
                role: "dev".to_string()
            }
            .specificity()
                > PolicyScope::Team {
                    team: "t".to_string()
                }
                .specificity()
        );
    }

    #[test]
    fn parse_policy_yaml() {
        let yaml = r#"
rules:
  - name: "allow-read"
    scope: global
    tool: "filesystem.read"
    action: allow
  - name: "deny-exec-outside-workspace"
    scope: global
    tool: "exec.*"
    action: deny
    priority: -1
"#;
        let config = PolicyConfig::from_yaml(yaml).expect("parse failed");
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].name, "allow-read");
        assert_eq!(config.rules[0].action, RuleAction::Allow);
        assert_eq!(config.rules[1].tool, "exec.*");
    }

    #[test]
    fn rule_action_to_decision() {
        let allow = RuleAction::Allow.to_decision("test");
        assert!(allow.is_allowed());

        let deny = RuleAction::Deny.to_decision("test");
        assert!(deny.is_denied());

        let ask = RuleAction::Ask.to_decision("test");
        assert!(ask.requires_approval());

        let always_ask = RuleAction::AlwaysAsk.to_decision("test");
        assert!(always_ask.requires_approval());
    }

    #[test]
    fn require_approval_yaml_alias_still_parses() {
        // YAML written before the Ask/AlwaysAsk split shipped used
        // `action: require_approval`. Existing deployments must keep
        // working when this code lands.
        let yaml = r#"
rules:
  - name: legacy-rule
    scope: global
    tool: dangerous.*
    action: require_approval
    priority: 0
"#;
        let config = PolicyConfig::from_yaml(yaml).expect("legacy alias parsed");
        assert_eq!(config.rules[0].action, RuleAction::Ask);
    }

    #[test]
    fn requires_prompt_classification() {
        assert!(!RuleAction::Allow.requires_prompt());
        assert!(!RuleAction::Deny.requires_prompt());
        assert!(RuleAction::Ask.requires_prompt());
        assert!(RuleAction::AlwaysAsk.requires_prompt());
    }
}

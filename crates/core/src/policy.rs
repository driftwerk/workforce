use serde::{Deserialize, Serialize};

use crate::types::AgentId;

/// The result of a policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    /// The action is allowed.
    Allow,
    /// The action is denied.
    Deny { reason: String },
    /// The action requires human approval before proceeding.
    ///
    /// `allow_save_allow` distinguishes the two prompt kinds introduced
    /// with the User RBAC + Permission Cascade work: when `true` the
    /// runtime offers the user an "Always Allow" option that saves a
    /// User-scope rule; when `false` (the `AlwaysAsk` action) Always-
    /// Allow is hidden and only the per-call Allow/Deny + Always-Deny
    /// are offered. Defaults to `true` so YAML rules written before
    /// the split (only `RequireApproval` existed) keep the previous
    /// behaviour — they're treated as Ask.
    RequireApproval {
        reason: String,
        #[serde(default = "default_allow_save_allow")]
        allow_save_allow: bool,
    },
}

fn default_allow_save_allow() -> bool {
    true
}

/// Build the prompt-options struct from a `RequireApproval`-shaped
/// decision. Centralises the policy → prompt mapping so the runner
/// and any future evaluator agree on which buttons to surface.
impl PolicyDecision {
    pub fn prompt_options(&self) -> Option<PermissionPromptOptions> {
        match self {
            PolicyDecision::RequireApproval {
                allow_save_allow, ..
            } => Some(PermissionPromptOptions::ask(*allow_save_allow)),
            _ => None,
        }
    }
}

/// Set of options the surface should render at an `ask` / `always_ask`
/// prompt. Decoupled from the choice enum so a single user click maps
/// cleanly onto exactly one option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPromptOptions {
    pub allow_once: bool,
    pub deny_once: bool,
    pub allow_always: bool,
    pub deny_always: bool,
}

impl PermissionPromptOptions {
    /// Standard `ask` prompt. `allow_save_allow = false` collapses to
    /// the `always_ask` shape (no "Always Allow" button).
    pub fn ask(allow_save_allow: bool) -> Self {
        Self {
            allow_once: true,
            deny_once: true,
            allow_always: allow_save_allow,
            deny_always: true,
        }
    }
}

impl Default for PermissionPromptOptions {
    fn default() -> Self {
        Self::ask(true)
    }
}

/// What the user clicked at the runtime prompt. The runner branches
/// on this to decide whether to persist a User-scope rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionChoice {
    /// Allow this call only.
    AllowOnce,
    /// Deny this call only.
    DenyOnce,
    /// Allow this call AND save a User-scope `allow` rule.
    AllowAlways,
    /// Deny this call AND save a User-scope `deny` rule.
    DenyAlways,
}

impl PermissionChoice {
    /// True for this call only.
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }

    /// True for choices that the runner must persist as a User-scope
    /// rule before resolving the approval.
    pub fn persists(&self) -> bool {
        matches!(self, Self::AllowAlways | Self::DenyAlways)
    }

    /// Map an approve/deny boolean (legacy approval flow) onto a
    /// choice. Surfaces that haven't been upgraded to send a typed
    /// `PermissionChoice` still send `approved: bool`; this is how
    /// the runner upgrades that into the new vocabulary.
    pub fn from_legacy_bool(approved: bool) -> Self {
        if approved {
            Self::AllowOnce
        } else {
            Self::DenyOnce
        }
    }
}

/// A request to evaluate a policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRequest {
    /// The agent requesting the action.
    pub agent_id: AgentId,
    /// The tool being invoked.
    pub tool_name: String,
    /// Parameters for the tool invocation.
    pub parameters: serde_json::Value,
    /// Additional context (node_id, team_id, etc.).
    pub context: PolicyContext,
}

/// Contextual information for policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyContext {
    /// The node the agent is running on.
    pub node_id: Option<String>,
    /// The team the agent belongs to.
    pub team_id: Option<String>,
    /// The task being worked on.
    pub task_id: Option<String>,
    /// The agent's role (e.g. "developer", "reviewer").
    #[serde(default)]
    pub role: Option<String>,
}

impl PolicyContext {
    pub fn empty() -> Self {
        Self {
            node_id: None,
            team_id: None,
            task_id: None,
            role: None,
        }
    }
}

/// A policy evaluation response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyResponse {
    /// The decision.
    pub decision: PolicyDecision,
    /// Which policy rule matched.
    pub matched_rule: Option<String>,
    /// Time taken to evaluate (microseconds).
    pub evaluation_time_us: u64,
}

impl PolicyDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PolicyDecision::Allow)
    }

    pub fn is_denied(&self) -> bool {
        matches!(self, PolicyDecision::Deny { .. })
    }

    pub fn requires_approval(&self) -> bool {
        matches!(self, PolicyDecision::RequireApproval { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_decision_checks() {
        let allow = PolicyDecision::Allow;
        assert!(allow.is_allowed());
        assert!(!allow.is_denied());
        assert!(!allow.requires_approval());

        let deny = PolicyDecision::Deny {
            reason: "not permitted".to_string(),
        };
        assert!(!deny.is_allowed());
        assert!(deny.is_denied());

        let approve = PolicyDecision::RequireApproval {
            reason: "sensitive action".to_string(),
            allow_save_allow: true,
        };
        assert!(approve.requires_approval());
    }

    #[test]
    fn prompt_options_reflect_allow_save_allow_flag() {
        let ask = PolicyDecision::RequireApproval {
            reason: "ask".to_string(),
            allow_save_allow: true,
        };
        let always_ask = PolicyDecision::RequireApproval {
            reason: "always_ask".to_string(),
            allow_save_allow: false,
        };
        assert!(ask.prompt_options().unwrap().allow_always);
        assert!(!always_ask.prompt_options().unwrap().allow_always);
        assert!(PolicyDecision::Allow.prompt_options().is_none());
    }

    #[test]
    fn legacy_require_approval_json_deserialises() {
        // Wire payloads from before the allow_save_allow field landed
        // omit it; serde must default to true (Ask semantics) so a
        // mid-deploy node receiving an older payload doesn't suddenly
        // hide the Always-Allow button.
        let legacy = r#"{"require_approval":{"reason":"legacy"}}"#;
        let decoded: PolicyDecision = serde_json::from_str(legacy).unwrap();
        match decoded {
            PolicyDecision::RequireApproval {
                reason,
                allow_save_allow,
            } => {
                assert_eq!(reason, "legacy");
                assert!(allow_save_allow, "default must be true (Ask)");
            }
            _ => panic!("expected RequireApproval"),
        }
    }

    #[test]
    fn permission_choice_legacy_bool_mapping() {
        assert_eq!(
            PermissionChoice::from_legacy_bool(true),
            PermissionChoice::AllowOnce
        );
        assert_eq!(
            PermissionChoice::from_legacy_bool(false),
            PermissionChoice::DenyOnce
        );
        assert!(PermissionChoice::AllowAlways.is_allow());
        assert!(PermissionChoice::AllowAlways.persists());
        assert!(!PermissionChoice::AllowOnce.persists());
        assert!(PermissionChoice::DenyAlways.persists());
    }

    #[test]
    fn policy_request_serialises() {
        let req = PolicyRequest {
            agent_id: AgentId::new("developer"),
            tool_name: "filesystem.write".to_string(),
            parameters: serde_json::json!({"path": "/workspace/test.rs"}),
            context: PolicyContext::empty(),
        };
        let json = serde_json::to_string(&req).expect("serialise failed");
        assert!(json.contains("filesystem.write"));
    }
}

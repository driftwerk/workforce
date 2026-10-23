use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// Restrictiveness ordering — `deny > always_ask > ask > allow`. Mat's
/// design (see plan doc): a lower hierarchy level may only set a state
/// **as restrictive as or more restrictive than** the level above; the
/// runtime evaluator takes the most-restrictive value across the admin
/// chain. The `Ord` derive is intentional — `max_restrictive` is just
/// `Ord::max` on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PermissionState {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "ask")]
    Ask,
    #[serde(rename = "always_ask")]
    AlwaysAsk,
    #[serde(rename = "deny")]
    Deny,
}

impl PermissionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::AlwaysAsk => "always_ask",
            Self::Deny => "deny",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "allow" => Self::Allow,
            "ask" => Self::Ask,
            "always_ask" => Self::AlwaysAsk,
            "deny" => Self::Deny,
            _ => Self::Deny,
        }
    }

    /// Most-restrictive of two states — `Ord::max` by another name. Kept
    /// as a named function so call sites read like the plan reads.
    pub fn most_restrictive(a: Self, b: Self) -> Self {
        a.max(b)
    }
}

/// Which level of the hierarchy a rule lives at. The System level is
/// **not** represented in this enum: System rules stay YAML-loaded with
/// `constitutional: true` (handled by the existing `policy-engine`).
/// This enum only covers what the DB stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    Team,
    Agent,
    User,
}

impl PermissionScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Agent => "agent",
            Self::User => "user",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "team" => Self::Team,
            "agent" => Self::Agent,
            _ => Self::User,
        }
    }
}

/// A target that may authorise a routed approval — either a specific
/// identity or every holder of a named role. Roles are expanded to user
/// ids by the hub adapter at decision time (membership is live).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApproverRef {
    /// Internal identity UUID (stringified).
    User { id: String },
    /// `"system_admin" | "manager" | "member"`.
    Role { role: String },
}

fn default_true() -> bool {
    true
}

/// How a rule entered the store. Drives the audit log and the "manage
/// my saved decisions" UI; the cascade itself does not branch on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOrigin {
    /// An operator wrote the rule through the admin UI or API.
    Admin,
    /// The end user clicked "Always Allow" / "Always Deny" at runtime.
    UserRuntimeChoice,
    /// An approved `policy_proposals` row was materialised here.
    Proposal,
}

impl PermissionOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::UserRuntimeChoice => "user_runtime_choice",
            Self::Proposal => "proposal",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "admin" => Self::Admin,
            "user_runtime_choice" => Self::UserRuntimeChoice,
            "proposal" => Self::Proposal,
            _ => Self::Admin,
        }
    }
}

/// A single permission rule scoped to a Team / Agent / User. System
/// rules live in YAML and never instantiate this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub id: Uuid,
    pub scope: PermissionScope,
    /// Team UUID, agent id, or user UUID depending on `scope`. Kept
    /// untyped because agent ids are not UUIDs in the existing model.
    pub scope_id: String,
    /// Tool name pattern (`memory.*`, `vault.delete`, `*`). Matched via
    /// `workforce_policy::rules::tool_matches` so semantics stay
    /// identical to the existing engine.
    pub tool_pattern: String,
    pub state: PermissionState,
    /// Optional parameter-level conditions. Schema lifted verbatim from
    /// `workforce_policy::rules::RuleCondition` — stored as JSONB so the
    /// shape is owned by the policy crate, not duplicated here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<JsonValue>,
    /// Approver targets for routed approval. Empty ⇒ no routing
    /// (self-serve). Only meaningful on Ask/AlwaysAsk rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<ApproverRef>,
    /// When false, the requester may not satisfy their own approval even
    /// if a named role would include them (four-eyes). Default true.
    #[serde(default = "default_true")]
    pub allow_self_approval: bool,
    pub origin: PermissionOrigin,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl PermissionRule {
    pub fn new(
        scope: PermissionScope,
        scope_id: impl Into<String>,
        tool_pattern: impl Into<String>,
        state: PermissionState,
        origin: PermissionOrigin,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            scope,
            scope_id: scope_id.into(),
            tool_pattern: tool_pattern.into(),
            state,
            conditions: None,
            approvers: Vec::new(),
            allow_self_approval: true,
            origin,
            created_by: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_conditions(mut self, conditions: JsonValue) -> Self {
        self.conditions = Some(conditions);
        self
    }

    pub fn with_approvers(mut self, approvers: Vec<ApproverRef>) -> Self {
        self.approvers = approvers;
        self
    }

    pub fn with_allow_self_approval(mut self, v: bool) -> Self {
        self.allow_self_approval = v;
        self
    }

    pub fn with_created_by(mut self, identity_id: Uuid) -> Self {
        self.created_by = Some(identity_id);
        self
    }
}

/// Filter shape for `PermissionStore::list`. Empty filter lists every
/// rule; combinable predicates AND together.
///
/// `scope` and `scopes` are mutually consistent: when `scopes` is `Some`
/// the backend must filter to those values (`scope_type IN (…)`); when
/// it's `None` the single `scope` field applies as before. The two-field
/// shape keeps the common single-scope case ergonomic while letting the
/// admin write-time check ask for "Team OR Agent" in one query.
#[derive(Debug, Clone, Default)]
pub struct PermissionFilter {
    pub scope: Option<PermissionScope>,
    pub scopes: Option<Vec<PermissionScope>>,
    pub scope_id: Option<String>,
    pub tool_pattern: Option<String>,
    pub origin: Option<PermissionOrigin>,
    pub limit: Option<usize>,
}

/// Coarse RBAC role: a named bundle of `permission_rules` that can be
/// granted to identities. Roles are team-scoped — the team that owns a
/// role can grant it to its own members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRoleDef {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub created_at: DateTime<Utc>,
}

impl PermissionRoleDef {
    pub fn new(team_id: Uuid, name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            team_id,
            name: name.into(),
            description: String::new(),
            created_at: Utc::now(),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }
}

/// One row of `identity_role_membership`: which user holds which role,
/// who granted it, when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRoleAssignment {
    pub identity_id: Uuid,
    pub role_id: Uuid,
    pub granted_by: Option<Uuid>,
    pub granted_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_ordering_matches_plan_restrictiveness() {
        assert!(PermissionState::Allow < PermissionState::Ask);
        assert!(PermissionState::Ask < PermissionState::AlwaysAsk);
        assert!(PermissionState::AlwaysAsk < PermissionState::Deny);

        assert_eq!(
            PermissionState::most_restrictive(PermissionState::Ask, PermissionState::Allow),
            PermissionState::Ask
        );
        assert_eq!(
            PermissionState::most_restrictive(PermissionState::Deny, PermissionState::AlwaysAsk),
            PermissionState::Deny
        );
    }

    #[test]
    fn state_as_str_round_trips() {
        for s in [
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ] {
            assert_eq!(PermissionState::from_str_lossy(s.as_str()), s);
        }
    }

    #[test]
    fn state_as_str_matches_serde_serialisation() {
        for s in [
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ] {
            let json = serde_json::to_value(s).unwrap();
            assert_eq!(json.as_str(), Some(s.as_str()));
        }
    }

    #[test]
    fn scope_and_origin_round_trip() {
        for s in [
            PermissionScope::Team,
            PermissionScope::Agent,
            PermissionScope::User,
        ] {
            assert_eq!(PermissionScope::from_str_lossy(s.as_str()), s);
        }
        for o in [
            PermissionOrigin::Admin,
            PermissionOrigin::UserRuntimeChoice,
            PermissionOrigin::Proposal,
        ] {
            assert_eq!(PermissionOrigin::from_str_lossy(o.as_str()), o);
        }
    }
}

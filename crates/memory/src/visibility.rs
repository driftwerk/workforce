//! Memory visibility scopes — controls who can see and access memories.
//!
//! Implements the visibility model, refactored
//! in DW-525 to centre the access decision on a `UserId` subject rather
//! than on `AgentId`. In single-agent deployments (one support agent for
//! a whole team) every user shares one agent — the previous agent-centric
//! model collapsed `Visibility::Private` to "private to the agent" rather
//! than "private to the requesting user."

use serde::{Deserialize, Serialize};
use workforce_core::types::AgentId;

use crate::types::{Memory, UserId, Visibility};

/// The principal a visibility check is being evaluated for. Carries the
/// user subject (always required) plus optional agent / team / org
/// membership. Agent is optional because the post-filter callers
/// (defence-in-depth in `MemoryStore::recall`) don't have the requester's
/// agent in scope; `Shared { agents }` grants are denied when agent is
/// `None`, which is the conservative default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessSubject<'a> {
    pub user: &'a UserId,
    pub agent: Option<&'a AgentId>,
    pub team: Option<&'a str>,
    pub organisation: Option<&'a str>,
}

impl<'a> AccessSubject<'a> {
    pub fn for_user(user: &'a UserId) -> Self {
        Self {
            user,
            agent: None,
            team: None,
            organisation: None,
        }
    }

    pub fn with_agent(mut self, agent: &'a AgentId) -> Self {
        self.agent = Some(agent);
        self
    }

    pub fn with_team(mut self, team: &'a str) -> Self {
        self.team = Some(team);
        self
    }

    pub fn with_organisation(mut self, organisation: &'a str) -> Self {
        self.organisation = Some(organisation);
        self
    }
}

/// Extended visibility scope with team and org support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum VisibilityScope {
    /// Only the owning user can access.
    Private,
    /// Shared with specific agents (cross-agent collaboration grant).
    Shared { agents: Vec<AgentId> },
    /// Shared with an entire team.
    Team { team_id: String },
    /// Shared across an entire organisation.
    Organisation { org_id: String },
    /// Globally accessible (all users, all orgs).
    Global,
}

impl VisibilityScope {
    /// Check if `subject` can access a memory with this scope, given the
    /// memory's owning user.
    pub fn can_access(&self, subject: &AccessSubject<'_>, owner_user: Option<&UserId>) -> bool {
        let same_user = owner_user.is_some_and(|u| u == subject.user);
        match self {
            VisibilityScope::Private => same_user,
            VisibilityScope::Shared { agents } => {
                same_user || subject.agent.is_some_and(|a| agents.contains(a))
            }
            VisibilityScope::Team { team_id } => {
                same_user || subject.team.is_some_and(|t| t == team_id.as_str())
            }
            VisibilityScope::Organisation { org_id } => {
                same_user || subject.organisation.is_some_and(|o| o == org_id.as_str())
            }
            VisibilityScope::Global => true,
        }
    }

    /// Convert from the simpler [`Visibility`] enum.
    pub fn from_basic(vis: &Visibility) -> Self {
        match vis {
            Visibility::Private => VisibilityScope::Private,
            Visibility::Team(team_id) => VisibilityScope::Team {
                team_id: team_id.clone(),
            },
            Visibility::Global => VisibilityScope::Global,
        }
    }
}

/// Access control decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    /// Access granted.
    Allow,
    /// Access denied with reason.
    Deny(String),
}

impl AccessDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, AccessDecision::Allow)
    }
}

/// Memory access controller — evaluates visibility rules.
pub struct AccessController {
    pub default_visibility: VisibilityScope,
}

impl AccessController {
    pub fn new() -> Self {
        Self {
            default_visibility: VisibilityScope::Private,
        }
    }

    /// Check if a subject can read a memory.
    pub fn check_read(&self, memory: &Memory, subject: &AccessSubject<'_>) -> AccessDecision {
        if memory.deleted {
            return AccessDecision::Deny("Memory has been deleted".to_string());
        }

        let scope = VisibilityScope::from_basic(&memory.visibility);
        let owner_user = memory.identity_id.as_deref().map(UserId::from);
        if scope.can_access(subject, owner_user.as_ref()) {
            AccessDecision::Allow
        } else {
            AccessDecision::Deny(format!(
                "User '{}' does not have access to memory owned by '{}'",
                subject.user,
                owner_user
                    .as_ref()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| format!("agent:{}", memory.owner)),
            ))
        }
    }

    /// Check if a subject can modify a memory.
    ///
    /// Only the owning user (or an admin) may write. Agent-level grants
    /// don't extend to writes — modifying another user's memory is never
    /// implicit, even within a "shared" scope.
    pub fn check_write(
        &self,
        memory: &Memory,
        subject: &AccessSubject<'_>,
        is_admin: bool,
    ) -> AccessDecision {
        if is_admin {
            return AccessDecision::Allow;
        }
        let owner_user = memory.identity_id.as_deref().map(UserId::from);
        match owner_user {
            Some(ref u) if u == subject.user => AccessDecision::Allow,
            Some(ref u) => {
                AccessDecision::Deny(format!("Only the owner '{u}' can modify this memory"))
            }
            None => AccessDecision::Deny(
                "Cannot modify a memory with no recorded owner identity".to_string(),
            ),
        }
    }

    /// Filter a list of memories to only those readable by `subject`.
    pub fn filter_accessible<'m>(
        &self,
        memories: &'m [Memory],
        subject: &AccessSubject<'_>,
    ) -> Vec<&'m Memory> {
        memories
            .iter()
            .filter(|m| self.check_read(m, subject).is_allow())
            .collect()
    }
}

impl Default for AccessController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryLayer;

    fn agent() -> AgentId {
        AgentId::new("atlas")
    }

    fn user(name: &str) -> UserId {
        UserId::new(name)
    }

    fn make_memory(owner_user: Option<&str>, visibility: Visibility) -> Memory {
        let mut m = Memory::new("note", MemoryLayer::Episodic, agent()).with_visibility(visibility);
        if let Some(uid) = owner_user {
            m = m.with_identity_id(uid);
        }
        m
    }

    #[test]
    fn private_scope_only_owner_user_can_read() {
        let alice = user("alice");
        let bob = user("bob");
        let mem = make_memory(Some("alice"), Visibility::Private);
        let controller = AccessController::new();

        assert!(controller
            .check_read(&mem, &AccessSubject::for_user(&alice))
            .is_allow());
        assert!(matches!(
            controller.check_read(&mem, &AccessSubject::for_user(&bob)),
            AccessDecision::Deny(_)
        ));
    }

    #[test]
    fn private_scope_with_no_owner_user_denies_everyone() {
        // DW-525: regression — pre-fix, AccessController allowed Private memories
        // to anyone whose AgentId matched the memory's owning agent. With single-
        // agent deployments (Atlas) that meant every user.
        let alice = user("alice");
        let mem = make_memory(None, Visibility::Private);
        let controller = AccessController::new();

        assert!(matches!(
            controller.check_read(&mem, &AccessSubject::for_user(&alice)),
            AccessDecision::Deny(_)
        ));
    }

    #[test]
    fn team_scope_grants_team_members() {
        let alice = user("alice");
        let bob = user("bob");
        let mem = make_memory(Some("alice"), Visibility::Team("backend".to_string()));
        let controller = AccessController::new();

        let bob_in_team = AccessSubject::for_user(&bob).with_team("backend");
        let bob_outside = AccessSubject::for_user(&bob);

        assert!(controller.check_read(&mem, &bob_in_team).is_allow());
        assert!(matches!(
            controller.check_read(&mem, &bob_outside),
            AccessDecision::Deny(_)
        ));
        assert!(controller
            .check_read(&mem, &AccessSubject::for_user(&alice))
            .is_allow());
    }

    #[test]
    fn organisation_scope_grants_org_members() {
        let scope = VisibilityScope::Organisation {
            org_id: "contoso".to_string(),
        };
        let alice = user("alice");
        let owner_user = UserId::new("bob");

        let in_org = AccessSubject::for_user(&alice).with_organisation("contoso");
        let other_org = AccessSubject::for_user(&alice).with_organisation("acme");

        assert!(scope.can_access(&in_org, Some(&owner_user)));
        assert!(!scope.can_access(&other_org, Some(&owner_user)));
    }

    #[test]
    fn global_scope_grants_everyone() {
        let scope = VisibilityScope::Global;
        let anon = user("anon");
        let owner_user = UserId::new("owner");
        assert!(scope.can_access(&AccessSubject::for_user(&anon), Some(&owner_user)));
    }

    #[test]
    fn shared_scope_grants_listed_agents() {
        let other_agent = AgentId::new("penny");
        let third_agent = AgentId::new("clawdy");
        let scope = VisibilityScope::Shared {
            agents: vec![other_agent.clone()],
        };
        let alice = user("alice");
        let owner_user = UserId::new("bob");

        let via_other_agent = AccessSubject::for_user(&alice).with_agent(&other_agent);
        let via_third_agent = AccessSubject::for_user(&alice).with_agent(&third_agent);
        let no_agent = AccessSubject::for_user(&alice);

        assert!(scope.can_access(&via_other_agent, Some(&owner_user)));
        assert!(!scope.can_access(&via_third_agent, Some(&owner_user)));
        assert!(
            !scope.can_access(&no_agent, Some(&owner_user)),
            "Shared denies cross-user access when subject agent is unknown"
        );
    }

    #[test]
    fn deleted_memory_denies_even_owner() {
        let alice = user("alice");
        let mut mem = make_memory(Some("alice"), Visibility::Private);
        mem.deleted = true;
        let controller = AccessController::new();

        assert!(matches!(
            controller.check_read(&mem, &AccessSubject::for_user(&alice)),
            AccessDecision::Deny(_)
        ));
    }

    #[test]
    fn check_write_only_owner_or_admin() {
        let alice = user("alice");
        let bob = user("bob");
        let controller = AccessController::new();
        let mem = make_memory(Some("alice"), Visibility::Global);

        assert!(controller
            .check_write(&mem, &AccessSubject::for_user(&alice), false)
            .is_allow());
        assert!(controller
            .check_write(&mem, &AccessSubject::for_user(&bob), true)
            .is_allow());
        assert!(matches!(
            controller.check_write(&mem, &AccessSubject::for_user(&bob), false),
            AccessDecision::Deny(_)
        ));
    }

    #[test]
    fn check_write_denies_when_no_owner_user() {
        let alice = user("alice");
        let controller = AccessController::new();
        let mem = make_memory(None, Visibility::Global);

        assert!(matches!(
            controller.check_write(&mem, &AccessSubject::for_user(&alice), false),
            AccessDecision::Deny(_)
        ));
    }

    #[test]
    fn filter_accessible_returns_only_allowed_memories() {
        let alice = user("alice");
        let memories = vec![
            make_memory(Some("alice"), Visibility::Private),
            make_memory(Some("alice"), Visibility::Global),
            make_memory(Some("bob"), Visibility::Private),
        ];
        let controller = AccessController::new();

        let visible = controller.filter_accessible(&memories, &AccessSubject::for_user(&alice));
        assert_eq!(visible.len(), 2);
        assert!(visible
            .iter()
            .all(|m| m.identity_id.as_deref() == Some("alice")
                || matches!(m.visibility, Visibility::Global)));
    }
}

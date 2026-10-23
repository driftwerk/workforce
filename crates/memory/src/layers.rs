//! Team and organisation memory layers — shared knowledge across teams and orgs.
//!
//! Implements the memory hierarchy:
//! Working → Episodic → Semantic → Team → Organisation

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use workforce_core::types::AgentId;

use crate::types::{Memory, MemoryLayer, Visibility};

/// Team memory — shared knowledge across a team of agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemory {
    pub team_id: String,
    pub memories: Vec<Memory>,
    pub conventions: Vec<Convention>,
    pub created_at: DateTime<Utc>,
}

/// Organisation-level memory — global knowledge and policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgMemory {
    pub org_id: String,
    pub memories: Vec<Memory>,
    pub policies: Vec<OrgPolicy>,
    pub created_at: DateTime<Utc>,
}

/// A team convention — reusable pattern or rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Convention {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub created_by: AgentId,
    pub created_at: DateTime<Utc>,
}

/// An organisation-level policy stored as memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgPolicy {
    pub id: String,
    pub name: String,
    pub content: String,
    pub enforced: bool,
    pub created_at: DateTime<Utc>,
}

/// Manages the memory layer hierarchy.
pub struct MemoryLayerManager {
    team_memories: HashMap<String, TeamMemory>,
    org_memories: HashMap<String, OrgMemory>,
}

impl MemoryLayerManager {
    pub fn new() -> Self {
        Self {
            team_memories: HashMap::new(),
            org_memories: HashMap::new(),
        }
    }

    /// Ensure a team memory store exists.
    pub fn ensure_team(&mut self, team_id: &str) {
        self.team_memories
            .entry(team_id.to_string())
            .or_insert_with(|| TeamMemory {
                team_id: team_id.to_string(),
                memories: Vec::new(),
                conventions: Vec::new(),
                created_at: Utc::now(),
            });
    }

    /// Ensure an org memory store exists.
    pub fn ensure_org(&mut self, org_id: &str) {
        self.org_memories
            .entry(org_id.to_string())
            .or_insert_with(|| OrgMemory {
                org_id: org_id.to_string(),
                memories: Vec::new(),
                policies: Vec::new(),
                created_at: Utc::now(),
            });
    }

    /// Add a memory to a team's shared store.
    pub fn add_team_memory(&mut self, team_id: &str, memory: Memory) {
        self.ensure_team(team_id);
        if let Some(team) = self.team_memories.get_mut(team_id) {
            team.memories.push(memory);
        }
    }

    /// Add a memory to an org's shared store.
    pub fn add_org_memory(&mut self, org_id: &str, memory: Memory) {
        self.ensure_org(org_id);
        if let Some(org) = self.org_memories.get_mut(org_id) {
            org.memories.push(memory);
        }
    }

    /// Add a convention to a team.
    pub fn add_convention(&mut self, team_id: &str, convention: Convention) {
        self.ensure_team(team_id);
        if let Some(team) = self.team_memories.get_mut(team_id) {
            team.conventions.push(convention);
        }
    }

    /// Add an org policy.
    pub fn add_org_policy(&mut self, org_id: &str, policy: OrgPolicy) {
        self.ensure_org(org_id);
        if let Some(org) = self.org_memories.get_mut(org_id) {
            org.policies.push(policy);
        }
    }

    /// Get all memories for a team.
    pub fn team_memories(&self, team_id: &str) -> Vec<&Memory> {
        self.team_memories
            .get(team_id)
            .map(|t| t.memories.iter().collect())
            .unwrap_or_default()
    }

    /// Get all memories for an org.
    pub fn org_memories(&self, org_id: &str) -> Vec<&Memory> {
        self.org_memories
            .get(org_id)
            .map(|o| o.memories.iter().collect())
            .unwrap_or_default()
    }

    /// Get team conventions.
    pub fn conventions(&self, team_id: &str) -> Vec<&Convention> {
        self.team_memories
            .get(team_id)
            .map(|t| t.conventions.iter().collect())
            .unwrap_or_default()
    }

    /// Get org policies.
    pub fn org_policies(&self, org_id: &str) -> Vec<&OrgPolicy> {
        self.org_memories
            .get(org_id)
            .map(|o| o.policies.iter().collect())
            .unwrap_or_default()
    }

    /// Gather all memories an agent has access to across layers.
    /// Returns: agent's own + team's + org's memories.
    pub fn gather_context<'a>(
        &'a self,
        agent_memories: &'a [Memory],
        team_id: Option<&str>,
        org_id: Option<&str>,
    ) -> Vec<&'a Memory> {
        let mut result: Vec<&'a Memory> = agent_memories.iter().collect();

        if let Some(tid) = team_id {
            result.extend(self.team_memories(tid));
        }
        if let Some(oid) = org_id {
            result.extend(self.org_memories(oid));
        }

        result
    }

    /// Promote a memory from agent-level to team-level.
    pub fn promote_to_team(&mut self, memory: Memory, team_id: &str) -> Memory {
        let mut promoted = memory;
        promoted.layer = MemoryLayer::Team;
        promoted.visibility = Visibility::Team(team_id.to_string());
        self.add_team_memory(team_id, promoted.clone());
        promoted
    }

    /// Promote a memory from team-level to org-level.
    pub fn promote_to_org(&mut self, memory: Memory, org_id: &str) -> Memory {
        let mut promoted = memory;
        promoted.layer = MemoryLayer::Organisation;
        promoted.visibility = Visibility::Global;
        self.add_org_memory(org_id, promoted.clone());
        promoted
    }

    /// Count memories across all layers.
    pub fn stats(&self) -> LayerStats {
        let team_count: usize = self.team_memories.values().map(|t| t.memories.len()).sum();
        let org_count: usize = self.org_memories.values().map(|o| o.memories.len()).sum();
        let convention_count: usize = self
            .team_memories
            .values()
            .map(|t| t.conventions.len())
            .sum();
        let policy_count: usize = self.org_memories.values().map(|o| o.policies.len()).sum();

        LayerStats {
            teams: self.team_memories.len(),
            orgs: self.org_memories.len(),
            team_memories: team_count,
            org_memories: org_count,
            conventions: convention_count,
            policies: policy_count,
        }
    }
}

impl Default for MemoryLayerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Stats about the memory layer hierarchy.
#[derive(Debug, Default)]
pub struct LayerStats {
    pub teams: usize,
    pub orgs: usize,
    pub team_memories: usize,
    pub org_memories: usize,
    pub conventions: usize,
    pub policies: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_memory(content: &str, layer: MemoryLayer) -> Memory {
        Memory::new(content, layer, AgentId::new("test"))
    }

    #[test]
    fn team_memory_lifecycle() {
        let mut mgr = MemoryLayerManager::new();

        mgr.add_team_memory(
            "backend",
            make_memory("use eyre for errors", MemoryLayer::Team),
        );
        mgr.add_team_memory("backend", make_memory("never unwrap", MemoryLayer::Team));

        assert_eq!(mgr.team_memories("backend").len(), 2);
        assert_eq!(mgr.team_memories("frontend").len(), 0);
    }

    #[test]
    fn org_memory_lifecycle() {
        let mut mgr = MemoryLayerManager::new();

        mgr.add_org_memory(
            "contoso",
            make_memory("security first", MemoryLayer::Organisation),
        );

        assert_eq!(mgr.org_memories("contoso").len(), 1);
    }

    #[test]
    fn conventions() {
        let mut mgr = MemoryLayerManager::new();

        mgr.add_convention(
            "backend",
            Convention {
                id: "c1".to_string(),
                name: "Error Handling".to_string(),
                description: "Use eyre, never unwrap".to_string(),
                category: "code-quality".to_string(),
                created_by: AgentId::new("architect"),
                created_at: Utc::now(),
            },
        );

        assert_eq!(mgr.conventions("backend").len(), 1);
    }

    #[test]
    fn gather_context() {
        let mut mgr = MemoryLayerManager::new();

        let agent_memories = vec![make_memory("my memory", MemoryLayer::Episodic)];
        mgr.add_team_memory("backend", make_memory("team rule", MemoryLayer::Team));
        mgr.add_org_memory(
            "contoso",
            make_memory("org policy", MemoryLayer::Organisation),
        );

        let context = mgr.gather_context(&agent_memories, Some("backend"), Some("contoso"));
        assert_eq!(context.len(), 3);
    }

    #[test]
    fn promote_to_team() {
        let mut mgr = MemoryLayerManager::new();
        let memory = make_memory("useful pattern", MemoryLayer::Semantic);

        let promoted = mgr.promote_to_team(memory, "backend");
        assert_eq!(promoted.layer, MemoryLayer::Team);
        assert_eq!(mgr.team_memories("backend").len(), 1);
    }

    #[test]
    fn promote_to_org() {
        let mut mgr = MemoryLayerManager::new();
        let memory = make_memory("company policy", MemoryLayer::Team);

        let promoted = mgr.promote_to_org(memory, "contoso");
        assert_eq!(promoted.layer, MemoryLayer::Organisation);
        assert_eq!(mgr.org_memories("contoso").len(), 1);
    }

    #[test]
    fn stats() {
        let mut mgr = MemoryLayerManager::new();
        mgr.add_team_memory("t1", make_memory("a", MemoryLayer::Team));
        mgr.add_team_memory("t1", make_memory("b", MemoryLayer::Team));
        mgr.add_org_memory("o1", make_memory("c", MemoryLayer::Organisation));

        let stats = mgr.stats();
        assert_eq!(stats.teams, 1);
        assert_eq!(stats.orgs, 1);
        assert_eq!(stats.team_memories, 2);
        assert_eq!(stats.org_memories, 1);
    }
}

use serde::{Deserialize, Serialize};

use crate::types::{AgentId, NodeId, SkillId};

/// The kind of participant in the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    /// A human user interacting via CLI, UI, or messaging platform.
    Human,
    /// An AI agent with a defined role and capabilities.
    Agent,
    /// A system service (orchestrator, router, etc.).
    System,
}

/// A participant in the workforce system.
///
/// Participants are the fundamental actors — they send messages, execute tasks,
/// and interact through the NATS message bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    /// Unique identifier (agent_id for agents, user_id for humans).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// What kind of participant this is.
    pub kind: ParticipantKind,
    /// The node this participant is running on (if applicable).
    pub node_id: Option<NodeId>,
}

/// Configuration for an AI agent participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Unique agent identifier.
    pub id: AgentId,
    /// Human-readable name.
    pub name: String,
    /// Role within a team (e.g., "backend-developer", "reviewer").
    pub role: String,
    /// Default LLM model to use.
    pub model: String,
    /// Path to the system prompt file.
    pub system_prompt_file: Option<String>,
    /// Skills assigned to this agent.
    pub skills: Vec<SkillId>,
    /// Maximum tokens per LLM response.
    pub max_tokens: Option<u32>,
    /// LLM temperature (0.0 - 1.0).
    pub temperature: Option<f32>,
    /// Node affinity preference.
    pub node_affinity: Option<NodeAffinity>,
}

/// Where an agent prefers to run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeAffinity {
    /// Prefer running on a local node (for interactive pairing).
    Local,
    /// Prefer running on a remote node (for autonomous work).
    Remote,
    /// Run on a specific named node.
    Node(NodeId),
    /// No preference — orchestrator decides.
    #[default]
    Any,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_serialises_to_yaml() {
        let config = AgentConfig {
            id: AgentId::new("developer"),
            name: "Dev Agent".to_string(),
            role: "backend-developer".to_string(),
            model: "claude-sonnet-4".to_string(),
            system_prompt_file: Some("prompts/developer.md".to_string()),
            skills: vec![
                SkillId::new("rust-development"),
                SkillId::new("git-operations"),
            ],
            max_tokens: Some(8192),
            temperature: Some(0.7),
            node_affinity: Some(NodeAffinity::Remote),
        };

        let yaml = serde_json::to_string_pretty(&config).expect("serialisation failed");
        assert!(yaml.contains("developer"));
        assert!(yaml.contains("claude-sonnet-4"));
    }

    #[test]
    fn participant_kind_defaults_correctly() {
        let participant = Participant {
            id: "allan".to_string(),
            name: "Allan".to_string(),
            kind: ParticipantKind::Agent,
            node_id: None,
        };
        assert_eq!(participant.kind, ParticipantKind::Agent);
    }
}

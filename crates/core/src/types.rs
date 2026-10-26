use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Strongly-typed identifier for agents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

/// Strongly-typed identifier for teams.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamId(pub String);

/// Strongly-typed identifier for nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Strongly-typed identifier for tasks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Uuid);

/// Strongly-typed identifier for messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub Uuid);

/// Strongly-typed identifier for skills.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SkillId(pub String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TeamId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl NodeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for TeamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a round's streamed text is dropped from the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamDiscardReason {
    /// The round's text was a tool call hallucinated as text: the runner is
    /// retrying and the streamed text is garbage nobody should keep seeing.
    Retry,
    /// The round's text was a real draft answer superseded by a further round
    /// (a follow-up was absorbed): dropped from the result, but legitimate
    /// content worth keeping visible.
    Superseded,
    /// The provider failed part-way through the call and it is being
    /// regenerated: everything the call previewed is void.
    Interrupted,
}

impl StreamDiscardReason {
    /// The word the wire carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            StreamDiscardReason::Retry => "retry",
            StreamDiscardReason::Superseded => "superseded",
            StreamDiscardReason::Interrupted => "interrupted",
        }
    }

    /// Whether the dropped text must come off screen; a superseded draft is
    /// a real answer the reader keeps.
    pub const fn retracts(self) -> bool {
        !matches!(self, StreamDiscardReason::Superseded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_discard_reason_names_match_serde() {
        for reason in [
            StreamDiscardReason::Retry,
            StreamDiscardReason::Superseded,
            StreamDiscardReason::Interrupted,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::from(reason.as_str())
            );
        }
    }

    #[test]
    fn only_a_superseded_draft_stays_on_screen() {
        assert!(StreamDiscardReason::Retry.retracts());
        assert!(StreamDiscardReason::Interrupted.retracts());
        assert!(!StreamDiscardReason::Superseded.retracts());
    }
}

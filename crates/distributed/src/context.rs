//! Context transfer — serialise and move agent working state between nodes.
//!
//! When work moves between nodes (e.g., started locally, handed off to remote),
//! context is serialised and published.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use workforce_core::types::AgentId;

/// A snapshot of agent context for node-to-node transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// Snapshot ID.
    pub id: String,
    /// Agent this context belongs to.
    pub agent_id: AgentId,
    /// Task being worked on.
    pub task_id: Option<String>,
    /// Conversation history (serialised messages).
    pub messages: Vec<TransferMessage>,
    /// Working memory entries.
    pub working_memory: Vec<String>,
    /// Files modified during the session.
    pub modified_files: Vec<FileState>,
    /// Source node.
    pub from_node: String,
    /// Target node.
    pub to_node: String,
    /// When the snapshot was created.
    pub created_at: DateTime<Utc>,
    /// Arbitrary metadata.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A message in the transfer context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferMessage {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// State of a file for context transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    /// Git commit hash or content hash.
    pub hash: Option<String>,
    /// Whether the file has uncommitted changes.
    pub dirty: bool,
}

/// Build a context snapshot for transfer.
pub struct ContextBuilder {
    agent_id: AgentId,
    task_id: Option<String>,
    messages: Vec<TransferMessage>,
    working_memory: Vec<String>,
    modified_files: Vec<FileState>,
    from_node: String,
    to_node: String,
}

impl ContextBuilder {
    pub fn new(agent_id: AgentId, from_node: &str, to_node: &str) -> Self {
        Self {
            agent_id,
            task_id: None,
            messages: Vec::new(),
            working_memory: Vec::new(),
            modified_files: Vec::new(),
            from_node: from_node.to_string(),
            to_node: to_node.to_string(),
        }
    }

    pub fn task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.to_string());
        self
    }

    pub fn message(mut self, role: &str, content: &str) -> Self {
        self.messages.push(TransferMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
        });
        self
    }

    pub fn messages(mut self, msgs: Vec<TransferMessage>) -> Self {
        self.messages = msgs;
        self
    }

    pub fn memory(mut self, entry: &str) -> Self {
        self.working_memory.push(entry.to_string());
        self
    }

    pub fn file(mut self, path: &str, hash: Option<&str>, dirty: bool) -> Self {
        self.modified_files.push(FileState {
            path: path.to_string(),
            hash: hash.map(String::from),
            dirty,
        });
        self
    }

    pub fn build(self) -> ContextSnapshot {
        ContextSnapshot {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: self.agent_id,
            task_id: self.task_id,
            messages: self.messages,
            working_memory: self.working_memory,
            modified_files: self.modified_files,
            from_node: self.from_node,
            to_node: self.to_node,
            created_at: Utc::now(),
            metadata: serde_json::Value::Null,
        }
    }
}

/// Validate a context snapshot before applying it.
pub fn validate_snapshot(snapshot: &ContextSnapshot) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    if snapshot.agent_id.to_string().is_empty() {
        errors.push("Agent ID is empty".to_string());
    }
    if snapshot.from_node.is_empty() {
        errors.push("Source node is empty".to_string());
    }
    if snapshot.to_node.is_empty() {
        errors.push("Target node is empty".to_string());
    }
    if snapshot.from_node == snapshot.to_node {
        errors.push("Source and target nodes are the same".to_string());
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Estimate the serialised size of a context snapshot.
pub fn estimate_size(snapshot: &ContextSnapshot) -> usize {
    serde_json::to_string(snapshot)
        .map(|s| s.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_context() {
        let ctx = ContextBuilder::new(AgentId::new("dev"), "laptop", "hub")
            .task("OPS-125")
            .message("user", "implement the handler")
            .message("assistant", "I'll start with the route definition")
            .memory("Using axum 0.8 with tower-http")
            .file("src/routes.rs", Some("abc123"), true)
            .build();

        assert_eq!(ctx.agent_id, AgentId::new("dev"));
        assert_eq!(ctx.task_id, Some("OPS-125".to_string()));
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.working_memory.len(), 1);
        assert_eq!(ctx.modified_files.len(), 1);
        assert!(ctx.modified_files[0].dirty);
    }

    #[test]
    fn validate_good_snapshot() {
        let ctx = ContextBuilder::new(AgentId::new("dev"), "laptop", "hub").build();
        assert!(validate_snapshot(&ctx).is_ok());
    }

    #[test]
    fn validate_same_node_fails() {
        let ctx = ContextBuilder::new(AgentId::new("dev"), "laptop", "laptop").build();
        assert!(validate_snapshot(&ctx).is_err());
    }

    #[test]
    fn serialization_roundtrip() {
        let ctx = ContextBuilder::new(AgentId::new("dev"), "laptop", "hub")
            .task("task-1")
            .message("user", "hello")
            .build();

        let json = serde_json::to_string(&ctx).unwrap();
        let parsed: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, AgentId::new("dev"));
        assert_eq!(parsed.messages.len(), 1);
    }

    #[test]
    fn estimate_size_works() {
        let ctx = ContextBuilder::new(AgentId::new("dev"), "a", "b").build();
        let size = estimate_size(&ctx);
        assert!(size > 0);
    }
}

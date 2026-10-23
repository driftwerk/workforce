//! In-memory agent inbox for inter-agent messaging.
//!
//! Each agent has a bounded queue of pending [`AgentMessage`]s. Messages are
//! pushed by senders and drained by receivers on their next turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// A message sent from one agent to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from_agent: String,
    pub to_agent: String,
    pub content: String,
    pub message_type: String,
    pub timestamp: DateTime<Utc>,
    pub correlation_id: Option<String>,
}

/// In-memory inbox for inter-agent messages.
pub struct AgentInbox {
    inboxes: Arc<RwLock<HashMap<String, VecDeque<AgentMessage>>>>,
    max_per_agent: usize,
}

impl AgentInbox {
    pub fn new() -> Self {
        Self {
            inboxes: Arc::new(RwLock::new(HashMap::new())),
            max_per_agent: 100,
        }
    }

    /// Push a message into an agent's inbox. Returns false if inbox is full.
    pub async fn push(&self, msg: AgentMessage) -> bool {
        let mut inboxes = self.inboxes.write().await;
        let queue = inboxes.entry(msg.to_agent.clone()).or_default();
        if queue.len() >= self.max_per_agent {
            return false;
        }
        queue.push_back(msg);
        true
    }

    /// Drain all pending messages for an agent.
    pub async fn drain(&self, agent_id: &str) -> Vec<AgentMessage> {
        let mut inboxes = self.inboxes.write().await;
        inboxes
            .get_mut(agent_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Peek at pending messages without removing them.
    pub async fn peek(&self, agent_id: &str, limit: usize) -> Vec<AgentMessage> {
        let inboxes = self.inboxes.read().await;
        inboxes
            .get(agent_id)
            .map(|q| q.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Count pending messages for an agent.
    pub async fn count(&self, agent_id: &str) -> usize {
        let inboxes = self.inboxes.read().await;
        inboxes.get(agent_id).map(|q| q.len()).unwrap_or(0)
    }
}

impl Default for AgentInbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(from: &str, to: &str, content: &str) -> AgentMessage {
        AgentMessage {
            from_agent: from.to_string(),
            to_agent: to.to_string(),
            content: content.to_string(),
            message_type: "info".to_string(),
            timestamp: Utc::now(),
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn push_and_drain_round_trip() {
        let inbox = AgentInbox::new();
        let msg = make_message("alice", "bob", "hello");
        assert!(inbox.push(msg).await);

        let messages = inbox.drain("bob").await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from_agent, "alice");
        assert_eq!(messages[0].content, "hello");
    }

    #[tokio::test]
    async fn peek_does_not_remove_messages() {
        let inbox = AgentInbox::new();
        inbox.push(make_message("alice", "bob", "msg1")).await;
        inbox.push(make_message("alice", "bob", "msg2")).await;

        let peeked = inbox.peek("bob", 10).await;
        assert_eq!(peeked.len(), 2);

        // Messages should still be there after peek
        let count = inbox.count("bob").await;
        assert_eq!(count, 2);

        // Drain should still return them
        let drained = inbox.drain("bob").await;
        assert_eq!(drained.len(), 2);
    }

    #[tokio::test]
    async fn capacity_limit_rejects_when_full() {
        let inbox = AgentInbox {
            inboxes: Arc::new(RwLock::new(HashMap::new())),
            max_per_agent: 3,
        };

        assert!(inbox.push(make_message("a", "b", "1")).await);
        assert!(inbox.push(make_message("a", "b", "2")).await);
        assert!(inbox.push(make_message("a", "b", "3")).await);
        // Fourth should be rejected
        assert!(!inbox.push(make_message("a", "b", "4")).await);

        assert_eq!(inbox.count("b").await, 3);
    }

    #[tokio::test]
    async fn count_accuracy() {
        let inbox = AgentInbox::new();
        assert_eq!(inbox.count("agent-x").await, 0);

        inbox.push(make_message("a", "agent-x", "one")).await;
        assert_eq!(inbox.count("agent-x").await, 1);

        inbox.push(make_message("b", "agent-x", "two")).await;
        assert_eq!(inbox.count("agent-x").await, 2);

        inbox.drain("agent-x").await;
        assert_eq!(inbox.count("agent-x").await, 0);
    }

    #[tokio::test]
    async fn drain_returns_empty_for_unknown_agent() {
        let inbox = AgentInbox::new();
        let messages = inbox.drain("nonexistent").await;
        assert!(messages.is_empty());
    }
}

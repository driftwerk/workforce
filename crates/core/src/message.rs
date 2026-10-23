use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::MessageId;

/// An emoji reaction to a message.
///
/// Represents both inbound reactions (user reacted to a bot message) and
/// outbound reactions (agent reacting to a user message). Platform-specific
/// translation (e.g. canonical name → Slack shortcode) is handled by the
/// `workforce_common::emoji` module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    /// Canonical emoji name (e.g. "thumbs_up", "heart", "fire").
    pub emoji: String,
    /// Platform-qualified channel (e.g. "slack:C0123ABC").
    pub channel: String,
    /// Message identifier to react to (e.g. Slack timestamp "1234567890.123456").
    pub message_id: String,
}

impl Reaction {
    /// Create a new reaction.
    pub fn new(
        emoji: impl Into<String>,
        channel: impl Into<String>,
        message_id: impl Into<String>,
    ) -> Self {
        Self {
            emoji: emoji.into(),
            channel: channel.into(),
            message_id: message_id.into(),
        }
    }
}

/// The content type of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageContent {
    /// Plain text message.
    Text(String),
    /// A code block with optional language hint.
    Code {
        language: Option<String>,
        content: String,
    },
    /// A tool invocation request.
    ToolRequest {
        tool_name: String,
        input: serde_json::Value,
    },
    /// The result of a tool invocation.
    ToolResult {
        tool_name: String,
        output: serde_json::Value,
        success: bool,
    },
    /// A status update (progress, completion, error).
    Status { kind: StatusKind, detail: String },
}

/// Kinds of status updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Progress,
    Complete,
    Error,
    Waiting,
}

/// Who the message is addressed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    /// Direct message to a specific participant.
    Direct(String),
    /// Broadcast to a named channel.
    Channel(String),
    /// Broadcast to all participants in a team.
    Team(String),
    /// System-level (policy engine, orchestrator, etc.).
    System,
}

/// A message flowing through the workforce system.
///
/// All communication between participants happens via messages published
/// to the NATS message bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique message identifier.
    pub id: MessageId,
    /// Who sent this message.
    pub sender: String,
    /// Who this message is for.
    pub recipient: Recipient,
    /// The message content.
    pub content: MessageContent,
    /// Optional thread ID for threaded conversations.
    pub thread_id: Option<MessageId>,
    /// When the message was created.
    pub timestamp: DateTime<Utc>,
    /// Arbitrary metadata.
    pub metadata: serde_json::Value,
}

impl Message {
    /// Create a new text message.
    pub fn text(sender: impl Into<String>, recipient: Recipient, text: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            sender: sender.into(),
            recipient,
            content: MessageContent::Text(text.into()),
            thread_id: None,
            timestamp: Utc::now(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a new tool request message.
    pub fn tool_request(
        sender: impl Into<String>,
        tool_name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id: MessageId::new(),
            sender: sender.into(),
            recipient: Recipient::System,
            content: MessageContent::ToolRequest {
                tool_name: tool_name.into(),
                input,
            },
            thread_id: None,
            timestamp: Utc::now(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a tool result message.
    pub fn tool_result(
        tool_name: impl Into<String>,
        output: serde_json::Value,
        success: bool,
    ) -> Self {
        Self {
            id: MessageId::new(),
            sender: "system".to_string(),
            recipient: Recipient::System,
            content: MessageContent::ToolResult {
                tool_name: tool_name.into(),
                output,
                success,
            },
            thread_id: None,
            timestamp: Utc::now(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Set the thread ID for this message.
    pub fn with_thread(mut self, thread_id: MessageId) -> Self {
        self.thread_id = Some(thread_id);
        self
    }

    /// Set metadata on this message.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_text_message() {
        let msg = Message::text(
            "agent-1",
            Recipient::Direct("agent-2".to_string()),
            "Hello!",
        );
        assert_eq!(msg.sender, "agent-1");
        assert!(matches!(msg.recipient, Recipient::Direct(ref r) if r == "agent-2"));
        assert!(matches!(msg.content, MessageContent::Text(ref t) if t == "Hello!"));
    }

    #[test]
    fn create_tool_request_message() {
        let input = serde_json::json!({"path": "/tmp/test.txt"});
        let msg = Message::tool_request("agent-1", "filesystem.read", input.clone());
        assert!(matches!(
            msg.content,
            MessageContent::ToolRequest { ref tool_name, input: _ }
            if tool_name == "filesystem.read"
        ));
    }

    #[test]
    fn message_serialises_roundtrip() {
        let msg = Message::text("agent-1", Recipient::Channel("general".to_string()), "Test");
        let json = serde_json::to_string(&msg).expect("serialise failed");
        let parsed: Message = serde_json::from_str(&json).expect("deserialise failed");
        assert_eq!(parsed.sender, msg.sender);
        assert_eq!(parsed.id, msg.id);
    }

    #[test]
    fn create_reaction() {
        let reaction = Reaction::new("thumbs_up", "slack:C0123ABC", "1234567890.123456");
        assert_eq!(reaction.emoji, "thumbs_up");
        assert_eq!(reaction.channel, "slack:C0123ABC");
        assert_eq!(reaction.message_id, "1234567890.123456");
    }

    #[test]
    fn reaction_serialises_roundtrip() {
        let reaction = Reaction::new("heart", "ws:default", "msg-42");
        let json = serde_json::to_string(&reaction).expect("serialise failed");
        let parsed: Reaction = serde_json::from_str(&json).expect("deserialise failed");
        assert_eq!(parsed, reaction);
    }

    #[test]
    fn message_threading() {
        let parent = Message::text("agent-1", Recipient::Direct("agent-2".to_string()), "Q?");
        let reply = Message::text("agent-2", Recipient::Direct("agent-1".to_string()), "A!")
            .with_thread(parent.id.clone());
        assert_eq!(reply.thread_id, Some(parent.id));
    }
}

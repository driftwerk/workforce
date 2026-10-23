use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::Envelope;

/// A message in a multi-agent conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    /// Which conversation thread this belongs to.
    pub thread_id: Uuid,
    /// The content of the message.
    pub content: MessageContent,
    /// Who the message is addressed to (None = everyone in channel).
    pub to: Option<String>,
    /// Whether this message mentions specific agents.
    #[serde(default)]
    pub mentions: Vec<String>,
}

/// Content types that can be sent in conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    /// Plain text message.
    Text { text: String },
    /// Code snippet with optional language.
    Code {
        code: String,
        language: Option<String>,
    },
    /// A task assignment or delegation.
    TaskAssignment {
        task_id: String,
        title: String,
        description: String,
        #[serde(default)]
        task_type: Option<String>,
    },
    /// A task result/update.
    TaskUpdate {
        task_id: String,
        status: String,
        summary: String,
    },
    /// A request for help/escalation.
    Escalation {
        reason: String,
        context: String,
        severity: EscalationSeverity,
    },
    /// A status update.
    StatusUpdate { status: AgentStatus, detail: String },
}

/// Escalation severity levels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Agent status for heartbeat/presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    WaitingForInput,
    Error,
    Offline,
}

/// A conversation thread with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    /// Unique thread ID.
    pub id: Uuid,
    /// Parent thread (for sub-conversations).
    pub parent_id: Option<Uuid>,
    /// Channel this conversation belongs to.
    pub channel: String,
    /// Team ID.
    pub team_id: String,
    /// Topic/title of the conversation.
    pub topic: String,
    /// Participants who have sent messages.
    #[serde(default)]
    pub participants: Vec<String>,
    /// When the conversation was started.
    pub started_at: DateTime<Utc>,
    /// When the last message was sent.
    pub last_message_at: DateTime<Utc>,
    /// Whether the conversation is still active.
    pub active: bool,
}

impl Conversation {
    /// Create a new conversation.
    pub fn new(
        team_id: impl Into<String>,
        channel: impl Into<String>,
        topic: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            channel: channel.into(),
            team_id: team_id.into(),
            topic: topic.into(),
            participants: Vec::new(),
            started_at: now,
            last_message_at: now,
            active: true,
        }
    }

    /// Create a sub-thread branching from this conversation.
    pub fn branch(&self, topic: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id: Some(self.id),
            channel: self.channel.clone(),
            team_id: self.team_id.clone(),
            topic: topic.into(),
            participants: Vec::new(),
            started_at: now,
            last_message_at: now,
            active: true,
        }
    }

    /// Record that an agent participated.
    pub fn add_participant(&mut self, agent_id: impl Into<String>) {
        let id = agent_id.into();
        if !self.participants.contains(&id) {
            self.participants.push(id);
        }
        self.last_message_at = Utc::now();
    }

    /// Create a message envelope for this conversation.
    pub fn create_message(
        &mut self,
        sender: &str,
        content: MessageContent,
    ) -> Envelope<ConversationMessage> {
        self.add_participant(sender);

        let subject = crate::subjects::team_channel(&self.team_id, &self.channel);

        Envelope::new(
            subject,
            sender,
            ConversationMessage {
                thread_id: self.id,
                content,
                to: None,
                mentions: Vec::new(),
            },
        )
    }

    /// Create a direct message within this conversation.
    pub fn create_direct_message(
        &mut self,
        sender: &str,
        to: &str,
        content: MessageContent,
    ) -> Envelope<ConversationMessage> {
        self.add_participant(sender);

        let subject = crate::subjects::agent_inbox(to);

        Envelope::new(
            subject,
            sender,
            ConversationMessage {
                thread_id: self.id,
                content,
                to: Some(to.to_string()),
                mentions: Vec::new(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_conversation() {
        let conv = Conversation::new("backend-team", "general", "Sprint planning");
        assert!(conv.active);
        assert!(conv.participants.is_empty());
        assert_eq!(conv.channel, "general");
        assert_eq!(conv.team_id, "backend-team");
    }

    #[test]
    fn branch_conversation() {
        let conv = Conversation::new("team", "dev", "Main discussion");
        let branch = conv.branch("Side topic about testing");
        assert_eq!(branch.parent_id, Some(conv.id));
        assert_eq!(branch.team_id, "team");
        assert_eq!(branch.channel, "dev");
    }

    #[test]
    fn create_channel_message() {
        let mut conv = Conversation::new("team", "dev", "Code review");
        let env = conv.create_message(
            "developer",
            MessageContent::Text {
                text: "I've finished the API routes".to_string(),
            },
        );

        assert_eq!(env.sender, "developer");
        assert_eq!(env.subject, "workforce.teams.team.messages.dev");
        assert_eq!(env.payload.thread_id, conv.id);
        assert!(conv.participants.contains(&"developer".to_string()));
    }

    #[test]
    fn create_direct_message() {
        let mut conv = Conversation::new("team", "dev", "Review request");
        let env = conv.create_direct_message(
            "developer",
            "reviewer",
            MessageContent::TaskAssignment {
                task_id: "task-1".to_string(),
                title: "Review PR".to_string(),
                description: "Please review the login PR".to_string(),
                task_type: None,
            },
        );

        assert_eq!(env.sender, "developer");
        assert_eq!(env.subject, "workforce.agents.reviewer.inbox");
        assert_eq!(env.payload.to, Some("reviewer".to_string()));
    }

    #[test]
    fn participant_tracking() {
        let mut conv = Conversation::new("team", "dev", "Discussion");
        conv.add_participant("alice");
        conv.add_participant("bob");
        conv.add_participant("alice"); // duplicate — should not add again

        assert_eq!(conv.participants.len(), 2);
    }

    #[test]
    fn message_content_serialization() {
        let content = MessageContent::Escalation {
            reason: "Agent stuck on complex merge conflict".to_string(),
            context: "Working on PR #42, two conflicting changes in auth module".to_string(),
            severity: EscalationSeverity::Medium,
        };

        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("escalation"));

        let deserialized: MessageContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, MessageContent::Escalation { .. }));
    }

    #[test]
    fn task_update_flow() {
        let mut conv = Conversation::new("team", "tasks", "Feature work");

        // Developer sends task update
        let env = conv.create_message(
            "developer",
            MessageContent::TaskUpdate {
                task_id: "task-1".to_string(),
                status: "in_progress".to_string(),
                summary: "API routes done, starting tests".to_string(),
            },
        );

        assert!(matches!(
            env.payload.content,
            MessageContent::TaskUpdate { .. }
        ));
    }
}

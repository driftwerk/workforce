//! NATS message types for interactive chat routing.
//!
//! These types flow between the gateway (WebSocket bridge) and agent chat workers
//! via NATS subjects defined in [`super::subjects`].

use serde::{Deserialize, Serialize};

/// A chat request published to `workforce.agents.{agent_id}.chat.{session_id}`.
///
/// The gateway publishes one of these when a WebSocket client sends a message.
/// An agent chat worker picks it up and calls `process_message_with_progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsChatRequest {
    /// Session ID (existing or newly created).
    pub session_id: String,
    /// Channel identifier (e.g., "terminal", "web").
    pub channel: String,
    /// User identifier.
    pub user: String,
    /// Message content (may include sentinel wrapper tags for LLM security).
    pub content: String,
    /// Original unwrapped content for display/storage.
    /// When absent, `content` is already clean and should be used directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    /// Base64-encoded file/image attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<NatsChatAttachment>,
    /// Voice-bridge channel this turn was spoken on, when it came from a
    /// satellite. Carried per request because the worker executing the turn
    /// is a different process from the gateway holding the bridge's socket,
    /// and the session record cannot answer "was THIS turn spoken" — one
    /// session is shared by every surface a person uses with an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_channel: Option<String>,
    /// Whether the client can answer a self-serve approval prompt. A client
    /// that cannot gets such calls refused at once rather than left waiting.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answers_approvals: bool,
    /// The Desk turn this is, when it came from Desk: an instance that
    /// predates it runs the turn as before, without its context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<workforce_core::desk::DeskTurnContext>,
}

/// A control message for a live turn, sent as a request on
/// [`crate::subjects::turn_control`] to whichever instance runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnControl {
    /// Stop the turn. Acknowledged once the turn has settled and its claim is
    /// released.
    Cancel,
    /// A follow-up message to absorb into the running turn. A follow-up
    /// from Desk names its place, and is refused as busy by a turn asked in
    /// another one.
    FollowUp {
        text: String,
        user: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        place: Option<workforce_core::desk::DeskPlace>,
    },
    /// An answer to one of the turn's approval prompts.
    Approval {
        request_id: String,
        approved: bool,
        user: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<workforce_core::policy::PermissionChoice>,
    },
    /// Put a snapshot of the live Desk turn into its own progress stream,
    /// for a Desk that connected mid-turn.
    Snapshot,
}

/// The owning instance's answer to a [`TurnControl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnControlReply {
    pub applied: bool,
    /// Why a control the turn is live for was not applied: `busy` for a
    /// follow-up from another place than the turn's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// [`TurnControlReply::refused`] for a follow-up the live turn will not take.
pub const REFUSED_BUSY: &str = "busy";

/// An attachment within a chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsChatAttachment {
    pub filename: String,
    pub mime_type: String,
    /// Inline payload; empty when `artifact_id` is set.
    #[serde(default)]
    pub data: String,
    /// Artifact-store id when the payload was uploaded out-of-band. Keeps
    /// large files out of the NATS message (whose server-side payload cap is
    /// far below any video).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}

/// A progress event published by an agent worker to
/// `workforce.agents.{agent_id}.progress.{session_id}`.
///
/// The gateway subscribes to this subject and forwards events to the WebSocket client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatProgress {
    /// Agent is processing (typing indicator).
    Typing,
    /// Start of a streamed response.
    StreamStart,
    /// A chunk of streamed text.
    StreamDelta { text: String },
    /// End of a streamed response.
    StreamEnd,
    /// The streamed text of the current model call is void; a replacement
    /// stream may follow.
    StreamDiscard {
        reason: workforce_core::types::StreamDiscardReason,
    },
    /// Complete (non-streamed) response.
    Message { text: String },
    /// Tool execution started.
    ToolStart { name: String, detail: String },
    /// Tool execution finished.
    ToolEnd {
        name: String,
        success: bool,
        duration_ms: u64,
        detail: String,
    },
    /// Session compaction started.
    CompactStart,
    /// Session compaction finished.
    CompactEnd,
    /// Context window usage update.
    ContextUpdate {
        estimated_tokens: u32,
        context_max: u32,
    },
    /// Deterministic code citations for the turn — the files/line-ranges the
    /// answer-ready `code_context` injector grounded the answer in. Published
    /// once, AFTER the answer (stream_end / message), so the client attaches the
    /// clickable source chips to the just-completed answer. Absent when the turn
    /// grounded no code.
    CodeCitations {
        citations: Vec<workforce_core::code_citation::CodeCitation>,
    },
    /// The turn found no login to run on: asks the person to connect one.
    /// Never carries a sign-in code.
    ConnectPrompt {
        prompt: workforce_core::subscription::ConnectPrompt,
    },
    /// What ran the turn ("Claude Code · sonnet · Alice's plan"), published
    /// once after the answer when one is shown.
    Route { line: String },
    /// Agent processing complete.
    Done { session_id: String },
    /// A tool call requires human approval.
    ApprovalRequired {
        request_id: String,
        agent_id: String,
        tool_name: String,
        detail: String,
        reason: String,
        /// Typed prompt payload — surfaces that render the four-button
        /// permission prompt read this. Older surfaces ignore it and
        /// fall back to the free-text `approve <id>` flow.
        #[serde(default = "default_prompt_options")]
        options: workforce_core::policy::PermissionPromptOptions,
    },
    /// An error occurred.
    Error { message: String },
    /// The model call was rate-limited; the turn retries after a wait. Sent
    /// for Desk turns only, as every variant below is.
    RateLimited { wait_secs: u64, attempt: u32 },
    /// The model was silent too long and the turn switches to the next one.
    SlowModelFailover { model: String, elapsed_secs: u64 },
    /// The live Desk turn as it stands.
    Snapshot(workforce_core::desk::TurnSnapshot),
    /// The Desk turn ended; its closing events have all gone out.
    TurnEnd(workforce_core::desk::TurnEnd),
}

fn default_prompt_options() -> workforce_core::policy::PermissionPromptOptions {
    workforce_core::policy::PermissionPromptOptions::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::policy::PermissionChoice;

    #[test]
    fn turn_control_round_trips() {
        [
            TurnControl::Cancel,
            TurnControl::FollowUp {
                text: "also check main".into(),
                user: "u1".into(),
                place: None,
            },
            TurnControl::FollowUp {
                text: "and the thread".into(),
                user: "u1".into(),
                place: Some(workforce_core::desk::DeskPlace::Dm),
            },
            TurnControl::Approval {
                request_id: "r1".into(),
                approved: true,
                user: "u1".into(),
                choice: Some(PermissionChoice::AllowAlways),
            },
            TurnControl::Snapshot,
        ]
        .into_iter()
        .for_each(|control| {
            let bytes = serde_json::to_vec(&control).unwrap();
            assert_eq!(
                serde_json::from_slice::<TurnControl>(&bytes).unwrap(),
                control
            );
        });
        assert_eq!(
            serde_json::to_value(TurnControl::Cancel).unwrap(),
            serde_json::json!({"kind": "cancel"})
        );
    }

    #[test]
    fn stream_discard_round_trips() {
        let wire = serde_json::to_value(ChatProgress::StreamDiscard {
            reason: workforce_core::types::StreamDiscardReason::Interrupted,
        })
        .unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"type": "stream_discard", "reason": "interrupted"})
        );
        assert!(matches!(
            serde_json::from_value::<ChatProgress>(wire).unwrap(),
            ChatProgress::StreamDiscard {
                reason: workforce_core::types::StreamDiscardReason::Interrupted
            }
        ));
    }

    #[test]
    fn a_desk_turn_s_snapshot_and_end_read_flat() {
        let stamp = workforce_core::desk::TurnStamp {
            turn_ref: Some("ref-1".into()),
            reply_id: "reply-1".into(),
        };
        let snapshot = ChatProgress::Snapshot(workforce_core::desk::TurnSnapshot {
            stamp,
            since: "2026-10-29T09:00:00Z".parse().unwrap(),
            status: workforce_core::desk::TurnPhase::Writing,
            text: "Hel".into(),
        });
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "type": "snapshot", "turn_ref": "ref-1", "reply_id": "reply-1",
                "since": "2026-10-29T09:00:00Z", "status": "writing", "text": "Hel",
            })
        );
        assert!(matches!(
            serde_json::from_value::<ChatProgress>(wire).unwrap(),
            ChatProgress::Snapshot(_)
        ));
        let end = serde_json::to_value(ChatProgress::TurnEnd(workforce_core::desk::TurnEnd {
            turn_ref: Some("ref-1".into()),
            reply_id: None,
        }))
        .unwrap();
        assert_eq!(
            end,
            serde_json::json!({"type": "turn_end", "turn_ref": "ref-1"})
        );
    }

    #[test]
    fn chat_request_without_answers_approvals_defaults_to_unanswerable() {
        let request: NatsChatRequest = serde_json::from_value(serde_json::json!({
            "session_id": "s", "channel": "ios", "user": "u", "content": "hi"
        }))
        .unwrap();
        assert!(!request.answers_approvals);
        assert!(request.desk.is_none());
    }

    #[test]
    fn an_old_instance_s_follow_up_and_reply_still_read() {
        let follow_up: TurnControl = serde_json::from_value(serde_json::json!({
            "kind": "follow_up", "text": "hi", "user": "u1"
        }))
        .unwrap();
        assert_eq!(
            follow_up,
            TurnControl::FollowUp {
                text: "hi".into(),
                user: "u1".into(),
                place: None
            }
        );
        let reply: TurnControlReply =
            serde_json::from_value(serde_json::json!({"applied": true})).unwrap();
        assert_eq!(
            reply,
            TurnControlReply {
                applied: true,
                refused: None
            }
        );
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::json!({"applied": true})
        );
    }
}

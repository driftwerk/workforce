//! Gateway protocol message types.
//!
//! Shared between the gateway server and clients.

use serde::{Deserialize, Serialize};

/// A file or image attachment sent with a chat message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Original filename (e.g. "screenshot.png").
    pub filename: String,
    /// MIME type (e.g. "image/png", "text/plain").
    pub mime_type: String,
    /// File data — base64-encoded for images, raw UTF-8 for text files.
    /// Empty when `artifact_id` carries the payload by reference.
    #[serde(default)]
    pub data: String,
    /// Artifact-store id of a payload carried by reference: a large upload
    /// (via `POST /api/sessions/{id}/upload`), or a non-image file a tool
    /// delivered, which the web UI fetches from the session file route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    /// Bytes, on a file a tool delivered to a client that declared what it
    /// accepts (see [`MediaAccept`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Pixels, for an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Seconds, for audio and video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

/// The media types a `/ws` client shows, as it declares them at connect
/// with `accept` (`/ws?accept=image/*,application/pdf,video/*`, or `*/*`
/// for everything). A client that declares nothing is shown images only,
/// each inline as it always was; a client that declares types gets the
/// files of those types, each with its `artifact_id` (to download from
/// `GET /api/agents/{agent}/attachments/{artifact_id}`) and what is known
/// of its size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaAccept {
    /// Lower-cased `type/subtype` ranges; empty when nothing was declared.
    ranges: Vec<String>,
}

impl MediaAccept {
    /// The ranges named in a comma-separated `accept` value. Anything that
    /// is not a `type/subtype` range is ignored, as is a `;q=` weight.
    pub fn from_names(names: Option<&str>) -> Self {
        let ranges = names
            .unwrap_or_default()
            .split(',')
            .filter_map(|r| r.split(';').next())
            .map(|r| r.trim().to_ascii_lowercase())
            .filter(|r| {
                r.split_once('/')
                    .is_some_and(|(t, s)| !t.is_empty() && !s.is_empty() && !s.contains('/'))
            })
            .collect();
        Self { ranges }
    }

    /// Whether the client said what it shows.
    pub fn declared(&self) -> bool {
        !self.ranges.is_empty()
    }

    /// Whether a file of `media_type` is shown: one of the declared ranges
    /// covers it, or, when none were declared, it is an image.
    pub fn admits(&self, media_type: &str) -> bool {
        let media_type = media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let Some((kind, _)) = media_type.split_once('/') else {
            return false;
        };
        if !self.declared() {
            return kind == "image";
        }
        self.ranges.iter().any(|range| match range.split_once('/') {
            Some(("*", "*")) => true,
            Some((t, "*")) => t == kind,
            _ => *range == media_type,
        })
    }

    /// `attachments` as this client is sent them: only those it shows, and
    /// for a client that declared nothing, each exactly as such clients
    /// have always had it (inline, no `artifact_id`, nothing more). `None`
    /// when none are left.
    pub fn shape(&self, attachments: Vec<Attachment>) -> Option<Vec<Attachment>> {
        let kept: Vec<Attachment> = attachments
            .into_iter()
            .filter(|a| self.admits(&a.mime_type))
            .map(|a| match self.declared() {
                true => a,
                false => Attachment {
                    filename: a.filename,
                    mime_type: a.mime_type,
                    data: a.data,
                    ..Attachment::default()
                },
            })
            .collect();
        (!kept.is_empty()).then_some(kept)
    }
}

/// Client → Server message format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    /// Message type: "chat", "ping", "status".
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Chat message content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Session ID (optional, auto-created if missing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Channel identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// File/image attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
    /// Target agent identifier (routes to specific agent; uses default if omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Target team identifier (routes to team instead of single agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Whether the client is in voice mode (TTS active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_mode: Option<bool>,
    /// Active TTS provider (e.g. "fish_audio", "elevenlabs", "system").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_provider: Option<String>,
    /// Approval request id this message is responding to (for
    /// `msg_type = "permission_response"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The user's typed choice at a permission prompt — pairs with
    /// `request_id`. Surfaces that don't render the typed prompt
    /// continue to send free-text via the existing chat path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<workforce_core::policy::PermissionChoice>,
    /// Set by a client that renders permission prompts, so its turns may raise
    /// self-serve approvals. Absent means such calls are refused at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers_approvals: Option<bool>,
    /// On a Desk connection (`/ws?…&desk=1`): where in Desk this message was
    /// asked, who asked, and the thread lines around it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<workforce_core::desk::DeskTurn>,
    /// The client's own id for the turn this message starts, at most
    /// [`TURN_REF_MAX`] characters, echoed on every frame of the turn on a
    /// Desk connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
}

/// The longest `turn_ref` a client may send.
pub const TURN_REF_MAX: usize = 64;

impl ClientMessage {
    /// Create a chat message.
    pub fn chat(content: &str) -> Self {
        Self {
            msg_type: "chat".to_string(),
            content: Some(content.to_string()),
            session_id: None,
            channel: Some("terminal".to_string()),
            attachments: None,
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: None,
            choice: None,
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Respond to a `permission_prompt` ServerMessage. Carries the
    /// typed user choice the runner reads to decide whether to
    /// persist a User-scope rule.
    pub fn permission_response(
        request_id: &str,
        choice: workforce_core::policy::PermissionChoice,
    ) -> Self {
        Self {
            msg_type: "permission_response".to_string(),
            content: None,
            session_id: None,
            channel: None,
            attachments: None,
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: Some(request_id.to_string()),
            choice: Some(choice),
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Create a chat message with a session ID.
    pub fn chat_with_session(content: &str, session_id: &str) -> Self {
        Self {
            msg_type: "chat".to_string(),
            content: Some(content.to_string()),
            session_id: Some(session_id.to_string()),
            channel: Some("terminal".to_string()),
            attachments: None,
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: None,
            choice: None,
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Create a chat message with attachments and an optional session ID.
    pub fn chat_with_attachments(
        content: &str,
        attachments: Vec<Attachment>,
        session_id: Option<&str>,
    ) -> Self {
        Self {
            msg_type: "chat".to_string(),
            content: Some(content.to_string()),
            session_id: session_id.map(String::from),
            channel: Some("terminal".to_string()),
            attachments: if attachments.is_empty() {
                None
            } else {
                Some(attachments)
            },
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: None,
            choice: None,
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Create a ping message.
    pub fn ping() -> Self {
        Self {
            msg_type: "ping".to_string(),
            content: None,
            session_id: None,
            channel: None,
            attachments: None,
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: None,
            choice: None,
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Create a cancel message to abort the current stream for a session.
    pub fn cancel(session_id: &str) -> Self {
        Self {
            msg_type: "cancel".to_string(),
            content: None,
            session_id: Some(session_id.to_string()),
            channel: None,
            attachments: None,
            agent_id: None,
            team_id: None,
            voice_mode: None,
            tts_provider: None,
            request_id: None,
            choice: None,
            answers_approvals: None,
            desk: None,
            turn_ref: None,
        }
    }

    /// Set the target agent for this message.
    pub fn with_agent(mut self, agent_id: &str) -> Self {
        self.agent_id = Some(agent_id.to_string());
        self
    }

    /// Set the target team for this message.
    pub fn with_team(mut self, team_id: &str) -> Self {
        self.team_id = Some(team_id.to_string());
        self
    }
}

/// Server → Client message format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessage {
    /// Message type.
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Message content.
    #[serde(default)]
    pub content: Option<String>,
    /// Session ID.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Error message (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Which agent sent this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Estimated context tokens used (sent with `context_update` messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// Maximum context window size (sent with `context_update` messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_max: Option<u32>,
    /// Which team this message belongs to (present for team conversations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Role of the responding agent within the team.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_role: Option<String>,
    /// Turn-taking indicator: which agent(s) should respond next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_speaker: Option<String>,
    /// Whether the client should read this message aloud in voice/audio mode.
    /// Tool events and markers are `false`; response text is `true`.
    /// Defaults to `true` and is omitted from JSON when `true` to save bandwidth.
    #[serde(default = "default_speakable", skip_serializing_if = "is_speakable")]
    pub speakable: bool,
    /// Which device-token slot authenticated the WS handshake — `"current"`
    /// or `"previous"`. Only set on the `connected` event for device-paired
    /// clients. `"previous"` means the presented token sits in the one-deep
    /// grace slot: some holder already rotated past it. Clients should skip
    /// their scheduled token rotation for the session — the server refuses
    /// (409) to advance the chain off a previous-slot match — and realign
    /// from persisted credentials on next launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_match: Option<String>,
    /// Approval request id for `msg_type = "permission_prompt"` — the
    /// client echoes this back in the [`ClientMessage::permission_response`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Tool the runner wants to invoke (e.g. `vault.delete`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Which of the four buttons to render. Carries the
    /// `ask` vs `always_ask` distinction by hiding Always-Allow when
    /// the parent state forbids it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_options: Option<workforce_core::policy::PermissionPromptOptions>,
    /// Outbound attachments — e.g. images generated by the `generate_image`
    /// tool. Carried inline (base64 in [`Attachment::data`]) for immediate
    /// client rendering. Present on `msg_type = "image"` and may also ride a
    /// normal `"message"`; clients render these whenever the array is
    /// non-empty, regardless of `msg_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
    /// Deterministic code citations — present on `msg_type = "code_citations"`
    /// (one per turn, delivered after the answer). The client renders these as
    /// clickable source chips under the just-completed answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_citations: Option<Vec<workforce_core::code_citation::CodeCitation>>,
    /// Present on `msg_type = "connect_prompt"`: the turn found no login to
    /// run on, and asks the person to connect one. Never carries a sign-in
    /// code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_prompt: Option<workforce_core::subscription::ConnectPrompt>,
    /// Present on `msg_type = "route"`: what ran the just-completed answer
    /// ("Claude Code · sonnet · Alice's plan"), shown beneath it. `content`
    /// stays empty, so a client that does not know the type shows nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// On a Desk overseer's `message` / `stream_end`: what the answer
    /// mentions to act on in place (`{"approvals": […], "suggestions": […]}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<serde_json::Value>,
    /// On a Desk DM's `user_message`: the message's id, as the DM's history
    /// gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// On a Desk DM's `user_message`: who said it,
    /// `{"kind": "you"|"overseer"|"agent", "id", "name"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<serde_json::Value>,
    /// On a Desk connection's `connected`: what the hub offers it
    /// (`desk.place`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
    /// On a Desk connection: the client's id for the turn a frame belongs
    /// to, as it sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
    /// On a `turn_snapshot`: what the live turn is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<workforce_core::desk::TurnPhase>,
    /// On a `turn_snapshot`: when the turn began, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// On a `turn_snapshot`: what the turn has streamed of its answer so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

fn default_speakable() -> bool {
    true
}

fn is_speakable(v: &bool) -> bool {
    *v
}

impl ServerMessage {
    /// Check if this is a text message with content.
    pub fn is_message(&self) -> bool {
        self.msg_type == "message"
    }

    /// Check if this is a streaming delta.
    pub fn is_stream_delta(&self) -> bool {
        self.msg_type == "stream_delta"
    }

    /// Check if this is the start of a stream.
    pub fn is_stream_start(&self) -> bool {
        self.msg_type == "stream_start"
    }

    /// Check if this is the end of a stream.
    pub fn is_stream_end(&self) -> bool {
        self.msg_type == "stream_end"
    }

    /// Check if this is a typing indicator.
    pub fn is_typing(&self) -> bool {
        self.msg_type == "typing"
    }

    /// Check if this is a connection acknowledgement.
    pub fn is_connected(&self) -> bool {
        self.msg_type == "connected"
    }

    /// Check if this is an error.
    pub fn is_error(&self) -> bool {
        self.msg_type == "error"
    }

    /// Check if this is a pong.
    pub fn is_pong(&self) -> bool {
        self.msg_type == "pong"
    }

    /// Check if this is an emoji reaction event.
    pub fn is_reaction(&self) -> bool {
        self.msg_type == "reaction"
    }

    /// Get the content text, if any.
    pub fn text(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// The message type as a parsed enum (for pattern matching).
    pub fn kind(&self) -> ServerMessageKind {
        match self.msg_type.as_str() {
            "connected" => ServerMessageKind::Connected,
            "message" => ServerMessageKind::Message,
            "typing" => ServerMessageKind::Typing,
            "stream_start" => ServerMessageKind::StreamStart,
            "stream_delta" => ServerMessageKind::StreamDelta,
            "stream_end" => ServerMessageKind::StreamEnd,
            "stream_discard" => ServerMessageKind::StreamDiscard,
            "pong" => ServerMessageKind::Pong,
            "error" => ServerMessageKind::Error,
            "reminder" => ServerMessageKind::Reminder,
            "tool_start" => ServerMessageKind::ToolStart,
            "tool_end" => ServerMessageKind::ToolEnd,
            "compact_start" => ServerMessageKind::CompactStart,
            "compact_end" => ServerMessageKind::CompactEnd,
            "reaction" => ServerMessageKind::Reaction,
            "context_update" => ServerMessageKind::ContextUpdate,
            "node_invoke" => ServerMessageKind::NodeInvoke,
            "team_update" => ServerMessageKind::TeamUpdate,
            "turn_indicator" => ServerMessageKind::TurnIndicator,
            "local_exec" => ServerMessageKind::LocalExec,
            "local_exec_result" => ServerMessageKind::LocalExecResult,
            "permission_prompt" => ServerMessageKind::PermissionPrompt,
            "image" => ServerMessageKind::Image,
            "connect_prompt" => ServerMessageKind::ConnectPrompt,
            "route" => ServerMessageKind::Route,
            _ => ServerMessageKind::Unknown,
        }
    }

    /// Check if this is a permission-prompt request that should be
    /// rendered as typed buttons.
    pub fn is_permission_prompt(&self) -> bool {
        self.msg_type == "permission_prompt"
    }

    /// Build a `permission_prompt` server message. Used by the gateway
    /// when forwarding a `ProgressEvent::ApprovalRequired` to the client.
    pub fn permission_prompt(
        request_id: &str,
        agent_id: Option<&str>,
        session_id: Option<&str>,
        tool_name: &str,
        reason: &str,
        options: workforce_core::policy::PermissionPromptOptions,
    ) -> Self {
        Self {
            msg_type: "permission_prompt".to_string(),
            content: Some(reason.to_string()),
            session_id: session_id.map(String::from),
            error: None,
            agent_id: agent_id.map(String::from),
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: false,
            token_match: None,
            request_id: Some(request_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            prompt_options: Some(options),
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
            status: None,
            since: None,
            text: None,
        }
    }
}

/// Parsed server message type for pattern matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessageKind {
    /// Initial connection acknowledgement.
    Connected,
    /// Complete agent response.
    Message,
    /// Agent is processing (typing indicator).
    Typing,
    /// Start of streamed response.
    StreamStart,
    /// Chunk of streamed response.
    StreamDelta,
    /// End of streamed response.
    StreamEnd,
    /// The current model call's streamed text is void; a replacement stream
    /// may follow.
    StreamDiscard,
    /// Response to ping.
    Pong,
    /// Error occurred.
    Error,
    /// Scheduler reminder/delivery.
    Reminder,
    /// Tool execution started.
    ToolStart,
    /// Tool execution finished.
    ToolEnd,
    /// Session compaction started.
    CompactStart,
    /// Session compaction finished.
    CompactEnd,
    /// Emoji reaction added to a message.
    Reaction,
    /// Context window usage update.
    ContextUpdate,
    /// Node invoke request (server asking client to execute a device capability).
    NodeInvoke,
    /// Team membership/status update.
    TeamUpdate,
    /// Turn-taking indicator (next speaker).
    TurnIndicator,
    /// Local exec request (gateway asking local agent to execute a command).
    LocalExec,
    /// Local exec result (local agent returning command result).
    LocalExecResult,
    /// Permission prompt — server is asking the user to approve a tool call.
    PermissionPrompt,
    /// Image attachment(s) for the client to render inline.
    Image,
    /// The turn found no login to run on and asks the person to connect one.
    ConnectPrompt,
    /// What ran the just-completed answer.
    Route,
    /// Unknown message type.
    Unknown,
}

/// REST chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional agent ID to select which agent to chat with.
    /// Defaults to the primary/default agent if not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The caller's own name for this turn (letters, digits, `-`, `_`, `.`;
    /// at most 64). The turn's cost and telemetry are then filed under the
    /// `task_id` the response carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

/// REST chat response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub response: String,
    pub session_id: String,
    /// What a turn sent with a `turn_id` is filed under: the key of
    /// `GET /api/stats/tasks/{task_id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub outcome: ChatOutcome,
    /// The turn stopped at its round cap: `response` is the wind-down, not
    /// a finished answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    /// The content hash of the agent's `config.toml` the whole turn ran
    /// under, when it has one and no reload landed mid-turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

/// Whether a REST chat turn ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatOutcome {
    /// It ran; `response` is the agent's reply.
    #[default]
    Answered,
    /// It did not run (the budget refused it, say); `response` says why.
    Refused,
}

/// Session info returned by the sessions endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub messages: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Sessions list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionInfo>,
}

/// Session transcript response from `/api/sessions/{id}/transcript`.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptResponse {
    pub session_id: String,
    pub entries: Vec<TranscriptEntryDto>,
}

/// A single transcript entry (client-side mirror of the server's `TranscriptEntry`).
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptEntryDto {
    /// Role: "user", "assistant", "system", or "tool".
    pub role: String,
    /// Text content.
    #[serde(default)]
    pub content: String,
    /// Tool metadata (present for tool entries).
    #[serde(default)]
    pub tool_metadata: Option<ToolMetadataDto>,
}

/// Lightweight tool metadata for transcript entries.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolMetadataDto {
    pub name: String,
    #[serde(default)]
    pub success: bool,
}

/// Agent info returned by the agents endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The model the agent answers with now (its live default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What it runs on: `claude-code`, `chatgpt`, `anthropic`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_type: Option<String>,
}

/// Agents list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsResponse {
    pub agents: Vec<AgentInfo>,
}

// ── Agent CRUD & Detail Protocol (DW-421) ──────────────────────────────────

/// Full agent detail response from `GET /api/agents/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetailResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub default: bool,
    /// List of identity file names present in the agent's workspace.
    #[serde(default)]
    pub identity_files: Vec<String>,
    /// Number of tools loaded for this agent.
    pub tool_count: usize,
    /// Active session count (from the session store).
    pub session_count: usize,
    /// Workspace path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_type: Option<String>,
}

/// Tool with full JSON schema from `GET /api/agents/{id}/tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Per-agent stats response from `GET /api/agents/{id}/stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatsResponse {
    pub agent_id: String,
    pub period: String,
    pub total_cost_usd: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub record_count: usize,
    /// Cost breakdown by model.
    #[serde(default)]
    pub by_model: std::collections::HashMap<String, f64>,
}

/// Request body for `POST /api/agents` (create agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub owner_type: Option<String>,
    /// `shared` (default) — automatic recall admits the user's memories
    /// captured with their other agents — or `isolated` — recall confined to
    /// what this agent learns itself. Persisted as the agent entry's
    /// `[agents.memory] agent_recall_scope`.
    #[serde(default)]
    pub agent_recall_scope: Option<String>,
}

/// Request body for `PUT /api/agents/{id}` (update agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAgentRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub default: Option<bool>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub owner_type: Option<String>,
}

// ── Node Invoke Protocol (DW-159) ──────────────────────────────────────────

/// Server → Client: request to invoke a node capability on a connected device.
///
/// The gateway sends this when an agent tool requests access to a device
/// capability (camera, location, notifications, etc.). The client executes
/// the command and responds with a [`NodeInvokeResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInvokeRequest {
    /// Always "node_invoke".
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Unique ID for correlating request with result.
    pub invoke_id: String,
    /// Capability command (e.g. "camera.snap", "location.get", "system.notify").
    pub command: String,
    /// Command parameters (JSON value, schema depends on command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// Timeout in milliseconds (default: 15000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl NodeInvokeRequest {
    pub fn new(invoke_id: &str, command: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            msg_type: "node_invoke".to_string(),
            invoke_id: invoke_id.to_string(),
            command: command.to_string(),
            params,
            timeout_ms: None,
        }
    }
}

/// Client → Server: result of a node invoke execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInvokeResult {
    /// Always "node_invoke_result".
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Matches the `invoke_id` from the corresponding [`NodeInvokeRequest`].
    pub invoke_id: String,
    /// Whether the invocation succeeded.
    pub ok: bool,
    /// Result payload (present on success, schema depends on command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Error message (present on failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Local Agent Protocol (DW-324) ──────────────────────────────────────────

/// Local Agent → Gateway: registration handshake sent immediately after connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAgentHello {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Unique identifier for this local agent instance.
    pub agent_node_id: String,
    /// Human-readable name (e.g. hostname).
    pub name: String,
    /// Capabilities this agent supports.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Filesystem paths this agent allows access to.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Tools this agent offers from an MCP server it tunnels, beside
    /// `exec.run`. Absent from agents that only run commands.
    #[serde(default)]
    pub tools: Vec<NodeToolSpec>,
}

/// Gateway → Local Agent: execute a shell command on the local machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalExecRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Unique ID for correlating request with result.
    pub request_id: String,
    /// Shell command to execute.
    pub command: String,
    /// Working directory (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Timeout in seconds (optional, default: 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Gateway → Local Agent: cancel a running command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalExecCancel {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Matches the request_id of the command to cancel.
    pub request_id: String,
}

/// Local Agent → Gateway: streaming output chunk during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalExecProgress {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
    /// Which stream: "stdout" or "stderr".
    pub stream: String,
    /// Output data chunk.
    pub data: String,
}

/// Local Agent → Gateway: final result of command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalExecResult {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One tool a local agent offers, as the MCP server it tunnels lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeToolSpec {
    /// The tool's name on that server.
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "object_schema")]
    pub input_schema: serde_json::Value,
    /// The server says the tool only reads (MCP `readOnlyHint`). Every
    /// other tool is gated as one that runs commands on the machine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

fn object_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Local Agent → Gateway: the tools it offers now, in place of those it
/// offered before.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTools {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub tools: Vec<NodeToolSpec>,
}

/// Gateway → Local Agent: run one of the tools it offers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeToolCall {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Unique ID correlating the call with its result.
    pub request_id: String,
    /// The tool's name as the agent offered it.
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Gateway → Local Agent: stop a tool call nobody waits for any more.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeToolCancel {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
}

/// Local Agent → Gateway: a tool call's outcome — the MCP `tools/call`
/// result (`content`, `isError`), or why there is none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeToolResult {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Gateway → Local Agent: the connection is refused, and why. The gateway
/// closes it next; connecting again the same way is refused the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAgentRefused {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::policy::{PermissionChoice, PermissionPromptOptions};

    fn delivered(name: &str, mime: &str) -> Attachment {
        Attachment {
            filename: name.into(),
            mime_type: mime.into(),
            data: if mime.starts_with("image/") {
                "aW1n".into()
            } else {
                String::new()
            },
            artifact_id: Some(format!("art_{name}")),
            size: Some(3),
            width: mime.starts_with("image/").then_some(2),
            height: mime.starts_with("image/").then_some(1),
            duration: None,
        }
    }

    #[test]
    fn a_client_that_declares_nothing_is_shown_images_as_it_always_was() {
        let accept = MediaAccept::from_names(None);
        assert!(!accept.declared());
        let shown = accept
            .shape(vec![
                delivered("chart.png", "image/png"),
                delivered("report.pdf", "application/pdf"),
                delivered("clip.mp4", "video/mp4"),
            ])
            .unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(
            serde_json::to_value(&shown[0]).unwrap(),
            serde_json::json!({"filename": "chart.png", "mime_type": "image/png", "data": "aW1n"}),
            "byte for byte what such a client was sent before"
        );
        assert_eq!(
            accept.shape(vec![delivered("report.pdf", "application/pdf")]),
            None,
            "nothing left to send"
        );
        assert_eq!(
            MediaAccept::from_names(Some(" , nonsense")),
            MediaAccept::default()
        );
    }

    #[test]
    fn a_client_is_shown_what_it_declared_with_its_reference_and_size() {
        let pdf_and_video = MediaAccept::from_names(Some("application/PDF, video/*;q=0.5"));
        assert!(pdf_and_video.admits("application/pdf"));
        assert!(pdf_and_video.admits("video/webm"));
        assert!(!pdf_and_video.admits("image/png"));
        assert!(!pdf_and_video.admits("text/csv"));
        let shown = pdf_and_video
            .shape(vec![
                delivered("chart.png", "image/png"),
                delivered("report.pdf", "application/pdf"),
            ])
            .unwrap();
        assert_eq!(shown, vec![delivered("report.pdf", "application/pdf")]);

        let everything = MediaAccept::from_names(Some("*/*"));
        assert!(everything.admits("application/vnd.ms-excel"));
        assert!(!everything.admits("not a type"));
        let shown = everything
            .shape(vec![delivered("chart.png", "image/png")])
            .unwrap();
        assert_eq!(shown[0].artifact_id.as_deref(), Some("art_chart.png"));
        assert_eq!((shown[0].width, shown[0].size), (Some(2), Some(3)));
    }

    #[test]
    fn permission_prompt_constructor_carries_typed_payload() {
        let opts = PermissionPromptOptions::ask(false);
        let msg = ServerMessage::permission_prompt(
            "req-1",
            Some("agent-x"),
            Some("sess-1"),
            "vault.delete",
            "delete secret",
            opts,
        );
        assert_eq!(msg.kind(), ServerMessageKind::PermissionPrompt);
        assert!(msg.is_permission_prompt());
        assert_eq!(msg.request_id.as_deref(), Some("req-1"));
        assert_eq!(msg.tool_name.as_deref(), Some("vault.delete"));
        let prompt = msg.prompt_options.expect("prompt options present");
        assert!(prompt.allow_once);
        assert!(prompt.deny_once);
        assert!(!prompt.allow_always);
        assert!(prompt.deny_always);
    }

    #[test]
    fn permission_response_serialises_typed_choice() {
        let msg = ClientMessage::permission_response("req-9", PermissionChoice::AllowAlways);
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"permission_response\""));
        assert!(s.contains("\"request_id\":\"req-9\""));
        assert!(s.contains("\"choice\":\"allow_always\""));
        let back: ClientMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back.choice, Some(PermissionChoice::AllowAlways));
        assert_eq!(back.request_id.as_deref(), Some("req-9"));
    }

    #[test]
    fn permission_prompt_roundtrip_keeps_payload() {
        let opts = PermissionPromptOptions::default();
        let original = ServerMessage::permission_prompt(
            "r1",
            None,
            Some("sess-7"),
            "memory.write",
            "save fact?",
            opts,
        );
        let json = serde_json::to_string(&original).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind(), ServerMessageKind::PermissionPrompt);
        assert_eq!(back.request_id.as_deref(), Some("r1"));
        assert_eq!(back.tool_name.as_deref(), Some("memory.write"));
        assert!(back.prompt_options.unwrap().allow_always);
    }

    #[test]
    fn stream_discard_maps_to_its_kind() {
        let msg: ServerMessage = serde_json::from_value(serde_json::json!({
            "type": "stream_discard",
            "content": "interrupted",
            "session_id": "sess-1",
            "speakable": false,
        }))
        .unwrap();
        assert_eq!(msg.kind(), ServerMessageKind::StreamDiscard);
        assert_eq!(msg.text(), Some("interrupted"));
    }
}

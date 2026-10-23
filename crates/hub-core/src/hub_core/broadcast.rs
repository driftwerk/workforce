//! What the hub pushes to connected gateway clients.

use serde::Serialize;
use workforce_gateway_client::protocol::Attachment;

/// A message broadcast to connected clients.
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastMessage {
    pub session_id: String,
    pub msg_type: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_max: Option<u32>,
    /// Whether the client should read this message aloud in audio/voice mode.
    /// Tool events and empty markers are `false`; response text is `true`.
    pub speakable: bool,
    /// Approval request id — set on `permission_prompt` messages so the
    /// client can echo it back in `permission_response`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Tool being approved — set on `permission_prompt` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Typed button payload for `permission_prompt` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_options: Option<workforce_core::policy::PermissionPromptOptions>,
    /// Which agent triggered the broadcast — set on `permission_prompt`
    /// messages so multi-agent sessions can label which agent is asking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Inline image attachments — set on `image` broadcasts so the WS
    /// forwarder carries them through to `ServerMessage.attachments`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
    /// Deterministic code citations — set on `code_citations` broadcasts (one per
    /// turn, after the answer). The WS forwarder carries them through to
    /// `ServerMessage.code_citations` so the client renders the source chips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_citations: Option<Vec<workforce_core::code_citation::CodeCitation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_prompt: Option<workforce_core::subscription::ConnectPrompt>,
    /// What ran the answer, set on `route` broadcasts with empty `content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Voice-bridge channel this message is addressed to. When set, the WS
    /// forwarder delivers ONLY to connections that registered the channel via
    /// `voice_listen` (and admit `target_identity`) — never to session
    /// clients, and never serialized toward any client.
    #[serde(skip_serializing)]
    pub target_channel: Option<String>,
    /// Identity the targeted content belongs to (a reminder's owner). Gates
    /// delivery alongside `target_channel`; routing metadata only, so it is
    /// never serialized.
    #[serde(skip_serializing)]
    pub target_identity: Option<String>,
}

impl BroadcastMessage {
    /// Create a broadcast message that should be spoken in audio mode.
    pub fn new(
        session_id: impl Into<String>,
        msg_type: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            msg_type: msg_type.into(),
            content: content.into(),
            context_tokens: None,
            context_max: None,
            speakable: true,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            agent_id: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            target_channel: None,
            target_identity: None,
        }
    }

    /// Build an `image` broadcast carrying inline image attachments for a
    /// session. The WS forwarder maps it to a `ServerMessage` with the same
    /// attachments; the in-process broadcast path delivers it to the client
    /// connected to `session_id`.
    pub fn image(
        session_id: impl Into<String>,
        agent_id: Option<String>,
        caption: Option<String>,
        attachments: Vec<Attachment>,
    ) -> Self {
        Self {
            agent_id,
            attachments: Some(attachments),
            // Images are visual, not spoken.
            ..Self::silent(session_id, "image", caption.unwrap_or_default())
        }
    }

    /// Create a broadcast message that should NOT be read aloud in audio mode.
    pub fn silent(
        session_id: impl Into<String>,
        msg_type: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            speakable: false,
            ..Self::new(session_id, msg_type, content)
        }
    }

    /// Build a typed `permission_prompt` broadcast — the WS forwarder maps
    /// this to a `ServerMessage::permission_prompt` for typed-button clients.
    /// `content` carries the human-readable detail for surfaces that don't
    /// render typed buttons (Slack fallback path is handled separately).
    pub fn permission_prompt(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        request_id: impl Into<String>,
        tool_name: impl Into<String>,
        reason: impl Into<String>,
        options: workforce_core::policy::PermissionPromptOptions,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            msg_type: "permission_prompt".to_string(),
            content: reason.into(),
            context_tokens: None,
            context_max: None,
            speakable: false,
            request_id: Some(request_id.into()),
            tool_name: Some(tool_name.into()),
            prompt_options: Some(options),
            agent_id: Some(agent_id.into()),
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            target_channel: None,
            target_identity: None,
        }
    }

    /// Build a device request for the connections of `owner` alone, whatever
    /// session each is in: the person's camera or location is asked of their
    /// own devices and shown to nobody else's.
    pub fn node_invoke(owner: &str, request: impl Into<String>) -> Self {
        Self {
            target_identity: Some(owner.to_string()),
            ..Self::silent("", "node_invoke", request)
        }
    }

    /// Build a spoken announcement targeted at the voice-bridge connection(s)
    /// registered for `channel` and belonging to `target_identity` — the
    /// delivery half of a voice reminder. Session-less by design: routing runs
    /// on the target fields instead.
    pub fn voice_announce(channel: &str, target_identity: &str, text: impl Into<String>) -> Self {
        Self {
            target_channel: Some(channel.to_string()),
            target_identity: Some(target_identity.to_string()),
            ..Self::new("", "reminder", text)
        }
    }

    /// Build a `code_citations` broadcast carrying the turn's deterministic code
    /// citations. Emitted once per turn AFTER the answer; the WS forwarder maps
    /// it to a `ServerMessage` the client attaches to the completed answer.
    pub fn code_citations(
        session_id: impl Into<String>,
        citations: Vec<workforce_core::code_citation::CodeCitation>,
    ) -> Self {
        Self {
            code_citations: Some(citations),
            ..Self::silent(session_id, "code_citations", String::new())
        }
    }

    /// Build a `route` broadcast: the line saying what ran the answer, in
    /// its own field.
    pub fn route(session_id: impl Into<String>, line: String) -> Self {
        Self {
            route: Some(line),
            ..Self::silent(session_id, "route", String::new())
        }
    }

    /// Build a `connect_prompt` broadcast asking the person a turn was for to
    /// connect the login it needed. Its text is the turn's answer, sent apart.
    pub fn connect_prompt(
        session_id: impl Into<String>,
        prompt: workforce_core::subscription::ConnectPrompt,
    ) -> Self {
        Self {
            connect_prompt: Some(prompt),
            ..Self::silent(session_id, "connect_prompt", String::new())
        }
    }
}

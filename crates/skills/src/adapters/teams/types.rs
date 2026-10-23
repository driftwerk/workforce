//! Bot Framework Activity wire types — the subset Workforce reads and writes.
//!
//! Field names follow the Bot Framework REST schema (camelCase). Unknown fields
//! are ignored on deserialize, so the types stay small without rejecting real
//! payloads. Reference:
//! <https://learn.microsoft.com/azure/bot-service/rest-api/bot-framework-rest-connector-api-reference>

use serde::{Deserialize, Serialize};

/// A Bot Framework Activity — the envelope for every inbound and outbound
/// message on the Teams channel. Only the fields Workforce uses are modeled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Activity type: `message`, `invoke`, `conversationUpdate`, `messageReaction`, …
    #[serde(rename = "type")]
    pub activity_type: String,
    /// Platform-assigned activity id (the inbound message id; `reply_to_id` target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Connector base URL replies are POSTed to. **Security-critical:** on an
    /// inbound activity this must equal the `serviceUrl` claim of the auth token,
    /// otherwise a forged activity could redirect our authenticated replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url: Option<String>,
    /// Originating channel (`msteams` for Teams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ChannelAccount>,
    /// Recipient (our bot, on inbound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<ChannelAccount>,
    /// Conversation (channel / group chat / 1:1) the activity belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationAccount>,
    /// Set on a reply to thread it under the referenced inbound activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<String>,
    /// Message body (for `message` activities).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Invoke name (e.g. `adaptiveCard/action`, `composeExtension/query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Invoke / event payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Teams-specific routing data (carries `tenant.id`, team/channel ids).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_data: Option<serde_json::Value>,
    /// Entities such as `mention`s. Kept opaque until the Phase 1 renderer needs them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<serde_json::Value>,
}

impl Activity {
    /// Whether this activity is an `invoke` — the channel expects a synchronous
    /// [`InvokeResponse`] in the HTTP body (Adaptive Card `Action.Execute`,
    /// task modules, message extensions). See [`super::event_handler`].
    pub fn is_invoke(&self) -> bool {
        self.activity_type.eq_ignore_ascii_case("invoke")
    }

    /// The tenant id, read from `conversation.tenantId` then `channelData.tenant.id`.
    /// Teams populates one or both depending on context.
    pub fn tenant_id(&self) -> Option<String> {
        if let Some(t) = self.conversation.as_ref().and_then(|c| c.tenant_id.clone()) {
            return Some(t);
        }
        self.channel_data
            .as_ref()
            .and_then(|d| d.get("tenant"))
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Build the [`ConversationReference`] needed to reply to (or later, proactively
    /// re-open) this conversation. Returns `None` if the activity lacks the
    /// connector `serviceUrl` or a conversation — both required to address a reply.
    pub fn conversation_reference(&self) -> Option<ConversationReference> {
        let service_url = self.service_url.clone()?;
        let conversation = self.conversation.clone()?;
        Some(ConversationReference {
            activity_id: self.id.clone(),
            user: self.from.clone(),
            bot: self.recipient.clone(),
            conversation,
            channel_id: self.channel_id.clone(),
            service_url,
        })
    }
}

/// A user or bot on a Teams conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelAccount {
    /// Channel-scoped account id (`from.id`). Always present; not stable across tenants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name, when the channel includes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Stable Entra (AAD) object id of the user — the preferred pairing key.
    /// Absent for guests/anonymous and some channel contexts (fall back to `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aad_object_id: Option<String>,
}

/// The conversation an activity belongs to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationAccount {
    /// Conversation id — the channel address for replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `personal` (1:1), `groupChat`, or `channel`. Drives privacy posture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_type: Option<String>,
    /// Tenant id when Teams attaches it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

impl ConversationAccount {
    /// Whether this is a 1:1 personal chat (the only context where bots can send
    /// files natively — see the Phase 3 files ticket DW-561).
    pub fn is_personal(&self) -> bool {
        self.conversation_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("personal"))
    }
}

/// Enough to address a reply or a later proactive message to a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationReference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ChannelAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<ChannelAccount>,
    pub conversation: ConversationAccount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Connector base URL — replies POST to `{service_url}/v3/conversations/...`.
    pub service_url: String,
}

/// The body a bot returns synchronously for an `invoke` activity. Teams expects
/// HTTP 200 with this JSON; `status` is the inner status the client acts on.
///
/// Phase 0 has no interactive handlers, so the inbound route answers
/// [`InvokeResponse::not_implemented`]. Phase 2 (DW-560) returns real Adaptive
/// Card payloads here. Reserved now so the handler contract does not change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeResponse {
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

impl InvokeResponse {
    /// `501` — no handler for this invoke yet (Phase 0).
    pub fn not_implemented() -> Self {
        Self {
            status: 501,
            body: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_activity_json() -> &'static str {
        r#"{
            "type": "message",
            "id": "1700000000000",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "channelId": "msteams",
            "from": { "id": "29:user-id", "name": "Allan", "aadObjectId": "aad-123" },
            "recipient": { "id": "28:bot-app-id", "name": "Workforce" },
            "conversation": { "id": "19:meeting@thread.tacv2", "conversationType": "channel", "tenantId": "tenant-abc" },
            "text": "<at>Workforce</at> hello",
            "channelData": { "tenant": { "id": "tenant-abc" } }
        }"#
    }

    #[test]
    fn parses_message_activity() {
        let a: Activity = serde_json::from_str(message_activity_json()).unwrap();
        assert_eq!(a.activity_type, "message");
        assert_eq!(a.id.as_deref(), Some("1700000000000"));
        assert_eq!(
            a.service_url.as_deref(),
            Some("https://smba.trafficmanager.net/amer/")
        );
        assert_eq!(
            a.from.as_ref().and_then(|f| f.aad_object_id.as_deref()),
            Some("aad-123")
        );
        assert!(!a.is_invoke());
    }

    #[test]
    fn tenant_id_reads_conversation_then_channel_data() {
        let a: Activity = serde_json::from_str(message_activity_json()).unwrap();
        assert_eq!(a.tenant_id().as_deref(), Some("tenant-abc"));

        // Falls back to channelData.tenant.id when conversation lacks it.
        let json = r#"{
            "type": "message",
            "conversation": { "id": "19:x" },
            "channelData": { "tenant": { "id": "from-channel-data" } }
        }"#;
        let a: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(a.tenant_id().as_deref(), Some("from-channel-data"));
    }

    #[test]
    fn conversation_reference_requires_service_url_and_conversation() {
        let a: Activity = serde_json::from_str(message_activity_json()).unwrap();
        let r = a.conversation_reference().expect("reference");
        assert_eq!(r.service_url, "https://smba.trafficmanager.net/amer/");
        assert_eq!(
            r.conversation.id.as_deref(),
            Some("19:meeting@thread.tacv2")
        );
        assert_eq!(r.activity_id.as_deref(), Some("1700000000000"));

        // Missing serviceUrl → no reference.
        let a = Activity {
            activity_type: "message".into(),
            conversation: Some(ConversationAccount::default()),
            ..Default::default()
        };
        assert!(a.conversation_reference().is_none());
    }

    #[test]
    fn is_invoke_is_case_insensitive() {
        let a = Activity {
            activity_type: "Invoke".into(),
            ..Default::default()
        };
        assert!(a.is_invoke());
    }

    #[test]
    fn conversation_type_personal_detection() {
        let c = ConversationAccount {
            conversation_type: Some("personal".into()),
            ..Default::default()
        };
        assert!(c.is_personal());
        let c = ConversationAccount {
            conversation_type: Some("channel".into()),
            ..Default::default()
        };
        assert!(!c.is_personal());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{ "type": "typing", "someFutureField": { "x": 1 }, "timestamp": "2026-01-01T00:00:00Z" }"#;
        let a: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(a.activity_type, "typing");
    }

    #[test]
    fn invoke_response_not_implemented() {
        let r = InvokeResponse::not_implemented();
        assert_eq!(r.status, 501);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("body"), "None body is omitted");
    }
}

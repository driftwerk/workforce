//! Inbound Activity → routed disposition.
//!
//! The hub's `POST /api/messages` handler (later increment) authenticates the
//! request, deserializes a [`Activity`], and calls [`TeamsEventHandler::handle`].
//! The returned [`TeamsDisposition`] tells the route what to do:
//!
//! - [`TeamsDisposition::Async`] — a `message`; hand the [`TeamsAction`] to the
//!   dispatch loop via the action channel and return `200` immediately.
//! - [`TeamsDisposition::Invoke`] — an `invoke` (Adaptive Card `Action.Execute`,
//!   task modules, message extensions). Bot Framework requires a **synchronous**
//!   [`InvokeResponse`] in the HTTP body. Phase 0 has no interactive handlers, so
//!   the route answers [`InvokeResponse::not_implemented`]; Phase 2 (DW-560)
//!   fills this in. The shape is reserved now so the contract does not change.
//! - [`TeamsDisposition::Ignore`] — self-authored, duplicate, or a type Phase 0
//!   does not handle (`conversationUpdate`, `typing`, `messageReaction`, …).

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

use super::types::{Activity, ConversationReference};

/// Resolved sender identity. Pairing prefers the stable Entra object id and
/// falls back to the channel-scoped `from.id` when it is absent (guests,
/// anonymous, some channel contexts).
#[derive(Debug, Clone, Default)]
pub struct TeamsIdentity {
    /// Channel-scoped account id (`from.id`).
    pub user_id: String,
    /// Stable Entra (AAD) object id, when present.
    pub aad_object_id: Option<String>,
    /// Tenant the user belongs to.
    pub tenant_id: Option<String>,
    /// Display name, when the channel includes it.
    pub name: Option<String>,
}

impl TeamsIdentity {
    /// The key used to look up / pair this user: Entra object id when available,
    /// else the channel-scoped id.
    pub fn pairing_key(&self) -> String {
        self.aad_object_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.user_id.clone())
    }

    fn from_activity(activity: &Activity) -> Self {
        let from = activity.from.clone().unwrap_or_default();
        Self {
            user_id: from.id.unwrap_or_default(),
            aad_object_id: from.aad_object_id,
            tenant_id: activity.tenant_id(),
            name: from.name,
        }
    }
}

/// A `message` ready for the dispatch loop.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub identity: TeamsIdentity,
    /// `teams:<conversation.id>` — the `platform:id` channel address.
    pub channel: String,
    /// Raw message text (still carries `<at>…</at>` mention markup; the Phase 1
    /// renderer normalizes it).
    pub text: String,
    /// Inbound activity id (reply target / dedup key).
    pub activity_id: String,
    /// Whether this is a 1:1 personal chat (drives privacy posture + the Phase 3
    /// native-file path).
    pub personal: bool,
    /// Everything needed to address a reply or a later proactive message.
    pub reference: ConversationReference,
}

/// An `invoke` activity that expects a synchronous [`InvokeResponse`]. Reserved
/// for Phase 2 interactive handlers; Phase 0 answers not-implemented.
#[derive(Debug, Clone)]
pub struct InvokeActivity {
    /// e.g. `adaptiveCard/action`, `composeExtension/query`, `task/fetch`.
    pub name: String,
    pub value: serde_json::Value,
    pub identity: TeamsIdentity,
    pub reference: ConversationReference,
}

/// Work the dispatch loop performs asynchronously (request already answered `200`).
#[derive(Debug, Clone)]
pub enum TeamsAction {
    ProcessMessage(InboundMessage),
}

/// What the inbound route should do with a parsed [`Activity`].
#[derive(Debug, Clone)]
pub enum TeamsDisposition {
    /// Dispatch asynchronously; answer the HTTP request `200` right away.
    Async(TeamsAction),
    /// `invoke` — the route must return a synchronous [`InvokeResponse`].
    Invoke(InvokeActivity),
    /// Nothing to do.
    Ignore,
}

/// Default dedup capacity (activity ids). Teams may redeliver on timeout.
const DEDUP_CAPACITY: usize = 4096;

/// Parses and routes inbound activities for one bot registration.
pub struct TeamsEventHandler {
    /// The bot's channel-scoped id (`28:<app-id>`); when set, its own activities
    /// are dropped.
    bot_id: Option<String>,
    /// Recently seen activity ids (retry/duplicate suppression).
    seen: Mutex<LruCache<String, ()>>,
}

impl Default for TeamsEventHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl TeamsEventHandler {
    pub fn new() -> Self {
        Self {
            bot_id: None,
            seen: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEDUP_CAPACITY).expect("non-zero"),
            )),
        }
    }

    /// Set the bot's channel-scoped id so its own activities are ignored.
    pub fn with_bot_id(mut self, bot_id: impl Into<String>) -> Self {
        self.bot_id = Some(bot_id.into());
        self
    }

    /// Records `activity.id`; reports whether it was already seen (retry/duplicate).
    fn is_duplicate(&self, activity: &Activity) -> bool {
        match activity.id.as_deref() {
            Some(id) => self
                .seen
                .lock()
                .expect("seen mutex")
                .put(id.to_string(), ())
                .is_some(),
            None => false,
        }
    }

    fn is_self_authored(&self, activity: &Activity) -> bool {
        match (
            self.bot_id.as_deref(),
            activity.from.as_ref().and_then(|f| f.id.as_deref()),
        ) {
            (Some(bot), Some(from)) => bot == from,
            _ => false,
        }
    }

    /// Async for parity with the Slack handler and to leave room for directory
    /// lookups in later phases.
    pub async fn handle(&self, activity: Activity) -> TeamsDisposition {
        if self.is_self_authored(&activity) || self.is_duplicate(&activity) {
            return TeamsDisposition::Ignore;
        }
        let identity = TeamsIdentity::from_activity(&activity);

        if activity.is_invoke() {
            return route_invoke(activity, identity);
        }
        if activity.activity_type == "message" {
            return route_message(activity, identity);
        }
        TeamsDisposition::Ignore
    }
}

/// `invoke` → the reserved synchronous path; Phase 2 (DW-560) fills in handlers.
fn route_invoke(activity: Activity, identity: TeamsIdentity) -> TeamsDisposition {
    let Some(reference) = activity.conversation_reference() else {
        return TeamsDisposition::Ignore;
    };
    TeamsDisposition::Invoke(InvokeActivity {
        name: activity.name.unwrap_or_default(),
        value: activity.value.unwrap_or(serde_json::Value::Null),
        identity,
        reference,
    })
}

/// `message` → async dispatch as [`TeamsAction::ProcessMessage`]. A message we
/// cannot address a reply to (no connector reference, or no conversation id) is
/// dropped rather than dispatched against a malformed channel.
fn route_message(activity: Activity, identity: TeamsIdentity) -> TeamsDisposition {
    let Some(reference) = activity.conversation_reference() else {
        return TeamsDisposition::Ignore;
    };
    let Some(conversation_id) = reference.conversation.id.clone() else {
        return TeamsDisposition::Ignore;
    };
    let personal = reference.conversation.is_personal();
    TeamsDisposition::Async(TeamsAction::ProcessMessage(InboundMessage {
        identity,
        channel: format!("teams:{conversation_id}"),
        text: activity.text.unwrap_or_default(),
        activity_id: activity.id.unwrap_or_default(),
        personal,
        reference,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> TeamsEventHandler {
        TeamsEventHandler::new().with_bot_id("28:bot-app-id")
    }

    fn message(id: &str, from_id: &str) -> Activity {
        serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": id,
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "channelId": "msteams",
            "from": { "id": from_id, "name": "Allan", "aadObjectId": "aad-123" },
            "recipient": { "id": "28:bot-app-id" },
            "conversation": { "id": "19:thread", "conversationType": "channel", "tenantId": "t1" },
            "text": "hello"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn message_becomes_process_message() {
        let d = handler().handle(message("a1", "29:user")).await;
        match d {
            TeamsDisposition::Async(TeamsAction::ProcessMessage(m)) => {
                assert_eq!(m.channel, "teams:19:thread");
                assert_eq!(m.text, "hello");
                assert_eq!(m.identity.pairing_key(), "aad-123");
                assert_eq!(m.activity_id, "a1");
                assert!(!m.personal);
            }
            other => panic!("expected ProcessMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn duplicate_activity_id_is_ignored() {
        let h = handler();
        assert!(matches!(
            h.handle(message("dup", "29:user")).await,
            TeamsDisposition::Async(_)
        ));
        assert!(matches!(
            h.handle(message("dup", "29:user")).await,
            TeamsDisposition::Ignore
        ));
    }

    #[tokio::test]
    async fn self_authored_is_ignored() {
        let d = handler().handle(message("a2", "28:bot-app-id")).await;
        assert!(matches!(d, TeamsDisposition::Ignore));
    }

    #[tokio::test]
    async fn invoke_is_reserved_for_sync_response() {
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "invoke",
            "id": "inv1",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "from": { "id": "29:user" },
            "conversation": { "id": "19:thread" },
            "name": "adaptiveCard/action",
            "value": { "action": { "verb": "approve" } }
        }))
        .unwrap();
        match handler().handle(activity).await {
            TeamsDisposition::Invoke(inv) => {
                assert_eq!(inv.name, "adaptiveCard/action");
                assert_eq!(inv.value["action"]["verb"], "approve");
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unhandled_type_is_ignored() {
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "conversationUpdate",
            "id": "cu1",
            "conversation": { "id": "19:thread" }
        }))
        .unwrap();
        assert!(matches!(
            handler().handle(activity).await,
            TeamsDisposition::Ignore
        ));
    }

    #[tokio::test]
    async fn pairing_key_falls_back_to_user_id_without_aad() {
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": "a3",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "from": { "id": "29:guest-no-aad" },
            "conversation": { "id": "19:dm", "conversationType": "personal" },
            "text": "hi"
        }))
        .unwrap();
        match handler().handle(activity).await {
            TeamsDisposition::Async(TeamsAction::ProcessMessage(m)) => {
                assert_eq!(m.identity.pairing_key(), "29:guest-no-aad");
                assert!(m.personal);
            }
            other => panic!("expected ProcessMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_without_conversation_is_ignored() {
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "invoke",
            "id": "inv2",
            "name": "adaptiveCard/action"
        }))
        .unwrap();
        assert!(matches!(
            handler().handle(activity).await,
            TeamsDisposition::Ignore
        ));
    }

    #[tokio::test]
    async fn message_without_conversation_id_is_ignored() {
        // Conversation present (so a reference builds) but unaddressable — fail safe.
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": "a4",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "from": { "id": "29:user" },
            "conversation": { "conversationType": "channel" },
            "text": "no conversation id"
        }))
        .unwrap();
        assert!(matches!(
            handler().handle(activity).await,
            TeamsDisposition::Ignore
        ));
    }

    #[tokio::test]
    async fn message_without_id_is_still_processed() {
        // No activity id → dedup is skipped, but a reply is still addressable.
        let activity: Activity = serde_json::from_value(serde_json::json!({
            "type": "message",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "from": { "id": "29:user" },
            "conversation": { "id": "19:thread" },
            "text": "no id"
        }))
        .unwrap();
        assert!(matches!(
            handler().handle(activity).await,
            TeamsDisposition::Async(_)
        ));
    }
}

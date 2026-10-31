//! `/ws/desk`: a Desk's hub link, one socket per paired hub, over which the
//! hub tells the person's Desk that one of their conversations gained a
//! message outside a live turn, and that their floor or their plans' usage
//! changed. It anchors no session and takes no chat; the Desk drains the
//! conversation it names through the conversation routes, and reads the
//! floor's routes again.
//!
//! Server → client: `{"type": "hello", "features": [...]}` once, then
//! `{"type": "desk_delivered", "agent_id", "channel", "session_id",
//! "message_id", "kind", "place", "spoken"}` per delivery,
//! `{"type": "floor_changed"}` / `{"type": "usage_changed"}` per change, and
//! `{"type": "pong"}` for each `{"type": "ping"}`.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use workforce_common::channels::desk::{DeskDelivered, DeskSignal};

use crate::broadcast::{BroadcastMessage, DESK_DELIVERED};
use crate::gateway::{authenticate_ws, desk_features, GatewayState};

#[derive(Debug, Deserialize)]
pub(crate) struct LinkQuery {
    token: Option<String>,
}

/// What the hub sends on a link.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LinkFrame<'a> {
    Hello { features: Vec<String> },
    DeskDelivered(&'a DeskDelivered),
    FloorChanged,
    UsageChanged,
    ReadChanged { agent_id: &'a str, channel: &'a str },
    Pong,
}

impl<'a> LinkFrame<'a> {
    /// The frame `signal`, as `msg` carries it, is sent as.
    fn signal(signal: DeskSignal, msg: &'a BroadcastMessage) -> Self {
        match signal {
            DeskSignal::Floor => Self::FloorChanged,
            DeskSignal::Usage => Self::UsageChanged,
            DeskSignal::Read => Self::ReadChanged {
                agent_id: msg.agent_id.as_deref().unwrap_or_default(),
                channel: &msg.content,
            },
        }
    }
}

/// Whose link it is: a person, and whether they hold this hub, so hear
/// what changed of its own (its Desk sessions, its shared logins).
#[derive(Debug, Clone)]
struct Listener {
    identity: String,
    holds: bool,
}

/// What a client sends on a link.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LinkRequest {
    Ping,
}

/// What a broadcast tells `listener`'s link, if it is for them.
fn admitted<'a>(msg: &'a BroadcastMessage, listener: &Listener) -> Option<LinkFrame<'a>> {
    let theirs = msg.target_identity.as_deref() == Some(listener.identity.as_str());
    match DeskSignal::of_frame(&msg.msg_type) {
        Some(DeskSignal::Read) => theirs.then(|| LinkFrame::signal(DeskSignal::Read, msg)),
        Some(signal) => (theirs || (msg.target_identity.is_none() && listener.holds))
            .then(|| LinkFrame::signal(signal, msg)),
        None => (msg.msg_type == DESK_DELIVERED && theirs)
            .then_some(msg.delivered.as_ref())
            .flatten()
            .map(LinkFrame::DeskDelivered),
    }
}

/// Upgrade a hub link. It is a person's: a device paired to nobody, or a
/// token naming nobody, is refused with 403 `link_requires_identity`.
pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    Query(query): Query<LinkQuery>,
    State(state): State<Arc<GatewayState>>,
) -> Response {
    let caller = match authenticate_ws(&state, query.token.as_deref().unwrap_or_default()).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let Some(identity) = caller.identity_id else {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "link_requires_identity" })),
        )
            .into_response();
    };
    if caller
        .scopes
        .as_ref()
        .is_some_and(|scopes| !crate::jwt::has_scope(scopes, crate::jwt::SCOPE_SESSIONS))
    {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "link_requires_sessions" })),
        )
            .into_response();
    }
    let listener = Listener {
        holds: holds(&state, &identity),
        identity,
    };
    let token = query.token.unwrap_or_default();
    ws.on_upgrade(move |socket| run(socket, state, listener, token))
        .into_response()
}

/// Whether `identity` holds this hub now.
fn holds(state: &GatewayState, identity: &str) -> bool {
    crate::desk_floor::holds_the_hub(
        state.identity_store.as_ref(),
        crate::desk_driver::desk().is_some(),
        Some(identity),
    )
}

/// Whether `token` still names `listener`'s person, read again on each ping
/// so a revoked device, an expired token or a suspension ends their link;
/// and, when it does, whether they hold the hub now.
async fn still_theirs(state: &GatewayState, token: &str, listener: &Listener) -> Option<bool> {
    let caller = authenticate_ws(state, token).await.ok()?;
    (caller.identity_id.as_deref() == Some(listener.identity.as_str()))
        .then(|| holds(state, &listener.identity))
}

async fn send(
    socket: &mut futures::stream::SplitSink<WebSocket, Message>,
    frame: &LinkFrame<'_>,
) -> bool {
    match serde_json::to_string(frame) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(e) => {
            tracing::warn!(error = %e, "encoding a hub link frame");
            true
        }
    }
}

/// Serve one link until either side ends it. A link that falls behind the
/// broadcast is closed rather than left silently short of signals: the Desk
/// reconnects and drains what changed since.
async fn run(socket: WebSocket, state: Arc<GatewayState>, mut listener: Listener, token: String) {
    let (mut sender, mut receiver) = socket.split();
    let mut broadcasts = state.broadcast_tx.subscribe();
    let hello = LinkFrame::Hello {
        features: desk_features(true),
    };
    if !send(&mut sender, &hello).await {
        return;
    }
    loop {
        tokio::select! {
            broadcast = broadcasts.recv() => match broadcast {
                Ok(msg) => {
                    if let Some(frame) = admitted(&msg, &listener) {
                        if !send(&mut sender, &frame).await {
                            return;
                        }
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "a Desk hub link fell behind; closing it to be drained");
                    let _ = sender.send(Message::Close(None)).await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<LinkRequest>(&text) {
                    Ok(LinkRequest::Ping) => {
                        let Some(holds) = still_theirs(&state, &token, &listener).await else {
                            return;
                        };
                        listener.holds = holds;
                        if !send(&mut sender, &LinkFrame::Pong).await {
                            return;
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "an unreadable hub link frame"),
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::desk::{DeliveryKind, DeskPlace};

    fn delivered() -> DeskDelivered {
        DeskDelivered {
            agent_id: "scout".into(),
            channel: "desk".into(),
            session_id: "s1".into(),
            message_id: "m1".into(),
            kind: DeliveryKind::Job,
            place: DeskPlace::Dm,
            spoken: false,
        }
    }

    fn listener(identity: &str, holds: bool) -> Listener {
        Listener {
            identity: identity.into(),
            holds,
        }
    }

    fn heard(msg: &BroadcastMessage, listener: &Listener) -> Option<serde_json::Value> {
        admitted(msg, listener).map(|frame| serde_json::to_value(frame).unwrap())
    }

    #[test]
    fn a_read_moved_is_heard_by_its_persons_links_alone_and_names_the_conversation() {
        let moved = BroadcastMessage::desk_read_changed("person-1", "allan", "desk");
        assert_eq!(
            heard(&moved, &listener("person-1", false)),
            Some(
                serde_json::json!({ "type": "read_changed", "agent_id": "allan", "channel": "desk" })
            )
        );
        assert_eq!(
            heard(&moved, &listener("person-2", true)),
            None,
            "not even the holder's"
        );
        assert!(moved.for_links_only());
    }

    #[test]
    fn a_link_hears_only_its_own_persons_deliveries() {
        let signal = BroadcastMessage::desk_delivered("person-1", delivered());
        assert_eq!(
            heard(&signal, &listener("person-1", false)),
            Some(serde_json::to_value(LinkFrame::DeskDelivered(&delivered())).unwrap())
        );
        assert_eq!(heard(&signal, &listener("person-2", true)), None);
        let message = BroadcastMessage::new("s1", "message", "hi");
        assert_eq!(heard(&message, &listener("person-1", true)), None);
    }

    #[test]
    fn a_link_hears_its_persons_changes_and_its_hubs_own_when_they_hold_it() {
        for (signal, frame) in [
            (DeskSignal::Floor, "floor_changed"),
            (DeskSignal::Usage, "usage_changed"),
        ] {
            let theirs = BroadcastMessage::desk_signal(signal, Some("person-1"));
            let hubs = BroadcastMessage::desk_signal(signal, None);
            let said = Some(serde_json::json!({ "type": frame }));
            assert_eq!(heard(&theirs, &listener("person-1", false)), said);
            assert_eq!(heard(&theirs, &listener("person-2", true)), None);
            assert_eq!(heard(&hubs, &listener("person-1", true)), said);
            assert_eq!(
                heard(&hubs, &listener("person-1", false)),
                None,
                "a hub's own change is only its holder's"
            );
        }
    }

    #[test]
    fn the_device_of_a_desk_hubs_administrator_hears_its_sessions_change() {
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};
        let store = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        let member = Identity::new("Member", IdentityRole::Member);
        store.create(&admin).unwrap();
        store.create(&member).unwrap();
        let store: Arc<dyn IdentityStore> = Arc::new(store);
        let link = |who: &Identity| {
            let identity = who.id.to_string();
            Listener {
                holds: crate::desk_floor::holds_the_hub(Some(&store), true, Some(&identity)),
                identity,
            }
        };
        let sessions = BroadcastMessage::desk_signal(DeskSignal::Floor, None);
        assert_eq!(
            heard(&sessions, &link(&admin)),
            Some(serde_json::json!({"type": "floor_changed"}))
        );
        assert_eq!(heard(&sessions, &link(&member)), None);
    }

    #[test]
    fn the_frames_are_flat_and_typed() {
        assert_eq!(
            serde_json::to_value(LinkFrame::DeskDelivered(&delivered())).unwrap(),
            serde_json::json!({
                "type": "desk_delivered", "agent_id": "scout", "channel": "desk",
                "session_id": "s1", "message_id": "m1", "kind": "job",
                "place": {"kind": "dm"}, "spoken": false,
            })
        );
        assert_eq!(
            serde_json::to_value(LinkFrame::Hello {
                features: vec!["desk.place".into()]
            })
            .unwrap(),
            serde_json::json!({"type": "hello", "features": ["desk.place"]})
        );
        assert_eq!(
            serde_json::to_value(LinkFrame::Pong).unwrap(),
            serde_json::json!({"type": "pong"})
        );
        assert!(matches!(
            serde_json::from_str::<LinkRequest>(r#"{"type":"ping"}"#),
            Ok(LinkRequest::Ping)
        ));
    }
}

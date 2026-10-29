//! `/ws/desk`: a Desk's hub link, one socket per paired hub, over which the
//! hub tells the person's Desk that one of their conversations gained a
//! message outside a live turn. It anchors no session and takes no chat; the
//! Desk drains the conversation it names through the conversation routes.
//!
//! Server → client: `{"type": "hello", "features": [...]}` once, then
//! `{"type": "desk_delivered", "agent_id", "channel", "session_id",
//! "message_id", "kind", "place", "spoken"}` per delivery, and
//! `{"type": "pong"}` for each `{"type": "ping"}`.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use workforce_common::channels::desk::DeskDelivered;

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
    Pong,
}

/// What a client sends on a link.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LinkRequest {
    Ping,
}

/// The delivery a broadcast tells the link of `identity`, if it is theirs.
fn admitted<'a>(msg: &'a BroadcastMessage, identity: &str) -> Option<&'a DeskDelivered> {
    (msg.msg_type == DESK_DELIVERED && msg.target_identity.as_deref() == Some(identity))
        .then_some(msg.delivered.as_ref())
        .flatten()
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
    ws.on_upgrade(move |socket| run(socket, state, identity))
        .into_response()
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
async fn run(socket: WebSocket, state: Arc<GatewayState>, identity: String) {
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
                    if let Some(delivered) = admitted(&msg, &identity) {
                        if !send(&mut sender, &LinkFrame::DeskDelivered(delivered)).await {
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
                        if crate::standing::suspended(state.identity_store.as_ref(), &identity).is_some() {
                            return;
                        }
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

    #[test]
    fn a_link_hears_only_its_own_persons_deliveries() {
        let signal = BroadcastMessage::desk_delivered("person-1", delivered());
        assert_eq!(admitted(&signal, "person-1"), Some(&delivered()));
        assert_eq!(admitted(&signal, "person-2"), None);
        let message = BroadcastMessage::new("s1", "message", "hi");
        assert_eq!(admitted(&message, "person-1"), None);
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

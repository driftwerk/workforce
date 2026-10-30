//! `/ws/desk`, a Desk's hub link: it is a person's, it hears that a
//! conversation of theirs gained a message, and nobody else's.

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;
use workforce_common::channels::desk::{DeliveryKind, DeskDelivered, DeskPlace};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::broadcast::BroadcastMessage;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{
    agent_service, scoped_token_for, serve, token_for, GatewayParts, GATEWAY_TOKEN,
};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .expect("a readable frame");
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

/// No frame arrives within a short wait.
async fn quiet(socket: &mut Socket) {
    let frame = tokio::time::timeout(std::time::Duration::from_millis(300), socket.next()).await;
    assert!(frame.is_err(), "expected nothing, got {frame:?}");
}

fn delivered(session: &str) -> DeskDelivered {
    DeskDelivered {
        agent_id: "scout".into(),
        channel: "desk".into(),
        session_id: session.into(),
        message_id: "m1".into(),
        kind: DeliveryKind::Reminder,
        place: DeskPlace::Dm,
        spoken: false,
    }
}

#[tokio::test]
async fn a_link_hears_its_persons_deliveries_and_no_one_elses() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let person = Identity::new("Person", IdentityRole::Member);
    let other = Identity::new("Other", IdentityRole::Member);
    identities.create(&person).unwrap();
    identities.create(&other).unwrap();
    let pairing = Arc::new(DevicePairingService::new(
        Arc::new(SqlitePairingStore::in_memory().unwrap()),
        PairingConfig {
            device_pairing_enabled: true,
            ..Default::default()
        },
    ));
    let state = GatewayParts {
        device_pairing: Some(Arc::clone(&pairing)),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("scout".to_string(), service)].into_iter().collect(),
                "scout".into(),
            ),
            Arc::clone(&identities) as Arc<dyn IdentityStore>,
            std::path::PathBuf::from("/tmp/desk-link-test-skills"),
        )
    }
    .state();
    let addr = serve(Arc::clone(&state)).await;
    let link = |token: String| async move {
        let (socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws/desk?token={token}"))
                .await
                .expect("the link connects");
        socket
    };

    let mut mine = link(token_for(&person)).await;
    let mut theirs = link(token_for(&other)).await;
    assert_eq!(
        next(&mut mine).await,
        json!({"type": "hello", "features": ["desk.place", "desk.stream", "desk.deliveries"]})
    );
    next(&mut theirs).await;
    let (mut chat, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?token={}&channel=desk&desk=1",
        token_for(&person)
    ))
    .await
    .unwrap();
    assert_eq!(next(&mut chat).await["type"], "connected");

    state
        .broadcast_tx
        .send(BroadcastMessage::desk_delivered(
            &person.id.to_string(),
            delivered("s1"),
        ))
        .unwrap();
    let heard = next(&mut mine).await;
    assert_eq!(heard["type"], "desk_delivered");
    assert_eq!(heard["session_id"], "s1");
    assert_eq!(heard["kind"], "reminder");
    quiet(&mut theirs).await;
    quiet(&mut chat).await;

    mine.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    assert_eq!(next(&mut mine).await, json!({"type": "pong"}));
    identities
        .update(&Identity {
            status: workforce_identity::types::IdentityStatus::Suspended,
            ..person.clone()
        })
        .unwrap();
    workforce_hub_core::hub_core::standing::forget(person.id);
    mine.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), mine.next())
        .await
        .expect("the link answers in time");
    assert!(
        matches!(
            ended,
            None | Some(Ok(tungstenite::Message::Close(_))) | Some(Err(_))
        ),
        "a person suspended while linked is heard no more: {ended:?}"
    );

    let (_, pending) = reqwest_json(
        addr,
        reqwest::Method::POST,
        "/api/pair/device",
        json!({"device_name": "Desk on a shared Mac", "device_kind": "desktop_device"}),
    )
    .await;
    let code = pending["code"].as_str().unwrap();
    let (_, approved) = reqwest_json(
        addr,
        reqwest::Method::POST,
        &format!("/api/pair/approve/{code}"),
        Value::Null,
    )
    .await;
    let refused = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/desk?token={}",
        approved["token"].as_str().unwrap()
    ))
    .await
    .expect_err("a device linked to nobody has no deliveries");
    let tungstenite::Error::Http(response) = refused else {
        panic!("expected an HTTP refusal, got {refused:?}");
    };
    assert_eq!(response.status(), 403);
    let body = String::from_utf8(response.body().clone().unwrap_or_default()).unwrap();
    assert!(body.contains("link_requires_identity"), "{body}");

    let refused = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws/desk?token={}",
        scoped_token_for(&other, &["chat"])
    ))
    .await
    .expect_err("a token that may not read conversations hears none of them");
    let tungstenite::Error::Http(response) = refused else {
        panic!("expected an HTTP refusal, got {refused:?}");
    };
    assert_eq!(response.status(), 403);
}

async fn reqwest_json(
    addr: std::net::SocketAddr,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let request = reqwest::Client::new()
        .request(method, format!("http://{addr}{path}"))
        .bearer_auth(GATEWAY_TOKEN);
    let request = match body {
        Value::Null => request,
        body => request.json(&body),
    };
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

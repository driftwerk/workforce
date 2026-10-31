//! End to end: how far a person has read a DM is kept on the agent's hub,
//! so two Desks of theirs agree. A delivery made while both were closed is
//! unread on each when it comes back; read on one, the other's link is told
//! which conversation moved, and it reads nothing unread.
#![cfg(feature = "postgres")]

// The API tests' support, of which this test needs the gateway and tokens.
#[allow(dead_code)]
#[path = "api/support.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskMark, DESK_CHANNEL};
use workforce_common::session::{MessageRole, SessionMessage};
use workforce_common::session_postgres::PostgresSessionStore;
use workforce_hub::serve::{AgentService, Compose};
use workforce_hub::session_push::SessionPush;
use workforce_postgres::test_support::with_test_pool;

use support::{serve, token_for, Gateway};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .expect("a readable frame");
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

/// A Desk's hub link, past its hello, which must offer read points.
async fn link(addr: SocketAddr, token: &str) -> Socket {
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws/desk?token={token}"))
            .await
            .expect("the link connects");
    let hello = next(&mut socket).await;
    assert_eq!(hello["type"], "hello");
    assert!(
        hello["features"]
            .as_array()
            .unwrap()
            .contains(&json!("desk.reads")),
        "{hello}"
    );
    socket
}

async fn call(
    addr: SocketAddr,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let request = reqwest::Client::new()
        .request(method, format!("http://{addr}{path}"))
        .bearer_auth(token);
    let response = match body {
        Some(body) => request.json(&body),
        None => request,
    }
    .send()
    .await
    .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// The person's DM with scout, as their Desk lists it on hello.
async fn dm(addr: SocketAddr, token: &str) -> Option<Value> {
    let (status, listed) = call(
        addr,
        token,
        reqwest::Method::GET,
        "/api/desk/conversations",
        None,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    listed["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["agent_id"] == "scout" && c["channel"] == DESK_CHANNEL)
        .cloned()
}

/// A reminder saved in `owner`'s DM with the agent, as a delivery saves it:
/// the session is the outbox. Its message id.
fn remind(scout: &AgentService, owner: &str, text: &str) -> String {
    let sessions = scout.sessions.as_ref();
    let mut session = sessions
        .get_or_create(DESK_CHANNEL, owner, scout.agent_id.as_str())
        .unwrap();
    let message = SessionMessage {
        id: Uuid::new_v4().to_string(),
        role: MessageRole::Assistant,
        content: text.into(),
        tool_data: None,
        timestamp: chrono::Utc::now(),
        tokens: 8,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
        desk: Some(DeskMark {
            delivery: Some(DeliveryKind::Reminder),
            ..DeskMark::default()
        }),
    };
    session.push_message(message.clone());
    sessions.save(&session).unwrap();
    sessions.append_to_archive(&session.id, &message).unwrap();
    message.id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dm_read_on_one_desk_is_read_on_the_persons_other_desk() {
    let pg = std::sync::OnceLock::new();
    with_test_pool(
        |config| {
            let _ = pg.set(config.clone());
            PostgresSessionStore::build_pool(config)
        },
        |_| run(pg.get().cloned().expect("the test database")),
    )
    .await;
}

async fn run(pg: workforce_postgres::PostgresConfig) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = dir.path().to_string_lossy().into_owned();
    config.memory.db_path = dir.path().join("memory.db").to_string_lossy().into_owned();
    config.llm.api_key = Some("test-key".into());
    config.sessions.backend = "postgres".into();
    config.postgres = Some(pg);
    let scout = AgentService::from_config(config, None)
        .await
        .expect("an agent whose sessions are in Postgres");
    let gateway = Gateway::new(vec![("scout", Arc::clone(&scout))], None);
    workforce_hub::session_push::install(SessionPush::new(
        None,
        gateway.state.broadcast_tx.clone(),
    ));
    let person = gateway.admin.id.to_string();
    let token = token_for(&gateway.admin);
    let addr = serve(Arc::clone(&gateway.state)).await;

    // Both Desks link and list once, then close.
    for _ in 0..2 {
        let desk = link(addr, &token).await;
        assert_eq!(dm(addr, &token).await, None, "no DM yet");
        drop(desk);
    }

    // A reminder lands in the DM while both are closed.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let reminder = remind(&scout, &person, "Reminder: the launch page ships at five");

    // Each comes back to it unread.
    let mut studio = link(addr, &token).await;
    let mut laptop = link(addr, &token).await;
    let mut last = Value::Null;
    for _ in [&studio, &laptop] {
        let dm = dm(addr, &token).await.expect("the DM is listed");
        let unread = &dm["unread"];
        assert_eq!(
            (&unread["count"], &unread["for_you"], &unread["first"]),
            (&json!(1), &json!(1), &json!(reminder)),
            "{dm}"
        );
        assert_eq!(unread["last"]["message_id"], json!(reminder));
        assert!(dm.get("read").is_none(), "nothing read yet: {dm}");
        last = unread["last"].clone();
    }

    // Read on the studio Desk, the laptop's link is told which conversation
    // moved, and the laptop reads nothing unread.
    let (status, moved) = call(
        addr,
        &token,
        reqwest::Method::PUT,
        "/api/desk/conversations/scout/read",
        Some(
            json!({"channel": DESK_CHANNEL, "at": last["at"], "message": last["message_id"],
                    "mark": "advance"}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["read"], last);
    loop {
        let frame = next(&mut laptop).await;
        if frame["type"] == "read_changed" {
            assert_eq!(
                frame,
                json!({"type": "read_changed", "agent_id": "scout", "channel": DESK_CHANNEL})
            );
            break;
        }
    }
    let dm = dm(addr, &token).await.expect("the DM is listed");
    assert_eq!(dm["unread"]["count"], json!(0), "{dm}");
    assert_eq!(dm["read"]["message_id"], json!(reminder));
    loop {
        if next(&mut studio).await["type"] == "read_changed" {
            break;
        }
    }
}

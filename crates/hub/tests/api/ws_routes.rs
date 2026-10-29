//! A `/ws` connection anchors on its app's own conversation, and a device
//! paired by the gateway's admin token chats there as its person.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{agent_service, serve, Gateway, GatewayParts, GATEWAY_TOKEN};

/// The session id the gateway hands a connection on `query` in its
/// `connected` message.
async fn anchored_session(addr: SocketAddr, query: &str) -> String {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{query}"))
        .await
        .expect("websocket connects");
    let frame = socket
        .next()
        .await
        .expect("a first frame")
        .expect("a readable frame");
    let connected: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(connected["type"], "connected", "{connected}");
    connected["session_id"]
        .as_str()
        .expect("an anchored session")
        .to_string()
}

async fn call(
    addr: SocketAddr,
    bearer: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Value {
    let request = reqwest::Client::new()
        .request(method, format!("http://{addr}{path}"))
        .bearer_auth(bearer);
    let response = match body {
        Some(body) => request.json(&body),
        None => request,
    }
    .send()
    .await
    .unwrap();
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    response.json().await.unwrap()
}

#[tokio::test]
async fn each_app_anchors_on_its_own_channels_conversation() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let gateway = Gateway::new(vec![("default", service.clone())], None);
    let token = gateway.admin_token();
    let addr = serve(gateway.state.clone()).await;

    let phone = anchored_session(addr, &format!("token={token}")).await;
    let desk = anchored_session(addr, &format!("token={token}&channel=desk")).await;
    let release = anchored_session(addr, &format!("token={token}&channel=desk:release")).await;
    let roadmap = anchored_session(addr, &format!("token={token}&channel=desk:roadmap")).await;
    let phone_again = anchored_session(addr, &format!("token={token}&channel=ios")).await;
    let release_again =
        anchored_session(addr, &format!("token={token}&channel=desk:release")).await;

    assert_ne!(phone, desk, "the desktop app keeps its own conversation");
    assert_ne!(
        release, roadmap,
        "each desk room keeps its own conversation"
    );
    assert!(
        ![&phone, &desk].contains(&&release),
        "a room is apart from both apps"
    );
    assert_eq!(phone, phone_again, "naming no channel anchors on iOS");
    assert_eq!(release, release_again, "a room's conversation is resumed");
    let owner = gateway.admin.id.to_string();
    for (id, channel) in [
        (&phone, "ios"),
        (&desk, "desk"),
        (&release, "desk:release"),
        (&roadmap, "desk:roadmap"),
    ] {
        let session = service.sessions.get(id).unwrap().expect("anchored session");
        assert_eq!(session.channel, channel);
        assert_eq!(session.owner, owner);
    }
}

#[tokio::test]
async fn an_unknown_channel_is_refused_rather_than_joining_the_phones_conversation() {
    let data = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(vec![("default", agent_service(data.path()).await)], None);
    let token = gateway.admin_token();
    let addr = serve(gateway.state.clone()).await;

    for channel in ["fridge", "slack:C123", "desk:", "desk:Release", "desk:a_b"] {
        let refused = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/ws?token={token}&channel={channel}"
        ))
        .await
        .expect_err("an unknown channel never upgrades");
        let tungstenite::Error::Http(response) = refused else {
            panic!("{channel}: expected an HTTP refusal, got {refused:?}");
        };
        assert_eq!(response.status(), 400, "{channel}");
        let body = String::from_utf8(response.body().clone().unwrap_or_default()).unwrap();
        assert!(body.contains("invalid_channel"), "{channel}: {body}");
    }
}

/// The bootstrap a local app runs holding only the admin token: create its
/// person, pair a desktop device to them, and chat as them on `desk`. The
/// person's phone, paired first, stays linked.
#[tokio::test]
async fn the_admin_token_pairs_a_desktop_device_that_chats_as_its_person() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let identities: Arc<dyn IdentityStore> = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let state = GatewayParts {
        device_pairing: Some(Arc::new(DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig {
                device_pairing_enabled: true,
                ..Default::default()
            },
        ))),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("default".to_string(), service.clone())]
                    .into_iter()
                    .collect(),
                "default".into(),
            ),
            Arc::clone(&identities),
            std::path::PathBuf::from("/tmp/ws-routes-test-skills"),
        )
    }
    .state();
    let addr = serve(state).await;

    let person = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::POST,
        "/api/identities",
        Some(
            json!({"display_name": "Kings", "email": "kings@example.com", "role": "system_admin"}),
        ),
    )
    .await;
    let person_id = person["id"].as_str().unwrap().to_string();
    assert_eq!(person["role"], "system_admin");

    let mut tokens = Vec::new();
    for (device_name, device_kind) in [
        ("Kings's iPhone", None),
        ("Desk on studio", Some("desktop_device")),
    ] {
        let pending = call(
            addr,
            GATEWAY_TOKEN,
            reqwest::Method::POST,
            "/api/pair/device",
            Some(
                json!({"device_name": device_name, "identity_hint": person_id, "device_kind": device_kind}),
            ),
        )
        .await;
        let code = pending["code"].as_str().unwrap();
        let approved = call(
            addr,
            GATEWAY_TOKEN,
            reqwest::Method::POST,
            &format!("/api/pair/approve/{code}"),
            None,
        )
        .await;
        assert_eq!(approved["linked_identity_id"], person_id.as_str());
        let status: Value = reqwest::get(format!("http://{addr}/api/pair/device/status/{code}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            status["token"], approved["token"],
            "polling once consumes the stored copy"
        );
        tokens.push(approved["token"].as_str().unwrap().to_string());
    }

    let person = identities
        .get_by_id(uuid::Uuid::parse_str(&person_id).unwrap())
        .unwrap()
        .unwrap();
    let kinds: Vec<&str> = person
        .linked_ids
        .keys()
        .filter_map(|key| key.split_once(':').map(|(kind, _)| kind))
        .collect();
    assert!(kinds.contains(&"ios_device"), "the phone stays linked");
    assert!(kinds.contains(&"desktop_device"));

    let desk = anchored_session(addr, &format!("token={}&channel=desk", tokens[1])).await;
    let session = service
        .sessions
        .get(&desk)
        .unwrap()
        .expect("anchored session");
    assert_eq!(
        session.owner, person_id,
        "the desktop device chats as its person"
    );
    assert_eq!(session.channel, "desk");

    let me = call(addr, &tokens[1], reqwest::Method::GET, "/api/me", None).await;
    assert_eq!(
        me["id"],
        person_id.as_str(),
        "a held token is checked as its person"
    );

    let devices = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::GET,
        "/api/pair/devices",
        None,
    )
    .await;
    let device_id = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_name"] == "Desk on studio" && d["revoked"] == false)
        .expect("the desktop device is listed")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rotated = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::POST,
        &format!("/api/pair/devices/{device_id}/rotate"),
        None,
    )
    .await;
    let resumed = anchored_session(
        addr,
        &format!("token={}&channel=desk", rotated["token"].as_str().unwrap()),
    )
    .await;
    assert_eq!(resumed, desk, "a lost token is replaced without re-pairing");
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A connection on `query` and its `connected` frame.
async fn connect(addr: SocketAddr, query: &str) -> (Socket, Value) {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{query}"))
        .await
        .expect("websocket connects");
    let connected = next_frame(&mut socket).await;
    assert_eq!(connected["type"], "connected", "{connected}");
    (socket, connected)
}

async fn next_frame(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .expect("a readable frame");
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

/// Frames until one of type `kind`, which is returned with those before it.
async fn frames_until(socket: &mut Socket, kind: &str) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = next_frame(socket).await;
        let done = frame["type"] == kind;
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    use futures::SinkExt;
    socket
        .send(tungstenite::Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Desk declares itself and is offered structured turns; its room turn
/// reaches the model told the room, the thread and the lines before it,
/// while a connection that declares nothing sees no change.
#[tokio::test]
async fn a_desk_connection_sends_turns_the_agent_reads_in_their_place() {
    let data = tempfile::tempdir().unwrap();
    let (service, llm) = crate::support::agent_on_mock_llm(data.path()).await;
    let gateway = Gateway::new(vec![("default", service.clone())], None);
    let token = gateway.admin_token();
    let addr = serve(gateway.state.clone()).await;
    let room = uuid::Uuid::new_v4();
    let channel = format!("desk:{room}");

    let (_old, connected) = connect(addr, &format!("token={token}&channel={channel}")).await;
    assert!(connected.get("features").is_none(), "{connected}");
    let (_phone, connected) = connect(addr, &format!("token={token}&desk=1")).await;
    assert!(
        connected.get("features").is_none(),
        "only a Desk channel can declare Desk: {connected}"
    );
    let (mut desk, connected) =
        connect(addr, &format!("token={token}&channel={channel}&desk=1")).await;
    assert_eq!(connected["features"], json!(["desk.place"]));

    let thread = uuid::Uuid::new_v4();
    let turn = |room| {
        json!({
            "place": {"kind": "room", "room": room, "name": "Planning", "thread": thread},
            "speaker": {"kind": "person"},
            "context": [{"from": "helper (agent)", "text": "the build is green"}],
        })
    };
    send(
        &mut desk,
        json!({"type": "chat", "content": "and the release?", "turn_ref": "ref-wrong",
               "desk": turn(uuid::Uuid::new_v4())}),
    )
    .await;
    let refused = next_frame(&mut desk).await;
    assert_eq!(
        (&refused["type"], &refused["turn_ref"]),
        (&json!("error"), &json!("ref-wrong"))
    );

    send(
        &mut desk,
        json!({"type": "chat", "content": "and the release?", "turn_ref": "ref-1", "desk": turn(room)}),
    )
    .await;
    let frames = frames_until(&mut desk, "stream_end").await;
    assert!(frames.iter().any(|f| f["content"] == "done"), "{frames:?}");

    let asked = llm.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&asked.last().unwrap().body).into_owned();
    assert!(
        body.contains("conversation: room \\\"Planning\\\", in a thread"),
        "{body}"
    );
    assert!(body.contains("## Earlier in this thread\\nhelper (agent): the build is green"));
    let session = service
        .sessions
        .find(&channel, &gateway.admin.id.to_string())
        .unwrap()
        .expect("the room's conversation");
    let user = session
        .messages
        .iter()
        .find(|m| m.role == workforce_common::session::MessageRole::User)
        .unwrap();
    assert_eq!(user.content, "and the release?");
    assert_eq!(
        user.desk.as_ref().and_then(|mark| mark.thread),
        Some(thread)
    );
}

/// A spoken Desk message gets the agent's voice settings for that message
/// alone: the conversation does not become a voice conversation.
#[tokio::test]
async fn a_spoken_desk_message_is_a_voice_turn_without_latching_voice_mode() {
    let data = tempfile::tempdir().unwrap();
    let (service, _llm) = crate::support::agent_on_mock_llm(data.path()).await;
    let gateway = Gateway::new(vec![("default", service.clone())], None);
    let token = gateway.admin_token();
    let addr = serve(gateway.state.clone()).await;
    let (mut desk, connected) = connect(addr, &format!("token={token}&channel=desk&desk=1")).await;
    send(
        &mut desk,
        json!({"type": "chat", "content": "what's next?", "turn_ref": "ref-1",
               "desk": {"place": {"kind": "dm"}, "speaker": {"kind": "person"}, "spoken": true}}),
    )
    .await;
    frames_until(&mut desk, "stream_end").await;
    let session = service
        .sessions
        .get(connected["session_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(session.is_voice_turn());
    assert!(!session.is_voice());
    assert!(session.metadata.get("voice_mode").is_none());
}

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
    assert!(
        person.linked_ids.contains_key("ios_device"),
        "the phone stays linked"
    );
    assert!(person.linked_ids.contains_key("desktop_device"));

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

//! A person's Desk DM with an agent is one conversation, kept across
//! reconnects and restarts, and read back from the agent's own sessions.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};
use workforce_common::channels::DESK_CHANNEL;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::GatewayState;
use workforce_hub::serve::AgentService;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{agent_service, GatewayParts, GATEWAY_TOKEN};

/// A hub serving `service` as its one agent, on stores that outlive it.
fn hub(
    service: &Arc<AgentService>,
    identities: &Arc<dyn IdentityStore>,
    pairing: &Arc<DevicePairingService>,
) -> Arc<GatewayState> {
    GatewayParts {
        device_pairing: Some(Arc::clone(pairing)),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("scout".to_string(), Arc::clone(service))]
                    .into_iter()
                    .collect(),
                "scout".into(),
            ),
            Arc::clone(identities),
            std::path::PathBuf::from("/tmp/desk-conversation-test-skills"),
        )
    }
    .state()
}

/// The gateway with the conversation route Desk mounts beside it.
async fn serve(state: Arc<GatewayState>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = workforce_hub::gateway::router(Arc::clone(&state))
        .merge(workforce_hub::desk_conversations::router(state));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

async fn call(
    addr: SocketAddr,
    bearer: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
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
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// A person, and the desktop device Desk pairs to them, as Desk's bootstrap
/// makes them with the admin token: the person's id and the device token.
async fn paired_desk(addr: SocketAddr) -> (String, String) {
    let (_, person) = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::POST,
        "/api/identities",
        Some(json!({"display_name": "Kings", "role": "system_admin"})),
    )
    .await;
    let person_id = person["id"].as_str().unwrap().to_string();
    let (_, pending) = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::POST,
        "/api/pair/device",
        Some(json!({"device_name": "Desk on studio", "identity_hint": person_id, "device_kind": "desktop_device"})),
    )
    .await;
    let code = pending["code"].as_str().unwrap();
    let (_, approved) = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::POST,
        &format!("/api/pair/approve/{code}"),
        None,
    )
    .await;
    (person_id, approved["token"].as_str().unwrap().to_string())
}

/// The session a DM connection anchors on, as its `connected` frame says.
async fn dm_session(addr: SocketAddr, token: &str) -> String {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?token={token}&agent_id=scout&channel=desk"
    ))
    .await
    .expect("websocket connects");
    let frame = socket.next().await.unwrap().unwrap();
    let connected: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(connected["type"], "connected", "{connected}");
    connected["session_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_dm_is_one_conversation_across_reconnects_and_restarts_and_reads_back() {
    let data = tempfile::tempdir().unwrap();
    let identities: Arc<dyn IdentityStore> = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let pairing = Arc::new(DevicePairingService::new(
        Arc::new(SqlitePairingStore::in_memory().unwrap()),
        PairingConfig {
            device_pairing_enabled: true,
            ..Default::default()
        },
    ));
    let service = agent_service(data.path()).await;
    let addr = serve(hub(&service, &identities, &pairing)).await;
    let (person, token) = paired_desk(addr).await;

    let first = dm_session(addr, &token).await;
    let reconnected = dm_session(addr, &token).await;
    assert_eq!(
        first, reconnected,
        "a reconnect resumes the same conversation"
    );

    // A turn as the agent leaves it in the session.
    let mut session = service.sessions.get(&first).unwrap().unwrap();
    assert_eq!(
        (session.channel.as_str(), session.owner.as_str()),
        (DESK_CHANNEL, person.as_str())
    );
    session.add_user_message("what are you here to do?", None);
    session.add_assistant_message("To watch **CI** for you.", 7);
    service.sessions.save(&session).unwrap();

    // The hub restarts: a new agent service over the same data.
    let restarted = agent_service(data.path()).await;
    let addr = serve(hub(&restarted, &identities, &pairing)).await;
    assert_eq!(
        dm_session(addr, &token).await,
        first,
        "a restart resumes it too"
    );

    let (status, page) = call(
        addr,
        &token,
        reqwest::Method::GET,
        "/api/desk/conversations/scout?limit=200",
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{page}");
    assert_eq!(messages[0]["from"], "user");
    assert_eq!(messages[0]["text"], "what are you here to do?");
    assert_eq!(messages[1]["from"], "agent");
    assert_eq!(messages[1]["text"], "To watch **CI** for you.");
    assert!(messages[1]["at"].as_str().unwrap().contains('T'));
    assert_eq!(page["more"], false);

    let (status, older) = call(
        addr,
        &token,
        reqwest::Method::GET,
        &format!(
            "/api/desk/conversations/scout?before={}",
            messages[1]["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(older["messages"].as_array().unwrap().len(), 1);

    let (status, refused) = call(
        addr,
        GATEWAY_TOKEN,
        reqwest::Method::GET,
        "/api/desk/conversations/scout",
        None,
    )
    .await;
    assert_eq!(status, 400, "the admin token is no one's: {refused}");
    let (status, _) = call(
        addr,
        &token,
        reqwest::Method::GET,
        "/api/desk/conversations/nobody",
        None,
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(
        addr,
        "wfp_not-a-token",
        reqwest::Method::GET,
        "/api/desk/conversations/scout",
        None,
    )
    .await;
    assert_eq!(status, 401);
}

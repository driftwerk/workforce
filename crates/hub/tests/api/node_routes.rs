//! The node uplink: a laptop's `/ws/agent` connection acts for the person
//! its paired device is linked to, offers that person's sessions the tools
//! of the MCP server it tunnels, and answers their calls over the socket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_agent::tools::Tool;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::GatewayState;
use workforce_hub::local_agent_tool::NodeTool;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{serve, GatewayParts, GATEWAY_TOKEN};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A hub with device pairing on and three people, each with a paired device
/// linked to them, plus a device linked to nobody.
struct Hub {
    state: Arc<GatewayState>,
    addr: SocketAddr,
    alice: Identity,
    bob: Identity,
    /// Device tokens: Alice's, Bob's, Carol's (suspended), one linked to
    /// nobody, and Alice's chat-only one.
    tokens: [String; 5],
}

impl Hub {
    async fn new() -> Self {
        let identities: Arc<dyn IdentityStore> =
            Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let pairing = Arc::new(DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig {
                device_pairing_enabled: true,
                ..Default::default()
            },
        ));
        let people = ["Alice", "Bob", "Carol"].map(|name| {
            let person = Identity::new(name, IdentityRole::SystemAdmin);
            identities.create(&person).unwrap();
            person
        });
        // Each device is linked under a kind of its own, as pairing does.
        let pair = |person: Option<&Identity>, scopes: Vec<String>, kind: &str| {
            let request = pairing
                .initiate("laptop", scopes, None, None, None, None)
                .unwrap();
            let (device, token) = pairing.approve(&request.code, "admin").unwrap();
            if let Some(person) = person {
                let mut linked = identities.get_by_id(person.id).unwrap().unwrap();
                linked.linked_ids.insert(kind.into(), device.id);
                identities.update(&linked).unwrap();
            }
            token
        };
        let scopes = workforce_hub::jwt::device_scopes;
        let tokens = [
            pair(Some(&people[0]), scopes(), "desktop_device"),
            pair(Some(&people[1]), scopes(), "desktop_device"),
            pair(Some(&people[2]), scopes(), "desktop_device"),
            pair(None, scopes(), "desktop_device"),
            pair(Some(&people[0]), vec!["chat".into()], "voice_device"),
        ];
        let mut carol = identities.get_by_id(people[2].id).unwrap().unwrap();
        carol.status = IdentityStatus::Suspended;
        identities.update(&carol).unwrap();
        let state = GatewayParts {
            device_pairing: Some(pairing),
            ..GatewayParts::new(
                AgentRegistry::from_parts(Default::default(), "default".into()),
                identities,
                std::path::PathBuf::from("/tmp/node-routes-test-skills"),
            )
        }
        .state();
        let addr = serve(Arc::clone(&state)).await;
        let [alice, bob, _] = people;
        Self {
            state,
            addr,
            alice,
            bob,
            tokens,
        }
    }

    async fn connect(&self, token: &str) -> Result<Socket, WsError> {
        tokio_tungstenite::connect_async(format!("ws://{}/ws/agent?token={token}", self.addr))
            .await
            .map(|(socket, _)| socket)
    }

    /// A node connected with `token` as `id`, offering `tools`, once the
    /// hub has answered its hello.
    async fn node(&self, token: &str, id: &str, tools: Value) -> (Socket, Value) {
        let mut socket = self.connect(token).await.expect("upgrades");
        send(
            &mut socket,
            json!({
                "type": "local_agent_hello",
                "agent_node_id": id,
                "name": format!("{id} laptop"),
                "capabilities": ["shell"],
                "tools": tools,
            }),
        )
        .await;
        let answer = next(&mut socket).await.expect("an answer to the hello");
        (socket, answer)
    }

    /// The tool `name` as `person`'s sessions are offered it.
    async fn offered(&self, person: &Identity, name: &str) -> Option<NodeTool> {
        self.state
            .local_agents
            .tools_for(&person.id.to_string())
            .await
            .into_iter()
            .find(|tool| tool.name == name)
            .map(|tool| NodeTool::new(Arc::clone(&self.state.local_agents), tool))
    }
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
}

/// The next JSON frame, or `None` once the socket closes.
async fn next(socket: &mut Socket) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame in time")?
        {
            Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

fn tools() -> Value {
    json!([
        {"name": "desk_sessions", "description": "List desk sessions", "input_schema": {"type": "object"}},
        {"name": "files_read", "description": "Read a file", "read_only": true},
    ])
}

fn refusal_status(refused: WsError) -> u16 {
    match refused {
        WsError::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

/// The node binds to its device's person and offers their sessions alone
/// its tools; nobody else's turns list or reach it.
#[tokio::test]
async fn a_node_offers_its_tools_to_its_devices_person_alone() {
    let hub = Hub::new().await;
    let (_node, ack) = hub.node(&hub.tokens[0], "studio", tools()).await;
    assert_eq!(ack["type"], "local_agent_ack", "{ack}");

    let registry = &hub.state.local_agents;
    let alices: Vec<String> = registry
        .tools_for(&hub.alice.id.to_string())
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        alices,
        ["node.studio.desk_sessions", "node.studio.files_read"]
    );
    assert!(registry.tools_for(&hub.bob.id.to_string()).await.is_empty());
    assert!(registry.list_for(&hub.bob.id.to_string()).await.is_empty());
    assert!(
        registry
            .is_bound_to("studio", &hub.alice.id.to_string())
            .await
    );
}

/// Nobody's token, a device linked to no one, a suspended person's device,
/// a device paired for chat alone and an unknown token never upgrade.
#[tokio::test]
async fn a_node_that_would_act_for_no_one_or_a_suspended_person_is_refused() {
    let hub = Hub::new().await;
    for (token, status) in [
        (GATEWAY_TOKEN, 403),
        (hub.tokens[3].as_str(), 403),
        (hub.tokens[2].as_str(), 403),
        (hub.tokens[4].as_str(), 403),
        ("wfp_unknown", 401),
    ] {
        let refused = hub.connect(token).await.expect_err("never upgrades");
        assert_eq!(refusal_status(refused), status);
    }
}

/// Another person cannot take over a connected node's id; their node is
/// told why and closed, and the first stays theirs.
#[tokio::test]
async fn a_node_id_another_person_holds_is_refused_over_the_socket() {
    let hub = Hub::new().await;
    let (_alices, _) = hub.node(&hub.tokens[0], "studio", tools()).await;
    let (mut bobs, refusal) = hub.node(&hub.tokens[1], "studio", json!([])).await;
    assert_eq!(refusal["type"], "local_agent_refused", "{refusal}");
    assert!(refusal["message"].as_str().unwrap().contains("--id"));
    assert!(next(&mut bobs).await.is_none(), "then closed");
    assert!(
        hub.state
            .local_agents
            .is_bound_to("studio", &hub.alice.id.to_string())
            .await
    );
}

/// A call runs on the node over the socket and its result comes back; the
/// same tool called in another person's turn sends the node nothing.
#[tokio::test]
async fn a_node_tool_call_round_trips_over_the_socket_for_its_person_only() {
    let hub = Hub::new().await;
    let (mut node, _) = hub.node(&hub.tokens[0], "studio", tools()).await;
    let tool = hub
        .offered(&hub.alice, "node.studio.desk_sessions")
        .await
        .expect("offered to Alice");

    let as_bob = CURRENT_USER_ID
        .scope(hub.bob.id.to_string(), tool.execute(json!({})))
        .await
        .unwrap();
    assert!(!as_bob.success);
    assert!(as_bob.error.unwrap().contains("another person"));

    let call = tokio::spawn(CURRENT_USER_ID.scope(hub.alice.id.to_string(), async move {
        tool.execute(json!({"limit": 5})).await
    }));
    let asked = next(&mut node).await.unwrap();
    assert_eq!(
        asked["type"], "node_tool_call",
        "Bob's call sent nothing first"
    );
    assert_eq!(asked["name"], "desk_sessions");
    assert_eq!(asked["arguments"], json!({"limit": 5}));
    let answer = json!({"content": [{"type": "text", "text": "2 sessions"}], "isError": false});
    send(
        &mut node,
        json!({"type": "node_tool_result", "request_id": asked["request_id"], "result": answer}),
    )
    .await;
    let ran = call.await.unwrap().unwrap();
    assert!(ran.success, "{:?}", ran.error);
    assert_eq!(ran.output, answer);
}

/// A tool error reported by the node fails the call with the tool's words.
#[tokio::test]
async fn a_tool_error_on_the_node_fails_the_call_with_its_text() {
    let hub = Hub::new().await;
    let (mut node, _) = hub.node(&hub.tokens[0], "studio", tools()).await;
    let tool = hub
        .offered(&hub.alice, "node.studio.files_read")
        .await
        .unwrap();
    let call = tokio::spawn(CURRENT_USER_ID.scope(hub.alice.id.to_string(), async move {
        tool.execute(json!({})).await
    }));
    let asked = next(&mut node).await.unwrap();
    send(
        &mut node,
        json!({"type": "node_tool_result", "request_id": asked["request_id"],
               "result": {"content": [{"type": "text", "text": "no such file"}], "isError": true}}),
    )
    .await;
    let ran = call.await.unwrap().unwrap();
    assert!(!ran.success);
    assert_eq!(ran.error.as_deref(), Some("no such file"));
}

/// A node that goes away mid-call fails the call at once, and its tools
/// are no longer offered.
#[tokio::test]
async fn a_node_disconnecting_mid_call_fails_the_call_cleanly() {
    let hub = Hub::new().await;
    let (mut node, _) = hub.node(&hub.tokens[0], "studio", tools()).await;
    let tool = hub
        .offered(&hub.alice, "node.studio.desk_sessions")
        .await
        .unwrap();
    let call = tokio::spawn(CURRENT_USER_ID.scope(hub.alice.id.to_string(), async move {
        tool.execute(json!({})).await
    }));
    assert_eq!(next(&mut node).await.unwrap()["type"], "node_tool_call");
    node.close(None).await.unwrap();
    drop(node);
    let ran = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("fails well before the call's timeout")
        .unwrap()
        .unwrap();
    assert!(!ran.success);
    assert!(ran.error.unwrap().contains("disconnected"), "clean failure");
    assert!(hub
        .offered(&hub.alice, "node.studio.desk_sessions")
        .await
        .is_none());
}

/// A node updates what it offers over the socket.
#[tokio::test]
async fn a_node_replaces_its_tools_with_an_update() {
    let hub = Hub::new().await;
    let (mut node, _) = hub.node(&hub.tokens[0], "studio", tools()).await;
    send(
        &mut node,
        json!({"type": "node_tools", "tools": [{"name": "desk_start", "description": "Start"}]}),
    )
    .await;
    let alice = hub.alice.id.to_string();
    tokio::time::timeout(Duration::from_secs(10), async {
        while hub.state.local_agents.tools_for(&alice).await.len() != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the update lands");
    assert!(hub
        .offered(&hub.alice, "node.studio.desk_start")
        .await
        .is_some());
}

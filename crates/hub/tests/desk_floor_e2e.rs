//! End to end: a person's floor on a hub, driven through the real `/ws`,
//! `/ws/desk` and floor routes as Desk drives them, with an agent thinking
//! on a scripted model. What it asks needs the person until they speak to
//! it again; a post it makes off Desk waits on them, is answered from the
//! floor, and once always allowed goes through unasked; a routed approval
//! is theirs to decide there; and every change is signalled on their link.
#![cfg(feature = "postgres")]

// The API tests' support, of which this test needs the gateway and tokens.
#[allow(dead_code)]
#[path = "api/support.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite;
use uuid::Uuid;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use workforce_hub::approvals::{PendingApprovalStore, SqlitePendingApprovalStore};
use workforce_hub::desk_floor::{router, Sources};
use workforce_hub::floor::postgres::PgFloorStore;
use workforce_hub::serve::{AgentService, Compose};
use workforce_hub::session_push::SessionPush;
use workforce_postgres::test_support::with_test_pool;

use support::{token_for, Gateway};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A model that answers by the words of the person's last message: one
/// that asks a question, one that posts to Slack, and a plain answer.
struct Scripted;

fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    let body = json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}});
    format!("data: {body}\n\n")
}

impl Respond for Scripted {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let last = messages.last().cloned().unwrap_or_default();
        let asked = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(text_of)
            .unwrap_or_default();
        let tool = |name: &str, args: Value| {
            body["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["function"]["name"].as_str())
                .find(|n| *n == name)
                .map(|n| (n.to_string(), args))
        };
        let call = match (last["role"].as_str(), asked.as_str()) {
            (Some("tool"), _) => None,
            (_, a) if a.contains("E2E-SLACK") => tool(
                "messaging_send",
                json!({"platform": "slack", "channel": "#ops", "text": "deploy done"}),
            ),
            _ => None,
        };
        let text = match (last["role"].as_str(), asked.as_str()) {
            (Some("tool"), _) => "Posted.".to_string(),
            (_, a) if a.contains("E2E-ASK") => "Built it. Deploy to staging?".to_string(),
            _ => "Noted.".to_string(),
        };
        let sse = match call {
            Some((name, args)) => [
                chunk(
                    json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_{}", Uuid::new_v4().simple()),
                        "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ]
            .concat(),
            None => [
                chunk(json!({"role": "assistant", "content": text}), None),
                chunk(json!({}), Some("stop")),
            ]
            .concat(),
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!("{sse}data: [DONE]\n\n"))
    }
}

async fn scripted_agent(dir: &std::path::Path) -> (Arc<AgentService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(Scripted)
        .mount(&server)
        .await;
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = dir.to_string_lossy().to_string();
    config.memory.db_path = dir.join("memory.db").to_string_lossy().into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    let service = AgentService::from_config(config, None)
        .await
        .expect("an agent on the scripted model");
    (service, server)
}

async fn connect(addr: SocketAddr, path: &str) -> Socket {
    tokio_tungstenite::connect_async(format!("ws://{addr}{path}"))
        .await
        .expect("websocket connects")
        .0
}

async fn next(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .expect("a readable frame");
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

/// Frames until one of type `kind`, which is returned with those before it.
async fn until(socket: &mut Socket, kind: &str) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = next(socket).await;
        let done = frame["type"] == kind;
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(tungstenite::Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// A message from the person's Desk DM, which answers prompts.
fn said(text: &str, turn_ref: &str) -> Value {
    json!({"type": "chat", "content": text, "turn_ref": turn_ref, "answers_approvals": true,
           "desk": {"place": {"kind": "dm"}, "speaker": {"kind": "person"}}})
}

struct Floor {
    addr: SocketAddr,
    token: String,
}

impl Floor {
    async fn read(&self) -> Value {
        let day = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let response = reqwest::Client::new()
            .get(format!("http://{}/api/desk/floor", self.addr))
            .query(&[("day", day)])
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        response.json().await.unwrap()
    }

    /// The needs, once `ready` says they are as the test expects.
    async fn needs_once(&self, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        for _ in 0..200 {
            let floor = self.read().await;
            let needs = floor["needs"].as_array().cloned().unwrap_or_default();
            if ready(&needs) {
                return needs;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the floor never showed it: {}", self.read().await);
    }

    async fn decide(&self, at: &Value, choice: &str) -> (u16, Value) {
        let response = reqwest::Client::new()
            .post(format!("http://{}/api/desk/needs/decide", self.addr))
            .bearer_auth(&self.token)
            .json(&json!({"at": at, "choice": choice}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

/// The link says the floor changed, within a moment.
async fn signalled(link: &mut Socket) {
    loop {
        if next(link).await["type"] == "floor_changed" {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persons_floor_follows_their_agent_and_is_answered_from_it() {
    let pg = std::sync::OnceLock::new();
    with_test_pool(
        |config| {
            let _ = pg.set(config.clone());
            PgFloorStore::build_pool(config)
        },
        |_| run(pg.get().cloned().expect("the test database")),
    )
    .await;
}

async fn run(pg: workforce_postgres::PostgresConfig) {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.postgres = Some(pg);
    workforce_hub::floor::install(&config).await;
    workforce_hub::surface_allows::install(&config).await;

    let dir = tempfile::tempdir().unwrap();
    let (scout, _model) = scripted_agent(dir.path()).await;
    let pending: Arc<dyn PendingApprovalStore> =
        Arc::new(SqlitePendingApprovalStore::in_memory().unwrap());
    scout.set_pending_approval_store(Arc::clone(&pending));
    let agent = scout.agent_id.to_string();
    let gateway = Gateway::new(vec![(agent.as_str(), scout)], None);
    workforce_hub::session_push::install(SessionPush::new(
        None,
        gateway.state.broadcast_tx.clone(),
    ));
    let person = gateway.admin.id.to_string();
    let token = token_for(&gateway.admin);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = workforce_hub::gateway::router(Arc::clone(&gateway.state))
        .merge(router(Arc::clone(&gateway.state), Sources::installed()))
        .merge(workforce_hub::surface_allows::routes(Arc::clone(
            &gateway.state,
        )));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let floor = Floor {
        addr,
        token: token.clone(),
    };

    let mut link = connect(addr, &format!("/ws/desk?token={token}")).await;
    let hello = next(&mut link).await;
    assert_eq!(hello["type"], "hello");
    let features: Vec<&str> = hello["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(features.contains(&"desk.floor") && features.contains(&"desk.usage"));

    let mut desk = connect(addr, &format!("/ws?token={token}&channel=desk&desk=1")).await;
    assert_eq!(next(&mut desk).await["type"], "connected");

    // What the agent asks needs the person, where they asked it.
    send(&mut desk, said("E2E-ASK the build", "ref-ask")).await;
    until(&mut desk, "turn_end").await;
    signalled(&mut link).await;
    let needs = floor.needs_once(|n| n.len() == 1).await;
    assert_eq!(needs[0]["kind"], "question");
    assert_eq!(needs[0]["ask"], "Deploy to staging?");
    assert_eq!(needs[0]["place"], json!({"kind": "dm"}));
    assert_eq!(needs[0]["answer"], json!({"kind": "reply"}));

    // Answered from another of their devices, it needs them no more.
    let mut phone = connect(addr, &format!("/ws?token={token}&channel=desk")).await;
    assert_eq!(next(&mut phone).await["type"], "connected");
    send(&mut phone, json!({"type": "chat", "content": "not yet"})).await;
    until(&mut phone, "stream_end").await;
    signalled(&mut link).await;
    floor.needs_once(<[Value]>::is_empty).await;

    // A post off Desk waits on them, answered from the floor.
    send(&mut desk, said("E2E-SLACK tell ops", "ref-post")).await;
    let prompt = until(&mut desk, "permission_prompt").await.pop().unwrap();
    let needs = floor
        .needs_once(|n| n.iter().any(|need| need["kind"] == "approval"))
        .await;
    let need = needs.iter().find(|n| n["kind"] == "approval").unwrap();
    let at = need["answer"]["at"].clone();
    assert_eq!(at["kind"], "prompt");
    assert_eq!(at["agentId"], json!(agent));
    assert_eq!(at["requestId"], prompt["request_id"]);
    assert_eq!(floor.decide(&at, "allow").await.0, 204);
    until(&mut desk, "turn_end").await;
    floor
        .needs_once(|n| n.iter().all(|need| need["kind"] != "approval"))
        .await;
    assert_eq!(
        floor.decide(&at, "allow").await,
        (409, json!({"error": "already_answered"}))
    );

    // Always allowed in the conversation, the next post goes unasked.
    send(&mut desk, said("E2E-SLACK tell ops again", "ref-always")).await;
    let prompt = until(&mut desk, "permission_prompt").await.pop().unwrap();
    assert_eq!(prompt["prompt_options"]["deny_always"], false);
    send(
        &mut desk,
        json!({"type": "permission_response", "request_id": prompt["request_id"],
               "choice": "allow_always"}),
    )
    .await;
    until(&mut desk, "turn_end").await;
    send(&mut desk, said("E2E-SLACK and once more", "ref-unasked")).await;
    let frames = until(&mut desk, "turn_end").await;
    assert!(
        frames.iter().all(|f| f["type"] != "permission_prompt"),
        "an always-allowed destination is not asked about: {frames:?}"
    );
    let allows: Value = reqwest::Client::new()
        .get(format!("http://{addr}/api/desk/allows"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let allows = allows["allows"].as_array().unwrap();
    assert_eq!(allows.len(), 1, "{allows:?}");
    assert_eq!(
        (
            &allows[0]["agentId"],
            &allows[0]["platform"],
            &allows[0]["destination"]
        ),
        (&json!(agent), &json!("slack"), &json!("ops"))
    );

    // A routed approval waiting on them is theirs to decide on the floor.
    let request = Uuid::new_v4().to_string();
    pending
        .insert_core(
            &workforce_agent::runner::PendingApprovalCore {
                request_id: request.clone(),
                agent_id: agent.clone(),
                session_key: "scheduler:j1".into(),
                channel_id: String::new(),
                requester_user_id: person.clone(),
                tool_name: "slack.send".into(),
                tool_input: json!({"channel": "#ops", "text": "weekly"}),
                reason: "Posts outside Desk".into(),
                allow_self_approval: true,
                approver_user_ids: vec![person.clone()],
                ttl: Duration::from_secs(3600),
            },
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .unwrap();
    pending.add_target(&request, &person, "").unwrap();
    let needs = floor.needs_once(|n| !n.is_empty()).await;
    let at = needs[0]["answer"]["at"].clone();
    assert_eq!(at, json!({"kind": "routed", "requestId": request}));
    assert_eq!(floor.decide(&at, "deny").await.0, 204);
    signalled(&mut link).await;
    floor.needs_once(<[Value]>::is_empty).await;
}

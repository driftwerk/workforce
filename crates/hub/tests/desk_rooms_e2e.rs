//! End to end: a room on the hub's own desk, driven through `/ws` and the
//! desk's routes as Desk drives them, with Allan, a local agent, thinking on
//! a scripted model, and Atlas answering from another hub. Allan's saved
//! answer says which message it answers and the tools its turn ran; posted
//! into the room it is for you, its thread names him, and it is unread
//! until its point passes it. Another hub's answer is Activity's, Allan's
//! turn is there once, a copy to the room is kept once, and search and
//! paging find his reply.
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
use workforce_common::locking::LockManager;
use workforce_common::settings::AgentConfig;
use workforce_hub::serve::{AgentService, Compose};
use workforce_hub::subscription::Subscriptions;
use workforce_postgres::test_support::with_test_pool;

use support::{token_for, Gateway};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The tool Allan's turn runs, twice, before it answers: as the model calls
/// it, and as its step names it.
const LOOKUP: &str = "identity_whoami";
const STEP: &str = "identity.whoami";

/// A model that, asked about the launch, looks twice and then answers.
struct Scripted;

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
        let looked = messages
            .iter()
            .rev()
            .take_while(|m| m["role"] != "user")
            .filter(|m| m["role"] == "tool")
            .count();
        let offered = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["function"]["name"] == LOOKUP);
        let sse = match (looked, offered) {
            (0 | 1, true) => [
                chunk(
                    json!({"role": "assistant", "tool_calls": [{"index": 0,
                        "id": format!("call_{}", Uuid::new_v4().simple()), "type": "function",
                        "function": {"name": LOOKUP,
                            "arguments": "{}"}}]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ]
            .concat(),
            _ => [
                chunk(
                    json!({"role": "assistant", "content": "The launch page is ready."}),
                    None,
                ),
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
    let subscriptions = Subscriptions::boot(&config, None);
    let service = AgentService::build_for_agent(
        config,
        None,
        Some(&AgentConfig {
            id: "allan".into(),
            name: Some("Allan".into()),
            ..AgentConfig::default()
        }),
        None,
        Arc::new(LockManager::new()),
        None,
        None,
        None,
        None,
        subscriptions,
    )
    .await
    .expect("Allan, on the scripted model");
    (service, server)
}

async fn next(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
        .await
        .expect("a frame in time")
        .expect("a frame")
        .expect("a readable frame");
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

struct Hub {
    addr: SocketAddr,
    token: String,
}

impl Hub {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let request = reqwest::Client::new()
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token);
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

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, 200, "{path}: {body}");
        body
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// Activity's events about `agent`, once `ready` says they are as the
    /// test expects.
    async fn activity_once(&self, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        for _ in 0..200 {
            let page = self.get("/api/desk/activity?limit=100").await;
            let events = page["events"].as_array().cloned().unwrap_or_default();
            if ready(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "Activity never showed it: {}",
            self.get("/api/desk/activity?limit=100").await
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_room_answer_says_what_it_answers_and_is_read_searched_copied_and_paged() {
    let pg = std::sync::OnceLock::new();
    with_test_pool(
        |config| {
            let _ = pg.set(config.clone());
            workforce_desk::store::build_pool(config)
        },
        |_| run(pg.get().cloned().expect("the test database")),
    )
    .await;
}

async fn run(pg: workforce_postgres::PostgresConfig) {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.postgres = Some(pg);
    config.desk.enabled = true;
    config.desk.adopt_interval_secs = 0;
    config.desk.restore = workforce_common::settings::DeskRestore::Off;
    let dir = tempfile::tempdir().unwrap();
    let (allan, _model) = scripted_agent(dir.path()).await;
    let gateway = Gateway::new(vec![("allan", allan)], None);
    let token = token_for(&gateway.admin);
    let app = workforce_hub::desk_driver::mount(
        workforce_hub::gateway::router(Arc::clone(&gateway.state)),
        &gateway.state,
        &config,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the hub's desk");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let hub = Hub {
        addr,
        token: token.clone(),
    };
    // Desk's first summary sets the floor: what comes after it is counted.
    hub.get("/api/desk/unread").await;
    tokio::time::sleep(Duration::from_millis(5)).await;

    // 1. "Atlas launch" with both, and your ask in a thread, to Allan.
    let (status, room) = hub
        .post(
            "/api/desk/rooms",
            json!({"name": "Atlas launch", "members": [
                {"hub": "local", "agent": "allan"}, {"hub": "atlas-hub", "agent": "atlas"}]}),
        )
        .await;
    assert_eq!(status, 200, "{room}");
    let room_id = room["id"].as_str().unwrap().to_string();
    let messages = format!("/api/desk/rooms/{room_id}/messages");
    let (_, plan) = hub
        .post(
            &messages,
            json!({"author": "you", "content": "Atlas launch: what's left?"}),
        )
        .await;
    let (_, ask) = hub
        .post(
            &messages,
            json!({"author": "you", "content": "@allan is the launch page ready?",
                   "threadId": plan["id"]}),
        )
        .await;
    let ask_id = ask["id"].as_str().unwrap().to_string();
    let channel = room["channel"].as_str().unwrap().to_string();
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?token={token}&agent_id=allan&channel={channel}&desk=1"
    ))
    .await
    .expect("websocket connects");
    assert_eq!(next(&mut socket).await["type"], "connected");
    socket
        .send(tungstenite::Message::Text(
            json!({"type": "chat", "content": "is the launch page ready?", "turn_ref": "ref-room",
                   "desk": {"place": {"kind": "room", "room": room_id, "name": "Atlas launch",
                                      "thread": plan["id"]},
                            "speaker": {"kind": "person"}, "message": ask_id}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    loop {
        if next(&mut socket).await["type"] == "turn_end" {
            break;
        }
    }

    // 2. Allan's saved answer names your ask and his two steps; drained into
    // the room it keeps both, is for you, and his name leads the thread's.
    let said = hub
        .get(&format!(
            "/api/desk/conversations/allan?channel={}",
            urlencode(&channel)
        ))
        .await;
    let answer = said["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["from"] == "agent")
        .cloned()
        .unwrap_or_else(|| panic!("Allan answered: {said}"));
    let mark = &answer["mark"];
    assert_eq!(mark["in_reply_to"], json!(ask_id), "{answer}");
    let steps = mark["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("his turn's steps: {answer}"));
    assert_eq!(steps.len(), 2, "{answer}");
    assert!(steps.iter().all(|s| s["tool"] == STEP), "{answer}");
    let (status, reply) = hub
        .post(
            &messages,
            json!({"author": "local/allan", "content": answer["text"], "threadId": plan["id"],
                   "source": format!("local/allan/{}", answer["id"].as_str().unwrap()),
                   "inReplyTo": mark["in_reply_to"], "steps": mark["steps"]}),
        )
        .await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(
        (
            &reply["inReplyTo"],
            &reply["forYou"],
            reply["steps"].as_array().unwrap().len()
        ),
        (&json!(ask_id), &json!(true), 2)
    );
    let reply_id = reply["id"].as_str().unwrap().to_string();
    let parent = hub
        .get(&format!("{messages}/{}", plan["id"].as_str().unwrap()))
        .await;
    assert_eq!(parent["replyAuthors"], json!(["local/allan", "you"]));

    // 3. Unread in its thread after a point before it; none past it.
    let thread_place = format!("thread:{room_id}/{}", plan["id"].as_str().unwrap());
    let put = |at: &Value, message: &Value| {
        json!({"puts": [{"place": thread_place, "point": {"at": at, "message": message},
                         "mark": "advance"}]})
    };
    let (status, _) = hub
        .call(
            reqwest::Method::PUT,
            "/api/desk/reads",
            Some(put(&ask["createdAt"], &ask["id"])),
        )
        .await;
    assert_eq!(status, 200);
    let unread = hub.get("/api/desk/unread").await;
    let summary = unread["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["roomId"] == json!(room_id))
        .cloned()
        .unwrap();
    assert_eq!(
        summary["threads"][0]["unread"]["forYou"],
        json!(1),
        "{summary}"
    );
    assert_eq!(summary["threads"][0]["parent"]["id"], plan["id"]);
    hub.call(
        reqwest::Method::PUT,
        "/api/desk/reads",
        Some(put(&reply["createdAt"], &reply["id"])),
    )
    .await;
    let unread = hub.get("/api/desk/unread").await;
    let summary = unread["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["roomId"] == json!(room_id))
        .cloned()
        .unwrap();
    assert_eq!(summary["threads"], json!([]), "{summary}");

    // 4. Atlas answers on its own hub, and Desk drains it into the room: a
    // `replied` row with the room's place. Allan's turn is in Activity once,
    // with its place, and his answer adds no row.
    let (status, atlas) = hub
        .post(
            &messages,
            json!({"author": "atlas-hub/atlas", "content": "The copy is signed off.",
                   "threadId": plan["id"], "source": "atlas-hub/atlas/m-1",
                   "inReplyTo": ask_id}),
        )
        .await;
    assert_eq!(status, 201, "{atlas}");
    let place = json!({"kind": "room", "room": room_id, "thread": plan["id"]});
    let events = hub
        .activity_once(|events| {
            events.iter().any(|e| e["kind"] == "replied")
                && events
                    .iter()
                    .any(|e| e["subject"]["id"] == "allan" && e["kind"] != "started")
        })
        .await;
    let replied: Vec<_> = events.iter().filter(|e| e["kind"] == "replied").collect();
    assert_eq!(replied.len(), 1, "{events:?}");
    assert_eq!(
        (
            &replied[0]["place"],
            &replied[0]["message"],
            &replied[0]["actor"]["id"]
        ),
        (&place, &atlas["id"], &json!("atlas-hub/atlas"))
    );
    let allans: Vec<_> = events
        .iter()
        .filter(|e| e["subject"]["id"] == "allan")
        .collect();
    assert_eq!(
        allans.iter().filter(|e| e["kind"] == "started").count(),
        1,
        "{allans:?}"
    );
    assert!(
        allans.iter().all(|e| e["place"] == place),
        "every row of Allan's turn opens the thread: {allans:?}"
    );
    assert!(
        allans
            .iter()
            .all(|e| e["kind"] != "replied" && e["kind"] != "delivered"),
        "a local agent's answer is its turn's, not a row of its own: {allans:?}"
    );

    // 5. Copied to the room once: in the timeline, at the last hop.
    let copy = format!("{messages}/{reply_id}/copy");
    let (status, copied) = hub.post(&copy, Value::Null).await;
    assert_eq!(status, 201, "{copied}");
    assert_eq!(
        (&copied["copyOf"], copied.get("threadId"), &copied["forYou"]),
        (&json!(reply_id), Some(&Value::Null), &json!(false))
    );
    assert_eq!(copied["hop"], json!(workforce_desk::rooms::MAX_HOPS));
    let (status, again) = hub.post(&copy, Value::Null).await;
    assert_eq!((status, &again["id"]), (200, &copied["id"]));
    let reply_now = hub.get(&format!("{messages}/{reply_id}")).await;
    assert_eq!(reply_now["postedToRoom"], json!(true));

    // 6. Search: Allan, in threads, finds his reply, its match bracketed.
    let hits = hub
        .get(&format!(
            "/api/desk/rooms/search?q=launch&from=local%2Fallan&inThread=true&room={room_id}"
        ))
        .await;
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["message"]["id"], json!(reply_id));
    let snippet = hits[0]["snippet"].as_str().unwrap();
    assert!(snippet.contains("\u{E000}launch\u{E001}"), "{snippet:?}");

    // 7. The thread paged around his reply holds your ask before it.
    let page = hub
        .get(&format!(
            "{messages}?thread={}&around={reply_id}&limit=3",
            plan["id"].as_str().unwrap()
        ))
        .await;
    let ids: Vec<&str> = page
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    let at = ids
        .iter()
        .position(|id| *id == reply_id)
        .expect("his reply");
    assert!(at > 0 && ids[at - 1] == ask_id, "{ids:?}");
}

fn urlencode(s: &str) -> String {
    s.replace(':', "%3A")
}

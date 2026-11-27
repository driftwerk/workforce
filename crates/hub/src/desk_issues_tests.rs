//! A devbox session's Linear, end to end and in process: a Mac hub whose
//! VEL board is linked to Linear through a fake Atlas (the real remote MCP
//! router on a local port, with a stand-in Linear that keeps what it is
//! told), a devbox hub with its own session, and a courier that carries
//! calls between their real routes as Desk's native side does. No real
//! Linear, hub or machine is ever called.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_desk::board::{Filter, System, Via};
use workforce_desk::{Desk, LinkBoard};
use workforce_postgres::test_support::with_test_pool;

use super::*;
use crate::desk_board_sync::tests::{board, local_hub, serve_remote, states, LocalHub};
use crate::desk_crew::tests::{desk, eventually, machine_desk, new_session};
use crate::remote_mcp::testing::{fixture, with_agents};

const VEL_ID: &str = "8a7b6c5d-0000-4000-8000-0000000000aa";
const STEP: Option<Duration> = Some(Duration::from_secs(25));
const MOVES: [(&str, &str); 3] = [("s1", "Todo"), ("s2", "In Progress"), ("s3", "In Review")];

/// What the fake Linear was told, and where VEL-15 is.
#[derive(Default)]
struct Linear {
    calls: Vec<(String, Value)>,
    comments: Vec<String>,
    state: String,
}

type Shared = Arc<Mutex<Linear>>;

struct FakeLinear {
    name: &'static str,
    linear: Shared,
}

#[async_trait::async_trait]
impl Tool for FakeLinear {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a fake Linear tool"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        let mut l = self.linear.lock().unwrap();
        l.calls.push((self.name.to_string(), input.clone()));
        let issue = |l: &Linear| json!({ "identifier": "VEL-15", "title": "Speed up recall", "state": { "name": l.state } });
        let out = match self.name {
            "linear.issue_list" => json!({ "issues": [issue(&l)] }),
            "linear.issue_get" => json!({ "issue": issue(&l) }),
            "linear.comment_list" => {
                json!({ "comments": l.comments.iter().map(|b| json!({ "body": b })).collect::<Vec<_>>() })
            }
            "linear.comment_add" => {
                let body = input["body"].as_str().unwrap_or_default().to_string();
                l.comments.push(body);
                json!({ "comment": { "id": format!("c-{}", l.comments.len()) } })
            }
            "linear.states_list" => json!({ "states": MOVES
                .iter()
                .map(|(id, name)| json!({ "id": id, "name": name }))
                .collect::<Vec<_>>() }),
            "linear.issue_update" => {
                let to = input["state_id"].as_str().unwrap_or_default();
                if let Some((_, name)) = MOVES.iter().find(|(id, _)| *id == to) {
                    l.state = name.to_string();
                }
                json!({ "issue": issue(&l) })
            }
            "linear.teams_list" => {
                json!({ "teams": [{ "id": VEL_ID, "key": "VEL", "name": "Vellev" }] })
            }
            _ => json!({}),
        };
        Ok(ToolResult::success(out, 0))
    }
}

/// Atlas, a hub with Linear: its remote MCP on a local port.
async fn atlas(dir: &std::path::Path) -> (crate::desk_board_sync::tests::RemoteHub, Shared) {
    let fx = fixture(true);
    with_agents(&fx, dir, &["default"]).await;
    let linear: Shared = Arc::new(Mutex::new(Linear {
        state: "Todo".into(),
        ..Default::default()
    }));
    let service = fx.state.registry.default_agent().unwrap();
    for name in workforce_desk::board_sync::LINEAR_TOOLS
        .iter()
        .chain(&["linear.comment_list"])
    {
        service.tools.register(Arc::new(FakeLinear {
            name,
            linear: Arc::clone(&linear),
        }));
    }
    let remote = serve_remote(fx, &["linear.*"], Arc::default(), None).await;
    (remote, linear)
}

/// The Mac: a VEL board linked to Atlas's Linear, as Sam.
async fn mac(desk: &Desk, dir: &std::path::Path) -> (LocalHub, Shared) {
    let (remote, linear) = atlas(&dir.join("atlas")).await;
    let local = local_hub(desk.clone(), dir);
    let (status, body) = local.link_atlas(&remote).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let b = board(desk, "Vellev").await;
    desk.link_board(
        b.id,
        LinkBoard {
            system: System::Linear,
            repo: None,
            target: "VEL".into(),
            target_id: Some(VEL_ID.into()),
            via: Via::Hub {
                hub: "atlas".into(),
            },
            agent: None,
            workspace: None,
            filter: Filter::All {
                include_done: false,
            },
            me: None,
            me_name: None,
            me_source: None,
            me_note: None,
            not_me: false,
            keep_tickets: false,
            states: states(),
        },
        &local.person.id.to_string(),
    )
    .await
    .unwrap();
    (local, linear)
}

/// The issue routes of a computer whose calls go by `carrier`, as a
/// listener serves them.
fn issue_app(state: &Arc<GatewayState>, carrier: Carrier) -> axum::Router {
    crate::serve::combined(routes(Arc::clone(state), Bound::To(carrier)))
}

async fn send(
    app: &axum::Router,
    uri: &str,
    bearer: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json");
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Desk's native side: each call the devbox queued, handed to the Mac as
/// devbox's, and the Mac's answer posted back. What it handed over is kept.
fn courier(
    relay: axum::Router,
    served: axum::Router,
    token: String,
    carried: Arc<Mutex<Vec<Value>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (_, calls) = send(&relay, "/api/desk/issues/relay", &token, None).await;
            for c in calls.as_array().cloned().unwrap_or_default() {
                let body = json!({
                    "id": c["id"], "machine": "devbox", "system": c["system"],
                    "session": c["session"], "sessionId": c["sessionId"], "input": c["input"],
                });
                carried.lock().unwrap().push(body.clone());
                let (_, answer) =
                    send(&served, "/api/desk/issues/served", &token, Some(body)).await;
                let id = c["id"].as_str().unwrap_or_default();
                send(
                    &relay,
                    &format!("/api/desk/issues/relay/{id}"),
                    &token,
                    Some(answer),
                )
                .await;
            }
        }
    })
}

fn told(linear: &Shared, tool: &str) -> Vec<Value> {
    linear
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(t, _)| t == tool)
        .map(|(_, a)| a.clone())
        .collect()
}

/// A devbox session lists, reads, comments on and moves its Linear issue
/// through Desk on the Mac, signed there as Desk on Sam's behalf; a call
/// served once is never served again; and Jira, linked nowhere, says so.
#[tokio::test]
async fn a_devbox_session_works_its_linear_issue_through_the_mac() {
    with_test_pool(workforce_desk::store::build_pool, |mac_pool| async move {
        with_test_pool(
            workforce_desk::store::build_pool,
            |devbox_pool| async move {
                devbox_works_its_issue(mac_pool, devbox_pool).await;
            },
        )
        .await;
    })
    .await;
}

async fn devbox_works_its_issue(
    mac_pool: workforce_postgres::PostgresPool,
    devbox_pool: workforce_postgres::PostgresPool,
) {
    let (mac_dir, devbox_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let home_desk = desk(mac_pool, mac_dir.path()).await;
    let (local, linear) = mac(&home_desk, mac_dir.path()).await;
    let home = HomeIssues::new(Arc::clone(&local.sync));
    let served = issue_app(&local.sync.state, Carrier::Home(Arc::clone(&home)));

    let there = machine_desk(devbox_pool, devbox_dir.path()).await;
    let machine = MachineIssues::new(Arc::default());
    there.issues_with(Arc::clone(&machine) as _);
    let devbox_state = fixture(false).state;
    let relay = issue_app(&devbox_state, Carrier::Machine(Arc::clone(&machine)));
    let session = new_session(&there, devbox_dir.path(), "vel-15").await;
    let token = devbox_state.auth_token.clone();
    assert_eq!(token, local.sync.state.auth_token);

    let carried: Arc<Mutex<Vec<Value>>> = Arc::default();
    let poller = courier(
        relay.clone(),
        served.clone(),
        token.clone(),
        Arc::clone(&carried),
    );
    eventually("Desk polling", async || {
        machine.pickup.polling().then_some(())
    })
    .await;
    let call = |input: Value| {
        let there = there.clone();
        async move { there.line_call(session.id, "linear", &input, STEP).await }
    };

    let listed = call(json!({ "action": "list" })).await.unwrap();
    let issues = listed["issues"].as_str().unwrap();
    assert!(
        issues.contains("VEL-15") && issues.contains("not the person's words"),
        "{issues}"
    );
    assert_eq!(told(&linear, "linear.issue_list")[0]["team_id"], VEL_ID);

    let got = call(json!({ "action": "get", "key": "vel-15" }))
        .await
        .unwrap();
    assert!(got["issue"].as_str().unwrap().contains("Speed up recall"));
    assert_eq!(told(&linear, "linear.issue_get")[0]["identifier"], "VEL-15");

    let said = call(json!({ "action": "comment", "key": "VEL-15", "text": "PR 2 is up." }))
        .await
        .unwrap();
    assert!(said["done"]
        .as_str()
        .unwrap()
        .starts_with("Commented on VEL-15"));
    let signed = "Desk on behalf of Sam (the vel-15 session on devbox)";
    assert_eq!(
        told(&linear, "linear.comment_add")[0]["body"],
        format!("{signed}:\n\nPR 2 is up.")
    );

    let moved = call(json!({ "action": "move", "key": "VEL-15", "state": "in review" }))
        .await
        .unwrap();
    assert!(moved["done"]
        .as_str()
        .unwrap()
        .starts_with("Moved VEL-15 to In Review"));
    assert_eq!(
        told(&linear, "linear.issue_update")[0],
        json!({ "identifier": "VEL-15", "state_id": "s3" })
    );
    assert_eq!(linear.lock().unwrap().state, "In Review");
    assert_eq!(
        linear.lock().unwrap().comments[1],
        format!("{signed}:\n\nmoved this to In Review.")
    );
    let got = call(json!({ "action": "get", "key": "VEL-15" }))
        .await
        .unwrap();
    assert!(got["comments"].as_str().unwrap().contains("PR 2 is up."));

    // A bad call is refused on the devbox, before anything is carried.
    let before = carried.lock().unwrap().len();
    let e = call(json!({ "action": "move", "key": "VEL-15" }))
        .await
        .unwrap_err();
    assert!(e.contains("which state"), "{e}");
    assert_eq!(carried.lock().unwrap().len(), before);

    // A key of a team no board it reaches is linked to is refused, never
    // sent through another team's link.
    let e = call(json!({ "action": "get", "key": "OPS-3" }))
        .await
        .unwrap_err();
    assert!(
        e.contains("OPS isn't linked to a board this session reaches"),
        "{e}"
    );
    assert!(told(&linear, "linear.issue_get")
        .iter()
        .all(|a| a["identifier"] != "OPS-3"));

    // Jira is linked to no board on the Mac: said, through the same path.
    let e = there
        .line_call(session.id, "jira", &json!({ "action": "list" }), STEP)
        .await
        .unwrap_err();
    assert!(
        e.contains("No board this session reaches in Desk is linked to Jira"),
        "{e}"
    );

    // Put in a group on the Mac, the session reaches only that group's
    // boards, as the ticket tool does: Vellev's link isn't its.
    let item = format!("session:devbox/{}", session.id);
    home_desk
        .change_layout(|l| l.create("g-acme", "Acme", std::slice::from_ref(&item), None))
        .await
        .unwrap();
    let e = call(json!({ "action": "list" })).await.unwrap_err();
    assert!(
        e.contains("No board this session reaches in Desk is linked to Linear"),
        "{e}"
    );

    // The same call handed over again is never served twice.
    let first = carried.lock().unwrap()[0].clone();
    let (status, _) = send(&served, "/api/desk/issues/served", &token, Some(first)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(told(&linear, "linear.issue_list").len(), 1);

    // Nor by anyone but Desk itself.
    let (status, _) = send(
        &served,
        "/api/desk/issues/served",
        "someone-else",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    poller.abort();

    // With no Desk carrying, a session hears so at once.
    let idle = MachineIssues::new(Arc::default());
    let started = Instant::now();
    let asker = Asker {
        session: session.id,
        name: "vel-15".into(),
    };
    let asked = Asked {
        system: System::Linear,
        input: json!({ "action": "get", "key": "VEL-15" }),
    };
    let e = idle.issue(&asker, &asked).await.unwrap_err();
    assert!(
        e.contains("isn't connected to this machine right now"),
        "{e}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // Desk reaching the machine by its other routes, and never asking for
    // these calls, is an older Desk.
    let seen = Arc::new(crate::desk_crew::relay::NativeSeen::default());
    seen.mark();
    let e = MachineIssues::new(seen)
        .issue(&asker, &asked)
        .await
        .unwrap_err();
    assert!(e.contains("needs an update"), "{e}");
}

/// Only Desk's native side, with the hub's own token in its header, reaches
/// either end; the Mac takes a call only for a machine and session it can
/// name, and its answer only once.
#[tokio::test]
async fn only_desk_itself_carries_the_calls() {
    let state = fixture(false).state;
    let token = state.auth_token.clone();
    let machine = MachineIssues::new(Arc::default());
    let relay = issue_app(&state, Carrier::Machine(Arc::clone(&machine)));
    for (bearer, uri) in [
        ("someone-else", "/api/desk/issues/relay"),
        (token.as_str(), "/api/desk/issues/relay?token=x"),
    ] {
        let (status, _) = send(&relay, uri, bearer, None).await;
        assert!(
            matches!(status, StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST),
            "{uri}: {status}"
        );
    }
    let id = uuid::Uuid::new_v4();
    let (status, _) = send(
        &relay,
        &format!("/api/desk/issues/relay/{id}"),
        &token,
        Some(json!({ "ok": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    let (status, _) = send(
        &relay,
        &format!("/api/desk/issues/relay/{id}"),
        &token,
        Some(json!({ "neither": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::to_value(Answer::Error("no".into())).unwrap(),
        json!({ "error": "no" })
    );
}

/// On a machine, any request Desk's native side makes, with the hub's own
/// token, is seen; anyone else's isn't.
#[tokio::test]
async fn desk_reaching_a_machine_by_any_route_is_seen() {
    let seen = Arc::new(crate::desk_crew::relay::NativeSeen::default());
    let app = crate::serve::combined(crate::desk_crew::relay::noting_native(
        Routes::new()
            .member("/api/desk/sessions", get(|| async { "[]" }))
            .member("/api/me", get(|| async { "{}" })),
        "the-hubs-token".into(),
        Arc::clone(&seen),
    ));
    let (status, _) = send(&app, "/api/desk/sessions", "someone-else", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!seen.within(Duration::from_secs(60)));
    // Desk Helper's bootstrap carries the same token and isn't Desk's
    // native side.
    send(&app, "/api/me", "the-hubs-token", None).await;
    assert!(!seen.within(Duration::from_secs(60)));
    send(&app, "/api/desk/sessions", "the-hubs-token", None).await;
    assert!(seen.within(Duration::from_secs(60)));
}

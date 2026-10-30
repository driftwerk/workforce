//! `GET /api/desk/floor` and `POST /api/desk/needs/decide`: a person's own
//! floor, read for their local day, and nobody else's.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::crew::ItemQuestion;
use workforce_desk::floor::{Outcome, Place, Subject, Turn, TurnOp};
use workforce_hub::desk_floor::{router, Sources};
use workforce_hub::floor::postgres::PgFloorStore;
use workforce_hub::floor::Installed;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_postgres::test_support::with_test_pool;

use crate::support::{agent_service, token_for, Gateway};

fn turn(owner: &str, agent: &str, started: DateTime<Utc>) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        owner: Some(owner.into()),
        subject: Subject::Agent {
            agent_id: agent.into(),
            session_id: Uuid::new_v4().to_string(),
        },
        name: agent.into(),
        place: Place::Dm,
        task: Some("Compare spend.".into()),
        doing: None,
        started_at: started,
        heard_at: started,
        ended_at: None,
        outcome: None,
        answer: None,
        question: None,
        settled_at: None,
        waits: vec![],
        idle: vec![],
        token: None,
        instance: None,
    }
}

fn asks(t: &Turn, at: DateTime<Utc>) -> TurnOp {
    TurnOp::End {
        turn: t.id,
        at,
        outcome: Outcome::Asked,
        answer: Some("Spend is flat.".into()),
        question: Some(ItemQuestion {
            text: "Cut the idle nodes?".into(),
            options: None,
        }),
    }
}

async fn get(addr: std::net::SocketAddr, path: &str, bearer: Option<&str>) -> (u16, Value) {
    let request = reqwest::Client::new().get(format!("http://{addr}{path}"));
    let response = match bearer {
        Some(b) => request.bearer_auth(b),
        None => request,
    }
    .send()
    .await
    .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

fn day_path(day: DateTime<Utc>) -> String {
    format!(
        "/api/desk/floor?day={}",
        urlencode(&day.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    )
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_floor_is_the_callers_own_for_their_day() {
    with_test_pool(PgFloorStore::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let scout = agent_service(dir.path()).await;
        let gateway = Gateway::new(vec![("scout", scout)], None);
        let alice = gateway.admin.clone();
        let bob = Identity::new("Bob", IdentityRole::Member);
        gateway
            .state
            .identity_store
            .as_ref()
            .unwrap()
            .create(&bob)
            .unwrap();
        let installed = Installed::open(PgFloorStore::with_pool(pool).await.unwrap());
        let log = Arc::clone(&installed.log);
        let state = Arc::clone(&gateway.state);
        let addr = serve_floor(state, Some(Sources::new(installed, Arc::new(|| None)))).await;

        let now = Utc::now();
        let alice_id = alice.id.to_string();
        let asked = turn(&alice_id, "scout", now - chrono::Duration::minutes(30));
        let running = Turn {
            instance: Some(workforce_common::instance::process_instance_id().to_string()),
            ..turn(&alice_id, "scout", now - chrono::Duration::minutes(5))
        };
        let gone = Turn {
            token: Some("an-ended-turn".into()),
            ..turn(&alice_id, "scout", now - chrono::Duration::minutes(10))
        };
        let theirs = turn(
            &bob.id.to_string(),
            "scout",
            now - chrono::Duration::minutes(20),
        );
        for op in [
            TurnOp::Begin {
                turn: Box::new(asked.clone()),
                by_person: true,
            },
            asks(&asked, now - chrono::Duration::minutes(25)),
            TurnOp::Begin {
                turn: Box::new(running.clone()),
                by_person: false,
            },
            TurnOp::Begin {
                turn: Box::new(gone.clone()),
                by_person: false,
            },
            TurnOp::Begin {
                turn: Box::new(theirs.clone()),
                by_person: true,
            },
            asks(&theirs, now - chrono::Duration::minutes(15)),
        ] {
            log.send(op);
        }

        let day = now - chrono::Duration::hours(2);
        let mine = token_for(&alice);
        let floor = eventually(async || {
            let (status, body) = get(addr, &day_path(day), Some(&mine)).await;
            (status == 200 && body["needs"].as_array().is_some_and(|n| n.len() == 1))
                .then_some(body)
        })
        .await;
        assert_eq!(floor["needs"][0]["id"], format!("question:{}", asked.id));
        assert_eq!(floor["needs"][0]["ask"], "Cut the idle nodes?");
        assert_eq!(
            (
                &floor["needs"][0]["who"]["kind"],
                &floor["needs"][0]["who"]["id"]
            ),
            (&json!("agent"), &json!("scout"))
        );
        assert_eq!(floor["needs"][0]["answer"], json!({"kind": "reply"}));
        assert_eq!(
            floor["working"].as_array().unwrap().len(),
            1,
            "a turn whose session moved on is not working: {floor}"
        );
        assert_eq!(floor["working"][0]["what"], "Compare spend.");
        assert_eq!(
            floor["counts"],
            json!({"working": 1, "needsYou": 1, "doneToday": 0})
        );
        assert_eq!(floor["more"], json!({"done": 0, "lanes": 0}));
        assert_eq!(floor["lanes"].as_array().unwrap().len(), 1);

        let (_, bobs) = get(addr, &day_path(day), Some(&token_for(&bob))).await;
        assert_eq!(bobs["needs"][0]["id"], format!("question:{}", theirs.id));
        assert_eq!(bobs["counts"]["needsYou"], 1, "never another person's");

        assert_eq!(get(addr, &day_path(day), None).await.0, 401);
        assert_eq!(get(addr, &day_path(day), Some("not-a-token")).await.0, 401);
        for bad in [
            "/api/desk/floor".to_string(),
            "/api/desk/floor?day=today".to_string(),
            day_path(now + chrono::Duration::hours(1)),
            day_path(now - chrono::Duration::days(3)),
        ] {
            let (status, body) = get(addr, &bad, Some(&mine)).await;
            assert_eq!((status, body), (400, json!({"error": "bad_day"})), "{bad}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_without_a_floor_says_so_and_a_session_need_is_not_a_remote_callers() {
    with_test_pool(PgFloorStore::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
        let mine = token_for(&gateway.admin);
        let none = serve_floor(Arc::clone(&gateway.state), None).await;
        let (status, body) = get(
            none,
            &day_path(Utc::now() - chrono::Duration::hours(1)),
            Some(&mine),
        )
        .await;
        assert_eq!((status, body), (404, json!({"error": "no_floor"})));

        let installed = Installed::open(PgFloorStore::with_pool(pool).await.unwrap());
        let addr = serve_floor(
            Arc::clone(&gateway.state),
            Some(Sources::new(installed, Arc::new(|| None))),
        )
        .await;
        let at =
            json!({"kind": "session", "sessionId": Uuid::new_v4(), "approvalId": Uuid::new_v4()});
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/api/desk/needs/decide"))
            .bearer_auth(&mine)
            .json(&json!({"at": at, "choice": "allow"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            403,
            "without a Desk, no session is theirs"
        );
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"error": "not_yours"})
        );
    })
    .await;
}

async fn decide(
    addr: std::net::SocketAddr,
    bearer: &str,
    at: &Value,
    choice: &str,
) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/desk/needs/decide"))
        .bearer_auth(bearer)
        .json(&json!({"at": at, "choice": choice}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_routed_approval_needs_its_approver_alone_and_is_decided_once() {
    use workforce_hub::approvals::{PendingApprovalStore, SqlitePendingApprovalStore};
    with_test_pool(PgFloorStore::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _model) = crate::support::agent_on_mock_llm(dir.path()).await;
        let store: Arc<dyn PendingApprovalStore> =
            Arc::new(SqlitePendingApprovalStore::in_memory().unwrap());
        scout.set_pending_approval_store(Arc::clone(&store));
        let gateway = Gateway::new(vec![("scout", scout)], None);
        let alice = gateway.admin.clone();
        let bob = Identity::new("Bob", IdentityRole::Member);
        gateway
            .state
            .identity_store
            .as_ref()
            .unwrap()
            .create(&bob)
            .unwrap();
        let installed = Installed::open(PgFloorStore::with_pool(pool).await.unwrap());
        let addr = serve_floor(
            Arc::clone(&gateway.state),
            Some(Sources::new(installed, Arc::new(|| None))),
        )
        .await;
        let request = Uuid::new_v4().to_string();
        store
            .insert_core(
                &workforce_agent::runner::PendingApprovalCore {
                    request_id: request.clone(),
                    agent_id: "scout".into(),
                    session_key: "scheduler:j1".into(),
                    channel_id: String::new(),
                    requester_user_id: alice.id.to_string(),
                    tool_name: "slack.send".into(),
                    tool_input: json!({"channel": "#ops", "text": "deploy done"}),
                    reason: "Posts outside Desk".into(),
                    allow_self_approval: true,
                    approver_user_ids: vec![alice.id.to_string()],
                    ttl: Duration::from_secs(3600),
                },
                None,
                None,
                Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        store
            .add_target(&request, &alice.id.to_string(), "")
            .unwrap();
        let path = day_path(Utc::now() - chrono::Duration::hours(1));
        let (status, floor) = get(addr, &path, Some(&token_for(&alice))).await;
        assert_eq!(status, 200);
        let needs = floor["needs"].as_array().unwrap();
        assert_eq!(needs.len(), 1, "{floor}");
        let at = json!({"kind": "routed", "requestId": request});
        assert_eq!(needs[0]["id"], json!(format!("routed:{request}")));
        assert_eq!(needs[0]["kind"], json!("approval"));
        assert_eq!(needs[0]["who"]["id"], json!("scout"));
        assert_eq!(needs[0]["place"], json!({"kind": "dm"}));
        assert_eq!(needs[0]["answer"], json!({"kind": "decide", "at": at}));
        assert_eq!(floor["counts"]["needsYou"], json!(1));

        let (_, theirs) = get(addr, &path, Some(&token_for(&bob))).await;
        assert_eq!(theirs["needs"], json!([]), "nobody else's to decide");
        assert_eq!(
            decide(addr, &token_for(&bob), &at, "allow").await,
            (403, json!({"error": "not_yours"}))
        );
        assert_eq!(decide(addr, &token_for(&alice), &at, "deny").await.0, 204);
        assert_eq!(
            decide(addr, &token_for(&alice), &at, "deny").await,
            (409, json!({"error": "already_answered"}))
        );
        let (_, after) = get(addr, &path, Some(&token_for(&alice))).await;
        assert_eq!(after["needs"], json!([]));
        assert_eq!(
            decide(
                addr,
                &token_for(&alice),
                &json!({"kind": "routed", "requestId": Uuid::new_v4().to_string()}),
                "allow"
            )
            .await,
            (404, json!({"error": "unknown"}))
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persons_plans_are_their_own_logins_and_any_replica_serves_the_newest() {
    use workforce_common::usage::{LoginRef, PlanKind, Reading, Window};
    with_test_pool(PgFloorStore::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
        let alice = gateway.admin.id.to_string();
        let writer = Installed::open(PgFloorStore::with_pool(pool.clone()).await.unwrap());
        let reader = Installed::open(PgFloorStore::with_pool(pool).await.unwrap());
        let reading = |login: &str, owner: Option<&str>, used: f64| Reading {
            kind: PlanKind::Claude,
            login: LoginRef::Hub {
                login: login.into(),
                owner: owner.map(str::to_string),
            },
            account: None,
            plan: None,
            windows: vec![Window {
                used_percent: used,
                resets_at: None,
                minutes: 300,
            }],
            observed_at: Utc::now(),
        };
        writer
            .store
            .record(&reading("theirs", Some(&alice), 62.0))
            .await
            .unwrap();
        writer
            .store
            .record(&reading("shared", None, 90.0))
            .await
            .unwrap();
        let addr = serve_floor(
            Arc::clone(&gateway.state),
            Some(Sources::new(reader, Arc::new(|| None))),
        )
        .await;
        let (status, usage) = get(addr, "/api/desk/usage", Some(&token_for(&gateway.admin))).await;
        assert_eq!(status, 200);
        assert_eq!(
            usage["readings"],
            json!([{"kind": "claude", "where": "hub", "account": null, "plan": null,
                "windows": [{"usedPercent": 62.0, "resetsAt": null, "minutes": 300}],
                "observedAt": usage["readings"][0]["observedAt"]}]),
            "a login the hub shares is not the person's plan"
        );
        assert_eq!(
            usage["logins"],
            json!([
                {"kind": "claude", "where": "hub", "signedIn": false},
                {"kind": "chatgpt", "where": "hub", "signedIn": false}
            ]),
            "no Mac on a hub that isn't theirs"
        );
        assert_eq!(get(addr, "/api/desk/usage", None).await.0, 401);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_sees_and_revokes_their_own_always_allows_alone() {
    use workforce_agent::tools::dispatch::SurfaceAllows;
    use workforce_agent::tools::Destination;
    use workforce_hub::surface_allows::PgSurfaceAllows;
    with_test_pool(PgSurfaceAllows::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
        let alice = gateway.admin.clone();
        let bob = Identity::new("Bob", IdentityRole::Member);
        gateway
            .state
            .identity_store
            .as_ref()
            .unwrap()
            .create(&bob)
            .unwrap();
        let store = Arc::new(PgSurfaceAllows::with_pool(pool).await.unwrap());
        let ops = Destination::of("slack", "#ops").unwrap();
        store
            .allow(&alice.id.to_string(), "scout", "slack.send", &ops)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = workforce_hub::surface_allows::router(Arc::clone(&gateway.state), Some(store));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (status, mine) = get(addr, "/api/desk/allows", Some(&token_for(&alice))).await;
        assert_eq!(status, 200);
        let allows = mine["allows"].as_array().unwrap();
        assert_eq!(allows.len(), 1);
        assert_eq!(
            (
                &allows[0]["agentId"],
                &allows[0]["tool"],
                &allows[0]["platform"],
                &allows[0]["destination"]
            ),
            (
                &json!("scout"),
                &json!("slack.send"),
                &json!("slack"),
                &json!("ops")
            )
        );
        let id = allows[0]["id"].as_str().unwrap().to_string();
        let (_, theirs) = get(addr, "/api/desk/allows", Some(&token_for(&bob))).await;
        assert_eq!(theirs, json!({"allows": []}));
        let revoke = |bearer: String| {
            let url = format!("http://{addr}/api/desk/allows/{id}");
            async move {
                reqwest::Client::new()
                    .delete(url)
                    .bearer_auth(bearer)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        assert_eq!(revoke(token_for(&bob)).await, 404, "not bob's to revoke");
        assert_eq!(revoke(token_for(&alice)).await, 204);
        let (_, after) = get(addr, "/api/desk/allows", Some(&token_for(&alice))).await;
        assert_eq!(after, json!({"allows": []}));
        assert_eq!(get(addr, "/api/desk/allows", None).await.0, 401);
    })
    .await;
}

async fn serve_floor(
    state: Arc<workforce_hub::gateway::GatewayState>,
    sources: Option<Sources>,
) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = workforce_hub::gateway::router(Arc::clone(&state)).merge(router(state, sources));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

async fn eventually<T>(mut check: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(found) = check().await {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the floor never showed it");
}

/// The floor's and the allow list's routes, mounted beside the gateway's,
/// answer with the security headers every gateway response carries and
/// refuse a body past its limit.
#[tokio::test]
async fn the_desk_routes_mounted_beside_the_gateway_are_hardened_like_it() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
    let app = workforce_hub::desk_driver::mount(
        axum::Router::new(),
        &gateway.state,
        &workforce_common::settings::WorkforceConfig::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    for path in ["/api/desk/allows", "/api/desk/floor"] {
        let response = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()["x-frame-options"],
            "DENY",
            "{path} carries the gateway's headers"
        );
    }
    let response = client
        .post(format!("http://{addr}/api/desk/needs/decide"))
        .header("content-type", "application/json")
        .body(vec![b' '; 2 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413, "a body past the gateway's limit");
}

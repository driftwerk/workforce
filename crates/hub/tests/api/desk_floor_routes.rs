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
        let running = turn(&alice_id, "scout", now - chrono::Duration::minutes(5));
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

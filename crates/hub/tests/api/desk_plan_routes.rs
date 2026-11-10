//! Plans are the person's to approve: a hub without Desk serves none of
//! their routes, and a boss's plan waiting on the Floor is approved there by
//! the person who holds the hub alone, as of the revision they read.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use workforce_desk::crew::{By, Party};
use workforce_desk::model::Engine;
use workforce_desk::plan::{Draft, DraftTicket, PlanState};
use workforce_desk::{Desk, DeskStore, NewPlan, Options, PlanEdit, RestorePolicy};
use workforce_hub::desk_floor::{router, Sources};
use workforce_hub::floor::postgres::PgFloorStore;
use workforce_hub::floor::Installed;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_postgres::test_support::with_test_pool;

use crate::support::{agent_service, token_for, Gateway};

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_without_desk_serves_no_plan_routes() {
    let config = workforce_common::settings::WorkforceConfig::default();
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
    let app = workforce_hub::desk_driver::mount(
        axum::Router::new(),
        &gateway.state,
        &config,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("a hub without desk");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let token = token_for(&gateway.admin);
    let id = uuid::Uuid::new_v4();
    let client = reqwest::Client::new();
    for (method, path) in [
        (reqwest::Method::GET, "/api/desk/plans".to_string()),
        (reqwest::Method::GET, format!("/api/desk/plans/{id}")),
        (reqwest::Method::PATCH, format!("/api/desk/plans/{id}")),
        (reqwest::Method::POST, format!("/api/desk/plans/{id}/pause")),
        (
            reqwest::Method::POST,
            format!("/api/desk/plans/{id}/resume"),
        ),
        (reqwest::Method::POST, format!("/api/desk/plans/{id}/drop")),
    ] {
        let status = client
            .request(method.clone(), format!("http://{addr}{path}"))
            .bearer_auth(&token)
            .json(&json!({ "revision": 1 }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{method} {path}");
    }
}

/// A Desk on `pool` whose engines are stand-ins that never start.
async fn desk(pool: workforce_postgres::PostgresPool, dir: &std::path::Path) -> Desk {
    use std::os::unix::fs::PermissionsExt;
    let cli = dir.join("fake-claude");
    std::fs::write(&cli, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    Desk::start(
        DeskStore::with_pool(pool).await.unwrap(),
        Options {
            programs: BTreeMap::from([(Engine::Claude, cli.clone()), (Engine::Codex, cli)]),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            run_dir: dir.join("run"),
            scrollback_bytes: 4096,
            approval_timeout: Duration::from_secs(60),
            restore: RestorePolicy::Off,
            adopt_every: None,
            holder: None,
            hub_internal: Default::default(),
            account_keys: None,
            line_reach: workforce_desk::line::Reach::Group,
            memory: Default::default(),
            bosses: Default::default(),
        },
        Default::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
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
async fn a_bosss_plan_is_approved_from_the_floor_by_the_person_alone_as_of_the_revision_read() {
    with_test_pool(PgFloorStore::build_pool, |floor_pool| async move {
        with_test_pool(workforce_desk::store::build_pool, |desk_pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
            let bob = Identity::new("Bob", IdentityRole::Member);
            gateway
                .state
                .identity_store
                .as_ref()
                .unwrap()
                .create(&bob)
                .unwrap();
            let d = desk(desk_pool, dir.path()).await;
            let sarah = Party::manager("sarah", "Sarah");
            let by_sarah = By::manager("sarah", "Sarah", "Sam asked for a green suite.");
            let p = d
                .propose_plan(
                    NewPlan {
                        draft: Draft {
                            goal: "Make the suite green".into(),
                            tickets: vec![DraftTicket {
                                title: "Fix the flaky payment test".into(),
                                folder: dir.path().to_string_lossy().into_owned(),
                                ..Default::default()
                            }],
                        },
                        board: None,
                        parallel: None,
                        engine: Engine::Claude,
                        group: None,
                        budget: None,
                    },
                    &sarah,
                    &by_sarah,
                )
                .await
                .unwrap();
            let id = p.summary.plan.id;
            let installed = Installed::open(PgFloorStore::with_pool(floor_pool).await.unwrap());
            let shown = d.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let state = Arc::clone(&gateway.state);
            let app = workforce_hub::gateway::router(Arc::clone(&state)).merge(router(
                state,
                Some(Sources::new(
                    installed,
                    Arc::new(move || Some(shown.clone())),
                )),
            ));
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap();
            });
            let mine = token_for(&gateway.admin);
            let theirs = token_for(&bob);

            // The person sees it on their Floor; someone else doesn't.
            let day = (Utc::now() - chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .replace('+', "%2B")
                .replace(':', "%3A");
            let floor = |bearer: String| {
                let day = day.clone();
                async move {
                    reqwest::Client::new()
                        .get(format!("http://{addr}/api/desk/floor?day={day}"))
                        .bearer_auth(bearer)
                        .send()
                        .await
                        .unwrap()
                        .json::<Value>()
                        .await
                        .unwrap()
                }
            };
            let need = format!("plan:{id}");
            let has = |v: &Value| {
                v["needs"]
                    .as_array()
                    .is_some_and(|n| n.iter().any(|x| x["id"] == need.as_str()))
            };
            assert!(has(&floor(mine.clone()).await));
            assert!(!has(&floor(theirs.clone()).await));

            let at = json!({ "kind": "plan", "plan": id, "revision": 1 });
            assert_eq!(
                decide(addr, &theirs, &at, "allow").await,
                (403, json!({"error": "not_yours"}))
            );
            // The hub's own token is no one: it never decides a plan.
            let gateway_token = gateway.state.auth_token.clone();
            assert_eq!(decide(addr, &gateway_token, &at, "allow").await.0, 401);
            // Deny leaves it as proposed, for the person to open; the card
            // reads the plan as it is now.
            let (status, denied) = decide(addr, &mine, &at, "deny").await;
            assert_eq!(status, 200, "{denied}");
            assert_eq!(denied["state"], "proposed");
            assert_eq!(
                d.plan(id).await.unwrap().summary.plan.state,
                PlanState::Proposed
            );

            // Sarah changes it: the revision the person read is old.
            d.change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    goal: Some("Make the suite green, fast".into()),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah,
            )
            .await
            .unwrap();
            assert_eq!(
                decide(addr, &mine, &at, "allow").await,
                (409, json!({"error": "already_answered"}))
            );
            let now = json!({ "kind": "plan", "plan": id, "revision": 2 });
            let (status, ran) = decide(addr, &mine, &now, "allow").await;
            assert_eq!(status, 200, "{ran}");
            assert_eq!(ran["state"], "running");
            assert_eq!(ran["approvedRevision"], 2);
            assert!(ran["tickets"].is_array() && ran["revisions"].is_array());
            assert!(!has(&floor(mine.clone()).await), "nothing waits on them");

            // Sarah adds a ticket: it waits on the person, who turns it
            // down by naming that revision.
            let detail = d.plan(id).await.unwrap();
            let mut tickets: Vec<DraftTicket> = detail
                .tickets
                .iter()
                .map(|t| DraftTicket {
                    id: Some(t.ticket.ticket.id),
                    title: t.ticket.ticket.title.clone(),
                    folder: dir.path().to_string_lossy().into_owned(),
                    ..Default::default()
                })
                .collect();
            tickets.push(DraftTicket {
                title: "Update the docs".into(),
                folder: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            });
            let waits = d
                .change_plan(
                    id,
                    PlanEdit {
                        revision: Some(2),
                        tickets: Some(tickets),
                        ..Default::default()
                    },
                    Engine::Claude,
                    false,
                    &by_sarah,
                )
                .await
                .unwrap();
            assert!(!waits.applied);
            assert!(has(&floor(mine.clone()).await));
            let change = json!({ "kind": "plan", "plan": id, "revision": 3 });
            assert_eq!(
                decide(addr, &theirs, &change, "deny").await,
                (403, json!({"error": "not_yours"}))
            );
            let (status, kept) = decide(addr, &mine, &change, "deny").await;
            assert_eq!(status, 200, "{kept}");
            assert_eq!(kept["revision"], 4);
            assert_eq!(kept["approvedRevision"], 4);
            assert_eq!(
                kept["tickets"].as_array().unwrap().len(),
                1,
                "nothing added"
            );
            assert!(!has(&floor(mine.clone()).await));
            // Answered: the same again is already answered.
            assert_eq!(
                decide(addr, &mine, &change, "deny").await,
                (409, json!({"error": "already_answered"}))
            );
        })
        .await;
    })
    .await;
}

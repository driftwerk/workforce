//! `/api/desk/activity` on the hub's own desk: a post that no turn of this
//! hub records, a delivery or another hub's answer, is a row of its own,
//! saying the room it is in and the message it is about.

use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use crate::support::{agent_service, token_for, Gateway};

#[tokio::test(flavor = "multi_thread")]
async fn an_activity_page_carries_a_room_posts_delivered_event_with_its_place() {
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
    let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
    let app = workforce_hub::desk_driver::mount(
        axum::Router::new(),
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
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let token = token_for(&gateway.admin);
    let call = |method: reqwest::Method, path: String, body: Option<Value>| {
        let request = reqwest::Client::new()
            .request(method, format!("http://{addr}{path}"))
            .bearer_auth(&token);
        async move {
            let response = match body {
                Some(body) => request.json(&body),
                None => request,
            }
            .send()
            .await
            .unwrap();
            let status = response.status().as_u16();
            (
                status,
                response.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };

    let (status, room) = call(
        reqwest::Method::POST,
        "/api/desk/rooms".into(),
        Some(json!({"name": "Atlas launch", "members": [{"hub": "atlas-hub", "agent": "atlas"}]})),
    )
    .await;
    assert_eq!(status, 200, "{room}");
    let room_id = room["id"].as_str().unwrap().to_string();
    let (status, reminder) = call(
        reqwest::Method::POST,
        format!("/api/desk/rooms/{room_id}/messages"),
        Some(
            json!({"author": "atlas-hub/atlas", "content": "Reminder: the launch is at five",
                    "delivery": "reminder", "source": "atlas-hub/atlas/m-1"}),
        ),
    )
    .await;
    assert_eq!(status, 201, "{reminder}");

    let mut events = Vec::new();
    for _ in 0..100 {
        let (status, page) = call(
            reqwest::Method::GET,
            "/api/desk/activity?limit=50".into(),
            None,
        )
        .await;
        assert_eq!(status, 200, "{page}");
        events = page["events"].as_array().cloned().unwrap_or_default();
        if events.iter().any(|e| e["kind"] == "delivered") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let delivered = events
        .iter()
        .find(|e| e["kind"] == "delivered")
        .unwrap_or_else(|| panic!("a delivered row: {events:?}"));
    assert_eq!(
        (
            &delivered["text"],
            &delivered["actor"]["id"],
            &delivered["place"],
            &delivered["message"]
        ),
        (
            &json!("sent a reminder"),
            &json!("atlas-hub/atlas"),
            &json!({"kind": "room", "room": room_id, "thread": null}),
            &reminder["id"]
        )
    );
}

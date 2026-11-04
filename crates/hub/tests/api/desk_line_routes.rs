//! Desk's lines are Desk's alone: a hub without `[desk]` serves none of
//! their routes, and a config written before them reads as it did.

use crate::support::{agent_service, token_for, Gateway};

#[test]
fn a_desk_config_written_before_lines_reads_with_the_group_reach() {
    let config: workforce_common::settings::DeskConfig =
        toml::from_str("enabled = true\nclaude = \"claude\"\n").unwrap();
    assert_eq!(
        config.line_reach,
        workforce_common::settings::DeskLineReach::Group
    );
    assert!(!workforce_common::settings::DeskConfig::default().enabled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_without_desk_serves_no_line_routes() {
    let config = workforce_common::settings::WorkforceConfig::default();
    assert!(!config.desk.enabled);
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
        (
            reqwest::Method::GET,
            format!("/api/desk/sessions/{id}/line"),
        ),
        (
            reqwest::Method::POST,
            format!("/api/desk/sessions/{id}/line"),
        ),
        (reqwest::Method::PATCH, format!("/api/desk/line/{id}")),
        (reqwest::Method::POST, format!("/api/desk/line/{id}/answer")),
        (
            reqwest::Method::POST,
            format!("/api/desk/line/{id}/dismiss"),
        ),
        (
            reqwest::Method::POST,
            format!("/api/desk/sessions/{id}/draft"),
        ),
    ] {
        let status = client
            .request(method.clone(), format!("http://{addr}{path}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"text": "hi"}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{method} {path}");
    }
}

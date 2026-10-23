//! The agent Slack routes: a connect needs the shared-vault-write capability,
//! and no response echoes the tokens it was sent.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::router;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, send, token_for, GatewayParts};

const BODY: &str = r#"{"bot_token":"xoxb-route-test-secret","app_token":"xapp-route-test-secret"}"#;

fn request(method: &str, caller: &Identity, body: Option<&str>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri("/api/agents/scout/slack")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token_for(caller)),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

#[tokio::test]
async fn connecting_an_agent_to_slack_needs_shared_vault_write_and_echoes_no_token() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let identities = SqliteIdentityStore::in_memory().unwrap();
    let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
    let member = Identity::new("Member", IdentityRole::Member);
    identities.create(&admin).unwrap();
    identities.create(&member).unwrap();
    let state = GatewayParts::new(
        AgentRegistry::from_parts(
            [("scout".to_string(), service)].into_iter().collect(),
            "scout".into(),
        ),
        Arc::new(identities),
        data.path().join("skills"),
    )
    .state();

    for method in ["PUT", "DELETE"] {
        let (status, _, text) = send(
            router(Arc::clone(&state)),
            request(method, &member, Some(BODY)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {text}");
        assert!(!text.contains("route-test-secret"), "{text}");
    }

    let (status, _, text) = send(
        router(Arc::clone(&state)),
        request("PUT", &admin, Some(BODY)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{text}");
    assert!(text.contains("Slack is not running"), "{text}");
    assert!(!text.contains("route-test-secret"), "{text}");

    let (status, _, text) = send(router(state), request("GET", &admin, None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{text}");
}

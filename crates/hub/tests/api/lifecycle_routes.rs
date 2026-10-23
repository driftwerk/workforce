//! The identity lifecycle routes as a client sees them: a dry run answers
//! with its report, a real run with `202` and the operation it started,
//! whose record the client then reads until it ends.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::router;
use workforce_hub::identity_lifecycle::{install, Lifecycle, Runs};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, send, token_for, GatewayParts};

async fn call(
    state: &Arc<workforce_hub::gateway::GatewayState>,
    method: &str,
    path: &str,
    caller: &Identity,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token_for(caller)),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let (status, _, text) = send(router(Arc::clone(state)), request).await;
    (status, serde_json::from_str(&text).unwrap_or_default())
}

/// The only test in this binary that installs the process-wide lifecycle.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_delete_answers_202_and_its_record_is_read_until_it_ends() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
    let (alice, bob) = (
        Identity::new("Alice", IdentityRole::Member),
        Identity::new("Bob", IdentityRole::Member),
    );
    for who in [&admin, &alice, &bob] {
        identities.create(who).unwrap();
    }
    let state = GatewayParts::new(
        AgentRegistry::from_parts(
            [("scout".to_string(), service)].into_iter().collect(),
            "scout".into(),
        ),
        Arc::clone(&identities) as Arc<dyn IdentityStore>,
        data.path().join("skills"),
    )
    .state();
    install(Lifecycle {
        registry: Arc::clone(&state.registry) as Arc<dyn workforce_hub::hosts::RegistryView>,
        identities: identities.clone(),
        devices: None,
        channels: None,
        defs: None,
        artifacts: None,
        external_skills: Arc::clone(&state.external_skills),
        data_dir: data.path().to_path_buf(),
        runs: Runs::default(),
    });
    let alice_path = format!("/api/identities/{}", alice.id);

    let (status, _) = call(&state, "DELETE", &alice_path, &bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, preview) = call(
        &state,
        "POST",
        &format!("{alice_path}/delete"),
        &admin,
        Some(serde_json::json!({"dry_run": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["dry_run"], true);
    assert!(preview["categories"].is_array());

    let (status, started) = call(&state, "DELETE", &alice_path, &admin, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["operation"]["kind"], "delete");
    assert_eq!(started["operation"]["subject"], alice.id.to_string());
    let record = format!(
        "/api/identity-lifecycle/operations/{}",
        started["operation"]["id"].as_str().unwrap()
    );

    let ended = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let (status, read) = call(&state, "GET", &record, &admin, None).await;
            assert_eq!(status, StatusCode::OK, "{read}");
            if read["operation"]["status"] != "running" {
                return read["operation"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the delete ends");
    assert_eq!(ended["status"], "complete", "{ended}");
    assert_eq!(ended["report"]["complete"], true);
    assert!(identities.get_by_id(alice.id).unwrap().is_none());

    let (status, _) = call(&state, "GET", &record, &bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &state,
        "GET",
        "/api/identity-lifecycle/operations/not-an-id",
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &state,
        "GET",
        &format!(
            "/api/identity-lifecycle/operations/{}",
            uuid::Uuid::new_v4()
        ),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

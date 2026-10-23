//! Work started over the admin API runs as the person behind the admin gate.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use workforce_hub::api::router as api_router;
use workforce_hub::gateway::require_admin_auth_except_operational;
use workforce_hub::state::HubState;
use workforce_hub::workflow_runtime::WorkflowRuntime;

use crate::support::{agent_service, hub_state, send, Gateway, GATEWAY_TOKEN};

/// The admin API behind the gate, mounted as the standalone admin port is.
fn admin_api(hub: Arc<HubState>, gateway: &Gateway) -> axum::Router {
    axum::Router::new()
        .nest("/api", api_router(hub))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gateway.state),
            require_admin_auth_except_operational,
        ))
}

fn post(path: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn rest_workflow_run_persists_caller_run_as() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let workflows = data.path().join("wf-defs");
    std::fs::create_dir_all(&workflows).unwrap();
    // A tool step naming an unknown tool fails fast and offline.
    std::fs::write(
        workflows.join("triage.toml"),
        "[workflow]\nid = \"triage\"\nname = \"Triage\"\n\
         \n[workflow.context]\nrequest = { required = false }\n\
         \n[[step]]\nid = \"s1\"\nname = \"s1\"\ntask = \"{}\"\n\
         tool = \"nope.nope\"\nexit = true\n",
    )
    .unwrap();
    let runtime = Arc::new(WorkflowRuntime::new(
        Arc::clone(&service),
        Arc::clone(&service.telemetry_store),
        workflows,
    ));
    runtime.reload_definitions().await.unwrap();
    let gateway = Gateway::new(vec![("default", service)], None);
    let hub = Arc::new(HubState {
        workflow_runtime: Some(Arc::clone(&runtime) as Arc<dyn workforce_hub::hosts::WorkflowHost>),
        ..hub_state(data.path())
    });
    let run_as = |bearer: String| {
        let (hub, runtime) = (Arc::clone(&hub), Arc::clone(&runtime));
        let app = admin_api(hub, &gateway);
        async move {
            let (status, _, text) = send(
                app,
                post(
                    "/api/workflows/triage/run",
                    &bearer,
                    serde_json::json!({"payload": {"request": "the build is red"}}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{text}");
            let id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["instance_id"]
                .as_str()
                .unwrap()
                .to_string();
            let row = runtime.get_instance_status(&id).unwrap().unwrap();
            serde_json::from_str::<serde_json::Value>(&row.variables).unwrap()["_run_as"]
                .as_str()
                .map(str::to_string)
        }
    };

    assert_eq!(
        run_as(gateway.admin_token()).await,
        Some(gateway.admin.id.to_string()),
        "the run belongs to the signed-in admin who started it"
    );
    assert_eq!(
        run_as(GATEWAY_TOKEN.to_string()).await,
        None,
        "the static admin token is nobody"
    );
}

#[tokio::test]
async fn rest_task_submit_carries_requester() {
    let data = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(vec![], None);
    let hub = Arc::new(hub_state(data.path()));

    let (status, _, text) = send(
        admin_api(Arc::clone(&hub), &gateway),
        post(
            "/api/tasks",
            &gateway.admin_token(),
            serde_json::json!({"title": "t", "description": "summarise the week"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let settled = hub
        .telemetry_store
        .get_task_history("default", 10, None, None)
        .unwrap();
    assert_eq!(
        settled
            .iter()
            .map(|row| row.identity_id.clone())
            .collect::<Vec<_>>(),
        [Some(gateway.admin.id.to_string())],
        "the task ran, and settled, as the admin who submitted it"
    );
}

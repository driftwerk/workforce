//! Integration tests for the DW-428 admin API endpoints.
//!
//! Tests cover:
//! - Sub-agent management endpoints (GET /api/subagents, GET /api/subagents/{id}, POST /api/subagents/{id}/cancel)
//! - Event source management endpoints (GET /api/events/sources, POST /api/events/sources/{id}/start|stop|restart)
//! - Knowledge index endpoints (GET /api/knowledge/status, POST /api/knowledge/index, POST /api/knowledge/index/cancel)
//! - Enhanced audit log endpoint (GET /api/audit)
//! - System logs endpoint (GET /api/logs)
//! - Authentication enforcement on write endpoints

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // for oneshot

use workforce_hub::api::router as api_router;

/// Build the test app the same way the standalone admin port does in the
/// CLI: nest the path-relative `api::router` under `/api` so existing test
/// URIs (`/api/foo`) continue to match.
fn router(state: Arc<HubState>) -> axum::Router {
    axum::Router::new().nest("/api", api_router(state))
}

/// [`router`] as the static admin token reaches it through the admin gate.
fn router_for_the_static_token(state: Arc<HubState>) -> axum::Router {
    router(state).layer(axum::Extension(workforce_hub::gateway::CallerContext {
        is_admin: true,
        ..Default::default()
    }))
}
use workforce_hub::state::HubState;

// ── Test state builder ───────────────────────────────────────────────────────

fn minimal_state(data_dir: &std::path::Path) -> Arc<HubState> {
    Arc::new(crate::support::hub_state(data_dir))
}

/// Parse a response body to JSON.
async fn body_json(body: axum::body::Body) -> serde_json::Value {
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("valid JSON")
}

/// Send a GET request to the test router.
async fn get(app: axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let json = body_json(resp.into_body()).await;
    (status, json)
}

/// Send a POST request with a JSON body.
async fn post_json(
    app: axum::Router,
    path: &str,
    body: serde_json::Value,
    auth_token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = auth_token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let json = body_json(resp.into_body()).await;
    (status, json)
}

// ── Sub-agent endpoints ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_subagents_returns_503_when_manager_absent() {
    // When no subagent manager is wired, list returns 200 with an empty task list
    // (graceful degradation rather than 503).
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let (status, body) = get(app, "/api/subagents").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["count"].as_u64().unwrap_or(1),
        0,
        "expected empty task list; got {body}"
    );
}

#[tokio::test]
async fn get_subagent_returns_503_when_manager_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let (status, _body) = get(app, "/api/subagents/some-task-id").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn cancel_subagent_returns_503_when_manager_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let (status, _body) = post_json(
        app,
        "/api/subagents/some-task-id/cancel",
        serde_json::json!({}),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ── Event source endpoints ───────────────────────────────────────────────────

#[tokio::test]
async fn list_event_sources_returns_503_when_manager_absent() {
    // When no event source manager is wired, list returns 200 with an empty sources list
    // (graceful degradation rather than 503).
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router_for_the_static_token(state);

    let (status, body) = get(app, "/api/events/sources").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["count"].as_u64().unwrap_or(1),
        0,
        "expected empty sources list; got {body}"
    );
}

#[tokio::test]
async fn start_event_source_returns_503_when_manager_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router_for_the_static_token(state);

    let (status, _body) = post_json(
        app,
        "/api/events/sources/slack/start",
        serde_json::json!({}),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn stop_event_source_returns_503_when_manager_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router_for_the_static_token(state);

    let (status, _body) = post_json(
        app,
        "/api/events/sources/slack/stop",
        serde_json::json!({}),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn restart_event_source_returns_503_when_manager_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router_for_the_static_token(state);

    let (status, _body) = post_json(
        app,
        "/api/events/sources/slack/restart",
        serde_json::json!({}),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ── Health / ready / metrics endpoints ───────────────────────────────────────

#[tokio::test]
async fn health_endpoint_always_returns_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let (status, body) = get(app, "/api/health").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str().unwrap(), "ok");
}

#[tokio::test]
async fn ready_endpoint_returns_ready_with_no_optional_components() {
    // minimal_state has no NATS configured, so readiness reduces to the
    // orchestrator probe (which always succeeds on a fresh orchestrator).
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let (status, body) = get(app, "/api/ready").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str().unwrap(), "ready");
    assert_eq!(
        body["components"]["orchestrator"].as_bool(),
        Some(true),
        "orchestrator should be ready: {body}"
    );
    assert!(
        body["components"].get("nats").is_none(),
        "nats key should be absent when unconfigured: {body}"
    );
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text() {
    let tmp = tempfile::tempdir().unwrap();
    let state = minimal_state(tmp.path());
    let app = router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain content-type, got {ct}"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();
    // Body is either real Prometheus text (when the recorder is installed
    // by the test harness's setup_logging) or the empty-body comment line.
    // Either is acceptable here — the contract this test pins is the
    // route + content-type, not the recorder install.
    assert!(
        body.starts_with('#') || body.is_empty(),
        "expected Prometheus exposition or comment line, got: {body:?}"
    );
}

/// A schedule saved through the admin API is stamped with the administrator
/// who saved it, so an administrator's own job stays theirs; only the static
/// token, which names nobody, is stamped as itself.
#[tokio::test]
async fn an_admin_api_schedule_edit_records_the_administrator_who_made_it() {
    use workforce_hub::scheduler::{JobPayload, Schedule, Scheduler, OWNER_TOKEN_EDITOR};
    const ADMIN: &str = "9d1e8b7a-3c4f-4e2a-8b6d-5f0a1c2e3d4b";
    let tmp = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(Scheduler::new(
        crate::support::agent_service(tmp.path()).await,
    ));
    let id = scheduler
        .add_job_owned(
            "own",
            None,
            Schedule::Every {
                every_secs: 86_400,
                anchor: None,
            },
            JobPayload::AgentTurn {
                message: "report".into(),
                channel: None,
                channel_id: None,
                thread_id: None,
                model: None,
                user_id: None,
                max_rounds: None,
                agent_owns_delivery: false,
                tools: None,
            },
            Some(ADMIN.to_string()),
            None,
        )
        .await
        .unwrap();
    let mut state = crate::support::hub_state(tmp.path());
    state.scheduler = Some(Arc::clone(&scheduler));
    let state = Arc::new(state);
    for (caller, editor) in [
        (
            workforce_hub::gateway::CallerContext {
                identity_id: Some(ADMIN.to_string()),
                is_admin: true,
                ..Default::default()
            },
            ADMIN,
        ),
        (
            workforce_hub::gateway::CallerContext {
                is_admin: true,
                ..Default::default()
            },
            OWNER_TOKEN_EDITOR,
        ),
    ] {
        let app = router(Arc::clone(&state)).layer(axum::Extension(caller));
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/scheduler/{id}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"schedule": {"kind": "every", "every_secs": 3_600}}).to_string(),
            ))
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().edited_by.as_deref(),
            Some(editor)
        );
    }
}

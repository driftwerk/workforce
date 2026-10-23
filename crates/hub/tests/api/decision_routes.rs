//! A decision is pressed over the admin API as the person behind the admin
//! gate: a named decider decides, an administrator who is not named does not.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use workforce_common::channels::{
    ChannelCapabilities, ChannelSurface, OutboundMessage, Reaction, SentMessage, SurfaceKey,
    SurfaceRegistry,
};
use workforce_hub::api::router as api_router;
use workforce_hub::gateway::require_admin_auth_except_operational;
use workforce_hub::state::HubState;
use workforce_hub::workflow_runtime::WorkflowRuntime;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, hub_state, send, token_for, Gateway};

/// A Slack channel that takes every message.
struct Channel;

#[async_trait::async_trait]
impl ChannelSurface for Channel {
    fn platform(&self) -> &str {
        "slack"
    }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }
    async fn add_reaction(&self, _: &Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn remove_reaction(&self, _: &Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn send_message(&self, m: &OutboundMessage) -> eyre::Result<SentMessage> {
        Ok(SentMessage {
            channel: m.channel.clone(),
            message_id: "1.0".into(),
            thread_id: m.thread_id.clone(),
            platform: "slack".into(),
        })
    }
}

fn request(method: &str, path: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn a_named_decider_presses_over_rest_and_an_admin_who_is_not_named_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let gateway = Gateway::new(vec![("default", Arc::clone(&service))], None);
    let identities = gateway.state.identity_store.clone().unwrap();
    service.set_identity_store(Arc::clone(&identities));
    let other_admin = Identity::new("Other admin", IdentityRole::SystemAdmin);
    identities.create(&other_admin).unwrap();
    let workflows = data.path().join("wf-defs");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(
        workflows.join("daily-digest-email.toml"),
        format!(
            "[workflow]\nid = \"daily-digest-email\"\nname = \"Digest\"\n\n\
             [[step]]\nid = \"review\"\nname = \"Review\"\nreport = \"Today.\"\n\
             report_render = \"template\"\npause = \"awaiting-approval\"\n\
             branches = [{{ when = \"send\", goto = \"send\" }}]\n\n\
             [step.decision]\nchannel = \"CRUN000001\"\ndeciders = \"{}\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Send\"\noutcome = \"send\"\n\n\
             [[step]]\nid = \"send\"\nname = \"Send\"\nreport = \"Sent.\"\n\
             report_render = \"template\"\nexit = true\n",
            gateway.admin.id
        ),
    )
    .unwrap();
    let runtime = Arc::new(WorkflowRuntime::new(
        Arc::clone(&service),
        Arc::clone(&service.telemetry_store),
        workflows,
    ));
    let mut registry = SurfaceRegistry::new();
    registry.register(SurfaceKey::workspace("slack", "W1"), Arc::new(Channel));
    runtime.set_surface_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    runtime.reload_definitions().await.unwrap();
    let hub = Arc::new(HubState {
        workflow_runtime: Some(Arc::clone(&runtime) as Arc<dyn workforce_hub::hosts::WorkflowHost>),
        ..hub_state(data.path())
    });
    let call = |method: &'static str, path: String, bearer: String, body: serde_json::Value| {
        let app = axum::Router::new()
            .nest("/api", api_router(Arc::clone(&hub)))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&gateway.state),
                require_admin_auth_except_operational,
            ));
        async move {
            let (status, _, text) = send(app, request(method, &path, &bearer, body)).await;
            (
                status,
                serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            )
        }
    };

    let (status, started) = call(
        "POST",
        "/api/workflows/daily-digest-email/run".into(),
        gateway.admin_token(),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let run = started["instance_id"].as_str().unwrap().to_string();
    let mut shown = serde_json::Value::Null;
    for _ in 0..500 {
        let (_, instance) = call(
            "GET",
            format!("/api/workflow-instances/{run}"),
            gateway.admin_token(),
            serde_json::Value::Null,
        )
        .await;
        let waiting = !instance["decision"].is_null();
        shown = instance;
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(shown["decision"]["may_decide"], true, "{shown}");
    let listed = || async {
        let (_, list) = call(
            "GET",
            "/api/workflow-instances".into(),
            gateway.admin_token(),
            serde_json::Value::Null,
        )
        .await;
        list["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["instance_id"] == run.as_str())
            .map(|i| i["awaiting_decision"].clone())
    };
    assert_eq!(listed().await, Some(serde_json::json!(true)));
    let resume = format!("/api/workflow-instances/{run}/resume");

    let (refused, body) = call(
        "POST",
        resume.clone(),
        token_for(&other_admin),
        serde_json::json!({ "outcome": "send" }),
    )
    .await;
    assert_eq!(refused, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "Only Admin can decide this.");

    let (decided, body) = call(
        "POST",
        resume,
        gateway.admin_token(),
        serde_json::json!({ "outcome": "send", "gate": shown["decision"]["gate"] }),
    )
    .await;
    assert_eq!(decided, StatusCode::OK, "{body}");
    assert_eq!(body["decided"]["summary"], "Send by Admin");
    assert_eq!(listed().await, Some(serde_json::json!(false)));
}

//! `PATCH /api/agents/{id}` renames an agent, with the instructions that
//! name it, for the agent list now and after a restart.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use workforce_common::settings::WorkforceConfig;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::{router, GatewayState};
use workforce_identity::sqlite::SqliteIdentityStore;

use crate::support::{send, GatewayParts, GATEWAY_TOKEN};

/// A base config as Desk's hub writes it on every start: the default agent
/// named `Desk`, and one more.
fn base(data: &std::path::Path) -> String {
    format!(
        r#"[hub]
data_dir = {data:?}

[memory]
db_path = {db:?}

[llm]
api_key = "test-key"

[[agents]]
id = "desk"
name = "Desk"
default = true

[[agents]]
id = "scout"
name = "Scout"
system_prompt = "You are Scout."
"#,
        data = data.display().to_string(),
        db = data.join("memory.db").display().to_string(),
    )
}

/// The hub as it starts from `config_path`.
async fn boot(config_path: &std::path::Path) -> Arc<GatewayState> {
    let config = WorkforceConfig::load(config_path).unwrap();
    let registry = AgentRegistry::from_config(&config).await.unwrap();
    GatewayParts {
        config_path: Some(config_path.to_path_buf()),
        ..GatewayParts::new(
            registry,
            Arc::new(SqliteIdentityStore::in_memory().unwrap()),
            std::path::PathBuf::from("/tmp/agent-rename-test-skills"),
        )
    }
    .state()
}

async fn call(
    state: &Arc<GatewayState>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {GATEWAY_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let (status, _, text) = send(router(Arc::clone(state)), request).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn listed_name(state: &Arc<GatewayState>, id: &str) -> Value {
    let (status, list) = call(state, "GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("{id} is listed: {list}"))["name"]
        .clone()
}

#[tokio::test]
async fn a_rename_and_its_instructions_are_listed_at_once_and_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config_path = dir.path().join("workforce.toml");
    std::fs::write(&config_path, base(&data)).unwrap();
    let state = boot(&config_path).await;

    for (id, name, prompt) in [
        (
            "desk",
            "Harbour",
            "You are Harbour, one of the agents the person works with in Desk.",
        ),
        (
            "scout",
            "Lookout",
            "You are Lookout, one of the agents the person works with in Desk.",
        ),
    ] {
        let (status, agent) = call(
            &state,
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({ "name": format!("  {name} "), "system_prompt": prompt })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{agent}");
        assert_eq!(agent["id"], id);
        assert_eq!(agent["name"], name, "trimmed");
        assert_eq!(agent["default"], id == "desk");
        assert_eq!(listed_name(&state, id).await, name, "the default agent too");
    }

    // Desk's hub rewrites its base config on every start; the rename holds.
    std::fs::write(&config_path, base(&data)).unwrap();
    let config = WorkforceConfig::load(&config_path).unwrap();
    let scout = config.agent_by_id("scout").unwrap();
    assert_eq!(
        scout.system_prompt.as_deref(),
        Some("You are Lookout, one of the agents the person works with in Desk.")
    );
    assert_eq!(
        config.agent_by_id("desk").unwrap().system_prompt.as_deref(),
        Some("You are Harbour, one of the agents the person works with in Desk.")
    );
    let restarted = boot(&config_path).await;
    assert_eq!(listed_name(&restarted, "desk").await, "Harbour");
    assert_eq!(listed_name(&restarted, "scout").await, "Lookout");

    let (status, _) = call(
        &restarted,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "Spyglass" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config = WorkforceConfig::load(&config_path).unwrap();
    let scout = config.agent_by_id("scout").unwrap();
    assert_eq!(scout.name.as_deref(), Some("Spyglass"));
    assert!(
        scout
            .system_prompt
            .as_deref()
            .unwrap()
            .starts_with("You are Lookout"),
        "a rename alone keeps the instructions"
    );
}

#[tokio::test]
async fn a_blank_name_or_an_unknown_agent_is_refused_and_put_renames_too() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config_path = dir.path().join("workforce.toml");
    std::fs::write(&config_path, base(&data)).unwrap();
    let state = boot(&config_path).await;

    let (status, body) = call(
        &state,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/agents/ghost",
        Some(json!({ "name": "Ghost" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(listed_name(&state, "scout").await, "Scout");

    let (status, body) = call(
        &state,
        "PUT",
        "/api/agents/scout",
        Some(json!({ "name": "Lookout" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"], "scout", "PUT keeps its own answer");
    assert_eq!(listed_name(&state, "scout").await, "Lookout");
    assert_eq!(
        WorkforceConfig::load(&config_path)
            .unwrap()
            .agent_by_id("scout")
            .unwrap()
            .name
            .as_deref(),
        Some("Lookout")
    );
}

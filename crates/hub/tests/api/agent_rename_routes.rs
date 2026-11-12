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
            .starts_with("You are Spyglass, one of the agents"),
        "a rename alone says the new name in the instructions Desk wrote"
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

/// Writes the files Desk writes when it sets `name` up, into `id`'s
/// workspace.
fn desk_files(state: &Arc<GatewayState>, id: &str, name: &str) -> std::path::PathBuf {
    let workspace = state.registry.get(id).unwrap().workspace.clone();
    std::fs::create_dir_all(&workspace).unwrap();
    let files = [
        ("IDENTITY.md", format!("# Identity\n- **Name:** {name}\n- **Vibe:** calm\n")),
        (
            "SOUL.md",
            format!("# Soul\n\n## What you're for\n\nYou are {name}. Your human set you up for this, in their words:\n\n> Watch CI.\n"),
        ),
        (
            "AGENTS.md",
            format!("# Guidelines\n\n## Your Job\n\nAs {name}, your work is this, in your human's words:\n\n> Watch CI.\n"),
        ),
        (
            "WELCOME.md",
            format!("Greet them as {name}, and say in one sentence what you're here for: Watch CI.\n"),
        ),
    ];
    for (file, text) in files {
        std::fs::write(workspace.join(file), text).unwrap();
    }
    workspace
}

fn says(workspace: &std::path::Path, name: &str) {
    let read = |f: &str| std::fs::read_to_string(workspace.join(f)).unwrap();
    assert!(
        read("IDENTITY.md").contains(&format!("- **Name:** {name}\n")),
        "{}",
        read("IDENTITY.md")
    );
    assert!(
        read("SOUL.md").contains(&format!("You are {name}. Your human")),
        "{}",
        read("SOUL.md")
    );
    assert!(
        read("AGENTS.md").contains(&format!("As {name}, your work")),
        "{}",
        read("AGENTS.md")
    );
    assert!(
        read("WELCOME.md").starts_with(&format!("Greet them as {name}, and say")),
        "{}",
        read("WELCOME.md")
    );
}

#[tokio::test]
async fn a_rename_puts_the_new_name_in_the_agents_files_the_default_agents_too() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config_path = dir.path().join("workforce.toml");
    std::fs::write(&config_path, base(&data)).unwrap();
    let state = boot(&config_path).await;

    for (id, was, name) in [("scout", "Scout", "Lookout"), ("desk", "Desk", "Harbour")] {
        let workspace = desk_files(&state, id, was);
        let (status, body) = call(
            &state,
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({ "name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        says(&workspace, name);
    }
    // Instructions Desk wrote say the new name too, when none are given.
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "Scout", "system_prompt": "You are Scout, one of the agents the person works with in Desk.\n\nWhat you are for, in their words:\nWatch CI." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "Spyglass" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config = WorkforceConfig::load(&config_path).unwrap();
    assert!(config
        .agent_by_id("scout")
        .unwrap()
        .system_prompt
        .as_deref()
        .unwrap()
        .starts_with("You are Spyglass, one of the agents"));
}

#[tokio::test]
async fn a_name_another_agent_has_is_refused_its_case_and_spaces_aside() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config_path = dir.path().join("workforce.toml");
    std::fs::write(&config_path, base(&data)).unwrap();
    let state = boot(&config_path).await;
    let workspace = desk_files(&state, "scout", "Scout");

    let (status, body) = call(
        &state,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "  desk " })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "That name is taken.");
    assert_eq!(listed_name(&state, "scout").await, "Scout");
    says(&workspace, "Scout");

    // Its own name in another case is no clash.
    let (status, body) = call(
        &state,
        "PATCH",
        "/api/agents/scout",
        Some(json!({ "name": "SCOUT" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(listed_name(&state, "scout").await, "SCOUT");
}

/// The agent tools, as an agent of `state`'s hub has them: built at
/// runtime, with no config path of its own.
fn tools_of(
    state: &Arc<GatewayState>,
    config_path: &std::path::Path,
    data: &std::path::Path,
    agent: &str,
) -> workforce_hub::agent_tool::AgentToolContext {
    state.registry.set_config_path(config_path.to_path_buf());
    let view: Arc<dyn workforce_hub::hosts::RegistryView> = state.registry.clone();
    let cell: workforce_hub::hosts::RegistryCell = Arc::default();
    cell.set(Arc::downgrade(&view)).ok().unwrap();
    workforce_hub::agent_tool::AgentToolContext {
        config_path: None,
        config: WorkforceConfig::load(config_path).unwrap(),
        data_dir: data.display().to_string(),
        lock_manager: Arc::clone(&state.registry.default_agent().unwrap().lock_manager),
        agent_id: agent.to_string(),
        def_store: None,
        registry: cell,
    }
}

#[tokio::test]
async fn an_agent_renames_itself_once_approved_and_agent_update_renames_the_same_way() {
    use workforce_agent::tools::Tool;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config_path = dir.path().join("workforce.toml");
    std::fs::write(&config_path, base(&data)).unwrap();
    let state = boot(&config_path).await;
    let workspace = desk_files(&state, "scout", "Scout");

    let rename = workforce_hub::agent_tool::AgentRenameTool::new(tools_of(
        &state,
        &config_path,
        &data,
        "scout",
    ));
    assert_eq!(rename.name(), "agent.rename");
    assert!(
        rename.approval_requirement().is_some(),
        "a person approves every call"
    );
    assert_eq!(
        rename
            .approval_summary(&json!({ "name": " Lookout " }))
            .as_deref(),
        Some("Rename scout to Lookout")
    );

    let taken = rename.execute(json!({ "name": "Desk" })).await.unwrap();
    assert!(!taken.success);
    assert!(
        taken.error.as_deref() == Some("That name is taken."),
        "{:?}",
        taken.error
    );

    let done = rename.execute(json!({ "name": "Lookout" })).await.unwrap();
    assert!(done.success, "{}", done.output);
    assert_eq!(listed_name(&state, "scout").await, "Lookout");
    says(&workspace, "Lookout");
    assert_eq!(
        WorkforceConfig::load(&config_path)
            .unwrap()
            .agent_by_id("scout")
            .unwrap()
            .name
            .as_deref(),
        Some("Lookout")
    );

    // The default agent renaming another: the same way, files and all.
    let update = workforce_hub::agent_tool::AgentUpdateTool::new(tools_of(
        &state,
        &config_path,
        &data,
        "desk",
    ));
    let taken = update
        .execute(json!({ "agent_id": "scout", "name": "desk" }))
        .await
        .unwrap();
    assert_eq!(taken.error.as_deref(), Some("That name is taken."));
    let done = update
        .execute(json!({ "agent_id": "scout", "name": "Spyglass" }))
        .await
        .unwrap();
    assert!(done.success, "{}", done.output);
    says(&workspace, "Spyglass");
    assert_eq!(listed_name(&state, "scout").await, "Spyglass");
}

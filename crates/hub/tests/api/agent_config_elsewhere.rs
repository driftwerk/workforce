//! On a Desk hub an agent's commands run as the hub, so a command could
//! rewrite its `config.toml`. A change made outside the hub waits on the
//! person instead of being applied; the agent's own tool, the person's saves
//! and Desk Helper's edits before boot all apply as ever.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use workforce_agent::tools::Tool;
use workforce_hub::gateway::router;
use workforce_hub::serve::{AgentService, Compose as _};
use workforce_orchestrator::workflow_store::content_hash;

use crate::support::{send, Gateway, GATEWAY_TOKEN};

const SAVED: &str = "[llm]\nmax_rounds = 12\n";
/// What a command writes: the agent opened to everyone, with a model change
/// to make it look ordinary.
const WIDENED: &str = "[llm]\nmax_rounds = 99\n\n[access]\nmode = \"open\"\n";

/// An agent on a fresh data directory, on a hub running Desk when `desk`.
async fn agent(data_dir: &std::path::Path, desk: bool) -> Arc<AgentService> {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = data_dir.join("memory.db").to_string_lossy().into_owned();
    config.llm.api_key = Some("test-key".into());
    config.desk.enabled = desk;
    AgentService::from_config(config, None)
        .await
        .expect("test service")
}

fn config_path(service: &AgentService) -> std::path::PathBuf {
    service.workspace.join("config.toml")
}

fn on_disk(service: &AgentService) -> String {
    std::fs::read_to_string(config_path(service)).unwrap_or_default()
}

fn running(service: &AgentService) -> Option<String> {
    service.config_applied.read().clone()
}

async fn put(gateway: &Gateway, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("PUT")
        .uri("/api/agents/scout/config")
        .header(header::AUTHORIZATION, format!("Bearer {GATEWAY_TOKEN}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _, text) = send(router(Arc::clone(&gateway.state)), request).await;
    (status, text)
}

async fn config_tool(
    service: &Arc<AgentService>,
    input: serde_json::Value,
) -> workforce_agent::tools::ToolResult {
    workforce_hub::config_tool::ConfigTool::new(Arc::clone(service), None)
        .execute(input)
        .await
        .unwrap()
}

/// Who may talk to the agent is the person's: its own tool refuses
/// `[access]`, and the file stays as the person saved it. A setting that is
/// the agent's own applies at once on a Desk hub, guarded file and all.
/// (Who may call the tool is the gate's to say, as on any hub.)
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_cannot_set_its_own_access_but_its_own_settings_apply() {
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);

    let refused = config_tool(
        &service,
        serde_json::json!({"action": "set", "key": "access.mode", "value": "allowlist"}),
    )
    .await;
    assert!(!refused.success, "{:?}", refused.output);
    assert!(
        refused.error.unwrap_or_default().contains("human-only"),
        "said why"
    );
    assert_eq!(on_disk(&service), SAVED, "the person's file is untouched");

    let result = config_tool(
        &service,
        serde_json::json!({"action": "set", "key": "llm.max_rounds", "value": 20}),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let body = on_disk(&service);
    assert!(body.contains("max_rounds = 20"), "{body}");
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(&body).as_str()),
        "the running agent applied it"
    );
    assert_eq!(
        workforce_hub::agent_config_write::changed_elsewhere(&service),
        None,
        "the hub's own write is never a change made elsewhere"
    );
    service.reload_config().await.unwrap();
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(&body).as_str())
    );
}

/// A command rewrites the file while the hub runs: the watcher's reload keeps
/// what is running.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_made_outside_the_hub_is_not_applied() {
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);
    let saved = running(&service);
    assert_eq!(saved.as_deref(), Some(content_hash(SAVED).as_str()));

    std::fs::write(config_path(&service), WIDENED).unwrap();
    service.reload_config().await.unwrap();

    assert_eq!(running(&service), saved, "the running settings are kept");
}

/// The next ordinary save reads the file from disk: it must not carry a change
/// made outside the hub into history and the running agent.
#[tokio::test(flavor = "multi_thread")]
async fn an_ordinary_save_never_carries_a_change_made_outside_the_hub() {
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);
    let saved = running(&service);
    std::fs::write(config_path(&service), WIDENED).unwrap();

    let result = config_tool(
        &service,
        serde_json::json!({"action": "set", "key": "llm.voice_model", "value": "gemini-3.1-pro"}),
    )
    .await;
    assert!(!result.success, "{:?}", result.output);
    assert_eq!(on_disk(&service), WIDENED, "the file waits on the person");
    assert_eq!(running(&service), saved);

    let (status, body) = put(&gateway, WIDENED).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("outside"), "{body}");
    assert_eq!(running(&service), saved);
}

/// The person's own saves, from Desk's settings, apply at once on a Desk hub.
#[tokio::test(flavor = "multi_thread")]
async fn the_persons_saves_still_apply_on_a_desk_hub() {
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    for body in [SAVED, "[llm]\nmax_rounds = 30\n", SAVED] {
        let (status, text) = put(&gateway, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(on_disk(&service), body);
        assert_eq!(
            running(&service).as_deref(),
            Some(content_hash(body).as_str())
        );
        // The watcher's reload of the hub's own write applies it too.
        service.reload_config().await.unwrap();
        assert_eq!(
            running(&service).as_deref(),
            Some(content_hash(body).as_str())
        );
    }
}

/// Desk Helper edits the file before it starts the hub (`ensure_rich`,
/// `ensure_oversight`): the hub adopts what it finds at boot.
#[tokio::test(flavor = "multi_thread")]
async fn what_is_on_disk_at_boot_is_the_hubs_own() {
    // Where the agent's workspace is under a data directory, from another.
    let elsewhere = tempfile::tempdir().unwrap();
    let within = agent(elsewhere.path(), false)
        .await
        .workspace
        .strip_prefix(elsewhere.path().canonicalize().unwrap())
        .unwrap()
        .to_path_buf();
    let data = tempfile::tempdir().unwrap();
    let workspace = data.path().join(within);
    std::fs::create_dir_all(&workspace).unwrap();
    let helper = "[rich]\ncharts = true\n\n[oversight]\nenabled = true\nlevel = \"watch\"\n";
    std::fs::write(workspace.join("config.toml"), helper).unwrap();

    let service = agent(data.path(), true).await;
    assert_eq!(service.workspace, workspace.canonicalize().unwrap());
    service.reload_config().await.unwrap();
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(helper).as_str())
    );
}

/// Allow on the Floor: the change is admitted, applied and saved as the
/// person's own, provided it is still the one they were shown.
#[tokio::test(flavor = "multi_thread")]
async fn the_person_uses_a_change_made_outside_the_hub() {
    use workforce_hub::agent_config_write::{changed_elsewhere, use_changed, Refused};
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);
    assert_eq!(changed_elsewhere(&service), None);

    let edited = "[llm]\nmax_rounds = 40\n";
    std::fs::write(config_path(&service), edited).unwrap();
    let waiting = changed_elsewhere(&service).expect("a change waits");
    assert_eq!(waiting.hash, content_hash(edited));
    assert!(waiting.loads && !waiting.removed);

    std::fs::write(config_path(&service), WIDENED).unwrap();
    let refused = use_changed(&service, None, &waiting.hash, None)
        .await
        .unwrap_err();
    assert!(matches!(refused, Refused::Stale { .. }), "{refused:?}");
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(SAVED).as_str())
    );

    std::fs::write(config_path(&service), edited).unwrap();
    use_changed(&service, None, &waiting.hash, Some("person"))
        .await
        .unwrap();
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(edited).as_str())
    );
    assert_eq!(changed_elsewhere(&service), None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK, "saves go on");
}

/// Deny on the Floor: the saved file is put back and nothing it held is lost.
#[tokio::test(flavor = "multi_thread")]
async fn the_person_keeps_the_saved_settings() {
    use workforce_hub::agent_config_write::{changed_elsewhere, keep_saved, Refused};
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);
    std::fs::write(config_path(&service), WIDENED).unwrap();
    let waiting = changed_elsewhere(&service).expect("a change waits");

    let refused = keep_saved(&service, &content_hash("something else")).unwrap_err();
    assert!(matches!(refused, Refused::Stale { .. }), "{refused:?}");
    keep_saved(&service, &waiting.hash).unwrap();

    assert_eq!(on_disk(&service), SAVED);
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(SAVED).as_str())
    );
    assert_eq!(changed_elsewhere(&service), None);
    let result = config_tool(
        &service,
        serde_json::json!({"action": "set", "key": "llm.max_rounds", "value": 30}),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
}

/// Off Desk the file is not guarded: a runner keeps commands off it, and
/// replicas may share the workspace, so an edit there applies as before.
#[tokio::test(flavor = "multi_thread")]
async fn off_desk_an_edit_to_the_file_applies_as_before() {
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), false).await;
    std::fs::write(config_path(&service), SAVED).unwrap();
    service.reload_config().await.unwrap();
    assert_eq!(
        running(&service).as_deref(),
        Some(content_hash(SAVED).as_str())
    );
    assert_eq!(
        workforce_hub::agent_config_write::changed_elsewhere(&service),
        None
    );
}

/// Whatever reads the agent's settings from the file (the crew reads
/// `[oversight]` there, Desk reads the file to edit it) reads the saved one
/// while a change made outside the hub waits.
#[tokio::test(flavor = "multi_thread")]
async fn every_reader_sees_the_saved_file_while_a_change_waits() {
    use workforce_common::settings::AgentWorkspaceConfig;
    let data = tempfile::tempdir().unwrap();
    let service = agent(data.path(), true).await;
    let gateway = Gateway::new(vec![("scout", Arc::clone(&service))], None);
    assert_eq!(put(&gateway, SAVED).await.0, StatusCode::OK);
    let oversees = "[oversight]\nenabled = true\nlevel = \"act\"\n";
    std::fs::write(config_path(&service), oversees).unwrap();

    let loaded = AgentWorkspaceConfig::load(&service.workspace)
        .unwrap()
        .unwrap();
    assert!(loaded.oversight.is_none(), "{loaded:?}");
    let request = Request::builder()
        .uri("/api/agents/scout/config")
        .header(header::AUTHORIZATION, format!("Bearer {GATEWAY_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(router(Arc::clone(&gateway.state)), request).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, SAVED));
    assert_eq!(
        on_disk(&service),
        oversees,
        "the file itself waits as it is"
    );
}

// ── Boot: what history last saved is the hub's own ──

async fn with_history<F, Fut>(test: F)
where
    F: FnOnce(Arc<workforce_orchestrator::workflow_store::WorkflowStore>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use workforce_orchestrator::workflow_store::WorkflowStore;
    workforce_postgres::test_support::with_test_pool(
        WorkflowStore::build_pool,
        |pool| async move {
            test(WorkflowStore::new(pool).await.expect("def store")).await;
        },
    )
    .await;
}

/// What a fresh process does as it boots onto the same workspace: compose
/// applies the file it finds, then the sweeps guard it against history.
async fn restart(
    service: &AgentService,
    defs: &workforce_orchestrator::workflow_store::WorkflowStore,
) -> bool {
    workforce_common::config_files::unguard(&config_path(service));
    service.reload_config().await.unwrap();
    let held = workforce_hub::agent_config_write::guard_at_boot(service, Some(defs)).await;
    if held {
        service.reload_config().await.unwrap();
    }
    held
}

/// A command rewrites the file and stops the hub: the hub that starts again
/// holds the change instead of adopting it, and runs the saved settings.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_never_adopts_a_change_made_outside_the_hub() {
    use workforce_hub::agent_config_write::{changed_elsewhere, save};
    use workforce_orchestrator::workflow_store::EditContext;
    with_history(|defs| async move {
        let data = tempfile::tempdir().unwrap();
        let service = agent(data.path(), true).await;
        save(
            &service,
            service.agent_id.as_str(),
            Some(&defs),
            SAVED,
            EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap();
        std::fs::write(config_path(&service), WIDENED).unwrap();

        assert!(restart(&service, &defs).await, "the change is held");
        assert_eq!(
            running(&service).as_deref(),
            Some(content_hash(SAVED).as_str())
        );
        let waiting = changed_elsewhere(&service).expect("a need for the person");
        assert_eq!(waiting.sections, ["access", "llm"]);
        assert_eq!(waiting.details, ["access.mode: unset → open"]);
        assert_eq!(on_disk(&service), WIDENED, "the file waits as it is");
    })
    .await;
}

/// The hub's own writes that keep no revision of their own (`system.llm`,
/// compaction, a report theme) go into history as the watcher sees them, so
/// a restart after one adopts the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_after_the_hubs_own_write_adopts_it() {
    use workforce_hub::agent_config_write::{changed_elsewhere, record_own, save};
    use workforce_orchestrator::workflow_store::EditContext;
    with_history(|defs| async move {
        let data = tempfile::tempdir().unwrap();
        let service = agent(data.path(), true).await;
        save(
            &service,
            service.agent_id.as_str(),
            Some(&defs),
            SAVED,
            EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap();
        let switched = "[llm]\nmax_rounds = 12\nprovider = \"gemini\"\n";
        workforce_common::config_files::replace(&config_path(&service), switched.as_bytes())
            .unwrap();
        record_own(&service, Some(&defs)).await;

        assert!(!restart(&service, &defs).await);
        assert_eq!(
            running(&service).as_deref(),
            Some(content_hash(switched).as_str())
        );
        assert_eq!(changed_elsewhere(&service), None);
    })
    .await;
}

/// An agent history knows nothing of yet adopts its file at boot, as today.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_boot_adopts_the_file() {
    with_history(|defs| async move {
        let data = tempfile::tempdir().unwrap();
        let service = agent(data.path(), true).await;
        std::fs::write(config_path(&service), SAVED).unwrap();
        assert!(!restart(&service, &defs).await);
        assert_eq!(
            running(&service).as_deref(),
            Some(content_hash(SAVED).as_str())
        );
        assert_eq!(
            workforce_hub::agent_config_write::changed_elsewhere(&service),
            None
        );
    })
    .await;
}

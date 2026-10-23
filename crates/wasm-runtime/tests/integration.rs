//! End-to-end integration tests using a real compiled WASM skill.
//!
//! These tests load the `echo-skill` fixture (compiled from
//! `tests/fixtures/echo-skill/`) and exercise the full pipeline:
//! SDK → WASM binary → WasmSkillEngine → host functions → ToolResult.
//!
//! The .wasm binary must be pre-built. Run:
//!   cd crates/wasm-runtime/tests/fixtures/echo-skill
//!   cargo build --target wasm32-unknown-unknown --release

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use workforce_common::settings::SecretStore;
use workforce_wasm_runtime::engine::WasmSkillEngine;
use workforce_wasm_runtime::manifest::SkillManifest;
use workforce_wasm_runtime::tool::WasmSkillTool;
use workforce_wasm_runtime::types::{SkillRuntimeBindings, WasmToolRequest};

/// In-memory `SecretStore` for tests. Keys are full vault paths after
/// the host has applied per-skill / per-user scoping.
struct MockSecretStore {
    data: Mutex<HashMap<String, String>>,
}

impl MockSecretStore {
    fn new(initial: &[(&str, &str)]) -> Self {
        Self {
            data: Mutex::new(
                initial
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        }
    }
}

impl SecretStore for MockSecretStore {
    fn get(&self, path: &str) -> Option<String> {
        self.data.lock().unwrap().get(path).cloned()
    }
    fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
        self.data
            .lock()
            .unwrap()
            .insert(path.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, path: &str) -> eyre::Result<bool> {
        Ok(self.data.lock().unwrap().remove(path).is_some())
    }
    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let map = self.data.lock().unwrap();
        match prefix {
            Some(p) => map.keys().filter(|k| k.starts_with(p)).cloned().collect(),
            None => map.keys().cloned().collect(),
        }
    }
}

/// Path to the pre-built .wasm fixture.
///
/// This file is committed to the repo so that CI can run integration tests
/// without needing `wasm32-unknown-unknown` installed. If you change the
/// echo-skill source or the SDK, rebuild it:
///   cd crates/wasm-runtime/tests/fixtures/echo-skill
///   cargo build --target wasm32-unknown-unknown --release
///   cp target/wasm32-unknown-unknown/release/echo_skill.wasm ../echo_skill.wasm
fn wasm_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_skill.wasm")
}

/// Load the echo-skill .wasm bytes.
fn load_wasm_bytes() -> Vec<u8> {
    let path = wasm_path();
    std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "echo_skill.wasm not found at {}.\n\
             Rebuild: cd crates/wasm-runtime/tests/fixtures/echo-skill && \
             cargo build --target wasm32-unknown-unknown --release && \
             cp target/wasm32-unknown-unknown/release/echo_skill.wasm ../echo_skill.wasm",
            path.display()
        )
    })
}

/// Load the echo-skill manifest.
fn load_manifest() -> SkillManifest {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-skill/Skill.toml");
    SkillManifest::from_file(&path).expect("Failed to parse echo-skill Skill.toml")
}

/// Create an engine with the echo-skill loaded.
async fn setup_engine() -> (WasmSkillEngine, SkillManifest) {
    let engine = WasmSkillEngine::new().expect("engine init");
    let wasm_bytes = load_wasm_bytes();
    let manifest = load_manifest();

    engine
        .load_module("echo-skill", 1, &wasm_bytes)
        .await
        .expect("compile echo-skill");

    (engine, manifest)
}

// ─── Engine: compile + execute ──────────────────────────────────────────────

#[tokio::test]
async fn compile_echo_skill() {
    let engine = WasmSkillEngine::new().unwrap();
    let wasm_bytes = load_wasm_bytes();

    engine
        .load_module("echo-skill", 1, &wasm_bytes)
        .await
        .unwrap();

    assert!(engine.has_module("echo-skill", 1).await);
    assert_eq!(engine.module_count().await, 1);
}

// ─── Echo tool: basic input/output ──────────────────────────────────────────

#[tokio::test]
async fn echo_tool_returns_input() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.echo".into(),
        handler: "echo".into(),
        input: serde_json::json!({"message": "hello workforce"}),
        agent_id: "test-agent".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .expect("echo execution");

    assert!(
        response.success,
        "echo should succeed: {:?}",
        response.error
    );
    assert_eq!(response.result["echoed"], "hello workforce");
    assert_eq!(response.result["tool_name"], "echo-skill.echo");
    assert_eq!(response.result["agent_id"], "test-agent");
}

#[tokio::test]
async fn echo_tool_missing_message_uses_default() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.echo".into(),
        handler: "echo".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["echoed"], "(no message)");
}

// ─── Add tool: numeric operations ───────────────────────────────────────────

#[tokio::test]
async fn add_tool_integers() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.add".into(),
        handler: "add".into(),
        input: serde_json::json!({"a": 10, "b": 32}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["sum"], 42.0);
    assert_eq!(response.result["a"], 10.0);
    assert_eq!(response.result["b"], 32.0);
}

#[tokio::test]
async fn add_tool_floats() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.add".into(),
        handler: "add".into(),
        input: serde_json::json!({"a": 2.75, "b": 3.25}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    let sum = response.result["sum"].as_f64().unwrap();
    assert!((sum - 6.0).abs() < 0.001);
}

#[tokio::test]
async fn add_tool_missing_param_returns_error() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.add".into(),
        handler: "add".into(),
        input: serde_json::json!({"a": 5}), // missing "b"
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(!response.success);
    assert!(response
        .error
        .as_deref()
        .unwrap()
        .contains("Missing parameter 'b'"));
}

// ─── Fail tool: error handling ──────────────────────────────────────────────

#[tokio::test]
async fn fail_tool_returns_error() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.fail".into(),
        handler: "fail".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(!response.success);
    assert_eq!(
        response.error.as_deref(),
        Some("This tool always fails on purpose")
    );
}

// ─── Log tool: exercises logging host functions ─────────────────────────────

#[tokio::test]
async fn log_test_tool_executes() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.log_test".into(),
        handler: "log_test".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["logged"], true);
}

// ─── Utility host functions: time + UUID ────────────────────────────────────

#[tokio::test]
async fn get_time_tool_returns_time_and_uuid() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.get_time".into(),
        handler: "get_time".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);

    // Time should be a non-empty RFC3339 string
    let time = response.result["time"].as_str().unwrap();
    assert!(!time.is_empty(), "time should not be empty");
    assert!(
        time.contains('T'),
        "time should look like RFC3339: {}",
        time
    );

    // UUID should be a non-empty string with hyphens
    let uuid = response.result["uuid"].as_str().unwrap();
    assert!(!uuid.is_empty(), "uuid should not be empty");
    assert!(uuid.contains('-'), "uuid should contain hyphens: {}", uuid);
    assert_eq!(uuid.len(), 36, "uuid should be 36 chars: {}", uuid);
}

// ─── Nonexistent handler ────────────────────────────────────────────────────

#[tokio::test]
async fn nonexistent_handler_returns_error() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.nonexistent".into(),
        handler: "nonexistent".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let result = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("nonexistent"));
}

// ─── WasmSkillTool bridge: Tool trait integration ───────────────────────────

#[tokio::test]
async fn wasm_skill_tool_bridge_echo() {
    let (engine, manifest) = setup_engine().await;
    let engine = Arc::new(engine);

    let tools = WasmSkillTool::from_manifest(
        &manifest,
        engine,
        1,
        HashMap::new(),
        None,
        SkillRuntimeBindings::unbound(),
    );

    // Find the echo tool
    let echo_tool = tools
        .iter()
        .find(|t| t.name() == "echo-skill.echo")
        .expect("echo tool should exist");

    assert_eq!(echo_tool.name(), "echo-skill.echo");
    assert_eq!(echo_tool.description(), "Echo back the input parameters");

    // Execute through the Tool trait
    let result = echo_tool
        .execute(serde_json::json!({"message": "via bridge"}))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.output["echoed"], "via bridge");
}

#[tokio::test]
async fn wasm_skill_tool_bridge_creates_all_tools() {
    let (engine, manifest) = setup_engine().await;
    let engine = Arc::new(engine);

    let tools = WasmSkillTool::from_manifest(
        &manifest,
        engine,
        1,
        HashMap::new(),
        None,
        SkillRuntimeBindings::unbound(),
    );

    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"echo-skill.echo"));
    assert!(names.contains(&"echo-skill.add"));
    assert!(names.contains(&"echo-skill.fail"));
    assert!(names.contains(&"echo-skill.log_test"));
    assert!(names.contains(&"echo-skill.get_time"));
    assert!(names.contains(&"echo-skill.infinite_loop"));
    assert!(names.contains(&"echo-skill.vault_read"));
    assert!(names.contains(&"echo-skill.config_read"));
    assert!(names.contains(&"echo-skill.invoke_many_test"));
    assert!(names.contains(&"echo-skill.fetch_burst"));
    assert_eq!(tools.len(), 10);
}

#[tokio::test]
async fn wasm_skill_tool_bridge_error_handling() {
    let (engine, manifest) = setup_engine().await;
    let engine = Arc::new(engine);

    let tools = WasmSkillTool::from_manifest(
        &manifest,
        engine,
        1,
        HashMap::new(),
        None,
        SkillRuntimeBindings::unbound(),
    );

    let fail_tool = tools
        .iter()
        .find(|t| t.name() == "echo-skill.fail")
        .unwrap();

    let result = fail_tool.execute(serde_json::json!({})).await.unwrap();

    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("always fails"));
}

// ─── Multiple sequential executions (no state leaking) ──────────────────────

#[tokio::test]
async fn multiple_executions_are_independent() {
    let (engine, manifest) = setup_engine().await;

    for i in 0..5 {
        let request = WasmToolRequest {
            tool_name: "echo-skill.echo".into(),
            handler: "echo".into(),
            input: serde_json::json!({"message": format!("iteration {}", i)}),
            agent_id: "agent-1".into(),
            user_id: None,
            task_id: None,
        };

        let response = engine
            .execute_tool(
                "echo-skill",
                1,
                &manifest,
                &request,
                &HashMap::new(),
                None,
                None,
            )
            .await
            .unwrap();

        assert!(response.success);
        assert_eq!(response.result["echoed"], format!("iteration {}", i));
    }
}

// ─── Store round-trip ───────────────────────────────────────────────────────

#[tokio::test]
async fn store_and_reload_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let wasm_bytes = load_wasm_bytes();
    let manifest = load_manifest();

    // Store
    let store = workforce_wasm_runtime::WasmSkillStore::open(tmp.path()).unwrap();
    let installed = workforce_wasm_runtime::types::InstalledWasmSkill {
        manifest: manifest.clone(),
        state: workforce_wasm_runtime::types::WasmSkillState::Active,
        installed_at: chrono::Utc::now(),
        source: "test".into(),
        config: HashMap::new(),
        binary_version: 1,
        sha256: String::new(),
        owner: None,
        visibility: Default::default(),
        shares: Vec::new(),
    };
    store
        .store_binary("echo-skill", &wasm_bytes, &manifest, &installed)
        .unwrap();

    // Reload in a fresh engine
    let engine = WasmSkillEngine::new().unwrap();
    let reloaded_bytes = store.load_binary("echo-skill", 1).unwrap();
    engine
        .load_module("echo-skill", 1, &reloaded_bytes)
        .await
        .unwrap();

    // Execute
    let request = WasmToolRequest {
        tool_name: "echo-skill.echo".into(),
        handler: "echo".into(),
        input: serde_json::json!({"message": "after reload"}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["echoed"], "after reload");
}

// ─── Vault host function ────────────────────────────────────────────────────

#[tokio::test]
async fn vault_read_with_secret_present() {
    let engine = WasmSkillEngine::new().unwrap();
    let wasm_bytes = load_wasm_bytes();
    let manifest = load_manifest();
    engine
        .load_module("echo-skill", 1, &wasm_bytes)
        .await
        .unwrap();

    // Wire a mock vault that returns a known value for the scoped path.
    // Per the new manifest schema, `api_key` defaults to `User` scope; the
    // host computes `wasm-skills/echo-skill/users/{uid}/api_key`.
    let store = Arc::new(MockSecretStore::new(&[(
        "wasm-skills/echo-skill/users/agent-1-user/api_key",
        "sk-test-12345",
    )]));

    let request = WasmToolRequest {
        tool_name: "echo-skill.vault_read".into(),
        handler: "vault_read".into(),
        input: serde_json::json!({"key": "api_key"}),
        agent_id: "agent-1".into(),
        user_id: Some("agent-1-user".into()),
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(store as Arc<dyn SecretStore>),
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["found"], true);
    assert_eq!(response.result["value"], "sk-test-12345");
    assert_eq!(response.result["key"], "api_key");
}

#[tokio::test]
async fn vault_read_with_secret_missing() {
    let engine = WasmSkillEngine::new().unwrap();
    let wasm_bytes = load_wasm_bytes();
    let manifest = load_manifest();
    engine
        .load_module("echo-skill", 1, &wasm_bytes)
        .await
        .unwrap();

    // Empty vault — every `get` returns None.
    let store = Arc::new(MockSecretStore::new(&[]));

    let request = WasmToolRequest {
        tool_name: "echo-skill.vault_read".into(),
        handler: "vault_read".into(),
        input: serde_json::json!({"key": "nonexistent"}),
        agent_id: "agent-1".into(),
        user_id: Some("agent-1-user".into()),
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(store as Arc<dyn SecretStore>),
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["found"], false);
}

// ─── Config host function ───────────────────────────────────────────────────

#[tokio::test]
async fn config_read_with_value_present() {
    let (engine, manifest) = setup_engine().await;

    let mut config = HashMap::new();
    config.insert("units".to_string(), "metric".to_string());
    config.insert("api_url".to_string(), "https://api.example.com".to_string());

    let request = WasmToolRequest {
        tool_name: "echo-skill.config_read".into(),
        handler: "config_read".into(),
        input: serde_json::json!({"key": "units"}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool("echo-skill", 1, &manifest, &request, &config, None, None)
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["found"], true);
    assert_eq!(response.result["value"], "metric");
}

#[tokio::test]
async fn config_read_with_value_missing() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.config_read".into(),
        handler: "config_read".into(),
        input: serde_json::json!({"key": "nonexistent"}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success);
    assert_eq!(response.result["found"], false);
}

// ─── Timeout enforcement via epoch interruption ─────────────────────────────

#[tokio::test]
async fn infinite_loop_handler_is_terminated_by_timeout() {
    let (engine, manifest) = setup_engine().await;

    let request = WasmToolRequest {
        tool_name: "echo-skill.infinite_loop".into(),
        handler: "infinite_loop".into(),
        input: serde_json::json!({}),
        agent_id: "agent-1".into(),
        user_id: None,
        task_id: None,
    };

    let start = std::time::Instant::now();
    let result = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await;

    let elapsed = start.elapsed();

    // Must fail (not hang forever)
    assert!(result.is_err(), "infinite loop should be interrupted");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("trapped") || err.contains("interrupt") || err.contains("epoch"),
        "error should indicate epoch interrupt, got: {}",
        err
    );

    // Must complete in a reasonable time
    assert!(
        elapsed.as_secs() < 35,
        "timeout should fire within 35s, took {}s",
        elapsed.as_secs()
    );
}

// ─── Batch tool invocation + HTTP budget through the WASM boundary ──────────

/// Answers every call with an envelope naming the tool — enough to verify
/// ordering, and that the host actually dispatched.
struct StubInvoker;

impl workforce_wasm_runtime::types::SkillToolInvoker for StubInvoker {
    fn invoke(
        &self,
        _ctx: &workforce_wasm_runtime::types::SkillCallContext,
        permission: &workforce_wasm_runtime::manifest::ManifestPermission,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "success": true,
            "output": {"tool": permission.tool, "args": args},
            "error": null,
        }))
    }
}

/// An invoker whose `big.rows` answers with far more than the inline limit.
struct BigInvoker;

fn big_rows() -> serde_json::Value {
    serde_json::Value::Array(
        (0..700)
            .map(|i| serde_json::json!({ "row": i, "text": "x".repeat(1000) }))
            .collect(),
    )
}

impl workforce_wasm_runtime::types::SkillToolInvoker for BigInvoker {
    fn invoke(
        &self,
        ctx: &workforce_wasm_runtime::types::SkillCallContext,
        permission: &workforce_wasm_runtime::manifest::ManifestPermission,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if permission.tool == "big.rows" {
            return Ok(serde_json::json!({ "success": true, "output": big_rows(), "error": null }));
        }
        StubInvoker.invoke(ctx, permission, args)
    }
}

#[tokio::test]
async fn an_oversized_batch_element_is_offloaded_and_read_back_inside_the_guest() {
    let dir = Box::leak(Box::new(tempfile::TempDir::new().unwrap()));
    workforce_agent::artifacts::set_global_store(
        workforce_agent::artifacts::FsArtifactStore::new(dir.path().to_path_buf()).unwrap(),
    );
    let (engine, manifest) = setup_engine().await;
    let request = invoke_many_request(serde_json::json!([
        {"tool": "echo.say", "args": {"i": 0}},
        {"tool": "big.rows", "args": {}},
        {"tool": "flaky.call", "args": {"i": 2}},
    ]));

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(tools_granted_store(&manifest)),
            Some(Arc::new(BigInvoker)),
        )
        .await
        .unwrap();

    assert!(response.success, "{:?}", response.error);
    let results = response.result["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["output"]["tool"], "echo.say");
    assert_eq!(results[1]["success"], true, "{}", results[1]);
    assert_eq!(results[1]["output"], big_rows());
    assert_eq!(results[2]["output"]["tool"], "flaky.call");
}

/// A store holding a tools grant whose build fingerprint is deliberately
/// wrong but whose contract fingerprint is current — proving live that
/// grants survive pure-logic rebuilds.
fn tools_granted_store(manifest: &SkillManifest) -> Arc<MockSecretStore> {
    let record = serde_json::json!({
        "fingerprint": "not-this-build",
        "contract_fingerprint": workforce_wasm_runtime::types::contract_fingerprint(manifest),
    })
    .to_string();
    Arc::new(MockSecretStore::new(&[(
        "wasm-skills/echo-skill/grants/users/alice/_tools",
        record.as_str(),
    )]))
}

fn invoke_many_request(calls: serde_json::Value) -> WasmToolRequest {
    WasmToolRequest {
        tool_name: "echo-skill.invoke_many_test".into(),
        handler: "invoke_many_test".into(),
        input: serde_json::json!({ "calls": calls }),
        agent_id: "test-agent".into(),
        user_id: Some("alice".into()),
        task_id: None,
    }
}

#[tokio::test]
async fn invoke_many_preserves_order_and_isolates_denials() {
    let (engine, manifest) = setup_engine().await;
    let request = invoke_many_request(serde_json::json!([
        {"tool": "echo.say", "args": {"i": 0}},
        {"tool": "not.declared", "args": {}},
        {"tool": "flaky.call", "args": {"i": 2}},
    ]));

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(tools_granted_store(&manifest)),
            Some(Arc::new(StubInvoker)),
        )
        .await
        .unwrap();

    assert!(response.success, "{:?}", response.error);
    let results = response.result["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["output"]["tool"], "echo.say");
    assert_eq!(results[0]["output"]["args"]["i"], 0);
    assert_eq!(results[1]["success"], false);
    assert!(results[1]["error"]
        .as_str()
        .unwrap()
        .contains("not declared"));
    assert_eq!(results[2]["output"]["tool"], "flaky.call");
    assert!(
        response.policy_notice.unwrap().contains("not declared"),
        "the first denial must surface as the policy notice"
    );
}

#[tokio::test]
async fn invoke_many_batch_over_budget_is_refused() {
    let (engine, manifest) = setup_engine().await;
    let calls: Vec<serde_json::Value> = (0..33)
        .map(|i| serde_json::json!({"tool": "echo.say", "args": {"i": i}}))
        .collect();
    let request = invoke_many_request(serde_json::Value::Array(calls));

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(tools_granted_store(&manifest)),
            Some(Arc::new(StubInvoker)),
        )
        .await
        .unwrap();

    assert!(response.result["batch_error"]
        .as_str()
        .unwrap()
        .contains("budget"));
    assert!(response.policy_notice.unwrap().contains("32-call budget"));
}

#[tokio::test]
async fn invoke_many_secret_markers_are_banned() {
    let (engine, manifest) = setup_engine().await;
    let request = invoke_many_request(serde_json::json!([
        {"tool": "echo.say", "args": {"password": "[[secret:prod_password]]"}},
    ]));

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(tools_granted_store(&manifest)),
            Some(Arc::new(StubInvoker)),
        )
        .await
        .unwrap();

    assert!(response.result["batch_error"]
        .as_str()
        .unwrap()
        .contains("denied by platform policy"));
    assert!(response.policy_notice.unwrap().contains("markers"));
}

#[tokio::test]
async fn invoke_many_without_current_grant_is_refused_with_remedy() {
    let (engine, manifest) = setup_engine().await;
    let stale = Arc::new(MockSecretStore::new(&[(
        "wasm-skills/echo-skill/grants/users/alice/_tools",
        r#"{"fingerprint": "old", "contract_fingerprint": "old-contract"}"#,
    )]));
    let request = invoke_many_request(serde_json::json!([
        {"tool": "echo.say", "args": {}},
    ]));

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            Some(stale),
            Some(Arc::new(StubInvoker)),
        )
        .await
        .unwrap();

    assert!(response.result["batch_error"]
        .as_str()
        .unwrap()
        .contains("denied by platform policy"));
    let notice = response.policy_notice.unwrap();
    assert!(notice.contains("stale"));
    assert!(notice.contains("cmd: skill grant echo-skill tools"));
}

#[tokio::test]
async fn http_fetch_budget_exhausts_with_notice() {
    let (engine, manifest) = setup_engine().await;
    let request = WasmToolRequest {
        tool_name: "echo-skill.fetch_burst".into(),
        handler: "fetch_burst".into(),
        // No [network] block in the fixture manifest, so every fetch is
        // allowlist-refused without touching the network — but each attempt
        // still consumes budget, and attempts past the cap flip the notice
        // from the allowlist remedy to the budget one.
        input: serde_json::json!({"count": 70, "url": "https://blocked.example.com/"}),
        agent_id: "test-agent".into(),
        user_id: None,
        task_id: None,
    };

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &request,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();

    assert!(response.success, "{:?}", response.error);
    assert_eq!(response.result["completed"], 0);
    assert!(response
        .policy_notice
        .unwrap()
        .contains("exhausted its HTTP budget (64"));
}

// ─── Manifest TOML round-trip with complex schema ───────────────────────────

#[test]
fn manifest_toml_round_trip_with_complex_schema() {
    use workforce_wasm_runtime::manifest::SkillManifest;

    let original_toml = r#"
[skill]
name = "complex-skill"
version = "1.0.0"
description = "Skill with complex input schemas"

[capabilities]
http = true
vault = true

[[tools]]
name = "search"
description = "Search with complex filters"

[tools.input_schema]
type = "object"
required = ["query"]

[tools.input_schema.properties.query]
type = "string"
description = "Search query"

[tools.input_schema.properties.limit]
type = "integer"
description = "Max results"

[tools.input_schema.properties.filters]
type = "object"
description = "Optional filters"
"#;

    let manifest = SkillManifest::from_toml(original_toml).expect("parse original");
    assert_eq!(manifest.name, "complex-skill");
    assert_eq!(manifest.tools.len(), 1);
    assert_eq!(manifest.tools[0].input_schema["type"], "object");
    assert_eq!(
        manifest.tools[0].input_schema["properties"]["query"]["type"],
        "string"
    );

    // Round-trip: to_toml() then from_toml()
    let toml_str = manifest.to_toml().expect("serialize");
    let reparsed = SkillManifest::from_toml(&toml_str).expect("re-parse");

    assert_eq!(reparsed.name, "complex-skill");
    assert_eq!(reparsed.tools.len(), 1);
    assert_eq!(reparsed.tools[0].name, "search");
    assert_eq!(reparsed.tools[0].input_schema["type"], "object");
    assert_eq!(
        reparsed.tools[0].input_schema["properties"]["query"]["type"],
        "string"
    );
    assert_eq!(
        reparsed.tools[0].input_schema["properties"]["limit"]["type"],
        "integer"
    );
}

// ─── Approval gate: the dry run is the one path that must stay open ─────────

/// `wasm_skill.test` dry-runs an installed tool by dispatching straight to the
/// engine, with no `Tool` bridge and so no gate. That has to stay true:
/// needing an approver to dry-run a gated tool would make the gate untestable
/// and push authors to try it in production instead. The bridge that agents
/// actually call still reports the requirement.
#[tokio::test]
async fn a_gated_tool_still_dry_runs_while_its_bridge_asks_for_approval() {
    let (engine, mut manifest) = setup_engine().await;
    manifest.tools[0].requires_approval = true;
    manifest.tools[0].approval_summary = Some("echo `{{message}}`".into());
    manifest.validate().expect("a gated manifest is valid");

    let response = engine
        .execute_tool(
            "echo-skill",
            1,
            &manifest,
            &WasmToolRequest {
                tool_name: "echo-skill.echo".into(),
                handler: "echo".into(),
                input: serde_json::json!({"message": "dry run"}),
                agent_id: "test-agent".into(),
                user_id: None,
                task_id: None,
            },
            &HashMap::new(),
            None,
            None,
        )
        .await
        .expect("dry run executes");
    assert!(response.success, "{:?}", response.error);
    assert_eq!(response.result["echoed"], "dry run");

    let tools = WasmSkillTool::from_manifest(
        &manifest,
        Arc::new(engine),
        1,
        HashMap::new(),
        None,
        SkillRuntimeBindings::unbound(),
    );
    let echo = tools
        .iter()
        .find(|t| t.name() == "echo-skill.echo")
        .expect("the bridge exists");
    assert!(echo.approval_requirement().is_some());
    assert_eq!(
        echo.approval_summary(&serde_json::json!({"message": "dry run"})),
        Some("echo `dry run`".to_string())
    );
    assert!(tools
        .iter()
        .find(|t| t.name() == "echo-skill.add")
        .expect("a sibling tool")
        .approval_requirement()
        .is_none());
}

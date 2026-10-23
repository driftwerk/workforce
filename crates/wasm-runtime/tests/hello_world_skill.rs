//! End-to-end smoke test for the hello-world WASM skill in
//! `~/dev/projects/driftwerk/workforce-skills/hello-world/`.
//!
//! This test exercises the **install → load → register → dispatch** path
//! with a real (non-fixture) skill and validates that:
//!  * The new `Skill.toml` schema (with `[[credentials]] scope = ...`,
//!    `[network] allowed_hosts`, `#[serde(deny_unknown_fields)]`) parses.
//!  * `WasmToolRequest.user_id` propagates through the engine and surfaces
//!    inside the WASM guest as `ToolInput.user_id`.
//!  * Per-user vault scoping (`wasm-skills/{skill}/users/{uid}/...`) and
//!    Shared scoping (`wasm-skills/{skill}/shared/...`) both resolve to
//!    the right paths against an in-memory `SecretStore`.
//!
//! The .wasm artifacts must be built first:
//!
//! ```sh
//! cd ~/dev/projects/driftwerk/workforce-skills
//! cargo build --target wasm32-unknown-unknown --release
//! ```
//!
//! Skipped (with a `println!`) if the artifact isn't present, so CI
//! without the workforce-skills sibling repo doesn't fail.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use workforce_common::settings::SecretStore;
use workforce_wasm_runtime::engine::WasmSkillEngine;
use workforce_wasm_runtime::manifest::SkillManifest;
use workforce_wasm_runtime::types::WasmToolRequest;

const HELLO_WORLD_WASM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../workforce-skills/target/wasm32-unknown-unknown/release/hello_world.wasm"
);
const HELLO_WORLD_MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../workforce-skills/hello-world/Skill.toml"
);
const HELLO_VAULT_WASM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../workforce-skills/target/wasm32-unknown-unknown/release/hello_vault.wasm"
);
const HELLO_VAULT_MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../workforce-skills/hello-vault/Skill.toml"
);

fn artifact_present(path: &str) -> bool {
    PathBuf::from(path).exists()
}

#[derive(Default)]
struct InMemoryStore {
    data: Mutex<HashMap<String, String>>,
}

impl SecretStore for InMemoryStore {
    fn get(&self, p: &str) -> Option<String> {
        self.data.lock().unwrap().get(p).cloned()
    }
    fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
        self.data
            .lock()
            .unwrap()
            .insert(p.to_string(), v.to_string());
        Ok(())
    }
    fn delete(&self, p: &str) -> eyre::Result<bool> {
        Ok(self.data.lock().unwrap().remove(p).is_some())
    }
    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let m = self.data.lock().unwrap();
        match prefix {
            Some(p) => m.keys().filter(|k| k.starts_with(p)).cloned().collect(),
            None => m.keys().cloned().collect(),
        }
    }
}

#[tokio::test]
async fn hello_world_round_trip() {
    if !artifact_present(HELLO_WORLD_WASM) {
        println!(
            "skipping hello_world_round_trip — build workforce-skills first: \
             cd ~/dev/projects/driftwerk/workforce-skills && cargo build \
             --target wasm32-unknown-unknown --release"
        );
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_WORLD_MANIFEST))
        .expect("parse hello-world Skill.toml");
    assert_eq!(manifest.name, "hello-world");
    assert!(
        manifest.allowed_hosts.is_empty(),
        "hello-world declares no hosts"
    );

    let wasm = std::fs::read(HELLO_WORLD_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();

    let req = WasmToolRequest {
        tool_name: "hello-world.hello".into(),
        handler: "hello".into(),
        input: serde_json::json!({"name": "workforce"}),
        agent_id: "agent-test".into(),
        user_id: Some("alice".into()),
        task_id: None,
    };
    let resp = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(resp.success);
    assert_eq!(resp.result["greeting"], "Hello, workforce!");
}

#[tokio::test]
async fn hello_world_threads_user_id_through_to_guest() {
    if !artifact_present(HELLO_WORLD_WASM) {
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_WORLD_MANIFEST)).unwrap();
    let wasm = std::fs::read(HELLO_WORLD_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();

    let req = WasmToolRequest {
        tool_name: "hello-world.echo".into(),
        handler: "echo".into(),
        input: serde_json::json!({"message": "ping"}),
        agent_id: "agent-test".into(),
        user_id: Some("alice".into()),
        task_id: None,
    };
    let resp = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(resp.success);
    assert_eq!(resp.result["message"], "ping");
    assert_eq!(resp.result["agent_id"], "agent-test");
    assert_eq!(
        resp.result["user_id"], "alice",
        "the WASM guest must observe the host-injected user_id"
    );
}

#[tokio::test]
async fn hello_world_user_id_null_for_system_jobs() {
    if !artifact_present(HELLO_WORLD_WASM) {
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_WORLD_MANIFEST)).unwrap();
    let wasm = std::fs::read(HELLO_WORLD_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();

    let req = WasmToolRequest {
        tool_name: "hello-world.echo".into(),
        handler: "echo".into(),
        input: serde_json::json!({"message": "system call"}),
        agent_id: "agent-test".into(),
        user_id: None,
        task_id: None,
    };
    let resp = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(resp.success);
    assert!(
        resp.result["user_id"].is_null(),
        "missing user_id should round-trip as null"
    );
}

#[tokio::test]
async fn hello_vault_per_user_isolation() {
    if !artifact_present(HELLO_VAULT_WASM) {
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_VAULT_MANIFEST)).unwrap();
    assert!(
        manifest.has_capability(workforce_wasm_runtime::types::Capability::Vault),
        "hello-vault must declare the vault capability"
    );
    let per_user = manifest
        .credentials
        .iter()
        .find(|c| c.name == "secret_per_user")
        .unwrap();
    let shared = manifest
        .credentials
        .iter()
        .find(|c| c.name == "secret_shared")
        .unwrap();
    assert_eq!(
        per_user.scope,
        workforce_wasm_runtime::types::CredentialScope::User,
        "secret_per_user defaults to User scope"
    );
    assert_eq!(
        shared.scope,
        workforce_wasm_runtime::types::CredentialScope::Shared,
        "secret_shared declares Shared scope"
    );

    let wasm = std::fs::read(HELLO_VAULT_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();
    let store: Arc<dyn SecretStore> = Arc::new(InMemoryStore::default());

    // Alice writes "alice-secret".
    let req = WasmToolRequest {
        tool_name: "hello-vault.set_per_user".into(),
        handler: "set_per_user".into(),
        input: serde_json::json!({"value": "alice-secret"}),
        agent_id: "a".into(),
        user_id: Some("alice".into()),
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    assert!(r.success, "alice set_per_user: {:?}", r);

    // Bob writes a different value to the same logical key.
    let req = WasmToolRequest {
        tool_name: "hello-vault.set_per_user".into(),
        handler: "set_per_user".into(),
        input: serde_json::json!({"value": "bob-secret"}),
        agent_id: "a".into(),
        user_id: Some("bob".into()),
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    assert!(r.success, "bob set_per_user: {:?}", r);

    // Confirm raw vault paths.
    assert_eq!(
        store
            .get("wasm-skills/hello-vault/users/alice/secret_per_user")
            .as_deref(),
        Some("alice-secret")
    );
    assert_eq!(
        store
            .get("wasm-skills/hello-vault/users/bob/secret_per_user")
            .as_deref(),
        Some("bob-secret")
    );

    // Alice reads back her own value.
    let req = WasmToolRequest {
        tool_name: "hello-vault.get_per_user".into(),
        handler: "get_per_user".into(),
        input: serde_json::json!({}),
        agent_id: "a".into(),
        user_id: Some("alice".into()),
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.result["value"], "alice-secret");
    assert_eq!(r.result["found"], true);

    // Bob reads back his own value.
    let req = WasmToolRequest {
        tool_name: "hello-vault.get_per_user".into(),
        handler: "get_per_user".into(),
        input: serde_json::json!({}),
        agent_id: "a".into(),
        user_id: Some("bob".into()),
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.result["value"], "bob-secret");
}

#[tokio::test]
async fn hello_vault_shared_is_cross_user() {
    if !artifact_present(HELLO_VAULT_WASM) {
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_VAULT_MANIFEST)).unwrap();
    let wasm = std::fs::read(HELLO_VAULT_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();
    let store: Arc<dyn SecretStore> = Arc::new(InMemoryStore::default());

    // Alice sets the shared secret.
    let req = WasmToolRequest {
        tool_name: "hello-vault.set_shared".into(),
        handler: "set_shared".into(),
        input: serde_json::json!({"value": "org-token"}),
        agent_id: "a".into(),
        user_id: Some("alice".into()),
        task_id: None,
    };
    engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();

    // The path is shared, not per-user.
    assert!(store
        .get("wasm-skills/hello-vault/shared/secret_shared")
        .is_some());
    assert!(store
        .get("wasm-skills/hello-vault/users/alice/secret_shared")
        .is_none());

    // Bob reads the same value.
    let req = WasmToolRequest {
        tool_name: "hello-vault.get_shared".into(),
        handler: "get_shared".into(),
        input: serde_json::json!({}),
        agent_id: "a".into(),
        user_id: Some("bob".into()),
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.result["value"], "org-token");
    assert_eq!(r.result["scope"], "shared");
}

#[tokio::test]
async fn hello_vault_per_user_hard_fails_without_identity() {
    if !artifact_present(HELLO_VAULT_WASM) {
        return;
    }

    let manifest = SkillManifest::from_file(&PathBuf::from(HELLO_VAULT_MANIFEST)).unwrap();
    let wasm = std::fs::read(HELLO_VAULT_WASM).unwrap();
    let engine = WasmSkillEngine::new().unwrap();
    engine.load_module(&manifest.name, 1, &wasm).await.unwrap();
    let store: Arc<dyn SecretStore> = Arc::new(InMemoryStore::default());

    // No user_id, no system identity. The host must reject the per-user
    // vault read rather than silently fall through.
    let req = WasmToolRequest {
        tool_name: "hello-vault.set_per_user".into(),
        handler: "set_per_user".into(),
        input: serde_json::json!({"value": "should-fail"}),
        agent_id: "a".into(),
        user_id: None,
        task_id: None,
    };
    let r = engine
        .execute_tool(
            &manifest.name,
            1,
            &manifest,
            &req,
            &HashMap::new(),
            Some(Arc::clone(&store)),
            None,
        )
        .await
        .unwrap();
    // The guest's `vault::set` returned `false`, which the skill turns
    // into a tool error.
    assert!(!r.success, "per-user write must fail without identity");
}

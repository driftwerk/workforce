use std::io::Cursor;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use uuid::Uuid;
use workforce_common::locking::LockManager;
use workforce_common::settings::{
    AgentConfig, AgentLlmConfig, AgentWorkspaceConfig, LlmVideoGenerationConfig, OwnerType,
    WorkforceConfig,
};
use workforce_core::types::AgentId;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};

use super::model::{IdentityResolutionKind, ImportOptions};
use super::{
    apply_import, export_bundle, plan_import, BundleReader, BundleWriter, TransferContext,
};

const BASE_CONFIG: &str = r#"
[nats]
url = "nats://localhost:4222"

[llm]
provider = "test"
default_model = "test-model"
max_tokens = 4096
max_rounds = 8

[[agents]]
id = "base"
default = true
"#;

/// A workspace config.toml carrying a secret the export must strip.
const WORKSPACE_WITH_SECRET: &str = "[llm]\napi_key = \"workspace-secret\"\n";

fn write_source_agent(
    data_dir: &Path,
    agent_id: &str,
    owner: &str,
    session_id: &str,
    workspace_config: &str,
) {
    let workspace = data_dir.join("agents").join(agent_id);
    let sessions = workspace.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(workspace.join("SOUL.md"), b"a soul").unwrap();
    std::fs::write(workspace.join("config.toml"), workspace_config).unwrap();
    let meta = serde_json::json!({
        "id": session_id,
        "label": null,
        "channel": "slack",
        "owner": owner,
        "agent_id": agent_id,
        "kind": "main",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "metadata": { "mode": "claude-chat", "started_by": owner, "chat_agent_id": agent_id },
        "total_tokens": 0,
        "message_count": 1,
    });
    std::fs::write(
        sessions.join(format!("{session_id}.meta.json")),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
    std::fs::write(
        sessions.join(format!("{session_id}.jsonl")),
        b"{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hi\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"tokens\":1}\n",
    )
    .unwrap();
}

async fn seed_memory(store: &InMemoryStore, agent_id: &str, owner: &str) {
    let mut memory = Memory::new(
        "a remembered fact",
        MemoryLayer::Semantic,
        AgentId::new(agent_id),
    );
    memory.identity_id = Some(owner.to_string());
    store.remember(memory).await.unwrap();
}

fn source_config(agent_id: &str) -> WorkforceConfig {
    let mut config = WorkforceConfig::from_toml(BASE_CONFIG).unwrap();
    config.agents.push(AgentConfig {
        id: agent_id.into(),
        owner_type: OwnerType::User,
        llm: Some(AgentLlmConfig {
            provider: Some("anthropic".into()),
            api_key: Some("super-secret".into()),
            video: Some(LlmVideoGenerationConfig {
                api_key: Some("media-secret".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    config
}

struct Install {
    _temp: TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
    memory: InMemoryStore,
    identity: SqliteIdentityStore,
    locks: LockManager,
}

impl Install {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config_path = temp.path().join("workforce.toml");
        std::fs::write(&config_path, BASE_CONFIG).unwrap();
        Self {
            _temp: temp,
            data_dir,
            config_path,
            memory: InMemoryStore::new(),
            identity: SqliteIdentityStore::in_memory().unwrap(),
            locks: LockManager::new(),
        }
    }

    fn context<'a>(
        &'a self,
        config: &'a WorkforceConfig,
        install_id: &str,
        actor: Option<String>,
    ) -> TransferContext<'a> {
        TransferContext {
            data_dir: self.data_dir.clone(),
            base_config_path: self.config_path.clone(),
            config,
            identity_store: &self.identity,
            memory_store: &self.memory,
            lock_manager: &self.locks,
            def_store: None,
            actor,
            install_id: install_id.to_string(),
            slack_team_id: None,
        }
    }
}

async fn export_install(
    install: &Install,
    install_id: &str,
    agent: &str,
    owner_email: &str,
    workspace_config: &str,
) -> Vec<u8> {
    let owner = Identity::new("Kings", IdentityRole::Member).with_email(owner_email);
    install.identity.create(&owner).unwrap();
    let session_id = Uuid::new_v4().to_string();
    write_source_agent(
        &install.data_dir,
        agent,
        &owner.id.to_string(),
        &session_id,
        workspace_config,
    );
    seed_memory(&install.memory, agent, &owner.id.to_string()).await;

    let config = source_config(agent);
    let ctx = install.context(&config, install_id, None);
    let (cursor, counts) = export_bundle(&ctx, &[agent.to_string()], Cursor::new(Vec::new()))
        .await
        .unwrap();
    assert_eq!(counts.agents, 1);
    assert_eq!(counts.sessions, 1);
    assert_eq!(counts.memories, 1);
    assert_eq!(counts.identities, 1);
    cursor.into_inner()
}

fn manifest(checksums: std::collections::BTreeMap<String, String>) -> super::model::Manifest {
    super::model::Manifest {
        bundle_version: super::model::BUNDLE_VERSION,
        source_install_id: "t".into(),
        source_slack_team_id: None,
        exported_at: chrono::Utc::now(),
        agents: vec![],
        schema_versions: Default::default(),
        counts: Default::default(),
        checksums,
    }
}

#[test]
fn bundle_roundtrips() {
    let mut writer = BundleWriter::new(Cursor::new(Vec::new()));
    writer.add_bytes("a.txt", b"hello").unwrap();
    writer.add_ndjson("nums.ndjson", &[1u32, 2, 3]).unwrap();
    let bytes = writer
        .finish(manifest(Default::default()))
        .unwrap()
        .into_inner();

    let mut reader = BundleReader::new(Cursor::new(bytes)).unwrap();
    reader.verify_checksums().unwrap();
    assert_eq!(reader.read_bytes("a.txt").unwrap(), b"hello");
    assert_eq!(
        reader.read_ndjson::<u32>("nums.ndjson").unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(reader.entries_with_prefix("a").len(), 1);
}

#[test]
fn verify_checksums_rejects_content_mismatch() {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("a.txt", options).unwrap();
    zip.write_all(b"hello").unwrap();
    let wrong = [("a.txt".to_string(), "0".repeat(64))]
        .into_iter()
        .collect();
    zip.start_file(super::model::MANIFEST_FILE, options)
        .unwrap();
    zip.write_all(&serde_json::to_vec(&manifest(wrong)).unwrap())
        .unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let mut reader = BundleReader::new(Cursor::new(bytes)).unwrap();
    assert!(reader.verify_checksums().is_err());
}

#[tokio::test]
async fn export_strips_secrets_and_slices_identities() {
    let install = Install::new();
    let bytes = export_install(
        &install,
        "installA",
        "allan",
        "kings@example.com",
        WORKSPACE_WITH_SECRET,
    )
    .await;

    let mut reader = BundleReader::new(Cursor::new(bytes)).unwrap();
    reader.verify_checksums().unwrap();
    let config: AgentConfig = reader.read_json("agents/allan/agent.json").unwrap();
    let llm = config.llm.unwrap();
    assert!(llm.api_key.is_none(), "inline api_key must be stripped");
    assert!(llm.video.is_none(), "media-block secret must be stripped");
    assert_eq!(
        llm.provider.as_deref(),
        Some("anthropic"),
        "non-secret config kept"
    );
    assert!(config.workspace.is_none());

    // The workspace config.toml is sanitized too, not copied verbatim.
    let workspace_toml =
        String::from_utf8(reader.read_bytes("agents/allan/workspace.toml").unwrap()).unwrap();
    assert!(!workspace_toml.contains("workspace-secret"));
    let workspace_config: AgentWorkspaceConfig = toml::from_str(&workspace_toml).unwrap();
    assert!(workspace_config.llm.is_none_or(|l| l.api_key.is_none()));

    let identities: Vec<Identity> = reader.read_ndjson("identities.ndjson").unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].email.as_deref(), Some("kings@example.com"));
}

#[tokio::test]
async fn import_into_empty_then_merge_with_collision() {
    let source_a = Install::new();
    let bundle_a = export_install(
        &source_a,
        "installA",
        "allan",
        "kings@example.com",
        WORKSPACE_WITH_SECRET,
    )
    .await;

    // Fresh target: import A cleanly, no collisions.
    let target = Install::new();
    let base = WorkforceConfig::from_toml(BASE_CONFIG).unwrap();
    let opts = ImportOptions {
        confirm: true,
        ..Default::default()
    };
    let outcome = apply_import(
        &target.context(&base, "target", Some("admin".into())),
        Cursor::new(bundle_a),
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(outcome.agents_imported, vec!["allan".to_string()]);
    assert!(outcome.renames_applied.is_empty());
    assert_eq!(outcome.identities_created, 1);
    assert_eq!(outcome.sessions_imported, 1);
    assert_eq!(outcome.memories_imported, 1);

    let imported_email = "kings@example.com";
    let matched = target
        .identity
        .get_by_email(imported_email)
        .unwrap()
        .unwrap();
    let allan_sessions = target.data_dir.join("agents/allan/sessions");
    assert_eq!(std::fs::read_dir(&allan_sessions).unwrap().count(), 2);
    assert!(target.data_dir.join("agents/allan/SOUL.md").exists());
    let allan_memories = target
        .memory
        .list(MemoryFilter {
            owner: Some(AgentId::new("allan")),
            system_unscoped: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(allan_memories.len(), 1);
    assert_eq!(
        allan_memories[0].identity_id.as_deref(),
        Some(matched.id.to_string().as_str())
    );

    // Second install B: same agent name, same human (same email), different uuid.
    let source_b = Install::new();
    let bundle_b = export_install(
        &source_b,
        "installB",
        "allan",
        imported_email,
        WORKSPACE_WITH_SECRET,
    )
    .await;

    // Target now has "allan" (simulate the effective config reload the CLI/API do).
    let mut reloaded = WorkforceConfig::from_toml(BASE_CONFIG).unwrap();
    reloaded.agents.push(AgentConfig {
        id: "allan".into(),
        ..Default::default()
    });
    let plan = plan_import(
        &target.context(&reloaded, "target", Some("admin".into())),
        Cursor::new(bundle_b.clone()),
        &opts,
    )
    .unwrap();
    let allan_plan = plan.agents.iter().find(|a| a.source_id == "allan").unwrap();
    assert!(allan_plan.collides);
    assert_eq!(allan_plan.target_id, "allan-installB");
    assert!(matches!(
        plan.identities[0].resolution,
        IdentityResolutionKind::Matched { .. }
    ));

    let outcome_b = apply_import(
        &target.context(&reloaded, "target", Some("admin".into())),
        Cursor::new(bundle_b),
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome_b.agents_imported,
        vec!["allan-installB".to_string()]
    );
    assert_eq!(
        outcome_b.renames_applied.get("allan").map(String::as_str),
        Some("allan-installB")
    );
    assert_eq!(
        outcome_b.identities_created, 0,
        "same email must reconcile, not duplicate"
    );
    assert_eq!(outcome_b.identities_matched, 1);

    // B's per-user memory landed under the renamed agent, re-stamped onto the reconciled identity.
    let renamed_memories = target
        .memory
        .list(MemoryFilter {
            owner: Some(AgentId::new("allan-installB")),
            system_unscoped: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(renamed_memories.len(), 1);
    assert_eq!(
        renamed_memories[0].identity_id.as_deref(),
        Some(matched.id.to_string().as_str())
    );

    // The session's id-bearing fields are all remapped: top-level agent_id/owner
    // and the claude-chat metadata (chat_agent_id via the agent map, started_by
    // via the identity map).
    let renamed_dir = target.data_dir.join("agents/allan-installB/sessions");
    let meta_path = std::fs::read_dir(&renamed_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().ends_with(".meta.json"))
        .unwrap();
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(meta_path).unwrap()).unwrap();
    assert_eq!(meta["agent_id"], "allan-installB");
    assert_eq!(meta["owner"], matched.id.to_string());
    assert_eq!(meta["metadata"]["chat_agent_id"], "allan-installB");
    assert_eq!(meta["metadata"]["started_by"], matched.id.to_string());
}

/// Import an agent whose workspace config.toml is `workspace_config` into a
/// fresh install, returning the outcome and the config.toml it wrote.
async fn import_workspace(workspace_config: &str) -> (super::model::ImportOutcome, Option<String>) {
    let base = WorkforceConfig::from_toml(BASE_CONFIG).unwrap();
    let source = Install::new();
    let bundle = export_install(
        &source,
        "installA",
        "allan",
        "kings@example.com",
        workspace_config,
    )
    .await;
    let target = Install::new();
    let outcome = apply_import(
        &target.context(&base, "target", Some("admin".into())),
        Cursor::new(bundle),
        &ImportOptions {
            confirm: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let written = std::fs::read_to_string(target.data_dir.join("agents/allan/config.toml")).ok();
    (outcome, written)
}

#[tokio::test]
async fn import_writes_only_a_config_toml_the_agent_would_load() {
    let (outcome, written) = import_workspace("[llm]\nfailover = [\"codex:gpt-5.5\"]\n").await;
    assert_eq!(written, None, "a config the agent would not boot with");
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.contains("config.toml was not imported") && n.contains("remote worker")),
        "{:?}",
        outcome.notes
    );

    let (_, written) = import_workspace("[llm]\nmax_rounds = 12\n").await;
    assert!(written.is_some_and(|body| body.contains("max_rounds = 12")));
}

use std::collections::BTreeSet;
use std::io::{Seek, Write};
use std::path::Path;

use chrono::Utc;
use eyre::Result;
use uuid::Uuid;
use workforce_common::session::is_install_state;
use workforce_common::settings::{AgentConfig, AgentLlmConfig, AgentWorkspaceConfig, OwnerType};
use workforce_core::types::AgentId;
use workforce_identity::types::Identity;
use workforce_memory::types::MemoryFilter;

use super::bundle::BundleWriter;
use super::model::{
    agent_config_entry, agent_identity_entry, agent_memory_entry, agent_sessions_prefix,
    agent_workspace_entry, Counts, Manifest, BUNDLE_VERSION, IDENTITIES_FILE, MEMORY_QUERY_LIMIT,
};
use super::{resolve_workspace, TransferContext};

pub const IDENTITY_FILES: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "TOOLS.md",
    "IDENTITY.md",
    "WELCOME.md",
];

/// Null every inline-credential field of an LLM block: the top-level `api_key`
/// and the whole media sub-blocks (each carries its own `api_key`). Non-secret
/// tuning (provider, model, pacing, routing, account *names*) is preserved.
fn redact_llm_secrets(llm: &mut AgentLlmConfig) {
    llm.api_key = None;
    llm.vision = None;
    llm.transcription = None;
    llm.image = None;
    llm.video = None;
}

/// Drop secrets and install-local paths so the bundle carries portable config
/// only — vault credentials and workspace location are re-established on the target.
fn sanitize_for_export(mut config: AgentConfig) -> AgentConfig {
    config.workspace = None;
    config.vault = None;
    if let Some(llm) = config.llm.as_mut() {
        redact_llm_secrets(llm);
    }
    config
}

fn referenced_owner(config: &AgentConfig) -> Option<Uuid> {
    if config.owner_type != OwnerType::User {
        return None;
    }
    config
        .owner_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
}

async fn export_agent<W: Write + Seek>(
    ctx: &TransferContext<'_>,
    agent_id: &str,
    writer: &mut BundleWriter<W>,
    referenced: &mut BTreeSet<Uuid>,
    counts: &mut Counts,
) -> Result<()> {
    let config = ctx
        .config
        .agent_by_id(agent_id)
        .ok_or_else(|| eyre::eyre!("agent {agent_id} not found in config"))?;
    if let Some(owner) = referenced_owner(config) {
        referenced.insert(owner);
    }
    writer.add_json(
        &agent_config_entry(agent_id),
        &sanitize_for_export(config.clone()),
    )?;

    let workspace = resolve_workspace(ctx, agent_id);
    if let Some(mut workspace_config) = AgentWorkspaceConfig::load(&workspace)? {
        if let Some(llm) = workspace_config.llm.as_mut() {
            redact_llm_secrets(llm);
        }
        writer.add_bytes(
            &agent_workspace_entry(agent_id),
            toml::to_string(&workspace_config)?.as_bytes(),
        )?;
    }
    for file in IDENTITY_FILES {
        let path = workspace.join(file);
        if path.is_file() {
            writer.add_bytes(
                &agent_identity_entry(agent_id, file),
                &std::fs::read(&path)?,
            )?;
        }
    }

    export_sessions(&workspace, agent_id, writer, referenced, counts)?;

    let memories = ctx
        .memory_store
        .list(MemoryFilter {
            owner: Some(AgentId::new(agent_id)),
            system_unscoped: true,
            limit: Some(MEMORY_QUERY_LIMIT),
            ..MemoryFilter::default()
        })
        .await?;
    for memory in &memories {
        if let Some(id) = memory
            .identity_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            referenced.insert(id);
        }
    }
    counts.memories += memories.len();
    writer.add_ndjson(&agent_memory_entry(agent_id), &memories)?;
    Ok(())
}

fn export_sessions<W: Write + Seek>(
    workspace: &Path,
    agent_id: &str,
    writer: &mut BundleWriter<W>,
    referenced: &mut BTreeSet<Uuid>,
    counts: &mut Counts,
) -> Result<()> {
    let sessions_dir = workspace.join("sessions");
    if !sessions_dir.is_dir() {
        return Ok(());
    }
    let prefix = agent_sessions_prefix(agent_id);
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // A turn record or journal names this install's instances and
        // workers; it belongs to this install, not to the conversation.
        // Carried into a bundle it would reach the importing hub as an
        // interrupted turn and be re-run there, delivering to the exporting
        // install's Slack thread.
        if is_install_state(&name) {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        if name.ends_with(".meta.json") {
            counts.sessions += 1;
            if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let started_by = meta.get("metadata").and_then(|m| m.get("started_by"));
                for value in [meta.get("owner"), started_by] {
                    if let Some(id) = value
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                    {
                        referenced.insert(id);
                    }
                }
            }
        }
        writer.add_bytes(&format!("{prefix}{name}"), &bytes)?;
    }
    Ok(())
}

pub async fn export_bundle<W: Write + Seek>(
    ctx: &TransferContext<'_>,
    agent_ids: &[String],
    writer: W,
) -> Result<(W, Counts)> {
    let mut bundle = BundleWriter::new(writer);
    let mut referenced = BTreeSet::new();
    let mut counts = Counts::default();

    for agent_id in agent_ids {
        export_agent(ctx, agent_id, &mut bundle, &mut referenced, &mut counts).await?;
        counts.agents += 1;
    }

    let identities: Vec<Identity> = referenced
        .iter()
        .filter_map(|id| ctx.identity_store.get_by_id(*id).transpose())
        .collect::<Result<_>>()?;
    counts.identities = identities.len();
    bundle.add_ndjson(IDENTITIES_FILE, &identities)?;

    let manifest = Manifest {
        bundle_version: BUNDLE_VERSION,
        source_install_id: ctx.install_id.clone(),
        source_slack_team_id: ctx.slack_team_id.clone(),
        exported_at: Utc::now(),
        agents: agent_ids.to_vec(),
        schema_versions: Default::default(),
        counts: counts.clone(),
        checksums: Default::default(),
    };
    Ok((bundle.finish(manifest)?, counts))
}

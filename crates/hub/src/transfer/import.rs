use std::collections::HashSet;
use std::io::{Read, Seek};
use std::path::Path;

use eyre::{bail, eyre, Result};
use serde_json::json;
use workforce_common::session::is_install_state;
use workforce_common::settings::{AgentConfig, AgentWorkspaceConfig, OwnerType};
use workforce_core::types::AgentId;
use workforce_identity::types::Identity;
use workforce_memory::types::{Memory, MemoryFilter};

use super::bundle::BundleReader;
use super::export::IDENTITY_FILES;
use super::model::{
    agent_config_entry, agent_identity_entry, agent_memory_entry, agent_sessions_prefix,
    agent_workspace_entry, AgentPlan, Counts, IdentityResolution, IdentityResolutionKind,
    ImportOptions, ImportOutcome, Manifest, MergePlan, RemapPlan, IDENTITIES_FILE,
    MEMORY_QUERY_LIMIT,
};
use super::TransferContext;
use crate::workspace_admission::WorkspaceAdmission;

fn target_agent_id(
    ctx: &TransferContext<'_>,
    manifest: &Manifest,
    opts: &ImportOptions,
    source_id: &str,
) -> String {
    if let Some(rename) = opts.renames.get(source_id) {
        return rename.clone();
    }
    if ctx.config.agent_by_id(source_id).is_some() {
        format!("{source_id}-{}", slugify(&manifest.source_install_id))
    } else {
        source_id.to_string()
    }
}

fn slugify(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn resolve_identity(
    ctx: &TransferContext<'_>,
    identity: &Identity,
    opts: &ImportOptions,
    source_team: Option<&str>,
) -> Result<IdentityResolution> {
    let source_id = identity.id.to_string();
    let resolution = if let Some(target) = opts.identity_overrides.get(&source_id) {
        IdentityResolutionKind::Matched {
            target_id: target.clone(),
        }
    } else if let Some(found) = identity
        .email
        .as_deref()
        .and_then(|email| ctx.identity_store.get_by_email(email).transpose())
        .transpose()?
    {
        IdentityResolutionKind::Matched {
            target_id: found.id.to_string(),
        }
    } else if let Some(found) = ctx
        .slack_workspace_matches(source_team)
        .then(|| identity.linked_ids.get("slack"))
        .flatten()
        .and_then(|slack| {
            ctx.identity_store
                .get_by_linked_id("slack", slack)
                .transpose()
        })
        .transpose()?
    {
        IdentityResolutionKind::Matched {
            target_id: found.id.to_string(),
        }
    } else if ctx.identity_store.get_by_id(identity.id)?.is_some() {
        IdentityResolutionKind::Matched {
            target_id: source_id.clone(),
        }
    } else {
        IdentityResolutionKind::Create {
            target_id: source_id.clone(),
        }
    };
    Ok(IdentityResolution {
        source_id,
        email: identity.email.clone(),
        resolution,
    })
}

fn reconcile(
    ctx: &TransferContext<'_>,
    identities: &[Identity],
    opts: &ImportOptions,
    source_team: Option<&str>,
) -> Result<Vec<IdentityResolution>> {
    identities
        .iter()
        .map(|identity| resolve_identity(ctx, identity, opts, source_team))
        .collect()
}

fn build_remap(
    agent_targets: &[(String, String)],
    resolutions: &[IdentityResolution],
) -> RemapPlan {
    RemapPlan {
        agents: agent_targets.iter().cloned().collect(),
        identities: resolutions
            .iter()
            .map(|r| (r.source_id.clone(), r.target_id().to_string()))
            .collect(),
    }
}

fn expected_credentials(config: &AgentConfig) -> Vec<String> {
    config
        .llm
        .as_ref()
        .and_then(|llm| llm.provider.clone())
        .map(|provider| vec![format!("llm provider '{provider}' credentials")])
        .unwrap_or_default()
}

pub fn plan_import<R: Read + Seek>(
    ctx: &TransferContext<'_>,
    reader: R,
    opts: &ImportOptions,
) -> Result<MergePlan> {
    let mut bundle = BundleReader::new(reader)?;
    bundle.verify_checksums()?;
    let manifest = bundle.manifest()?;
    let identities: Vec<Identity> = bundle.read_ndjson(IDENTITIES_FILE)?;
    let resolutions = reconcile(
        ctx,
        &identities,
        opts,
        manifest.source_slack_team_id.as_deref(),
    )?;

    let agents = manifest
        .agents
        .iter()
        .map(|source_id| {
            let target_id = target_agent_id(ctx, &manifest, opts, source_id);
            let config: AgentConfig = bundle.read_json(&agent_config_entry(source_id))?;
            let sessions = bundle
                .entries_with_prefix(&agent_sessions_prefix(source_id))
                .iter()
                .filter(|name| name.ends_with(".meta.json"))
                .count();
            let memories = bundle
                .read_ndjson::<Memory>(&agent_memory_entry(source_id))?
                .len();
            Ok(AgentPlan {
                collides: ctx.config.agent_by_id(source_id).is_some(),
                source_id: source_id.clone(),
                target_id,
                sessions,
                memories,
                expected_credentials: expected_credentials(&config),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(MergePlan {
        source_install_id: manifest.source_install_id.clone(),
        source_slack_team_id: manifest.source_slack_team_id.clone(),
        counts: counts_from(&agents, &resolutions),
        agents,
        identities: resolutions,
    })
}

fn counts_from(agents: &[AgentPlan], resolutions: &[IdentityResolution]) -> Counts {
    Counts {
        agents: agents.len(),
        sessions: agents.iter().map(|a| a.sessions).sum(),
        memories: agents.iter().map(|a| a.memories).sum(),
        identities: resolutions.len(),
    }
}

pub async fn apply_import<R: Read + Seek>(
    ctx: &TransferContext<'_>,
    reader: R,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    if !opts.confirm {
        bail!("import not confirmed");
    }
    let mut bundle = BundleReader::new(reader)?;
    bundle.verify_checksums()?;
    let manifest = bundle.manifest()?;
    let identities: Vec<Identity> = bundle.read_ndjson(IDENTITIES_FILE)?;
    let resolutions = reconcile(
        ctx,
        &identities,
        opts,
        manifest.source_slack_team_id.as_deref(),
    )?;
    let agent_targets: Vec<(String, String)> = manifest
        .agents
        .iter()
        .map(|source_id| {
            (
                source_id.clone(),
                target_agent_id(ctx, &manifest, opts, source_id),
            )
        })
        .collect();
    let remap = build_remap(&agent_targets, &resolutions);

    let mut outcome = ImportOutcome::default();
    apply_identities(ctx, &identities, &resolutions, &mut outcome)?;
    for (source_id, target_id) in &agent_targets {
        apply_agent(ctx, &mut bundle, source_id, target_id, &remap, &mut outcome).await?;
        if source_id != target_id {
            outcome
                .renames_applied
                .insert(source_id.clone(), target_id.clone());
        }
        outcome.agents_imported.push(target_id.clone());
    }
    Ok(outcome)
}

fn apply_identities(
    ctx: &TransferContext<'_>,
    identities: &[Identity],
    resolutions: &[IdentityResolution],
    outcome: &mut ImportOutcome,
) -> Result<()> {
    for (identity, resolution) in identities.iter().zip(resolutions) {
        if resolution.is_create() {
            ctx.identity_store.create(identity)?;
            outcome.identities_created += 1;
        } else {
            outcome.identities_matched += 1;
        }
    }
    Ok(())
}

async fn apply_agent<R: Read + Seek>(
    ctx: &TransferContext<'_>,
    bundle: &mut BundleReader<R>,
    source_id: &str,
    target_id: &str,
    remap: &RemapPlan,
    outcome: &mut ImportOutcome,
) -> Result<()> {
    if !workforce_common::config_files::is_plain_name(target_id) {
        bail!("'{target_id}' is not an agent id");
    }
    let config: AgentConfig = bundle.read_json(&agent_config_entry(source_id))?;
    if let Some(note) = credential_note(target_id, &config) {
        outcome.notes.push(note);
    }
    let config = normalize_config(config, target_id, remap);
    let admission = WorkspaceAdmission::for_new_agent(ctx.config, &config);
    upsert_agent_config(ctx, config).await?;

    let workspace = ctx.data_dir.join("agents").join(target_id);
    std::fs::create_dir_all(&workspace)?;
    if let Some(bytes) = bundle.try_read_bytes(&agent_workspace_entry(source_id))? {
        let body = String::from_utf8(bytes)?;
        let admitted =
            admission.and_then(|admission| admission.review_toml(&body).map_err(eyre::Report::new));
        match admitted {
            Ok(_) => {
                AgentWorkspaceConfig::locked(&workspace, || {
                    AgentWorkspaceConfig::write(&workspace, &body)
                })?;
            }
            Err(refusal) => outcome.notes.push(format!(
                "agent '{target_id}': its config.toml was not imported, \
                 the agent would not load it ({refusal})"
            )),
        }
    }
    for file in IDENTITY_FILES {
        if let Some(bytes) = bundle.try_read_bytes(&agent_identity_entry(source_id, file))? {
            workforce_common::config_files::replace(&workspace.join(file), &bytes)?;
        }
    }
    import_sessions(bundle, source_id, &workspace, target_id, remap, outcome)?;
    import_memories(ctx, bundle, source_id, target_id, remap, outcome).await?;
    Ok(())
}

fn credential_note(target_id: &str, config: &AgentConfig) -> Option<String> {
    config.llm.as_ref().and_then(|llm| llm.provider.clone()).map(|provider| {
        format!("agent '{target_id}' needs '{provider}' credentials provisioned (vault is not transferred)")
    })
}

fn normalize_config(mut config: AgentConfig, target_id: &str, remap: &RemapPlan) -> AgentConfig {
    config.id = target_id.to_string();
    config.workspace = None;
    config.vault = None;
    let resolved_owner = (config.owner_type == OwnerType::User)
        .then_some(config.owner_id.as_deref())
        .flatten()
        .and_then(|owner| remap.identities.get(owner).cloned());
    match resolved_owner {
        Some(owner) => config.owner_id = Some(owner),
        None => {
            config.owner_id = None;
            config.owner_type = OwnerType::default();
        }
    }
    config
}

async fn upsert_agent_config(ctx: &TransferContext<'_>, config: AgentConfig) -> Result<()> {
    let table = config_to_table(&config)?;
    crate::config_writer::overlay_upsert_agent(
        &ctx.base_config_path,
        &ctx.data_dir,
        table,
        ctx.actor.as_deref(),
        "import",
        ctx.def_store,
        ctx.lock_manager,
    )
    .await
}

fn config_to_table(config: &AgentConfig) -> Result<toml::Table> {
    match json_to_toml(&serde_json::to_value(config)?) {
        Some(toml::Value::Table(table)) => Ok(table),
        _ => bail!("agent config did not serialize to a table"),
    }
}

/// JSON → TOML, dropping nulls (TOML has no null) so absent optionals fall back
/// to the target's defaults on reload.
fn json_to_toml(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(toml::Value::Boolean(*b)),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| n.as_f64().map(toml::Value::Float)),
        serde_json::Value::String(s) => Some(toml::Value::String(s.clone())),
        serde_json::Value::Array(items) => Some(toml::Value::Array(
            items.iter().filter_map(json_to_toml).collect(),
        )),
        serde_json::Value::Object(map) => {
            let table = map
                .iter()
                .filter_map(|(k, v)| json_to_toml(v).map(|tv| (k.clone(), tv)))
                .collect();
            Some(toml::Value::Table(table))
        }
    }
}

fn import_sessions<R: Read + Seek>(
    bundle: &mut BundleReader<R>,
    source_id: &str,
    workspace: &Path,
    target_agent: &str,
    remap: &RemapPlan,
    outcome: &mut ImportOutcome,
) -> Result<()> {
    let prefix = agent_sessions_prefix(source_id);
    let entries = bundle.entries_with_prefix(&prefix);
    if entries.is_empty() {
        return Ok(());
    }
    let sessions_dir = workspace.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let present: HashSet<String> = std::fs::read_dir(&sessions_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".meta.json")
                .map(str::to_string)
        })
        .collect();

    for entry in &entries {
        let name = &entry[prefix.len()..];
        if !workforce_common::config_files::is_plain_name(name) {
            bail!("bundle entry '{entry}' is not a session file");
        }
        // Turn records and journals name the install that was running the
        // turn, so they are never imported — export drops them too, and a
        // bundle old enough to carry one would hand this hub a turn belonging
        // to another install.
        if is_install_state(name) {
            continue;
        }
        let (sid, _) = split_session_name(name);
        if present.contains(sid) {
            continue;
        }
        let bytes = bundle.read_bytes(entry)?;
        if name.ends_with(".meta.json") {
            std::fs::write(
                sessions_dir.join(name),
                rewrite_session_meta(&bytes, target_agent, remap)?,
            )?;
            outcome.sessions_imported += 1;
        } else {
            std::fs::write(sessions_dir.join(name), bytes)?;
        }
    }
    Ok(())
}

fn split_session_name(name: &str) -> (&str, &str) {
    for suffix in [".meta.json", ".archive.jsonl", ".jsonl"] {
        if let Some(sid) = name.strip_suffix(suffix) {
            return (sid, suffix);
        }
    }
    (name, "")
}

fn rewrite_session_meta(bytes: &[u8], target_agent: &str, remap: &RemapPlan) -> Result<Vec<u8>> {
    let mut meta: serde_json::Value = serde_json::from_slice(bytes)?;
    let obj = meta
        .as_object_mut()
        .ok_or_else(|| eyre!("session meta is not an object"))?;
    obj.insert("agent_id".into(), json!(target_agent));
    if let Some(owner) = obj.get("owner").and_then(|v| v.as_str()) {
        obj.insert("owner".into(), json!(remap.identity(owner)));
    }
    if let Some(metadata) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        if let Some(agent) = metadata.get("chat_agent_id").and_then(|v| v.as_str()) {
            let mapped = remap.agent(agent);
            metadata.insert("chat_agent_id".into(), json!(mapped));
        }
        if let Some(started_by) = metadata.get("started_by").and_then(|v| v.as_str()) {
            let mapped = remap.identity(started_by);
            metadata.insert("started_by".into(), json!(mapped));
        }
    }
    Ok(serde_json::to_vec_pretty(&meta)?)
}

async fn import_memories<R: Read + Seek>(
    ctx: &TransferContext<'_>,
    bundle: &mut BundleReader<R>,
    source_id: &str,
    target_id: &str,
    remap: &RemapPlan,
    outcome: &mut ImportOutcome,
) -> Result<()> {
    let memories: Vec<Memory> = bundle.read_ndjson(&agent_memory_entry(source_id))?;
    if memories.is_empty() {
        return Ok(());
    }
    let present: HashSet<String> = ctx
        .memory_store
        .list(MemoryFilter {
            owner: Some(AgentId::new(target_id)),
            system_unscoped: true,
            limit: Some(MEMORY_QUERY_LIMIT),
            ..MemoryFilter::default()
        })
        .await?
        .into_iter()
        .map(|m| m.id.0)
        .collect();

    for mut memory in memories {
        if present.contains(&memory.id.0) {
            outcome.memories_skipped += 1;
            continue;
        }
        let memory_id = memory.id.0.clone();
        memory.owner = AgentId::new(target_id);
        if let Some(identity_id) = memory.identity_id.as_deref() {
            memory.identity_id = Some(remap.identity(identity_id));
        }
        match ctx.memory_store.remember(memory).await {
            Ok(_) => outcome.memories_imported += 1,
            Err(e) => outcome
                .notes
                .push(format!("memory {memory_id} could not be stored: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_common::settings::{AgentLlmConfig, AgentVaultConfig};

    fn remap_with(src: &str, dst: &str) -> RemapPlan {
        RemapPlan {
            identities: [(src.to_string(), dst.to_string())].into_iter().collect(),
            ..RemapPlan::default()
        }
    }

    /// A crafted bundle names a session file that climbs out of `sessions/`.
    #[test]
    fn a_session_entry_that_leaves_the_sessions_directory_is_refused() {
        let mut writer = super::super::BundleWriter::new(std::io::Cursor::new(Vec::new()));
        let entry = format!("{}../config.toml.jsonl", agent_sessions_prefix("scout"));
        writer.add_bytes(&entry, b"{}").unwrap();
        let bytes = writer
            .finish(super::super::model::Manifest {
                bundle_version: super::super::model::BUNDLE_VERSION,
                source_install_id: "t".into(),
                source_slack_team_id: None,
                exported_at: chrono::Utc::now(),
                agents: vec![],
                schema_versions: Default::default(),
                counts: Default::default(),
                checksums: Default::default(),
            })
            .unwrap()
            .into_inner();
        let mut bundle = super::super::BundleReader::new(std::io::Cursor::new(bytes)).unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let refused = import_sessions(
            &mut bundle,
            "scout",
            workspace.path(),
            "scout",
            &RemapPlan::default(),
            &mut ImportOutcome::default(),
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("not a session file"),
            "{refused}"
        );
        assert!(!workspace.path().join("config.toml.jsonl").exists());
    }

    #[test]
    fn json_to_toml_drops_nulls_recursively() {
        let value = json!({
            "id": "allan",
            "name": null,
            "nested": { "keep": 1, "drop": null },
            "list": [1, null, 2],
            "flag": true,
        });
        let Some(toml::Value::Table(table)) = json_to_toml(&value) else {
            panic!("expected a table");
        };
        assert!(table.contains_key("id"));
        assert!(!table.contains_key("name"));
        let nested = table["nested"].as_table().unwrap();
        assert!(nested.contains_key("keep"));
        assert!(!nested.contains_key("drop"));
        assert_eq!(table["list"].as_array().unwrap().len(), 2);
        assert_eq!(table["flag"].as_bool(), Some(true));
    }

    #[test]
    fn normalize_remaps_user_owner_and_strips_paths() {
        let src = Uuid::new_v4().to_string();
        let dst = Uuid::new_v4().to_string();
        let config = AgentConfig {
            id: "allan".into(),
            workspace: Some("/abs/ws".into()),
            vault: Some(AgentVaultConfig::default()),
            owner_type: OwnerType::User,
            owner_id: Some(src.clone()),
            llm: Some(AgentLlmConfig {
                api_key: Some("secret".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = normalize_config(config, "allan-dev", &remap_with(&src, &dst));
        assert_eq!(out.id, "allan-dev");
        assert!(out.workspace.is_none());
        assert!(out.vault.is_none());
        assert_eq!(out.owner_id.as_deref(), Some(dst.as_str()));
    }

    #[test]
    fn normalize_clears_team_and_unresolvable_owner() {
        let config = AgentConfig {
            id: "x".into(),
            owner_type: OwnerType::Team,
            owner_id: Some(Uuid::new_v4().to_string()),
            ..Default::default()
        };
        let out = normalize_config(config, "x", &RemapPlan::default());
        assert!(out.owner_id.is_none());
        assert_eq!(out.owner_type, OwnerType::default());
    }

    #[test]
    fn split_session_name_picks_the_right_suffix() {
        assert_eq!(split_session_name("s1.meta.json"), ("s1", ".meta.json"));
        assert_eq!(
            split_session_name("s1.archive.jsonl"),
            ("s1", ".archive.jsonl")
        );
        assert_eq!(split_session_name("s1.jsonl"), ("s1", ".jsonl"));
    }

    #[test]
    fn config_to_table_carries_id() {
        let table = config_to_table(&AgentConfig {
            id: "allan".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(table.get("id").and_then(|v| v.as_str()), Some("allan"));
    }
}

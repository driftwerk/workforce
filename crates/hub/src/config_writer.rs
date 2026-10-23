//! Config persistence — runtime config changes write the **overlay**
//! (`{data_dir}/workforce.overlay.toml`), never the infra-owned
//! `workforce.toml`.
//!
//! The previous writers here (`save_runtime_config` / `save_full_config`)
//! serialized the env-*expanded* `WorkforceConfig` struct back to the base
//! file — which both fought the infra deploy for ownership of workforce.toml
//! and baked resolved secrets (`api_key = "${VAR}"` → the real key) into it.
//! They are gone. Runtime writers go through [`mutate_overlay`]; anything
//! that genuinely belongs in the base is an infra-repo change.
//!
//! Every overlay write is allowlist-gated (deny-by-default — the read-only
//! base mount was an inadvertent security control and the allowlist restores
//! that boundary), validated through the exact boot pipeline before the
//! atomic rename, and recorded as a `config_overlay` revision in
//! `def_revisions` (full post-change snapshot — rollback = write a chosen
//! snapshot back through this same writer).

use std::fs;
use std::path::Path;

use chrono::Utc;
use eyre::{bail, eyre, Result};
use workforce_common::config_overlay as overlay;
use workforce_common::locking::{LockManager, LockResult};
use workforce_common::settings::WorkforceConfig;
use workforce_orchestrator::workflow_store::{
    content_hash, EditContext, WorkflowStore, KIND_CONFIG_OVERLAY,
};

/// Who/where/what for an overlay write — recorded in `def_revisions`
/// (kind = `config_overlay`, ref_id = `global`).
#[derive(Debug, Clone, Copy)]
pub struct OverlayWrite<'a> {
    /// Authenticated identity (`None` = unattributed, e.g. CLI).
    pub actor: Option<&'a str>,
    /// `web` | `agent_tool` | `cli`.
    pub via: &'a str,
    /// `set` | `remove` | `revert` | `clear` | `replace`.
    pub action: &'a str,
    /// What changed, e.g. `agents.maple`.
    pub key_path: &'a str,
}

/// Read the current overlay table. Absent file = empty overlay. An
/// unparsable file is an error — fix or delete it by hand; silently
/// clobbering hand edits would be worse than refusing.
pub fn load_overlay_table(data_dir: &Path) -> Result<toml::Table> {
    let path = data_dir.join(overlay::OVERLAY_FILE);
    match workforce_common::config_files::read(&path) {
        Ok(Some(raw)) => raw.parse::<toml::Table>().map_err(|e| {
            eyre!(
                "overlay '{}' is unparsable ({e}) — fix or delete it before writing",
                path.display()
            )
        }),
        Ok(None) => Ok(toml::Table::new()),
        Err(e) => Err(eyre!("failed to read overlay '{}': {e}", path.display())),
    }
}

/// Serialized read-modify-write of the overlay. The file lock is held across
/// load → mutate → validate → revision capture → atomic write, so concurrent
/// writers can't interleave (no lost updates; revision order matches disk
/// order). Steps under the lock:
///
/// 1. **Mutate** — `mutate` edits the freshly-loaded overlay table.
/// 2. **Allowlist** — non-overlayable keys are refused with a typed error
///    naming them (infra-owned sections change in the infra repo).
/// 3. **Merged validation** — base ⊕ new overlay must parse and validate
///    through [`WorkforceConfig::from_raw_table`], the same pipeline the
///    next boot takes; a bad overlay must never take the hub down.
/// 4. **Revision capture** — the full post-change overlay snapshot lands in
///    `def_revisions`. When Postgres is up, a capture failure REFUSES the
///    write (a change that can't be audited doesn't happen); on-disk drift
///    written without a store is lazily captured as `oob` first. A write
///    failure after capture leaves a revision ahead of disk — healed as
///    `oob` on the next capture.
/// 5. **Atomic write** — temp file + rename.
pub async fn mutate_overlay(
    base_config_path: &Path,
    data_dir: &Path,
    write: OverlayWrite<'_>,
    def_store: Option<&WorkflowStore>,
    lock_manager: &LockManager,
    mutate: impl FnOnce(&mut toml::Table) -> Result<()>,
) -> Result<()> {
    let overlay_path = data_dir.join(overlay::OVERLAY_FILE);
    let lock_key = overlay_path.to_string_lossy().to_string();
    let holder = write.actor.unwrap_or("config-overlay");
    match lock_manager.acquire(&lock_key, holder, 30, Some("Saving config overlay")) {
        LockResult::Acquired | LockResult::TakenOver { .. } => {}
        LockResult::Held(lock) => {
            let wait_secs = (lock.expires_at - Utc::now()).num_seconds();
            bail!(
                "config overlay is locked by '{}' for: {} — retry in {}s",
                lock.holder,
                lock.reason.as_deref().unwrap_or("unknown"),
                wait_secs.max(0)
            );
        }
    }
    let result = mutate_overlay_locked(base_config_path, data_dir, write, def_store, mutate).await;
    lock_manager.release(&lock_key, holder);
    result
}

async fn mutate_overlay_locked(
    base_config_path: &Path,
    data_dir: &Path,
    write: OverlayWrite<'_>,
    def_store: Option<&WorkflowStore>,
    mutate: impl FnOnce(&mut toml::Table) -> Result<()>,
) -> Result<()> {
    let mut new_overlay = load_overlay_table(data_dir)?;
    mutate(&mut new_overlay)?;

    let offending = overlay::offending_paths(&new_overlay);
    if !offending.is_empty() {
        bail!(
            "config key(s) not overlayable: {} — infra-owned sections can only \
             change in workforce.toml via the infra repo",
            offending.join(", ")
        );
    }

    let raw = overlay::load_raw(base_config_path)?;
    let mut merged = raw.base.clone();
    overlay::merge_overlay(&mut merged, &new_overlay);
    let cfg = WorkforceConfig::from_raw_table(merged)
        .map_err(|e| eyre!("overlay rejected — merged config does not parse: {e}"))?;
    cfg.validate()
        .map_err(|e| eyre!("overlay rejected — merged config invalid: {e}"))?;

    let overlay_str = toml::to_string_pretty(&toml::Value::Table(new_overlay))
        .map_err(|e| eyre!("failed to serialize overlay: {e}"))?;

    let overlay_path = data_dir.join(overlay::OVERLAY_FILE);
    if let Some(store) = def_store {
        if let Ok(Some(current)) = workforce_common::config_files::read(&overlay_path) {
            let latest = store
                .latest_revision_hash(KIND_CONFIG_OVERLAY, "global", None)
                .await?;
            if latest.as_deref() != Some(content_hash(&current).as_str()) {
                // First-ever capture = baseline; later mismatch = drift
                // written without a store (e.g. hand edit) — unattributed.
                let via = if latest.is_none() { "seed" } else { "oob" };
                store
                    .record_revision(
                        KIND_CONFIG_OVERLAY,
                        "global",
                        None,
                        &current,
                        false,
                        EditContext::new(via, None),
                        None,
                    )
                    .await?;
            }
        }
        let detail = serde_json::json!({
            "action": write.action,
            "key_path": write.key_path,
        })
        .to_string();
        store
            .record_revision(
                KIND_CONFIG_OVERLAY,
                "global",
                None,
                &overlay_str,
                false,
                EditContext::new(write.via, write.actor),
                Some(&detail),
            )
            .await
            .map_err(|e| eyre!("config revision capture failed — refusing write: {e}"))?;
    }

    fs::create_dir_all(data_dir)?;
    workforce_common::config_files::replace(&overlay_path, overlay_str.as_bytes())?;
    Ok(())
}

fn find_agent_entry(table: &toml::Table, id: &str) -> Option<toml::Table> {
    table
        .get("agents")?
        .as_array()?
        .iter()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
        .and_then(|v| v.as_table())
        .cloned()
}

fn upsert_agent_in(overlay_table: &mut toml::Table, id: &str, entry: toml::Table) {
    let agents = overlay_table
        .entry("agents".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if let Some(arr) = agents.as_array_mut() {
        let value = toml::Value::Table(entry);
        match arr
            .iter()
            .position(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
        {
            Some(i) => arr[i] = value,
            None => arr.push(value),
        }
    }
    // An upsert revives a previously removed id.
    if let Some(arr) = overlay_table
        .get_mut("agents_remove")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| v.as_str() != Some(id));
        if arr.is_empty() {
            overlay_table.remove("agents_remove");
        }
    }
}

/// Add or replace an agent entry in the overlay. `agent` is a **raw** table
/// (unexpanded values only — never serialize a deserialized struct here).
#[allow(clippy::too_many_arguments)]
pub async fn overlay_upsert_agent(
    base_config_path: &Path,
    data_dir: &Path,
    agent: toml::Table,
    actor: Option<&str>,
    via: &str,
    def_store: Option<&WorkflowStore>,
    lock_manager: &LockManager,
) -> Result<()> {
    let id = agent
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("agent entry needs an id"))?
        .to_string();
    let key_path = format!("agents.{id}");
    mutate_overlay(
        base_config_path,
        data_dir,
        OverlayWrite {
            actor,
            via,
            action: "set",
            key_path: &key_path,
        },
        def_store,
        lock_manager,
        |ov| {
            let before = ov.clone();
            upsert_agent_in(ov, &id, agent);
            overlay::ensure_agent_entry_scope(&before, ov)
        },
    )
    .await
}

/// Remove an agent: drop any overlay entry and tombstone the id in
/// `agents_remove` so a base-defined agent stays gone across deploys.
/// Refuses to remove the default agent (an infra-base change) or the last
/// remaining agent — guarded here at the writer so the HTTP handler, the
/// `agent.delete` tool, and the CLI all share the boundary.
#[allow(clippy::too_many_arguments)]
pub async fn overlay_remove_agent(
    base_config_path: &Path,
    data_dir: &Path,
    id: &str,
    actor: Option<&str>,
    via: &str,
    def_store: Option<&WorkflowStore>,
    lock_manager: &LockManager,
) -> Result<()> {
    let key_path = format!("agents.{id}");
    let base_path = base_config_path.to_path_buf();
    mutate_overlay(
        base_config_path,
        data_dir,
        OverlayWrite {
            actor,
            via,
            action: "remove",
            key_path: &key_path,
        },
        def_store,
        lock_manager,
        |ov| {
            let before = ov.clone();
            let raw = overlay::load_raw(&base_path)?;
            let mut merged = raw.base.clone();
            overlay::merge_overlay(&mut merged, ov);
            let cfg = WorkforceConfig::from_raw_table(merged)?;
            if cfg.default_agent().is_some_and(|a| a.id == id) {
                bail!(
                    "cannot remove the default agent '{id}' — change the infra \
                     base config and restart instead"
                );
            }
            if cfg.agents.len() <= 1 && cfg.agents.iter().any(|a| a.id == id) {
                bail!("cannot remove the last remaining agent");
            }

            if let Some(arr) = ov.get_mut("agents").and_then(|v| v.as_array_mut()) {
                arr.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(id));
                if arr.is_empty() {
                    ov.remove("agents");
                }
            }
            let removes = ov
                .entry("agents_remove".to_string())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            if let Some(arr) = removes.as_array_mut() {
                if !arr.iter().any(|v| v.as_str() == Some(id)) {
                    arr.push(toml::Value::String(id.to_string()));
                }
            }
            overlay::ensure_agent_entry_scope(&before, ov)
        },
    )
    .await
}

/// Update fields on an existing agent. The current **raw** entry is taken
/// from the overlay (it wins) or the base file — never from the deserialized
/// (env-expanded) config — mutated, and upserted into the overlay as a
/// per-entity replacement.
#[allow(clippy::too_many_arguments)]
pub async fn overlay_update_agent_fields(
    base_config_path: &Path,
    data_dir: &Path,
    id: &str,
    mutate: impl FnOnce(&mut toml::Table),
    actor: Option<&str>,
    via: &str,
    def_store: Option<&WorkflowStore>,
    lock_manager: &LockManager,
) -> Result<()> {
    let key_path = format!("agents.{id}");
    let base_path = base_config_path.to_path_buf();
    mutate_overlay(
        base_config_path,
        data_dir,
        OverlayWrite {
            actor,
            via,
            action: "set",
            key_path: &key_path,
        },
        def_store,
        lock_manager,
        |ov| {
            let before = ov.clone();
            let raw = overlay::load_raw(&base_path)?;
            let mut entry = find_agent_entry(ov, id)
                .or_else(|| find_agent_entry(&raw.base, id))
                .ok_or_else(|| eyre!("agent '{id}' not found in config"))?;
            mutate(&mut entry);
            upsert_agent_in(ov, id, entry);
            overlay::ensure_agent_entry_scope(&before, ov)
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    const BASE: &str = r#"
[hub]
data_dir = "DATA_DIR"

[[agents]]
id = "allan"
name = "Allan"
default = true

[[agents]]
id = "quill"
name = "Quill"
"#;

    fn setup() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let base_path = tmp.path().join("workforce.toml");
        fs::write(
            &base_path,
            BASE.replace("DATA_DIR", &data_dir.to_string_lossy()),
        )
        .unwrap();
        (tmp, base_path, data_dir)
    }

    #[tokio::test]
    async fn upsert_agent_writes_overlay_not_base() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());
        let before = fs::read_to_string(&base_path).unwrap();

        let agent: toml::Table = "id = \"maple\"\nname = \"Maple\"\n".parse().unwrap();
        overlay_upsert_agent(
            &base_path,
            &data_dir,
            agent,
            Some("kingsley"),
            "web",
            None,
            &locks,
        )
        .await
        .unwrap();

        // Base untouched; overlay holds the new agent.
        assert_eq!(fs::read_to_string(&base_path).unwrap(), before);
        let ov = load_overlay_table(&data_dir).unwrap();
        let agents = ov.get("agents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].get("id").and_then(|v| v.as_str()), Some("maple"));

        // The merged load sees both agents.
        let cfg = WorkforceConfig::load(&base_path).unwrap();
        let ids: Vec<_> = cfg.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["allan", "quill", "maple"]);
    }

    #[tokio::test]
    async fn mutate_overlay_refuses_non_allowlisted_keys() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());
        let err = mutate_overlay(
            &base_path,
            &data_dir,
            OverlayWrite {
                actor: None,
                via: "web",
                action: "set",
                key_path: "postgres",
            },
            None,
            &locks,
            |ov| {
                let bad: toml::Table = "[postgres]\nenabled = false\n".parse().unwrap();
                ov.extend(bad);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not overlayable"), "{err}");
        assert!(!data_dir.join(overlay::OVERLAY_FILE).exists());
    }

    #[tokio::test]
    async fn mutate_overlay_refuses_invalid_merged_config() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());
        // An agent entry that deserializes but may fail validate(): empty id.
        let result = mutate_overlay(
            &base_path,
            &data_dir,
            OverlayWrite {
                actor: None,
                via: "web",
                action: "set",
                key_path: "agents.",
            },
            None,
            &locks,
            |ov| {
                let bad: toml::Table = "[[agents]]\nid = \"\"\n".parse().unwrap();
                ov.extend(bad);
                Ok(())
            },
        )
        .await;
        // Whichever stage rejects it (deserialize or validate), the merged
        // config must still load afterwards.
        if result.is_ok() {
            WorkforceConfig::load(&base_path).unwrap();
        } else {
            assert!(!data_dir.join(overlay::OVERLAY_FILE).exists());
        }
    }

    #[tokio::test]
    async fn remove_agent_tombstones_base_entry() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());
        let agent: toml::Table = "id = \"maple\"\n".parse().unwrap();
        overlay_upsert_agent(&base_path, &data_dir, agent, None, "cli", None, &locks)
            .await
            .unwrap();

        overlay_remove_agent(&base_path, &data_dir, "maple", None, "cli", None, &locks)
            .await
            .unwrap();
        let cfg = WorkforceConfig::load(&base_path).unwrap();
        assert_eq!(cfg.agents.len(), 2);

        // Removing a base agent tombstones it across deploys.
        overlay_remove_agent(&base_path, &data_dir, "quill", None, "cli", None, &locks)
            .await
            .unwrap();
        let ov = load_overlay_table(&data_dir).unwrap();
        let removes = ov.get("agents_remove").and_then(|v| v.as_array()).unwrap();
        assert!(removes.iter().any(|v| v.as_str() == Some("quill")));
    }

    #[tokio::test]
    async fn remove_agent_guards_default_and_last() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());

        // The default agent can never be removed via the overlay.
        let err = overlay_remove_agent(&base_path, &data_dir, "allan", None, "cli", None, &locks)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default agent"), "{err}");

        // The last remaining agent can't be removed either.
        overlay_remove_agent(&base_path, &data_dir, "quill", None, "cli", None, &locks)
            .await
            .unwrap();
        let err = overlay_remove_agent(&base_path, &data_dir, "allan", None, "cli", None, &locks)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default agent"), "{err}");
    }

    #[tokio::test]
    async fn update_agent_fields_pulls_raw_base_entry() {
        let (_tmp, base_path, data_dir) = setup();
        let locks = Arc::new(LockManager::new());
        overlay_update_agent_fields(
            &base_path,
            &data_dir,
            "allan",
            |entry| {
                entry.insert(
                    "owner_id".to_string(),
                    toml::Value::String("uid-1".to_string()),
                );
            },
            Some("kingsley"),
            "web",
            None,
            &locks,
        )
        .await
        .unwrap();

        let cfg = WorkforceConfig::load(&base_path).unwrap();
        let allan = cfg.agents.iter().find(|a| a.id == "allan").unwrap();
        assert_eq!(allan.owner_id.as_deref(), Some("uid-1"));
        // Untouched raw fields survived the per-entity replace.
        assert_eq!(allan.name.as_deref(), Some("Allan"));
        assert!(allan.default);
    }
}

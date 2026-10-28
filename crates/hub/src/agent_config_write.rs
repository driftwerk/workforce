//! The one way an agent's workspace `config.toml` is replaced, whoever asks:
//! the config API, a revision restore, or the agent's own `system.config`
//! tool. Each write is checked against the body its writer read, admitted
//! only if the agent would load it, recorded as a revision before it lands,
//! and hot-applied to the running agent.

use std::path::Path;

use workforce_orchestrator::workflow_store::{
    content_hash, EditContext, WorkflowStore, KIND_AGENT_CONFIG,
};

use crate::serve::AgentService;
use crate::subscription::Finding;
use crate::workspace_admission::ConfigRefusal;

/// Why a config write did not land. The file and its revision history are as
/// they were in every case.
#[derive(Debug)]
pub enum Refused {
    /// The file is no longer the one the writer read: someone saved since.
    Stale { current_hash: String },
    /// The agent would not load the body.
    Unloadable(ConfigRefusal),
    /// History could not record the write, so it was not made.
    Revision(String),
    /// The file could not be read or replaced.
    Io(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale { .. } => {
                f.write_str("config.toml changed while this edit was made; get it again and retry")
            }
            Self::Unloadable(refusal) => write!(f, "the agent would not load that: {refusal}"),
            Self::Revision(e) => write!(f, "not saved: its history could not record it ({e})"),
            Self::Io(e) => write!(f, "not saved: {e}"),
        }
    }
}

/// A write that landed.
#[derive(Debug)]
pub struct Saved {
    /// What admission noted about the body, none of it refusing.
    pub findings: Vec<Finding>,
    /// Fields that change only once the agent restarts.
    pub restart_required: Vec<String>,
    /// The content hash of the body now on disk.
    pub hash: String,
}

/// Replace `service`'s config.toml with `body`, recorded in history under
/// `ref_id` (the agent's id), provided `expects` accepts the
/// file it replaces (`None`: there is none), both before the revision is
/// recorded and again under the lock every writer holds as it lands.
///
/// A body that makes the agent the crew's overseer switches the overseer
/// before it off, in its own config.toml: one agent oversees at a time.
pub async fn save(
    service: &AgentService,
    ref_id: &str,
    store: Option<&WorkflowStore>,
    body: &str,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
) -> Result<Saved, Refused> {
    let saved = save_one(service, ref_id, store, body, edit, expects).await?;
    if oversees(body) {
        one_overseer(service, store).await;
    }
    #[cfg(feature = "postgres")]
    crate::desk_crew::config_saved(service);
    Ok(saved)
}

/// Whether a config.toml body makes its agent the crew's overseer.
pub(crate) fn oversees(body: &str) -> bool {
    workforce_common::settings::AgentWorkspaceConfig::parse(body)
        .ok()
        .and_then(|c| c.oversight().ok())
        .is_some_and(|o| o.enabled)
}

/// `body` with its `[oversight]` switched off and everything else as it was,
/// the table itself kept (an absent one reads as never set, which Desk's hub
/// would turn back on for its own agent). `None` when it doesn't parse.
pub(crate) fn oversight_off(body: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = body.parse().ok()?;
    match doc.get_mut("oversight")? {
        toml_edit::Item::Table(t) => {
            t.insert("enabled", toml_edit::value(false));
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
            t.insert("enabled", false.into());
        }
        _ => return None,
    }
    Some(doc.to_string())
}

/// Switches every agent but `keep` that oversees the crew off, through the
/// same write path (history, admission, hot reload).
pub(crate) async fn one_overseer(keep: &AgentService, store: Option<&WorkflowStore>) {
    let Some(registry) = keep.agent_registry.get().and_then(std::sync::Weak::upgrade) else {
        return;
    };
    for other in registry.all_services() {
        if other.agent_id == keep.agent_id {
            continue;
        }
        let path = other
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let Ok(Some(current)) = workforce_common::config_files::read(&path) else {
            continue;
        };
        if !oversees(&current) {
            continue;
        }
        let Some(off) = oversight_off(&current) else {
            continue;
        };
        let id = other.agent_id.as_str().to_string();
        match save_one(
            &other,
            &id,
            store,
            &off,
            EditContext::new("oversight", None),
            |now| now == Some(current.as_str()),
        )
        .await
        {
            Ok(_) => tracing::info!(
                agent_id = %id,
                overseer = %keep.agent_id,
                "Another agent oversees the crew now; this one no longer does"
            ),
            Err(e) => tracing::warn!(agent_id = %id, error = %e, "Could not switch oversight off"),
        }
    }
}

async fn save_one(
    service: &AgentService,
    ref_id: &str,
    store: Option<&WorkflowStore>,
    body: &str,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
) -> Result<Saved, Refused> {
    use workforce_common::settings::AgentWorkspaceConfig;
    let path = service
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let admit = |current: Option<&str>| {
        if !expects(current) {
            return Err(Refused::Stale {
                current_hash: content_hash(current.unwrap_or_default()),
            });
        }
        crate::workspace_admission::admit_workspace(service, body).map_err(Refused::Unloadable)
    };
    admit(read(&path)?.as_deref())?;

    tokio::fs::create_dir_all(&service.workspace)
        .await
        .map_err(|e| Refused::Io(format!("failed to create the workspace directory: {e}")))?;
    record_revision(store, KIND_AGENT_CONFIG, ref_id, &path, body, edit)
        .await
        .map_err(Refused::Revision)?;

    let findings = AgentWorkspaceConfig::locked(&service.workspace, || {
        let findings = admit(read(&path)?.as_deref())?;
        AgentWorkspaceConfig::write(&service.workspace, body)
            .map_err(|e| Refused::Io(e.to_string()))?;
        Ok(findings)
    })?;
    tracing::info!(agent_id = %service.agent_id, via = edit.edited_via, "Agent config.toml saved");

    let restart_required = service.reload_config().await.unwrap_or_else(|e| {
        tracing::warn!(agent_id = %service.agent_id, error = %e, "Config hot-reload failed after save");
        Vec::new()
    });
    Ok(Saved {
        findings,
        restart_required,
        hash: content_hash(body),
    })
}

/// The file at `path` as it stands, `None` when there is none.
fn read(path: &Path) -> Result<Option<String>, Refused> {
    workforce_common::config_files::read(path)
        .map_err(|e| Refused::Io(format!("failed to read {}: {e}", path.display())))
}

/// Record `new_content` as the next revision of the file at `path`, first
/// capturing what is on disk when history has not seen it: the baseline on a
/// first capture, an out-of-band edit after that. No store: nothing to record.
pub async fn record_revision(
    store: Option<&WorkflowStore>,
    kind: &str,
    ref_id: &str,
    path: &Path,
    new_content: &str,
    edit: EditContext<'_>,
) -> Result<(), String> {
    let Some(store) = store else {
        return Ok(());
    };
    if let Ok(Some(current)) = workforce_common::config_files::read(path) {
        let latest = store
            .latest_revision_hash(kind, ref_id, None)
            .await
            .map_err(|e| e.to_string())?;
        if latest.as_deref() != Some(content_hash(&current).as_str()) {
            let via = if latest.is_none() { "seed" } else { "oob" };
            store
                .record_revision(
                    kind,
                    ref_id,
                    None,
                    &current,
                    false,
                    EditContext::new(via, None),
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    store
        .record_revision(kind, ref_id, None, new_content, false, edit, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversight_is_switched_off_in_place_in_either_form() {
        let tables = "# mine\n[llm]\nprovider = \"chatgpt\"\n\n[oversight]\nenabled = true\nlevel = \"act\"\n\n[oversight.wake]\nquiet_minutes = 0\n";
        let off = oversight_off(tables).unwrap();
        assert!(off.starts_with("# mine\n[llm]\nprovider = \"chatgpt\"\n"));
        assert!(!oversees(&off));
        assert!(off.contains("level = \"act\""));
        assert!(off.contains("[oversight.wake]\nquiet_minutes = 0"));

        let inline = "oversight = { enabled = true, level = \"watch\" }\n";
        assert!(oversees(inline));
        let off = oversight_off(inline).unwrap();
        assert!(!oversees(&off));
        assert!(off.contains("level = \"watch\""));

        assert_eq!(oversight_off("[llm]\n"), None);
    }
}

//! Where each vault lives: the file it is (or was) stored in, the key that
//! file is sealed with, and its Postgres scope.

use std::path::{Path, PathBuf};

use eyre::{bail, Result};
use workforce_common::settings::{AgentVaultConfig, WorkforceConfig};

use super::Vault;
use crate::hub_core::layout::agent_workspace;

/// Scope of the hub-level vault that every agent layers under its own.
pub const GLOBAL_SCOPE: &str = "global";

/// The agent a hub runs when no `[[agents]]` are configured.
pub(super) const DEFAULT_AGENT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultTarget {
    /// `global` or `agent:<id>`. An agent whose vault file is the global
    /// file shares the global scope, as it shares the file.
    pub scope: String,
    pub file: PathBuf,
    pub key_file: Option<PathBuf>,
    pub data_dir: PathBuf,
}

impl VaultTarget {
    /// The master key this target's file is sealed with.
    pub fn master_key(&self) -> Result<String> {
        Vault::resolve_master_key(self.key_file.as_deref(), &self.data_dir)
    }
}

/// The hub-level vault; `None` when `[vault] enabled = false`.
pub fn global_target(config: &WorkforceConfig) -> Option<VaultTarget> {
    let data_dir = PathBuf::from(&config.hub.data_dir);
    config.vault.enabled.then(|| VaultTarget {
        scope: GLOBAL_SCOPE.to_owned(),
        file: config
            .vault
            .path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("vault.enc")),
        key_file: config.vault.key_file.as_ref().map(PathBuf::from),
        data_dir,
    })
}

/// An agent's own vault; `None` when disabled for the agent (or globally
/// without an agent override).
pub fn agent_target(
    config: &WorkforceConfig,
    agent_id: &str,
    agent_vault: Option<&AgentVaultConfig>,
    workspace: &Path,
) -> Option<VaultTarget> {
    let enabled = agent_vault
        .and_then(|v| v.enabled)
        .unwrap_or(config.vault.enabled);
    enabled.then(|| {
        let file = agent_vault
            .and_then(|v| v.path.as_ref())
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("vault.enc"));
        let shares_global_file = global_target(config)
            .is_some_and(|global| canonical_file(&global.file) == canonical_file(&file));
        VaultTarget {
            scope: if shares_global_file {
                GLOBAL_SCOPE.to_owned()
            } else {
                format!("agent:{agent_id}")
            },
            file,
            key_file: agent_vault
                .and_then(|v| v.key_file.as_ref())
                .or(config.vault.key_file.as_ref())
                .map(PathBuf::from),
            data_dir: PathBuf::from(&config.hub.data_dir),
        }
    })
}

/// A configured agent's vault by id — the implicit `default` agent counts
/// when no agents are configured. `None` when that agent's vault is off.
pub fn configured_agent_target(
    config: &WorkforceConfig,
    agent_id: &str,
) -> Result<Option<VaultTarget>> {
    let (vault, workspace) = match config.agent_by_id(agent_id) {
        Some(agent) => (agent.vault.as_ref(), agent.workspace.as_deref()),
        None if config.agents.is_empty() && agent_id == DEFAULT_AGENT => (None, None),
        None => bail!("no agent `{agent_id}` in the config"),
    };
    let workspace = agent_workspace(Path::new(&config.hub.data_dir), agent_id, workspace);
    Ok(agent_target(config, agent_id, vault, &workspace))
}

/// A vault file's identity: its canonical directory plus its name, so the
/// file itself need not exist yet.
pub fn canonical_file(path: &Path) -> PathBuf {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match (dir.canonicalize(), path.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::AgentConfig;

    fn config(data_dir: &Path) -> WorkforceConfig {
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = data_dir.to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config
    }

    #[test]
    fn global_target_defaults_into_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = global_target(&config(tmp.path())).unwrap();
        assert_eq!(target.scope, GLOBAL_SCOPE);
        assert_eq!(target.file, tmp.path().join("vault.enc"));
        assert_eq!(target.key_file, None);
    }

    #[test]
    fn disabled_vault_has_no_global_target() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(tmp.path());
        config.vault.enabled = false;
        assert!(global_target(&config).is_none());
    }

    #[test]
    fn agent_target_defaults_into_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("agents/sarah");
        let target = agent_target(&config(tmp.path()), "sarah", None, &workspace).unwrap();
        assert_eq!(target.scope, "agent:sarah");
        assert_eq!(target.file, workspace.join("vault.enc"));
    }

    #[test]
    fn agent_override_wins_over_global_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(tmp.path());
        config.vault.enabled = false;
        config.vault.key_file = Some("global.key".into());
        let agent = AgentVaultConfig {
            enabled: Some(true),
            path: Some(tmp.path().join("own.enc").to_string_lossy().into_owned()),
            key_file: Some("agent.key".into()),
        };
        let target = agent_target(&config, "a", Some(&agent), tmp.path()).unwrap();
        assert_eq!(target.file, tmp.path().join("own.enc"));
        assert_eq!(target.key_file, Some(PathBuf::from("agent.key")));

        let disabled = AgentVaultConfig {
            enabled: Some(false),
            ..AgentVaultConfig::default()
        };
        let config = self::config(tmp.path());
        assert!(agent_target(&config, "a", Some(&disabled), tmp.path()).is_none());
    }

    #[test]
    fn agent_key_falls_back_to_the_global_key_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(tmp.path());
        config.vault.key_file = Some("global.key".into());
        let target = agent_target(&config, "a", None, tmp.path()).unwrap();
        assert_eq!(target.key_file, Some(PathBuf::from("global.key")));
    }

    #[test]
    fn agent_on_the_global_file_shares_the_global_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentVaultConfig {
            path: Some(
                tmp.path()
                    .join(".")
                    .join("vault.enc")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..AgentVaultConfig::default()
        };
        let target = agent_target(&config(tmp.path()), "a", Some(&agent), tmp.path()).unwrap();
        assert_eq!(target.scope, GLOBAL_SCOPE);
    }

    #[test]
    fn configured_agents_resolve_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = config(tmp.path());
        let default = configured_agent_target(&bare, DEFAULT_AGENT)
            .unwrap()
            .unwrap();
        assert_eq!(default.file, tmp.path().join("agents/default/vault.enc"));
        assert!(configured_agent_target(&bare, "sarah").is_err());

        let mut with_agents = config(tmp.path());
        with_agents.agents = vec![AgentConfig {
            id: "sarah".into(),
            workspace: Some(tmp.path().join("ws").to_string_lossy().into_owned()),
            ..AgentConfig::default()
        }];
        let sarah = configured_agent_target(&with_agents, "sarah")
            .unwrap()
            .unwrap();
        assert_eq!(sarah.file, tmp.path().join("ws/vault.enc"));
        assert!(configured_agent_target(&with_agents, DEFAULT_AGENT).is_err());
    }

    #[test]
    fn canonical_file_resolves_the_directory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dotted = tmp.path().join("sub").join("..").join("vault.enc");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        assert_eq!(
            canonical_file(&dotted),
            tmp.path().canonicalize().unwrap().join("vault.enc")
        );
        assert_eq!(
            canonical_file(Path::new("/no/such/dir/vault.enc")),
            PathBuf::from("/no/such/dir/vault.enc")
        );
    }
}

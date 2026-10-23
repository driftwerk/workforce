use std::path::PathBuf;

use eyre::{Result, WrapErr};

const DATA_DIR_ENV: &str = "WORKFORCE_LOCAL_DATA_DIR";
const WORKSPACE_ENV: &str = "WORKFORCE_LOCAL_WORKSPACE";
const PROJECTS_DIR_ENV: &str = "WORKFORCE_LOCAL_PROJECTS_DIR";
const APP_DIR_NAME: &str = "workforce-local";
const PROJECTS_SUBDIR: &str = "projects";

/// Configuration for the local MCP server.
///
/// Everything is derived from environment variables with sensible defaults so
/// Claude Code can spawn the binary with no extra flags.
pub struct LocalMcpConfig {
    /// Where the SQLite databases (codegraph, memory) live.
    pub data_dir: PathBuf,
    /// The directory indexed by default and used as a base for relative repo
    /// paths in tool calls. Defaults to the process's current working directory.
    pub workspace: PathBuf,
    /// Where project TOML files live. May not exist; callers tolerate absence.
    pub projects_dir: PathBuf,
}

/// Raw overrides read from the environment. Extracted for testability — every
/// overridable value is explicit so unit tests never have to mutate process
/// env state.
#[derive(Debug, Default)]
pub struct ConfigOverrides {
    pub data_dir: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub projects_dir: Option<PathBuf>,
}

impl LocalMcpConfig {
    /// Resolve config from env + defaults. `WORKFORCE_LOCAL_DATA_DIR` overrides
    /// the data directory; otherwise it defaults to the platform's user-local
    /// data dir (e.g. `~/Library/Application Support/workforce-local` on macOS,
    /// `~/.local/share/workforce-local` on Linux). `WORKFORCE_LOCAL_WORKSPACE`
    /// overrides the workspace; otherwise it defaults to `$PWD`.
    /// `WORKFORCE_LOCAL_PROJECTS_DIR` overrides the projects directory;
    /// otherwise it defaults to `<data_dir>/projects`.
    pub fn load() -> Result<Self> {
        Self::resolve(ConfigOverrides {
            data_dir: std::env::var_os(DATA_DIR_ENV).map(PathBuf::from),
            workspace: std::env::var_os(WORKSPACE_ENV).map(PathBuf::from),
            projects_dir: std::env::var_os(PROJECTS_DIR_ENV).map(PathBuf::from),
        })
    }

    /// Apply overrides on top of defaults and create the data directory. Pure
    /// with respect to the env; [`load`](Self::load) is the thin wrapper that
    /// reads env vars.
    pub fn resolve(overrides: ConfigOverrides) -> Result<Self> {
        let data_dir = match overrides.data_dir {
            Some(v) => v,
            None => default_data_dir()?,
        };
        std::fs::create_dir_all(&data_dir)
            .wrap_err_with(|| format!("creating data dir {}", data_dir.display()))?;

        let workspace = match overrides.workspace {
            Some(v) => v,
            None => std::env::current_dir().wrap_err("resolving current directory")?,
        };

        let projects_dir = overrides
            .projects_dir
            .unwrap_or_else(|| data_dir.join(PROJECTS_SUBDIR));

        Ok(Self {
            data_dir,
            workspace,
            projects_dir,
        })
    }
}

fn default_data_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join(APP_DIR_NAME))
        .ok_or_else(|| eyre::eyre!("could not determine a user data directory; set {DATA_DIR_ENV}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_overrides_and_creates_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let workspace = tmp.path().join("workspace");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&workspace).unwrap();

        let cfg = LocalMcpConfig::resolve(ConfigOverrides {
            data_dir: Some(data.clone()),
            workspace: Some(workspace.clone()),
            projects_dir: Some(projects.clone()),
        })
        .unwrap();

        assert_eq!(cfg.data_dir, data);
        assert_eq!(cfg.workspace, workspace);
        assert_eq!(cfg.projects_dir, projects);
        assert!(data.is_dir(), "data dir should have been created");
    }

    #[test]
    fn projects_dir_defaults_to_data_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let cfg = LocalMcpConfig::resolve(ConfigOverrides {
            data_dir: Some(data.clone()),
            workspace: Some(workspace),
            projects_dir: None,
        })
        .unwrap();

        assert_eq!(cfg.projects_dir, data.join(PROJECTS_SUBDIR));
    }
}

use chrono::Utc;
use eyre::{eyre, Result};

use crate::model::{Project, ProjectRepo};

/// Per-agent project persistence.
///
/// One store instance serves every agent on the hub; methods take `agent_id`
/// explicitly so the same backend (one SQLite file or one Postgres schema)
/// holds projects for all agents under a composite `(agent_id, name)` key.
/// Sync to match the rest of the workforce store traits (`IdentityStore`,
/// `PairingStore`, `SessionStore`).
///
/// Both SQLite and Postgres back the repo list as a single JSON column, so
/// `add_repo` / `remove_repo` are read-modify-write at the application layer
/// — implemented once as default methods on top of `get` + `put` and shared
/// across backends.
pub trait ProjectStore: Send + Sync {
    fn list(&self, agent_id: &str) -> Result<Vec<Project>>;

    /// Every project for every agent. Used by hub-wide sweeps (the
    /// knowledge indexer enumerating all repos to index) where iterating
    /// `list(agent_id)` per agent would mean N round-trips for nothing.
    fn list_all(&self) -> Result<Vec<Project>>;

    fn get(&self, agent_id: &str, name: &str) -> Result<Option<Project>>;

    /// Caller is responsible for setting `updated_at`.
    fn put(&self, project: &Project) -> Result<()>;

    /// `Ok(false)` means no project existed for this agent.
    fn delete(&self, agent_id: &str, name: &str) -> Result<bool>;

    /// Add or replace by `alias`. Errors if the project doesn't exist.
    fn add_repo(&self, agent_id: &str, name: &str, repo: ProjectRepo) -> Result<()> {
        let mut project = self
            .get(agent_id, name)?
            .ok_or_else(|| eyre!("project '{}' not found for agent '{}'", name, agent_id))?;
        match project.repos.iter().position(|r| r.alias == repo.alias) {
            Some(i) => project.repos[i] = repo,
            None => project.repos.push(repo),
        }
        project.updated_at = Utc::now();
        self.put(&project)
    }

    /// `Ok(false)` means the alias wasn't present. Errors if the project
    /// doesn't exist.
    fn remove_repo(&self, agent_id: &str, name: &str, alias: &str) -> Result<bool> {
        let mut project = self
            .get(agent_id, name)?
            .ok_or_else(|| eyre!("project '{}' not found for agent '{}'", name, agent_id))?;
        let before = project.repos.len();
        project.repos.retain(|r| r.alias != alias);
        if project.repos.len() == before {
            return Ok(false);
        }
        project.updated_at = Utc::now();
        self.put(&project)?;
        Ok(true)
    }

    /// Rename a project for `agent_id` from `old_name` to `new_name`.
    ///
    /// `Ok(true)` — renamed; `created_at` preserved, `updated_at`
    /// bumped, repo list and defaults carried across.
    /// `Ok(false)` — no project at `old_name`.
    /// `Err(...)` — `new_name` already exists for this agent, or
    /// `old_name == new_name` (caller bug).
    ///
    /// The default implementation is a read-modify-write composed of
    /// `get` + `delete` + `put`. It's safe under SQLite's per-store
    /// `Mutex<Connection>` (every call serialises). Backends with
    /// connection-per-call concurrency (Postgres) override to wrap the
    /// three steps in one transaction so a concurrent rename can't
    /// double-claim `new_name`.
    fn rename(&self, agent_id: &str, old_name: &str, new_name: &str) -> Result<bool> {
        if old_name == new_name {
            return Err(eyre!(
                "rename: old_name and new_name are identical ('{}')",
                old_name
            ));
        }
        if self.get(agent_id, new_name)?.is_some() {
            return Err(eyre!(
                "project '{}' already exists for agent '{}'",
                new_name,
                agent_id
            ));
        }
        let mut project = match self.get(agent_id, old_name)? {
            Some(p) => p,
            None => return Ok(false),
        };
        project.name = new_name.to_string();
        project.updated_at = Utc::now();
        self.delete(agent_id, old_name)?;
        self.put(&project)?;
        Ok(true)
    }
}

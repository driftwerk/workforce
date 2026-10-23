use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;

use crate::model::{Project, ProjectDefaults, ProjectRepo};
use crate::store::ProjectStore;

pub struct SqliteProjectStore {
    conn: Mutex<Connection>,
}

impl SqliteProjectStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("Failed to open project database: {:?}", path.as_ref()))?;
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Project store opened");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS projects (
                agent_id    TEXT NOT NULL,
                name        TEXT NOT NULL,
                repos       TEXT NOT NULL DEFAULT '[]',
                defaults    TEXT NOT NULL DEFAULT '{}',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                PRIMARY KEY (agent_id, name)
            );

            CREATE INDEX IF NOT EXISTS idx_projects_agent ON projects(agent_id);
            ",
        )
        .wrap_err("Failed to initialise project schema")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre!("Failed to lock project database: {}", e))
    }

    fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
        let agent_id: String = row.get("agent_id")?;
        let name: String = row.get("name")?;
        let repos_json: String = row.get("repos")?;
        let defaults_json: String = row.get("defaults")?;
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let repos: Vec<ProjectRepo> = serde_json::from_str(&repos_json).unwrap_or_default();
        let defaults: ProjectDefaults = serde_json::from_str(&defaults_json).unwrap_or_default();
        let created_at = parse_ts(&created_at_str);
        let updated_at = parse_ts(&updated_at_str);

        Ok(Project {
            agent_id,
            name,
            defaults,
            repos,
            created_at,
            updated_at,
        })
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl ProjectStore for SqliteProjectStore {
    fn list(&self, agent_id: &str) -> Result<Vec<Project>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                 FROM projects WHERE agent_id = ?1 ORDER BY name",
            )
            .wrap_err("Failed to prepare project list")?;
        let rows = stmt
            .query_map(params![agent_id], Self::parse_row)
            .wrap_err("Failed to execute project list")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.wrap_err("Failed to parse project row")?);
        }
        Ok(out)
    }

    fn list_all(&self) -> Result<Vec<Project>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                 FROM projects ORDER BY agent_id, name",
            )
            .wrap_err("Failed to prepare list_all")?;
        let rows = stmt
            .query_map([], Self::parse_row)
            .wrap_err("Failed to execute list_all")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.wrap_err("Failed to parse project row")?);
        }
        Ok(out)
    }

    fn get(&self, agent_id: &str, name: &str) -> Result<Option<Project>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                 FROM projects WHERE agent_id = ?1 AND name = ?2",
                params![agent_id, name],
                Self::parse_row,
            )
            .optional()
            .wrap_err("Failed to fetch project")?;
        Ok(row)
    }

    fn put(&self, project: &Project) -> Result<()> {
        let conn = self.lock()?;
        let repos_json = serde_json::to_string(&project.repos)?;
        let defaults_json = serde_json::to_string(&project.defaults)?;
        conn.execute(
            "INSERT INTO projects (agent_id, name, repos, defaults, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(agent_id, name) DO UPDATE SET \
                 repos = excluded.repos, \
                 defaults = excluded.defaults, \
                 updated_at = excluded.updated_at",
            params![
                project.agent_id,
                project.name,
                repos_json,
                defaults_json,
                project.created_at.to_rfc3339(),
                project.updated_at.to_rfc3339(),
            ],
        )
        .wrap_err("Failed to upsert project")?;
        Ok(())
    }

    fn delete(&self, agent_id: &str, name: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM projects WHERE agent_id = ?1 AND name = ?2",
                params![agent_id, name],
            )
            .wrap_err("Failed to delete project")?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepoSource;

    fn store() -> SqliteProjectStore {
        SqliteProjectStore::in_memory().unwrap()
    }

    fn sample_repo(alias: &str, slug: &str) -> ProjectRepo {
        let (owner, repo) = slug.split_once('/').unwrap();
        ProjectRepo {
            alias: alias.to_string(),
            source: RepoSource::Github {
                owner: owner.to_string(),
                repo: repo.to_string(),
            },
            default_branch: None,
            setup: None,
        }
    }

    #[test]
    fn put_and_get_round_trips() {
        let s = store();
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        s.put(&p).unwrap();

        let fetched = s.get("allan", "harbor").unwrap().unwrap();
        assert_eq!(fetched.agent_id, "allan");
        assert_eq!(fetched.name, "harbor");
        assert_eq!(fetched.repos.len(), 1);
        assert_eq!(fetched.repos[0].alias, "rails");
    }

    #[test]
    fn get_returns_none_when_missing() {
        let s = store();
        assert!(s.get("nobody", "nothing").unwrap().is_none());
    }

    #[test]
    fn list_filters_by_agent_and_orders_by_name() {
        let s = store();
        s.put(&Project::new("allan", "zeta")).unwrap();
        s.put(&Project::new("allan", "alpha")).unwrap();
        s.put(&Project::new("rita", "alpha")).unwrap();

        let allans = s.list("allan").unwrap();
        assert_eq!(allans.len(), 2);
        assert_eq!(allans[0].name, "alpha");
        assert_eq!(allans[1].name, "zeta");

        let ritas = s.list("rita").unwrap();
        assert_eq!(ritas.len(), 1);
        assert_eq!(ritas[0].name, "alpha");
    }

    #[test]
    fn put_is_upsert_overwriting_repos() {
        let s = store();
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        s.put(&p).unwrap();

        p.repos.clear();
        p.repos
            .push(sample_repo("frontend", "harborlabs/HarborApps"));
        s.put(&p).unwrap();

        let fetched = s.get("allan", "harbor").unwrap().unwrap();
        assert_eq!(fetched.repos.len(), 1);
        assert_eq!(fetched.repos[0].alias, "frontend");
    }

    #[test]
    fn delete_returns_true_on_hit_false_on_miss() {
        let s = store();
        s.put(&Project::new("allan", "harbor")).unwrap();
        assert!(s.delete("allan", "harbor").unwrap());
        assert!(!s.delete("allan", "harbor").unwrap());
        assert!(s.get("allan", "harbor").unwrap().is_none());
    }

    #[test]
    fn delete_is_scoped_to_agent() {
        let s = store();
        s.put(&Project::new("allan", "shared-name")).unwrap();
        s.put(&Project::new("rita", "shared-name")).unwrap();
        assert!(s.delete("allan", "shared-name").unwrap());
        assert!(s.get("allan", "shared-name").unwrap().is_none());
        assert!(s.get("rita", "shared-name").unwrap().is_some());
    }

    #[test]
    fn add_repo_appends_when_alias_new() {
        let s = store();
        s.put(&Project::new("allan", "harbor")).unwrap();
        s.add_repo(
            "allan",
            "harbor",
            sample_repo("rails", "harborlabs/HarborRails"),
        )
        .unwrap();
        let p = s.get("allan", "harbor").unwrap().unwrap();
        assert_eq!(p.repos.len(), 1);
        assert_eq!(p.repos[0].alias, "rails");
    }

    #[test]
    fn add_repo_replaces_when_alias_exists() {
        let s = store();
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        s.put(&p).unwrap();

        let mut replacement = sample_repo("rails", "harborlabs/HarborRails");
        replacement.default_branch = Some("develop".to_string());
        s.add_repo("allan", "harbor", replacement).unwrap();

        let fetched = s.get("allan", "harbor").unwrap().unwrap();
        assert_eq!(fetched.repos.len(), 1);
        assert_eq!(fetched.repos[0].default_branch.as_deref(), Some("develop"));
    }

    #[test]
    fn add_repo_errors_when_project_missing() {
        let s = store();
        let err = s
            .add_repo(
                "allan",
                "missing",
                sample_repo("rails", "harborlabs/HarborRails"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn remove_repo_returns_true_on_hit_false_on_miss() {
        let s = store();
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        s.put(&p).unwrap();

        assert!(s.remove_repo("allan", "harbor", "rails").unwrap());
        assert!(!s.remove_repo("allan", "harbor", "rails").unwrap());

        let fetched = s.get("allan", "harbor").unwrap().unwrap();
        assert!(fetched.repos.is_empty());
    }

    #[test]
    fn remove_repo_errors_when_project_missing() {
        let s = store();
        let err = s.remove_repo("allan", "missing", "rails").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn put_then_add_repo_bumps_updated_at() {
        let s = store();
        let mut p = Project::new("allan", "harbor");
        let original_updated_at = p.updated_at;
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        s.put(&p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.add_repo(
            "allan",
            "harbor",
            sample_repo("frontend", "harborlabs/HarborApps"),
        )
        .unwrap();
        let fetched = s.get("allan", "harbor").unwrap().unwrap();
        assert!(fetched.updated_at > original_updated_at);
    }

    #[test]
    fn list_all_returns_every_agents_projects_ordered_by_agent_then_name() {
        let s = store();
        s.put(&Project::new("rita", "alpha")).unwrap();
        s.put(&Project::new("allan", "zeta")).unwrap();
        s.put(&Project::new("allan", "alpha")).unwrap();

        let all = s.list_all().unwrap();
        let pairs: Vec<(&str, &str)> = all
            .iter()
            .map(|p| (p.agent_id.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("allan", "alpha"), ("allan", "zeta"), ("rita", "alpha")]
        );
    }

    #[test]
    fn rename_moves_manifest_under_new_name_and_preserves_repos() {
        let s = store();
        let mut p = Project::new("allan", "old-name");
        p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
        p.defaults.default_branch = Some("trunk".to_string());
        s.put(&p).unwrap();
        let original_created_at = p.created_at;

        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(s.rename("allan", "old-name", "new-name").unwrap());

        // Old name vacated, new name carries every field across.
        assert!(s.get("allan", "old-name").unwrap().is_none());
        let renamed = s.get("allan", "new-name").unwrap().unwrap();
        assert_eq!(renamed.name, "new-name");
        assert_eq!(renamed.repos.len(), 1);
        assert_eq!(renamed.repos[0].alias, "rails");
        assert_eq!(renamed.defaults.default_branch.as_deref(), Some("trunk"));
        assert_eq!(renamed.created_at, original_created_at);
        assert!(renamed.updated_at > original_created_at);
    }

    #[test]
    fn rename_returns_false_when_source_missing() {
        let s = store();
        assert!(!s.rename("allan", "missing", "new-name").unwrap());
        assert!(s.get("allan", "new-name").unwrap().is_none());
    }

    #[test]
    fn rename_errors_when_destination_exists_for_same_agent() {
        let s = store();
        s.put(&Project::new("allan", "a")).unwrap();
        s.put(&Project::new("allan", "b")).unwrap();
        let err = s.rename("allan", "a", "b").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // Source untouched after the conflict.
        assert!(s.get("allan", "a").unwrap().is_some());
        assert!(s.get("allan", "b").unwrap().is_some());
    }

    #[test]
    fn rename_allows_same_name_in_different_agents() {
        let s = store();
        s.put(&Project::new("rita", "shared")).unwrap();
        s.put(&Project::new("allan", "old")).unwrap();
        // rita has 'shared' but renaming allan's old → shared must
        // succeed: project namespace is per-agent.
        assert!(s.rename("allan", "old", "shared").unwrap());
        assert!(s.get("allan", "shared").unwrap().is_some());
        assert!(s.get("rita", "shared").unwrap().is_some());
    }

    #[test]
    fn rename_rejects_no_op_same_name() {
        let s = store();
        s.put(&Project::new("allan", "p")).unwrap();
        let err = s.rename("allan", "p", "p").unwrap_err();
        assert!(err.to_string().contains("identical"), "{err}");
    }

    #[test]
    fn schema_persists_across_open_close() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("projects.db");

        let s1 = SqliteProjectStore::new(&db).unwrap();
        s1.put(&Project::new("allan", "harbor")).unwrap();
        drop(s1);

        let s2 = SqliteProjectStore::new(&db).unwrap();
        assert!(s2.get("allan", "harbor").unwrap().is_some());
    }
}

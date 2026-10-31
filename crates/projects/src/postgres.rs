//! Postgres backend for [`ProjectStore`].
//!
//! Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqliteProjectStore`]. Selected at runtime via
//! `[projects] backend = "postgres"` in `workforce.toml`; the
//! [`ProjectStore`] trait is stable so call sites are unchanged
//! regardless of which backend is in use.
//!
//! ## Async-to-sync bridge
//!
//! [`ProjectStore`] is a synchronous trait (matching the SQLite backend
//! and the rest of the workforce store traits). `tokio_postgres` is
//! async; we bridge with [`tokio::task::block_in_place`] +
//! `Handle::current().block_on(...)`, which yields the calling worker
//! thread to the runtime for the duration of the query. This requires
//! the multi-threaded tokio runtime that the workforce hub always uses.
//!
//! ## Schema and search_path
//!
//! Tables live in the `projects` Postgres schema. The pool is built via
//! [`PostgresProjectStore::build_pool`] which installs a `post_create`
//! hook that runs `SET search_path TO projects, public` on every new
//! connection — so unqualified table names always resolve regardless of
//! which connection deadpool hands out.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::model::{Project, ProjectDefaults, ProjectRepo};
use crate::store::ProjectStore;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresProjectStore {
    pool: PostgresPool,
}

impl PostgresProjectStore {
    pub const SCHEMA: &'static str = "projects";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO projects, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building project pool")?;
        Self::with_pool(pool).await
    }

    /// Variant that takes an already-built pool (caller is responsible for
    /// pinning `search_path`). Used by tests against the embedded harness.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedProjectMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running project migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedProjectMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedProjectMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

fn row_to_project(row: &Row) -> Result<Project> {
    let repos_json: serde_json::Value = row.try_get("repos")?;
    let repos: Vec<ProjectRepo> = serde_json::from_value(repos_json).unwrap_or_default();
    let defaults_json: serde_json::Value = row.try_get("defaults")?;
    let defaults: ProjectDefaults = serde_json::from_value(defaults_json).unwrap_or_default();
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;

    Ok(Project {
        agent_id: row.try_get("agent_id")?,
        name: row.try_get("name")?,
        defaults,
        repos,
        created_at,
        updated_at,
    })
}

impl ProjectStore for PostgresProjectStore {
    fn list(&self, agent_id: &str) -> Result<Vec<Project>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                     FROM projects WHERE agent_id = $1 ORDER BY name",
                    &[&agent_id],
                )
                .await?;
            rows.iter().map(row_to_project).collect()
        })
    }

    fn list_all(&self) -> Result<Vec<Project>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                     FROM projects ORDER BY agent_id, name",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_project).collect()
        })
    }

    fn get(&self, agent_id: &str, name: &str) -> Result<Option<Project>> {
        let agent_id = agent_id.to_string();
        let name = name.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT agent_id, name, repos, defaults, created_at, updated_at \
                     FROM projects WHERE agent_id = $1 AND name = $2",
                    &[&agent_id, &name],
                )
                .await?;
            row.as_ref().map(row_to_project).transpose()
        })
    }

    fn put(&self, project: &Project) -> Result<()> {
        let project = project.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let repos_json = serde_json::to_value(&project.repos)?;
            let defaults_json = serde_json::to_value(&project.defaults)?;
            conn.execute(
                "INSERT INTO projects (agent_id, name, repos, defaults, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (agent_id, name) DO UPDATE SET \
                     repos = EXCLUDED.repos, \
                     defaults = EXCLUDED.defaults, \
                     updated_at = EXCLUDED.updated_at",
                &[
                    &project.agent_id,
                    &project.name,
                    &repos_json,
                    &defaults_json,
                    &project.created_at,
                    &project.updated_at,
                ],
            )
            .await
            .context("upsert project")?;
            Ok(())
        })
    }

    fn delete(&self, agent_id: &str, name: &str) -> Result<bool> {
        let agent_id = agent_id.to_string();
        let name = name.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "DELETE FROM projects WHERE agent_id = $1 AND name = $2",
                    &[&agent_id, &name],
                )
                .await?;
            Ok(n > 0)
        })
    }

    /// Single-transaction rename so concurrent renames into the same
    /// `new_name` can't both succeed via the default trait
    /// implementation's read-then-write window. Locks the source row
    /// with `SELECT … FOR UPDATE` to serialise siblings, then runs the
    /// conflict check + name update inside the same transaction.
    fn rename(&self, agent_id: &str, old_name: &str, new_name: &str) -> Result<bool> {
        if old_name == new_name {
            return Err(eyre::eyre!(
                "rename: old_name and new_name are identical ('{}')",
                old_name
            ));
        }
        let agent_id = agent_id.to_string();
        let old_name = old_name.to_string();
        let new_name = new_name.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await.context("begin rename tx")?;

            let lock = tx
                .query_opt(
                    "SELECT 1 FROM projects WHERE agent_id = $1 AND name = $2 FOR UPDATE",
                    &[&agent_id, &old_name],
                )
                .await
                .context("lock old project row")?;
            if lock.is_none() {
                tx.commit().await.context("commit rename tx (miss)")?;
                return Ok(false);
            }

            let exists = tx
                .query_opt(
                    "SELECT 1 FROM projects WHERE agent_id = $1 AND name = $2",
                    &[&agent_id, &new_name],
                )
                .await
                .context("check new_name conflict")?;
            if exists.is_some() {
                return Err(eyre::eyre!(
                    "project '{}' already exists for agent '{}'",
                    new_name,
                    agent_id
                ));
            }

            let now = Utc::now();
            let n = tx
                .execute(
                    "UPDATE projects SET name = $1, updated_at = $2 \
                     WHERE agent_id = $3 AND name = $4",
                    &[&new_name, &now, &agent_id, &old_name],
                )
                .await
                .context("apply project rename")?;
            tx.commit().await.context("commit rename tx")?;
            Ok(n > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepoSource;
    use workforce_postgres::test_support::with_test_pool;

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

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresProjectStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresProjectStore::build_pool, |pool| async move {
            let store = PostgresProjectStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_put_get_round_trips() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            let mut p = Project::new("allan", "harbor");
            p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
            s.put(&p).unwrap();

            let fetched = s.get("allan", "harbor").unwrap().unwrap();
            assert_eq!(fetched.repos.len(), 1);
            assert_eq!(fetched.repos[0].alias, "rails");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_get_returns_none_when_missing() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            assert!(s.get("nobody", "nothing").unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_filters_by_agent_and_orders_by_name() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            s.put(&Project::new("allan", "zeta")).unwrap();
            s.put(&Project::new("allan", "alpha")).unwrap();
            s.put(&Project::new("rita", "alpha")).unwrap();

            let allans = s.list("allan").unwrap();
            assert_eq!(allans.len(), 2);
            assert_eq!(allans[0].name, "alpha");
            assert_eq!(allans[1].name, "zeta");

            let ritas = s.list("rita").unwrap();
            assert_eq!(ritas.len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_put_is_upsert_overwriting_repos() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
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
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_returns_true_on_hit_false_on_miss() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            s.put(&Project::new("allan", "harbor")).unwrap();
            assert!(s.delete("allan", "harbor").unwrap());
            assert!(!s.delete("allan", "harbor").unwrap());
            assert!(s.get("allan", "harbor").unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_add_repo_appends_then_replaces_by_alias() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            s.put(&Project::new("allan", "harbor")).unwrap();
            s.add_repo(
                "allan",
                "harbor",
                sample_repo("rails", "harborlabs/HarborRails"),
            )
            .unwrap();

            let mut replacement = sample_repo("rails", "harborlabs/HarborRails");
            replacement.default_branch = Some("develop".to_string());
            s.add_repo("allan", "harbor", replacement).unwrap();

            let fetched = s.get("allan", "harbor").unwrap().unwrap();
            assert_eq!(fetched.repos.len(), 1);
            assert_eq!(fetched.repos[0].default_branch.as_deref(), Some("develop"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_remove_repo_returns_true_on_hit_false_on_miss() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            let mut p = Project::new("allan", "harbor");
            p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
            s.put(&p).unwrap();

            assert!(s.remove_repo("allan", "harbor", "rails").unwrap());
            assert!(!s.remove_repo("allan", "harbor", "rails").unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_all_returns_every_agents_projects() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
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
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_add_repo_errors_when_project_missing() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            let err = s
                .add_repo(
                    "allan",
                    "missing",
                    sample_repo("rails", "harborlabs/HarborRails"),
                )
                .unwrap_err();
            assert!(err.to_string().contains("not found"), "{err}");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_rename_round_trips_through_transaction() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            let mut p = Project::new("allan", "old-name");
            p.repos.push(sample_repo("rails", "harborlabs/HarborRails"));
            s.put(&p).unwrap();

            assert!(s.rename("allan", "old-name", "new-name").unwrap());
            assert!(s.get("allan", "old-name").unwrap().is_none());
            let renamed = s.get("allan", "new-name").unwrap().unwrap();
            assert_eq!(renamed.repos.len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_rename_returns_false_when_source_missing() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            assert!(!s.rename("allan", "missing", "anything").unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_rename_errors_when_destination_exists() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn ProjectStore> = store;
            s.put(&Project::new("allan", "a")).unwrap();
            s.put(&Project::new("allan", "b")).unwrap();
            let err = s.rename("allan", "a", "b").unwrap_err();
            assert!(err.to_string().contains("already exists"), "{err}");
            // Both untouched on conflict — the FOR UPDATE row lock
            // releases on rollback without mutating either row.
            assert!(s.get("allan", "a").unwrap().is_some());
            assert!(s.get("allan", "b").unwrap().is_some());
        })
        .await;
    }
}

//! One-shot SQLite → Postgres migration for the projects store.
//!
//! Reads the `projects` table from a SQLite `projects.db` file and
//! writes it into a Postgres backend (after running the refinery
//! migrations against the destination). Driven by the
//! `workforce migrate-projects` CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (agent_id, name)
//! DO NOTHING`, so re-running on a partially-migrated destination is safe.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresProjectStore;

#[derive(Debug, Default, Clone)]
pub struct ProjectMigrationReport {
    pub projects: usize,
}

impl ProjectMigrationReport {
    pub fn total(&self) -> usize {
        self.projects
    }
}

/// Migrate a SQLite projects database into a Postgres backend.
///
/// Builds a fresh project pool from `pg_config` (with `search_path`
/// pinned to the projects schema), opens the SQLite file read-only,
/// runs the refinery migrations against the destination, then copies
/// every row.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<ProjectMigrationReport> {
    let pool = PostgresProjectStore::build_pool(pg_config)
        .context("building projects pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the projects schema (use [`PostgresProjectStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<ProjectMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresProjectStore> = PostgresProjectStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres projects destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;
    let projects = read_projects(&sqlite)?;
    drop(sqlite);

    let count = write_projects(&pool, projects).await?;
    Ok(ProjectMigrationReport { projects: count })
}

struct ProjectRow {
    agent_id: String,
    name: String,
    repos: serde_json::Value,
    defaults: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn read_projects(sqlite: &Connection) -> Result<Vec<ProjectRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT agent_id, name, repos, defaults, created_at, updated_at
         FROM projects",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let repos_text: String = r.get(2)?;
            let defaults_text: String = r.get(3)?;
            let created_at_str: String = r.get(4)?;
            let updated_at_str: String = r.get(5)?;
            Ok(ProjectRow {
                agent_id: r.get(0)?,
                name: r.get(1)?,
                repos: serde_json::from_str(&repos_text).unwrap_or_else(|_| serde_json::json!([])),
                defaults: serde_json::from_str(&defaults_text)
                    .unwrap_or_else(|_| serde_json::json!({})),
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_projects(pool: &PostgresPool, rows: Vec<ProjectRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO projects
             (agent_id, name, repos, defaults, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (agent_id, name) DO NOTHING",
            &[
                &r.agent_id,
                &r.name,
                &r.repos,
                &r.defaults,
                &r.created_at,
                &r.updated_at,
            ],
        )
        .await
        .context("insert projects row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Project, ProjectRepo, RepoSource};
    use crate::sqlite::SqliteProjectStore;
    use crate::store::ProjectStore;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_projects_round_trip() {
        with_test_pool(PostgresProjectStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteProjectStore::new(tmp.path()).unwrap();
            let mut project = Project::new("allan", "harbor");
            project.defaults.default_branch = Some("trunk".to_string());
            project.repos.push(ProjectRepo {
                alias: "rails".to_string(),
                source: RepoSource::Github {
                    owner: "harborlabs".to_string(),
                    repo: "HarborRails".to_string(),
                },
                default_branch: Some("develop".to_string()),
                setup: None,
            });
            sqlite.put(&project).unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.projects, 1);

            let dest = PostgresProjectStore::with_pool(pool.clone()).await.unwrap();
            let store: Arc<dyn ProjectStore> = dest;
            let fetched = store
                .get("allan", "harbor")
                .unwrap()
                .expect("project present");
            assert_eq!(fetched.defaults.default_branch.as_deref(), Some("trunk"));
            assert_eq!(fetched.repos.len(), 1);
            assert_eq!(fetched.repos[0].alias, "rails");
            assert_eq!(fetched.repos[0].default_branch.as_deref(), Some("develop"));
            // Postgres timestamptz stores microseconds; the nanosecond tail
            // of Utc::now() is lost in the round trip, so compare at µs.
            assert_eq!(
                fetched.created_at.timestamp_micros(),
                project.created_at.timestamp_micros()
            );

            // Idempotency: re-running is a no-op, not a duplicate-key error.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.projects, 1);
        })
        .await;
    }
}

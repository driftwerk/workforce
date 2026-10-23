//! One-shot SQLite → Postgres migration for the MCP server store.
//!
//! Reads the `mcp_servers` table from a SQLite `mcp_store.db` file and
//! writes it into a Postgres backend (after running the refinery
//! migrations against the destination). Driven by the
//! `workforce migrate-mcp-store` CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (agent_id, server_id)
//! DO NOTHING`, so re-running on a partially-migrated destination is safe.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresMcpServerStore;

#[derive(Debug, Default, Clone)]
pub struct McpMigrationReport {
    pub servers: usize,
}

impl McpMigrationReport {
    pub fn total(&self) -> usize {
        self.servers
    }
}

/// Migrate a SQLite MCP-server database into a Postgres backend.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<McpMigrationReport> {
    let pool = PostgresMcpServerStore::build_pool(pg_config)
        .context("building mcp-store pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the mcp_store schema (use [`PostgresMcpServerStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<McpMigrationReport> {
    let _store: Arc<PostgresMcpServerStore> = PostgresMcpServerStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres mcp-store destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;
    let servers = read_servers(&sqlite)?;
    drop(sqlite);

    let count = write_servers(&pool, servers).await?;
    Ok(McpMigrationReport { servers: count })
}

struct McpRow {
    agent_id: String,
    server_id: String,
    config: serde_json::Value,
    installed_via: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn read_servers(sqlite: &Connection) -> Result<Vec<McpRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT agent_id, server_id, config, installed_via, created_at, updated_at
         FROM mcp_servers",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let config_text: String = r.get(2)?;
            let created_at_str: String = r.get(4)?;
            let updated_at_str: String = r.get(5)?;
            Ok(McpRow {
                agent_id: r.get(0)?,
                server_id: r.get(1)?,
                config: serde_json::from_str(&config_text)
                    .unwrap_or_else(|_| serde_json::json!({})),
                installed_via: r.get(3)?,
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

async fn write_servers(pool: &PostgresPool, rows: Vec<McpRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO mcp_servers
             (agent_id, server_id, config, installed_via, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (agent_id, server_id) DO NOTHING",
            &[
                &r.agent_id,
                &r.server_id,
                &r.config,
                &r.installed_via,
                &r.created_at,
                &r.updated_at,
            ],
        )
        .await
        .context("insert mcp_servers row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InstalledVia, McpServerRecord};
    use crate::sqlite::SqliteMcpServerStore;
    use crate::store::McpServerStore;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_mcp_servers_round_trip() {
        with_test_pool(PostgresMcpServerStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteMcpServerStore::new(tmp.path()).unwrap();
            let record = McpServerRecord::new(
                "agent-a",
                "linear",
                serde_json::json!({"auth": {"provider": "linear"}}),
                InstalledVia::Agent,
            );
            sqlite.put(&record).unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.servers, 1);

            let dest = PostgresMcpServerStore::with_pool(pool.clone())
                .await
                .unwrap();
            let store: Arc<dyn McpServerStore> = dest;
            let fetched = store
                .get("agent-a", "linear")
                .unwrap()
                .expect("server present");
            assert_eq!(fetched.installed_via, InstalledVia::Agent);
            assert_eq!(
                fetched.config,
                serde_json::json!({"auth": {"provider": "linear"}})
            );

            // Idempotency: re-running is a no-op, not a duplicate-key error.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.servers, 1);
        })
        .await;
    }
}

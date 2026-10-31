//! Postgres backend for [`McpServerStore`].
//!
//! Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqliteMcpServerStore`]. Selected at runtime via
//! `[mcp_store] backend = "postgres"` in `workforce.toml`; the
//! [`McpServerStore`] trait is stable so call sites are unchanged
//! regardless of which backend is in use.
//!
//! The async-to-sync bridge and per-store-pool architecture are the
//! same as `workforce-projects`; see that crate's `postgres.rs` doc
//! comment for the full rationale.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::model::{InstalledVia, McpServerRecord};
use crate::store::McpServerStore;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresMcpServerStore {
    pool: PostgresPool,
}

impl PostgresMcpServerStore {
    pub const SCHEMA: &'static str = "mcp_store";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO mcp_store, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building mcp-store pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedMcpStoreMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running mcp-store migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedMcpStoreMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedMcpStoreMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

fn row_to_record(row: &Row) -> Result<McpServerRecord> {
    let config: serde_json::Value = row.try_get("config")?;
    let installed_via_str: String = row.try_get("installed_via")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
    Ok(McpServerRecord {
        agent_id: row.try_get("agent_id")?,
        server_id: row.try_get("server_id")?,
        config,
        installed_via: InstalledVia::parse(&installed_via_str),
        created_at,
        updated_at,
    })
}

impl McpServerStore for PostgresMcpServerStore {
    fn list(&self, agent_id: &str) -> Result<Vec<McpServerRecord>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                     FROM mcp_servers WHERE agent_id = $1 ORDER BY server_id",
                    &[&agent_id],
                )
                .await?;
            rows.iter().map(row_to_record).collect()
        })
    }

    fn list_all(&self) -> Result<Vec<McpServerRecord>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                     FROM mcp_servers ORDER BY agent_id, server_id",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_record).collect()
        })
    }

    fn get(&self, agent_id: &str, server_id: &str) -> Result<Option<McpServerRecord>> {
        let agent_id = agent_id.to_string();
        let server_id = server_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                     FROM mcp_servers WHERE agent_id = $1 AND server_id = $2",
                    &[&agent_id, &server_id],
                )
                .await?;
            row.as_ref().map(row_to_record).transpose()
        })
    }

    fn put(&self, record: &McpServerRecord) -> Result<()> {
        let record = record.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let installed_via = record.installed_via.as_str();
            conn.execute(
                "INSERT INTO mcp_servers \
                     (agent_id, server_id, config, installed_via, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (agent_id, server_id) DO UPDATE SET \
                     config        = EXCLUDED.config, \
                     installed_via = EXCLUDED.installed_via, \
                     updated_at    = EXCLUDED.updated_at",
                &[
                    &record.agent_id,
                    &record.server_id,
                    &record.config,
                    &installed_via,
                    &record.created_at,
                    &record.updated_at,
                ],
            )
            .await
            .context("upsert mcp server")?;
            Ok(())
        })
    }

    fn delete(&self, agent_id: &str, server_id: &str) -> Result<bool> {
        let agent_id = agent_id.to_string();
        let server_id = server_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "DELETE FROM mcp_servers WHERE agent_id = $1 AND server_id = $2",
                    &[&agent_id, &server_id],
                )
                .await?;
            Ok(n > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    fn sample_config(endpoint: &str) -> serde_json::Value {
        json!({
            "endpoint": endpoint,
            "transport": "streamable-http",
            "auth": { "kind": "pat", "vault_path": "mcp/v1/test/token" },
            "allowed_hosts": [],
            "refresh_interval_secs": 3600,
            "shadows": [],
        })
    }

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresMcpServerStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresMcpServerStore::build_pool, |pool| async move {
            let store = PostgresMcpServerStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_put_get_round_trips() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            let r = McpServerRecord::new(
                "allan",
                "linear",
                sample_config("https://mcp.linear.app/mcp"),
                InstalledVia::Agent,
            );
            s.put(&r).unwrap();
            let fetched = s.get("allan", "linear").unwrap().unwrap();
            assert_eq!(fetched.config["endpoint"], "https://mcp.linear.app/mcp");
            assert_eq!(fetched.installed_via, InstalledVia::Agent);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_get_returns_none_when_missing() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            assert!(s.get("nobody", "missing").unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_filters_by_agent_and_orders_by_server_id() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            s.put(&McpServerRecord::new(
                "allan",
                "z",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            s.put(&McpServerRecord::new(
                "allan",
                "a",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            s.put(&McpServerRecord::new(
                "rita",
                "a",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            let allans = s.list("allan").unwrap();
            assert_eq!(allans.len(), 2);
            assert_eq!(allans[0].server_id, "a");
            assert_eq!(allans[1].server_id, "z");
            let ritas = s.list("rita").unwrap();
            assert_eq!(ritas.len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_put_is_upsert_overwriting_config() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            let mut r =
                McpServerRecord::new("allan", "linear", sample_config("old"), InstalledVia::Toml);
            s.put(&r).unwrap();
            r.config = sample_config("new");
            r.installed_via = InstalledVia::Agent;
            r.updated_at = now_micros();
            s.put(&r).unwrap();
            let fetched = s.get("allan", "linear").unwrap().unwrap();
            assert_eq!(fetched.config["endpoint"], "new");
            assert_eq!(fetched.installed_via, InstalledVia::Agent);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_returns_true_on_hit_false_on_miss() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            s.put(&McpServerRecord::new(
                "allan",
                "linear",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            assert!(s.delete("allan", "linear").unwrap());
            assert!(!s.delete("allan", "linear").unwrap());
            assert!(s.get("allan", "linear").unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_all_returns_every_agents_servers() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn McpServerStore> = store;
            s.put(&McpServerRecord::new(
                "rita",
                "alpha",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            s.put(&McpServerRecord::new(
                "allan",
                "zeta",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            s.put(&McpServerRecord::new(
                "allan",
                "alpha",
                sample_config("u"),
                InstalledVia::Agent,
            ))
            .unwrap();
            let all = s.list_all().unwrap();
            let pairs: Vec<(&str, &str)> = all
                .iter()
                .map(|r| (r.agent_id.as_str(), r.server_id.as_str()))
                .collect();
            assert_eq!(
                pairs,
                vec![("allan", "alpha"), ("allan", "zeta"), ("rita", "alpha")]
            );
        })
        .await;
    }
}

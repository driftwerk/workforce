//! Postgres backend for [`crate::metabase_schema::MetabaseSchemaStore`].
//!
//! Tables live in the `metabase_schema` schema; the pool pins `search_path` so
//! unqualified names resolve. Usage counters are written as additive deltas, so
//! two hub replicas flushing concurrently compose instead of one dropping the
//! other's counts.

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::{Context, Result};
use serde_json::Value;

use crate::metabase_schema::{DbSchema, InstanceSchema, MetabaseSchemaStore, TableUsage};
use crate::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

mod embedded {
    refinery::embed_migrations!("migrations/metabase_schema");
}

pub struct MetabaseSchemaPostgresStore {
    pool: PostgresPool,
}

impl MetabaseSchemaPostgresStore {
    pub const SCHEMA: &'static str = "metabase_schema";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO metabase_schema, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building metabase schema pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        run_metabase_schema_migrations(&pool).await?;
        Ok(Arc::new(Self { pool }))
    }

    /// `true` when `instance` is stored against a different `site_url` — the
    /// names belong to another Metabase and must not be served.
    async fn is_foreign(&self, instance: &str, site_url: &str) -> Result<bool> {
        let conn = self.pool.get().await?;
        let stored: Option<String> = conn
            .query_opt(
                "SELECT site_url FROM instances WHERE instance = $1",
                &[&instance],
            )
            .await?
            .map(|row| row.get(0));
        Ok(stored.is_some_and(|stored| stored != site_url))
    }
}

fn to_db_schema(tables: Value, columns: Value, engine: Option<String>) -> DbSchema {
    DbSchema {
        tables: serde_json::from_value(tables).unwrap_or_default(),
        columns: serde_json::from_value(columns).unwrap_or_default(),
        engine,
    }
}

#[async_trait::async_trait]
impl MetabaseSchemaStore for MetabaseSchemaPostgresStore {
    async fn load(&self, instance: &str, site_url: &str) -> Result<InstanceSchema> {
        if self.is_foreign(instance, site_url).await? {
            return Ok(InstanceSchema::default());
        }
        let conn = self.pool.get().await?;
        let databases = conn
            .query(
                "SELECT database_id, tables, columns, engine FROM databases WHERE instance = $1",
                &[&instance],
            )
            .await?
            .into_iter()
            .map(|row| {
                (
                    row.get::<_, i64>(0),
                    to_db_schema(row.get(1), row.get(2), row.get(3)),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let usage = conn
            .query(
                "SELECT database_id, table_name, hits, misses FROM table_usage WHERE instance = $1",
                &[&instance],
            )
            .await?
            .into_iter()
            .map(|row| {
                (
                    (row.get::<_, i64>(0), row.get::<_, String>(1)),
                    TableUsage {
                        hits: row.get::<_, i64>(2).max(0) as u64,
                        misses: row.get::<_, i64>(3).max(0) as u64,
                    },
                )
            })
            .collect();
        Ok(InstanceSchema { databases, usage })
    }

    async fn upsert_database(
        &self,
        instance: &str,
        site_url: &str,
        database_id: i64,
        schema: &DbSchema,
    ) -> Result<()> {
        let tables = serde_json::to_value(&schema.tables)?;
        let columns = serde_json::to_value(&schema.columns)?;
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "INSERT INTO instances (instance, site_url) VALUES ($1, $2)
             ON CONFLICT (instance) DO UPDATE SET site_url = EXCLUDED.site_url, synced_at = now()",
            &[&instance, &site_url],
        )
        .await?;
        tx.execute(
            "INSERT INTO databases (instance, database_id, tables, columns, engine)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (instance, database_id) DO UPDATE
             SET tables = EXCLUDED.tables, columns = EXCLUDED.columns,
                 engine = COALESCE(EXCLUDED.engine, databases.engine), synced_at = now()",
            &[&instance, &database_id, &tables, &columns, &schema.engine],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_engines(
        &self,
        instance: &str,
        site_url: &str,
        engines: &[(i64, String)],
    ) -> Result<()> {
        if engines.is_empty() {
            return Ok(());
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "INSERT INTO instances (instance, site_url) VALUES ($1, $2)
             ON CONFLICT (instance) DO UPDATE SET site_url = EXCLUDED.site_url, synced_at = now()",
            &[&instance, &site_url],
        )
        .await?;
        let statement = tx
            .prepare(
                "INSERT INTO databases (instance, database_id, tables, columns, engine)
                 VALUES ($1, $2, '[]'::jsonb, '{}'::jsonb, $3)
                 ON CONFLICT (instance, database_id) DO UPDATE SET engine = EXCLUDED.engine",
            )
            .await?;
        for (database_id, engine) in engines {
            tx.execute(&statement, &[&instance, database_id, engine])
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn prune_databases(&self, instance: &str, keep: &[i64]) -> Result<()> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM databases WHERE instance = $1 AND NOT (database_id = ANY($2))",
            &[&instance, &keep],
        )
        .await?;
        tx.execute(
            "DELETE FROM table_usage WHERE instance = $1 AND NOT (database_id = ANY($2))",
            &[&instance, &keep],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn add_usage(&self, instance: &str, deltas: &[(i64, String, TableUsage)]) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let statement = tx
            .prepare(
                "INSERT INTO table_usage AS u (instance, database_id, table_name, hits, misses)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (instance, database_id, table_name) DO UPDATE
                 SET hits = u.hits + EXCLUDED.hits, misses = u.misses + EXCLUDED.misses",
            )
            .await?;
        for (database_id, table, usage) in deltas {
            tx.execute(
                &statement,
                &[
                    &instance,
                    database_id,
                    table,
                    &(usage.hits as i64),
                    &(usage.misses as i64),
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

pub async fn run_metabase_schema_migrations(pool: &PostgresPool) -> Result<()> {
    let migration_set = MigrationSet::new(
        MetabaseSchemaPostgresStore::SCHEMA,
        Box::new(EmbeddedMetabaseSchemaMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[migration_set])
        .await
        .context("running metabase schema migrations")?;
    Ok(())
}

struct EmbeddedMetabaseSchemaMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedMetabaseSchemaMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(|e| PostgresError::Migration(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    fn schema(tables: &[&str], columns: &[(&str, &[&str])]) -> DbSchema {
        DbSchema {
            tables: tables.iter().map(|t| t.to_string()).collect(),
            columns: columns
                .iter()
                .map(|(t, cols)| {
                    (
                        t.to_string(),
                        cols.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn schema_round_trips_and_usage_accumulates() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            let site = "https://mb.example.com";
            store
                .upsert_database(
                    "default",
                    site,
                    1,
                    &DbSchema {
                        engine: Some("postgres".into()),
                        ..schema(&["people"], &[("people", &["id"])])
                    },
                )
                .await
                .unwrap();
            store
                .add_usage(
                    "default",
                    &[(1, "people".into(), TableUsage { hits: 2, misses: 1 })],
                )
                .await
                .unwrap();

            let loaded = store.load("default", site).await.unwrap();
            assert_eq!(loaded.databases[&1].tables, vec!["people".to_string()]);
            assert_eq!(
                loaded.databases[&1].columns["people"],
                vec!["id".to_string()]
            );
            assert_eq!(loaded.databases[&1].engine.as_deref(), Some("postgres"));
            assert_eq!(
                loaded.usage[&(1, "people".to_string())],
                TableUsage { hits: 2, misses: 1 }
            );
        })
        .await;
    }

    #[tokio::test]
    async fn an_upsert_without_an_engine_keeps_the_stored_one() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            let site = "https://mb.example.com";
            store
                .upsert_engines("default", site, &[(1, "mysql".into())])
                .await
                .unwrap();
            store
                .upsert_database("default", site, 1, &schema(&["people"], &[]))
                .await
                .unwrap();

            let loaded = store.load("default", site).await.unwrap();
            assert_eq!(loaded.databases[&1].tables, vec!["people".to_string()]);
            assert_eq!(
                loaded.databases[&1].engine.as_deref(),
                Some("mysql"),
                "a names-only write never erases a known engine"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn engines_upsert_creates_rows_that_load_with_no_tables() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            let site = "https://mb.example.com";
            store
                .upsert_engines(
                    "default",
                    site,
                    &[(3, "postgres".into()), (4, "mysql".into())],
                )
                .await
                .unwrap();

            let loaded = store.load("default", site).await.unwrap();
            assert_eq!(
                loaded.known_engines(),
                vec![(3, "postgres".to_string()), (4, "mysql".to_string())]
            );
            assert!(loaded.databases.values().all(|db| db.tables.is_empty()));

            store.prune_databases("default", &[3]).await.unwrap();
            let pruned = store.load("default", site).await.unwrap();
            assert_eq!(pruned.known_engines(), vec![(3, "postgres".to_string())]);
        })
        .await;
    }

    /// The reason counters are rows rather than part of the snapshot: two hub
    /// replicas flushing at once must compose, not clobber.
    #[tokio::test]
    async fn concurrent_usage_deltas_add_rather_than_overwrite() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            let site = "https://mb.example.com";
            store
                .upsert_database("default", site, 1, &schema(&["people"], &[]))
                .await
                .unwrap();

            let deltas = vec![(
                1i64,
                "people".to_string(),
                TableUsage { hits: 3, misses: 1 },
            )];
            let (a, b) = tokio::join!(
                store.add_usage("default", &deltas),
                store.add_usage("default", &deltas)
            );
            a.unwrap();
            b.unwrap();

            let loaded = store.load("default", site).await.unwrap();
            assert_eq!(
                loaded.usage[&(1, "people".to_string())],
                TableUsage { hits: 6, misses: 2 },
                "both writers' counts survive"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_different_site_url_reads_as_empty() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            store
                .upsert_database(
                    "default",
                    "https://mb.example.com",
                    1,
                    &schema(&["people"], &[]),
                )
                .await
                .unwrap();

            let other = store
                .load("default", "https://elsewhere.example.com")
                .await
                .unwrap();
            assert!(
                other.databases.is_empty(),
                "one instance's names must never be served for another"
            );
            let same = store
                .load("default", "https://mb.example.com")
                .await
                .unwrap();
            assert_eq!(same.databases.len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn pruning_drops_departed_databases_and_their_usage() {
        with_test_pool(MetabaseSchemaPostgresStore::build_pool, |pool| async move {
            let store = MetabaseSchemaPostgresStore::with_pool(pool).await.unwrap();
            let site = "https://mb.example.com";
            for id in [1i64, 2] {
                store
                    .upsert_database("default", site, id, &schema(&["t"], &[]))
                    .await
                    .unwrap();
                store
                    .add_usage(
                        "default",
                        &[(id, "t".into(), TableUsage { hits: 1, misses: 0 })],
                    )
                    .await
                    .unwrap();
            }

            store.prune_databases("default", &[1]).await.unwrap();

            let loaded = store.load("default", site).await.unwrap();
            assert_eq!(
                loaded.databases.keys().copied().collect::<Vec<_>>(),
                vec![1]
            );
            assert!(
                loaded.usage.keys().all(|(db, _)| *db == 1),
                "a departed database takes its counters with it"
            );
        })
        .await;
    }
}

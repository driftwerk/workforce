//! Postgres backend: `budget` schema, shared across hub instances so an
//! override granted on one replica applies fleet-wide within the governor's
//! cache window. Approvals-store shape: sync trait over a `block_on` shim.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{BudgetOverride, BudgetOverrideStore, OverrideAuditEntry, OverrideScope};

// Own directory, never a subdir of another embed root — refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_budget");
}

pub struct PostgresBudgetOverrideStore {
    pool: PostgresPool,
}

impl PostgresBudgetOverrideStore {
    pub const SCHEMA: &'static str = "budget";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(config, &["SET search_path TO budget, public"])
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building budget overrides pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedBudgetMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[set])
            .await
            .context("running budget override migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedBudgetMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedBudgetMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

impl BudgetOverrideStore for PostgresBudgetOverrideStore {
    fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<BudgetOverride>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT scope, key, daily_usd, expires_at, note, created_by, updated_at
                     FROM budget_overrides
                     WHERE expires_at IS NULL OR expires_at > $1",
                    &[&now],
                )
                .await?;
            Ok(rows
                .into_iter()
                .filter_map(|r| {
                    Some(BudgetOverride {
                        scope: OverrideScope::parse(r.get::<_, &str>(0))?,
                        key: r.get(1),
                        daily_usd: r.get(2),
                        expires_at: r.get(3),
                        note: r.get(4),
                        created_by: r.get(5),
                        updated_at: r.get(6),
                    })
                })
                .collect())
        })
    }

    fn upsert(&self, o: &BudgetOverride) -> Result<()> {
        let o = o.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.execute(
                "DELETE FROM budget_overrides WHERE expires_at IS NOT NULL AND expires_at < $1",
                &[&o.updated_at],
            )
            .await?;
            tx.execute(
                "INSERT INTO budget_overrides
                 (scope, key, daily_usd, expires_at, note, created_by, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (scope, key) DO UPDATE SET
                     daily_usd = EXCLUDED.daily_usd,
                     expires_at = EXCLUDED.expires_at,
                     note = EXCLUDED.note,
                     created_by = EXCLUDED.created_by,
                     updated_at = EXCLUDED.updated_at",
                &[
                    &o.scope.as_str(),
                    &o.key,
                    &o.daily_usd,
                    &o.expires_at,
                    &o.note,
                    &o.created_by,
                    &o.updated_at,
                ],
            )
            .await?;
            tx.execute(
                "INSERT INTO budget_override_audit
                 (action, scope, key, daily_usd, expires_at, actor, at)
                 VALUES ('set', $1, $2, $3, $4, $5, $6)",
                &[
                    &o.scope.as_str(),
                    &o.key,
                    &o.daily_usd,
                    &o.expires_at,
                    &o.created_by,
                    &o.updated_at,
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn remove(&self, scope: OverrideScope, key: &str, actor: &str) -> Result<bool> {
        let key = key.to_string();
        let actor = actor.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let removed = tx
                .execute(
                    "DELETE FROM budget_overrides WHERE scope = $1 AND key = $2",
                    &[&scope.as_str(), &key],
                )
                .await?;
            if removed > 0 {
                tx.execute(
                    "INSERT INTO budget_override_audit
                     (action, scope, key, daily_usd, expires_at, actor, at)
                     VALUES ('clear', $1, $2, NULL, NULL, $3, $4)",
                    &[&scope.as_str(), &key, &actor, &Utc::now()],
                )
                .await?;
            }
            tx.commit().await?;
            Ok(removed > 0)
        })
    }

    fn audit(&self, limit: usize) -> Result<Vec<OverrideAuditEntry>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT action, scope, key, daily_usd, expires_at, actor, at
                     FROM budget_override_audit ORDER BY id DESC LIMIT $1",
                    &[&(limit as i64)],
                )
                .await?;
            Ok(rows
                .into_iter()
                .filter_map(|r| {
                    Some(OverrideAuditEntry {
                        action: r.get(0),
                        scope: OverrideScope::parse(r.get::<_, &str>(1))?,
                        key: r.get(2),
                        daily_usd: r.get(3),
                        expires_at: r.get(4),
                        actor: r.get(5),
                        at: r.get(6),
                    })
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    fn row(scope: OverrideScope, key: &str, usd: f64) -> BudgetOverride {
        BudgetOverride {
            scope,
            key: key.to_string(),
            daily_usd: usd,
            expires_at: None,
            note: Some("boost".to_string()),
            created_by: "admin-uuid".to_string(),
            updated_at: now_micros(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_expiry_and_audit() {
        with_test_pool(PostgresBudgetOverrideStore::build_pool, |pool| async move {
            let s = PostgresBudgetOverrideStore::with_pool(pool)
                .await
                .expect("store");
            s.upsert(&row(OverrideScope::Identity, "u1", 100.0))
                .unwrap();
            s.upsert(&row(OverrideScope::Identity, "u1", 150.0))
                .unwrap();
            let mut expiring = row(OverrideScope::Agent, "atlas", 900.0);
            expiring.expires_at = Some(now_micros() - chrono::Duration::minutes(1));
            s.upsert(&expiring).unwrap();

            let active = s.list_active(now_micros()).unwrap();
            assert_eq!(active.len(), 1, "expired agent override filtered");
            assert_eq!(active[0].daily_usd, 150.0);

            assert!(s
                .remove(OverrideScope::Identity, "u1", "admin-uuid")
                .unwrap());
            assert!(s.list_active(now_micros()).unwrap().is_empty());
            let audit = s.audit(10).unwrap();
            assert_eq!(audit[0].action, "clear");
            assert!(audit.iter().filter(|a| a.action == "set").count() >= 3);
        })
        .await;
    }
}

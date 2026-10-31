//! Postgres backend for [`crate::slack_claim::SlackMessageClaimer`].
//!
//! Tables live in the `slack` schema (see [`SlackResponseClaimStore::SCHEMA`]);
//! the pool pins `search_path` so unqualified table names resolve. A single
//! `response_claims` row is one message's claim — written with
//! `INSERT … ON CONFLICT DO NOTHING`, whose row count tells the caller whether
//! it won. Rows are pruned on a timer (see [`SlackResponseClaimStore::spawn_pruner`]).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::{Context, Result};
use tokio::task::JoinHandle;

use crate::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use crate::slack_claim::SlackMessageClaimer;

mod embedded {
    refinery::embed_migrations!("migrations/slack_claims");
}

/// Postgres-backed cross-task claim store.
pub struct SlackResponseClaimStore {
    pool: PostgresPool,
    instance_id: String,
}

impl SlackResponseClaimStore {
    /// Schema this store owns.
    pub const SCHEMA: &'static str = "slack";

    /// Build a [`PostgresPool`] with `search_path` pinned to the `slack` schema.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO slack, public"],
        )
    }

    /// Build a pool from config, run migrations, and return a ready store.
    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building slack claim pool")?;
        Self::with_pool(pool).await
    }

    /// Variant taking an already-built pool (see [`Self::build_pool`]).
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        run_slack_claim_migrations(&pool).await?;
        Ok(Arc::new(Self {
            pool,
            instance_id: uuid::Uuid::new_v4().to_string(),
        }))
    }

    /// Delete claims older than `ttl`, returning how many were removed. Claims
    /// only matter for the seconds around delivery, so the table stays tiny.
    pub async fn prune_expired(&self, ttl: Duration) -> Result<u64> {
        let cutoff = Utc::now() - chrono::Duration::from_std(ttl).context("ttl out of range")?;
        let conn = self.pool.get().await?;
        let removed = conn
            .execute(
                "DELETE FROM response_claims WHERE claimed_at < $1",
                &[&cutoff],
            )
            .await?;
        Ok(removed)
    }

    /// Spawn a background task that prunes expired claims every `every`.
    pub fn spawn_pruner(self: Arc<Self>, every: Duration, ttl: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            loop {
                ticker.tick().await;
                if let Err(e) = self.prune_expired(ttl).await {
                    tracing::warn!(error = %e, "Pruning Slack response claims failed");
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl SlackMessageClaimer for SlackResponseClaimStore {
    async fn try_claim(
        &self,
        agent_id: &str,
        workspace_id: &str,
        channel: &str,
        ts: &str,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let inserted = conn
            .execute(
                "INSERT INTO response_claims (agent_id, workspace_id, channel, ts, claimed_by)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (agent_id, workspace_id, channel, ts) DO NOTHING",
                &[&agent_id, &workspace_id, &channel, &ts, &self.instance_id],
            )
            .await?;
        Ok(inserted == 1)
    }

    async fn release(
        &self,
        agent_id: &str,
        workspace_id: &str,
        channel: &str,
        ts: &str,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM response_claims
             WHERE agent_id = $1 AND workspace_id = $2 AND channel = $3 AND ts = $4",
            &[&agent_id, &workspace_id, &channel, &ts],
        )
        .await?;
        Ok(())
    }
}

/// Apply the embedded migration set against a pool whose `search_path` includes
/// the `slack` schema (use [`SlackResponseClaimStore::build_pool`]).
pub async fn run_slack_claim_migrations(pool: &PostgresPool) -> Result<()> {
    let migration_set = MigrationSet::new(
        SlackResponseClaimStore::SCHEMA,
        Box::new(EmbeddedSlackClaimMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[migration_set])
        .await
        .context("running slack claim migrations")?;
    Ok(())
}

struct EmbeddedSlackClaimMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedSlackClaimMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<SlackResponseClaimStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(SlackResponseClaimStore::build_pool, |pool| async move {
            let store = SlackResponseClaimStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_task_wins_second_skips() {
        with_store(|store| async move {
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
            assert!(!store.try_claim("a", "w", "C1", "100.1").await.unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_messages_each_win() {
        with_store(|store| async move {
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
            assert!(store.try_claim("a", "w", "C1", "100.2").await.unwrap());
            assert!(store.try_claim("a", "w", "C2", "100.1").await.unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_agents_are_independent() {
        with_store(|store| async move {
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
            assert!(store.try_claim("b", "w", "C1", "100.1").await.unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_frees_exactly_the_claimed_message() {
        with_store(|store| async move {
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
            assert!(store.try_claim("a", "w", "C1", "100.2").await.unwrap());

            store.release("a", "w", "C1", "100.1").await.unwrap();
            assert!(
                store.try_claim("a", "w", "C1", "100.1").await.unwrap(),
                "released message is claimable again"
            );
            assert!(
                !store.try_claim("a", "w", "C1", "100.2").await.unwrap(),
                "other claims untouched"
            );

            store.release("a", "w", "C1", "999.9").await.unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_removes_only_expired() {
        with_store(|store| async move {
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
            assert_eq!(
                store
                    .prune_expired(Duration::from_secs(3600))
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                store.prune_expired(Duration::from_secs(0)).await.unwrap(),
                1
            );
            assert!(store.try_claim("a", "w", "C1", "100.1").await.unwrap());
        })
        .await;
    }
}

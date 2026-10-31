//! Postgres backend for [`PendingApprovalStore`].
//!
//! Mirrors `workforce_permissions::postgres::PostgresPermissionStore`:
//! a `block_on` shim over an async pool, a `post_create` session-init
//! hook setting `search_path`, and an embedded refinery migration set.
//! Tables live in the `approvals` schema.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{ApprovalOutcome, PendingApproval, PendingApprovalStore, Target, PRUNABLE};
use workforce_agent::runner::PendingApprovalCore;
use workforce_common::telemetry::{prune_in_batches, PRUNE_BATCH};

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresPendingApprovalStore {
    pool: PostgresPool,
}

impl PostgresPendingApprovalStore {
    pub const SCHEMA: &'static str = "approvals";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO approvals, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building approvals pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedApprovalMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[set])
            .await
            .context("running approval migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedApprovalMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedApprovalMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

fn row_to_approval(row: &Row) -> Result<PendingApproval> {
    let tool_input: String = row.try_get("tool_input")?;
    Ok(PendingApproval {
        request_id: row.try_get::<_, uuid::Uuid>("request_id")?.to_string(),
        agent_id: row.try_get("agent_id")?,
        session_key: row.try_get("session_key")?,
        workspace_id: row.try_get("workspace_id")?,
        channel_id: row.try_get("channel_id")?,
        thread_ts: row.try_get("thread_ts")?,
        requester_id: row.try_get("requester_id")?,
        tool_name: row.try_get("tool_name")?,
        tool_input: serde_json::from_str(&tool_input).unwrap_or(serde_json::Value::Null),
        reason: row.try_get("reason")?,
        allow_self_approval: row.try_get("allow_self_approval")?,
        status: row.try_get("status")?,
        resolved_by: row.try_get("resolved_by")?,
        channel_notice: row.try_get("channel_notice")?,
        channel_notice_ts: row.try_get("channel_notice_ts")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
    })
}

fn row_to_target(row: &Row) -> Result<Target> {
    Ok(Target {
        request_id: row.try_get::<_, uuid::Uuid>("request_id")?.to_string(),
        approver_id: row.try_get("approver_id")?,
        slack_user_id: row.try_get("slack_user_id")?,
        dm_channel: row.try_get("dm_channel")?,
        message_ts: row.try_get("message_ts")?,
    })
}

fn parse_uuid(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).with_context(|| format!("request_id is not a UUID: {s}"))
}

const SELECT_COLS: &str = "request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
     requester_id, tool_name, tool_input, reason, allow_self_approval, status,
     resolved_by, channel_notice, channel_notice_ts, created_at, expires_at";

impl PendingApprovalStore for PostgresPendingApprovalStore {
    fn insert_core(
        &self,
        core: &PendingApprovalCore,
        workspace_id: Option<&str>,
        thread_ts: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let request_id = parse_uuid(&core.request_id)?;
        let tool_input = serde_json::to_string(&core.tool_input)?;
        let workspace_id = workspace_id.map(str::to_string);
        let thread_ts = thread_ts.map(str::to_string);
        let core = core.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO pending_approvals (
                    request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
                    requester_id, tool_name, tool_input, reason, allow_self_approval, status,
                    created_at, expires_at
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'pending', $12, $13)
                 ON CONFLICT (request_id) DO NOTHING",
                &[
                    &request_id,
                    &core.agent_id,
                    &core.session_key,
                    &workspace_id,
                    &core.channel_id,
                    &thread_ts,
                    &core.requester_user_id,
                    &core.tool_name,
                    &tool_input,
                    &core.reason,
                    &core.allow_self_approval,
                    &Utc::now(),
                    &expires_at,
                ],
            )
            .await
            .context("insert_core failed")?;
            Ok(())
        })
    }

    fn add_target(&self, request_id: &str, approver_id: &str, slack_user_id: &str) -> Result<()> {
        let request_id = parse_uuid(request_id)?;
        let approver_id = approver_id.to_string();
        let slack_user_id = slack_user_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO pending_approval_targets (request_id, approver_id, slack_user_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (request_id, approver_id)
                 DO UPDATE SET slack_user_id = EXCLUDED.slack_user_id",
                &[&request_id, &approver_id, &slack_user_id],
            )
            .await
            .context("add_target failed")?;
            Ok(())
        })
    }

    fn set_target_message(
        &self,
        request_id: &str,
        approver_id: &str,
        dm_channel: &str,
        message_ts: &str,
    ) -> Result<()> {
        let request_id = parse_uuid(request_id)?;
        let approver_id = approver_id.to_string();
        let dm_channel = dm_channel.to_string();
        let message_ts = message_ts.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE pending_approval_targets SET dm_channel = $3, message_ts = $4
                 WHERE request_id = $1 AND approver_id = $2",
                &[&request_id, &approver_id, &dm_channel, &message_ts],
            )
            .await
            .context("set_target_message failed")?;
            Ok(())
        })
    }

    fn set_channel_notice(&self, request_id: &str, channel: &str, ts: &str) -> Result<()> {
        let request_id = parse_uuid(request_id)?;
        let channel = channel.to_string();
        let ts = ts.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE pending_approvals SET channel_notice = $2, channel_notice_ts = $3
                 WHERE request_id = $1",
                &[&request_id, &channel, &ts],
            )
            .await
            .context("set_channel_notice failed")?;
            Ok(())
        })
    }

    fn get(&self, request_id: &str) -> Result<Option<PendingApproval>> {
        let request_id = parse_uuid(request_id)?;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!("SELECT {SELECT_COLS} FROM pending_approvals WHERE request_id = $1");
            let row = conn.query_opt(&sql, &[&request_id]).await?;
            row.as_ref().map(row_to_approval).transpose()
        })
    }

    fn targets(&self, request_id: &str) -> Result<Vec<Target>> {
        let request_id = parse_uuid(request_id)?;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT request_id, approver_id, slack_user_id, dm_channel, message_ts
                     FROM pending_approval_targets WHERE request_id = $1 ORDER BY approver_id",
                    &[&request_id],
                )
                .await?;
            rows.iter().map(row_to_target).collect()
        })
    }

    fn claim(&self, request_id: &str, outcome: ApprovalOutcome, resolver_id: &str) -> Result<bool> {
        let request_id = parse_uuid(request_id)?;
        let status = outcome.as_str();
        let resolver_id = resolver_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "UPDATE pending_approvals
                     SET status = $2, resolved_by = $3, resolved_at = $4
                     WHERE request_id = $1 AND status = 'pending'
                       AND ($2 = 'expired' OR expires_at > $4)",
                    &[&request_id, &status, &resolver_id, &Utc::now()],
                )
                .await?;
            Ok(n == 1)
        })
    }

    fn due_for_expiry(&self, now: DateTime<Utc>) -> Result<Vec<PendingApproval>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!(
                "SELECT {SELECT_COLS} FROM pending_approvals
                 WHERE status = 'pending' AND expires_at <= $1
                 ORDER BY expires_at"
            );
            let rows = conn.query(&sql, &[&now]).await?;
            rows.iter().map(row_to_approval).collect()
        })
    }

    fn recently_resolved(&self, limit: usize) -> Result<Vec<PendingApproval>> {
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!(
                "SELECT {SELECT_COLS} FROM pending_approvals
                 WHERE status <> 'pending'
                 ORDER BY resolved_at DESC LIMIT $1"
            );
            let rows = conn.query(&sql, &[&limit]).await?;
            rows.iter().map(row_to_approval).collect()
        })
    }

    fn pending_for(&self, approver: &str, now: DateTime<Utc>) -> Result<Vec<PendingApproval>> {
        let approver = approver.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!(
                "SELECT {SELECT_COLS} FROM pending_approvals
                 WHERE status = 'pending' AND expires_at > $2
                   AND request_id IN (SELECT request_id FROM pending_approval_targets
                                      WHERE approver_id = $1)
                 ORDER BY created_at"
            );
            let rows = conn.query(&sql, &[&approver, &now]).await?;
            rows.iter().map(row_to_approval).collect()
        })
    }

    fn requested_by(&self, requester: &str) -> Result<Vec<PendingApproval>> {
        let requester = requester.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!(
                "SELECT {SELECT_COLS} FROM pending_approvals
                 WHERE requester_id = $1 ORDER BY created_at DESC"
            );
            let rows = conn.query(&sql, &[&requester]).await?;
            rows.iter().map(row_to_approval).collect()
        })
    }

    fn approver_targets(&self, approver: &str) -> Result<usize> {
        let approver = approver.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COUNT(*) FROM pending_approval_targets WHERE approver_id = $1",
                    &[&approver],
                )
                .await?;
            Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or_default())
        })
    }

    fn identity_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn.query(super::HOLDER_ROWS, &[]).await?;
            Ok(super::sum_holders(rows.iter().map(|row| {
                (row.get::<_, String>(0), row.get::<_, i64>(1))
            })))
        })
    }

    fn reassign_requester(&self, from: &str, to: &str) -> Result<usize> {
        self.execute(
            "UPDATE pending_approvals SET requester_id = $2
             WHERE requester_id = $1 AND status = 'pending'",
            &[from, to],
        )
    }

    fn forget_requester(&self, requester: &str) -> Result<usize> {
        self.execute(
            "DELETE FROM pending_approvals WHERE requester_id = $1",
            &[requester],
        )
    }

    fn drop_approver(&self, approver: &str) -> Result<usize> {
        self.execute(
            "DELETE FROM pending_approval_targets WHERE approver_id = $1",
            &[approver],
        )
    }

    fn prune_resolved(&self, before: DateTime<Utc>) -> Result<usize> {
        let delete = format!(
            "DELETE FROM pending_approvals WHERE request_id IN (
                 SELECT request_id FROM pending_approvals WHERE {PRUNABLE}$1
                 LIMIT $2 FOR UPDATE SKIP LOCKED)"
        );
        let batch = PRUNE_BATCH as i64;
        prune_in_batches(|| {
            self.block_on(async {
                let conn = self.pool.get().await?;
                Ok(conn.execute(&delete, &[&before, &batch]).await? as usize)
            })
        })
    }
}

impl PostgresPendingApprovalStore {
    fn execute(&self, sql: &'static str, params: &[&str]) -> Result<usize> {
        let params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
                .iter()
                .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            Ok(usize::try_from(conn.execute(sql, &bound).await?).unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn resolved_requests_are_pruned_past_their_window() {
        with_test_pool(
            PostgresPendingApprovalStore::build_pool,
            |pool| async move {
                let store = PostgresPendingApprovalStore::with_pool(pool).await.unwrap();
                crate::approvals::store::prune_contract(store.as_ref());
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_waits_on_an_approver_is_their_pending_unexpired_requests() {
        with_test_pool(
            PostgresPendingApprovalStore::build_pool,
            |pool| async move {
                let store = PostgresPendingApprovalStore::with_pool(pool).await.unwrap();
                crate::approvals::store::pending_for_contract(store.as_ref());
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn identity_operations_reach_requests_and_approver_targets() {
        with_test_pool(
            PostgresPendingApprovalStore::build_pool,
            |pool| async move {
                let store = PostgresPendingApprovalStore::with_pool(pool).await.unwrap();
                crate::approvals::store::identity_operations_contract(store.as_ref());
            },
        )
        .await;
    }
}

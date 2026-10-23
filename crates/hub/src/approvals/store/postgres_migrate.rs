//! One-shot SQLite → Postgres migration for the pending-approval store.
//!
//! Reads the `pending_approvals` and `pending_approval_targets` tables
//! from a SQLite approvals DB file and writes them into a Postgres
//! backend (after running the refinery migrations against the
//! destination).
//!
//! Idempotent — every insert uses `ON CONFLICT (<pk>) DO NOTHING`, so
//! re-running on a partially-migrated destination is safe.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::approvals::store::postgres::PostgresPendingApprovalStore;

#[derive(Debug, Default, Clone)]
pub struct PendingApprovalMigrationReport {
    pub approvals: usize,
    pub targets: usize,
}

impl PendingApprovalMigrationReport {
    pub fn total(&self) -> usize {
        self.approvals + self.targets
    }
}

/// Migrate a SQLite pending-approval database into a Postgres backend.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<PendingApprovalMigrationReport> {
    let pool = PostgresPendingApprovalStore::build_pool(pg_config)
        .context("building approvals pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the approvals schema (use
/// [`PostgresPendingApprovalStore::build_pool`]). Runs the destination
/// migrations first.
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<PendingApprovalMigrationReport> {
    let _store: Arc<PostgresPendingApprovalStore> =
        PostgresPendingApprovalStore::with_pool(pool.clone())
            .await
            .context("preparing Postgres approvals destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let approvals = read_approvals(&sqlite)?;
    let targets = read_targets(&sqlite)?;
    drop(sqlite);

    // Parents before children — `pending_approval_targets.request_id`
    // references `pending_approvals(request_id)`.
    let approval_count = write_approvals(&pool, approvals).await?;
    let target_count = write_targets(&pool, targets).await?;

    Ok(PendingApprovalMigrationReport {
        approvals: approval_count,
        targets: target_count,
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

struct ApprovalMigRow {
    request_id: Uuid,
    agent_id: String,
    session_key: String,
    workspace_id: Option<String>,
    channel_id: Option<String>,
    thread_ts: Option<String>,
    requester_id: String,
    tool_name: String,
    tool_input: String,
    reason: String,
    allow_self_approval: bool,
    status: String,
    resolved_by: Option<String>,
    resolved_at: Option<DateTime<Utc>>,
    channel_notice: Option<String>,
    channel_notice_ts: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

fn read_approvals(sqlite: &Connection) -> Result<Vec<ApprovalMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
                requester_id, tool_name, tool_input, reason, allow_self_approval, status,
                resolved_by, resolved_at, channel_notice, channel_notice_ts,
                created_at, expires_at
         FROM pending_approvals",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let allow_self_approval: i64 = r.get(10)?;
            let resolved_at: Option<String> = r.get(13)?;
            let created_at_str: String = r.get(16)?;
            let expires_at_str: String = r.get(17)?;
            Ok(ApprovalMigRow {
                request_id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                agent_id: r.get(1)?,
                session_key: r.get(2)?,
                workspace_id: r.get(3)?,
                channel_id: r.get(4)?,
                thread_ts: r.get(5)?,
                requester_id: r.get(6)?,
                tool_name: r.get(7)?,
                tool_input: r.get(8)?,
                reason: r.get(9)?,
                allow_self_approval: allow_self_approval != 0,
                status: r.get(11)?,
                resolved_by: r.get(12)?,
                resolved_at: resolved_at.as_deref().map(parse_ts),
                channel_notice: r.get(14)?,
                channel_notice_ts: r.get(15)?,
                created_at: parse_ts(&created_at_str),
                expires_at: parse_ts(&expires_at_str),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_approvals(pool: &PostgresPool, rows: Vec<ApprovalMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO pending_approvals (
                request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
                requester_id, tool_name, tool_input, reason, allow_self_approval, status,
                resolved_by, resolved_at, channel_notice, channel_notice_ts,
                created_at, expires_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                       $13, $14, $15, $16, $17, $18)
             ON CONFLICT (request_id) DO NOTHING",
            &[
                &r.request_id,
                &r.agent_id,
                &r.session_key,
                &r.workspace_id,
                &r.channel_id,
                &r.thread_ts,
                &r.requester_id,
                &r.tool_name,
                &r.tool_input,
                &r.reason,
                &r.allow_self_approval,
                &r.status,
                &r.resolved_by,
                &r.resolved_at,
                &r.channel_notice,
                &r.channel_notice_ts,
                &r.created_at,
                &r.expires_at,
            ],
        )
        .await
        .context("insert pending_approvals row")?;
    }
    Ok(rows.len())
}

struct TargetMigRow {
    request_id: Uuid,
    approver_id: String,
    slack_user_id: String,
    dm_channel: Option<String>,
    message_ts: Option<String>,
}

fn read_targets(sqlite: &Connection) -> Result<Vec<TargetMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT request_id, approver_id, slack_user_id, dm_channel, message_ts
         FROM pending_approval_targets",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            Ok(TargetMigRow {
                request_id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                approver_id: r.get(1)?,
                slack_user_id: r.get(2)?,
                dm_channel: r.get(3)?,
                message_ts: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_targets(pool: &PostgresPool, rows: Vec<TargetMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO pending_approval_targets
             (request_id, approver_id, slack_user_id, dm_channel, message_ts)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (request_id, approver_id) DO NOTHING",
            &[
                &r.request_id,
                &r.approver_id,
                &r.slack_user_id,
                &r.dm_channel,
                &r.message_ts,
            ],
        )
        .await
        .context("insert pending_approval_targets row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::store::sqlite::SqlitePendingApprovalStore;
    use crate::approvals::store::{ApprovalOutcome, PendingApprovalStore};
    use workforce_agent::runner::PendingApprovalCore;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    fn core(request_id: &str) -> PendingApprovalCore {
        PendingApprovalCore {
            request_id: request_id.to_string(),
            agent_id: "atlas".to_string(),
            session_key: "slack:ws:C1:T1".to_string(),
            channel_id: "C1".to_string(),
            requester_user_id: "alice".to_string(),
            tool_name: "vault.delete".to_string(),
            tool_input: serde_json::json!({"id": 7}),
            reason: "sensitive".to_string(),
            allow_self_approval: false,
            approver_user_ids: vec!["bob".to_string()],
            ttl: std::time::Duration::from_secs(3600),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_pending_approvals_round_trip() {
        with_test_pool(
            PostgresPendingApprovalStore::build_pool,
            |pool| async move {
                let pending_id = Uuid::new_v4().to_string();
                let resolved_id = Uuid::new_v4().to_string();
                let expires = now_micros() + chrono::Duration::hours(1);

                let tmp = tempfile::NamedTempFile::new().unwrap();
                let sqlite = SqlitePendingApprovalStore::open(tmp.path()).unwrap();

                // A still-pending row: resolved_at must stay NULL.
                sqlite
                    .insert_core(&core(&pending_id), Some("ws"), Some("T1"), expires)
                    .unwrap();
                sqlite.add_target(&pending_id, "bob", "U_BOB").unwrap();
                sqlite
                    .set_target_message(&pending_id, "bob", "D_BOB", "111.1")
                    .unwrap();
                sqlite
                    .set_channel_notice(&pending_id, "C1", "999.9")
                    .unwrap();

                // A resolved row: resolved_at must be preserved non-null.
                sqlite
                    .insert_core(&core(&resolved_id), None, None, expires)
                    .unwrap();
                sqlite
                    .claim(&resolved_id, ApprovalOutcome::Approved, "bob")
                    .unwrap();
                drop(sqlite);

                let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                    .await
                    .expect("migration succeeds");
                assert_eq!(report.approvals, 2);
                assert_eq!(report.targets, 1);

                let dest = PostgresPendingApprovalStore::with_pool(pool.clone())
                    .await
                    .unwrap();
                let store: Arc<dyn PendingApprovalStore> = dest;

                let pending = store.get(&pending_id).unwrap().expect("pending present");
                assert_eq!(pending.tool_name, "vault.delete");
                assert_eq!(pending.tool_input, serde_json::json!({"id": 7}));
                assert_eq!(pending.requester_id, "alice");
                assert_eq!(pending.workspace_id.as_deref(), Some("ws"));
                assert_eq!(pending.thread_ts.as_deref(), Some("T1"));
                assert!(!pending.allow_self_approval);
                assert!(pending.is_pending());
                assert_eq!(pending.channel_notice.as_deref(), Some("C1"));
                assert_eq!(pending.channel_notice_ts.as_deref(), Some("999.9"));

                let targets = store.targets(&pending_id).unwrap();
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].slack_user_id, "U_BOB");
                assert_eq!(targets[0].dm_channel.as_deref(), Some("D_BOB"));
                assert_eq!(targets[0].message_ts.as_deref(), Some("111.1"));

                let resolved = store.get(&resolved_id).unwrap().expect("resolved present");
                assert_eq!(resolved.status, "approved");
                assert_eq!(resolved.resolved_by.as_deref(), Some("bob"));

                // The store cannot surface resolved_at — assert the nullable
                // timestamp migrated correctly with a raw read.
                let conn = pool.get().await.unwrap();
                let pending_uuid = Uuid::parse_str(&pending_id).unwrap();
                let resolved_uuid = Uuid::parse_str(&resolved_id).unwrap();
                let pending_resolved_at: Option<DateTime<Utc>> = conn
                    .query_one(
                        "SELECT resolved_at FROM pending_approvals WHERE request_id = $1",
                        &[&pending_uuid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert!(
                    pending_resolved_at.is_none(),
                    "pending row keeps NULL resolved_at"
                );
                let resolved_resolved_at: Option<DateTime<Utc>> = conn
                    .query_one(
                        "SELECT resolved_at FROM pending_approvals WHERE request_id = $1",
                        &[&resolved_uuid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert!(
                    resolved_resolved_at.is_some(),
                    "resolved row keeps non-null resolved_at"
                );
                drop(conn);

                // Idempotency: re-running is a no-op, not a duplicate-key error.
                let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                    .await
                    .expect("idempotent");
                assert_eq!(report2.approvals, 2);
                assert_eq!(report2.targets, 1);
            },
        )
        .await;
    }
}

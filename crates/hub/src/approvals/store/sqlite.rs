//! SQLite backend for [`PendingApprovalStore`].
//!
//! Mirrors `workforce_permissions::sqlite::SqlitePermissionStore`:
//! `Mutex<Connection>`, `init_schema` on open, lossy row parsing.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use rusqlite::{params, Connection, Row};
use tracing::info;

use super::{ApprovalOutcome, PendingApproval, PendingApprovalStore, Target, PRUNABLE};
use workforce_agent::runner::PendingApprovalCore;
use workforce_common::telemetry::{prune_in_batches, PRUNE_BATCH};

pub struct SqlitePendingApprovalStore {
    conn: Mutex<Connection>,
}

impl SqlitePendingApprovalStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("Failed to open approvals DB: {:?}", path.as_ref()))?;
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Pending-approval store opened");
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
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .wrap_err("Failed to enable foreign keys")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS pending_approvals (
                request_id          TEXT PRIMARY KEY,
                agent_id            TEXT NOT NULL,
                session_key         TEXT NOT NULL,
                workspace_id        TEXT,
                channel_id          TEXT,
                thread_ts           TEXT,
                requester_id        TEXT NOT NULL,
                tool_name           TEXT NOT NULL,
                tool_input          TEXT NOT NULL,
                reason              TEXT NOT NULL,
                allow_self_approval INTEGER NOT NULL,
                status              TEXT NOT NULL,
                resolved_by         TEXT,
                resolved_at         TEXT,
                channel_notice      TEXT,
                channel_notice_ts   TEXT,
                created_at          TEXT NOT NULL,
                expires_at          TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_pending_approvals_due
                ON pending_approvals(status, expires_at);

            CREATE TABLE IF NOT EXISTS pending_approval_targets (
                request_id      TEXT NOT NULL,
                approver_id     TEXT NOT NULL,
                slack_user_id   TEXT NOT NULL,
                dm_channel      TEXT,
                message_ts      TEXT,
                PRIMARY KEY (request_id, approver_id),
                FOREIGN KEY (request_id) REFERENCES pending_approvals(request_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_pending_approval_targets_slack
                ON pending_approval_targets(request_id, slack_user_id);
            CREATE INDEX IF NOT EXISTS idx_pending_approval_targets_approver
                ON pending_approval_targets(approver_id);
            ",
        )
        .wrap_err("Failed to initialise pending-approval schema")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock approvals DB: {}", e))
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_approval(row: &Row<'_>) -> rusqlite::Result<PendingApproval> {
    let tool_input: String = row.get("tool_input")?;
    let allow_self_approval: i64 = row.get("allow_self_approval")?;
    let created_at: String = row.get("created_at")?;
    let expires_at: String = row.get("expires_at")?;
    Ok(PendingApproval {
        request_id: row.get("request_id")?,
        agent_id: row.get("agent_id")?,
        session_key: row.get("session_key")?,
        workspace_id: row.get("workspace_id")?,
        channel_id: row.get("channel_id")?,
        thread_ts: row.get("thread_ts")?,
        requester_id: row.get("requester_id")?,
        tool_name: row.get("tool_name")?,
        tool_input: serde_json::from_str(&tool_input).unwrap_or(serde_json::Value::Null),
        reason: row.get("reason")?,
        allow_self_approval: allow_self_approval != 0,
        status: row.get("status")?,
        resolved_by: row.get("resolved_by")?,
        channel_notice: row.get("channel_notice")?,
        channel_notice_ts: row.get("channel_notice_ts")?,
        created_at: parse_ts(&created_at),
        expires_at: parse_ts(&expires_at),
    })
}

fn parse_target(row: &Row<'_>) -> rusqlite::Result<Target> {
    Ok(Target {
        request_id: row.get("request_id")?,
        approver_id: row.get("approver_id")?,
        slack_user_id: row.get("slack_user_id")?,
        dm_channel: row.get("dm_channel")?,
        message_ts: row.get("message_ts")?,
    })
}

const SELECT_COLS: &str = "request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
     requester_id, tool_name, tool_input, reason, allow_self_approval, status,
     resolved_by, channel_notice, channel_notice_ts, created_at, expires_at";

impl PendingApprovalStore for SqlitePendingApprovalStore {
    fn insert_core(
        &self,
        core: &PendingApprovalCore,
        workspace_id: Option<&str>,
        thread_ts: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock()?;
        let tool_input = serde_json::to_string(&core.tool_input)?;
        conn.execute(
            "INSERT INTO pending_approvals (
                request_id, agent_id, session_key, workspace_id, channel_id, thread_ts,
                requester_id, tool_name, tool_input, reason, allow_self_approval, status,
                created_at, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13)
             ON CONFLICT (request_id) DO NOTHING",
            params![
                core.request_id,
                core.agent_id,
                core.session_key,
                workspace_id,
                core.channel_id,
                thread_ts,
                core.requester_user_id,
                core.tool_name,
                tool_input,
                core.reason,
                core.allow_self_approval as i64,
                Utc::now().to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        )
        .wrap_err("insert_core failed")?;
        Ok(())
    }

    fn add_target(&self, request_id: &str, approver_id: &str, slack_user_id: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO pending_approval_targets (request_id, approver_id, slack_user_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (request_id, approver_id) DO UPDATE SET slack_user_id = excluded.slack_user_id",
            params![request_id, approver_id, slack_user_id],
        )
        .wrap_err("add_target failed")?;
        Ok(())
    }

    fn set_target_message(
        &self,
        request_id: &str,
        approver_id: &str,
        dm_channel: &str,
        message_ts: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE pending_approval_targets SET dm_channel = ?3, message_ts = ?4
             WHERE request_id = ?1 AND approver_id = ?2",
            params![request_id, approver_id, dm_channel, message_ts],
        )
        .wrap_err("set_target_message failed")?;
        Ok(())
    }

    fn set_channel_notice(&self, request_id: &str, channel: &str, ts: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE pending_approvals SET channel_notice = ?2, channel_notice_ts = ?3
             WHERE request_id = ?1",
            params![request_id, channel, ts],
        )
        .wrap_err("set_channel_notice failed")?;
        Ok(())
    }

    fn get(&self, request_id: &str) -> Result<Option<PendingApproval>> {
        let conn = self.lock()?;
        let sql = format!("SELECT {SELECT_COLS} FROM pending_approvals WHERE request_id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![request_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(parse_approval(row)?)),
            None => Ok(None),
        }
    }

    fn targets(&self, request_id: &str) -> Result<Vec<Target>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT request_id, approver_id, slack_user_id, dm_channel, message_ts
             FROM pending_approval_targets WHERE request_id = ?1 ORDER BY approver_id",
        )?;
        let rows = stmt.query_map(params![request_id], parse_target)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("targets failed")
    }

    fn claim(&self, request_id: &str, outcome: ApprovalOutcome, resolver_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE pending_approvals
                 SET status = ?2, resolved_by = ?3, resolved_at = ?4
                 WHERE request_id = ?1 AND status = 'pending'
                   AND (?2 = 'expired' OR expires_at > ?4)",
                params![
                    request_id,
                    outcome.as_str(),
                    resolver_id,
                    Utc::now().to_rfc3339(),
                ],
            )
            .wrap_err("claim failed")?;
        Ok(n == 1)
    }

    fn due_for_expiry(&self, now: DateTime<Utc>) -> Result<Vec<PendingApproval>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {SELECT_COLS} FROM pending_approvals
             WHERE status = 'pending' AND expires_at <= ?1
             ORDER BY expires_at"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now.to_rfc3339()], parse_approval)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("due_for_expiry failed")
    }

    fn recently_resolved(&self, limit: usize) -> Result<Vec<PendingApproval>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {SELECT_COLS} FROM pending_approvals
             WHERE status != 'pending'
             ORDER BY resolved_at DESC LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], parse_approval)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("recently_resolved failed")
    }

    fn pending_for(&self, approver: &str, now: DateTime<Utc>) -> Result<Vec<PendingApproval>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLS} FROM pending_approvals
             WHERE status = 'pending' AND expires_at > ?2
               AND request_id IN (SELECT request_id FROM pending_approval_targets
                                  WHERE approver_id = ?1)
             ORDER BY created_at"
        ))?;
        let rows = stmt.query_map(params![approver, now.to_rfc3339()], parse_approval)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("pending_for failed")
    }

    fn requested_by(&self, requester: &str) -> Result<Vec<PendingApproval>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLS} FROM pending_approvals
             WHERE requester_id = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![requester], parse_approval)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("requested_by failed")
    }

    fn approver_targets(&self, approver: &str) -> Result<usize> {
        let n: i64 = self.lock()?.query_row(
            "SELECT COUNT(*) FROM pending_approval_targets WHERE approver_id = ?1",
            params![approver],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or_default())
    }

    fn reassign_requester(&self, from: &str, to: &str) -> Result<usize> {
        Ok(self.lock()?.execute(
            "UPDATE pending_approvals SET requester_id = ?2
             WHERE requester_id = ?1 AND status = 'pending'",
            params![from, to],
        )?)
    }

    fn forget_requester(&self, requester: &str) -> Result<usize> {
        Ok(self.lock()?.execute(
            "DELETE FROM pending_approvals WHERE requester_id = ?1",
            params![requester],
        )?)
    }

    fn drop_approver(&self, approver: &str) -> Result<usize> {
        Ok(self.lock()?.execute(
            "DELETE FROM pending_approval_targets WHERE approver_id = ?1",
            params![approver],
        )?)
    }

    fn prune_resolved(&self, before: DateTime<Utc>) -> Result<usize> {
        let delete = format!(
            "DELETE FROM pending_approvals WHERE request_id IN (
                 SELECT request_id FROM pending_approvals WHERE {PRUNABLE}?1 LIMIT ?2)"
        );
        let before = before.to_rfc3339();
        prune_in_batches(|| {
            Ok(self
                .lock()?
                .execute(&delete, params![before, PRUNE_BATCH as i64])?)
        })
    }

    fn identity_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(super::HOLDER_ROWS)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(super::sum_holders(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_waits_on_an_approver_is_their_pending_unexpired_requests() {
        crate::approvals::store::pending_for_contract(
            &SqlitePendingApprovalStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn resolved_requests_are_pruned_past_their_window() {
        crate::approvals::store::prune_contract(&SqlitePendingApprovalStore::in_memory().unwrap());
    }

    #[test]
    fn identity_operations_reach_requests_and_approver_targets() {
        crate::approvals::store::identity_operations_contract(
            &SqlitePendingApprovalStore::in_memory().unwrap(),
        );
    }

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

    #[test]
    fn round_trip_core_targets_and_notice() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let expires = Utc::now() + chrono::Duration::hours(1);
        store
            .insert_core(&core("r1"), Some("ws"), Some("T1"), expires)
            .unwrap();
        store.add_target("r1", "bob", "U_BOB").unwrap();
        store
            .set_target_message("r1", "bob", "D_BOB", "111.1")
            .unwrap();
        store.set_channel_notice("r1", "C1", "999.9").unwrap();

        let row = store.get("r1").unwrap().expect("present");
        assert_eq!(row.tool_name, "vault.delete");
        assert_eq!(row.tool_input, serde_json::json!({"id": 7}));
        assert_eq!(row.requester_id, "alice");
        assert_eq!(row.workspace_id.as_deref(), Some("ws"));
        assert_eq!(row.thread_ts.as_deref(), Some("T1"));
        assert!(!row.allow_self_approval);
        assert!(row.is_pending());
        assert_eq!(row.channel_notice.as_deref(), Some("C1"));
        assert_eq!(row.channel_notice_ts.as_deref(), Some("999.9"));

        let targets = store.targets("r1").unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].slack_user_id, "U_BOB");
        assert_eq!(targets[0].dm_channel.as_deref(), Some("D_BOB"));
        assert_eq!(targets[0].message_ts.as_deref(), Some("111.1"));
    }

    #[test]
    fn insert_core_is_idempotent() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let expires = Utc::now() + chrono::Duration::hours(1);
        store.insert_core(&core("r1"), None, None, expires).unwrap();
        // A second deferral with a different reason must not overwrite.
        let mut c2 = core("r1");
        c2.reason = "changed".to_string();
        store.insert_core(&c2, None, None, expires).unwrap();
        assert_eq!(store.get("r1").unwrap().unwrap().reason, "sensitive");
    }

    #[test]
    fn claim_is_single_winner() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let expires = Utc::now() + chrono::Duration::hours(1);
        store.insert_core(&core("r1"), None, None, expires).unwrap();

        assert!(store.claim("r1", ApprovalOutcome::Approved, "bob").unwrap());
        // Second claim loses — row is no longer pending.
        assert!(!store.claim("r1", ApprovalOutcome::Denied, "carol").unwrap());

        let row = store.get("r1").unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert_eq!(row.resolved_by.as_deref(), Some("bob"));
    }

    #[test]
    fn target_acl_lookup_finds_eligible_slack_id() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let expires = Utc::now() + chrono::Duration::hours(1);
        store.insert_core(&core("r1"), None, None, expires).unwrap();
        store.add_target("r1", "bob", "U_BOB").unwrap();
        store.add_target("r1", "carol", "U_CAROL").unwrap();

        let targets = store.targets("r1").unwrap();
        let slack_ids: Vec<&str> = targets.iter().map(|t| t.slack_user_id.as_str()).collect();
        assert!(slack_ids.contains(&"U_BOB"));
        assert!(slack_ids.contains(&"U_CAROL"));
        assert!(!slack_ids.contains(&"U_MALLORY"));
    }

    #[test]
    fn due_for_expiry_selects_only_lapsed_pending() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let past = Utc::now() - chrono::Duration::minutes(5);
        let future = Utc::now() + chrono::Duration::hours(1);
        store
            .insert_core(&core("expired"), None, None, past)
            .unwrap();
        store
            .insert_core(&core("live"), None, None, future)
            .unwrap();
        let mut resolved = core("resolved");
        resolved.request_id = "resolved".to_string();
        store.insert_core(&resolved, None, None, past).unwrap();
        store
            .claim("resolved", ApprovalOutcome::Expired, "system")
            .unwrap();

        let due = store.due_for_expiry(Utc::now()).unwrap();
        let ids: Vec<&str> = due.iter().map(|r| r.request_id.as_str()).collect();
        assert_eq!(ids, vec!["expired"], "only lapsed + still-pending rows");
    }

    #[test]
    fn cascade_delete_removes_targets() {
        let store = SqlitePendingApprovalStore::in_memory().unwrap();
        let expires = Utc::now() + chrono::Duration::hours(1);
        store.insert_core(&core("r1"), None, None, expires).unwrap();
        store.add_target("r1", "bob", "U_BOB").unwrap();
        let conn = store.lock().unwrap();
        conn.execute("DELETE FROM pending_approvals WHERE request_id = 'r1'", [])
            .unwrap();
        drop(conn);
        assert!(store.targets("r1").unwrap().is_empty());
    }
}

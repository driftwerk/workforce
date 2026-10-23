use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;
use uuid::Uuid;

use std::sync::Arc;

use crate::lifecycle::{LifecycleOp, LifecycleOpStore, OpStatus};
use crate::remote_mcp_token::{RemoteMcpToken, RemoteMcpTokenStore};
use crate::store::{IdentityAndTeamStore, IdentityFilter, IdentityStore};
use crate::team::{Team, TeamStore};
use crate::types::{Identity, IdentityRole, IdentityStatus, ProvisioningSource};

pub struct SqliteIdentityStore {
    conn: Mutex<Connection>,
}

impl SqliteIdentityStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("Failed to open identity database: {:?}", path.as_ref()))?;
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Identity store opened");
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
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS identities (
                id              TEXT PRIMARY KEY,
                display_name    TEXT NOT NULL,
                email           TEXT UNIQUE,
                password_hash   TEXT,
                role            TEXT NOT NULL DEFAULT 'member',
                status          TEXT NOT NULL DEFAULT 'active',
                provisioning_source TEXT NOT NULL DEFAULT 'manual',
                linked_ids      TEXT NOT NULL DEFAULT '{}',
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_email
                ON identities(email) WHERE email IS NOT NULL;

            CREATE TABLE IF NOT EXISTS teams (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                creator_id      TEXT NOT NULL,
                members         TEXT NOT NULL DEFAULT '[]',
                join_requests   TEXT NOT NULL DEFAULT '[]',
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS remote_mcp_tokens (
                id              TEXT PRIMARY KEY,
                identity_id     TEXT NOT NULL,
                label           TEXT NOT NULL,
                token_hash      TEXT NOT NULL UNIQUE,
                token_prefix    TEXT NOT NULL,
                scopes          TEXT NOT NULL DEFAULT '[]',
                created_at      TEXT NOT NULL,
                expires_at      TEXT NOT NULL,
                last_used_at    TEXT,
                revoked_at      TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_remote_mcp_tokens_identity
                ON remote_mcp_tokens(identity_id);

            CREATE TABLE IF NOT EXISTS identity_lifecycle_ops (
                id              TEXT PRIMARY KEY,
                subject         TEXT NOT NULL,
                kind            TEXT NOT NULL,
                target          TEXT,
                actor           TEXT NOT NULL,
                names           TEXT NOT NULL DEFAULT '[]',
                status          TEXT NOT NULL,
                report          TEXT NOT NULL DEFAULT '{}',
                started_at      TEXT NOT NULL,
                lease_until     TEXT NOT NULL,
                finished_at     TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_identity_lifecycle_ops_subject
                ON identity_lifecycle_ops(subject, started_at DESC);
            ",
        )
        .wrap_err("Failed to initialize identity schema")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock identity database: {}", e))
    }

    /// Build the shared `WHERE` clause and its bound parameters for `list`
    /// and `count_filtered`. `search` matches `display_name` or `email` via a
    /// single `%term%` parameter reused for both columns.
    fn filter_clause(filter: &IdentityFilter) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
        let mut conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(role) = &filter.role {
            conditions.push(format!("role = ?{}", params.len() + 1));
            params.push(Box::new(role.as_str().to_string()));
        }
        if let Some(status) = &filter.status {
            conditions.push(format!("status = ?{}", params.len() + 1));
            params.push(Box::new(status.as_str().to_string()));
        }
        if let Some(search) = &filter.search {
            let idx = params.len() + 1;
            conditions.push(format!("(display_name LIKE ?{idx} OR email LIKE ?{idx})"));
            params.push(Box::new(format!("%{search}%")));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        (where_clause, params)
    }

    fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Identity> {
        let id_str: String = row.get("id")?;
        let display_name: String = row.get("display_name")?;
        let email: Option<String> = row.get("email")?;
        let password_hash: Option<String> = row.get("password_hash")?;
        let role_str: String = row.get("role")?;
        let status_str: String = row.get("status")?;
        let source_str: String = row.get("provisioning_source")?;
        let linked_ids_json: String = row.get("linked_ids")?;
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let id = Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4());
        let linked_ids: HashMap<String, String> =
            serde_json::from_str(&linked_ids_json).unwrap_or_default();
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(Identity {
            id,
            display_name,
            email,
            password_hash,
            role: IdentityRole::from_str_lossy(&role_str),
            role_key: role_str,
            status: IdentityStatus::from_str_lossy(&status_str),
            provisioning_source: ProvisioningSource::from_str_lossy(&source_str),
            linked_ids,
            created_at,
            updated_at,
        })
    }
}

impl IdentityStore for SqliteIdentityStore {
    fn create(&self, identity: &Identity) -> Result<()> {
        let conn = self.lock()?;
        let linked_ids_json = serde_json::to_string(&identity.linked_ids)?;
        conn.execute(
            "INSERT INTO identities (id, display_name, email, password_hash, role, status, provisioning_source, linked_ids, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                identity.id.to_string(),
                identity.display_name,
                identity.email,
                identity.password_hash,
                identity.effective_role_key(),
                identity.status.as_str(),
                identity.provisioning_source.as_str(),
                linked_ids_json,
                identity.created_at.to_rfc3339(),
                identity.updated_at.to_rfc3339(),
            ],
        )
        .wrap_err("Failed to create identity")?;
        Ok(())
    }

    fn get_by_id(&self, id: Uuid) -> Result<Option<Identity>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT * FROM identities WHERE id = ?1")?;
        let result = stmt
            .query_row(params![id.to_string()], Self::parse_row)
            .ok();
        Ok(result)
    }

    fn get_by_email(&self, email: &str) -> Result<Option<Identity>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT * FROM identities WHERE email = ?1")?;
        let result = stmt.query_row(params![email], Self::parse_row).ok();
        Ok(result)
    }

    fn get_by_linked_id(&self, platform: &str, platform_id: &str) -> Result<Option<Identity>> {
        let conn = self.lock()?;
        let pattern = format!("%\"{}\":\"{}\"%", platform, platform_id);
        let mut stmt = conn.prepare("SELECT * FROM identities WHERE linked_ids LIKE ?1")?;
        let result = stmt.query_row(params![pattern], Self::parse_row).ok();
        Ok(result)
    }

    fn update(&self, identity: &Identity) -> Result<()> {
        let conn = self.lock()?;
        let linked_ids_json = serde_json::to_string(&identity.linked_ids)?;
        conn.execute(
            "UPDATE identities SET display_name = ?1, email = ?2, password_hash = ?3, role = ?4, status = ?5, provisioning_source = ?6, linked_ids = ?7, updated_at = ?8 WHERE id = ?9",
            params![
                identity.display_name,
                identity.email,
                identity.password_hash,
                identity.effective_role_key(),
                identity.status.as_str(),
                identity.provisioning_source.as_str(),
                linked_ids_json,
                identity.updated_at.to_rfc3339(),
                identity.id.to_string(),
            ],
        )
        .wrap_err("Failed to update identity")?;
        Ok(())
    }

    fn delete(&self, id: Uuid) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM remote_mcp_tokens WHERE identity_id = ?1",
            params![id.to_string()],
        )?;
        conn.execute(
            "DELETE FROM identities WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    fn list(&self, filter: &IdentityFilter) -> Result<Vec<Identity>> {
        let conn = self.lock()?;
        let (where_clause, param_values) = Self::filter_clause(filter);
        let limit = filter.limit.unwrap_or(100);
        let offset = filter.offset.unwrap_or(0);

        let sql = format!(
            "SELECT * FROM identities {where_clause} ORDER BY created_at DESC, id DESC LIMIT {limit} OFFSET {offset}"
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), Self::parse_row)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    fn count_filtered(&self, filter: &IdentityFilter) -> Result<usize> {
        let conn = self.lock()?;
        let (where_clause, param_values) = Self::filter_clause(filter);
        let sql = format!("SELECT COUNT(*) FROM identities {where_clause}");
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let count: usize = conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))?;
        Ok(count)
    }
}

impl TeamStore for SqliteIdentityStore {
    fn create_team(&self, team: &Team) -> Result<()> {
        let conn = self.lock()?;
        let members = serde_json::to_string(&team.members)?;
        let join_requests = serde_json::to_string(&team.join_requests)?;
        conn.execute(
            "INSERT INTO teams (id, name, creator_id, members, join_requests, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                team.id.to_string(),
                team.name,
                team.creator_id.to_string(),
                members,
                join_requests,
                team.created_at.to_rfc3339(),
                team.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn get_team(&self, id: Uuid) -> Result<Option<Team>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT * FROM teams WHERE id = ?1")?;
        let result = stmt.query_row(params![id.to_string()], parse_team_row).ok();
        Ok(result)
    }

    fn update_team(&self, team: &Team) -> Result<()> {
        let conn = self.lock()?;
        let members = serde_json::to_string(&team.members)?;
        let join_requests = serde_json::to_string(&team.join_requests)?;
        conn.execute(
            "UPDATE teams SET name = ?1, members = ?2, join_requests = ?3, updated_at = ?4 WHERE id = ?5",
            params![
                team.name,
                members,
                join_requests,
                team.updated_at.to_rfc3339(),
                team.id.to_string(),
            ],
        )?;
        Ok(())
    }

    fn delete_team(&self, id: Uuid) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM teams WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    fn list_teams(&self) -> Result<Vec<Team>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT * FROM teams ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], parse_team_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn teams_for_member(&self, identity_id: Uuid) -> Result<Vec<Team>> {
        let conn = self.lock()?;
        let pattern = format!("%\"{}\"%", identity_id);
        let mut stmt =
            conn.prepare("SELECT * FROM teams WHERE members LIKE ?1 ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map(params![pattern], parse_team_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

const REMOTE_MCP_TOKEN_COLUMNS: &str = "id, identity_id, label, token_hash, token_prefix, scopes, \
                                        created_at, expires_at, last_used_at, revoked_at";

impl RemoteMcpTokenStore for SqliteIdentityStore {
    fn create_token(&self, token: &RemoteMcpToken) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO remote_mcp_tokens (id, identity_id, label, token_hash, token_prefix, scopes, created_at, expires_at, last_used_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                token.id.to_string(),
                token.identity_id.to_string(),
                token.label,
                token.token_hash,
                token.token_prefix,
                serde_json::to_string(&token.scopes)?,
                token.created_at.to_rfc3339(),
                token.expires_at.to_rfc3339(),
                token.last_used_at.map(|t| t.to_rfc3339()),
                token.revoked_at.map(|t| t.to_rfc3339()),
            ],
        )
        .wrap_err("Failed to create remote MCP token")?;
        Ok(())
    }

    fn get_token_by_hash(&self, token_hash: &str) -> Result<Option<RemoteMcpToken>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens WHERE token_hash = ?1"
        ))?;
        Ok(stmt
            .query_row(params![token_hash], parse_remote_mcp_token_row)
            .ok())
    }

    fn list_tokens_for_identity(&self, identity_id: Uuid) -> Result<Vec<RemoteMcpToken>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens
             WHERE identity_id = ?1 AND revoked_at IS NULL
             ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map(params![identity_id.to_string()], parse_remote_mcp_token_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn list_all_tokens(&self) -> Result<Vec<RemoteMcpToken>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens
             WHERE revoked_at IS NULL
             ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map([], parse_remote_mcp_token_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn revoke_token(&self, id: Uuid, identity_id: Option<Uuid>, at: DateTime<Utc>) -> Result<bool> {
        let conn = self.lock()?;
        let changed = match identity_id {
            Some(owner) => conn.execute(
                "UPDATE remote_mcp_tokens SET revoked_at = ?1
                 WHERE id = ?2 AND identity_id = ?3 AND revoked_at IS NULL",
                params![at.to_rfc3339(), id.to_string(), owner.to_string()],
            )?,
            None => conn.execute(
                "UPDATE remote_mcp_tokens SET revoked_at = ?1
                 WHERE id = ?2 AND revoked_at IS NULL",
                params![at.to_rfc3339(), id.to_string()],
            )?,
        };
        Ok(changed > 0)
    }

    fn touch_token_last_used(&self, id: Uuid, at: DateTime<Utc>) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE remote_mcp_tokens SET last_used_at = ?1 WHERE id = ?2",
            params![at.to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    fn count_live_tokens_for_identity(
        &self,
        identity_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        Ok(self
            .list_tokens_for_identity(identity_id)?
            .iter()
            .filter(|t| t.is_live(now))
            .count())
    }
}

impl IdentityAndTeamStore for SqliteIdentityStore {
    fn as_identity_store(self: Arc<Self>) -> Arc<dyn IdentityStore> {
        self
    }

    fn as_team_store(self: Arc<Self>) -> Arc<dyn TeamStore> {
        self
    }

    fn as_remote_mcp_token_store(self: Arc<Self>) -> Arc<dyn RemoteMcpTokenStore> {
        self
    }

    fn as_lifecycle_op_store(self: Arc<Self>) -> Arc<dyn LifecycleOpStore> {
        self
    }
}

pub(crate) const LIFECYCLE_OP_COLUMNS: &str =
    "id, subject, kind, target, actor, names, status, report, started_at, lease_until, finished_at";

impl LifecycleOpStore for SqliteIdentityStore {
    fn claim_op(&self, op: &LifecycleOp, now: DateTime<Utc>) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let parties: Vec<String> = op.parties().iter().map(Uuid::to_string).collect();
        let mut running: Vec<(String, String)> = Vec::new();
        for party in &parties {
            running.extend(
                tx.prepare(
                    "SELECT id, lease_until FROM identity_lifecycle_ops
                     WHERE status = 'running' AND (subject = ?1 OR target = ?1)",
                )?
                .query_map(params![party], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        if running.iter().any(|(_, lease)| parse_rfc3339(lease) > now) {
            return Ok(false);
        }
        for (id, _) in &running {
            tx.execute(
                "UPDATE identity_lifecycle_ops SET status = 'interrupted' WHERE id = ?1",
                params![id],
            )?;
        }
        tx.execute(
            &format!(
                "INSERT INTO identity_lifecycle_ops ({LIFECYCLE_OP_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                op.id.to_string(),
                op.subject.to_string(),
                op.kind,
                op.target.map(|t| t.to_string()),
                op.actor,
                serde_json::to_string(&op.names)?,
                op.status.as_str(),
                op.report.to_string(),
                op.started_at.to_rfc3339(),
                op.lease_until.to_rfc3339(),
                op.finished_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn renew_op(
        &self,
        id: Uuid,
        lease_until: DateTime<Utc>,
        report: &serde_json::Value,
    ) -> Result<bool> {
        Ok(self.lock()?.execute(
            "UPDATE identity_lifecycle_ops SET lease_until = ?1, report = ?2
             WHERE id = ?3 AND status = 'running'",
            params![lease_until.to_rfc3339(), report.to_string(), id.to_string()],
        )? > 0)
    }

    fn finish_op(
        &self,
        id: Uuid,
        status: OpStatus,
        report: &serde_json::Value,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self.lock()?.execute(
            "UPDATE identity_lifecycle_ops
             SET status = ?1, report = ?2, finished_at = ?3, lease_until = ?3
             WHERE id = ?4 AND status = 'running'",
            params![
                status.as_str(),
                report.to_string(),
                at.to_rfc3339(),
                id.to_string()
            ],
        )? > 0)
    }

    fn get_op(&self, id: Uuid) -> Result<Option<LifecycleOp>> {
        Ok(self
            .lock()?
            .query_row(
                &format!("SELECT {LIFECYCLE_OP_COLUMNS} FROM identity_lifecycle_ops WHERE id = ?1"),
                params![id.to_string()],
                parse_lifecycle_op_row,
            )
            .optional()?)
    }

    fn list_ops(&self, subject: Option<Uuid>, limit: usize) -> Result<Vec<LifecycleOp>> {
        let conn = self.lock()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = conn.prepare(&format!(
            "SELECT {LIFECYCLE_OP_COLUMNS} FROM identity_lifecycle_ops
             WHERE ?1 IS NULL OR subject = ?1
             ORDER BY started_at DESC, rowid DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(
                params![subject.map(|s| s.to_string()), limit],
                parse_lifecycle_op_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

pub(crate) fn parse_lifecycle_op_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LifecycleOp> {
    let uuid = |s: String| Uuid::parse_str(&s).unwrap_or_default();
    let names: String = row.get("names")?;
    let report: String = row.get("report")?;
    let status: String = row.get("status")?;
    let target: Option<String> = row.get("target")?;
    let finished_at: Option<String> = row.get("finished_at")?;
    Ok(LifecycleOp {
        id: uuid(row.get("id")?),
        subject: uuid(row.get("subject")?),
        kind: row.get("kind")?,
        target: target.map(uuid),
        actor: row.get("actor")?,
        names: serde_json::from_str(&names).unwrap_or_default(),
        status: OpStatus::from_str_lossy(&status),
        report: serde_json::from_str(&report).unwrap_or_default(),
        started_at: parse_rfc3339(&row.get::<_, String>("started_at")?),
        lease_until: parse_rfc3339(&row.get::<_, String>("lease_until")?),
        finished_at: finished_at.as_deref().map(parse_rfc3339),
    })
}

fn parse_rfc3339(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

pub(crate) fn parse_remote_mcp_token_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RemoteMcpToken> {
    let id: String = row.get("id")?;
    let identity_id: String = row.get("identity_id")?;
    let scopes: String = row.get("scopes")?;
    let created_at: String = row.get("created_at")?;
    let expires_at: String = row.get("expires_at")?;
    let last_used_at: Option<String> = row.get("last_used_at")?;
    let revoked_at: Option<String> = row.get("revoked_at")?;
    Ok(RemoteMcpToken {
        id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
        identity_id: Uuid::parse_str(&identity_id).unwrap_or_else(|_| Uuid::new_v4()),
        label: row.get("label")?,
        token_hash: row.get("token_hash")?,
        token_prefix: row.get("token_prefix")?,
        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        created_at: parse_rfc3339(&created_at),
        expires_at: parse_rfc3339(&expires_at),
        last_used_at: last_used_at.as_deref().map(parse_rfc3339),
        revoked_at: revoked_at.as_deref().map(parse_rfc3339),
    })
}

fn parse_team_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Team> {
    let id_str: String = row.get("id")?;
    let name: String = row.get("name")?;
    let creator_str: String = row.get("creator_id")?;
    let members_json: String = row.get("members")?;
    let join_requests_json: String = row.get("join_requests")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(Team {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
        name,
        creator_id: Uuid::parse_str(&creator_str).unwrap_or_else(|_| Uuid::new_v4()),
        members: serde_json::from_str(&members_json).unwrap_or_default(),
        join_requests: serde_json::from_str(&join_requests_json).unwrap_or_default(),
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> SqliteIdentityStore {
        SqliteIdentityStore::in_memory().expect("in-memory store")
    }

    #[test]
    fn lifecycle_claims_are_exclusive_per_subject_until_the_lease_lapses() {
        crate::lifecycle::contract::claims_are_exclusive_per_subject_until_the_lease_lapses(
            &make_store(),
        );
    }

    #[test]
    fn lifecycle_renewal_extends_the_lease_and_records_progress() {
        crate::lifecycle::contract::renewal_extends_the_lease_and_records_progress(&make_store());
    }

    #[test]
    fn lifecycle_a_taken_over_operation_is_fenced() {
        crate::lifecycle::contract::a_taken_over_operation_is_fenced(&make_store());
    }

    #[test]
    fn custom_role_key_round_trips_and_lossy_role_collapses() {
        let store = make_store();
        let mut identity = Identity::new("Rey", IdentityRole::Member);
        identity.set_role_key("reviewer");
        store.create(&identity).unwrap();

        let got = store.get_by_id(identity.id).unwrap().expect("present");
        // The raw key survives; the lossy enum collapses to Member for the gates.
        assert_eq!(got.role_key, "reviewer");
        assert_eq!(got.role, IdentityRole::Member);
        assert_eq!(got.effective_role_key(), "reviewer");
    }

    #[test]
    fn create_and_get() {
        let store = make_store();
        let identity = Identity::new("Alice", IdentityRole::Member).with_email("alice@example.com");

        store.create(&identity).unwrap();

        let retrieved = store.get_by_id(identity.id).unwrap().expect("not found");
        assert_eq!(retrieved.display_name, "Alice");
        assert_eq!(retrieved.email.as_deref(), Some("alice@example.com"));
        assert_eq!(retrieved.role, IdentityRole::Member);
        assert_eq!(retrieved.status, IdentityStatus::Active);
    }

    #[test]
    fn get_by_email() {
        let store = make_store();
        let identity =
            Identity::new("Bob", IdentityRole::SystemAdmin).with_email("bob@example.com");
        store.create(&identity).unwrap();

        let found = store
            .get_by_email("bob@example.com")
            .unwrap()
            .expect("not found");
        assert_eq!(found.id, identity.id);
        assert_eq!(found.role, IdentityRole::SystemAdmin);
    }

    #[test]
    fn get_by_linked_slack_id() {
        let store = make_store();
        let identity =
            Identity::new("Charlie", IdentityRole::Member).with_linked_id("slack", "U0ABCDEF");
        store.create(&identity).unwrap();

        let found = store
            .get_by_linked_id("slack", "U0ABCDEF")
            .unwrap()
            .expect("not found");
        assert_eq!(found.id, identity.id);
        assert_eq!(found.linked_ids.get("slack").unwrap(), "U0ABCDEF");
    }

    #[test]
    fn get_by_linked_id_not_found() {
        let store = make_store();
        let result = store.get_by_linked_id("slack", "NONEXISTENT").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn update_identity() {
        let store = make_store();
        let mut identity = Identity::new("Dana", IdentityRole::Member);
        store.create(&identity).unwrap();

        identity.display_name = "Dana Updated".to_string();
        identity.set_role(IdentityRole::Manager);
        identity.link_id("slack", "U999");
        store.update(&identity).unwrap();

        let retrieved = store.get_by_id(identity.id).unwrap().expect("not found");
        assert_eq!(retrieved.display_name, "Dana Updated");
        assert_eq!(retrieved.role, IdentityRole::Manager);
        assert_eq!(retrieved.linked_ids.get("slack").unwrap(), "U999");
    }

    #[test]
    fn delete_identity() {
        let store = make_store();
        let identity = Identity::new("Eve", IdentityRole::Member);
        store.create(&identity).unwrap();

        store.delete(identity.id).unwrap();
        assert!(store.get_by_id(identity.id).unwrap().is_none());
    }

    #[test]
    fn list_with_filters() {
        let store = make_store();
        store
            .create(&Identity::new("Admin", IdentityRole::SystemAdmin))
            .unwrap();
        store
            .create(&Identity::new("Manager", IdentityRole::Manager))
            .unwrap();
        store
            .create(&Identity::new("Member1", IdentityRole::Member))
            .unwrap();
        store
            .create(&Identity::new("Member2", IdentityRole::Member))
            .unwrap();

        let all = store.list(&IdentityFilter::default()).unwrap();
        assert_eq!(all.len(), 4);

        let members = store
            .list(&IdentityFilter {
                role: Some(IdentityRole::Member),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(members.len(), 2);

        let admins = store
            .list(&IdentityFilter {
                role: Some(IdentityRole::SystemAdmin),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(admins.len(), 1);
        assert_eq!(admins[0].display_name, "Admin");
    }

    #[test]
    fn count() {
        let store = make_store();
        assert_eq!(store.count().unwrap(), 0);

        store
            .create(&Identity::new("A", IdentityRole::Member))
            .unwrap();
        store
            .create(&Identity::new("B", IdentityRole::Member))
            .unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    fn seed(store: &SqliteIdentityStore, name: &str, email: &str, secs: i64) {
        let mut identity = Identity::new(name, IdentityRole::Member).with_email(email);
        identity.created_at = chrono::DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap();
        store.create(&identity).unwrap();
    }

    fn names(identities: &[Identity]) -> Vec<&str> {
        identities.iter().map(|i| i.display_name.as_str()).collect()
    }

    #[test]
    fn list_paginates_newest_first_with_stable_order() {
        let store = make_store();
        for i in 0..5 {
            seed(
                &store,
                &format!("User{i}"),
                &format!("user{i}@example.com"),
                i,
            );
        }

        let page = |offset| {
            store
                .list(&IdentityFilter {
                    limit: Some(2),
                    offset: Some(offset),
                    ..Default::default()
                })
                .unwrap()
        };

        assert_eq!(names(&page(0)), ["User4", "User3"]);
        assert_eq!(names(&page(2)), ["User2", "User1"]);
        assert_eq!(names(&page(4)), ["User0"]);
    }

    #[test]
    fn search_matches_name_or_email_case_insensitively() {
        let store = make_store();
        seed(&store, "Mat", "matthew.jaggard@example.com", 3);
        seed(&store, "Sam Carter", "sam@example.com", 2);
        seed(&store, "Chris", "chris@example.com", 1);

        let search = |term: &str| {
            store
                .list(&IdentityFilter {
                    search: Some(term.to_string()),
                    ..Default::default()
                })
                .unwrap()
        };

        assert_eq!(names(&search("cart")), ["Sam Carter"]);
        assert_eq!(names(&search("JAGGARD")), ["Mat"]);
        assert!(search("absent").is_empty());
    }

    #[test]
    fn count_filtered_counts_matches_ignoring_pagination() {
        let store = make_store();
        for i in 0..5 {
            seed(
                &store,
                &format!("Mat{i}"),
                &format!("mat{i}@example.com"),
                i,
            );
        }
        seed(&store, "Other", "other@example.com", 9);

        let filter = IdentityFilter {
            search: Some("mat".to_string()),
            limit: Some(2),
            offset: Some(0),
            ..Default::default()
        };
        assert_eq!(store.list(&filter).unwrap().len(), 2);
        assert_eq!(store.count_filtered(&filter).unwrap(), 5);
        assert_eq!(store.count().unwrap(), 6);
    }

    #[test]
    fn list_breaks_equal_timestamps_by_id_deterministically() {
        let store = make_store();
        let ts = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for (suffix, name) in [("0001", "A"), ("0002", "B")] {
            let mut identity = Identity::new(name, IdentityRole::Member);
            identity.id = Uuid::parse_str(&format!("00000000-0000-0000-0000-00000000{suffix}"))
                .expect("uuid");
            identity.created_at = ts;
            store.create(&identity).unwrap();
        }

        let first = store.list(&IdentityFilter::default()).unwrap();
        assert_eq!(names(&first), ["B", "A"]);
        assert_eq!(
            names(&store.list(&IdentityFilter::default()).unwrap()),
            ["B", "A"]
        );

        let page = |offset| {
            store
                .list(&IdentityFilter {
                    limit: Some(1),
                    offset: Some(offset),
                    ..Default::default()
                })
                .unwrap()
        };
        assert_eq!(names(&page(0)), ["B"]);
        assert_eq!(names(&page(1)), ["A"]);
    }

    #[test]
    fn auto_provisioned_status() {
        let store = make_store();
        let identity = Identity::new("SlackUser", IdentityRole::Member)
            .with_status(IdentityStatus::AutoProvisioned)
            .with_provisioning_source(ProvisioningSource::SlackAuto)
            .with_linked_id("slack", "U0XYZ");
        store.create(&identity).unwrap();

        let retrieved = store.get_by_id(identity.id).unwrap().expect("not found");
        assert_eq!(retrieved.status, IdentityStatus::AutoProvisioned);
        assert_eq!(retrieved.provisioning_source, ProvisioningSource::SlackAuto);
    }

    #[test]
    fn password_hash_not_serialized() {
        let identity =
            Identity::new("Secure", IdentityRole::Member).with_password_hash("argon2hash");
        let json = serde_json::to_string(&identity).unwrap();
        assert!(!json.contains("argon2hash"));
    }

    #[test]
    fn password_hash_stored_and_retrieved() {
        let store = make_store();
        let identity = Identity::new("HasPassword", IdentityRole::Member)
            .with_email("pw@test.com")
            .with_password_hash("$argon2id$v=19$m=19456,t=2,p=1$salt$hash");
        store.create(&identity).unwrap();

        let retrieved = store.get_by_id(identity.id).unwrap().expect("not found");
        assert_eq!(
            retrieved.password_hash.as_deref(),
            Some("$argon2id$v=19$m=19456,t=2,p=1$salt$hash")
        );
    }

    #[test]
    fn unique_email_constraint() {
        let store = make_store();
        let a = Identity::new("A", IdentityRole::Member).with_email("same@test.com");
        let b = Identity::new("B", IdentityRole::Member).with_email("same@test.com");
        store.create(&a).unwrap();
        assert!(store.create(&b).is_err());
    }

    #[test]
    fn multiple_linked_ids() {
        let store = make_store();
        let identity = Identity::new("MultiPlatform", IdentityRole::Member)
            .with_linked_id("slack", "U123")
            .with_linked_id("ios_device", "device-uuid-456")
            .with_linked_id("discord", "user#7890");
        store.create(&identity).unwrap();

        let by_slack = store.get_by_linked_id("slack", "U123").unwrap().unwrap();
        assert_eq!(by_slack.id, identity.id);

        let by_ios = store
            .get_by_linked_id("ios_device", "device-uuid-456")
            .unwrap()
            .unwrap();
        assert_eq!(by_ios.id, identity.id);

        let by_discord = store
            .get_by_linked_id("discord", "user#7890")
            .unwrap()
            .unwrap();
        assert_eq!(by_discord.id, identity.id);
    }

    // ── Team Tests ──────────────────────────────────────────────────────

    #[test]
    fn team_create_and_get() {
        let store = make_store();
        let creator = Uuid::new_v4();
        let team = Team::new("Engineering", creator);
        store.create_team(&team).unwrap();

        let retrieved = store.get_team(team.id).unwrap().expect("not found");
        assert_eq!(retrieved.name, "Engineering");
        assert_eq!(retrieved.creator_id, creator);
        assert!(retrieved.has_member(creator));
    }

    #[test]
    fn team_add_remove_members() {
        let store = make_store();
        let creator = Uuid::new_v4();
        let member = Uuid::new_v4();
        let mut team = Team::new("Backend", creator);
        assert!(team.add_member(member));
        assert!(!team.add_member(member)); // duplicate
        store.create_team(&team).unwrap();

        let retrieved = store.get_team(team.id).unwrap().unwrap();
        assert_eq!(retrieved.members.len(), 2);

        team.remove_member(member);
        store.update_team(&team).unwrap();
        let retrieved = store.get_team(team.id).unwrap().unwrap();
        assert_eq!(retrieved.members.len(), 1);
    }

    #[test]
    fn team_join_request_flow() {
        let store = make_store();
        let creator = Uuid::new_v4();
        let applicant = Uuid::new_v4();
        let mut team = Team::new("Platform", creator);
        store.create_team(&team).unwrap();

        assert!(team.request_join(applicant));
        assert!(!team.request_join(applicant)); // duplicate
        store.update_team(&team).unwrap();

        let retrieved = store.get_team(team.id).unwrap().unwrap();
        assert_eq!(retrieved.join_requests.len(), 1);
        assert_eq!(retrieved.join_requests[0].identity_id, applicant);

        // Approve
        team.add_member(applicant); // clears join request
        store.update_team(&team).unwrap();
        let retrieved = store.get_team(team.id).unwrap().unwrap();
        assert!(retrieved.join_requests.is_empty());
        assert!(retrieved.has_member(applicant));
    }

    #[test]
    fn team_reject_join() {
        let store = make_store();
        let creator = Uuid::new_v4();
        let applicant = Uuid::new_v4();
        let mut team = Team::new("Infra", creator);
        team.request_join(applicant);
        store.create_team(&team).unwrap();

        team.reject_join(applicant);
        store.update_team(&team).unwrap();
        let retrieved = store.get_team(team.id).unwrap().unwrap();
        assert!(retrieved.join_requests.is_empty());
        assert!(!retrieved.has_member(applicant));
    }

    #[test]
    fn team_list_and_delete() {
        let store = make_store();
        let creator = Uuid::new_v4();
        store.create_team(&Team::new("Alpha", creator)).unwrap();
        store.create_team(&Team::new("Beta", creator)).unwrap();

        let teams = store.list_teams().unwrap();
        assert_eq!(teams.len(), 2);

        store.delete_team(teams[0].id).unwrap();
        assert_eq!(store.list_teams().unwrap().len(), 1);
    }

    #[test]
    fn teams_for_member() {
        let store = make_store();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();

        let mut t1 = Team::new("Team1", alice);
        t1.add_member(bob);
        store.create_team(&t1).unwrap();

        let t2 = Team::new("Team2", alice);
        store.create_team(&t2).unwrap();

        let t3 = Team::new("Team3", bob);
        store.create_team(&t3).unwrap();

        let alice_teams = store.teams_for_member(alice).unwrap();
        assert_eq!(alice_teams.len(), 2); // Team1 + Team2

        let bob_teams = store.teams_for_member(bob).unwrap();
        assert_eq!(bob_teams.len(), 2); // Team1 + Team3
    }

    fn token_for(identity_id: Uuid, created_at: DateTime<Utc>, label: &str) -> RemoteMcpToken {
        let minted = crate::remote_mcp_token::mint();
        RemoteMcpToken {
            id: Uuid::new_v4(),
            identity_id,
            label: label.into(),
            token_hash: minted.hash,
            token_prefix: minted.prefix,
            scopes: vec!["remote_mcp".into()],
            created_at,
            expires_at: created_at + chrono::Duration::days(90),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn remote_mcp_token_create_and_get_by_hash_round_trips() {
        let store = make_store();
        let token = token_for(Uuid::new_v4(), Utc::now(), "laptop");
        store.create_token(&token).unwrap();
        let got = store
            .get_token_by_hash(&token.token_hash)
            .unwrap()
            .expect("present");
        assert_eq!(got.id, token.id);
        assert_eq!(got.identity_id, token.identity_id);
        assert_eq!(got.label, "laptop");
        assert_eq!(got.token_prefix, token.token_prefix);
        assert_eq!(got.scopes, vec!["remote_mcp".to_string()]);
        assert_eq!(got.created_at.to_rfc3339(), token.created_at.to_rfc3339());
        assert_eq!(got.expires_at.to_rfc3339(), token.expires_at.to_rfc3339());
        assert!(got.last_used_at.is_none() && got.revoked_at.is_none());
        assert!(store.get_token_by_hash("missing").unwrap().is_none());
    }

    #[test]
    fn remote_mcp_token_lists_newest_first_and_hides_revoked() {
        let store = make_store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let older = token_for(owner, now - chrono::Duration::hours(2), "older");
        let newer = token_for(owner, now - chrono::Duration::hours(1), "newer");
        let revoked = token_for(owner, now, "revoked");
        let other = token_for(Uuid::new_v4(), now, "other");
        for t in [&older, &newer, &revoked, &other] {
            store.create_token(t).unwrap();
        }
        assert!(store.revoke_token(revoked.id, Some(owner), now).unwrap());

        let mine: Vec<_> = store
            .list_tokens_for_identity(owner)
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(mine, vec!["newer", "older"]);
        let all: Vec<_> = store
            .list_all_tokens()
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(all, vec!["other", "newer", "older"]);
        let still_there = store
            .get_token_by_hash(&revoked.token_hash)
            .unwrap()
            .unwrap();
        assert!(still_there.revoked_at.is_some());
        assert!(!still_there.is_live(now));
    }

    #[test]
    fn remote_mcp_token_revoke_is_owner_scoped_and_idempotent() {
        let store = make_store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let token = token_for(owner, now, "laptop");
        store.create_token(&token).unwrap();

        assert!(!store
            .revoke_token(token.id, Some(Uuid::new_v4()), now)
            .unwrap());
        assert!(store.revoke_token(token.id, Some(owner), now).unwrap());
        assert!(!store.revoke_token(token.id, Some(owner), now).unwrap());
        assert!(!store.revoke_token(token.id, None, now).unwrap());

        let admin_target = token_for(owner, now, "ci");
        store.create_token(&admin_target).unwrap();
        assert!(store.revoke_token(admin_target.id, None, now).unwrap());
        assert!(store.list_tokens_for_identity(owner).unwrap().is_empty());
    }

    #[test]
    fn remote_mcp_token_count_live_ignores_expired_and_revoked() {
        let store = make_store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let live = token_for(owner, now, "live");
        let expired = RemoteMcpToken {
            expires_at: now - chrono::Duration::seconds(1),
            ..token_for(owner, now - chrono::Duration::days(91), "expired")
        };
        let revoked = token_for(owner, now, "revoked");
        for t in [&live, &expired, &revoked] {
            store.create_token(t).unwrap();
        }
        store.revoke_token(revoked.id, Some(owner), now).unwrap();
        assert_eq!(store.count_live_tokens_for_identity(owner, now).unwrap(), 1);
        assert_eq!(
            store
                .count_live_tokens_for_identity(Uuid::new_v4(), now)
                .unwrap(),
            0
        );
    }

    #[test]
    fn remote_mcp_token_touch_last_used_persists() {
        let store = make_store();
        let token = token_for(Uuid::new_v4(), Utc::now(), "laptop");
        store.create_token(&token).unwrap();
        let used_at = Utc::now() + chrono::Duration::minutes(5);
        store.touch_token_last_used(token.id, used_at).unwrap();
        let got = store.get_token_by_hash(&token.token_hash).unwrap().unwrap();
        assert_eq!(
            got.last_used_at.map(|t| t.to_rfc3339()),
            Some(used_at.to_rfc3339())
        );
    }

    #[test]
    fn deleting_an_identity_removes_its_remote_mcp_tokens() {
        let store = make_store();
        let identity = Identity::new("Alice", IdentityRole::Member);
        store.create(&identity).unwrap();
        let token = token_for(identity.id, Utc::now(), "laptop");
        store.create_token(&token).unwrap();
        store.delete(identity.id).unwrap();
        assert!(store
            .get_token_by_hash(&token.token_hash)
            .unwrap()
            .is_none());
    }
}

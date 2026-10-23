use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use rusqlite::{
    params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension, Row,
};
use serde_json::Value as JsonValue;
use tracing::info;
use uuid::Uuid;

use crate::capability::{
    CapabilityAuditEntry, CapabilityChange, CapabilityDecision, CapabilityOverride,
    CapabilityOverrideStore, CustomRole, CustomRoleStore,
};
use crate::store::PermissionStore;
use crate::types::{
    ApproverRef, PermissionFilter, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef,
    PermissionRule, PermissionScope, PermissionState,
};

pub struct SqlitePermissionStore {
    conn: Mutex<Connection>,
}

impl SqlitePermissionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("Failed to open permissions DB: {:?}", path.as_ref()))?;
        // The capability-override store opens a second connection to this same
        // file; wait on a concurrent write rather than erroring with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .wrap_err("setting busy_timeout on permissions DB")?;
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Permission store opened");
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
        // SQLite leaves foreign-key enforcement off by default; without this
        // the ON DELETE CASCADE on permission_role_grants and
        // identity_role_membership would silently leave orphan rows when
        // their parent role is dropped.
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .wrap_err("Failed to enable foreign keys")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS permission_rules (
                id              TEXT PRIMARY KEY,
                scope_type      TEXT NOT NULL,
                scope_id        TEXT NOT NULL,
                tool_pattern    TEXT NOT NULL,
                state           TEXT NOT NULL,
                conditions      TEXT,
                approvers       TEXT NOT NULL DEFAULT '[]',
                allow_self_approval INTEGER NOT NULL DEFAULT 1,
                origin          TEXT NOT NULL,
                created_by      TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                UNIQUE (scope_type, scope_id, tool_pattern)
            );
            CREATE INDEX IF NOT EXISTS idx_permission_rules_scope
                ON permission_rules(scope_type, scope_id);
            CREATE INDEX IF NOT EXISTS idx_permission_rules_tool
                ON permission_rules(tool_pattern);

            CREATE TABLE IF NOT EXISTS permission_role (
                id              TEXT PRIMARY KEY,
                team_id         TEXT NOT NULL,
                name            TEXT NOT NULL,
                description     TEXT NOT NULL DEFAULT '',
                created_at      TEXT NOT NULL,
                UNIQUE (team_id, name)
            );
            CREATE INDEX IF NOT EXISTS idx_permission_role_team
                ON permission_role(team_id);

            CREATE TABLE IF NOT EXISTS permission_role_grants (
                role_id         TEXT NOT NULL,
                rule_id         TEXT NOT NULL,
                PRIMARY KEY (role_id, rule_id),
                FOREIGN KEY (role_id) REFERENCES permission_role(id) ON DELETE CASCADE,
                FOREIGN KEY (rule_id) REFERENCES permission_rules(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS identity_role_membership (
                identity_id     TEXT NOT NULL,
                role_id         TEXT NOT NULL,
                granted_by      TEXT,
                granted_at      TEXT NOT NULL,
                PRIMARY KEY (identity_id, role_id),
                FOREIGN KEY (role_id) REFERENCES permission_role(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_identity_role_membership_identity
                ON identity_role_membership(identity_id);

            CREATE TABLE IF NOT EXISTS permission_rules_generation (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 0),
                generation INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO permission_rules_generation (singleton, generation)
                VALUES (0, 0);

            CREATE TABLE IF NOT EXISTS capability_override (
                role        TEXT NOT NULL,
                capability  TEXT NOT NULL,
                decision    TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                updated_by  TEXT,
                PRIMARY KEY (role, capability)
            );

            CREATE TABLE IF NOT EXISTS capability_override_audit (
                id            TEXT PRIMARY KEY,
                at            TEXT NOT NULL,
                actor_id      TEXT,
                role          TEXT NOT NULL,
                capability    TEXT NOT NULL,
                old_decision  TEXT,
                new_decision  TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_capability_override_audit_at
                ON capability_override_audit(at DESC);

            CREATE TABLE IF NOT EXISTS capability_override_generation (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 0),
                generation INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO capability_override_generation (singleton, generation)
                VALUES (0, 0);

            CREATE TABLE IF NOT EXISTS custom_role (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                name        TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                created_by  TEXT
            );
            ",
        )
        .wrap_err("Failed to initialise permission schema")?;
        Self::add_missing_columns(conn)?;
        Ok(())
    }

    /// Additive migration for DBs created before the routing columns
    /// existed: `CREATE TABLE IF NOT EXISTS` leaves the old shape intact,
    /// so add any column the live table is missing. Idempotent.
    fn add_missing_columns(conn: &Connection) -> Result<()> {
        let existing: std::collections::HashSet<String> = conn
            .prepare("PRAGMA table_info(permission_rules)")?
            .query_map([], |row| row.get::<_, String>("name"))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, decl) in [
            ("approvers", "TEXT NOT NULL DEFAULT '[]'"),
            ("allow_self_approval", "INTEGER NOT NULL DEFAULT 1"),
        ] {
            if !existing.contains(name) {
                conn.execute_batch(&format!(
                    "ALTER TABLE permission_rules ADD COLUMN {name} {decl};"
                ))
                .wrap_err_with(|| format!("Failed to add permission_rules.{name}"))?;
            }
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock permission DB: {}", e))
    }

    fn bump_generation(tx: &rusqlite::Transaction<'_>) -> Result<()> {
        tx.execute(
            "UPDATE permission_rules_generation SET generation = generation + 1 WHERE singleton = 0",
            [],
        )
        .wrap_err("Failed to bump permission generation")?;
        Ok(())
    }
}

// ─── Row parsing ───────────────────────────────────────────────────────

fn parse_rule(row: &Row<'_>) -> rusqlite::Result<PermissionRule> {
    let id: String = row.get("id")?;
    let scope_type: String = row.get("scope_type")?;
    let scope_id: String = row.get("scope_id")?;
    let tool_pattern: String = row.get("tool_pattern")?;
    let state: String = row.get("state")?;
    let conditions: Option<String> = row.get("conditions")?;
    let approvers: Option<String> = row.get("approvers")?;
    let allow_self_approval: Option<i64> = row.get("allow_self_approval")?;
    let origin: String = row.get("origin")?;
    let created_by: Option<String> = row.get("created_by")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;

    Ok(PermissionRule {
        id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
        scope: PermissionScope::from_str_lossy(&scope_type),
        scope_id,
        tool_pattern,
        state: PermissionState::from_str_lossy(&state),
        conditions: conditions.and_then(|s| serde_json::from_str::<JsonValue>(&s).ok()),
        approvers: approvers
            .and_then(|s| serde_json::from_str::<Vec<ApproverRef>>(&s).ok())
            .unwrap_or_default(),
        allow_self_approval: allow_self_approval.map(|v| v != 0).unwrap_or(true),
        origin: PermissionOrigin::from_str_lossy(&origin),
        created_by: created_by.and_then(|s| Uuid::parse_str(&s).ok()),
        created_at: parse_ts(&created_at),
        updated_at: parse_ts(&updated_at),
    })
}

fn parse_role(row: &Row<'_>) -> rusqlite::Result<PermissionRoleDef> {
    let id: String = row.get("id")?;
    let team_id: String = row.get("team_id")?;
    let name: String = row.get("name")?;
    let description: String = row.get("description")?;
    let created_at: String = row.get("created_at")?;

    Ok(PermissionRoleDef {
        id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
        team_id: Uuid::parse_str(&team_id).unwrap_or_else(|_| Uuid::new_v4()),
        name,
        description,
        created_at: parse_ts(&created_at),
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_override(row: &Row<'_>) -> rusqlite::Result<CapabilityOverride> {
    let updated_by: Option<String> = row.get("updated_by")?;
    Ok(CapabilityOverride {
        role: row.get("role")?,
        capability: row.get("capability")?,
        decision: CapabilityDecision::from_str_lossy(&row.get::<_, String>("decision")?),
        updated_at: parse_ts(&row.get::<_, String>("updated_at")?),
        updated_by: updated_by.and_then(|s| Uuid::parse_str(&s).ok()),
    })
}

fn parse_custom_role(row: &Row<'_>) -> rusqlite::Result<CustomRole> {
    let id: String = row.get("id")?;
    let created_by: Option<String> = row.get("created_by")?;
    Ok(CustomRole {
        id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::new_v4()),
        key: row.get("key")?,
        name: row.get("name")?,
        description: row.get("description")?,
        created_at: parse_ts(&row.get::<_, String>("created_at")?),
        created_by: created_by.and_then(|s| Uuid::parse_str(&s).ok()),
    })
}

fn parse_audit(row: &Row<'_>) -> rusqlite::Result<CapabilityAuditEntry> {
    let actor_id: Option<String> = row.get("actor_id")?;
    let old_decision: Option<String> = row.get("old_decision")?;
    let new_decision: Option<String> = row.get("new_decision")?;
    Ok(CapabilityAuditEntry {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).unwrap_or_else(|_| Uuid::new_v4()),
        at: parse_ts(&row.get::<_, String>("at")?),
        actor_id: actor_id.and_then(|s| Uuid::parse_str(&s).ok()),
        role: row.get("role")?,
        capability: row.get("capability")?,
        old_decision: old_decision
            .as_deref()
            .map(CapabilityDecision::from_str_lossy),
        new_decision: new_decision
            .as_deref()
            .map(CapabilityDecision::from_str_lossy),
    })
}

// ─── PermissionStore impl ──────────────────────────────────────────────

impl PermissionStore for SqlitePermissionStore {
    fn upsert_rule(&self, rule: &PermissionRule) -> Result<()> {
        let mut conn = self.lock()?;
        let conditions_json = rule
            .conditions
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let approvers_json = serde_json::to_string(&rule.approvers)?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO permission_rules (
                id, scope_type, scope_id, tool_pattern, state, conditions,
                approvers, allow_self_approval, origin, created_by,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (scope_type, scope_id, tool_pattern) DO UPDATE SET
                state = excluded.state,
                conditions = excluded.conditions,
                approvers = excluded.approvers,
                allow_self_approval = excluded.allow_self_approval,
                origin = excluded.origin,
                created_by = excluded.created_by,
                updated_at = excluded.updated_at",
            params![
                rule.id.to_string(),
                rule.scope.as_str(),
                rule.scope_id,
                rule.tool_pattern,
                rule.state.as_str(),
                conditions_json,
                approvers_json,
                rule.allow_self_approval as i64,
                rule.origin.as_str(),
                rule.created_by.map(|u| u.to_string()),
                rule.created_at.to_rfc3339(),
                rule.updated_at.to_rfc3339(),
            ],
        )
        .wrap_err("upsert_rule failed")?;
        Self::bump_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn get_rule(&self, id: Uuid) -> Result<Option<PermissionRule>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                    approvers, allow_self_approval, origin, created_by,
                    created_at, updated_at
             FROM permission_rules WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(parse_rule(row)?)),
            None => Ok(None),
        }
    }

    fn delete_rule(&self, id: Uuid) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM permission_rules WHERE id = ?1",
            params![id.to_string()],
        )?;
        if n > 0 {
            Self::bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn list_rules(&self, filter: &PermissionFilter) -> Result<Vec<PermissionRule>> {
        let mut sql = String::from(
            "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                    approvers, allow_self_approval, origin, created_by,
                    created_at, updated_at
             FROM permission_rules WHERE 1=1",
        );
        let mut params: Vec<SqlValue> = Vec::new();
        if let Some(ref scope) = filter.scope {
            sql.push_str(" AND scope_type = ?");
            params.push(SqlValue::Text(scope.as_str().to_string()));
        }
        if let Some(ref scopes) = filter.scopes {
            if scopes.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = vec!["?"; scopes.len()].join(",");
            sql.push_str(&format!(" AND scope_type IN ({placeholders})"));
            for scope in scopes {
                params.push(SqlValue::Text(scope.as_str().to_string()));
            }
        }
        if let Some(ref scope_id) = filter.scope_id {
            sql.push_str(" AND scope_id = ?");
            params.push(SqlValue::Text(scope_id.clone()));
        }
        if let Some(ref tool) = filter.tool_pattern {
            // Substring match so the admin UI's search box surfaces every rule
            // whose pattern contains the typed text. `instr` (not `LIKE`) keeps
            // `_`/`%` literal, since tool patterns are full of underscores.
            sql.push_str(" AND instr(tool_pattern, ?) > 0");
            params.push(SqlValue::Text(tool.clone()));
        }
        if let Some(ref origin) = filter.origin {
            sql.push_str(" AND origin = ?");
            params.push(SqlValue::Text(origin.as_str().to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), parse_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list_rules failed")
    }

    fn rules_for_scopes(&self, scopes: &[(PermissionScope, &str)]) -> Result<Vec<PermissionRule>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = scopes
            .iter()
            .enumerate()
            .map(|(i, _)| format!("(?{},?{})", i * 2 + 1, i * 2 + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                    approvers, allow_self_approval, origin, created_by,
                    created_at, updated_at
             FROM permission_rules
             WHERE (scope_type, scope_id) IN ({placeholders})"
        );
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<SqlValue> = scopes
            .iter()
            .flat_map(|(scope, id)| {
                [
                    SqlValue::Text(scope.as_str().to_string()),
                    SqlValue::Text((*id).to_string()),
                ]
            })
            .collect();
        let rows = stmt.query_map(params_from_iter(params.iter()), parse_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("rules_for_scopes failed")
    }

    fn current_generation(&self) -> Result<u64> {
        let conn = self.lock()?;
        let gen: i64 = conn
            .query_row(
                "SELECT generation FROM permission_rules_generation WHERE singleton = 0",
                [],
                |row| row.get(0),
            )
            .wrap_err("current_generation failed")?;
        Ok(gen.max(0) as u64)
    }

    fn upsert_role(&self, role: &PermissionRoleDef) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO permission_role (id, team_id, name, description, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (team_id, name) DO UPDATE SET description = excluded.description",
            params![
                role.id.to_string(),
                role.team_id.to_string(),
                role.name,
                role.description,
                role.created_at.to_rfc3339(),
            ],
        )
        .wrap_err("upsert_role failed")?;
        Self::bump_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn get_role(&self, id: Uuid) -> Result<Option<PermissionRoleDef>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, team_id, name, description, created_at
             FROM permission_role WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(parse_role(row)?)),
            None => Ok(None),
        }
    }

    fn delete_role(&self, id: Uuid) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM permission_role WHERE id = ?1",
            params![id.to_string()],
        )?;
        if n > 0 {
            Self::bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn list_roles_for_team(&self, team_id: Uuid) -> Result<Vec<PermissionRoleDef>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, team_id, name, description, created_at
             FROM permission_role WHERE team_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![team_id.to_string()], parse_role)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list_roles_for_team failed")
    }

    fn grant_rule_to_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO permission_role_grants (role_id, rule_id)
             VALUES (?1, ?2)",
            params![role_id.to_string(), rule_id.to_string()],
        )
        .wrap_err("grant_rule_to_role failed")?;
        Self::bump_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn revoke_rule_from_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM permission_role_grants WHERE role_id = ?1 AND rule_id = ?2",
            params![role_id.to_string(), rule_id.to_string()],
        )?;
        if n > 0 {
            Self::bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn rules_for_role(&self, role_id: Uuid) -> Result<Vec<PermissionRule>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.scope_type, r.scope_id, r.tool_pattern, r.state, r.conditions,
                    r.approvers, r.allow_self_approval, r.origin, r.created_by,
                    r.created_at, r.updated_at
             FROM permission_rules r
             JOIN permission_role_grants g ON g.rule_id = r.id
             WHERE g.role_id = ?1",
        )?;
        let rows = stmt.query_map(params![role_id.to_string()], parse_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("rules_for_role failed")
    }

    fn assign_role_to_identity(&self, assignment: &PermissionRoleAssignment) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO identity_role_membership (identity_id, role_id, granted_by, granted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (identity_id, role_id) DO UPDATE SET
                granted_by = excluded.granted_by,
                granted_at = excluded.granted_at",
            params![
                assignment.identity_id.to_string(),
                assignment.role_id.to_string(),
                assignment.granted_by.map(|u| u.to_string()),
                assignment.granted_at.to_rfc3339(),
            ],
        )
        .wrap_err("assign_role_to_identity failed")?;
        Self::bump_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn unassign_role_from_identity(&self, identity_id: Uuid, role_id: Uuid) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM identity_role_membership WHERE identity_id = ?1 AND role_id = ?2",
            params![identity_id.to_string(), role_id.to_string()],
        )?;
        if n > 0 {
            Self::bump_generation(&tx)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn roles_for_identity(&self, identity_id: Uuid) -> Result<Vec<PermissionRoleDef>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.team_id, r.name, r.description, r.created_at
             FROM permission_role r
             JOIN identity_role_membership m ON m.role_id = r.id
             WHERE m.identity_id = ?1
             ORDER BY r.name",
        )?;
        let rows = stmt.query_map(params![identity_id.to_string()], parse_role)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("roles_for_identity failed")
    }

    fn role_holders(&self) -> Result<std::collections::BTreeMap<Uuid, usize>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT identity_id, COUNT(*) FROM identity_role_membership GROUP BY identity_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                usize::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
            ))
        })?;
        rows.map(|row| {
            let (id, count) = row?;
            Ok((
                Uuid::parse_str(&id).wrap_err("unreadable identity id")?,
                count,
            ))
        })
        .collect()
    }
}

impl CapabilityOverrideStore for SqlitePermissionStore {
    fn list_overrides(&self) -> Result<Vec<CapabilityOverride>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT role, capability, decision, updated_at, updated_by FROM capability_override",
        )?;
        let rows = stmt.query_map([], parse_override)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list_overrides failed")
    }

    fn apply_changes(&self, changes: &[CapabilityChange], actor: Option<Uuid>) -> Result<u64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let actor_str = actor.map(|u| u.to_string());
        let now = Utc::now().to_rfc3339();
        let mut mutated = 0usize;
        for change in changes {
            let prior = tx
                .query_row(
                    "SELECT decision FROM capability_override WHERE role = ?1 AND capability = ?2",
                    params![change.role, change.capability],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .as_deref()
                .map(CapabilityDecision::from_str_lossy);
            if prior == change.decision {
                continue;
            }
            match change.decision {
                Some(decision) => tx.execute(
                    "INSERT INTO capability_override (role, capability, decision, updated_at, updated_by)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (role, capability) DO UPDATE SET
                        decision = excluded.decision,
                        updated_at = excluded.updated_at,
                        updated_by = excluded.updated_by",
                    params![change.role, change.capability, decision.as_str(), now, actor_str],
                ),
                None => tx.execute(
                    "DELETE FROM capability_override WHERE role = ?1 AND capability = ?2",
                    params![change.role, change.capability],
                ),
            }
            .wrap_err("apply capability change failed")?;
            tx.execute(
                "INSERT INTO capability_override_audit
                    (id, at, actor_id, role, capability, old_decision, new_decision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    now,
                    actor_str,
                    change.role,
                    change.capability,
                    prior.map(CapabilityDecision::as_str),
                    change.decision.map(CapabilityDecision::as_str),
                ],
            )
            .wrap_err("append capability audit failed")?;
            mutated += 1;
        }
        if mutated > 0 {
            tx.execute(
                "UPDATE capability_override_generation SET generation = generation + 1 WHERE singleton = 0",
                [],
            )?;
        }
        let generation: i64 = tx.query_row(
            "SELECT generation FROM capability_override_generation WHERE singleton = 0",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(generation.max(0) as u64)
    }

    fn current_generation(&self) -> Result<u64> {
        let conn = self.lock()?;
        let gen: i64 = conn
            .query_row(
                "SELECT generation FROM capability_override_generation WHERE singleton = 0",
                [],
                |row| row.get(0),
            )
            .wrap_err("capability current_generation failed")?;
        Ok(gen.max(0) as u64)
    }

    fn list_audit(&self, limit: usize) -> Result<Vec<CapabilityAuditEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, at, actor_id, role, capability, old_decision, new_decision
             FROM capability_override_audit ORDER BY at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], parse_audit)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list_audit failed")
    }
}

impl CustomRoleStore for SqlitePermissionStore {
    fn list_custom_roles(&self) -> Result<Vec<CustomRole>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, key, name, description, created_at, created_by
             FROM custom_role ORDER BY created_at DESC, key ASC",
        )?;
        let rows = stmt.query_map([], parse_custom_role)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list_custom_roles failed")
    }

    fn create_custom_role(&self, role: &CustomRole) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO custom_role (id, key, name, description, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                role.id.to_string(),
                role.key,
                role.name,
                role.description,
                role.created_at.to_rfc3339(),
                role.created_by.map(|u| u.to_string()),
            ],
        )
        .wrap_err("create_custom_role failed")?;
        Ok(())
    }

    fn update_custom_role(&self, id: Uuid, name: &str, description: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE custom_role SET name = ?2, description = ?3 WHERE id = ?1",
                params![id.to_string(), name, description],
            )
            .wrap_err("update_custom_role failed")?;
        Ok(changed > 0)
    }

    fn delete_custom_role(&self, id: Uuid) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let key: Option<String> = tx
            .query_row(
                "SELECT key FROM custom_role WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(key) = key else {
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM custom_role WHERE id = ?1",
            params![id.to_string()],
        )?;
        let cleared = tx.execute(
            "DELETE FROM capability_override WHERE role = ?1",
            params![key],
        )?;
        if cleared > 0 {
            tx.execute(
                "UPDATE capability_override_generation SET generation = generation + 1 WHERE singleton = 0",
                [],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    fn get_custom_role_by_key(&self, key: &str) -> Result<Option<CustomRole>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, key, name, description, created_at, created_by
             FROM custom_role WHERE key = ?1",
            params![key],
            parse_custom_role,
        )
        .optional()
        .wrap_err("get_custom_role_by_key failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PermissionFilter;

    fn rule(scope: PermissionScope, scope_id: &str, tool: &str) -> PermissionRule {
        PermissionRule::new(
            scope,
            scope_id,
            tool,
            PermissionState::Ask,
            PermissionOrigin::Admin,
        )
    }

    #[test]
    fn role_holders_count_assignments_per_identity() {
        crate::store::role_holders_contract(&SqlitePermissionStore::in_memory().unwrap());
    }

    #[test]
    fn approvers_round_trip_through_upsert_and_reads() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        let user = Uuid::new_v4().to_string();
        let rule = rule(PermissionScope::Agent, "atlas", "vault.delete")
            .with_approvers(vec![
                ApproverRef::User { id: user.clone() },
                ApproverRef::Role {
                    role: "system_admin".to_string(),
                },
            ])
            .with_allow_self_approval(false);
        store.upsert_rule(&rule).unwrap();

        let loaded = store.get_rule(rule.id).unwrap().expect("present");
        assert_eq!(
            loaded.approvers,
            vec![
                ApproverRef::User { id: user },
                ApproverRef::Role {
                    role: "system_admin".to_string()
                },
            ]
        );
        assert!(!loaded.allow_self_approval);

        let listed = store.list_rules(&PermissionFilter::default()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].approvers.len(), 2);
        assert!(!listed[0].allow_self_approval);
    }

    #[test]
    fn legacy_row_decodes_with_defaults() {
        // Build the pre-routing table shape, insert a bare row, then run
        // the additive migration via `init_schema` and read it back.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE permission_rules (
                id              TEXT PRIMARY KEY,
                scope_type      TEXT NOT NULL,
                scope_id        TEXT NOT NULL,
                tool_pattern    TEXT NOT NULL,
                state           TEXT NOT NULL,
                conditions      TEXT,
                origin          TEXT NOT NULL,
                created_by      TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                UNIQUE (scope_type, scope_id, tool_pattern)
            );",
        )
        .unwrap();
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO permission_rules
                (id, scope_type, scope_id, tool_pattern, state, origin, created_at, updated_at)
             VALUES (?1, 'agent', 'atlas', 'vault.delete', 'ask', 'admin', ?2, ?2)",
            params![id.to_string(), now],
        )
        .unwrap();

        SqlitePermissionStore::init_schema(&conn).unwrap();
        let store = SqlitePermissionStore {
            conn: Mutex::new(conn),
        };

        let loaded = store.get_rule(id).unwrap().expect("present");
        assert!(loaded.approvers.is_empty());
        assert!(loaded.allow_self_approval);
    }

    #[test]
    fn approval_routing_picks_most_specific_and_respects_emptiness() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        let team = Uuid::new_v4().to_string();
        let approver = Uuid::new_v4().to_string();

        let team_rule =
            rule(PermissionScope::Team, &team, "vault.*").with_approvers(vec![ApproverRef::Role {
                role: "manager".to_string(),
            }]);
        let agent_rule = rule(PermissionScope::Agent, "atlas", "vault.*").with_approvers(vec![
            ApproverRef::User {
                id: approver.clone(),
            },
        ]);
        store.upsert_rule(&team_rule).unwrap();
        store.upsert_rule(&agent_rule).unwrap();

        let scopes = [
            (PermissionScope::Team, team.as_str()),
            (PermissionScope::Agent, "atlas"),
        ];
        let picked = store
            .approval_routing_for(&scopes, "vault.delete")
            .unwrap()
            .expect("a routing rule matches");
        assert_eq!(picked.id, agent_rule.id, "Agent specificity beats Team");

        // A matching rule without approvers yields no route.
        let store = SqlitePermissionStore::in_memory().unwrap();
        store
            .upsert_rule(&rule(PermissionScope::Agent, "atlas", "vault.*"))
            .unwrap();
        assert!(store
            .approval_routing_for(&scopes, "vault.delete")
            .unwrap()
            .is_none());

        // A non-matching tool yields no route even with approvers present.
        assert!(store
            .approval_routing_for(&scopes, "memory.recall")
            .unwrap()
            .is_none());
    }

    fn set(role: &str, cap: &str, decision: CapabilityDecision) -> CapabilityChange {
        CapabilityChange {
            role: role.to_string(),
            capability: cap.to_string(),
            decision: Some(decision),
        }
    }

    #[test]
    fn overrides_round_trip_and_bump_generation() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        assert_eq!(
            CapabilityOverrideStore::current_generation(&store).unwrap(),
            0
        );

        let actor = Uuid::new_v4();
        let gen = store
            .apply_changes(
                &[
                    set("member", "skills_enable", CapabilityDecision::Allow),
                    set("system_admin", "shell_exec", CapabilityDecision::Deny),
                ],
                Some(actor),
            )
            .unwrap();
        assert_eq!(gen, 1, "one batch bumps the generation once");

        let mut listed = store.list_overrides().unwrap();
        listed.sort_by(|a, b| {
            (a.role.clone(), a.capability.clone()).cmp(&(b.role.clone(), b.capability.clone()))
        });
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].role, "member");
        assert_eq!(listed[0].decision, CapabilityDecision::Allow);
        assert_eq!(listed[0].updated_by, Some(actor));
    }

    #[test]
    fn clearing_to_default_deletes_the_row() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        store
            .apply_changes(
                &[set("member", "config_manage", CapabilityDecision::Allow)],
                None,
            )
            .unwrap();
        assert_eq!(store.list_overrides().unwrap().len(), 1);

        store
            .apply_changes(
                &[CapabilityChange {
                    role: "member".into(),
                    capability: "config_manage".into(),
                    decision: None,
                }],
                None,
            )
            .unwrap();
        assert!(store.list_overrides().unwrap().is_empty());
    }

    #[test]
    fn noop_change_is_skipped_and_does_not_bump() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        let g0 = store
            .apply_changes(
                &[set("member", "ops_read", CapabilityDecision::Allow)],
                None,
            )
            .unwrap();
        let g1 = store
            .apply_changes(
                &[set("member", "ops_read", CapabilityDecision::Allow)],
                None,
            )
            .unwrap();
        assert_eq!(g0, g1, "re-applying the same decision is a no-op");
        assert_eq!(
            store.list_audit(10).unwrap().len(),
            1,
            "no audit for the no-op"
        );
    }

    #[test]
    fn audit_records_old_and_new_transitions() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        store
            .apply_changes(
                &[set("member", "knowledge_manage", CapabilityDecision::Allow)],
                None,
            )
            .unwrap();
        store
            .apply_changes(
                &[set("member", "knowledge_manage", CapabilityDecision::Deny)],
                None,
            )
            .unwrap();
        store
            .apply_changes(
                &[CapabilityChange {
                    role: "member".into(),
                    capability: "knowledge_manage".into(),
                    decision: None,
                }],
                None,
            )
            .unwrap();

        let audit = store.list_audit(10).unwrap();
        assert_eq!(audit.len(), 3, "newest first");
        assert_eq!(audit[0].old_decision, Some(CapabilityDecision::Deny));
        assert_eq!(audit[0].new_decision, None);
        assert_eq!(audit[2].old_decision, None);
        assert_eq!(audit[2].new_decision, Some(CapabilityDecision::Allow));
    }

    fn custom_role(key: &str) -> CustomRole {
        CustomRole {
            id: Uuid::new_v4(),
            key: key.to_string(),
            name: format!("{key} role"),
            description: String::new(),
            created_at: Utc::now(),
            created_by: None,
        }
    }

    #[test]
    fn custom_role_crud_round_trips() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        let role = custom_role("reviewer");
        store.create_custom_role(&role).unwrap();

        assert_eq!(
            store
                .get_custom_role_by_key("reviewer")
                .unwrap()
                .unwrap()
                .id,
            role.id
        );
        assert_eq!(store.list_custom_roles().unwrap().len(), 1);

        assert!(store
            .update_custom_role(role.id, "Reviewer", "Reads everything")
            .unwrap());
        let updated = store.get_custom_role_by_key("reviewer").unwrap().unwrap();
        assert_eq!(updated.name, "Reviewer");
        assert_eq!(updated.description, "Reads everything");

        assert!(store.delete_custom_role(role.id).unwrap());
        assert!(store.get_custom_role_by_key("reviewer").unwrap().is_none());
        assert!(!store.delete_custom_role(role.id).unwrap());
    }

    #[test]
    fn duplicate_custom_role_key_is_rejected() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        store.create_custom_role(&custom_role("reviewer")).unwrap();
        assert!(store.create_custom_role(&custom_role("reviewer")).is_err());
    }

    #[test]
    fn deleting_custom_role_clears_its_overrides_and_bumps_generation() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        let role = custom_role("reviewer");
        store.create_custom_role(&role).unwrap();
        store
            .apply_changes(
                &[CapabilityChange {
                    role: "reviewer".into(),
                    capability: "ops_read".into(),
                    decision: Some(CapabilityDecision::Allow),
                }],
                None,
            )
            .unwrap();
        let before = CapabilityOverrideStore::current_generation(&store).unwrap();

        store.delete_custom_role(role.id).unwrap();

        assert!(
            store.list_overrides().unwrap().is_empty(),
            "override cleared"
        );
        assert!(
            CapabilityOverrideStore::current_generation(&store).unwrap() > before,
            "generation bumped"
        );
    }

    #[test]
    fn tool_pattern_filter_is_substring_with_literal_underscore() {
        let store = SqlitePermissionStore::in_memory().unwrap();
        for tool in [
            "vault.delete",
            "vault.set",
            "memory.recall",
            "code.find_replace",
        ] {
            store
                .upsert_rule(&rule(PermissionScope::Agent, "a", tool))
                .unwrap();
        }
        let by = |pat: &str| {
            store
                .list_rules(&PermissionFilter {
                    tool_pattern: Some(pat.to_string()),
                    ..Default::default()
                })
                .unwrap()
        };
        // Substring matches every vault rule, not just an exact pattern.
        assert_eq!(by("vault").len(), 2);
        assert_eq!(by("find_replace").len(), 1);
        // `_` is literal, not a LIKE single-char wildcard.
        assert!(by("findxreplace").is_empty());
        assert!(by("nomatch").is_empty());
    }
}

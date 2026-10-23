//! One-shot SQLite → Postgres migration for the permission store.
//!
//! Reads every table from a SQLite permissions DB file and writes it
//! into a Postgres backend (after running the refinery migrations
//! against the destination). Driven by the `workforce migrate-permissions`
//! CLI subcommand.
//!
//! Idempotent — data rows insert with `ON CONFLICT … DO NOTHING`, and the
//! two singleton generation counters insert with `ON CONFLICT (singleton)
//! DO UPDATE SET generation = EXCLUDED.generation`, so re-running on a
//! partially-migrated destination is safe and preserves the counters.
//!
//! Foreign keys force a parent-before-child write order:
//! `permission_rules` + `permission_role` before `permission_role_grants`
//! and `identity_role_membership`; `capability_override` before
//! `capability_override_audit`. Postgres enforces FKs immediately, so the
//! call sequence below is load-bearing.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value as JsonValue;
use uuid::Uuid;
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresPermissionStore;

#[derive(Debug, Default, Clone)]
pub struct PermissionsMigrationReport {
    pub permission_rules: usize,
    pub permission_role: usize,
    pub permission_role_grants: usize,
    pub identity_role_membership: usize,
    pub capability_override: usize,
    pub capability_override_audit: usize,
    pub custom_role: usize,
    /// Rows migrated for `permission_rules_generation` (0 or 1).
    pub permission_rules_generation: usize,
    /// Rows migrated for `capability_override_generation` (0 or 1).
    pub capability_override_generation: usize,
}

impl PermissionsMigrationReport {
    pub fn total(&self) -> usize {
        self.permission_rules
            + self.permission_role
            + self.permission_role_grants
            + self.identity_role_membership
            + self.capability_override
            + self.capability_override_audit
            + self.custom_role
            + self.permission_rules_generation
            + self.capability_override_generation
    }
}

/// Migrate a SQLite permissions database into a Postgres backend.
///
/// Builds a fresh permissions pool from `pg_config` (with `search_path`
/// pinned to the permissions schema), opens the SQLite file read-only,
/// runs the refinery migrations against the destination, then copies
/// every row.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<PermissionsMigrationReport> {
    let pool = PostgresPermissionStore::build_pool(pg_config)
        .context("building permissions pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the permissions schema (use [`PostgresPermissionStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<PermissionsMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresPermissionStore> = PostgresPermissionStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres permissions destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let rules = read_rules(&sqlite)?;
    let roles = read_roles(&sqlite)?;
    let grants = read_grants(&sqlite)?;
    let memberships = read_memberships(&sqlite)?;
    let overrides = read_overrides(&sqlite)?;
    let audit = read_audit(&sqlite)?;
    let custom_roles = read_custom_roles(&sqlite)?;
    let rules_generation = read_generation(&sqlite, "permission_rules_generation")?;
    let override_generation = read_generation(&sqlite, "capability_override_generation")?;
    drop(sqlite);

    // Parents before children — FKs are enforced immediately.
    let permission_rules = write_rules(&pool, rules).await?;
    let permission_role = write_roles(&pool, roles).await?;
    let permission_role_grants = write_grants(&pool, grants).await?;
    let identity_role_membership = write_memberships(&pool, memberships).await?;
    let capability_override = write_overrides(&pool, overrides).await?;
    let capability_override_audit = write_audit(&pool, audit).await?;
    let custom_role = write_custom_roles(&pool, custom_roles).await?;
    let permission_rules_generation =
        write_generation(&pool, "permission_rules_generation", rules_generation).await?;
    let capability_override_generation =
        write_generation(&pool, "capability_override_generation", override_generation).await?;

    Ok(PermissionsMigrationReport {
        permission_rules,
        permission_role,
        permission_role_grants,
        identity_role_membership,
        capability_override,
        capability_override_audit,
        custom_role,
        permission_rules_generation,
        capability_override_generation,
    })
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::new_v4())
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ─── permission_rules ──────────────────────────────────────────────────

struct RuleRow {
    id: Uuid,
    scope_type: String,
    scope_id: String,
    tool_pattern: String,
    state: String,
    // JSONB in Postgres.
    conditions: Option<JsonValue>,
    // TEXT in Postgres (a JSON-ish list stored verbatim) — copied as-is.
    approvers: String,
    allow_self_approval: bool,
    origin: String,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn read_rules(sqlite: &Connection) -> Result<Vec<RuleRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                approvers, allow_self_approval, origin, created_by,
                created_at, updated_at
         FROM permission_rules",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let conditions_text: Option<String> = r.get(5)?;
            let approvers: Option<String> = r.get(6)?;
            let allow_self_approval: Option<i64> = r.get(7)?;
            let created_by: Option<String> = r.get(9)?;
            let created_at_str: String = r.get(10)?;
            let updated_at_str: String = r.get(11)?;
            Ok(RuleRow {
                id: parse_uuid(&r.get::<_, String>(0)?),
                scope_type: r.get(1)?,
                scope_id: r.get(2)?,
                tool_pattern: r.get(3)?,
                state: r.get(4)?,
                conditions: conditions_text.and_then(|s| serde_json::from_str(&s).ok()),
                approvers: approvers.unwrap_or_else(|| "[]".to_string()),
                allow_self_approval: allow_self_approval.map(|v| v != 0).unwrap_or(true),
                origin: r.get(8)?,
                created_by: created_by.as_deref().map(parse_uuid),
                created_at: parse_ts(&created_at_str),
                updated_at: parse_ts(&updated_at_str),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_rules(pool: &PostgresPool, rows: Vec<RuleRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO permission_rules
             (id, scope_type, scope_id, tool_pattern, state, conditions,
              approvers, allow_self_approval, origin, created_by,
              created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.scope_type,
                &r.scope_id,
                &r.tool_pattern,
                &r.state,
                &r.conditions,
                &r.approvers,
                &r.allow_self_approval,
                &r.origin,
                &r.created_by,
                &r.created_at,
                &r.updated_at,
            ],
        )
        .await
        .context("insert permission_rules row")?;
    }
    Ok(rows.len())
}

// ─── permission_role ───────────────────────────────────────────────────

struct RoleRow {
    id: Uuid,
    team_id: Uuid,
    name: String,
    description: String,
    created_at: DateTime<Utc>,
}

fn read_roles(sqlite: &Connection) -> Result<Vec<RoleRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, team_id, name, description, created_at
         FROM permission_role",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let created_at_str: String = r.get(4)?;
            Ok(RoleRow {
                id: parse_uuid(&r.get::<_, String>(0)?),
                team_id: parse_uuid(&r.get::<_, String>(1)?),
                name: r.get(2)?,
                description: r.get(3)?,
                created_at: parse_ts(&created_at_str),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_roles(pool: &PostgresPool, rows: Vec<RoleRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO permission_role
             (id, team_id, name, description, created_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO NOTHING",
            &[&r.id, &r.team_id, &r.name, &r.description, &r.created_at],
        )
        .await
        .context("insert permission_role row")?;
    }
    Ok(rows.len())
}

// ─── permission_role_grants ────────────────────────────────────────────

struct GrantRow {
    role_id: Uuid,
    rule_id: Uuid,
}

fn read_grants(sqlite: &Connection) -> Result<Vec<GrantRow>> {
    let mut stmt = sqlite.prepare("SELECT role_id, rule_id FROM permission_role_grants")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(GrantRow {
                role_id: parse_uuid(&r.get::<_, String>(0)?),
                rule_id: parse_uuid(&r.get::<_, String>(1)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_grants(pool: &PostgresPool, rows: Vec<GrantRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO permission_role_grants
             (role_id, rule_id)
             VALUES ($1, $2)
             ON CONFLICT (role_id, rule_id) DO NOTHING",
            &[&r.role_id, &r.rule_id],
        )
        .await
        .context("insert permission_role_grants row")?;
    }
    Ok(rows.len())
}

// ─── identity_role_membership ──────────────────────────────────────────

struct MembershipRow {
    identity_id: Uuid,
    role_id: Uuid,
    granted_by: Option<Uuid>,
    granted_at: DateTime<Utc>,
}

fn read_memberships(sqlite: &Connection) -> Result<Vec<MembershipRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT identity_id, role_id, granted_by, granted_at
         FROM identity_role_membership",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let granted_by: Option<String> = r.get(2)?;
            let granted_at_str: String = r.get(3)?;
            Ok(MembershipRow {
                identity_id: parse_uuid(&r.get::<_, String>(0)?),
                role_id: parse_uuid(&r.get::<_, String>(1)?),
                granted_by: granted_by.as_deref().map(parse_uuid),
                granted_at: parse_ts(&granted_at_str),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_memberships(pool: &PostgresPool, rows: Vec<MembershipRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO identity_role_membership
             (identity_id, role_id, granted_by, granted_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (identity_id, role_id) DO NOTHING",
            &[&r.identity_id, &r.role_id, &r.granted_by, &r.granted_at],
        )
        .await
        .context("insert identity_role_membership row")?;
    }
    Ok(rows.len())
}

// ─── capability_override ───────────────────────────────────────────────

struct OverrideRow {
    role: String,
    capability: String,
    decision: String,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
}

fn read_overrides(sqlite: &Connection) -> Result<Vec<OverrideRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT role, capability, decision, updated_at, updated_by
         FROM capability_override",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let updated_at_str: String = r.get(3)?;
            let updated_by: Option<String> = r.get(4)?;
            Ok(OverrideRow {
                role: r.get(0)?,
                capability: r.get(1)?,
                decision: r.get(2)?,
                updated_at: parse_ts(&updated_at_str),
                updated_by: updated_by.as_deref().map(parse_uuid),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_overrides(pool: &PostgresPool, rows: Vec<OverrideRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO capability_override
             (role, capability, decision, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (role, capability) DO NOTHING",
            &[
                &r.role,
                &r.capability,
                &r.decision,
                &r.updated_at,
                &r.updated_by,
            ],
        )
        .await
        .context("insert capability_override row")?;
    }
    Ok(rows.len())
}

// ─── capability_override_audit ─────────────────────────────────────────

struct AuditRow {
    id: Uuid,
    at: DateTime<Utc>,
    actor_id: Option<Uuid>,
    role: String,
    capability: String,
    old_decision: Option<String>,
    new_decision: Option<String>,
}

fn read_audit(sqlite: &Connection) -> Result<Vec<AuditRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, at, actor_id, role, capability, old_decision, new_decision
         FROM capability_override_audit",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let at_str: String = r.get(1)?;
            let actor_id: Option<String> = r.get(2)?;
            Ok(AuditRow {
                id: parse_uuid(&r.get::<_, String>(0)?),
                at: parse_ts(&at_str),
                actor_id: actor_id.as_deref().map(parse_uuid),
                role: r.get(3)?,
                capability: r.get(4)?,
                old_decision: r.get(5)?,
                new_decision: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_audit(pool: &PostgresPool, rows: Vec<AuditRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO capability_override_audit
             (id, at, actor_id, role, capability, old_decision, new_decision)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.at,
                &r.actor_id,
                &r.role,
                &r.capability,
                &r.old_decision,
                &r.new_decision,
            ],
        )
        .await
        .context("insert capability_override_audit row")?;
    }
    Ok(rows.len())
}

// ─── custom_role ───────────────────────────────────────────────────────

struct CustomRoleRow {
    id: Uuid,
    key: String,
    name: String,
    description: String,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
}

fn read_custom_roles(sqlite: &Connection) -> Result<Vec<CustomRoleRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, key, name, description, created_at, created_by
         FROM custom_role",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let created_at_str: String = r.get(4)?;
            let created_by: Option<String> = r.get(5)?;
            Ok(CustomRoleRow {
                id: parse_uuid(&r.get::<_, String>(0)?),
                key: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                created_at: parse_ts(&created_at_str),
                created_by: created_by.as_deref().map(parse_uuid),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_custom_roles(pool: &PostgresPool, rows: Vec<CustomRoleRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO custom_role
             (id, key, name, description, created_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.key,
                &r.name,
                &r.description,
                &r.created_at,
                &r.created_by,
            ],
        )
        .await
        .context("insert custom_role row")?;
    }
    Ok(rows.len())
}

// ─── *_generation singletons ───────────────────────────────────────────

fn read_generation(sqlite: &Connection, table: &str) -> Result<Option<i64>> {
    let value: Option<i64> = sqlite
        .query_row(
            &format!("SELECT generation FROM {table} WHERE singleton = 0"),
            [],
            |r| r.get(0),
        )
        .optional()
        .with_context(|| format!("reading {table} singleton"))?;
    Ok(value)
}

/// Preserve the singleton counter: the destination migration already
/// seeded `(0, 0)`, so overwrite it with the source value rather than
/// `DO NOTHING`. Returns 1 if a source value was present, else 0.
async fn write_generation(pool: &PostgresPool, table: &str, value: Option<i64>) -> Result<usize> {
    let Some(generation) = value else {
        return Ok(0);
    };
    let conn = pool.get().await?;
    conn.execute(
        &format!(
            "INSERT INTO {table} (singleton, generation)
             VALUES (0, $1)
             ON CONFLICT (singleton) DO UPDATE SET generation = EXCLUDED.generation"
        ),
        &[&generation],
    )
    .await
    .with_context(|| format!("upsert {table} singleton"))?;
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityChange, CapabilityDecision, CapabilityOverrideStore};
    use crate::sqlite::SqlitePermissionStore;
    use crate::store::PermissionStore;
    use crate::types::{
        ApproverRef, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef, PermissionRule,
        PermissionScope, PermissionState,
    };
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_permissions_round_trip() {
        with_test_pool(PostgresPermissionStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqlitePermissionStore::open(tmp.path()).unwrap();

            let team_id = Uuid::new_v4();
            let identity_id = Uuid::new_v4();
            let approver = Uuid::new_v4();
            let actor = Uuid::new_v4();

            // A rule carrying every tricky-typed column away from its default:
            // non-empty approvers (TEXT), allow_self_approval=false (BOOLEAN),
            // conditions=Some (JSONB).
            let rule = PermissionRule::new(
                PermissionScope::Agent,
                "atlas",
                "vault.delete",
                PermissionState::AlwaysAsk,
                PermissionOrigin::Admin,
            )
            .with_approvers(vec![
                ApproverRef::User {
                    id: approver.to_string(),
                },
                ApproverRef::Role {
                    role: "system_admin".to_string(),
                },
            ])
            .with_allow_self_approval(false)
            .with_conditions(serde_json::json!({"max_amount": 100}));
            sqlite.upsert_rule(&rule).unwrap();

            let role = PermissionRoleDef::new(team_id, "agent_operator");
            sqlite.upsert_role(&role).unwrap();
            sqlite.grant_rule_to_role(role.id, rule.id).unwrap();
            sqlite
                .assign_role_to_identity(&PermissionRoleAssignment {
                    identity_id,
                    role_id: role.id,
                    granted_by: Some(actor),
                    granted_at: now_micros(),
                })
                .unwrap();

            sqlite
                .apply_changes(
                    &[CapabilityChange {
                        role: "member".to_string(),
                        capability: "skills_enable".to_string(),
                        decision: Some(CapabilityDecision::Allow),
                    }],
                    Some(actor),
                )
                .unwrap();

            let rules_gen = PermissionStore::current_generation(&sqlite).unwrap();
            let override_gen = CapabilityOverrideStore::current_generation(&sqlite).unwrap();
            assert!(rules_gen > 0);
            assert!(override_gen > 0);
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.permission_rules, 1);
            assert_eq!(report.permission_role, 1);
            assert_eq!(report.permission_role_grants, 1);
            assert_eq!(report.identity_role_membership, 1);
            assert_eq!(report.capability_override, 1);
            assert_eq!(report.capability_override_audit, 1);
            assert_eq!(report.permission_rules_generation, 1);
            assert_eq!(report.capability_override_generation, 1);

            let dest = PostgresPermissionStore::with_pool(pool.clone())
                .await
                .unwrap();

            // Rule survives with every tricky column intact.
            let store: Arc<dyn PermissionStore> = dest.clone();
            let fetched = store.get_rule(rule.id).unwrap().expect("rule present");
            assert_eq!(fetched.tool_pattern, "vault.delete");
            assert_eq!(fetched.state, PermissionState::AlwaysAsk);
            assert!(!fetched.allow_self_approval);
            assert_eq!(fetched.approvers.len(), 2);
            assert_eq!(
                fetched.conditions,
                Some(serde_json::json!({"max_amount": 100}))
            );

            // Linked data survives across the FK chain.
            assert_eq!(store.rules_for_role(role.id).unwrap().len(), 1);
            assert_eq!(store.roles_for_identity(identity_id).unwrap().len(), 1);

            // Counters preserved (not reset to the seeded 0).
            assert_eq!(
                PermissionStore::current_generation(&*dest).unwrap(),
                rules_gen
            );

            let cap: Arc<dyn CapabilityOverrideStore> = dest.clone();
            let overrides = cap.list_overrides().unwrap();
            assert_eq!(overrides.len(), 1);
            assert_eq!(overrides[0].role, "member");
            assert_eq!(overrides[0].decision, CapabilityDecision::Allow);
            assert_eq!(overrides[0].updated_by, Some(actor));
            assert_eq!(cap.current_generation().unwrap(), override_gen);
            assert!(!cap.list_audit(10).unwrap().is_empty());

            // Idempotency: counts match and nothing doubles or rewinds.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.permission_rules, 1);
            assert_eq!(report2.capability_override, 1);
            assert_eq!(report2.permission_rules_generation, 1);
            assert_eq!(store.rules_for_role(role.id).unwrap().len(), 1);
            assert_eq!(cap.list_overrides().unwrap().len(), 1);
            assert_eq!(
                PermissionStore::current_generation(&*dest).unwrap(),
                rules_gen
            );
            assert_eq!(cap.current_generation().unwrap(), override_gen);
        })
        .await;
    }
}

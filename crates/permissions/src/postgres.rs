//! Postgres backend for [`PermissionStore`].
//!
//! Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqlitePermissionStore`]. Selected at runtime when
//! the hub is configured with a Postgres-backed identity store; the
//! `PermissionStore` trait is stable so call sites are unchanged.
//!
//! Tables live in the `permissions` schema. The pool is built via
//! [`PostgresPermissionStore::build_pool`] which installs a
//! `post_create` hook that runs `SET search_path TO permissions, public`
//! on every new connection.

use std::sync::Arc;

use chrono::Utc;
use eyre::{Context, Result};
use serde_json::Value as JsonValue;
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::capability::{
    CapabilityAuditEntry, CapabilityChange, CapabilityDecision, CapabilityOverride,
    CapabilityOverrideStore, CustomRole, CustomRoleStore,
};
use crate::store::PermissionStore;
use crate::types::{
    ApproverRef, PermissionFilter, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef,
    PermissionRule, PermissionScope, PermissionState,
};

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresPermissionStore {
    pool: PostgresPool,
}

impl PostgresPermissionStore {
    pub const SCHEMA: &'static str = "permissions";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO permissions, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building permissions pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedPermissionMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[set])
            .await
            .context("running permission migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedPermissionMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedPermissionMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Row parsing ───────────────────────────────────────────────────────

fn row_to_rule(row: &Row) -> Result<PermissionRule> {
    let scope_str: String = row.try_get("scope_type")?;
    let state_str: String = row.try_get("state")?;
    let origin_str: String = row.try_get("origin")?;
    let conditions: Option<JsonValue> = row.try_get("conditions")?;
    let approvers: Option<String> = row.try_get("approvers")?;
    let allow_self_approval: Option<bool> = row.try_get("allow_self_approval")?;

    Ok(PermissionRule {
        id: row.try_get("id")?,
        scope: PermissionScope::from_str_lossy(&scope_str),
        scope_id: row.try_get("scope_id")?,
        tool_pattern: row.try_get("tool_pattern")?,
        state: PermissionState::from_str_lossy(&state_str),
        conditions,
        approvers: approvers
            .and_then(|s| serde_json::from_str::<Vec<ApproverRef>>(&s).ok())
            .unwrap_or_default(),
        allow_self_approval: allow_self_approval.unwrap_or(true),
        origin: PermissionOrigin::from_str_lossy(&origin_str),
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_role(row: &Row) -> Result<PermissionRoleDef> {
    Ok(PermissionRoleDef {
        id: row.try_get("id")?,
        team_id: row.try_get("team_id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_override(row: &Row) -> Result<CapabilityOverride> {
    let decision: String = row.try_get("decision")?;
    Ok(CapabilityOverride {
        role: row.try_get("role")?,
        capability: row.try_get("capability")?,
        decision: CapabilityDecision::from_str_lossy(&decision),
        updated_at: row.try_get("updated_at")?,
        updated_by: row.try_get("updated_by")?,
    })
}

fn row_to_custom_role(row: &Row) -> Result<CustomRole> {
    Ok(CustomRole {
        id: row.try_get("id")?,
        key: row.try_get("key")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        created_at: row.try_get("created_at")?,
        created_by: row.try_get("created_by")?,
    })
}

fn row_to_audit(row: &Row) -> Result<CapabilityAuditEntry> {
    let old_decision: Option<String> = row.try_get("old_decision")?;
    let new_decision: Option<String> = row.try_get("new_decision")?;
    Ok(CapabilityAuditEntry {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        actor_id: row.try_get("actor_id")?,
        role: row.try_get("role")?,
        capability: row.try_get("capability")?,
        old_decision: old_decision
            .as_deref()
            .map(CapabilityDecision::from_str_lossy),
        new_decision: new_decision
            .as_deref()
            .map(CapabilityDecision::from_str_lossy),
    })
}

// ─── PermissionStore impl ──────────────────────────────────────────────

impl PermissionStore for PostgresPermissionStore {
    fn upsert_rule(&self, rule: &PermissionRule) -> Result<()> {
        let rule = rule.clone();
        let approvers_json = serde_json::to_string(&rule.approvers)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.execute(
                "INSERT INTO permission_rules (
                    id, scope_type, scope_id, tool_pattern, state, conditions,
                    approvers, allow_self_approval, origin, created_by,
                    created_at, updated_at
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                 ON CONFLICT (scope_type, scope_id, tool_pattern) DO UPDATE SET
                    state = EXCLUDED.state,
                    conditions = EXCLUDED.conditions,
                    approvers = EXCLUDED.approvers,
                    allow_self_approval = EXCLUDED.allow_self_approval,
                    origin = EXCLUDED.origin,
                    created_by = EXCLUDED.created_by,
                    updated_at = EXCLUDED.updated_at",
                &[
                    &rule.id,
                    &rule.scope.as_str(),
                    &rule.scope_id,
                    &rule.tool_pattern,
                    &rule.state.as_str(),
                    &rule.conditions,
                    &approvers_json,
                    &rule.allow_self_approval,
                    &rule.origin.as_str(),
                    &rule.created_by,
                    &rule.created_at,
                    &rule.updated_at,
                ],
            )
            .await
            .context("upsert_rule failed")?;
            bump_generation(&tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn get_rule(&self, id: Uuid) -> Result<Option<PermissionRule>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                            approvers, allow_self_approval, origin, created_by,
                            created_at, updated_at
                     FROM permission_rules WHERE id = $1",
                    &[&id],
                )
                .await?;
            row.as_ref().map(row_to_rule).transpose()
        })
    }

    fn delete_rule(&self, id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let n = tx
                .execute("DELETE FROM permission_rules WHERE id = $1", &[&id])
                .await?;
            if n > 0 {
                bump_generation(&tx).await?;
            }
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    fn list_rules(&self, filter: &PermissionFilter) -> Result<Vec<PermissionRule>> {
        let scope = filter.scope.map(|s| s.as_str().to_string());
        let scopes: Option<Vec<String>> = filter
            .scopes
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str().to_string()).collect());
        if matches!(scopes.as_ref(), Some(v) if v.is_empty()) {
            return Ok(Vec::new());
        }
        let scope_id = filter.scope_id.clone();
        let tool = filter.tool_pattern.clone();
        let origin = filter.origin.map(|o| o.as_str().to_string());
        let limit = filter.limit.unwrap_or(500) as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let mut where_parts: Vec<String> = Vec::new();
            let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
            if let Some(ref s) = scope {
                where_parts.push(format!("scope_type = ${}", params.len() + 1));
                params.push(s);
            }
            if let Some(ref ss) = scopes {
                where_parts.push(format!("scope_type = ANY(${})", params.len() + 1));
                params.push(ss);
            }
            if let Some(ref sid) = scope_id {
                where_parts.push(format!("scope_id = ${}", params.len() + 1));
                params.push(sid);
            }
            if let Some(ref t) = tool {
                // Substring match for the admin UI's search box. `strpos` (not
                // `LIKE`) keeps `_`/`%` literal, since tool patterns use them.
                where_parts.push(format!("strpos(tool_pattern, ${}) > 0", params.len() + 1));
                params.push(t);
            }
            if let Some(ref o) = origin {
                where_parts.push(format!("origin = ${}", params.len() + 1));
                params.push(o);
            }
            let where_clause = if where_parts.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", where_parts.join(" AND "))
            };
            let limit_idx = params.len() + 1;
            params.push(&limit);
            let sql = format!(
                "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                        approvers, allow_self_approval, origin, created_by,
                        created_at, updated_at
                 FROM permission_rules {where_clause}
                 ORDER BY created_at DESC
                 LIMIT ${limit_idx}"
            );
            let rows = conn.query(&sql, &params).await?;
            rows.iter().map(row_to_rule).collect()
        })
    }

    fn rules_for_scopes(&self, scopes: &[(PermissionScope, &str)]) -> Result<Vec<PermissionRule>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let scope_strs: Vec<String> = scopes.iter().map(|(s, _)| s.as_str().to_string()).collect();
        let scope_ids: Vec<String> = scopes.iter().map(|(_, id)| (*id).to_string()).collect();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // unnest the two parallel arrays into pairs and filter on the
            // composite key — one round-trip regardless of pair count.
            let rows = conn
                .query(
                    "SELECT id, scope_type, scope_id, tool_pattern, state, conditions,
                            approvers, allow_self_approval, origin, created_by,
                            created_at, updated_at
                     FROM permission_rules
                     WHERE (scope_type, scope_id) IN (
                        SELECT * FROM unnest($1::text[], $2::text[])
                     )",
                    &[&scope_strs, &scope_ids],
                )
                .await?;
            rows.iter().map(row_to_rule).collect()
        })
    }

    fn current_generation(&self) -> Result<u64> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT generation FROM permission_rules_generation WHERE singleton = 0",
                    &[],
                )
                .await?;
            let gen: i64 = row.try_get("generation")?;
            Ok(gen.max(0) as u64)
        })
    }

    fn upsert_role(&self, role: &PermissionRoleDef) -> Result<()> {
        let role = role.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.execute(
                "INSERT INTO permission_role (id, team_id, name, description, created_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (team_id, name) DO UPDATE SET description = EXCLUDED.description",
                &[
                    &role.id,
                    &role.team_id,
                    &role.name,
                    &role.description,
                    &role.created_at,
                ],
            )
            .await
            .context("upsert_role failed")?;
            bump_generation(&tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn get_role(&self, id: Uuid) -> Result<Option<PermissionRoleDef>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, team_id, name, description, created_at
                     FROM permission_role WHERE id = $1",
                    &[&id],
                )
                .await?;
            row.as_ref().map(row_to_role).transpose()
        })
    }

    fn delete_role(&self, id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let n = tx
                .execute("DELETE FROM permission_role WHERE id = $1", &[&id])
                .await?;
            if n > 0 {
                bump_generation(&tx).await?;
            }
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    fn list_roles_for_team(&self, team_id: Uuid) -> Result<Vec<PermissionRoleDef>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, team_id, name, description, created_at
                     FROM permission_role WHERE team_id = $1 ORDER BY name",
                    &[&team_id],
                )
                .await?;
            rows.iter().map(row_to_role).collect()
        })
    }

    fn grant_rule_to_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<()> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.execute(
                "INSERT INTO permission_role_grants (role_id, rule_id)
                 VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
                &[&role_id, &rule_id],
            )
            .await
            .context("grant_rule_to_role failed")?;
            bump_generation(&tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn revoke_rule_from_role(&self, role_id: Uuid, rule_id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let n = tx
                .execute(
                    "DELETE FROM permission_role_grants WHERE role_id = $1 AND rule_id = $2",
                    &[&role_id, &rule_id],
                )
                .await?;
            if n > 0 {
                bump_generation(&tx).await?;
            }
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    fn rules_for_role(&self, role_id: Uuid) -> Result<Vec<PermissionRule>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT r.id, r.scope_type, r.scope_id, r.tool_pattern, r.state, r.conditions,
                            r.approvers, r.allow_self_approval, r.origin, r.created_by,
                            r.created_at, r.updated_at
                     FROM permission_rules r
                     JOIN permission_role_grants g ON g.rule_id = r.id
                     WHERE g.role_id = $1",
                    &[&role_id],
                )
                .await?;
            rows.iter().map(row_to_rule).collect()
        })
    }

    fn assign_role_to_identity(&self, assignment: &PermissionRoleAssignment) -> Result<()> {
        let assignment = assignment.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.execute(
                "INSERT INTO identity_role_membership (identity_id, role_id, granted_by, granted_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (identity_id, role_id) DO UPDATE SET
                    granted_by = EXCLUDED.granted_by,
                    granted_at = EXCLUDED.granted_at",
                &[
                    &assignment.identity_id,
                    &assignment.role_id,
                    &assignment.granted_by,
                    &assignment.granted_at,
                ],
            )
            .await
            .context("assign_role_to_identity failed")?;
            bump_generation(&tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn unassign_role_from_identity(&self, identity_id: Uuid, role_id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let n = tx
                .execute(
                    "DELETE FROM identity_role_membership WHERE identity_id = $1 AND role_id = $2",
                    &[&identity_id, &role_id],
                )
                .await?;
            if n > 0 {
                bump_generation(&tx).await?;
            }
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    fn roles_for_identity(&self, identity_id: Uuid) -> Result<Vec<PermissionRoleDef>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT r.id, r.team_id, r.name, r.description, r.created_at
                     FROM permission_role r
                     JOIN identity_role_membership m ON m.role_id = r.id
                     WHERE m.identity_id = $1
                     ORDER BY r.name",
                    &[&identity_id],
                )
                .await?;
            rows.iter().map(row_to_role).collect()
        })
    }

    fn role_holders(&self) -> Result<std::collections::BTreeMap<Uuid, usize>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT identity_id, COUNT(*) FROM identity_role_membership GROUP BY identity_id",
                    &[],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|row| {
                    (
                        row.get::<_, Uuid>(0),
                        usize::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
                    )
                })
                .collect())
        })
    }
}

async fn bump_generation(tx: &tokio_postgres::Transaction<'_>) -> Result<()> {
    tx.execute(
        "UPDATE permission_rules_generation
         SET generation = generation + 1
         WHERE singleton = 0",
        &[],
    )
    .await
    .context("bump_generation failed")?;
    Ok(())
}

impl CapabilityOverrideStore for PostgresPermissionStore {
    fn list_overrides(&self) -> Result<Vec<CapabilityOverride>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT role, capability, decision, updated_at, updated_by FROM capability_override",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_override).collect()
        })
    }

    fn apply_changes(&self, changes: &[CapabilityChange], actor: Option<Uuid>) -> Result<u64> {
        let changes = changes.to_vec();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let now = Utc::now();
            let mut mutated = 0usize;
            for change in &changes {
                let prior = tx
                    .query_opt(
                        "SELECT decision FROM capability_override WHERE role = $1 AND capability = $2",
                        &[&change.role, &change.capability],
                    )
                    .await?
                    .map(|row| row.get::<_, String>(0));
                let prior = prior.as_deref().map(CapabilityDecision::from_str_lossy);
                if prior == change.decision {
                    continue;
                }
                match change.decision {
                    Some(decision) => {
                        tx.execute(
                            "INSERT INTO capability_override (role, capability, decision, updated_at, updated_by)
                             VALUES ($1, $2, $3, $4, $5)
                             ON CONFLICT (role, capability) DO UPDATE SET
                                decision = EXCLUDED.decision,
                                updated_at = EXCLUDED.updated_at,
                                updated_by = EXCLUDED.updated_by",
                            &[&change.role, &change.capability, &decision.as_str(), &now, &actor],
                        )
                        .await?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM capability_override WHERE role = $1 AND capability = $2",
                            &[&change.role, &change.capability],
                        )
                        .await?;
                    }
                }
                let id = Uuid::new_v4();
                let old_str = prior.map(CapabilityDecision::as_str);
                let new_str = change.decision.map(CapabilityDecision::as_str);
                tx.execute(
                    "INSERT INTO capability_override_audit
                        (id, at, actor_id, role, capability, old_decision, new_decision)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[&id, &now, &actor, &change.role, &change.capability, &old_str, &new_str],
                )
                .await?;
                mutated += 1;
            }
            if mutated > 0 {
                tx.execute(
                    "UPDATE capability_override_generation SET generation = generation + 1 WHERE singleton = 0",
                    &[],
                )
                .await?;
            }
            let generation: i64 = tx
                .query_one(
                    "SELECT generation FROM capability_override_generation WHERE singleton = 0",
                    &[],
                )
                .await?
                .get(0);
            tx.commit().await?;
            Ok(generation.max(0) as u64)
        })
    }

    fn current_generation(&self) -> Result<u64> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let generation: i64 = conn
                .query_one(
                    "SELECT generation FROM capability_override_generation WHERE singleton = 0",
                    &[],
                )
                .await?
                .get(0);
            Ok(generation.max(0) as u64)
        })
    }

    fn list_audit(&self, limit: usize) -> Result<Vec<CapabilityAuditEntry>> {
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, at, actor_id, role, capability, old_decision, new_decision
                     FROM capability_override_audit ORDER BY at DESC, id DESC LIMIT $1",
                    &[&limit],
                )
                .await?;
            rows.iter().map(row_to_audit).collect()
        })
    }
}

impl CustomRoleStore for PostgresPermissionStore {
    fn list_custom_roles(&self) -> Result<Vec<CustomRole>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, key, name, description, created_at, created_by
                     FROM custom_role ORDER BY created_at DESC, key ASC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_custom_role).collect()
        })
    }

    fn create_custom_role(&self, role: &CustomRole) -> Result<()> {
        let role = role.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO custom_role (id, key, name, description, created_at, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &role.id,
                    &role.key,
                    &role.name,
                    &role.description,
                    &role.created_at,
                    &role.created_by,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn update_custom_role(&self, id: Uuid, name: &str, description: &str) -> Result<bool> {
        let (name, description) = (name.to_string(), description.to_string());
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let changed = conn
                .execute(
                    "UPDATE custom_role SET name = $2, description = $3 WHERE id = $1",
                    &[&id, &name, &description],
                )
                .await?;
            Ok(changed > 0)
        })
    }

    fn delete_custom_role(&self, id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let key: Option<String> = tx
                .query_opt("SELECT key FROM custom_role WHERE id = $1", &[&id])
                .await?
                .map(|row| row.get(0));
            let Some(key) = key else {
                return Ok(false);
            };
            tx.execute("DELETE FROM custom_role WHERE id = $1", &[&id])
                .await?;
            let cleared = tx
                .execute("DELETE FROM capability_override WHERE role = $1", &[&key])
                .await?;
            if cleared > 0 {
                tx.execute(
                    "UPDATE capability_override_generation SET generation = generation + 1 WHERE singleton = 0",
                    &[],
                )
                .await?;
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    fn get_custom_role_by_key(&self, key: &str) -> Result<Option<CustomRole>> {
        let key = key.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.query_opt(
                "SELECT id, key, name, description, created_at, created_by
                 FROM custom_role WHERE key = $1",
                &[&key],
            )
            .await?
            .as_ref()
            .map(row_to_custom_role)
            .transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresPermissionStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresPermissionStore::build_pool, |pool| async move {
            let store = PostgresPermissionStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_rule_round_trip_and_generation_bump() {
        with_embedded_pg(|store| async move {
            let store: Arc<dyn PermissionStore> = store;
            let team = Uuid::new_v4();
            assert_eq!(store.current_generation().unwrap(), 0);

            let rule = PermissionRule::new(
                PermissionScope::Team,
                team.to_string(),
                "memory.*",
                PermissionState::Ask,
                PermissionOrigin::Admin,
            );
            store.upsert_rule(&rule).unwrap();
            assert_eq!(store.current_generation().unwrap(), 1);

            let loaded = store.get_rule(rule.id).unwrap().expect("present");
            assert_eq!(loaded.tool_pattern, "memory.*");
            assert_eq!(loaded.state, PermissionState::Ask);

            let mut updated = rule.clone();
            updated.state = PermissionState::Deny;
            updated.updated_at = now_micros();
            store.upsert_rule(&updated).unwrap();
            assert_eq!(
                store.get_rule(rule.id).unwrap().unwrap().state,
                PermissionState::Deny
            );
            assert_eq!(store.current_generation().unwrap(), 2);

            assert!(store.delete_rule(rule.id).unwrap());
            assert_eq!(store.current_generation().unwrap(), 3);
            assert!(!store.delete_rule(rule.id).unwrap());
            assert_eq!(store.current_generation().unwrap(), 3);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_rules_for_scopes_batch() {
        with_embedded_pg(|store| async move {
            let store: Arc<dyn PermissionStore> = store;
            let team = Uuid::new_v4().to_string();
            let agent = "atlas".to_string();
            let user = Uuid::new_v4().to_string();

            for (scope, sid, tool, state) in [
                (
                    PermissionScope::Team,
                    team.as_str(),
                    "memory.*",
                    PermissionState::Ask,
                ),
                (
                    PermissionScope::Agent,
                    agent.as_str(),
                    "vault.*",
                    PermissionState::AlwaysAsk,
                ),
                (
                    PermissionScope::User,
                    user.as_str(),
                    "memory.*",
                    PermissionState::Allow,
                ),
            ] {
                store
                    .upsert_rule(&PermissionRule::new(
                        scope,
                        sid,
                        tool,
                        state,
                        PermissionOrigin::Admin,
                    ))
                    .unwrap();
            }

            let rows = store
                .rules_for_scopes(&[
                    (PermissionScope::Team, team.as_str()),
                    (PermissionScope::Agent, agent.as_str()),
                    (PermissionScope::User, user.as_str()),
                ])
                .unwrap();
            assert_eq!(rows.len(), 3);

            let empty = store.rules_for_scopes(&[]).unwrap();
            assert!(empty.is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_role_holders_count_assignments_per_identity() {
        with_embedded_pg(|store| async move {
            crate::store::role_holders_contract(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_role_lifecycle() {
        with_embedded_pg(|store| async move {
            let store: Arc<dyn PermissionStore> = store;
            let team_id = Uuid::new_v4();
            let identity_id = Uuid::new_v4();

            let role = PermissionRoleDef::new(team_id, "agent_operator");
            store.upsert_role(&role).unwrap();
            assert_eq!(store.list_roles_for_team(team_id).unwrap().len(), 1);

            let rule = PermissionRule::new(
                PermissionScope::Team,
                team_id.to_string(),
                "messaging.*",
                PermissionState::Allow,
                PermissionOrigin::Admin,
            );
            store.upsert_rule(&rule).unwrap();
            store.grant_rule_to_role(role.id, rule.id).unwrap();
            assert_eq!(store.rules_for_role(role.id).unwrap().len(), 1);

            let assignment = PermissionRoleAssignment {
                identity_id,
                role_id: role.id,
                granted_by: None,
                granted_at: now_micros(),
            };
            store.assign_role_to_identity(&assignment).unwrap();
            assert_eq!(store.roles_for_identity(identity_id).unwrap().len(), 1);

            assert!(store.delete_role(role.id).unwrap());
            assert!(store.roles_for_identity(identity_id).unwrap().is_empty());
        })
        .await;
    }
}

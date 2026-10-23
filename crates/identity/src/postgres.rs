//! Postgres backend for [`IdentityStore`] and [`TeamStore`].
//!
//! Phase 4 of the multi-store migration roadmap (see
//! `docs/POSTGRES.md`). Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqliteIdentityStore`]. Selected at runtime via
//! `[identity] backend = "postgres"` in `workforce.toml`; the
//! `IdentityStore` and `TeamStore` traits are stable so call sites
//! are unchanged regardless of which backend is in use.
//!
//! ## Async-to-sync bridge
//!
//! Like the telemetry store, [`IdentityStore`] and [`TeamStore`] are
//! synchronous traits (matching the historical `SqliteIdentityStore`
//! API). `tokio_postgres` is async. We bridge with
//! [`tokio::task::block_in_place`] + `Handle::current().block_on(...)`,
//! which yields the calling worker thread to the runtime for the
//! duration of the query. This requires the multi-threaded tokio
//! runtime that the workforce hub always uses.
//!
//! ## Schema and search_path
//!
//! Tables live in the `identity` Postgres schema. The pool is built
//! via [`PostgresIdentityStore::build_pool`] which installs a
//! `post_create` hook that runs `SET search_path TO identity, public`
//! on every new connection — so unqualified table names always
//! resolve regardless of which connection deadpool hands out.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::lifecycle::{LifecycleOp, LifecycleOpStore, OpStatus};
use crate::remote_mcp_token::{RemoteMcpToken, RemoteMcpTokenStore};
use crate::store::{IdentityAndTeamStore, IdentityFilter, IdentityStore};
use crate::team::{JoinRequest, Team, TeamStore};
use crate::types::{Identity, IdentityRole, IdentityStatus, ProvisioningSource};

mod embedded {
    refinery::embed_migrations!("migrations");
}

/// Postgres backend for the identity / team store.
pub struct PostgresIdentityStore {
    pool: PostgresPool,
}

impl PostgresIdentityStore {
    /// Schema this store owns. All identity tables live under it.
    pub const SCHEMA: &'static str = "identity";

    /// Build a [`PostgresPool`] suitable for identity queries: every
    /// connection it hands out has its `search_path` pinned to the
    /// identity schema, so unqualified table names always resolve.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO identity, public"],
        )
    }

    /// Build a pool from config, run pending migrations, and return a
    /// ready-to-use store.
    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building identity pool")?;
        Self::with_pool(pool).await
    }

    /// Variant that takes an already-built pool. Caller is responsible
    /// for ensuring the pool was built via [`Self::build_pool`] (or
    /// otherwise has its `search_path` pinned to the identity schema).
    /// Used by tests against the embedded Postgres harness.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedIdentityMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running identity migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    /// Run a future that returns `Result<T>` on the current Tokio
    /// runtime, blocking the calling worker thread for the duration.
    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedIdentityMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedIdentityMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn row_to_identity(row: &Row) -> Result<Identity> {
    let linked_ids_json: serde_json::Value = row.try_get("linked_ids")?;
    let linked_ids: HashMap<String, String> =
        serde_json::from_value(linked_ids_json).unwrap_or_default();

    let role_str: String = row.try_get("role")?;
    let status_str: String = row.try_get("status")?;
    let source_str: String = row.try_get("provisioning_source")?;

    Ok(Identity {
        id: row.try_get("id")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        password_hash: row.try_get("password_hash")?,
        role: IdentityRole::from_str_lossy(&role_str),
        role_key: role_str,
        status: IdentityStatus::from_str_lossy(&status_str),
        provisioning_source: ProvisioningSource::from_str_lossy(&source_str),
        linked_ids,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_team(row: &Row) -> Result<Team> {
    let members_json: serde_json::Value = row.try_get("members")?;
    let members: Vec<Uuid> = serde_json::from_value(members_json).unwrap_or_default();
    let join_requests_json: serde_json::Value = row.try_get("join_requests")?;
    let join_requests: Vec<JoinRequest> =
        serde_json::from_value(join_requests_json).unwrap_or_default();

    Ok(Team {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        creator_id: row.try_get("creator_id")?,
        members,
        join_requests,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_remote_mcp_token(row: &Row) -> Result<RemoteMcpToken> {
    let scopes_json: serde_json::Value = row.try_get("scopes")?;
    Ok(RemoteMcpToken {
        id: row.try_get("id")?,
        identity_id: row.try_get("identity_id")?,
        label: row.try_get("label")?,
        token_hash: row.try_get("token_hash")?,
        token_prefix: row.try_get("token_prefix")?,
        scopes: serde_json::from_value(scopes_json).unwrap_or_default(),
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used_at: row.try_get("last_used_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

const REMOTE_MCP_TOKEN_COLUMNS: &str = "id, identity_id, label, token_hash, token_prefix, scopes, \
                                        created_at, expires_at, last_used_at, revoked_at";

// ─── IdentityStore impl ────────────────────────────────────────────────

impl IdentityStore for PostgresIdentityStore {
    fn create(&self, identity: &Identity) -> Result<()> {
        let identity = identity.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let linked_ids_json = serde_json::to_value(&identity.linked_ids)?;
            conn.execute(
                "INSERT INTO identities
                 (id, display_name, email, password_hash, role, status,
                  provisioning_source, linked_ids, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                &[
                    &identity.id,
                    &identity.display_name,
                    &identity.email,
                    &identity.password_hash,
                    &identity.effective_role_key(),
                    &identity.status.as_str(),
                    &identity.provisioning_source.as_str(),
                    &linked_ids_json,
                    &identity.created_at,
                    &identity.updated_at,
                ],
            )
            .await
            .context("insert identity")?;
            Ok(())
        })
    }

    fn get_by_id(&self, id: Uuid) -> Result<Option<Identity>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, display_name, email, password_hash, role, status,
                            provisioning_source, linked_ids, created_at, updated_at
                     FROM identities WHERE id = $1",
                    &[&id],
                )
                .await?;
            row.as_ref().map(row_to_identity).transpose()
        })
    }

    fn get_by_email(&self, email: &str) -> Result<Option<Identity>> {
        let email = email.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, display_name, email, password_hash, role, status,
                            provisioning_source, linked_ids, created_at, updated_at
                     FROM identities WHERE email = $1",
                    &[&email],
                )
                .await?;
            row.as_ref().map(row_to_identity).transpose()
        })
    }

    fn get_by_linked_id(&self, platform: &str, platform_id: &str) -> Result<Option<Identity>> {
        let platform = platform.to_string();
        let platform_id = platform_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // Use jsonb containment, indexed by the GIN index on
            // linked_ids. Build the probe value as a single-key map.
            let mut probe = serde_json::Map::new();
            probe.insert(platform, serde_json::Value::String(platform_id));
            let probe = serde_json::Value::Object(probe);
            let row = conn
                .query_opt(
                    "SELECT id, display_name, email, password_hash, role, status,
                            provisioning_source, linked_ids, created_at, updated_at
                     FROM identities WHERE linked_ids @> $1
                     LIMIT 1",
                    &[&probe],
                )
                .await?;
            row.as_ref().map(row_to_identity).transpose()
        })
    }

    fn update(&self, identity: &Identity) -> Result<()> {
        let identity = identity.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let linked_ids_json = serde_json::to_value(&identity.linked_ids)?;
            conn.execute(
                "UPDATE identities SET
                    display_name = $1,
                    email = $2,
                    password_hash = $3,
                    role = $4,
                    status = $5,
                    provisioning_source = $6,
                    linked_ids = $7,
                    updated_at = $8
                 WHERE id = $9",
                &[
                    &identity.display_name,
                    &identity.email,
                    &identity.password_hash,
                    &identity.effective_role_key(),
                    &identity.status.as_str(),
                    &identity.provisioning_source.as_str(),
                    &linked_ids_json,
                    &identity.updated_at,
                    &identity.id,
                ],
            )
            .await
            .context("update identity")?;
            Ok(())
        })
    }

    fn delete(&self, id: Uuid) -> Result<()> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute("DELETE FROM identities WHERE id = $1", &[&id])
                .await?;
            Ok(())
        })
    }

    fn list(&self, filter: &IdentityFilter) -> Result<Vec<Identity>> {
        let role = filter.role.map(|r| r.as_str().to_string());
        let status = filter.status.map(|s| s.as_str().to_string());
        let search = filter.search.as_ref().map(|s| format!("%{s}%"));
        let limit = filter.limit.unwrap_or(100) as i64;
        let offset = filter.offset.unwrap_or(0) as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
            let where_clause = push_identity_filter(&role, &status, &search, &mut params);
            let limit_idx = params.len() + 1;
            let offset_idx = params.len() + 2;
            params.push(&limit);
            params.push(&offset);
            let sql = format!(
                "SELECT id, display_name, email, password_hash, role, status,
                        provisioning_source, linked_ids, created_at, updated_at
                 FROM identities {where_clause}
                 ORDER BY created_at DESC, id DESC
                 LIMIT ${limit_idx} OFFSET ${offset_idx}"
            );
            let rows = conn.query(&sql, &params).await?;
            rows.iter().map(row_to_identity).collect()
        })
    }

    fn count_filtered(&self, filter: &IdentityFilter) -> Result<usize> {
        let role = filter.role.map(|r| r.as_str().to_string());
        let status = filter.status.map(|s| s.as_str().to_string());
        let search = filter.search.as_ref().map(|s| format!("%{s}%"));
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
            let where_clause = push_identity_filter(&role, &status, &search, &mut params);
            let sql = format!("SELECT COUNT(*)::BIGINT FROM identities {where_clause}");
            let row = conn.query_one(&sql, &params).await?;
            Ok(row.get::<_, i64>(0) as usize)
        })
    }
}

/// Append the `role` / `status` / `search` predicates shared by `list` and
/// `count_filtered`, binding each into `params` in placeholder order.
/// tokio-postgres needs a `&[&(dyn ToSql + Sync)]`, so callers own the values
/// and this borrows them. `search` matches `display_name` or `email` via a
/// single `$n` reused for both columns.
fn push_identity_filter<'a>(
    role: &'a Option<String>,
    status: &'a Option<String>,
    search: &'a Option<String>,
    params: &mut Vec<&'a (dyn tokio_postgres::types::ToSql + Sync)>,
) -> String {
    let mut where_parts: Vec<String> = Vec::new();
    if let Some(r) = role {
        where_parts.push(format!("role = ${}", params.len() + 1));
        params.push(r);
    }
    if let Some(s) = status {
        where_parts.push(format!("status = ${}", params.len() + 1));
        params.push(s);
    }
    if let Some(q) = search {
        let idx = params.len() + 1;
        where_parts.push(format!("(display_name ILIKE ${idx} OR email ILIKE ${idx})"));
        params.push(q);
    }
    if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    }
}

// ─── TeamStore impl ────────────────────────────────────────────────────

impl TeamStore for PostgresIdentityStore {
    fn create_team(&self, team: &Team) -> Result<()> {
        let team = team.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let members = serde_json::to_value(&team.members)?;
            let join_requests = serde_json::to_value(&team.join_requests)?;
            conn.execute(
                "INSERT INTO teams (id, name, creator_id, members, join_requests, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &team.id,
                    &team.name,
                    &team.creator_id,
                    &members,
                    &join_requests,
                    &team.created_at,
                    &team.updated_at,
                ],
            )
            .await
            .context("insert team")?;
            Ok(())
        })
    }

    fn get_team(&self, id: Uuid) -> Result<Option<Team>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, name, creator_id, members, join_requests, created_at, updated_at
                     FROM teams WHERE id = $1",
                    &[&id],
                )
                .await?;
            row.as_ref().map(row_to_team).transpose()
        })
    }

    fn update_team(&self, team: &Team) -> Result<()> {
        let team = team.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let members = serde_json::to_value(&team.members)?;
            let join_requests = serde_json::to_value(&team.join_requests)?;
            conn.execute(
                "UPDATE teams SET name = $1, members = $2, join_requests = $3, updated_at = $4
                 WHERE id = $5",
                &[
                    &team.name,
                    &members,
                    &join_requests,
                    &team.updated_at,
                    &team.id,
                ],
            )
            .await
            .context("update team")?;
            Ok(())
        })
    }

    fn delete_team(&self, id: Uuid) -> Result<()> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute("DELETE FROM teams WHERE id = $1", &[&id])
                .await?;
            Ok(())
        })
    }

    fn list_teams(&self) -> Result<Vec<Team>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, name, creator_id, members, join_requests, created_at, updated_at
                     FROM teams ORDER BY created_at DESC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_team).collect()
        })
    }

    fn teams_for_member(&self, identity_id: Uuid) -> Result<Vec<Team>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // members is a JSONB array of UUID strings; use array
            // containment so the GIN index kicks in.
            let probe =
                serde_json::Value::Array(vec![serde_json::Value::String(identity_id.to_string())]);
            let rows = conn
                .query(
                    "SELECT id, name, creator_id, members, join_requests, created_at, updated_at
                     FROM teams WHERE members @> $1
                     ORDER BY created_at DESC",
                    &[&probe],
                )
                .await?;
            rows.iter().map(row_to_team).collect()
        })
    }
}

impl RemoteMcpTokenStore for PostgresIdentityStore {
    fn create_token(&self, token: &RemoteMcpToken) -> Result<()> {
        let token = token.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let scopes = serde_json::to_value(&token.scopes)?;
            conn.execute(
                "INSERT INTO remote_mcp_tokens
                 (id, identity_id, label, token_hash, token_prefix, scopes,
                  created_at, expires_at, last_used_at, revoked_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                &[
                    &token.id,
                    &token.identity_id,
                    &token.label,
                    &token.token_hash,
                    &token.token_prefix,
                    &scopes,
                    &token.created_at,
                    &token.expires_at,
                    &token.last_used_at,
                    &token.revoked_at,
                ],
            )
            .await
            .context("insert remote mcp token")?;
            Ok(())
        })
    }

    fn get_token_by_hash(&self, token_hash: &str) -> Result<Option<RemoteMcpToken>> {
        let token_hash = token_hash.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!(
                        "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens WHERE token_hash = $1"
                    ),
                    &[&token_hash],
                )
                .await?;
            row.as_ref().map(row_to_remote_mcp_token).transpose()
        })
    }

    fn list_tokens_for_identity(&self, identity_id: Uuid) -> Result<Vec<RemoteMcpToken>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens
                         WHERE identity_id = $1 AND revoked_at IS NULL
                         ORDER BY created_at DESC, id DESC"
                    ),
                    &[&identity_id],
                )
                .await?;
            rows.iter().map(row_to_remote_mcp_token).collect()
        })
    }

    fn list_all_tokens(&self) -> Result<Vec<RemoteMcpToken>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {REMOTE_MCP_TOKEN_COLUMNS} FROM remote_mcp_tokens
                         WHERE revoked_at IS NULL
                         ORDER BY created_at DESC, id DESC"
                    ),
                    &[],
                )
                .await?;
            rows.iter().map(row_to_remote_mcp_token).collect()
        })
    }

    fn revoke_token(&self, id: Uuid, identity_id: Option<Uuid>, at: DateTime<Utc>) -> Result<bool> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let changed = match identity_id {
                Some(owner) => {
                    conn.execute(
                        "UPDATE remote_mcp_tokens SET revoked_at = $1
                         WHERE id = $2 AND identity_id = $3 AND revoked_at IS NULL",
                        &[&at, &id, &owner],
                    )
                    .await?
                }
                None => {
                    conn.execute(
                        "UPDATE remote_mcp_tokens SET revoked_at = $1
                         WHERE id = $2 AND revoked_at IS NULL",
                        &[&at, &id],
                    )
                    .await?
                }
            };
            Ok(changed > 0)
        })
    }

    fn touch_token_last_used(&self, id: Uuid, at: DateTime<Utc>) -> Result<()> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE remote_mcp_tokens SET last_used_at = $1 WHERE id = $2",
                &[&at, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn count_live_tokens_for_identity(
        &self,
        identity_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COUNT(*)::BIGINT FROM remote_mcp_tokens
                     WHERE identity_id = $1 AND revoked_at IS NULL AND expires_at > $2",
                    &[&identity_id, &now],
                )
                .await?;
            Ok(row.get::<_, i64>(0) as usize)
        })
    }
}

impl IdentityAndTeamStore for PostgresIdentityStore {
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

const LIFECYCLE_OP_COLUMNS: &str =
    "id, subject, kind, target, actor, names, status, report, started_at, lease_until, finished_at";

fn row_to_lifecycle_op(row: &Row) -> Result<LifecycleOp> {
    let status: String = row.try_get("status")?;
    Ok(LifecycleOp {
        id: row.try_get("id")?,
        subject: row.try_get("subject")?,
        kind: row.try_get("kind")?,
        target: row.try_get("target")?,
        actor: row.try_get("actor")?,
        names: serde_json::from_value(row.try_get("names")?).unwrap_or_default(),
        status: OpStatus::from_str_lossy(&status),
        report: row.try_get("report")?,
        started_at: row.try_get("started_at")?,
        lease_until: row.try_get("lease_until")?,
        finished_at: row.try_get("finished_at")?,
    })
}

impl LifecycleOpStore for PostgresIdentityStore {
    fn claim_op(&self, op: &LifecycleOp, now: DateTime<Utc>) -> Result<bool> {
        let op = op.clone();
        let parties = op.parties();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            // Serialises claims on each identity across replicas, taken in
            // one order so two claims sharing identities cannot deadlock:
            // without it two could both see no live lease and both insert.
            for party in &parties {
                tx.execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended('identity_lifecycle:' || $1::uuid::text, 0))",
                    &[party],
                )
                .await?;
            }
            let live = tx
                .query_opt(
                    "SELECT 1 FROM identity_lifecycle_ops
                     WHERE status = 'running' AND lease_until > $2
                       AND (subject = ANY($1) OR target = ANY($1))
                     LIMIT 1",
                    &[&parties, &now],
                )
                .await?;
            if live.is_some() {
                return Ok(false);
            }
            tx.execute(
                "UPDATE identity_lifecycle_ops SET status = 'interrupted'
                 WHERE status = 'running' AND (subject = ANY($1) OR target = ANY($1))",
                &[&parties],
            )
            .await?;
            tx.execute(
                &format!(
                    "INSERT INTO identity_lifecycle_ops ({LIFECYCLE_OP_COLUMNS})
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
                ),
                &[
                    &op.id,
                    &op.subject,
                    &op.kind,
                    &op.target,
                    &op.actor,
                    &serde_json::to_value(&op.names)?,
                    &op.status.as_str(),
                    &op.report,
                    &op.started_at,
                    &op.lease_until,
                    &op.finished_at,
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
    }

    fn renew_op(
        &self,
        id: Uuid,
        lease_until: DateTime<Utc>,
        report: &serde_json::Value,
    ) -> Result<bool> {
        let report = report.clone();
        self.block_on(async move {
            Ok(self
                .pool
                .get()
                .await?
                .execute(
                    "UPDATE identity_lifecycle_ops SET lease_until = $1, report = $2
                     WHERE id = $3 AND status = 'running'",
                    &[&lease_until, &report, &id],
                )
                .await?
                > 0)
        })
    }

    fn get_op(&self, id: Uuid) -> Result<Option<LifecycleOp>> {
        self.block_on(async move {
            self.pool
                .get()
                .await?
                .query_opt(
                    &format!(
                        "SELECT {LIFECYCLE_OP_COLUMNS} FROM identity_lifecycle_ops WHERE id = $1"
                    ),
                    &[&id],
                )
                .await?
                .as_ref()
                .map(row_to_lifecycle_op)
                .transpose()
        })
    }

    fn finish_op(
        &self,
        id: Uuid,
        status: OpStatus,
        report: &serde_json::Value,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let report = report.clone();
        self.block_on(async move {
            Ok(self
                .pool
                .get()
                .await?
                .execute(
                    "UPDATE identity_lifecycle_ops
                     SET status = $1, report = $2, finished_at = $3, lease_until = $3
                     WHERE id = $4 AND status = 'running'",
                    &[&status.as_str(), &report, &at, &id],
                )
                .await?
                > 0)
        })
    }

    fn list_ops(&self, subject: Option<Uuid>, limit: usize) -> Result<Vec<LifecycleOp>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async move {
            let rows = self
                .pool
                .get()
                .await?
                .query(
                    &format!(
                        "SELECT {LIFECYCLE_OP_COLUMNS} FROM identity_lifecycle_ops
                         WHERE $1::uuid IS NULL OR subject = $1
                         ORDER BY started_at DESC, id DESC LIMIT $2"
                    ),
                    &[&subject, &limit],
                )
                .await?;
            rows.iter().map(row_to_lifecycle_op).collect()
        })
    }
}

// ─── Parity tests against embedded Postgres ────────────────────────────
//
// Pattern copied from `workforce-common::telemetry_postgres::tests` —
// shared instance per test binary, unique DB per test, no Docker, no
// env vars.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DurationRound as _;
    use workforce_postgres::test_support::with_test_pool;

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresIdentityStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresIdentityStore::build_pool, |pool| async move {
            let store = PostgresIdentityStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_claims_are_exclusive_per_subject_until_the_lease_lapses() {
        with_embedded_pg(|store| async move {
            crate::lifecycle::contract::claims_are_exclusive_per_subject_until_the_lease_lapses(
                store.as_ref(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_a_taken_over_operation_is_fenced() {
        with_embedded_pg(|store| async move {
            crate::lifecycle::contract::a_taken_over_operation_is_fenced(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lifecycle_renewal_extends_the_lease_and_records_progress() {
        with_embedded_pg(|store| async move {
            crate::lifecycle::contract::renewal_extends_the_lease_and_records_progress(
                store.as_ref(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_create_get_update_delete() {
        with_embedded_pg(|store| async move {
            let id_store: Arc<dyn IdentityStore> = store;

            assert_eq!(id_store.count().unwrap(), 0);

            let mut identity = Identity::new("Alice", IdentityRole::Member)
                .with_email("alice@example.com")
                .with_password_hash("hash");
            id_store.create(&identity).unwrap();

            let fetched = id_store.get_by_id(identity.id).unwrap().expect("by id");
            assert_eq!(fetched.display_name, "Alice");
            assert_eq!(fetched.email.as_deref(), Some("alice@example.com"));
            assert_eq!(fetched.password_hash.as_deref(), Some("hash"));

            let by_email = id_store
                .get_by_email("alice@example.com")
                .unwrap()
                .expect("by email");
            assert_eq!(by_email.id, identity.id);

            identity.set_role(IdentityRole::Manager);
            identity.display_name = "Alice Updated".into();
            identity.link_id("slack", "U123");
            id_store.update(&identity).unwrap();

            let updated = id_store.get_by_id(identity.id).unwrap().unwrap();
            assert_eq!(updated.role, IdentityRole::Manager);
            assert_eq!(updated.display_name, "Alice Updated");
            assert_eq!(updated.linked_ids.get("slack").unwrap(), "U123");

            let by_link = id_store
                .get_by_linked_id("slack", "U123")
                .unwrap()
                .expect("by link");
            assert_eq!(by_link.id, identity.id);

            assert_eq!(id_store.count().unwrap(), 1);
            id_store.delete(identity.id).unwrap();
            assert_eq!(id_store.count().unwrap(), 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_with_filter() {
        with_embedded_pg(|store| async move {
            let id_store: Arc<dyn IdentityStore> = store;
            id_store
                .create(&Identity::new("Admin", IdentityRole::SystemAdmin))
                .unwrap();
            id_store
                .create(&Identity::new("Manager", IdentityRole::Manager))
                .unwrap();
            id_store
                .create(&Identity::new("Member1", IdentityRole::Member))
                .unwrap();
            id_store
                .create(&Identity::new("Member2", IdentityRole::Member))
                .unwrap();

            assert_eq!(id_store.list(&IdentityFilter::default()).unwrap().len(), 4);
            assert_eq!(
                id_store
                    .list(&IdentityFilter {
                        role: Some(IdentityRole::Member),
                        ..Default::default()
                    })
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                id_store
                    .list(&IdentityFilter {
                        role: Some(IdentityRole::SystemAdmin),
                        ..Default::default()
                    })
                    .unwrap()
                    .len(),
                1
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_search_pagination_and_count() {
        with_embedded_pg(|store| async move {
            let id_store: Arc<dyn IdentityStore> = store;
            for i in 0i64..5 {
                let mut identity = Identity::new(format!("Mat{i}"), IdentityRole::Member)
                    .with_email(format!("mat{i}@example.com"));
                identity.created_at =
                    chrono::DateTime::from_timestamp(1_700_000_000 + i, 0).unwrap();
                id_store.create(&identity).unwrap();
            }
            let mut other =
                Identity::new("Other", IdentityRole::Member).with_email("other@example.com");
            other.created_at = chrono::DateTime::from_timestamp(1_700_000_009, 0).unwrap();
            id_store.create(&other).unwrap();

            let search = IdentityFilter {
                search: Some("MAT".to_string()),
                ..Default::default()
            };
            assert_eq!(id_store.count_filtered(&search).unwrap(), 5);

            let page = IdentityFilter {
                search: Some("mat".to_string()),
                limit: Some(2),
                offset: Some(0),
                ..Default::default()
            };
            let names: Vec<String> = id_store
                .list(&page)
                .unwrap()
                .into_iter()
                .map(|i| i.display_name)
                .collect();
            assert_eq!(names, ["Mat4", "Mat3"]);
            assert_eq!(id_store.count_filtered(&page).unwrap(), 5);

            assert_eq!(id_store.count().unwrap(), 6);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_team_crud_and_membership_lookup() {
        with_embedded_pg(|store| async move {
            let team_store: Arc<dyn TeamStore> = store;
            let alice = Uuid::new_v4();
            let bob = Uuid::new_v4();

            let mut t1 = Team::new("Team1", alice);
            t1.add_member(bob);
            team_store.create_team(&t1).unwrap();

            let t2 = Team::new("Team2", alice);
            team_store.create_team(&t2).unwrap();

            let t3 = Team::new("Team3", bob);
            team_store.create_team(&t3).unwrap();

            assert_eq!(team_store.list_teams().unwrap().len(), 3);

            // Alice is in Team1 (creator) and Team2 (creator).
            assert_eq!(team_store.teams_for_member(alice).unwrap().len(), 2);
            // Bob is in Team1 (added) and Team3 (creator).
            assert_eq!(team_store.teams_for_member(bob).unwrap().len(), 2);

            let mut t1_updated = team_store.get_team(t1.id).unwrap().unwrap();
            t1_updated.name = "Team1 Renamed".into();
            team_store.update_team(&t1_updated).unwrap();
            assert_eq!(
                team_store.get_team(t1.id).unwrap().unwrap().name,
                "Team1 Renamed"
            );

            team_store.delete_team(t3.id).unwrap();
            assert_eq!(team_store.list_teams().unwrap().len(), 2);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_remote_mcp_tokens() {
        with_embedded_pg(|store| async move {
            let owner = Identity::new("Alice", IdentityRole::Member);
            store.create(&owner).unwrap();
            // Postgres keeps microseconds; a nanosecond clock (Linux) would
            // make the round-trip compare unequal on the last three digits.
            let now = Utc::now()
                .duration_trunc(chrono::Duration::microseconds(1))
                .unwrap();
            let mk = |label: &str, created_at: DateTime<Utc>| {
                let minted = crate::remote_mcp_token::mint();
                RemoteMcpToken {
                    id: Uuid::new_v4(),
                    identity_id: owner.id,
                    label: label.into(),
                    token_hash: minted.hash,
                    token_prefix: minted.prefix,
                    scopes: vec!["remote_mcp".into()],
                    created_at,
                    expires_at: created_at + chrono::Duration::days(90),
                    last_used_at: None,
                    revoked_at: None,
                }
            };
            let older = mk("older", now - chrono::Duration::hours(1));
            let newer = mk("newer", now);
            let expired = RemoteMcpToken {
                expires_at: now - chrono::Duration::seconds(1),
                ..mk("expired", now - chrono::Duration::days(91))
            };
            for t in [&older, &newer, &expired] {
                store.create_token(t).unwrap();
            }

            let got = store
                .get_token_by_hash(&newer.token_hash)
                .unwrap()
                .expect("by hash");
            assert_eq!(got, newer);
            assert_eq!(
                store.count_live_tokens_for_identity(owner.id, now).unwrap(),
                2
            );

            let labels: Vec<_> = store
                .list_tokens_for_identity(owner.id)
                .unwrap()
                .into_iter()
                .map(|t| t.label)
                .collect();
            assert_eq!(labels, vec!["newer", "older", "expired"]);

            store.touch_token_last_used(newer.id, now).unwrap();
            assert!(store
                .get_token_by_hash(&newer.token_hash)
                .unwrap()
                .unwrap()
                .last_used_at
                .is_some());

            assert!(!store
                .revoke_token(older.id, Some(Uuid::new_v4()), now)
                .unwrap());
            assert!(store.revoke_token(older.id, Some(owner.id), now).unwrap());
            assert!(!store.revoke_token(older.id, None, now).unwrap());
            assert_eq!(store.list_all_tokens().unwrap().len(), 2);
            assert_eq!(
                store.count_live_tokens_for_identity(owner.id, now).unwrap(),
                1
            );

            store.delete(owner.id).unwrap();
            assert!(store.list_all_tokens().unwrap().is_empty());
        })
        .await;
    }
}

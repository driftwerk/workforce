//! One-shot SQLite → Postgres migration for the identity store.
//!
//! Reads the `identities` and `teams` tables from a SQLite
//! `identities.db` file and writes them into a Postgres backend
//! (after running the refinery migrations against the destination).
//! Driven by the `workforce migrate-identity` CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (id) DO NOTHING`, so
//! re-running on a partially-migrated destination is safe.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::lifecycle::LifecycleOp;
use crate::postgres::PostgresIdentityStore;
use crate::remote_mcp_token::RemoteMcpToken;
use crate::sqlite::{parse_lifecycle_op_row, parse_remote_mcp_token_row, LIFECYCLE_OP_COLUMNS};

#[derive(Debug, Default, Clone)]
pub struct IdentityMigrationReport {
    pub identities: usize,
    pub teams: usize,
    pub remote_mcp_tokens: usize,
    pub lifecycle_ops: usize,
}

impl IdentityMigrationReport {
    pub fn total(&self) -> usize {
        self.identities + self.teams + self.remote_mcp_tokens + self.lifecycle_ops
    }
}

/// Migrate a SQLite identity database into a Postgres backend.
///
/// Builds a fresh identity pool from `pg_config` (with `search_path`
/// pinned to the identity schema), opens the SQLite file read-only,
/// runs the refinery migrations against the destination, then copies
/// every row.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<IdentityMigrationReport> {
    let pool = PostgresIdentityStore::build_pool(pg_config)
        .context("building identity pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the identity schema (use [`PostgresIdentityStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<IdentityMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresIdentityStore> = PostgresIdentityStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres identity destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let identities = read_identities(&sqlite)?;
    let teams = read_teams(&sqlite)?;
    let tokens = read_remote_mcp_tokens(&sqlite)?;
    let ops = read_lifecycle_ops(&sqlite)?;
    drop(sqlite);

    let identity_count = write_identities(&pool, identities).await?;
    let team_count = write_teams(&pool, teams).await?;
    let token_count = write_remote_mcp_tokens(&pool, tokens).await?;
    let op_count = write_lifecycle_ops(&pool, ops).await?;

    Ok(IdentityMigrationReport {
        identities: identity_count,
        teams: team_count,
        remote_mcp_tokens: token_count,
        lifecycle_ops: op_count,
    })
}

#[allow(clippy::type_complexity)]
struct IdentityMigRow {
    id: Uuid,
    display_name: String,
    email: Option<String>,
    password_hash: Option<String>,
    role: String,
    status: String,
    provisioning_source: String,
    linked_ids: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn read_identities(sqlite: &Connection) -> Result<Vec<IdentityMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, display_name, email, password_hash, role, status,
                provisioning_source, linked_ids, created_at, updated_at
         FROM identities",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let id = Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4());
            let linked_ids_text: String = r.get(7)?;
            let linked_ids: serde_json::Value =
                serde_json::from_str(&linked_ids_text).unwrap_or_else(|_| serde_json::json!({}));
            let created_at_str: String = r.get(8)?;
            let updated_at_str: String = r.get(9)?;
            Ok(IdentityMigRow {
                id,
                display_name: r.get(1)?,
                email: r.get(2)?,
                password_hash: r.get(3)?,
                role: r.get(4)?,
                status: r.get(5)?,
                provisioning_source: r.get(6)?,
                linked_ids,
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_identities(pool: &PostgresPool, rows: Vec<IdentityMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO identities
             (id, display_name, email, password_hash, role, status,
              provisioning_source, linked_ids, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.display_name,
                &r.email,
                &r.password_hash,
                &r.role,
                &r.status,
                &r.provisioning_source,
                &r.linked_ids,
                &r.created_at,
                &r.updated_at,
            ],
        )
        .await
        .context("insert identities row")?;
    }
    Ok(rows.len())
}

struct TeamMigRow {
    id: Uuid,
    name: String,
    creator_id: Uuid,
    members: serde_json::Value,
    join_requests: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn read_teams(sqlite: &Connection) -> Result<Vec<TeamMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, name, creator_id, members, join_requests, created_at, updated_at
         FROM teams",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let creator_str: String = r.get(2)?;
            let members_text: String = r.get(3)?;
            let join_requests_text: String = r.get(4)?;
            let created_at_str: String = r.get(5)?;
            let updated_at_str: String = r.get(6)?;
            Ok(TeamMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                name: r.get(1)?,
                creator_id: Uuid::parse_str(&creator_str).unwrap_or_else(|_| Uuid::new_v4()),
                members: serde_json::from_str(&members_text)
                    .unwrap_or_else(|_| serde_json::json!([])),
                join_requests: serde_json::from_str(&join_requests_text)
                    .unwrap_or_else(|_| serde_json::json!([])),
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_teams(pool: &PostgresPool, rows: Vec<TeamMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO teams
             (id, name, creator_id, members, join_requests, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.name,
                &r.creator_id,
                &r.members,
                &r.join_requests,
                &r.created_at,
                &r.updated_at,
            ],
        )
        .await
        .context("insert teams row")?;
    }
    Ok(rows.len())
}

fn read_remote_mcp_tokens(sqlite: &Connection) -> Result<Vec<RemoteMcpToken>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, identity_id, label, token_hash, token_prefix, scopes,
                created_at, expires_at, last_used_at, revoked_at
         FROM remote_mcp_tokens",
    )?;
    let rows = stmt
        .query_map([], parse_remote_mcp_token_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_remote_mcp_tokens(pool: &PostgresPool, rows: Vec<RemoteMcpToken>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for t in &rows {
        let scopes = serde_json::to_value(&t.scopes)?;
        conn.execute(
            "INSERT INTO remote_mcp_tokens
             (id, identity_id, label, token_hash, token_prefix, scopes,
              created_at, expires_at, last_used_at, revoked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO NOTHING",
            &[
                &t.id,
                &t.identity_id,
                &t.label,
                &t.token_hash,
                &t.token_prefix,
                &scopes,
                &t.created_at,
                &t.expires_at,
                &t.last_used_at,
                &t.revoked_at,
            ],
        )
        .await
        .context("insert remote_mcp_tokens row")?;
    }
    Ok(rows.len())
}

/// A source written before lifecycle operations existed has no table.
fn read_lifecycle_ops(sqlite: &Connection) -> Result<Vec<LifecycleOp>> {
    let present = sqlite
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'identity_lifecycle_ops'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !present {
        return Ok(Vec::new());
    }
    let mut stmt = sqlite.prepare(&format!(
        "SELECT {LIFECYCLE_OP_COLUMNS} FROM identity_lifecycle_ops"
    ))?;
    let rows = stmt
        .query_map([], parse_lifecycle_op_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_lifecycle_ops(pool: &PostgresPool, rows: Vec<LifecycleOp>) -> Result<usize> {
    let conn = pool.get().await?;
    for op in &rows {
        conn.execute(
            "INSERT INTO identity_lifecycle_ops
             (id, subject, kind, target, actor, names, status, report,
              started_at, lease_until, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id) DO NOTHING",
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
        .await
        .context("insert identity_lifecycle_ops row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp_token::RemoteMcpTokenStore;
    use crate::sqlite::SqliteIdentityStore;
    use crate::store::IdentityStore;
    use crate::team::{Team, TeamStore};
    use crate::types::{Identity, IdentityRole};
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_identity_round_trip() {
        with_test_pool(PostgresIdentityStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteIdentityStore::new(tmp.path()).unwrap();
            let alice =
                Identity::new("Alice", IdentityRole::Member).with_email("alice@example.com");
            let alice_id = alice.id;
            sqlite.create(&alice).unwrap();
            let team = Team::new("Engineering", uuid::Uuid::new_v4());
            sqlite.create_team(&team).unwrap();
            let minted = crate::remote_mcp_token::mint();
            let token = RemoteMcpToken {
                id: uuid::Uuid::new_v4(),
                identity_id: alice_id,
                label: "laptop".into(),
                token_hash: minted.hash,
                token_prefix: minted.prefix,
                scopes: vec!["remote_mcp".into()],
                created_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::days(90),
                last_used_at: None,
                revoked_at: None,
            };
            sqlite.create_token(&token).unwrap();
            let op = crate::lifecycle::contract::op(alice_id, Utc::now());
            crate::lifecycle::LifecycleOpStore::claim_op(&sqlite, &op, Utc::now()).unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.identities, 1);
            assert_eq!(report.teams, 1);
            assert_eq!(report.remote_mcp_tokens, 1);
            assert_eq!(report.lifecycle_ops, 1);

            let dest = PostgresIdentityStore::with_pool(pool.clone())
                .await
                .unwrap();
            let id_store: Arc<dyn IdentityStore> = dest.clone();
            let dest_tokens: Arc<dyn RemoteMcpTokenStore> = dest.clone();
            let ops =
                crate::lifecycle::LifecycleOpStore::list_ops(dest.as_ref(), None, 10).unwrap();
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].names, op.names);
            let team_store: Arc<dyn TeamStore> = dest;
            let fetched = id_store.get_by_id(alice_id).unwrap().expect("alice");
            assert_eq!(fetched.display_name, "Alice");
            let teams = team_store.list_teams().unwrap();
            assert_eq!(teams.len(), 1);
            assert_eq!(teams[0].name, "Engineering");
            let migrated = dest_tokens
                .get_token_by_hash(&token.token_hash)
                .unwrap()
                .expect("token");
            assert_eq!(migrated.label, "laptop");
            assert_eq!(migrated.identity_id, alice_id);

            // Idempotency: re-running on the same destination is a no-op.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.identities, 1);
            assert_eq!(report2.teams, 1);
            assert_eq!(report2.remote_mcp_tokens, 1);
        })
        .await;
    }
}

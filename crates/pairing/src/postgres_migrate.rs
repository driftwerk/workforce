//! One-shot SQLite → Postgres migration for the pairing store.
//!
//! Reads every table from a SQLite `pairing.db` file and writes them
//! into a Postgres backend (after running the refinery migrations
//! against the destination). Driven by the `workforce migrate-pairing`
//! CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (id) DO NOTHING` (or
//! the appropriate unique key) so re-running on a partially-migrated
//! destination is safe.
//!
//! ## Channel allowlist note
//!
//! The SQLite `channel_allowlist` table has a composite UNIQUE
//! constraint on `(platform, sender_id)`. The Postgres translation
//! preserves that, so the migration's `ON CONFLICT (platform, sender_id)`
//! clause matches and re-runs are no-ops for already-migrated rows.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresPairingStore;

#[derive(Debug, Default, Clone)]
pub struct PairingMigrationReport {
    pub channel_pairing_requests: usize,
    pub channel_allowlist: usize,
    pub device_pairing_requests: usize,
    pub approved_devices: usize,
    pub node_pairing_requests: usize,
}

impl PairingMigrationReport {
    pub fn total(&self) -> usize {
        self.channel_pairing_requests
            + self.channel_allowlist
            + self.device_pairing_requests
            + self.approved_devices
            + self.node_pairing_requests
    }
}

pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<PairingMigrationReport> {
    let pool = PostgresPairingStore::build_pool(pg_config)
        .context("building pairing pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<PairingMigrationReport> {
    let _store: Arc<PostgresPairingStore> = PostgresPairingStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres pairing destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let channel_requests = read_channel_requests(&sqlite)?;
    let allowlist = read_allowlist(&sqlite)?;
    let device_requests = read_device_requests(&sqlite)?;
    let approved = read_approved_devices(&sqlite)?;
    let node_requests = read_node_requests(&sqlite)?;
    drop(sqlite);

    Ok(PairingMigrationReport {
        channel_pairing_requests: write_channel_requests(&pool, channel_requests).await?,
        channel_allowlist: write_allowlist(&pool, allowlist).await?,
        device_pairing_requests: write_device_requests(&pool, device_requests).await?,
        approved_devices: write_approved_devices(&pool, approved).await?,
        node_pairing_requests: write_node_requests(&pool, node_requests).await?,
    })
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_dt_opt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref()
        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// `column` when the source table has it, else a `NULL` in its place: a
/// database last opened by an older hub predates the columns added since.
fn column_or_null(sqlite: &Connection, table: &str, column: &'static str) -> Result<String> {
    let present: bool = sqlite.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        rusqlite::params![table, column],
        |r| r.get(0),
    )?;
    Ok(match present {
        true => column.to_string(),
        false => format!("NULL AS {column}"),
    })
}

fn parse_json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!([]))
}

// ─── channel_pairing_requests ───────────────────────────────────────────

struct ChannelReqRow {
    id: String,
    code: String,
    platform: String,
    channel_id: String,
    sender_id: String,
    sender_name: String,
    status: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    approved_by: Option<String>,
    approved_at: Option<DateTime<Utc>>,
}

fn read_channel_requests(sqlite: &Connection) -> Result<Vec<ChannelReqRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, code, platform, channel_id, sender_id, sender_name,
                status, created_at, expires_at, approved_by, approved_at
         FROM channel_pairing_requests",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let created_at: String = r.get(7)?;
            let expires_at: String = r.get(8)?;
            let approved_at: Option<String> = r.get(10)?;
            Ok(ChannelReqRow {
                id: r.get(0)?,
                code: r.get(1)?,
                platform: r.get(2)?,
                channel_id: r.get(3)?,
                sender_id: r.get(4)?,
                sender_name: r.get(5)?,
                status: r.get(6)?,
                created_at: parse_dt(&created_at),
                expires_at: parse_dt(&expires_at),
                approved_by: r.get(9)?,
                approved_at: parse_dt_opt(approved_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_channel_requests(pool: &PostgresPool, rows: Vec<ChannelReqRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO channel_pairing_requests
             (id, code, platform, channel_id, sender_id, sender_name,
              status, created_at, expires_at, approved_by, approved_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.code,
                &r.platform,
                &r.channel_id,
                &r.sender_id,
                &r.sender_name,
                &r.status,
                &r.created_at,
                &r.expires_at,
                &r.approved_by,
                &r.approved_at,
            ],
        )
        .await
        .context("insert channel_pairing_requests row")?;
    }
    Ok(rows.len())
}

// ─── channel_allowlist ──────────────────────────────────────────────────

struct AllowlistRow {
    id: String,
    platform: String,
    channel_id: String,
    sender_id: String,
    sender_name: String,
    added_at: DateTime<Utc>,
    added_by: String,
}

fn read_allowlist(sqlite: &Connection) -> Result<Vec<AllowlistRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, platform, channel_id, sender_id, sender_name, added_at, added_by
         FROM channel_allowlist",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let added_at: String = r.get(5)?;
            Ok(AllowlistRow {
                id: r.get(0)?,
                platform: r.get(1)?,
                channel_id: r.get(2)?,
                sender_id: r.get(3)?,
                sender_name: r.get(4)?,
                added_at: parse_dt(&added_at),
                added_by: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_allowlist(pool: &PostgresPool, rows: Vec<AllowlistRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO channel_allowlist
             (id, platform, channel_id, sender_id, sender_name, added_at, added_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (platform, sender_id) DO NOTHING",
            &[
                &r.id,
                &r.platform,
                &r.channel_id,
                &r.sender_id,
                &r.sender_name,
                &r.added_at,
                &r.added_by,
            ],
        )
        .await
        .context("insert channel_allowlist row")?;
    }
    Ok(rows.len())
}

// ─── device_pairing_requests ────────────────────────────────────────────

struct DeviceReqRow {
    id: String,
    code: String,
    device_name: String,
    public_key: Option<String>,
    scopes: serde_json::Value,
    status: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    approved_by: Option<String>,
    approved_at: Option<DateTime<Utc>>,
    pending_token: Option<String>,
    client_id: Option<String>,
    identity_hint: Option<String>,
    device_kind: Option<String>,
}

fn read_device_requests(sqlite: &Connection) -> Result<Vec<DeviceReqRow>> {
    let identity_hint = column_or_null(sqlite, "device_pairing_requests", "identity_hint")?;
    let device_kind = column_or_null(sqlite, "device_pairing_requests", "device_kind")?;
    let mut stmt = sqlite.prepare(&format!(
        "SELECT id, code, device_name, public_key, scopes, status,
                created_at, expires_at, approved_by, approved_at,
                pending_token, client_id, {identity_hint}, {device_kind}
         FROM device_pairing_requests"
    ))?;
    let rows = stmt
        .query_map([], |r| {
            let scopes: String = r.get(4)?;
            let created_at: String = r.get(6)?;
            let expires_at: String = r.get(7)?;
            let approved_at: Option<String> = r.get(9)?;
            Ok(DeviceReqRow {
                id: r.get(0)?,
                code: r.get(1)?,
                device_name: r.get(2)?,
                public_key: r.get(3)?,
                scopes: parse_json(&scopes),
                status: r.get(5)?,
                created_at: parse_dt(&created_at),
                expires_at: parse_dt(&expires_at),
                approved_by: r.get(8)?,
                approved_at: parse_dt_opt(approved_at),
                pending_token: r.get(10)?,
                client_id: r.get(11)?,
                identity_hint: r.get(12)?,
                device_kind: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_device_requests(pool: &PostgresPool, rows: Vec<DeviceReqRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO device_pairing_requests
             (id, code, device_name, public_key, scopes, status,
              created_at, expires_at, approved_by, approved_at,
              pending_token, client_id, identity_hint, device_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.code,
                &r.device_name,
                &r.public_key,
                &r.scopes,
                &r.status,
                &r.created_at,
                &r.expires_at,
                &r.approved_by,
                &r.approved_at,
                &r.pending_token,
                &r.client_id,
                &r.identity_hint,
                &r.device_kind,
            ],
        )
        .await
        .context("insert device_pairing_requests row")?;
    }
    Ok(rows.len())
}

// ─── approved_devices ───────────────────────────────────────────────────

struct ApprovedDeviceRow {
    id: String,
    device_name: String,
    scopes: serde_json::Value,
    token_hash: String,
    created_at: DateTime<Utc>,
    last_used: Option<DateTime<Utc>>,
    revoked: bool,
    expires_at: Option<DateTime<Utc>>,
    client_id: Option<String>,
    prev_token_hash: Option<String>,
}

fn read_approved_devices(sqlite: &Connection) -> Result<Vec<ApprovedDeviceRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, device_name, scopes, token_hash, created_at, last_used,
                revoked, expires_at, client_id, prev_token_hash
         FROM approved_devices",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let scopes: String = r.get(2)?;
            let created_at: String = r.get(4)?;
            let last_used: Option<String> = r.get(5)?;
            let revoked_int: i64 = r.get(6)?;
            let expires_at: Option<String> = r.get(7)?;
            Ok(ApprovedDeviceRow {
                id: r.get(0)?,
                device_name: r.get(1)?,
                scopes: parse_json(&scopes),
                token_hash: r.get(3)?,
                created_at: parse_dt(&created_at),
                last_used: parse_dt_opt(last_used),
                revoked: revoked_int != 0,
                expires_at: parse_dt_opt(expires_at),
                client_id: r.get(8)?,
                prev_token_hash: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_approved_devices(
    pool: &PostgresPool,
    rows: Vec<ApprovedDeviceRow>,
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO approved_devices
             (id, device_name, scopes, token_hash, created_at, last_used,
              revoked, expires_at, client_id, prev_token_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.device_name,
                &r.scopes,
                &r.token_hash,
                &r.created_at,
                &r.last_used,
                &r.revoked,
                &r.expires_at,
                &r.client_id,
                &r.prev_token_hash,
            ],
        )
        .await
        .context("insert approved_devices row")?;
    }
    Ok(rows.len())
}

// ─── node_pairing_requests ──────────────────────────────────────────────

struct NodeReqRow {
    id: String,
    code: String,
    node_id: String,
    node_name: String,
    node_type: String,
    capabilities: serde_json::Value,
    status: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    approved_by: Option<String>,
    approved_at: Option<DateTime<Utc>>,
}

fn read_node_requests(sqlite: &Connection) -> Result<Vec<NodeReqRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, code, node_id, node_name, node_type, capabilities,
                status, created_at, expires_at, approved_by, approved_at
         FROM node_pairing_requests",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let caps: String = r.get(5)?;
            let created_at: String = r.get(7)?;
            let expires_at: String = r.get(8)?;
            let approved_at: Option<String> = r.get(10)?;
            Ok(NodeReqRow {
                id: r.get(0)?,
                code: r.get(1)?,
                node_id: r.get(2)?,
                node_name: r.get(3)?,
                node_type: r.get(4)?,
                capabilities: parse_json(&caps),
                status: r.get(6)?,
                created_at: parse_dt(&created_at),
                expires_at: parse_dt(&expires_at),
                approved_by: r.get(9)?,
                approved_at: parse_dt_opt(approved_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_node_requests(pool: &PostgresPool, rows: Vec<NodeReqRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO node_pairing_requests
             (id, code, node_id, node_name, node_type, capabilities,
              status, created_at, expires_at, approved_by, approved_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.code,
                &r.node_id,
                &r.node_name,
                &r.node_type,
                &r.capabilities,
                &r.status,
                &r.created_at,
                &r.expires_at,
                &r.approved_by,
                &r.approved_at,
            ],
        )
        .await
        .context("insert node_pairing_requests row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{PairingStore, SqlitePairingStore};
    use crate::types::*;
    use chrono::Duration;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_pairing_round_trip() {
        with_test_pool(PostgresPairingStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqlitePairingStore::open(tmp.path()).unwrap();
            let now = now_micros();

            sqlite
                .create_channel_request(&ChannelPairingRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    code: "ABCD1234".into(),
                    platform: "slack".into(),
                    channel_id: "C123".into(),
                    sender_id: "U001".into(),
                    sender_name: "Test".into(),
                    status: PairingStatus::Pending,
                    created_at: now,
                    expires_at: now + Duration::minutes(15),
                    approved_by: None,
                    approved_at: None,
                })
                .unwrap();
            sqlite
                .add_to_allowlist(&ChannelAllowlistEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    platform: "slack".into(),
                    channel_id: "C123".into(),
                    sender_id: "U002".into(),
                    sender_name: "Allowed".into(),
                    added_at: now,
                    added_by: "manual".into(),
                })
                .unwrap();
            sqlite
                .create_device_request(&DevicePairingRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    code: "DEV12345".into(),
                    device_name: "phone".into(),
                    client_id: Some("client-uuid".into()),
                    public_key: None,
                    scopes: vec!["chat".into()],
                    status: PairingStatus::Pending,
                    created_at: now,
                    expires_at: now + Duration::minutes(5),
                    approved_by: None,
                    approved_at: None,
                    pending_token: None,
                    identity_hint: Some("alice@example.com".into()),
                    device_kind: Some("voice_device".into()),
                })
                .unwrap();
            sqlite
                .register_device(&ApprovedDevice {
                    id: uuid::Uuid::new_v4().to_string(),
                    device_name: "approved".into(),
                    client_id: Some("c2".into()),
                    scopes: vec!["chat".into()],
                    token_hash: "tokhash".into(),
                    created_at: now,
                    last_used: None,
                    revoked: false,
                    expires_at: None,
                })
                .unwrap();
            sqlite
                .create_node_request(&NodePairingRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    code: "NODE1234".into(),
                    node_id: "node-1".into(),
                    node_name: "edge".into(),
                    node_type: "remote".into(),
                    capabilities: vec!["compute".into()],
                    status: PairingStatus::Pending,
                    created_at: now,
                    expires_at: now + Duration::minutes(15),
                    approved_by: None,
                    approved_at: None,
                })
                .unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration");
            assert_eq!(report.channel_pairing_requests, 1);
            assert_eq!(report.channel_allowlist, 1);
            assert_eq!(report.device_pairing_requests, 1);
            assert_eq!(report.approved_devices, 1);
            assert_eq!(report.node_pairing_requests, 1);

            let dest = PostgresPairingStore::with_pool(pool.clone()).await.unwrap();
            let s: Arc<dyn PairingStore> = dest;
            assert!(s.get_channel_request_by_code("ABCD1234").unwrap().is_some());
            assert!(s.is_sender_allowed("slack", "U002").unwrap());
            let device = s.get_device_request_by_code("DEV12345").unwrap().unwrap();
            assert_eq!(
                (
                    device.identity_hint.as_deref(),
                    device.device_kind.as_deref()
                ),
                (Some("alice@example.com"), Some("voice_device")),
                "the approval links the device by what the request carried"
            );
            assert_eq!(s.list_devices().unwrap().len(), 1);
            assert!(s.get_node_request_by_code("NODE1234").unwrap().is_some());

            // Idempotency: re-running on the same destination is a no-op.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.total(), 5);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_device_requests_from_a_database_that_predates_hint_and_kind() {
        with_test_pool(PostgresPairingStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqlitePairingStore::open(tmp.path()).unwrap();
            let now = now_micros();
            sqlite
                .create_device_request(&DevicePairingRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    code: "OLD12345".into(),
                    device_name: "phone".into(),
                    client_id: None,
                    public_key: None,
                    scopes: vec!["chat".into()],
                    status: PairingStatus::Pending,
                    created_at: now,
                    expires_at: now + Duration::minutes(5),
                    approved_by: None,
                    approved_at: None,
                    pending_token: None,
                    identity_hint: None,
                    device_kind: None,
                })
                .unwrap();
            drop(sqlite);
            Connection::open(tmp.path())
                .unwrap()
                .execute_batch(
                    "ALTER TABLE device_pairing_requests DROP COLUMN identity_hint;
                     ALTER TABLE device_pairing_requests DROP COLUMN device_kind;",
                )
                .unwrap();

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("an older source still migrates");
            assert_eq!(report.device_pairing_requests, 1);
            let dest = PostgresPairingStore::with_pool(pool).await.unwrap();
            let device = dest
                .get_device_request_by_code("OLD12345")
                .unwrap()
                .unwrap();
            assert_eq!((device.identity_hint, device.device_kind), (None, None));
        })
        .await;
    }
}

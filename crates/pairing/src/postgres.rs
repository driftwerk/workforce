//! Postgres backend for [`PairingStore`].
//!
//! Phase 4 of the multi-store migration roadmap (see
//! `docs/POSTGRES.md`). Mirrors every method of the SQLite backend at
//! [`crate::store::SqlitePairingStore`]. Selected at runtime via
//! `[pairing] backend = "postgres"` in `workforce.toml`.
//!
//! Tables live in the `pairing` Postgres schema. The pool is built
//! via [`PostgresPairingStore::build_pool`] which installs a
//! `post_create` hook that runs `SET search_path TO pairing, public`
//! on every new connection — so unqualified table names always
//! resolve regardless of which connection deadpool hands out.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::store::{PairingRequestKind, PairingStore};
use crate::types::*;

mod embedded {
    refinery::embed_migrations!("migrations");
}

/// Postgres backend for the pairing store.
pub struct PostgresPairingStore {
    pool: PostgresPool,
}

impl PostgresPairingStore {
    /// Schema this store owns. All pairing tables live under it.
    pub const SCHEMA: &'static str = "pairing";

    /// Build a [`PostgresPool`] suitable for pairing queries: every
    /// connection it hands out has its `search_path` pinned to the
    /// pairing schema.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(config, &["SET search_path TO pairing, public"])
    }

    /// Build a pool from config, run pending migrations, and return a
    /// ready-to-use store.
    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building pairing pool")?;
        Self::with_pool(pool).await
    }

    /// Variant taking a pre-built pool. Caller must ensure the pool's
    /// `search_path` is pinned to the pairing schema.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedPairingMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running pairing migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedPairingMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedPairingMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Row mappers ───────────────────────────────────────────────────────

fn row_to_channel_request(row: &Row) -> Result<ChannelPairingRequest> {
    let status_str: String = row.try_get("status")?;
    Ok(ChannelPairingRequest {
        id: row.try_get("id")?,
        code: row.try_get("code")?,
        platform: row.try_get("platform")?,
        channel_id: row.try_get("channel_id")?,
        sender_id: row.try_get("sender_id")?,
        sender_name: row.try_get("sender_name")?,
        status: PairingStatus::parse(&status_str).unwrap_or(PairingStatus::Pending),
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        approved_by: row.try_get("approved_by")?,
        approved_at: row.try_get("approved_at")?,
    })
}

fn row_to_allowlist(row: &Row) -> Result<ChannelAllowlistEntry> {
    Ok(ChannelAllowlistEntry {
        id: row.try_get("id")?,
        platform: row.try_get("platform")?,
        channel_id: row.try_get("channel_id")?,
        sender_id: row.try_get("sender_id")?,
        sender_name: row.try_get("sender_name")?,
        added_at: row.try_get("added_at")?,
        added_by: row.try_get("added_by")?,
    })
}

fn row_to_device_request(row: &Row) -> Result<DevicePairingRequest> {
    let status_str: String = row.try_get("status")?;
    let scopes_json: serde_json::Value = row.try_get("scopes")?;
    let scopes: Vec<String> = serde_json::from_value(scopes_json).unwrap_or_default();
    Ok(DevicePairingRequest {
        id: row.try_get("id")?,
        code: row.try_get("code")?,
        device_name: row.try_get("device_name")?,
        client_id: row.try_get("client_id")?,
        public_key: row.try_get("public_key")?,
        scopes,
        status: PairingStatus::parse(&status_str).unwrap_or(PairingStatus::Pending),
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        approved_by: row.try_get("approved_by")?,
        approved_at: row.try_get("approved_at")?,
        pending_token: row.try_get("pending_token")?,
        identity_hint: row.try_get("identity_hint")?,
        device_kind: row.try_get("device_kind")?,
    })
}

fn row_to_approved_device(row: &Row) -> Result<ApprovedDevice> {
    let scopes_json: serde_json::Value = row.try_get("scopes")?;
    let scopes: Vec<String> = serde_json::from_value(scopes_json).unwrap_or_default();
    Ok(ApprovedDevice {
        id: row.try_get("id")?,
        device_name: row.try_get("device_name")?,
        client_id: row.try_get("client_id")?,
        scopes,
        token_hash: row.try_get("token_hash")?,
        created_at: row.try_get("created_at")?,
        last_used: row.try_get("last_used")?,
        revoked: row.try_get("revoked")?,
        expires_at: row.try_get("expires_at")?,
    })
}

fn row_to_node_request(row: &Row) -> Result<NodePairingRequest> {
    let status_str: String = row.try_get("status")?;
    let caps_json: serde_json::Value = row.try_get("capabilities")?;
    let capabilities: Vec<String> = serde_json::from_value(caps_json).unwrap_or_default();
    Ok(NodePairingRequest {
        id: row.try_get("id")?,
        code: row.try_get("code")?,
        node_id: row.try_get("node_id")?,
        node_name: row.try_get("node_name")?,
        node_type: row.try_get("node_type")?,
        capabilities,
        status: PairingStatus::parse(&status_str).unwrap_or(PairingStatus::Pending),
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        approved_by: row.try_get("approved_by")?,
        approved_at: row.try_get("approved_at")?,
    })
}

// ─── PairingStore impl ─────────────────────────────────────────────────

impl PairingStore for PostgresPairingStore {
    // ---- Channel pairing ----

    fn create_channel_request(&self, req: &ChannelPairingRequest) -> Result<()> {
        let req = req.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO channel_pairing_requests
                    (id, code, platform, channel_id, sender_id, sender_name,
                     status, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &req.id,
                    &req.code,
                    &req.platform,
                    &req.channel_id,
                    &req.sender_id,
                    &req.sender_name,
                    &req.status.as_str(),
                    &req.created_at,
                    &req.expires_at,
                ],
            )
            .await
            .context("insert channel_pairing_requests")?;
            Ok(())
        })
    }

    fn get_channel_request_by_code(&self, code: &str) -> Result<Option<ChannelPairingRequest>> {
        let code = code.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, code, platform, channel_id, sender_id, sender_name,
                            status, created_at, expires_at, approved_by, approved_at
                     FROM channel_pairing_requests WHERE code = $1",
                    &[&code],
                )
                .await?;
            row.as_ref().map(row_to_channel_request).transpose()
        })
    }

    fn list_pending_channel_requests(&self) -> Result<Vec<ChannelPairingRequest>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, code, platform, channel_id, sender_id, sender_name,
                            status, created_at, expires_at, approved_by, approved_at
                     FROM channel_pairing_requests WHERE status = 'pending'
                     ORDER BY created_at DESC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_channel_request).collect()
        })
    }

    fn update_channel_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let id = id.to_string();
        let approved_by = approved_by.map(|s| s.to_string());
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let approved_at: Option<DateTime<Utc>> = if status == PairingStatus::Approved {
                Some(Utc::now())
            } else {
                None
            };
            conn.execute(
                "UPDATE channel_pairing_requests
                 SET status = $1, approved_by = $2, approved_at = $3
                 WHERE id = $4",
                &[&status.as_str(), &approved_by, &approved_at, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn is_sender_allowed(&self, platform: &str, sender_id: &str) -> Result<bool> {
        let platform = platform.to_string();
        let sender_id = sender_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COUNT(*)::BIGINT FROM channel_allowlist
                     WHERE platform = $1 AND sender_id = $2",
                    &[&platform, &sender_id],
                )
                .await?;
            Ok(row.get::<_, i64>(0) > 0)
        })
    }

    fn add_to_allowlist(&self, entry: &ChannelAllowlistEntry) -> Result<()> {
        let entry = entry.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO channel_allowlist
                    (id, platform, channel_id, sender_id, sender_name, added_at, added_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (platform, sender_id) DO UPDATE SET
                    id = EXCLUDED.id,
                    channel_id = EXCLUDED.channel_id,
                    sender_name = EXCLUDED.sender_name,
                    added_at = EXCLUDED.added_at,
                    added_by = EXCLUDED.added_by",
                &[
                    &entry.id,
                    &entry.platform,
                    &entry.channel_id,
                    &entry.sender_id,
                    &entry.sender_name,
                    &entry.added_at,
                    &entry.added_by,
                ],
            )
            .await
            .context("upsert channel_allowlist")?;
            Ok(())
        })
    }

    fn remove_from_allowlist(&self, platform: &str, sender_id: &str) -> Result<bool> {
        let platform = platform.to_string();
        let sender_id = sender_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "DELETE FROM channel_allowlist WHERE platform = $1 AND sender_id = $2",
                    &[&platform, &sender_id],
                )
                .await?;
            Ok(n > 0)
        })
    }

    fn list_allowlist(&self, platform: Option<&str>) -> Result<Vec<ChannelAllowlistEntry>> {
        let platform = platform.map(|s| s.to_string());
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = if let Some(p) = platform {
                conn.query(
                    "SELECT id, platform, channel_id, sender_id, sender_name, added_at, added_by
                     FROM channel_allowlist WHERE platform = $1
                     ORDER BY added_at DESC",
                    &[&p],
                )
                .await?
            } else {
                conn.query(
                    "SELECT id, platform, channel_id, sender_id, sender_name, added_at, added_by
                     FROM channel_allowlist
                     ORDER BY added_at DESC",
                    &[],
                )
                .await?
            };
            rows.iter().map(row_to_allowlist).collect()
        })
    }

    fn expire_stale_requests(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let c = conn
                .execute(
                    "UPDATE channel_pairing_requests SET status = 'expired'
                     WHERE status = 'pending' AND expires_at < $1",
                    &[&cutoff],
                )
                .await?;
            let d = conn
                .execute(
                    "UPDATE device_pairing_requests SET status = 'expired'
                     WHERE status = 'pending' AND expires_at < $1",
                    &[&cutoff],
                )
                .await?;
            let n = conn
                .execute(
                    "UPDATE node_pairing_requests SET status = 'expired'
                     WHERE status = 'pending' AND expires_at < $1",
                    &[&cutoff],
                )
                .await?;
            Ok((c + d + n) as usize)
        })
    }

    fn count_pending(&self) -> Result<usize> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT
                        (SELECT COUNT(*) FROM channel_pairing_requests WHERE status = 'pending') +
                        (SELECT COUNT(*) FROM device_pairing_requests WHERE status = 'pending') +
                        (SELECT COUNT(*) FROM node_pairing_requests WHERE status = 'pending')",
                    &[],
                )
                .await?;
            Ok(row.get::<_, i64>(0) as usize)
        })
    }

    // ---- Device pairing ----

    fn create_device_request(&self, req: &DevicePairingRequest) -> Result<()> {
        let req = req.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let scopes = serde_json::to_value(&req.scopes)?;
            conn.execute(
                "INSERT INTO device_pairing_requests
                    (id, code, device_name, public_key, scopes, status,
                     created_at, expires_at, pending_token, client_id, identity_hint,
                     device_kind)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &req.id,
                    &req.code,
                    &req.device_name,
                    &req.public_key,
                    &scopes,
                    &req.status.as_str(),
                    &req.created_at,
                    &req.expires_at,
                    &req.pending_token,
                    &req.client_id,
                    &req.identity_hint,
                    &req.device_kind,
                ],
            )
            .await
            .context("insert device_pairing_requests")?;
            Ok(())
        })
    }

    fn get_device_request_by_code(&self, code: &str) -> Result<Option<DevicePairingRequest>> {
        let code = code.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, code, device_name, public_key, scopes, status,
                            created_at, expires_at, approved_by, approved_at,
                            pending_token, client_id, identity_hint, device_kind
                     FROM device_pairing_requests WHERE code = $1",
                    &[&code],
                )
                .await?;
            row.as_ref().map(row_to_device_request).transpose()
        })
    }

    fn list_pending_device_requests(&self) -> Result<Vec<DevicePairingRequest>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, code, device_name, public_key, scopes, status,
                            created_at, expires_at, approved_by, approved_at,
                            pending_token, client_id, identity_hint, device_kind
                     FROM device_pairing_requests WHERE status = 'pending'
                     ORDER BY created_at DESC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_device_request).collect()
        })
    }

    fn update_device_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let id = id.to_string();
        let approved_by = approved_by.map(|s| s.to_string());
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let approved_at: Option<DateTime<Utc>> = if status == PairingStatus::Approved {
                Some(Utc::now())
            } else {
                None
            };
            conn.execute(
                "UPDATE device_pairing_requests
                 SET status = $1, approved_by = $2, approved_at = $3
                 WHERE id = $4",
                &[&status.as_str(), &approved_by, &approved_at, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn register_device(&self, device: &ApprovedDevice) -> Result<()> {
        let device = device.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let scopes = serde_json::to_value(&device.scopes)?;
            conn.execute(
                "INSERT INTO approved_devices
                    (id, device_name, scopes, token_hash, created_at, revoked,
                     expires_at, client_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (id) DO UPDATE SET
                    device_name = EXCLUDED.device_name,
                    scopes = EXCLUDED.scopes,
                    token_hash = EXCLUDED.token_hash,
                    prev_token_hash = NULL,
                    created_at = EXCLUDED.created_at,
                    revoked = EXCLUDED.revoked,
                    expires_at = EXCLUDED.expires_at,
                    client_id = EXCLUDED.client_id",
                &[
                    &device.id,
                    &device.device_name,
                    &scopes,
                    &device.token_hash,
                    &device.created_at,
                    &device.revoked,
                    &device.expires_at,
                    &device.client_id,
                ],
            )
            .await
            .context("upsert approved_devices")?;
            Ok(())
        })
    }

    fn get_device_by_id(&self, id: &str) -> Result<Option<ApprovedDevice>> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, device_name, scopes, token_hash, created_at, last_used,
                            revoked, expires_at, client_id
                     FROM approved_devices WHERE id = $1",
                    &[&id],
                )
                .await?;
            row.as_ref().map(row_to_approved_device).transpose()
        })
    }

    fn validate_device_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(ApprovedDevice, TokenMatch)>> {
        let token_hash_owned = token_hash.to_string();
        let device = self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, device_name, scopes, token_hash, created_at, last_used,
                            revoked, expires_at, client_id
                     FROM approved_devices
                     WHERE (token_hash = $1 OR prev_token_hash = $1) AND revoked = FALSE",
                    &[&token_hash_owned],
                )
                .await?;
            row.as_ref().map(row_to_approved_device).transpose()
        })?;
        Ok(device.map(|d| {
            let kind = if d.token_hash == token_hash {
                TokenMatch::Current
            } else {
                TokenMatch::Previous
            };
            (d, kind)
        }))
    }

    fn revoke_device(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE approved_devices SET revoked = TRUE WHERE id = $1",
                &[&id],
            )
            .await?;
            Ok(())
        })
    }

    fn normalize_device_scopes(&self, canonical: &[String]) -> Result<usize> {
        let canonical = canonical.to_vec();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let scopes = serde_json::to_value(&canonical)?;
            let devices = conn
                .execute(
                    "UPDATE approved_devices SET scopes = $1 WHERE revoked = FALSE AND scopes <> $1",
                    &[&scopes],
                )
                .await?;
            // Pending requests are healed too: approval mints a device from the
            // request's stored scopes, so a request that pre-dates this rule must
            // not grant a stale surface when an operator approves it post-restart.
            let pending = conn
                .execute(
                    "UPDATE device_pairing_requests SET scopes = $1 WHERE status = 'pending' AND scopes <> $1",
                    &[&scopes],
                )
                .await?;
            Ok((devices + pending) as usize)
        })
    }

    fn list_devices(&self) -> Result<Vec<ApprovedDevice>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, device_name, scopes, token_hash, created_at, last_used,
                            revoked, expires_at, client_id
                     FROM approved_devices
                     WHERE expires_at IS NULL OR expires_at > NOW()
                     ORDER BY created_at DESC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_approved_device).collect()
        })
    }

    fn update_device_last_used(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let now = Utc::now();
            conn.execute(
                "UPDATE approved_devices SET last_used = $1 WHERE id = $2",
                &[&now, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn rotate_device_token(
        &self,
        id: &str,
        new_token_hash: &str,
        new_expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let id = id.to_string();
        let new_token_hash = new_token_hash.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "UPDATE approved_devices
                     SET prev_token_hash = token_hash,
                         token_hash = $1,
                         expires_at = $2
                     WHERE id = $3 AND revoked = FALSE",
                    &[&new_token_hash, &new_expires_at, &id],
                )
                .await?;
            if n == 0 {
                return Err(eyre::eyre!("Device not found or already revoked: {id}"));
            }
            Ok(())
        })
    }

    fn set_device_pending_token(&self, id: &str, token: &str) -> Result<()> {
        let id = id.to_string();
        let token = token.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE device_pairing_requests SET pending_token = $1 WHERE id = $2",
                &[&token, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn clear_device_pending_token(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE device_pairing_requests SET pending_token = NULL WHERE id = $1",
                &[&id],
            )
            .await?;
            Ok(())
        })
    }

    fn find_device_by_client_id(&self, client_id: &str) -> Result<Option<ApprovedDevice>> {
        let client_id = client_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, device_name, scopes, token_hash, created_at, last_used,
                            revoked, expires_at, client_id
                     FROM approved_devices
                     WHERE client_id = $1 AND revoked = FALSE
                     ORDER BY created_at DESC LIMIT 1",
                    &[&client_id],
                )
                .await?;
            row.as_ref().map(row_to_approved_device).transpose()
        })
    }

    // ---- Node pairing ----

    fn create_node_request(&self, req: &NodePairingRequest) -> Result<()> {
        let req = req.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let caps = serde_json::to_value(&req.capabilities)?;
            conn.execute(
                "INSERT INTO node_pairing_requests
                    (id, code, node_id, node_name, node_type, capabilities,
                     status, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &req.id,
                    &req.code,
                    &req.node_id,
                    &req.node_name,
                    &req.node_type,
                    &caps,
                    &req.status.as_str(),
                    &req.created_at,
                    &req.expires_at,
                ],
            )
            .await
            .context("insert node_pairing_requests")?;
            Ok(())
        })
    }

    fn get_node_request_by_code(&self, code: &str) -> Result<Option<NodePairingRequest>> {
        let code = code.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, code, node_id, node_name, node_type, capabilities,
                            status, created_at, expires_at, approved_by, approved_at
                     FROM node_pairing_requests WHERE code = $1",
                    &[&code],
                )
                .await?;
            row.as_ref().map(row_to_node_request).transpose()
        })
    }

    fn list_pending_node_requests(&self) -> Result<Vec<NodePairingRequest>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, code, node_id, node_name, node_type, capabilities,
                            status, created_at, expires_at, approved_by, approved_at
                     FROM node_pairing_requests WHERE status = 'pending'
                     ORDER BY created_at DESC",
                    &[],
                )
                .await?;
            rows.iter().map(row_to_node_request).collect()
        })
    }

    fn update_node_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let id = id.to_string();
        let approved_by = approved_by.map(|s| s.to_string());
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let approved_at: Option<DateTime<Utc>> = if status == PairingStatus::Approved {
                Some(Utc::now())
            } else {
                None
            };
            conn.execute(
                "UPDATE node_pairing_requests
                 SET status = $1, approved_by = $2, approved_at = $3
                 WHERE id = $4",
                &[&status.as_str(), &approved_by, &approved_at, &id],
            )
            .await?;
            Ok(())
        })
    }

    fn is_node_approved(&self, node_id: &str) -> Result<bool> {
        let node_id = node_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COUNT(*)::BIGINT FROM node_pairing_requests
                     WHERE node_id = $1 AND status = 'approved'",
                    &[&node_id],
                )
                .await?;
            Ok(row.get::<_, i64>(0) > 0)
        })
    }

    // ---- Generic code lookup ----

    fn get_any_request_by_code(&self, code: &str) -> Result<Option<PairingRequestKind>> {
        if let Some(r) = self.get_channel_request_by_code(code)? {
            return Ok(Some(PairingRequestKind::Channel(r)));
        }
        if let Some(r) = self.get_device_request_by_code(code)? {
            return Ok(Some(PairingRequestKind::Device(r)));
        }
        if let Some(r) = self.get_node_request_by_code(code)? {
            return Ok(Some(PairingRequestKind::Node(r)));
        }
        Ok(None)
    }
}

// ─── Parity tests against embedded Postgres ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresPairingStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresPairingStore::build_pool, |pool| async move {
            let store = PostgresPairingStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    fn make_channel_request(code: &str, sender_id: &str) -> ChannelPairingRequest {
        let now = now_micros();
        ChannelPairingRequest {
            id: uuid::Uuid::new_v4().to_string(),
            code: code.to_string(),
            platform: "slack".to_string(),
            channel_id: "C123".to_string(),
            sender_id: sender_id.to_string(),
            sender_name: "Test User".to_string(),
            status: PairingStatus::Pending,
            created_at: now,
            expires_at: now + Duration::minutes(15),
            approved_by: None,
            approved_at: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_channel_pairing_roundtrip() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;

            let req = make_channel_request("ABCD1234", "U001");
            s.create_channel_request(&req).unwrap();

            let found = s
                .get_channel_request_by_code("ABCD1234")
                .unwrap()
                .expect("found");
            assert_eq!(found.sender_id, "U001");
            assert_eq!(found.status, PairingStatus::Pending);
            assert_eq!(s.list_pending_channel_requests().unwrap().len(), 1);

            s.update_channel_request_status(&req.id, PairingStatus::Approved, Some("admin"))
                .unwrap();
            let after = s.get_channel_request_by_code("ABCD1234").unwrap().unwrap();
            assert_eq!(after.status, PairingStatus::Approved);
            assert_eq!(after.approved_by.as_deref(), Some("admin"));
            assert!(after.approved_at.is_some());
            assert_eq!(s.list_pending_channel_requests().unwrap().len(), 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_device_requests_keep_their_identity_hint_and_kind() {
        with_embedded_pg(|store| async move {
            crate::store::device_request_contract(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_allowlist_operations() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;

            assert!(!s.is_sender_allowed("slack", "U001").unwrap());

            let entry = ChannelAllowlistEntry {
                id: uuid::Uuid::new_v4().to_string(),
                platform: "slack".to_string(),
                channel_id: "C123".to_string(),
                sender_id: "U001".to_string(),
                sender_name: "Test".to_string(),
                added_at: now_micros(),
                added_by: "pairing:ABCD1234".to_string(),
            };
            s.add_to_allowlist(&entry).unwrap();
            assert!(s.is_sender_allowed("slack", "U001").unwrap());

            let list = s.list_allowlist(Some("slack")).unwrap();
            assert_eq!(list.len(), 1);

            let entry2 = ChannelAllowlistEntry {
                id: uuid::Uuid::new_v4().to_string(),
                sender_name: "Renamed".to_string(),
                added_by: "manual".to_string(),
                added_at: now_micros(),
                ..entry.clone()
            };
            s.add_to_allowlist(&entry2).unwrap();
            let list = s.list_allowlist(Some("slack")).unwrap();
            assert_eq!(list.len(), 1, "upsert must not duplicate");
            assert_eq!(list[0].sender_name, "Renamed");
            assert_eq!(list[0].added_by, "manual");

            assert!(s.remove_from_allowlist("slack", "U001").unwrap());
            assert!(!s.is_sender_allowed("slack", "U001").unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_device_pairing_lifecycle() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;
            let now = now_micros();
            let req = DevicePairingRequest {
                id: uuid::Uuid::new_v4().to_string(),
                code: "DEVX5678".to_string(),
                device_name: "my-app".to_string(),
                client_id: Some("client-uuid".to_string()),
                public_key: None,
                scopes: vec!["chat".to_string(), "sessions".to_string()],
                status: PairingStatus::Pending,
                created_at: now,
                expires_at: now + Duration::minutes(5),
                approved_by: None,
                approved_at: None,
                pending_token: None,
                identity_hint: None,
                device_kind: None,
            };
            s.create_device_request(&req).unwrap();

            let found = s
                .get_device_request_by_code("DEVX5678")
                .unwrap()
                .expect("found");
            assert_eq!(found.scopes, vec!["chat", "sessions"]);
            assert_eq!(found.client_id.as_deref(), Some("client-uuid"));

            s.set_device_pending_token(&req.id, "tok-plain").unwrap();
            let after = s.get_device_request_by_code("DEVX5678").unwrap().unwrap();
            assert_eq!(after.pending_token.as_deref(), Some("tok-plain"));

            s.update_device_request_status(&req.id, PairingStatus::Approved, Some("admin"))
                .unwrap();

            let device = ApprovedDevice {
                id: req.id.clone(),
                device_name: "my-app".to_string(),
                client_id: Some("client-uuid".to_string()),
                scopes: req.scopes.clone(),
                token_hash: "abc123hash".to_string(),
                created_at: now_micros(),
                last_used: None,
                revoked: false,
                expires_at: None,
            };
            s.register_device(&device).unwrap();

            let (validated, kind) = s
                .validate_device_token_hash("abc123hash")
                .unwrap()
                .expect("found");
            assert_eq!(validated.device_name, "my-app");
            assert_eq!(kind, TokenMatch::Current);

            // Rotation keeps the previous hash valid within the grace window.
            s.rotate_device_token(&device.id, "newhash", None).unwrap();
            assert!(s.validate_device_token_hash("newhash").unwrap().is_some());
            assert!(s
                .validate_device_token_hash("abc123hash")
                .unwrap()
                .is_some());

            // A second rotation retires the oldest hash: only the newest two
            // generations validate.
            s.rotate_device_token(&device.id, "newesthash", None)
                .unwrap();
            assert!(s
                .validate_device_token_hash("newesthash")
                .unwrap()
                .is_some());
            assert!(s.validate_device_token_hash("newhash").unwrap().is_some());
            assert!(s
                .validate_device_token_hash("abc123hash")
                .unwrap()
                .is_none());

            let by_client = s
                .find_device_by_client_id("client-uuid")
                .unwrap()
                .expect("found");
            assert_eq!(by_client.id, device.id);

            s.revoke_device(&device.id).unwrap();
            assert!(s
                .validate_device_token_hash("newesthash")
                .unwrap()
                .is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_normalize_device_scopes_heals_devices_and_pending_requests() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;
            let canonical: Vec<String> =
                ["chat", "sessions", "agents", "pair", "heartbeat", "models"]
                    .map(String::from)
                    .to_vec();
            let now = now_micros();
            let device = |name: &str, scopes: Vec<String>, revoked: bool| ApprovedDevice {
                id: format!("id-{name}"),
                device_name: name.to_string(),
                client_id: None,
                scopes,
                token_hash: format!("tok-{name}"),
                created_at: now,
                last_used: None,
                revoked,
                expires_at: None,
            };
            s.register_device(&device("wild", vec!["*".to_string()], false))
                .unwrap();
            s.register_device(&device("ok", canonical.clone(), false))
                .unwrap();
            s.register_device(&device("revoked", vec!["*".to_string()], true))
                .unwrap();
            let request = |code: &str, status: PairingStatus| DevicePairingRequest {
                id: format!("req-{code}"),
                code: code.to_string(),
                device_name: code.to_string(),
                public_key: None,
                scopes: vec!["*".to_string()],
                status,
                created_at: now,
                expires_at: now + Duration::minutes(5),
                approved_by: None,
                approved_at: None,
                pending_token: None,
                client_id: None,
                identity_hint: None,
                device_kind: None,
            };
            s.create_device_request(&request("pend", PairingStatus::Pending))
                .unwrap();
            s.create_device_request(&request("done", PairingStatus::Approved))
                .unwrap();

            // The JSONB `<>` path heals the wildcard device + pending request;
            // the canonical device, the revoked device, and the terminal
            // request are untouched. Pins parity with the SQLite text path.
            assert_eq!(s.normalize_device_scopes(&canonical).unwrap(), 2);
            assert_eq!(
                s.get_device_by_id("id-wild").unwrap().unwrap().scopes,
                canonical
            );
            assert_eq!(
                s.get_device_by_id("id-revoked").unwrap().unwrap().scopes,
                vec!["*".to_string()]
            );
            assert_eq!(
                s.get_device_request_by_code("pend")
                    .unwrap()
                    .unwrap()
                    .scopes,
                canonical
            );
            assert_eq!(
                s.get_device_request_by_code("done")
                    .unwrap()
                    .unwrap()
                    .scopes,
                vec!["*".to_string()]
            );
            assert_eq!(s.normalize_device_scopes(&canonical).unwrap(), 0);
        })
        .await;
    }

    /// Re-registering the same device id (re-pair path) must reset
    /// `prev_token_hash` — the SQLite store gets this for free via
    /// `INSERT OR REPLACE`, but the Postgres `ON CONFLICT DO UPDATE` clause
    /// needs an explicit assignment.
    #[tokio::test(flavor = "multi_thread")]
    async fn parity_register_device_clears_prev_token_hash_on_repair() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;
            let device = ApprovedDevice {
                id: uuid::Uuid::new_v4().to_string(),
                device_name: "phone".to_string(),
                client_id: Some("client-1".to_string()),
                scopes: vec!["chat".to_string()],
                token_hash: "tok_a".to_string(),
                created_at: now_micros(),
                last_used: None,
                revoked: false,
                expires_at: None,
            };
            s.register_device(&device).unwrap();
            s.rotate_device_token(&device.id, "tok_b", None).unwrap();
            assert!(s.validate_device_token_hash("tok_a").unwrap().is_some());

            let repaired = ApprovedDevice {
                token_hash: "tok_c".to_string(),
                created_at: now_micros(),
                ..device.clone()
            };
            s.register_device(&repaired).unwrap();

            assert!(
                s.validate_device_token_hash("tok_a").unwrap().is_none(),
                "stale prev_token_hash must not validate after re-pair"
            );
            assert!(
                s.validate_device_token_hash("tok_b").unwrap().is_none(),
                "pre-rotation token_hash must not validate after re-pair"
            );
            assert!(s.validate_device_token_hash("tok_c").unwrap().is_some());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_validate_returns_correct_token_match_slot() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;
            let device = ApprovedDevice {
                id: uuid::Uuid::new_v4().to_string(),
                device_name: "phone".to_string(),
                client_id: Some("client-1".to_string()),
                scopes: vec!["chat".to_string()],
                token_hash: "tok_a".to_string(),
                created_at: now_micros(),
                last_used: None,
                revoked: false,
                expires_at: None,
            };
            s.register_device(&device).unwrap();

            let (_, kind) = s
                .validate_device_token_hash("tok_a")
                .unwrap()
                .expect("matched");
            assert_eq!(kind, TokenMatch::Current);

            s.rotate_device_token(&device.id, "tok_b", None).unwrap();

            let (_, kind_new) = s
                .validate_device_token_hash("tok_b")
                .unwrap()
                .expect("matched new");
            assert_eq!(kind_new, TokenMatch::Current);

            let (_, kind_old) = s
                .validate_device_token_hash("tok_a")
                .unwrap()
                .expect("matched old");
            assert_eq!(kind_old, TokenMatch::Previous);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_node_pairing_and_expire_stale() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn PairingStore> = store;
            let now = now_micros();
            let node_req = NodePairingRequest {
                id: uuid::Uuid::new_v4().to_string(),
                code: "NODE1234".to_string(),
                node_id: "node-1".to_string(),
                node_name: "edge-1".to_string(),
                node_type: "remote".to_string(),
                capabilities: vec!["compute".to_string(), "storage".to_string()],
                status: PairingStatus::Pending,
                created_at: now,
                expires_at: now + Duration::minutes(15),
                approved_by: None,
                approved_at: None,
            };
            s.create_node_request(&node_req).unwrap();

            assert!(!s.is_node_approved("node-1").unwrap());
            s.update_node_request_status(&node_req.id, PairingStatus::Approved, Some("admin"))
                .unwrap();
            assert!(s.is_node_approved("node-1").unwrap());

            let stale = ChannelPairingRequest {
                id: uuid::Uuid::new_v4().to_string(),
                code: "EXPD0001".to_string(),
                platform: "slack".to_string(),
                channel_id: "C123".to_string(),
                sender_id: "U002".to_string(),
                sender_name: "Old User".to_string(),
                status: PairingStatus::Pending,
                created_at: now - Duration::hours(2),
                expires_at: now - Duration::hours(1),
                approved_by: None,
                approved_at: None,
            };
            s.create_channel_request(&stale).unwrap();
            let n = s.expire_stale_requests(now).unwrap();
            assert!(n >= 1);
            assert!(s.list_pending_channel_requests().unwrap().is_empty());

            match s.get_any_request_by_code("NODE1234").unwrap() {
                Some(PairingRequestKind::Node(r)) => assert_eq!(r.node_id, "node-1"),
                other => panic!("expected Node, got {}", other.is_some()),
            }
            assert!(s.get_any_request_by_code("NOTEXIST").unwrap().is_none());
        })
        .await;
    }
}

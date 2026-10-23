use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::Result;
use rusqlite::{params, Connection};

use crate::types::*;

/// Trait for pairing state persistence.
pub trait PairingStore: Send + Sync {
    // -- Channel pairing --

    fn create_channel_request(&self, req: &ChannelPairingRequest) -> Result<()>;
    fn get_channel_request_by_code(&self, code: &str) -> Result<Option<ChannelPairingRequest>>;
    fn list_pending_channel_requests(&self) -> Result<Vec<ChannelPairingRequest>>;
    fn update_channel_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()>;
    fn is_sender_allowed(&self, platform: &str, sender_id: &str) -> Result<bool>;
    fn add_to_allowlist(&self, entry: &ChannelAllowlistEntry) -> Result<()>;
    fn remove_from_allowlist(&self, platform: &str, sender_id: &str) -> Result<bool>;
    fn list_allowlist(&self, platform: Option<&str>) -> Result<Vec<ChannelAllowlistEntry>>;
    fn expire_stale_requests(&self, cutoff: DateTime<Utc>) -> Result<usize>;
    fn count_pending(&self) -> Result<usize>;

    // -- Device pairing --

    fn create_device_request(&self, req: &DevicePairingRequest) -> Result<()>;
    fn get_device_request_by_code(&self, code: &str) -> Result<Option<DevicePairingRequest>>;
    fn list_pending_device_requests(&self) -> Result<Vec<DevicePairingRequest>>;
    fn update_device_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()>;
    fn register_device(&self, device: &ApprovedDevice) -> Result<()>;
    fn get_device_by_id(&self, id: &str) -> Result<Option<ApprovedDevice>>;
    /// Look up a device by hashed bearer token. Returns the device plus
    /// [`TokenMatch`] indicating which column matched — `Current` for the
    /// active `token_hash`, `Previous` for the historic `prev_token_hash`
    /// recovery slot. Callers that perform mutations conditioned on the
    /// match kind (rotation, in particular) rely on this distinction.
    fn validate_device_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(ApprovedDevice, TokenMatch)>>;
    fn revoke_device(&self, id: &str) -> Result<()>;
    fn list_devices(&self) -> Result<Vec<ApprovedDevice>>;
    fn update_device_last_used(&self, id: &str) -> Result<()>;
    fn rotate_device_token(
        &self,
        id: &str,
        new_token_hash: &str,
        new_expires_at: Option<DateTime<Utc>>,
    ) -> Result<()>;
    fn set_device_pending_token(&self, id: &str, token: &str) -> Result<()>;
    fn clear_device_pending_token(&self, id: &str) -> Result<()>;
    fn find_device_by_client_id(&self, client_id: &str) -> Result<Option<ApprovedDevice>>;
    /// Re-scope every non-revoked device and every pending pairing request
    /// whose scopes differ from `canonical` to exactly `canonical`, returning
    /// the number of rows changed. Device tokens are server-authoritative, so
    /// this heals legacy wildcard grants down and pre-enforcement partial
    /// grants up — including requests not yet approved — without a re-pair.
    fn normalize_device_scopes(&self, canonical: &[String]) -> Result<usize>;

    // -- Node pairing --

    fn create_node_request(&self, req: &NodePairingRequest) -> Result<()>;
    fn get_node_request_by_code(&self, code: &str) -> Result<Option<NodePairingRequest>>;
    fn list_pending_node_requests(&self) -> Result<Vec<NodePairingRequest>>;
    fn update_node_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()>;
    fn is_node_approved(&self, node_id: &str) -> Result<bool>;

    // -- Generic (code lookup across all types) --

    fn get_any_request_by_code(&self, code: &str) -> Result<Option<PairingRequestKind>>;
}

/// Which kind of request a code resolves to.
#[derive(Debug)]
pub enum PairingRequestKind {
    Channel(ChannelPairingRequest),
    Device(DevicePairingRequest),
    Node(NodePairingRequest),
}

/// What every backend must keep of a device request: the hint and kind the
/// approval links the device by, through every read and a status change.
#[cfg(test)]
pub(crate) fn device_request_contract(store: &dyn PairingStore) {
    let now = workforce_postgres::test_support::now_micros();
    let request =
        |code: &str, identity_hint: Option<&str>, device_kind: Option<&str>| DevicePairingRequest {
            id: uuid::Uuid::new_v4().to_string(),
            code: code.to_string(),
            device_name: code.to_string(),
            client_id: None,
            public_key: None,
            scopes: vec!["chat".to_string()],
            status: PairingStatus::Pending,
            created_at: now,
            expires_at: now + chrono::Duration::minutes(5),
            approved_by: None,
            approved_at: None,
            pending_token: None,
            identity_hint: identity_hint.map(str::to_string),
            device_kind: device_kind.map(str::to_string),
        };
    let voice = request("VOICE001", Some("alice@example.com"), Some("voice_device"));
    let legacy = request("LEGACY01", None, None);
    store.create_device_request(&voice).unwrap();
    store.create_device_request(&legacy).unwrap();

    let kept = |r: &DevicePairingRequest| (r.identity_hint.clone(), r.device_kind.clone());
    let expected = (
        Some("alice@example.com".to_string()),
        Some("voice_device".to_string()),
    );
    let found = store
        .get_device_request_by_code("VOICE001")
        .unwrap()
        .unwrap();
    assert_eq!(kept(&found), expected);
    let pending = store.list_pending_device_requests().unwrap();
    let listed = pending.iter().find(|r| r.code == "VOICE001").unwrap();
    assert_eq!(kept(listed), expected);
    match store.get_any_request_by_code("VOICE001").unwrap() {
        Some(PairingRequestKind::Device(r)) => assert_eq!(kept(&r), expected),
        other => panic!("expected a device request, got {other:?}"),
    }
    let found = store
        .get_device_request_by_code("LEGACY01")
        .unwrap()
        .unwrap();
    assert_eq!(kept(&found), (None, None), "nothing is invented");

    store
        .update_device_request_status(&voice.id, PairingStatus::Approved, Some("admin"))
        .unwrap();
    let approved = store
        .get_device_request_by_code("VOICE001")
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, PairingStatus::Approved);
    assert_eq!(kept(&approved), expected);
}

/// SQLite-backed pairing store.
pub struct SqlitePairingStore {
    conn: Mutex<Connection>,
}

impl SqlitePairingStore {
    /// Open (or create) a pairing database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS channel_pairing_requests (
                id TEXT PRIMARY KEY,
                code TEXT NOT NULL UNIQUE,
                platform TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                approved_by TEXT,
                approved_at TEXT
            );

            CREATE TABLE IF NOT EXISTS channel_allowlist (
                id TEXT PRIMARY KEY,
                platform TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                added_at TEXT NOT NULL,
                added_by TEXT NOT NULL,
                UNIQUE(platform, sender_id)
            );

            CREATE TABLE IF NOT EXISTS device_pairing_requests (
                id TEXT PRIMARY KEY,
                code TEXT NOT NULL UNIQUE,
                device_name TEXT NOT NULL,
                public_key TEXT,
                scopes TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                approved_by TEXT,
                approved_at TEXT
            );

            CREATE TABLE IF NOT EXISTS approved_devices (
                id TEXT PRIMARY KEY,
                device_name TEXT NOT NULL,
                scopes TEXT NOT NULL,
                token_hash TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                last_used TEXT,
                revoked INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS node_pairing_requests (
                id TEXT PRIMARY KEY,
                code TEXT NOT NULL UNIQUE,
                node_id TEXT NOT NULL,
                node_name TEXT NOT NULL,
                node_type TEXT NOT NULL,
                capabilities TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                approved_by TEXT,
                approved_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_channel_allowlist_lookup
                ON channel_allowlist(platform, sender_id);
            CREATE INDEX IF NOT EXISTS idx_device_token_hash
                ON approved_devices(token_hash);
            CREATE INDEX IF NOT EXISTS idx_channel_requests_status
                ON channel_pairing_requests(status);
            CREATE INDEX IF NOT EXISTS idx_node_requests_node_id
                ON node_pairing_requests(node_id, status);
            ",
        )?;

        // Idempotent migration: add expires_at to approved_devices (DW-181).
        let _ = conn.execute_batch("ALTER TABLE approved_devices ADD COLUMN expires_at TEXT;");

        // Idempotent migration: add pending_token to device_pairing_requests (self-service pairing).
        let _ = conn
            .execute_batch("ALTER TABLE device_pairing_requests ADD COLUMN pending_token TEXT;");

        // Idempotent migration: add client_id to device_pairing_requests and approved_devices.
        let _ =
            conn.execute_batch("ALTER TABLE device_pairing_requests ADD COLUMN client_id TEXT;");
        let _ = conn.execute_batch("ALTER TABLE approved_devices ADD COLUMN client_id TEXT;");

        // Idempotent migration: add prev_token_hash for rotation grace period.
        // When a token is rotated, the old hash is kept here so the client can still
        // authenticate if it never received the rotation response.
        let _ = conn.execute_batch("ALTER TABLE approved_devices ADD COLUMN prev_token_hash TEXT;");

        // Idempotent migration: add identity_hint to device_pairing_requests.
        // The hint (email or identity UUID) is captured at pairing-initiation
        // time so the gateway can auto-link the device to the right identity
        // on approval — removing the separate JWT login step.
        let _ = conn
            .execute_batch("ALTER TABLE device_pairing_requests ADD COLUMN identity_hint TEXT;");
        // Idempotent migration: add device_kind to device_pairing_requests.
        let _ =
            conn.execute_batch("ALTER TABLE device_pairing_requests ADD COLUMN device_kind TEXT;");

        // Idempotent backfill: rows that pre-date device-token scope
        // enforcement have `scopes = '[]'` and would be denied access by
        // every scope-gated REST route after the `has_scope` semantic flip.
        // Replace empty arrays with `["*"]` so existing devices keep their
        // pre-enforcement surface; new pairings must declare scopes
        // explicitly. Mirrors `V003__backfill_device_scopes.sql` for the
        // Postgres path.
        let _ = conn.execute_batch(
            "UPDATE approved_devices SET scopes = '[\"*\"]' WHERE scopes = '[]';
             UPDATE device_pairing_requests SET scopes = '[\"*\"]' WHERE scopes = '[]';",
        );

        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {e}"))
    }
}

// Helper to parse a chrono DateTime from a SQLite TEXT column.
fn parse_dt(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_default()
}

fn parse_optional_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|v| v.parse::<DateTime<Utc>>().ok())
}

impl PairingStore for SqlitePairingStore {
    // ---- Channel pairing ----

    fn create_channel_request(&self, req: &ChannelPairingRequest) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO channel_pairing_requests
                (id, code, platform, channel_id, sender_id, sender_name, status, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                req.id,
                req.code,
                req.platform,
                req.channel_id,
                req.sender_id,
                req.sender_name,
                req.status.as_str(),
                req.created_at.to_rfc3339(),
                req.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn get_channel_request_by_code(&self, code: &str) -> Result<Option<ChannelPairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, platform, channel_id, sender_id, sender_name,
                    status, created_at, expires_at, approved_by, approved_at
             FROM channel_pairing_requests WHERE code = ?1",
        )?;
        let mut rows = stmt.query_map(params![code], |row| {
            Ok(ChannelPairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                platform: row.get(2)?,
                channel_id: row.get(3)?,
                sender_id: row.get(4)?,
                sender_name: row.get(5)?,
                status: PairingStatus::parse(&row.get::<_, String>(6)?)
                    .unwrap_or(PairingStatus::Pending),
                created_at: parse_dt(&row.get::<_, String>(7)?),
                expires_at: parse_dt(&row.get::<_, String>(8)?),
                approved_by: row.get(9)?,
                approved_at: parse_optional_dt(row.get(10)?),
            })
        })?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn list_pending_channel_requests(&self) -> Result<Vec<ChannelPairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, platform, channel_id, sender_id, sender_name,
                    status, created_at, expires_at, approved_by, approved_at
             FROM channel_pairing_requests WHERE status = 'pending'
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ChannelPairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                platform: row.get(2)?,
                channel_id: row.get(3)?,
                sender_id: row.get(4)?,
                sender_name: row.get(5)?,
                status: PairingStatus::Pending,
                created_at: parse_dt(&row.get::<_, String>(7)?),
                expires_at: parse_dt(&row.get::<_, String>(8)?),
                approved_by: row.get(9)?,
                approved_at: parse_optional_dt(row.get(10)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn update_channel_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock()?;
        let approved_at = if status == PairingStatus::Approved {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        conn.execute(
            "UPDATE channel_pairing_requests
             SET status = ?1, approved_by = ?2, approved_at = ?3
             WHERE id = ?4",
            params![status.as_str(), approved_by, approved_at, id],
        )?;
        Ok(())
    }

    fn is_sender_allowed(&self, platform: &str, sender_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM channel_allowlist
             WHERE platform = ?1 AND sender_id = ?2",
            params![platform, sender_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn add_to_allowlist(&self, entry: &ChannelAllowlistEntry) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO channel_allowlist
                (id, platform, channel_id, sender_id, sender_name, added_at, added_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.id,
                entry.platform,
                entry.channel_id,
                entry.sender_id,
                entry.sender_name,
                entry.added_at.to_rfc3339(),
                entry.added_by,
            ],
        )?;
        Ok(())
    }

    fn remove_from_allowlist(&self, platform: &str, sender_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "DELETE FROM channel_allowlist WHERE platform = ?1 AND sender_id = ?2",
            params![platform, sender_id],
        )?;
        Ok(changed > 0)
    }

    fn list_allowlist(&self, platform: Option<&str>) -> Result<Vec<ChannelAllowlistEntry>> {
        let conn = self.lock()?;
        let (sql, param_values): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match platform {
            Some(p) => (
                "SELECT id, platform, channel_id, sender_id, sender_name, added_at, added_by
                 FROM channel_allowlist WHERE platform = ?1 ORDER BY added_at DESC",
                vec![Box::new(p.to_string())],
            ),
            None => (
                "SELECT id, platform, channel_id, sender_id, sender_name, added_at, added_by
                 FROM channel_allowlist ORDER BY added_at DESC",
                vec![],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(ChannelAllowlistEntry {
                id: row.get(0)?,
                platform: row.get(1)?,
                channel_id: row.get(2)?,
                sender_id: row.get(3)?,
                sender_name: row.get(4)?,
                added_at: parse_dt(&row.get::<_, String>(5)?),
                added_by: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn expire_stale_requests(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock()?;
        let cutoff_str = cutoff.to_rfc3339();
        let changed = conn.execute(
            "UPDATE channel_pairing_requests SET status = 'expired'
             WHERE status = 'pending' AND expires_at < ?1",
            params![cutoff_str],
        )?;
        // Also expire device and node requests
        let d = conn.execute(
            "UPDATE device_pairing_requests SET status = 'expired'
             WHERE status = 'pending' AND expires_at < ?1",
            params![cutoff_str],
        )?;
        let n = conn.execute(
            "UPDATE node_pairing_requests SET status = 'expired'
             WHERE status = 'pending' AND expires_at < ?1",
            params![cutoff_str],
        )?;
        Ok(changed + d + n)
    }

    fn count_pending(&self) -> Result<usize> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM channel_pairing_requests WHERE status = 'pending') +
                (SELECT COUNT(*) FROM device_pairing_requests WHERE status = 'pending') +
                (SELECT COUNT(*) FROM node_pairing_requests WHERE status = 'pending')",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    // ---- Device pairing ----

    fn create_device_request(&self, req: &DevicePairingRequest) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO device_pairing_requests
                (id, code, device_name, public_key, scopes, status, created_at, expires_at, pending_token, client_id, identity_hint, device_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                req.id,
                req.code,
                req.device_name,
                req.public_key,
                serde_json::to_string(&req.scopes)?,
                req.status.as_str(),
                req.created_at.to_rfc3339(),
                req.expires_at.to_rfc3339(),
                req.pending_token,
                req.client_id,
                req.identity_hint,
                req.device_kind,
            ],
        )?;
        Ok(())
    }

    fn get_device_request_by_code(&self, code: &str) -> Result<Option<DevicePairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, device_name, public_key, scopes,
                    status, created_at, expires_at, approved_by, approved_at, pending_token, client_id, identity_hint,
                    device_kind
             FROM device_pairing_requests WHERE code = ?1",
        )?;
        let mut rows = stmt.query_map(params![code], |row| {
            let scopes_json: String = row.get(4)?;
            Ok(DevicePairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                device_name: row.get(2)?,
                client_id: row.get(11)?,
                public_key: row.get(3)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                status: PairingStatus::parse(&row.get::<_, String>(5)?)
                    .unwrap_or(PairingStatus::Pending),
                created_at: parse_dt(&row.get::<_, String>(6)?),
                expires_at: parse_dt(&row.get::<_, String>(7)?),
                approved_by: row.get(8)?,
                approved_at: parse_optional_dt(row.get(9)?),
                pending_token: row.get(10)?,
                identity_hint: row.get(12)?,
                device_kind: row.get(13)?,
            })
        })?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn list_pending_device_requests(&self) -> Result<Vec<DevicePairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, device_name, public_key, scopes,
                    status, created_at, expires_at, approved_by, approved_at, pending_token, client_id, identity_hint,
                    device_kind
             FROM device_pairing_requests WHERE status = 'pending'
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let scopes_json: String = row.get(4)?;
            Ok(DevicePairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                device_name: row.get(2)?,
                client_id: row.get(11)?,
                public_key: row.get(3)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                status: PairingStatus::Pending,
                created_at: parse_dt(&row.get::<_, String>(6)?),
                expires_at: parse_dt(&row.get::<_, String>(7)?),
                approved_by: row.get(8)?,
                approved_at: parse_optional_dt(row.get(9)?),
                pending_token: row.get(10)?,
                identity_hint: row.get(12)?,
                device_kind: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn update_device_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock()?;
        let approved_at = if status == PairingStatus::Approved {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        conn.execute(
            "UPDATE device_pairing_requests
             SET status = ?1, approved_by = ?2, approved_at = ?3
             WHERE id = ?4",
            params![status.as_str(), approved_by, approved_at, id],
        )?;
        Ok(())
    }

    fn register_device(&self, device: &ApprovedDevice) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO approved_devices
                (id, device_name, scopes, token_hash, created_at, revoked, expires_at, client_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                device.id,
                device.device_name,
                serde_json::to_string(&device.scopes)?,
                device.token_hash,
                device.created_at.to_rfc3339(),
                device.revoked as i32,
                device.expires_at.map(|dt| dt.to_rfc3339()),
                device.client_id,
            ],
        )?;
        Ok(())
    }

    fn get_device_by_id(&self, id: &str) -> Result<Option<ApprovedDevice>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, device_name, scopes, token_hash, created_at, last_used, revoked, expires_at, client_id
             FROM approved_devices WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            let scopes_json: String = row.get(2)?;
            Ok(ApprovedDevice {
                id: row.get(0)?,
                device_name: row.get(1)?,
                client_id: row.get(8)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                token_hash: row.get(3)?,
                created_at: parse_dt(&row.get::<_, String>(4)?),
                last_used: parse_optional_dt(row.get(5)?),
                revoked: row.get::<_, i32>(6)? != 0,
                expires_at: parse_optional_dt(row.get(7)?),
            })
        })?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn validate_device_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<(ApprovedDevice, TokenMatch)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, device_name, scopes, token_hash, created_at, last_used, revoked, expires_at, client_id
             FROM approved_devices WHERE (token_hash = ?1 OR prev_token_hash = ?1) AND revoked = 0",
        )?;
        let mut rows = stmt.query_map(params![token_hash], |row| {
            let scopes_json: String = row.get(2)?;
            Ok(ApprovedDevice {
                id: row.get(0)?,
                device_name: row.get(1)?,
                client_id: row.get(8)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                token_hash: row.get(3)?,
                created_at: parse_dt(&row.get::<_, String>(4)?),
                last_used: parse_optional_dt(row.get(5)?),
                revoked: false,
                expires_at: parse_optional_dt(row.get(7)?),
            })
        })?;
        match rows.next() {
            Some(Ok(device)) => {
                let kind = if device.token_hash == token_hash {
                    TokenMatch::Current
                } else {
                    TokenMatch::Previous
                };
                Ok(Some((device, kind)))
            }
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn revoke_device(&self, id: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE approved_devices SET revoked = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    fn normalize_device_scopes(&self, canonical: &[String]) -> Result<usize> {
        let json = serde_json::to_string(canonical)?;
        let conn = self.lock()?;
        let devices = conn.execute(
            "UPDATE approved_devices SET scopes = ?1 WHERE revoked = 0 AND scopes != ?1",
            params![json],
        )?;
        // Pending requests are healed too: approval mints a device from the
        // request's stored scopes, so a request that pre-dates this rule must
        // not grant a stale surface when an operator approves it post-restart.
        let pending = conn.execute(
            "UPDATE device_pairing_requests SET scopes = ?1 WHERE status = 'pending' AND scopes != ?1",
            params![json],
        )?;
        Ok(devices + pending)
    }

    fn list_devices(&self) -> Result<Vec<ApprovedDevice>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, device_name, scopes, token_hash, created_at, last_used, revoked, expires_at, client_id
             FROM approved_devices
             WHERE (expires_at IS NULL OR expires_at > datetime('now'))
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let scopes_json: String = row.get(2)?;
            Ok(ApprovedDevice {
                id: row.get(0)?,
                device_name: row.get(1)?,
                client_id: row.get(8)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                token_hash: row.get(3)?,
                created_at: parse_dt(&row.get::<_, String>(4)?),
                last_used: parse_optional_dt(row.get(5)?),
                revoked: row.get::<_, i32>(6)? != 0,
                expires_at: parse_optional_dt(row.get(7)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn update_device_last_used(&self, id: &str) -> Result<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE approved_devices SET last_used = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    fn rotate_device_token(
        &self,
        id: &str,
        new_token_hash: &str,
        new_expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE approved_devices SET prev_token_hash = token_hash, token_hash = ?1, expires_at = ?2
             WHERE id = ?3 AND revoked = 0",
            params![new_token_hash, new_expires_at.map(|dt| dt.to_rfc3339()), id,],
        )?;
        if changed == 0 {
            return Err(eyre::eyre!("Device not found or already revoked: {id}"));
        }
        Ok(())
    }

    fn set_device_pending_token(&self, id: &str, token: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE device_pairing_requests SET pending_token = ?1 WHERE id = ?2",
            params![token, id],
        )?;
        Ok(())
    }

    fn clear_device_pending_token(&self, id: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE device_pairing_requests SET pending_token = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    fn find_device_by_client_id(&self, client_id: &str) -> Result<Option<ApprovedDevice>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, device_name, scopes, token_hash, created_at, last_used, revoked, expires_at, client_id
             FROM approved_devices WHERE client_id = ?1 AND revoked = 0
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![client_id], |row| {
            let scopes_json: String = row.get(2)?;
            Ok(ApprovedDevice {
                id: row.get(0)?,
                device_name: row.get(1)?,
                client_id: row.get(8)?,
                scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
                token_hash: row.get(3)?,
                created_at: parse_dt(&row.get::<_, String>(4)?),
                last_used: parse_optional_dt(row.get(5)?),
                revoked: false,
                expires_at: parse_optional_dt(row.get(7)?),
            })
        })?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    // ---- Node pairing ----

    fn create_node_request(&self, req: &NodePairingRequest) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO node_pairing_requests
                (id, code, node_id, node_name, node_type, capabilities, status, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                req.id,
                req.code,
                req.node_id,
                req.node_name,
                req.node_type,
                serde_json::to_string(&req.capabilities)?,
                req.status.as_str(),
                req.created_at.to_rfc3339(),
                req.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn get_node_request_by_code(&self, code: &str) -> Result<Option<NodePairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, node_id, node_name, node_type, capabilities,
                    status, created_at, expires_at, approved_by, approved_at
             FROM node_pairing_requests WHERE code = ?1",
        )?;
        let mut rows = stmt.query_map(params![code], |row| {
            let caps_json: String = row.get(5)?;
            Ok(NodePairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                node_id: row.get(2)?,
                node_name: row.get(3)?,
                node_type: row.get(4)?,
                capabilities: serde_json::from_str(&caps_json).unwrap_or_default(),
                status: PairingStatus::parse(&row.get::<_, String>(6)?)
                    .unwrap_or(PairingStatus::Pending),
                created_at: parse_dt(&row.get::<_, String>(7)?),
                expires_at: parse_dt(&row.get::<_, String>(8)?),
                approved_by: row.get(9)?,
                approved_at: parse_optional_dt(row.get(10)?),
            })
        })?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn list_pending_node_requests(&self) -> Result<Vec<NodePairingRequest>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, code, node_id, node_name, node_type, capabilities,
                    status, created_at, expires_at, approved_by, approved_at
             FROM node_pairing_requests WHERE status = 'pending'
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let caps_json: String = row.get(5)?;
            Ok(NodePairingRequest {
                id: row.get(0)?,
                code: row.get(1)?,
                node_id: row.get(2)?,
                node_name: row.get(3)?,
                node_type: row.get(4)?,
                capabilities: serde_json::from_str(&caps_json).unwrap_or_default(),
                status: PairingStatus::Pending,
                created_at: parse_dt(&row.get::<_, String>(7)?),
                expires_at: parse_dt(&row.get::<_, String>(8)?),
                approved_by: row.get(9)?,
                approved_at: parse_optional_dt(row.get(10)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn update_node_request_status(
        &self,
        id: &str,
        status: PairingStatus,
        approved_by: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock()?;
        let approved_at = if status == PairingStatus::Approved {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        conn.execute(
            "UPDATE node_pairing_requests
             SET status = ?1, approved_by = ?2, approved_at = ?3
             WHERE id = ?4",
            params![status.as_str(), approved_by, approved_at, id],
        )?;
        Ok(())
    }

    fn is_node_approved(&self, node_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM node_pairing_requests
             WHERE node_id = ?1 AND status = 'approved'",
            params![node_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn device_requests_keep_their_identity_hint_and_kind() {
        device_request_contract(&SqlitePairingStore::in_memory().unwrap());
    }

    fn make_channel_request(code: &str, sender_id: &str) -> ChannelPairingRequest {
        let now = Utc::now();
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

    #[test]
    fn channel_pairing_roundtrip() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let req = make_channel_request("ABCD1234", "U001");
        store.create_channel_request(&req).unwrap();

        let found = store
            .get_channel_request_by_code("ABCD1234")
            .unwrap()
            .expect("should find request");
        assert_eq!(found.sender_id, "U001");
        assert_eq!(found.status, PairingStatus::Pending);

        let pending = store.list_pending_channel_requests().unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn allowlist_operations() {
        let store = SqlitePairingStore::in_memory().unwrap();

        assert!(!store.is_sender_allowed("slack", "U001").unwrap());

        let entry = ChannelAllowlistEntry {
            id: uuid::Uuid::new_v4().to_string(),
            platform: "slack".to_string(),
            channel_id: "C123".to_string(),
            sender_id: "U001".to_string(),
            sender_name: "Test".to_string(),
            added_at: Utc::now(),
            added_by: "pairing:ABCD1234".to_string(),
        };
        store.add_to_allowlist(&entry).unwrap();
        assert!(store.is_sender_allowed("slack", "U001").unwrap());

        let list = store.list_allowlist(Some("slack")).unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.remove_from_allowlist("slack", "U001").unwrap());
        assert!(!store.is_sender_allowed("slack", "U001").unwrap());
    }

    #[test]
    fn device_pairing_roundtrip() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let now = Utc::now();
        let req = DevicePairingRequest {
            id: uuid::Uuid::new_v4().to_string(),
            code: "DEVX5678".to_string(),
            device_name: "my-app".to_string(),
            client_id: None,
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
        store.create_device_request(&req).unwrap();

        let found = store
            .get_device_request_by_code("DEVX5678")
            .unwrap()
            .expect("should find");
        assert_eq!(found.scopes, vec!["chat", "sessions"]);

        // Approve and register device
        store
            .update_device_request_status(&req.id, PairingStatus::Approved, Some("admin"))
            .unwrap();

        let device = ApprovedDevice {
            id: req.id.clone(),
            device_name: "my-app".to_string(),
            client_id: None,
            scopes: req.scopes.clone(),
            token_hash: "abc123hash".to_string(),
            created_at: Utc::now(),
            last_used: None,
            revoked: false,
            expires_at: None,
        };
        store.register_device(&device).unwrap();

        let (found, kind) = store
            .validate_device_token_hash("abc123hash")
            .unwrap()
            .expect("should find");
        assert_eq!(found.device_name, "my-app");
        assert_eq!(kind, TokenMatch::Current);

        // Revoke
        store.revoke_device(&device.id).unwrap();
        assert!(store
            .validate_device_token_hash("abc123hash")
            .unwrap()
            .is_none());
    }

    /// validate_device_token_hash must report which slot matched so the
    /// rotation endpoint can refuse to advance the chain when the client
    /// is on the recovery slot. Misreporting `Current` for a prev match
    /// burns the safety net.
    #[test]
    fn validate_returns_correct_token_match_slot() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let device = ApprovedDevice {
            id: uuid::Uuid::new_v4().to_string(),
            device_name: "phone".to_string(),
            client_id: Some("client-1".to_string()),
            scopes: vec!["chat".to_string()],
            token_hash: "tok_a".to_string(),
            created_at: Utc::now(),
            last_used: None,
            revoked: false,
            expires_at: None,
        };
        store.register_device(&device).unwrap();

        let (_, kind) = store
            .validate_device_token_hash("tok_a")
            .unwrap()
            .expect("matched");
        assert_eq!(kind, TokenMatch::Current);

        store
            .rotate_device_token(&device.id, "tok_b", None)
            .unwrap();

        let (_, kind_new) = store
            .validate_device_token_hash("tok_b")
            .unwrap()
            .expect("matched new");
        assert_eq!(kind_new, TokenMatch::Current);

        let (_, kind_old) = store
            .validate_device_token_hash("tok_a")
            .unwrap()
            .expect("matched old");
        assert_eq!(kind_old, TokenMatch::Previous);

        // Only the next rotation retires the oldest hash — the newest two
        // generations stay valid, everything older is gone.
        store
            .rotate_device_token(&device.id, "tok_c", None)
            .unwrap();
        assert!(store.validate_device_token_hash("tok_c").unwrap().is_some());
        assert!(store.validate_device_token_hash("tok_b").unwrap().is_some());
        assert!(store.validate_device_token_hash("tok_a").unwrap().is_none());
    }

    /// Re-registering a device id (the re-pair path) must clear any stale
    /// `prev_token_hash`, otherwise a long-defunct token could resurrect via
    /// the grace-window match.
    #[test]
    fn register_device_clears_prev_token_hash_on_repair() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let device = ApprovedDevice {
            id: uuid::Uuid::new_v4().to_string(),
            device_name: "phone".to_string(),
            client_id: Some("client-1".to_string()),
            scopes: vec!["chat".to_string()],
            token_hash: "tok_a".to_string(),
            created_at: Utc::now(),
            last_used: None,
            revoked: false,
            expires_at: None,
        };
        store.register_device(&device).unwrap();
        store
            .rotate_device_token(&device.id, "tok_b", None)
            .unwrap();
        assert!(store.validate_device_token_hash("tok_a").unwrap().is_some());

        let repaired = ApprovedDevice {
            token_hash: "tok_c".to_string(),
            created_at: Utc::now(),
            ..device.clone()
        };
        store.register_device(&repaired).unwrap();

        assert!(
            store.validate_device_token_hash("tok_a").unwrap().is_none(),
            "stale prev_token_hash must not validate after re-pair"
        );
        assert!(
            store.validate_device_token_hash("tok_b").unwrap().is_none(),
            "pre-rotation token_hash must not validate after re-pair"
        );
        assert!(store.validate_device_token_hash("tok_c").unwrap().is_some());
    }

    #[test]
    fn normalize_device_scopes_heals_non_revoked_devices_only() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let canonical: Vec<String> = ["chat", "sessions", "agents", "pair", "heartbeat", "models"]
            .map(String::from)
            .to_vec();
        let device = |name: &str, scopes: Vec<String>, revoked: bool| ApprovedDevice {
            id: format!("id-{name}"),
            device_name: name.to_string(),
            client_id: None,
            scopes,
            token_hash: format!("tok-{name}"),
            created_at: Utc::now(),
            last_used: None,
            revoked,
            expires_at: None,
        };
        store
            .register_device(&device("wild", vec!["*".to_string()], false))
            .unwrap();
        store
            .register_device(&device(
                "partial",
                vec![
                    "chat".into(),
                    "sessions".into(),
                    "agents".into(),
                    "pair".into(),
                ],
                false,
            ))
            .unwrap();
        store
            .register_device(&device("ok", canonical.clone(), false))
            .unwrap();
        store
            .register_device(&device("revoked", vec!["*".to_string()], true))
            .unwrap();

        // A pending request is healed (approval would otherwise mint a stale
        // device); a terminal one is left as the historical record.
        let request = |code: &str, status: PairingStatus| DevicePairingRequest {
            id: format!("req-{code}"),
            code: code.to_string(),
            device_name: code.to_string(),
            public_key: None,
            scopes: vec!["*".to_string()],
            status,
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::minutes(5),
            approved_by: None,
            approved_at: None,
            pending_token: None,
            client_id: None,
            identity_hint: None,
            device_kind: None,
        };
        store
            .create_device_request(&request("pend", PairingStatus::Pending))
            .unwrap();
        store
            .create_device_request(&request("done", PairingStatus::Approved))
            .unwrap();

        let changed = store.normalize_device_scopes(&canonical).unwrap();

        // The wildcard + partial devices and the pending request change; the
        // already-canonical device, the revoked device, and the terminal
        // request are left untouched.
        assert_eq!(changed, 3);
        let scopes_of = |id: &str| store.get_device_by_id(id).unwrap().unwrap().scopes;
        assert_eq!(scopes_of("id-wild"), canonical);
        assert_eq!(scopes_of("id-partial"), canonical);
        assert_eq!(scopes_of("id-ok"), canonical);
        assert_eq!(scopes_of("id-revoked"), vec!["*".to_string()]);
        let req_scopes = |code: &str| {
            store
                .get_device_request_by_code(code)
                .unwrap()
                .unwrap()
                .scopes
        };
        assert_eq!(req_scopes("pend"), canonical);
        assert_eq!(req_scopes("done"), vec!["*".to_string()]);

        // Idempotent: a second pass changes nothing.
        assert_eq!(store.normalize_device_scopes(&canonical).unwrap(), 0);
    }

    #[test]
    fn expire_stale() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let now = Utc::now();
        // Create an already-expired request
        let req = ChannelPairingRequest {
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
        store.create_channel_request(&req).unwrap();

        let expired = store.expire_stale_requests(now).unwrap();
        assert_eq!(expired, 1);

        // Should no longer be in pending list
        let pending = store.list_pending_channel_requests().unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn generic_code_lookup() {
        let store = SqlitePairingStore::in_memory().unwrap();
        let req = make_channel_request("FIND0001", "U003");
        store.create_channel_request(&req).unwrap();

        match store.get_any_request_by_code("FIND0001").unwrap() {
            Some(PairingRequestKind::Channel(r)) => assert_eq!(r.sender_id, "U003"),
            other => panic!("Expected Channel, got {:?}", other.is_some()),
        }

        assert!(store.get_any_request_by_code("NOTEXIST").unwrap().is_none());
    }
}

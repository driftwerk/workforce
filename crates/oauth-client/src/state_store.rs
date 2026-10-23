//! Pending-redirect state store for OAuth web flows.
//!
//! v1 of `workforce-oauth-client` shipped a single in-memory
//! implementation. The MCP-server install/click-to-authorise flow
//! needs the state to survive a hub restart between "user clicks
//! authorise in the UI" and "vendor calls our callback URL", so this
//! module promotes the store to a trait with two impls:
//!
//! - [`InMemoryOAuthStateStore`] — original behaviour, the default.
//! - [`SqliteOAuthStateStore`] — persisted to a sqlite file under
//!   the hub's data dir.
//!
//! `expires_at` was previously held as `std::time::Instant`. Instants
//! are monotonic and meaningless across restarts, which would defeat
//! the point of the persistent store, so the field is now a Unix
//! seconds `u64` (matching the same convention used by the rest of
//! `workforce-oauth-client` for token timestamps).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eyre::{eyre, Result, WrapErr};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::warn;

/// Default time a pending redirect-flow entry remains valid after
/// `OAuthClient::start_web_flow`. Overrideable per store impl.
pub const STATE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct StateEntry {
    pub provider: String,
    pub uid: String,
    pub workspace: String,
    pub verifier: String,
    /// Unix seconds. `0` means "no expiry tracked"; tests may set it
    /// directly to simulate a stale entry.
    pub expires_at: u64,
}

/// Persistence boundary for pending OAuth redirect-flow state.
///
/// Implementations are responsible for opportunistic cleanup of
/// expired entries on insert (no background task required) and for
/// "single-use" semantics on `take` — a second call with the same
/// `state` must return `None` even within the TTL.
pub trait OAuthStateStore: Send + Sync {
    /// Insert a pending flow keyed by the random `state` parameter
    /// the user carries through the redirect.
    fn insert(
        &self,
        state: String,
        provider: String,
        uid: String,
        workspace: String,
        verifier: String,
    ) -> Result<()>;

    /// Remove and return the entry for `state` if it exists and
    /// hasn't expired. Single-use.
    fn take(&self, state: &str) -> Result<Option<StateEntry>>;
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────
// In-memory implementation (the default; used by tests and any hub
// deployment that doesn't need restart-survival of pending OAuth
// flows).
// ──────────────────────────────────────────────────────────────────

pub struct InMemoryOAuthStateStore {
    entries: Mutex<HashMap<String, StateEntry>>,
    ttl: Duration,
}

impl Default for InMemoryOAuthStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryOAuthStateStore {
    pub fn new() -> Self {
        Self::with_ttl(STATE_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("oauth state mutex poisoned")
            .len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl OAuthStateStore for InMemoryOAuthStateStore {
    fn insert(
        &self,
        state: String,
        provider: String,
        uid: String,
        workspace: String,
        verifier: String,
    ) -> Result<()> {
        let now = now_unix();
        let mut map = self.entries.lock().expect("oauth state mutex poisoned");
        map.retain(|_, e| e.expires_at > now);
        map.insert(
            state,
            StateEntry {
                provider,
                uid,
                workspace,
                verifier,
                expires_at: now + self.ttl.as_secs(),
            },
        );
        Ok(())
    }

    fn take(&self, state: &str) -> Result<Option<StateEntry>> {
        let mut map = self.entries.lock().expect("oauth state mutex poisoned");
        let Some(entry) = map.remove(state) else {
            return Ok(None);
        };
        if entry.expires_at > now_unix() {
            Ok(Some(entry))
        } else {
            Ok(None)
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// SQLite implementation. Selected by the hub when the operator
// enables `[oauth_state] backend = "sqlite"` so a UI-initiated OAuth
// flow can survive a hub restart between authorise-click and
// vendor-callback.
// ──────────────────────────────────────────────────────────────────

pub struct SqliteOAuthStateStore {
    conn: Mutex<Connection>,
    ttl: Duration,
}

impl SqliteOAuthStateStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_ttl(path, STATE_TTL)
    }

    pub fn with_ttl(path: impl AsRef<Path>, ttl: Duration) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("opening OAuth state database: {:?}", path.as_ref()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            ttl,
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            ttl: STATE_TTL,
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS oauth_state (
                state       TEXT PRIMARY KEY,
                provider    TEXT NOT NULL,
                uid         TEXT NOT NULL,
                workspace   TEXT NOT NULL,
                verifier    TEXT NOT NULL,
                expires_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_oauth_state_expires ON oauth_state(expires_at);
            ",
        )
        .wrap_err("initialising oauth_state schema")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre!("oauth state lock poisoned: {}", e))
    }
}

impl OAuthStateStore for SqliteOAuthStateStore {
    fn insert(
        &self,
        state: String,
        provider: String,
        uid: String,
        workspace: String,
        verifier: String,
    ) -> Result<()> {
        let conn = self.lock()?;
        let now = now_unix() as i64;
        // Opportunistic cleanup — keeps the table small without a
        // background sweeper. Same pattern as the in-memory impl.
        if let Err(e) = conn.execute(
            "DELETE FROM oauth_state WHERE expires_at <= ?1",
            params![now],
        ) {
            warn!(error = %e, "oauth_state cleanup failed; continuing");
        }
        let expires_at = now + self.ttl.as_secs() as i64;
        conn.execute(
            "INSERT INTO oauth_state (state, provider, uid, workspace, verifier, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(state) DO UPDATE SET \
                 provider   = excluded.provider, \
                 uid        = excluded.uid, \
                 workspace  = excluded.workspace, \
                 verifier   = excluded.verifier, \
                 expires_at = excluded.expires_at",
            params![state, provider, uid, workspace, verifier, expires_at],
        )
        .wrap_err("upserting oauth_state row")?;
        Ok(())
    }

    fn take(&self, state: &str) -> Result<Option<StateEntry>> {
        let conn = self.lock()?;
        // SELECT + DELETE in one connection lock = single-use under
        // concurrent callers. SQLite's per-connection serialisation
        // gives us the atomicity we need without an explicit tx.
        let row = conn
            .query_row(
                "SELECT provider, uid, workspace, verifier, expires_at \
                 FROM oauth_state WHERE state = ?1",
                params![state],
                |row: &rusqlite::Row<'_>| {
                    Ok(StateEntry {
                        provider: row.get(0)?,
                        uid: row.get(1)?,
                        workspace: row.get(2)?,
                        verifier: row.get(3)?,
                        expires_at: row.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .optional()
            .wrap_err("fetching oauth_state row")?;
        let Some(entry) = row else {
            return Ok(None);
        };
        conn.execute("DELETE FROM oauth_state WHERE state = ?1", params![state])
            .wrap_err("deleting consumed oauth_state row")?;
        if entry.expires_at > now_unix() {
            Ok(Some(entry))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(expires_at: u64) -> StateEntry {
        StateEntry {
            provider: "google".into(),
            uid: "kings".into(),
            workspace: "default".into(),
            verifier: "v1".into(),
            expires_at,
        }
    }

    // ── In-memory ──────────────────────────────────────────────

    #[test]
    fn in_memory_take_returns_inserted_entry_once() {
        let store = InMemoryOAuthStateStore::new();
        store
            .insert(
                "s1".into(),
                "google".into(),
                "kings".into(),
                "default".into(),
                "v1".into(),
            )
            .unwrap();

        let first = store.take("s1").unwrap().expect("entry should be present");
        assert_eq!(first.provider, "google");
        assert_eq!(first.uid, "kings");
        assert_eq!(first.workspace, "default");
        assert_eq!(first.verifier, "v1");

        assert!(
            store.take("s1").unwrap().is_none(),
            "second take must return None"
        );
    }

    #[test]
    fn in_memory_unknown_state_does_not_evict_others() {
        let store = InMemoryOAuthStateStore::new();
        store
            .insert(
                "s1".into(),
                "google".into(),
                "kings".into(),
                "default".into(),
                "v1".into(),
            )
            .unwrap();
        assert!(store.take("other").unwrap().is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn in_memory_insert_prunes_expired_entries() {
        let store = InMemoryOAuthStateStore::new();
        store
            .entries
            .lock()
            .expect("oauth state mutex poisoned")
            .insert("stale".into(), entry(1));
        assert_eq!(store.len(), 1);

        store
            .insert(
                "fresh".into(),
                "google".into(),
                "kings".into(),
                "default".into(),
                "v2".into(),
            )
            .unwrap();
        assert_eq!(store.len(), 1, "stale entry should have been pruned");
        assert!(store.take("stale").unwrap().is_none());
        assert!(store.take("fresh").unwrap().is_some());
    }

    #[test]
    fn in_memory_provider_is_carried_through_take() {
        let store = InMemoryOAuthStateStore::new();
        store
            .insert(
                "s1".into(),
                "linear".into(),
                "kings".into(),
                "default".into(),
                "v1".into(),
            )
            .unwrap();
        let entry = store.take("s1").unwrap().unwrap();
        assert_eq!(entry.provider, "linear");
    }

    // ── SQLite ────────────────────────────────────────────────

    #[test]
    fn sqlite_take_returns_inserted_entry_once() {
        let store = SqliteOAuthStateStore::in_memory().unwrap();
        store
            .insert(
                "s1".into(),
                "linear".into(),
                "kings".into(),
                "default".into(),
                "v1".into(),
            )
            .unwrap();
        let first = store.take("s1").unwrap().unwrap();
        assert_eq!(first.provider, "linear");
        assert!(store.take("s1").unwrap().is_none(), "single-use");
    }

    #[test]
    fn sqlite_expired_entry_returns_none_on_take() {
        let store = SqliteOAuthStateStore::in_memory().unwrap();
        // Backdate by writing directly through the connection.
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO oauth_state (state, provider, uid, workspace, verifier, expires_at) \
                 VALUES ('stale', 'google', 'kings', 'default', 'v1', 1)",
                [],
            )
            .unwrap();
        }
        assert!(store.take("stale").unwrap().is_none());
    }

    #[test]
    fn sqlite_insert_cleans_up_expired_rows() {
        let store = SqliteOAuthStateStore::in_memory().unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO oauth_state (state, provider, uid, workspace, verifier, expires_at) \
                 VALUES ('stale', 'google', 'kings', 'default', 'v1', 1)",
                [],
            )
            .unwrap();
        }
        store
            .insert(
                "fresh".into(),
                "google".into(),
                "kings".into(),
                "default".into(),
                "v2".into(),
            )
            .unwrap();
        let count: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM oauth_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "stale row should have been pruned");
    }

    #[test]
    fn sqlite_state_persists_across_open_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_state.db");
        let store1 = SqliteOAuthStateStore::new(&path).unwrap();
        store1
            .insert(
                "s1".into(),
                "linear".into(),
                "kings".into(),
                "default".into(),
                "v1".into(),
            )
            .unwrap();
        drop(store1);

        let store2 = SqliteOAuthStateStore::new(&path).unwrap();
        let entry = store2
            .take("s1")
            .unwrap()
            .expect("persisted entry should survive restart");
        assert_eq!(entry.provider, "linear");
        assert_eq!(entry.verifier, "v1");
    }

    #[test]
    fn sqlite_unknown_state_returns_none() {
        let store = SqliteOAuthStateStore::in_memory().unwrap();
        assert!(store.take("nothing").unwrap().is_none());
    }
}

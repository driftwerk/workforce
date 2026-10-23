//! Quarantine store for content blocked by the sentinel security system.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use eyre::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// A quarantined content entry that was blocked by the sentinel system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineEntry {
    pub id: String,
    pub content_hash: String,
    pub raw_content: String,
    pub source: String,
    pub sender_id: Option<String>,
    pub trust_level: String,
    pub threat_score: f64,
    pub threat_level: String,
    pub status: String, // "quarantined", "released", "deleted"
    pub created_at: String,
    pub reviewed_at: Option<String>,
    pub reviewed_by: Option<String>,
}

/// SQLite-backed quarantine store for content that the sentinel system has
/// blocked from reaching the agent. Content that scores as "hostile" gets
/// quarantined instead of being passed to the agent.
pub struct QuarantineStore {
    conn: Mutex<Connection>,
}

impl QuarantineStore {
    /// Open (or create) a SQLite database at the given path.
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

    /// Run schema migrations.
    fn migrate(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS quarantine_entries (
                id TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                raw_content TEXT NOT NULL,
                source TEXT NOT NULL,
                sender_id TEXT,
                trust_level TEXT NOT NULL,
                threat_score REAL NOT NULL,
                threat_level TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'quarantined',
                created_at TEXT NOT NULL,
                reviewed_at TEXT,
                reviewed_by TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_quarantine_status ON quarantine_entries(status);
            CREATE INDEX IF NOT EXISTS idx_quarantine_created ON quarantine_entries(created_at);
            ",
        )?;
        Ok(())
    }

    /// Store a quarantined entry.
    pub fn store(&self, entry: &QuarantineEntry) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO quarantine_entries
             (id, content_hash, raw_content, source, sender_id, trust_level,
              threat_score, threat_level, status, created_at, reviewed_at, reviewed_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                entry.id,
                entry.content_hash,
                entry.raw_content,
                entry.source,
                entry.sender_id,
                entry.trust_level,
                entry.threat_score,
                entry.threat_level,
                entry.status,
                entry.created_at,
                entry.reviewed_at,
                entry.reviewed_by,
            ],
        )?;
        Ok(())
    }

    /// Get a quarantined entry by ID.
    pub fn get(&self, id: &str) -> Result<Option<QuarantineEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, content_hash, raw_content, source, sender_id, trust_level,
                    threat_score, threat_level, status, created_at, reviewed_at, reviewed_by
             FROM quarantine_entries WHERE id = ?1",
        )?;

        let row = stmt.query_row(params![id], |row| {
            Ok(QuarantineEntry {
                id: row.get(0)?,
                content_hash: row.get(1)?,
                raw_content: row.get(2)?,
                source: row.get(3)?,
                sender_id: row.get(4)?,
                trust_level: row.get(5)?,
                threat_score: row.get(6)?,
                threat_level: row.get(7)?,
                status: row.get(8)?,
                created_at: row.get(9)?,
                reviewed_at: row.get(10)?,
                reviewed_by: row.get(11)?,
            })
        });

        match row {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List quarantined entries, optionally filtered by status.
    pub fn list(&self, status: Option<&str>, limit: u32) -> Result<Vec<QuarantineEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let sql = if status.is_some() {
            "SELECT id, content_hash, raw_content, source, sender_id, trust_level,
                    threat_score, threat_level, status, created_at, reviewed_at, reviewed_by
             FROM quarantine_entries WHERE status = ?1
             ORDER BY created_at DESC LIMIT ?2"
        } else {
            "SELECT id, content_hash, raw_content, source, sender_id, trust_level,
                    threat_score, threat_level, status, created_at, reviewed_at, reviewed_by
             FROM quarantine_entries
             ORDER BY created_at DESC LIMIT ?2"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![s, limit], row_to_entry)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![Option::<String>::None, limit], row_to_entry)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        Ok(rows)
    }

    /// Release a quarantined entry, marking it as reviewed.
    /// Returns `true` if an entry was found and released.
    pub fn release(&self, id: &str, reviewed_by: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        let changed = conn.execute(
            "UPDATE quarantine_entries SET status = 'released', reviewed_at = ?1, reviewed_by = ?2
             WHERE id = ?3 AND status = 'quarantined'",
            params![now, reviewed_by, id],
        )?;
        Ok(changed > 0)
    }

    /// Soft-delete a quarantined entry, marking it as deleted.
    /// Returns `true` if an entry was found and deleted.
    pub fn delete_entry(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        let changed = conn.execute(
            "UPDATE quarantine_entries SET status = 'deleted', reviewed_at = ?1
             WHERE id = ?2 AND status = 'quarantined'",
            params![now, id],
        )?;
        Ok(changed > 0)
    }

    /// Delete old entries that have been released or deleted beyond the
    /// specified age. Returns the number of entries removed.
    pub fn cleanup(&self, max_age_days: u64) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let cutoff = Utc::now() - chrono::Duration::days(max_age_days as i64);
        let deleted = conn.execute(
            "DELETE FROM quarantine_entries WHERE created_at < ?1 AND status IN ('released', 'deleted')",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(deleted)
    }

    /// Count quarantined entries, optionally filtered by status.
    pub fn count(&self, status: Option<&str>) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let count: i64 = if let Some(s) = status {
            conn.query_row(
                "SELECT COUNT(*) FROM quarantine_entries WHERE status = ?1",
                params![s],
                |row| row.get(0),
            )?
        } else {
            conn.query_row("SELECT COUNT(*) FROM quarantine_entries", [], |row| {
                row.get(0)
            })?
        };

        Ok(count as u64)
    }
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<QuarantineEntry> {
    Ok(QuarantineEntry {
        id: row.get(0)?,
        content_hash: row.get(1)?,
        raw_content: row.get(2)?,
        source: row.get(3)?,
        sender_id: row.get(4)?,
        trust_level: row.get(5)?,
        threat_score: row.get(6)?,
        threat_level: row.get(7)?,
        status: row.get(8)?,
        created_at: row.get(9)?,
        reviewed_at: row.get(10)?,
        reviewed_by: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str, status: &str) -> QuarantineEntry {
        QuarantineEntry {
            id: id.to_string(),
            content_hash: format!("hash-{}", id),
            raw_content: format!("malicious content from {}", id),
            source: "slack".to_string(),
            sender_id: Some("user-42".to_string()),
            trust_level: "untrusted".to_string(),
            threat_score: 0.92,
            threat_level: "hostile".to_string(),
            status: status.to_string(),
            created_at: Utc::now().to_rfc3339(),
            reviewed_at: None,
            reviewed_by: None,
        }
    }

    #[test]
    fn test_store_and_get() {
        let store = QuarantineStore::in_memory().unwrap();
        let entry = make_entry("q-0001", "quarantined");

        store.store(&entry).unwrap();

        let retrieved = store.get("q-0001").unwrap().unwrap();
        assert_eq!(retrieved.id, "q-0001");
        assert_eq!(retrieved.content_hash, "hash-q-0001");
        assert_eq!(retrieved.raw_content, "malicious content from q-0001");
        assert_eq!(retrieved.source, "slack");
        assert_eq!(retrieved.sender_id, Some("user-42".to_string()));
        assert_eq!(retrieved.trust_level, "untrusted");
        assert!((retrieved.threat_score - 0.92).abs() < 0.001);
        assert_eq!(retrieved.threat_level, "hostile");
        assert_eq!(retrieved.status, "quarantined");
        assert!(retrieved.reviewed_at.is_none());
        assert!(retrieved.reviewed_by.is_none());
    }

    #[test]
    fn test_list_by_status() {
        let store = QuarantineStore::in_memory().unwrap();

        store.store(&make_entry("q-0001", "quarantined")).unwrap();
        store.store(&make_entry("q-0002", "quarantined")).unwrap();
        store.store(&make_entry("q-0003", "released")).unwrap();

        let all = store.list(None, 100).unwrap();
        assert_eq!(all.len(), 3);

        let quarantined = store.list(Some("quarantined"), 100).unwrap();
        assert_eq!(quarantined.len(), 2);

        let released = store.list(Some("released"), 100).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].id, "q-0003");

        let deleted = store.list(Some("deleted"), 100).unwrap();
        assert_eq!(deleted.len(), 0);
    }

    #[test]
    fn test_release() {
        let store = QuarantineStore::in_memory().unwrap();
        store.store(&make_entry("q-0001", "quarantined")).unwrap();

        let released = store.release("q-0001", "admin").unwrap();
        assert!(released);

        let entry = store.get("q-0001").unwrap().unwrap();
        assert_eq!(entry.status, "released");
        assert_eq!(entry.reviewed_by, Some("admin".to_string()));
        assert!(entry.reviewed_at.is_some());

        // Releasing again should return false (already released)
        let released_again = store.release("q-0001", "admin").unwrap();
        assert!(!released_again);

        // Releasing non-existent entry should return false
        let not_found = store.release("q-9999", "admin").unwrap();
        assert!(!not_found);
    }

    #[test]
    fn test_delete_entry() {
        let store = QuarantineStore::in_memory().unwrap();
        store.store(&make_entry("q-0001", "quarantined")).unwrap();

        let deleted = store.delete_entry("q-0001").unwrap();
        assert!(deleted);

        let entry = store.get("q-0001").unwrap().unwrap();
        assert_eq!(entry.status, "deleted");
        assert!(entry.reviewed_at.is_some());

        // Deleting again should return false (already deleted)
        let deleted_again = store.delete_entry("q-0001").unwrap();
        assert!(!deleted_again);

        // Deleting non-existent entry should return false
        let not_found = store.delete_entry("q-9999").unwrap();
        assert!(!not_found);
    }

    #[test]
    fn test_count() {
        let store = QuarantineStore::in_memory().unwrap();

        store.store(&make_entry("q-0001", "quarantined")).unwrap();
        store.store(&make_entry("q-0002", "quarantined")).unwrap();
        store.store(&make_entry("q-0003", "released")).unwrap();
        store.store(&make_entry("q-0004", "deleted")).unwrap();

        assert_eq!(store.count(None).unwrap(), 4);
        assert_eq!(store.count(Some("quarantined")).unwrap(), 2);
        assert_eq!(store.count(Some("released")).unwrap(), 1);
        assert_eq!(store.count(Some("deleted")).unwrap(), 1);
    }

    #[test]
    fn test_not_found() {
        let store = QuarantineStore::in_memory().unwrap();

        let result = store.get("nonexistent-id").unwrap();
        assert!(result.is_none());
    }
}

//! Security event audit log with async non-blocking writer.
//!
//! Uses `tokio::sync::mpsc` to write events in the background without
//! blocking agent execution. Events are batched and flushed periodically
//! or when the batch reaches a configurable threshold.
//!
//! # Architecture
//!
//! - [`SecurityAuditWriter`] accepts events via a non-blocking `log()` call
//!   and forwards them over an unbounded channel to a background task.
//! - The background task accumulates events into a batch and flushes them
//!   to the [`SqliteStore`](crate::persistence::SqliteStore) every second
//!   or when the batch reaches 100 events, whichever comes first.
//! - Query functions (`insert_security_event`, `query_security_events`,
//!   `security_event_stats`, `cleanup_security_events`) are defined as
//!   methods on `SqliteStore` via an `impl` block in this module.
//!
//! # Table schema
//!
//! The `security_events` table must be created in
//! [`SqliteStore::migrate`](crate::persistence::SqliteStore) (in
//! `persistence.rs`). Expected DDL:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS security_events (
//!     id INTEGER PRIMARY KEY AUTOINCREMENT,
//!     timestamp TEXT NOT NULL,
//!     event_type TEXT NOT NULL,
//!     agent_id TEXT NOT NULL,
//!     session_id TEXT,
//!     severity TEXT NOT NULL,
//!     source TEXT,
//!     detail TEXT NOT NULL,
//!     content_hash TEXT
//! );
//! CREATE INDEX IF NOT EXISTS idx_security_events_timestamp ON security_events(timestamp);
//! CREATE INDEX IF NOT EXISTS idx_security_events_type ON security_events(event_type);
//! CREATE INDEX IF NOT EXISTS idx_security_events_agent ON security_events(agent_id);
//! CREATE INDEX IF NOT EXISTS idx_security_events_severity ON security_events(severity);
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;
use tracing::warn;

use crate::persistence::SqliteStore;
use crate::telemetry::TelemetryStore;

// ─── Event type constants ───────────────────────────────────────────

pub const EVENT_DETECTION_REGEX: &str = "detection.regex";
pub const EVENT_DETECTION_HEURISTIC: &str = "detection.heuristic";
pub const EVENT_QUARANTINE_CREATED: &str = "quarantine.created";
pub const EVENT_QUARANTINE_RELEASED: &str = "quarantine.released";
pub const EVENT_QUARANTINE_DELETED: &str = "quarantine.deleted";
pub const EVENT_INTEGRITY_VIOLATION: &str = "integrity.violation";
pub const EVENT_ANOMALY_DETECTED: &str = "anomaly.detected";
pub const EVENT_POLICY_VIOLATION: &str = "policy.violation";

// ─── Types ──────────────────────────────────────────────────────────

/// A security event to be recorded in the audit log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityEvent {
    /// One of the `EVENT_*` constants (e.g. `"detection.regex"`).
    pub event_type: String,
    /// The agent that triggered or is associated with the event.
    pub agent_id: String,
    /// Optional session identifier for correlation.
    pub session_id: Option<String>,
    /// Severity level: `"critical"`, `"warning"`, or `"info"`.
    pub severity: String,
    /// Optional source identifier (scanner name, module, etc.).
    pub source: Option<String>,
    /// Arbitrary structured detail payload.
    pub detail: serde_json::Value,
    /// Optional content hash for deduplication or integrity tracking.
    pub content_hash: Option<String>,
}

/// A security event row as returned from database queries.
///
/// Includes the auto-generated `id` and `timestamp` that are not part
/// of the input [`SecurityEvent`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityEventRow {
    pub id: i64,
    pub timestamp: String,
    pub event_type: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub severity: String,
    pub source: Option<String>,
    pub detail: serde_json::Value,
    pub content_hash: Option<String>,
}

/// Filter criteria for querying security events.
#[derive(Debug, Default)]
pub struct SecurityEventFilter {
    /// Only return events at or after this ISO-8601 timestamp.
    pub since: Option<String>,
    /// Only return events at or before this ISO-8601 timestamp.
    pub until: Option<String>,
    /// Filter by event type (exact match).
    pub event_type: Option<String>,
    /// Filter by agent ID (exact match).
    pub agent_id: Option<String>,
    /// Filter by severity (exact match).
    pub severity: Option<String>,
    /// Maximum number of rows to return (default 50).
    pub limit: Option<u32>,
}

/// Aggregated statistics over a set of security events.
#[derive(Debug, serde::Serialize)]
pub struct SecurityStats {
    /// Total number of events matching the query.
    pub total_events: u64,
    /// Event counts grouped by `event_type`.
    pub by_type: HashMap<String, u64>,
    /// Event counts grouped by `severity`.
    pub by_severity: HashMap<String, u64>,
    /// The lower-bound timestamp used for the aggregation.
    pub since: String,
}

// ─── Async writer ───────────────────────────────────────────────────

/// Non-blocking writer that batches security events and flushes them to
/// the database via a background tokio task.
pub struct SecurityAuditWriter {
    tx: tokio::sync::mpsc::UnboundedSender<SecurityEvent>,
}

impl SecurityAuditWriter {
    /// Create a new writer that spawns a background task to flush events
    /// to the store.
    ///
    /// The background task will:
    /// - Accumulate incoming events into a batch.
    /// - Flush the batch every 1 second, or when it reaches 100 events.
    /// - Flush any remaining events when the channel is closed (i.e.
    ///   when all `SecurityAuditWriter` clones are dropped).
    pub fn new(store: Arc<dyn TelemetryStore>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SecurityEvent>();
        tokio::spawn(async move {
            let mut batch: Vec<SecurityEvent> = Vec::new();
            let flush_interval = tokio::time::interval(std::time::Duration::from_secs(1));
            tokio::pin!(flush_interval);
            loop {
                tokio::select! {
                    Some(event) = rx.recv() => {
                        batch.push(event);
                        if batch.len() >= 100 {
                            flush_batch(store.as_ref(), &mut batch);
                        }
                    }
                    _ = flush_interval.tick() => {
                        if !batch.is_empty() {
                            flush_batch(store.as_ref(), &mut batch);
                        }
                    }
                    else => {
                        // Channel closed, flush remaining events.
                        if !batch.is_empty() {
                            flush_batch(store.as_ref(), &mut batch);
                        }
                        break;
                    }
                }
            }
        });
        Self { tx }
    }

    /// Log a security event (non-blocking -- sends to background writer).
    ///
    /// If the background task has already exited (e.g. because the
    /// runtime is shutting down), the event is silently dropped.
    pub fn log(&self, event: SecurityEvent) {
        let _ = self.tx.send(event);
    }
}

// ─── Batch flush ────────────────────────────────────────────────────

/// Insert all events in `batch` into the store, then clear the batch.
/// Errors are logged but do not propagate — the audit writer must not
/// crash the host process.
fn flush_batch(store: &dyn TelemetryStore, batch: &mut Vec<SecurityEvent>) {
    for event in batch.drain(..) {
        if let Err(e) = store.insert_security_event(&event) {
            warn!("Failed to insert security event: {}", e);
        }
    }
}

// ─── SqliteStore methods ────────────────────────────────────────────
//
// These are defined here (same crate) so that all security-audit logic
// lives in one file. `SqliteStore.conn` is accessible because this is
// the same crate.

impl SqliteStore {
    /// Insert a single security event into the `security_events` table.
    pub fn insert_security_event(&self, event: &SecurityEvent) -> eyre::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        let detail_str = serde_json::to_string(&event.detail)?;
        conn.execute(
            "INSERT INTO security_events
             (timestamp, event_type, agent_id, session_id, severity, source, detail, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                now,
                event.event_type,
                event.agent_id,
                event.session_id,
                event.severity,
                event.source,
                detail_str,
                event.content_hash,
            ],
        )?;
        Ok(())
    }

    /// Query security events with optional filtering.
    ///
    /// Results are returned in reverse chronological order (newest first).
    pub fn query_security_events(
        &self,
        filter: &SecurityEventFilter,
    ) -> eyre::Result<Vec<SecurityEventRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let limit = filter.limit.unwrap_or(50) as i64;

        let sql = "SELECT id, timestamp, event_type, agent_id, session_id,
                          severity, source, detail, content_hash
                   FROM security_events
                   WHERE (?1 IS NULL OR timestamp >= ?1)
                     AND (?2 IS NULL OR timestamp <= ?2)
                     AND (?3 IS NULL OR event_type = ?3)
                     AND (?4 IS NULL OR agent_id = ?4)
                     AND (?5 IS NULL OR severity = ?5)
                   ORDER BY id DESC
                   LIMIT ?6";

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(
                params![
                    filter.since,
                    filter.until,
                    filter.event_type,
                    filter.agent_id,
                    filter.severity,
                    limit,
                ],
                |row| {
                    let detail_str: String = row.get(7)?;
                    let detail: serde_json::Value =
                        serde_json::from_str(&detail_str).unwrap_or(serde_json::Value::Null);
                    Ok(SecurityEventRow {
                        id: row.get(0)?,
                        timestamp: row.get(1)?,
                        event_type: row.get(2)?,
                        agent_id: row.get(3)?,
                        session_id: row.get(4)?,
                        severity: row.get(5)?,
                        source: row.get(6)?,
                        detail,
                        content_hash: row.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Compute aggregated statistics for security events since a given timestamp.
    pub fn security_event_stats(&self, since: &str) -> eyre::Result<SecurityStats> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        // Total count
        let total_events: u64 = conn.query_row(
            "SELECT COUNT(*) FROM security_events WHERE timestamp >= ?1",
            params![since],
            |row| row.get::<_, i64>(0).map(|v| v as u64),
        )?;

        // By event_type
        let mut by_type: HashMap<String, u64> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT event_type, COUNT(*) FROM security_events
                 WHERE timestamp >= ?1
                 GROUP BY event_type",
            )?;
            let rows = stmt.query_map(params![since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            for row in rows {
                let (k, v) = row?;
                by_type.insert(k, v);
            }
        }

        // By severity
        let mut by_severity: HashMap<String, u64> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT severity, COUNT(*) FROM security_events
                 WHERE timestamp >= ?1
                 GROUP BY severity",
            )?;
            let rows = stmt.query_map(params![since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            for row in rows {
                let (k, v) = row?;
                by_severity.insert(k, v);
            }
        }

        Ok(SecurityStats {
            total_events,
            by_type,
            by_severity,
            since: since.to_string(),
        })
    }

    /// Delete security events older than the given ISO-8601 timestamp.
    ///
    /// Returns the number of rows deleted.
    pub fn cleanup_security_events(&self, before: &str) -> eyre::Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let deleted = conn.execute(
            "DELETE FROM security_events WHERE timestamp < ?1",
            params![before],
        )?;
        Ok(deleted)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_security_event_serialization() {
        let event = SecurityEvent {
            event_type: EVENT_DETECTION_REGEX.to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("sess-abc".to_string()),
            severity: "critical".to_string(),
            source: Some("security_scanner".to_string()),
            detail: serde_json::json!({
                "rule": "shell_exec",
                "file": "/tmp/test.js",
                "line": 42,
                "matched": "child_process.exec"
            }),
            content_hash: Some("abc123".to_string()),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event_type"], "detection.regex");
        assert_eq!(json["agent_id"], "agent-1");
        assert_eq!(json["session_id"], "sess-abc");
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["source"], "security_scanner");
        assert_eq!(json["detail"]["rule"], "shell_exec");
        assert_eq!(json["detail"]["line"], 42);
        assert_eq!(json["content_hash"], "abc123");
    }

    #[test]
    fn test_filter_defaults() {
        let filter = SecurityEventFilter::default();
        assert!(filter.since.is_none());
        assert!(filter.until.is_none());
        assert!(filter.event_type.is_none());
        assert!(filter.agent_id.is_none());
        assert!(filter.severity.is_none());
        assert!(filter.limit.is_none());
    }
}

//! Audit log for inter-agent communications.
//!
//! Records all agent-to-agent messages with their delivery status and
//! supports configurable oversight modes for owner control.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Status of an agent communication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommStatus {
    Pending,
    Approved,
    Denied,
    AutoApproved,
    Delivered,
    Failed(String),
}

/// Oversight mode for agent-to-agent messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OversightMode {
    /// Messages are delivered immediately and recorded as auto-approved.
    #[default]
    Auto,
    /// Messages require explicit owner approval before delivery.
    RequireApproval,
    /// Messages are delivered immediately and the owner is notified.
    Notify,
}

/// Configuration for agent comms oversight.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OversightConfig {
    /// Oversight mode (default: Auto).
    #[serde(default)]
    pub mode: OversightMode,
    /// Agent pairs that are auto-approved regardless of mode.
    /// Format: "from_agent:to_agent" (e.g., "architect:coder").
    #[serde(default)]
    pub auto_approve_pairs: Vec<String>,
}

impl OversightConfig {
    /// Check if a specific agent pair is auto-approved.
    pub fn is_pair_auto_approved(&self, from: &str, to: &str) -> bool {
        let pair = format!("{from}:{to}");
        self.auto_approve_pairs.iter().any(|p| p == &pair)
    }
}

/// A single audit record for an agent communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCommRecord {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub from_agent: String,
    pub to_agent: String,
    pub content_preview: String,
    pub status: CommStatus,
    pub channel_id: Option<String>,
}

/// Truncate a string to at most `max_bytes` bytes without splitting a
/// multi-byte UTF-8 character. Returns the full string when it is already
/// within the limit.
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Audit log for agent communications with JSONL persistence.
pub struct AgentCommsAuditLog {
    records: RwLock<VecDeque<AgentCommRecord>>,
    log_path: PathBuf,
    max_records: usize,
}

impl AgentCommsAuditLog {
    /// Create a new audit log, loading any existing records from disk.
    pub fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join("agent-comms");
        let _ = fs::create_dir_all(&dir);
        let log_path = dir.join("audit.jsonl");

        let mut records = VecDeque::new();
        if log_path.exists() {
            if let Ok(file) = fs::File::open(&log_path) {
                let reader = BufReader::new(file);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(record) = serde_json::from_str::<AgentCommRecord>(&line) {
                        records.push_back(record);
                    }
                }
            }
        }

        // Keep only the most recent records
        let max_records = 1000;
        while records.len() > max_records {
            records.pop_front();
        }

        Self {
            records: RwLock::new(records),
            log_path,
            max_records,
        }
    }

    /// Record a new communication event.
    pub async fn record(&self, record: AgentCommRecord) {
        // Append to JSONL file
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            if let Ok(json) = serde_json::to_string(&record) {
                let _ = writeln!(file, "{}", json);
            }
        }

        // Add to in-memory buffer
        let mut records = self.records.write().await;
        records.push_back(record);
        while records.len() > self.max_records {
            records.pop_front();
        }
    }

    /// List recent communication records.
    pub async fn list(&self, limit: usize) -> Vec<AgentCommRecord> {
        let records = self.records.read().await;
        records.iter().rev().take(limit).cloned().collect()
    }

    /// List pending approval requests.
    pub async fn pending_approvals(&self) -> Vec<AgentCommRecord> {
        let records = self.records.read().await;
        records
            .iter()
            .filter(|r| r.status == CommStatus::Pending)
            .cloned()
            .collect()
    }

    /// Update the status of a record by ID.
    pub async fn update_status(&self, id: &str, status: CommStatus) -> bool {
        let mut records = self.records.write().await;
        if let Some(record) = records.iter_mut().find(|r| r.id == id) {
            record.status = status;
            // Re-write the full log (simple approach for correctness)
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.log_path)
            {
                for r in records.iter() {
                    if let Ok(json) = serde_json::to_string(r) {
                        let _ = writeln!(file, "{}", json);
                    }
                }
            }
            true
        } else {
            false
        }
    }

    /// Create a new comm record with a preview of the content.
    pub fn make_record(from: &str, to: &str, content: &str, status: CommStatus) -> AgentCommRecord {
        let preview = if content.len() > 200 {
            format!("{}...", safe_truncate(content, 197))
        } else {
            content.to_string()
        };
        AgentCommRecord {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            from_agent: from.to_string(),
            to_agent: to.to_string(),
            content_preview: preview,
            status,
            channel_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_and_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let log = AgentCommsAuditLog::new(dir.path());

        let record = AgentCommsAuditLog::make_record(
            "agent-a",
            "agent-b",
            "hello world",
            CommStatus::Delivered,
        );
        let id = record.id.clone();
        log.record(record).await;

        let records = log.list(10).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, id);
        assert_eq!(records[0].from_agent, "agent-a");
        assert_eq!(records[0].to_agent, "agent-b");
        assert_eq!(records[0].content_preview, "hello world");
        assert_eq!(records[0].status, CommStatus::Delivered);
    }

    #[tokio::test]
    async fn pending_approvals_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let log = AgentCommsAuditLog::new(dir.path());

        let r1 = AgentCommsAuditLog::make_record("a", "b", "msg1", CommStatus::Pending);
        let r2 = AgentCommsAuditLog::make_record("a", "c", "msg2", CommStatus::Delivered);
        let r3 = AgentCommsAuditLog::make_record("b", "c", "msg3", CommStatus::Pending);

        log.record(r1).await;
        log.record(r2).await;
        log.record(r3).await;

        let pending = log.pending_approvals().await;
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|r| r.status == CommStatus::Pending));
    }

    #[tokio::test]
    async fn update_status_works() {
        let dir = tempfile::tempdir().unwrap();
        let log = AgentCommsAuditLog::new(dir.path());

        let record = AgentCommsAuditLog::make_record("a", "b", "msg", CommStatus::Pending);
        let id = record.id.clone();
        log.record(record).await;

        // Update from Pending to Approved
        let updated = log.update_status(&id, CommStatus::Approved).await;
        assert!(updated);

        let records = log.list(10).await;
        assert_eq!(records[0].status, CommStatus::Approved);

        // Non-existent ID returns false
        let not_found = log
            .update_status("nonexistent-id", CommStatus::Denied)
            .await;
        assert!(!not_found);
    }

    #[test]
    fn auto_approve_pairs_matching() {
        let config = OversightConfig {
            mode: OversightMode::RequireApproval,
            auto_approve_pairs: vec!["architect:coder".to_string(), "pm:designer".to_string()],
        };

        assert!(config.is_pair_auto_approved("architect", "coder"));
        assert!(config.is_pair_auto_approved("pm", "designer"));
        assert!(!config.is_pair_auto_approved("coder", "architect"));
        assert!(!config.is_pair_auto_approved("unknown", "agent"));
    }

    #[tokio::test]
    async fn jsonl_persistence() {
        let dir = tempfile::tempdir().unwrap();

        // Write records with the first instance
        {
            let log = AgentCommsAuditLog::new(dir.path());
            let r1 = AgentCommsAuditLog::make_record(
                "agent-a",
                "agent-b",
                "first",
                CommStatus::Delivered,
            );
            let r2 = AgentCommsAuditLog::make_record(
                "agent-b",
                "agent-c",
                "second",
                CommStatus::AutoApproved,
            );
            log.record(r1).await;
            log.record(r2).await;
        }

        // Create a new instance and verify records are loaded from disk
        {
            let log = AgentCommsAuditLog::new(dir.path());
            let records = log.list(10).await;
            assert_eq!(records.len(), 2);
            // list() returns most recent first
            assert_eq!(records[0].content_preview, "second");
            assert_eq!(records[1].content_preview, "first");
        }
    }

    #[test]
    fn safe_truncate_handles_multibyte() {
        // "hello" is all ASCII, trivial case
        assert_eq!(safe_truncate("hello", 3), "hel");
        assert_eq!(safe_truncate("hello", 100), "hello");

        // Multi-byte: each emoji is 4 bytes
        let emoji = "\u{1F600}\u{1F601}"; // 8 bytes total
                                          // Truncating at 5 should land on the boundary after the first emoji (4)
        assert_eq!(safe_truncate(emoji, 5), "\u{1F600}");
        assert_eq!(safe_truncate(emoji, 4), "\u{1F600}");
        assert_eq!(safe_truncate(emoji, 3), ""); // Can't fit a full emoji
    }

    #[test]
    fn make_record_truncates_long_content() {
        let long_content = "a".repeat(300);
        let record =
            AgentCommsAuditLog::make_record("a", "b", &long_content, CommStatus::Delivered);
        assert!(record.content_preview.len() <= 200);
        assert!(record.content_preview.ends_with("..."));
    }

    #[test]
    fn oversight_mode_defaults_to_auto() {
        let config = OversightConfig::default();
        assert_eq!(config.mode, OversightMode::Auto);
        assert!(config.auto_approve_pairs.is_empty());
    }
}

//! The heartbeat's wake handle and records, as the service holds and the surfaces read them.

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A single heartbeat result in the history ring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRecord {
    pub timestamp: DateTime<Utc>,
    pub response: String,
    pub actionable: bool,
    pub delivered: bool,
    /// ID of the check that produced this record (empty for legacy HEARTBEAT.md).
    #[serde(default)]
    pub check_id: String,
    /// Human-readable name of the check.
    #[serde(default)]
    pub check_name: String,
    /// How long the check took in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// Error message if the check failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Why the check did not run, when it was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Command sent through the wake channel.
#[derive(Debug, Clone)]
pub enum WakeCommand {
    /// Wake all checks.
    All,
    /// Wake a specific check by ID.
    Specific(String),
}

/// Handle to trigger manual heartbeat wake-ups.
#[derive(Clone)]
pub struct HeartbeatWakeHandle {
    pub wake_tx: tokio::sync::mpsc::Sender<WakeCommand>,
    pub history: Arc<RwLock<Vec<HeartbeatRecord>>>,
}

impl HeartbeatWakeHandle {
    /// Trigger an immediate heartbeat check (all checks).
    pub async fn wake(&self) -> bool {
        self.wake_tx.send(WakeCommand::All).await.is_ok()
    }

    /// Trigger a specific heartbeat check by ID.
    pub async fn wake_check(&self, id: String) -> bool {
        self.wake_tx.send(WakeCommand::Specific(id)).await.is_ok()
    }

    /// Get the heartbeat history.
    pub async fn history(&self) -> Vec<HeartbeatRecord> {
        self.history.read().await.clone()
    }

    /// Get the last heartbeat record.
    pub async fn last(&self) -> Option<HeartbeatRecord> {
        self.history.read().await.last().cloned()
    }
}

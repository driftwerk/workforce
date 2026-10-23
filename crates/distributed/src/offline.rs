//! Offline mode — store-and-forward with limited local autonomy.
//!
//! When a local node loses connection to the hub, it queues messages
//! and operates with cached policies.

use std::collections::VecDeque;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Offline mode configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineConfig {
    /// Policy cache TTL in seconds.
    #[serde(default = "default_policy_ttl")]
    pub policy_cache_ttl_secs: u64,
    /// Max queued messages before dropping oldest.
    #[serde(default = "default_max_queue")]
    pub max_queue_size: usize,
    /// Max queue size in bytes.
    #[serde(default = "default_max_queue_bytes")]
    pub max_queue_bytes: usize,
    /// Tools allowed in restricted mode (after policy cache expires).
    #[serde(default = "default_safe_tools")]
    pub safe_tools: Vec<String>,
}

impl Default for OfflineConfig {
    fn default() -> Self {
        Self {
            policy_cache_ttl_secs: default_policy_ttl(),
            max_queue_size: default_max_queue(),
            max_queue_bytes: default_max_queue_bytes(),
            safe_tools: default_safe_tools(),
        }
    }
}

fn default_policy_ttl() -> u64 {
    300
}
fn default_max_queue() -> usize {
    1000
}
fn default_max_queue_bytes() -> usize {
    100 * 1024 * 1024
}
fn default_safe_tools() -> Vec<String> {
    vec![
        "filesystem.read".to_string(),
        "filesystem.list".to_string(),
        "exec.git_status".to_string(),
    ]
}

/// Connection state of a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// Normal operation — connected to hub.
    Connected,
    /// Lost connection, using cached policies.
    Disconnected,
    /// Cached policies expired — restricted to safe tools only.
    Restricted,
    /// Reconnecting and draining queue.
    Reconnecting,
}

/// A queued message waiting for reconnection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub subject: String,
    pub payload: Vec<u8>,
    pub queued_at: DateTime<Utc>,
}

/// Manages offline state and message queue.
pub struct OfflineManager {
    config: OfflineConfig,
    state: ConnectionState,
    queue: VecDeque<QueuedMessage>,
    queue_bytes: usize,
    disconnected_at: Option<DateTime<Utc>>,
    last_policy_refresh: Option<DateTime<Utc>>,
}

impl OfflineManager {
    pub fn new(config: OfflineConfig) -> Self {
        Self {
            config,
            state: ConnectionState::Connected,
            queue: VecDeque::new(),
            queue_bytes: 0,
            disconnected_at: None,
            last_policy_refresh: Some(Utc::now()),
        }
    }

    /// Get current connection state.
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Transition to disconnected state.
    pub fn on_disconnect(&mut self) {
        if self.state == ConnectionState::Connected {
            self.state = ConnectionState::Disconnected;
            self.disconnected_at = Some(Utc::now());
            tracing::warn!("Node disconnected from hub — entering offline mode");
        }
    }

    /// Transition to reconnected state and start draining.
    pub fn on_reconnect(&mut self) {
        self.state = ConnectionState::Reconnecting;
        self.last_policy_refresh = Some(Utc::now());
        tracing::info!(
            queued = self.queue.len(),
            "Reconnected to hub — draining queue"
        );
    }

    /// Complete reconnection (queue drained).
    pub fn on_drain_complete(&mut self) {
        self.state = ConnectionState::Connected;
        self.disconnected_at = None;
        tracing::info!("Queue drained — back to normal operation");
    }

    /// Check and potentially transition to restricted mode.
    pub fn check_state(&mut self) {
        if self.state == ConnectionState::Disconnected {
            if let Some(last_refresh) = self.last_policy_refresh {
                let elapsed = Utc::now() - last_refresh;
                if elapsed > Duration::seconds(self.config.policy_cache_ttl_secs as i64) {
                    self.state = ConnectionState::Restricted;
                    tracing::warn!(
                        "Policy cache expired — entering restricted mode (safe tools only)"
                    );
                }
            }
        }
    }

    /// Check if a tool is allowed in the current state.
    pub fn is_tool_allowed(&self, tool: &str) -> bool {
        match self.state {
            ConnectionState::Connected | ConnectionState::Reconnecting => true,
            ConnectionState::Disconnected => true, // Cached policies still valid
            ConnectionState::Restricted => self
                .config
                .safe_tools
                .iter()
                .any(|t| tool.starts_with(t) || t == "*"),
        }
    }

    /// Queue a message for later delivery.
    pub fn enqueue(&mut self, subject: &str, payload: Vec<u8>) -> bool {
        let size = payload.len();

        // Check limits
        if self.queue.len() >= self.config.max_queue_size {
            self.queue.pop_front(); // Drop oldest
            tracing::warn!("Offline queue full — dropped oldest message");
        }

        if self.queue_bytes + size > self.config.max_queue_bytes {
            // Drop messages until we have space
            while self.queue_bytes + size > self.config.max_queue_bytes {
                if let Some(msg) = self.queue.pop_front() {
                    self.queue_bytes -= msg.payload.len();
                } else {
                    break;
                }
            }
        }

        self.queue_bytes += size;
        self.queue.push_back(QueuedMessage {
            subject: subject.to_string(),
            payload,
            queued_at: Utc::now(),
        });

        true
    }

    /// Drain the next message from the queue.
    pub fn dequeue(&mut self) -> Option<QueuedMessage> {
        if let Some(msg) = self.queue.pop_front() {
            self.queue_bytes -= msg.payload.len();
            Some(msg)
        } else {
            None
        }
    }

    /// Number of queued messages.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Total bytes queued.
    pub fn queue_bytes(&self) -> usize {
        self.queue_bytes
    }

    /// How long we've been disconnected.
    pub fn disconnected_duration(&self) -> Option<chrono::Duration> {
        self.disconnected_at.map(|t| Utc::now() - t)
    }
}

impl Default for OfflineManager {
    fn default() -> Self {
        Self::new(OfflineConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_operation() {
        let mgr = OfflineManager::default();
        assert_eq!(*mgr.state(), ConnectionState::Connected);
        assert!(mgr.is_tool_allowed("anything"));
    }

    #[test]
    fn disconnect_and_reconnect() {
        let mut mgr = OfflineManager::default();

        mgr.on_disconnect();
        assert_eq!(*mgr.state(), ConnectionState::Disconnected);
        assert!(mgr.is_tool_allowed("filesystem.write"));

        mgr.enqueue("test.subject", b"hello".to_vec());
        assert_eq!(mgr.queue_len(), 1);

        mgr.on_reconnect();
        assert_eq!(*mgr.state(), ConnectionState::Reconnecting);

        let msg = mgr.dequeue().unwrap();
        assert_eq!(msg.subject, "test.subject");
        assert_eq!(mgr.queue_len(), 0);

        mgr.on_drain_complete();
        assert_eq!(*mgr.state(), ConnectionState::Connected);
    }

    #[test]
    fn restricted_mode() {
        let mut mgr = OfflineManager::new(OfflineConfig {
            policy_cache_ttl_secs: 0, // Expire immediately
            ..Default::default()
        });

        mgr.on_disconnect();
        mgr.last_policy_refresh = Some(Utc::now() - Duration::seconds(10));
        mgr.check_state();

        assert_eq!(*mgr.state(), ConnectionState::Restricted);
        assert!(mgr.is_tool_allowed("filesystem.read"));
        assert!(mgr.is_tool_allowed("filesystem.list_dir"));
        assert!(!mgr.is_tool_allowed("filesystem.write"));
        assert!(!mgr.is_tool_allowed("exec.run"));
    }

    #[test]
    fn queue_limits() {
        let mut mgr = OfflineManager::new(OfflineConfig {
            max_queue_size: 2,
            ..Default::default()
        });

        mgr.on_disconnect();
        mgr.enqueue("a", b"1".to_vec());
        mgr.enqueue("b", b"2".to_vec());
        mgr.enqueue("c", b"3".to_vec()); // Should drop "a"

        assert_eq!(mgr.queue_len(), 2);
        let msg = mgr.dequeue().unwrap();
        assert_eq!(msg.subject, "b"); // "a" was dropped
    }

    #[test]
    fn queue_byte_limit() {
        let mut mgr = OfflineManager::new(OfflineConfig {
            max_queue_bytes: 10,
            ..Default::default()
        });

        mgr.on_disconnect();
        mgr.enqueue("a", vec![0u8; 6]);
        mgr.enqueue("b", vec![0u8; 6]); // Should drop "a" to make room

        assert_eq!(mgr.queue_len(), 1);
    }

    #[test]
    fn disconnected_duration() {
        let mut mgr = OfflineManager::default();
        assert!(mgr.disconnected_duration().is_none());

        mgr.on_disconnect();
        assert!(mgr.disconnected_duration().is_some());
    }
}

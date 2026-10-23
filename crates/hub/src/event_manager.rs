use std::collections::HashMap;
use std::sync::Arc;

use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, info};

/// Event source status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSourceStatus {
    pub id: String,
    pub name: String,
    pub source_type: String,
    pub status: EventSourceState,
    pub events_received: u64,
    pub last_event: Option<String>,
    pub started_at: Option<String>,
}

/// Running state of an event source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSourceState {
    Running,
    Stopped,
    Error,
    Starting,
}

/// Manages event source lifecycle.
pub struct EventSourceManager {
    sources: Arc<RwLock<HashMap<String, EventSourceStatus>>>,
    handles: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl EventSourceManager {
    pub fn new() -> Self {
        Self {
            sources: Arc::new(RwLock::new(HashMap::new())),
            handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register an event source.
    pub async fn register(&self, id: &str, name: &str, source_type: &str) {
        let status = EventSourceStatus {
            id: id.to_string(),
            name: name.to_string(),
            source_type: source_type.to_string(),
            status: EventSourceState::Stopped,
            events_received: 0,
            last_event: None,
            started_at: None,
        };
        self.sources.write().await.insert(id.to_string(), status);
    }

    /// Start an event source with a future.
    pub async fn start<F>(&self, id: &str, fut: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let sources = Arc::clone(&self.sources);
        let source_id = id.to_string();

        // Update status
        {
            let mut srcs = sources.write().await;
            if let Some(s) = srcs.get_mut(id) {
                s.status = EventSourceState::Running;
                s.started_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        let handle = tokio::spawn({
            let sources = Arc::clone(&sources);
            let id = source_id.clone();
            async move {
                fut.await;
                // Mark as stopped when the future completes
                let mut srcs = sources.write().await;
                if let Some(s) = srcs.get_mut(&id) {
                    s.status = EventSourceState::Stopped;
                }
            }
        });

        self.handles.write().await.insert(source_id, handle);
        Ok(())
    }

    /// Stop an event source.
    pub async fn stop(&self, id: &str) -> bool {
        let mut handles = self.handles.write().await;
        if let Some(handle) = handles.remove(id) {
            handle.abort();
            let mut srcs = self.sources.write().await;
            if let Some(s) = srcs.get_mut(id) {
                s.status = EventSourceState::Stopped;
            }
            info!(id = id, "Event source stopped");
            true
        } else {
            false
        }
    }

    /// Record an event received.
    pub async fn record_event(&self, source_id: &str) {
        let mut srcs = self.sources.write().await;
        if let Some(s) = srcs.get_mut(source_id) {
            s.events_received += 1;
            s.last_event = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Get status of all event sources.
    pub async fn status(&self) -> Vec<EventSourceStatus> {
        self.sources.read().await.values().cloned().collect()
    }

    /// Get status of a specific source.
    pub async fn get_status(&self, id: &str) -> Option<EventSourceStatus> {
        self.sources.read().await.get(id).cloned()
    }

    /// Restart a failed event source.
    pub async fn restart_failed(&self) {
        let sources = self.sources.read().await;
        for (id, status) in sources.iter() {
            if status.status == EventSourceState::Error {
                error!(
                    id = id,
                    "Event source in error state — needs manual restart"
                );
            }
        }
    }
}

impl Default for EventSourceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn event_source_lifecycle() {
        let mgr = EventSourceManager::new();

        mgr.register("gh", "GitHub Webhooks", "webhook").await;
        let status = mgr.status().await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status, EventSourceState::Stopped);

        // Start with a no-op future
        mgr.start("gh", async {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await
        })
        .await
        .unwrap();

        let s = mgr.get_status("gh").await.unwrap();
        assert_eq!(s.status, EventSourceState::Running);

        // Record events
        mgr.record_event("gh").await;
        mgr.record_event("gh").await;
        let s = mgr.get_status("gh").await.unwrap();
        assert_eq!(s.events_received, 2);

        // Stop
        assert!(mgr.stop("gh").await);
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

//! Typed `vector_backend` state. Composes the Weaviate readiness
//! probe with a bounded ring buffer of recent insert-batch failures
//! so the agent surface can distinguish "vectorizer module is down"
//! from "no entities indexed yet" — both look like `vector_entities: 0`
//! at the graph layer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;
use workforce_codebase_knowledge::vector::health::{InsertError, InsertErrorSink};
use workforce_codebase_knowledge::vector::CodeVectorStore;

use super::ring_buffer::RingBuffer;

/// Default insert-error window: 60 seconds. A run of failures inside
/// this window flips the probe to `RecentInsertErrors`; once errors
/// stop, the probe falls back to `Healthy` after the window expires.
pub const DEFAULT_ERROR_WINDOW: Duration = Duration::from_secs(60);

/// Default Weaviate readiness timeout: 2 seconds. Tuned for the
/// `text2vec-ollama` setup where readiness can wait on module init.
pub const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(2);

/// How many recent insert errors to retain. Match the pass-history
/// capacity so a burst of failures during one pass is fully visible.
const ERROR_HISTORY_CAPACITY: usize = 32;

/// Discrete vector-backend state. Always one of three values; the
/// `Unhealthy` variant carries a typed reason.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VectorBackendHealth {
    Disabled,
    Healthy {
        url: String,
        last_checked_at: DateTime<Utc>,
    },
    Unhealthy {
        url: String,
        last_checked_at: DateTime<Utc>,
        reason: UnhealthyReason,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnhealthyReason {
    ReadinessFailed {
        error: String,
    },
    Timeout {
        after_ms: u64,
    },
    RecentInsertErrors {
        window_secs: u64,
        error_count: u64,
        sample_error: String,
    },
}

/// Composes the readiness probe with the recent-insert error stream.
/// One probe per hub process; cheap to clone (Arcs all the way down).
pub struct BackendHealthProbe {
    store: Option<Arc<dyn CodeVectorStore>>,
    recent_errors: Mutex<RingBuffer<InsertError>>,
    error_window: Duration,
    readiness_timeout: Duration,
}

impl BackendHealthProbe {
    pub fn new(store: Option<Arc<dyn CodeVectorStore>>) -> Self {
        Self::with_tuning(store, DEFAULT_ERROR_WINDOW, DEFAULT_READINESS_TIMEOUT)
    }

    pub fn with_tuning(
        store: Option<Arc<dyn CodeVectorStore>>,
        error_window: Duration,
        readiness_timeout: Duration,
    ) -> Self {
        Self {
            store,
            recent_errors: Mutex::new(RingBuffer::new(ERROR_HISTORY_CAPACITY)),
            error_window,
            readiness_timeout,
        }
    }

    /// Snapshot the current backend state. Always returns; readiness
    /// failures and timeouts both translate to `Unhealthy` variants.
    pub async fn current(&self) -> VectorBackendHealth {
        let Some(store) = &self.store else {
            return VectorBackendHealth::Disabled;
        };
        let url = store.base_url().to_string();
        let now = Utc::now();
        match tokio::time::timeout(self.readiness_timeout, store.health_check()).await {
            Err(_) => VectorBackendHealth::Unhealthy {
                url,
                last_checked_at: now,
                reason: UnhealthyReason::Timeout {
                    after_ms: self.readiness_timeout.as_millis() as u64,
                },
            },
            Ok(Err(e)) => VectorBackendHealth::Unhealthy {
                url,
                last_checked_at: now,
                reason: UnhealthyReason::ReadinessFailed {
                    error: e.to_string(),
                },
            },
            Ok(Ok(false)) => VectorBackendHealth::Unhealthy {
                url,
                last_checked_at: now,
                reason: UnhealthyReason::ReadinessFailed {
                    error: "readiness endpoint returned non-success".to_string(),
                },
            },
            Ok(Ok(true)) => self.recent_errors_state(url, now),
        }
    }

    fn recent_errors_state(&self, url: String, now: DateTime<Utc>) -> VectorBackendHealth {
        let in_window = self.recent_errors_within_window(now);
        if in_window.is_empty() {
            return VectorBackendHealth::Healthy {
                url,
                last_checked_at: now,
            };
        }
        let sample = in_window
            .last()
            .map(|e| e.message.clone())
            .unwrap_or_default();
        VectorBackendHealth::Unhealthy {
            url,
            last_checked_at: now,
            reason: UnhealthyReason::RecentInsertErrors {
                window_secs: self.error_window.as_secs(),
                error_count: in_window.len() as u64,
                sample_error: sample,
            },
        }
    }

    fn recent_errors_within_window(&self, now: DateTime<Utc>) -> Vec<InsertError> {
        let window = chrono::Duration::from_std(self.error_window)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        let cutoff = now - window;
        let Ok(buf) = self.recent_errors.lock() else {
            return Vec::new();
        };
        buf.iter().filter(|e| e.at >= cutoff).cloned().collect()
    }
}

impl InsertErrorSink for BackendHealthProbe {
    fn record(&self, message: String) {
        if let Ok(mut buf) = self.recent_errors.lock() {
            buf.push(InsertError {
                at: Utc::now(),
                message,
            });
        }
    }
}

/// Log on `VectorBackendHealth` transitions. Compares the
/// state-tag against the previous state; identical states are
/// silenced so the log stream isn't noisy when nothing changes.
pub fn log_state_transition(previous: Option<&VectorBackendHealth>, current: &VectorBackendHealth) {
    let prev_tag = previous.map(state_tag);
    let curr_tag = state_tag(current);
    if prev_tag == Some(curr_tag) {
        return;
    }
    match current {
        VectorBackendHealth::Healthy { .. } => tracing::info!(
            previous = prev_tag.unwrap_or("none"),
            current = curr_tag,
            "vector_backend state changed"
        ),
        VectorBackendHealth::Disabled => tracing::info!(
            previous = prev_tag.unwrap_or("none"),
            current = curr_tag,
            "vector_backend state changed"
        ),
        VectorBackendHealth::Unhealthy { reason, .. } => warn!(
            previous = prev_tag.unwrap_or("none"),
            current = curr_tag,
            reason = ?reason,
            "vector_backend state changed"
        ),
    }
}

fn state_tag(state: &VectorBackendHealth) -> &'static str {
    match state {
        VectorBackendHealth::Disabled => "disabled",
        VectorBackendHealth::Healthy { .. } => "healthy",
        VectorBackendHealth::Unhealthy { .. } => "unhealthy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_when_no_store() {
        let probe = BackendHealthProbe::new(None);
        assert_eq!(probe.current().await, VectorBackendHealth::Disabled);
    }

    #[tokio::test]
    async fn unhealthy_when_readiness_returns_false() {
        // Point at an address that nothing is listening on so the HTTP
        // call fails fast — health_check returns Ok(false) in that case.
        let store: Arc<dyn CodeVectorStore> = Arc::new(
            workforce_codebase_knowledge::vector::WeaviateCodeVectorStore::new(
                "http://127.0.0.1:1",
                None,
            ),
        );
        let probe = BackendHealthProbe::with_tuning(
            Some(store),
            Duration::from_secs(60),
            Duration::from_secs(2),
        );
        match probe.current().await {
            VectorBackendHealth::Unhealthy { reason, .. } => match reason {
                UnhealthyReason::ReadinessFailed { .. } | UnhealthyReason::Timeout { .. } => {}
                other => panic!("unexpected reason: {other:?}"),
            },
            other => panic!("expected Unhealthy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_vellev_store_is_probed_as_weaviate_is() {
        let down: Arc<dyn CodeVectorStore> = Arc::new(
            workforce_codebase_knowledge::vector::VellevCodeVectorStore::new(
                "http://127.0.0.1:1",
                "key",
                "workforce",
            )
            .unwrap(),
        );
        let probe = BackendHealthProbe::with_tuning(
            Some(down),
            Duration::from_secs(60),
            Duration::from_secs(2),
        );
        assert!(matches!(
            probe.current().await,
            VectorBackendHealth::Unhealthy { .. }
        ));
        let Some(node) = workforce_vellev::testing::Node::fresh().await else {
            return;
        };
        let up: Arc<dyn CodeVectorStore> = Arc::new(
            workforce_codebase_knowledge::vector::VellevCodeVectorStore::new(
                &node.url,
                &node.key,
                &node.tenant,
            )
            .unwrap(),
        );
        let probe = BackendHealthProbe::new(Some(up));
        assert!(matches!(
            probe.current().await,
            VectorBackendHealth::Healthy { ref url, .. } if *url == node.url
        ));
    }

    /// A code collection of the wrong shape, such as one vellev's importer
    /// copied from Weaviate with its vectors, shows as unhealthy, saying why.
    #[tokio::test]
    async fn a_refused_vellev_collection_is_unhealthy_with_the_reason() {
        let Some(node) = workforce_vellev::testing::Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        let imported = workforce_vellev::collection(
            vellev_core::CollectionName::new("CodeEntity").unwrap(),
            vec![vellev_core::Property::new(
                "repoId",
                vellev_core::DataType::Text,
            )],
            vellev_core::Embedding {
                provider: vellev_core::EmbeddingProvider::None,
                model: "weaviate:text2vec-transformers".into(),
                dimensions: 384,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
        );
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        let store: Arc<dyn CodeVectorStore> = Arc::new(
            workforce_codebase_knowledge::vector::VellevCodeVectorStore::new(
                &node.url,
                &node.key,
                &node.tenant,
            )
            .unwrap(),
        );
        // Given longer than the two seconds a status page waits: this is
        // about what is said, not how soon.
        let probe = BackendHealthProbe::with_tuning(
            Some(store),
            DEFAULT_ERROR_WINDOW,
            Duration::from_secs(60),
        );
        match probe.current().await {
            VectorBackendHealth::Unhealthy {
                reason: UnhealthyReason::ReadinessFailed { error },
                ..
            } => assert!(
                error.contains("copied from Weaviate")
                    && error.contains("run a full index of each repository"),
                "{error}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_appends_to_ring_buffer() {
        let probe = BackendHealthProbe::new(None);
        probe.record("boom 1".into());
        probe.record("boom 2".into());
        let snap = probe.recent_errors_within_window(Utc::now());
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "boom 1");
        assert_eq!(snap[1].message, "boom 2");
    }

    #[tokio::test]
    async fn record_evicts_past_capacity() {
        let probe = BackendHealthProbe::new(None);
        for i in 0..(ERROR_HISTORY_CAPACITY + 5) {
            probe.record(format!("e{i}"));
        }
        let snap = probe.recent_errors_within_window(Utc::now());
        assert_eq!(snap.len(), ERROR_HISTORY_CAPACITY);
        assert_eq!(snap.first().unwrap().message, format!("e{}", 5));
    }

    #[tokio::test]
    async fn errors_older_than_window_filtered_out() {
        let probe =
            BackendHealthProbe::with_tuning(None, Duration::from_secs(1), Duration::from_secs(2));
        {
            let mut buf = probe.recent_errors.lock().unwrap();
            buf.push(InsertError {
                at: Utc::now() - chrono::Duration::seconds(10),
                message: "old".into(),
            });
            buf.push(InsertError {
                at: Utc::now(),
                message: "fresh".into(),
            });
        }
        let snap = probe.recent_errors_within_window(Utc::now());
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].message, "fresh");
    }

    #[test]
    fn state_serialises_with_tag() {
        let v = serde_json::to_value(VectorBackendHealth::Disabled).unwrap();
        assert_eq!(v, serde_json::json!({ "state": "disabled" }));
        let v = serde_json::to_value(VectorBackendHealth::Healthy {
            url: "u".into(),
            last_checked_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        })
        .unwrap();
        assert_eq!(v["state"], "healthy");
        assert_eq!(v["url"], "u");
    }

    #[test]
    fn unhealthy_reason_serialises_with_kind() {
        let v = serde_json::to_value(UnhealthyReason::Timeout { after_ms: 2000 }).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "kind": "timeout", "after_ms": 2000 })
        );
        let v =
            serde_json::to_value(UnhealthyReason::ReadinessFailed { error: "x".into() }).unwrap();
        assert_eq!(v["kind"], "readiness_failed");
    }
}

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
use workforce_knowledge_service::RepoVectors;

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

/// Discrete vector-backend state. The `Unhealthy` variant carries a typed
/// reason.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VectorBackendHealth {
    Disabled,
    Healthy {
        url: String,
        /// The tenant the store keeps its vectors in, where it has one.
        #[serde(skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
        last_checked_at: DateTime<Utc>,
    },
    /// Ready, with what the store already held of some repo being compared
    /// with the graph and mended, searched meanwhile: `complete` of
    /// `repos` are recorded complete.
    Adopting {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
        last_checked_at: DateTime<Utc>,
        complete: usize,
        repos: usize,
    },
    Unhealthy {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
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

/// What a probe found of a store itself: where it is, when it looked, and
/// why it is unhealthy, if it is.
struct Checked {
    url: String,
    tenant: Option<String>,
    at: DateTime<Utc>,
    failed: Option<UnhealthyReason>,
}

/// The state of a store found as `checked`, keeping `repos`' code vectors:
/// a failure outranks all, then any repo being adopted.
fn state(checked: Checked, repos: &[RepoVectors]) -> VectorBackendHealth {
    let Checked {
        url,
        tenant,
        at,
        failed,
    } = checked;
    match failed {
        Some(reason) => VectorBackendHealth::Unhealthy {
            url,
            tenant,
            last_checked_at: at,
            reason,
        },
        None if repos.iter().any(RepoVectors::adopting) => VectorBackendHealth::Adopting {
            url,
            tenant,
            last_checked_at: at,
            complete: repos.iter().filter(|repo| repo.complete).count(),
            repos: repos.len(),
        },
        None => VectorBackendHealth::Healthy {
            url,
            tenant,
            last_checked_at: at,
        },
    }
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
    /// failures and timeouts both translate to `Unhealthy` variants. A
    /// store that keeps track of its repos' vectors is `Adopting` while
    /// this process's knowledge services last saw one being adopted.
    pub async fn current(&self) -> VectorBackendHealth {
        let Some(store) = &self.store else {
            return VectorBackendHealth::Disabled;
        };
        let at = Utc::now();
        let failed = match tokio::time::timeout(self.readiness_timeout, store.health_check()).await
        {
            Err(_) => Some(UnhealthyReason::Timeout {
                after_ms: self.readiness_timeout.as_millis() as u64,
            }),
            Ok(Err(e)) => Some(UnhealthyReason::ReadinessFailed {
                error: e.to_string(),
            }),
            Ok(Ok(false)) => Some(UnhealthyReason::ReadinessFailed {
                error: "readiness endpoint returned non-success".to_string(),
            }),
            Ok(Ok(true)) => self.recent_errors_reason(at),
        };
        let repos = match store.tracks_completeness() {
            true => super::wire::code_vector_status(),
            false => Vec::new(),
        };
        let checked = Checked {
            url: store.base_url().to_string(),
            tenant: store.tenant(),
            at,
            failed,
        };
        state(checked, &repos)
    }

    /// Why the store is unhealthy for the insert errors it had within the
    /// window, if it had any.
    fn recent_errors_reason(&self, now: DateTime<Utc>) -> Option<UnhealthyReason> {
        let in_window = self.recent_errors_within_window(now);
        let sample = in_window.last()?.message.clone();
        Some(UnhealthyReason::RecentInsertErrors {
            window_secs: self.error_window.as_secs(),
            error_count: in_window.len() as u64,
            sample_error: sample,
        })
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
        VectorBackendHealth::Healthy { .. }
        | VectorBackendHealth::Adopting { .. }
        | VectorBackendHealth::Disabled => tracing::info!(
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
        VectorBackendHealth::Adopting { .. } => "adopting",
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
            VectorBackendHealth::Healthy { ref url, ref tenant, .. }
                if *url == node.url && *tenant == Some(node.tenant.clone())
        ));
    }

    /// A code store whose node does not serve its tenant is unhealthy,
    /// naming the node and the tenant.
    #[tokio::test]
    async fn a_misrouted_code_store_is_unhealthy_naming_its_node_and_tenant() {
        let url = workforce_vellev::testing::NotServing::up().await.url;
        let store: Arc<dyn CodeVectorStore> = Arc::new(
            workforce_codebase_knowledge::vector::VellevCodeVectorStore::new(&url, "key", "code")
                .unwrap(),
        );
        let probe = BackendHealthProbe::new(Some(store));
        match probe.current().await {
            VectorBackendHealth::Unhealthy {
                tenant,
                reason: UnhealthyReason::ReadinessFailed { error },
                ..
            } => {
                assert_eq!(tenant.as_deref(), Some("code"));
                assert!(
                    error.contains("misrouted") && error.contains("'code'") && error.contains(&url),
                    "{error}"
                );
            }
            other => panic!("expected a misroute, got {other:?}"),
        }
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
        let healthy = |tenant: Option<&str>| {
            serde_json::to_value(VectorBackendHealth::Healthy {
                url: "u".into(),
                tenant: tenant.map(str::to_string),
                last_checked_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            })
            .unwrap()
        };
        let v = healthy(None);
        assert_eq!(v["state"], "healthy");
        assert_eq!(v["url"], "u");
        assert!(v.get("tenant").is_none(), "weaviate has none: {v}");
        assert_eq!(healthy(Some("code"))["tenant"], "code");
    }

    /// Repos as a knowledge service saw them: `complete` of them recorded
    /// complete, then `adopting` being adopted, then the rest filled.
    fn repos(complete: usize, adopting: usize, of: usize) -> Vec<RepoVectors> {
        use workforce_codebase_knowledge::graph::vectors::{FillStage, VectorFill};
        (0..of)
            .map(|n| RepoVectors {
                repo_id: format!("r{n}"),
                name: format!("r{n}"),
                complete: n < complete,
                fill: Some(VectorFill {
                    generation: "g".into(),
                    collection: "c".into(),
                    stage: match n {
                        n if n < complete => FillStage::Done,
                        n if n < complete + adopting => FillStage::AdoptEntities,
                        _ => FillStage::Entities,
                    },
                    cursor: String::new(),
                    done: 0,
                    total: 10,
                }),
                pending_files: 0,
                last_error: None,
                compared_secs_ago: None,
            })
            .collect()
    }

    /// A store with a repo being adopted is adopting, saying how many of
    /// its repos are complete; one that failed is unhealthy whatever its
    /// repos are; one adopting none is healthy.
    #[test]
    fn an_adopting_store_is_adopting_n_of_m_and_unhealthy_outranks_it() {
        let at = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let checked = |failed| Checked {
            url: "u".into(),
            tenant: Some("code".into()),
            at,
            failed,
        };
        let adopting = state(checked(None), &repos(1, 1, 3));
        assert_eq!(
            adopting,
            VectorBackendHealth::Adopting {
                url: "u".into(),
                tenant: Some("code".into()),
                last_checked_at: at,
                complete: 1,
                repos: 3,
            }
        );
        let said = serde_json::to_value(&adopting).unwrap();
        assert_eq!(
            (said["state"].clone(), said["complete"].clone()),
            ("adopting".into(), 1.into())
        );
        let failed = UnhealthyReason::RecentInsertErrors {
            window_secs: 60,
            error_count: 1,
            sample_error: "refused".into(),
        };
        assert!(matches!(
            state(checked(Some(failed)), &repos(1, 1, 3)),
            VectorBackendHealth::Unhealthy { .. }
        ));
        assert!(matches!(
            state(checked(None), &repos(1, 0, 3)),
            VectorBackendHealth::Healthy { .. }
        ));
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

//! Runtime engagement mode overrides.
//!
//! A per-agent, per-channel (optionally per-thread) response-mode override that
//! takes effect immediately without restart. Scoping by agent keeps one bot's
//! mode independent of every other bot sharing the same channel.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::settings::ResponseMode;

/// Identifies the agent, channel, and optional thread an override applies to.
///
/// Thread-level scopes (`thread: Some`) take precedence over channel-level
/// scopes (`thread: None`) for the same agent and channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EngagementScope {
    pub agent_id: String,
    pub channel: String,
    pub thread: Option<String>,
}

impl EngagementScope {
    /// Build a scope, treating an empty thread id as channel-level.
    pub fn new(
        agent_id: impl Into<String>,
        channel: impl Into<String>,
        thread: Option<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            channel: channel.into(),
            thread: thread.filter(|t| !t.is_empty()),
        }
    }
}

/// The override payload stored against a scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementOverride {
    pub mode: ResponseMode,
    pub set_by: String,
    pub set_at: DateTime<Utc>,
}

/// A scope and its override flattened into one record — the persistence and
/// listing shape, so neither callers nor the vault parse composite keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementRecord {
    pub agent_id: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub mode: ResponseMode,
    pub set_by: String,
    pub set_at: DateTime<Utc>,
}

impl EngagementRecord {
    fn split(self) -> (EngagementScope, EngagementOverride) {
        (
            EngagementScope::new(self.agent_id, self.channel, self.thread),
            EngagementOverride {
                mode: self.mode,
                set_by: self.set_by,
                set_at: self.set_at,
            },
        )
    }

    fn join(scope: &EngagementScope, over: &EngagementOverride) -> Self {
        Self {
            agent_id: scope.agent_id.clone(),
            channel: scope.channel.clone(),
            thread: scope.thread.clone(),
            mode: over.mode.clone(),
            set_by: over.set_by.clone(),
            set_at: over.set_at,
        }
    }
}

/// Runtime engagement mode overrides, shared hub-wide across event handlers,
/// the `messaging.*_mode` tools, and the admin API.
pub struct EngagementOverrides {
    overrides: RwLock<HashMap<EngagementScope, EngagementOverride>>,
}

impl EngagementOverrides {
    pub fn new() -> Self {
        Self {
            overrides: RwLock::new(HashMap::new()),
        }
    }

    /// Replace all overrides (e.g. from the vault on startup).
    pub async fn load(&self, records: Vec<EngagementRecord>) {
        let mut guard = self.overrides.write().await;
        *guard = records.into_iter().map(EngagementRecord::split).collect();
    }

    /// Set or update an override for the given scope.
    pub async fn set(&self, scope: EngagementScope, mode: ResponseMode, set_by: &str) {
        let mut guard = self.overrides.write().await;
        guard.insert(
            scope,
            EngagementOverride {
                mode,
                set_by: set_by.to_string(),
                set_at: Utc::now(),
            },
        );
    }

    /// Remove an override. Returns `true` if it existed.
    pub async fn remove(&self, scope: &EngagementScope) -> bool {
        let mut guard = self.overrides.write().await;
        guard.remove(scope).is_some()
    }

    /// Resolve the effective mode for an agent in a channel/thread.
    ///
    /// Thread-level scope wins over channel-level; `None` means no runtime
    /// override, so the caller falls back to config defaults.
    pub async fn resolve(
        &self,
        agent_id: &str,
        channel: &str,
        thread: Option<&str>,
    ) -> Option<ResponseMode> {
        let guard = self.overrides.read().await;
        let lookup = |thread: Option<String>| {
            guard
                .get(&EngagementScope::new(agent_id, channel, thread))
                .map(|entry| entry.mode.clone())
        };
        thread
            .and_then(|ts| lookup(Some(ts.to_string())))
            .or_else(|| lookup(None))
    }

    /// All overrides as flat records, optionally filtered to one agent.
    pub async fn list(&self, agent_id: Option<&str>) -> Vec<EngagementRecord> {
        let guard = self.overrides.read().await;
        guard
            .iter()
            .filter(|(scope, _)| agent_id.is_none_or(|id| scope.agent_id == id))
            .map(|(scope, over)| EngagementRecord::join(scope, over))
            .collect()
    }

    /// Serialize all overrides to JSON for vault persistence.
    pub async fn snapshot_json(&self) -> String {
        serde_json::to_string(&self.list(None).await).unwrap_or_else(|_| "[]".to_string())
    }
}

impl Default for EngagementOverrides {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(agent: &str, channel: &str, thread: Option<&str>) -> EngagementScope {
        EngagementScope::new(agent, channel, thread.map(str::to_string))
    }

    #[tokio::test]
    async fn set_and_resolve_for_agent() {
        let store = EngagementOverrides::new();
        store
            .set(scope("atlas", "C1", None), ResponseMode::All, "u1")
            .await;

        assert_eq!(
            store.resolve("atlas", "C1", None).await,
            Some(ResponseMode::All)
        );
        assert_eq!(store.resolve("atlas", "C2", None).await, None);
    }

    #[tokio::test]
    async fn one_agent_override_does_not_affect_another() {
        let store = EngagementOverrides::new();
        store
            .set(scope("bernard", "C1", None), ResponseMode::All, "u1")
            .await;

        assert_eq!(
            store.resolve("bernard", "C1", None).await,
            Some(ResponseMode::All)
        );
        assert_eq!(store.resolve("atlas", "C1", None).await, None);
        assert_eq!(store.resolve("atlas", "C1", Some("t.1")).await, None);
    }

    #[tokio::test]
    async fn thread_overrides_channel_within_agent() {
        let store = EngagementOverrides::new();
        store
            .set(scope("atlas", "C1", None), ResponseMode::All, "u1")
            .await;
        store
            .set(scope("atlas", "C1", Some("t.1")), ResponseMode::Quiet, "u1")
            .await;

        assert_eq!(
            store.resolve("atlas", "C1", Some("t.1")).await,
            Some(ResponseMode::Quiet)
        );
        assert_eq!(
            store.resolve("atlas", "C1", Some("t.9")).await,
            Some(ResponseMode::All)
        );
        assert_eq!(
            store.resolve("atlas", "C1", None).await,
            Some(ResponseMode::All)
        );
    }

    #[tokio::test]
    async fn empty_thread_is_channel_level() {
        let store = EngagementOverrides::new();
        store
            .set(
                EngagementScope::new("atlas", "C1", Some(String::new())),
                ResponseMode::All,
                "u1",
            )
            .await;
        assert_eq!(
            store.resolve("atlas", "C1", None).await,
            Some(ResponseMode::All)
        );
    }

    #[tokio::test]
    async fn remove_override() {
        let store = EngagementOverrides::new();
        store
            .set(scope("atlas", "C1", None), ResponseMode::Quiet, "u1")
            .await;
        assert!(store.remove(&scope("atlas", "C1", None)).await);
        assert!(!store.remove(&scope("atlas", "C1", None)).await);
        assert_eq!(store.resolve("atlas", "C1", None).await, None);
    }

    #[tokio::test]
    async fn list_filters_by_agent() {
        let store = EngagementOverrides::new();
        store
            .set(scope("atlas", "C1", None), ResponseMode::All, "u1")
            .await;
        store
            .set(scope("bernard", "C2", None), ResponseMode::Quiet, "u2")
            .await;

        assert_eq!(store.list(None).await.len(), 2);
        let only = store.list(Some("atlas")).await;
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].agent_id, "atlas");
        assert_eq!(only[0].channel, "C1");
    }

    #[tokio::test]
    async fn snapshot_load_roundtrip() {
        let store = EngagementOverrides::new();
        store
            .set(scope("atlas", "C1", None), ResponseMode::All, "u1")
            .await;
        store
            .set(
                scope("bernard", "C2", Some("t.1")),
                ResponseMode::Quiet,
                "u2",
            )
            .await;

        let json = store.snapshot_json().await;
        let records: Vec<EngagementRecord> = serde_json::from_str(&json).expect("valid JSON");

        let restored = EngagementOverrides::new();
        restored.load(records).await;
        assert_eq!(
            restored.resolve("atlas", "C1", None).await,
            Some(ResponseMode::All)
        );
        assert_eq!(
            restored.resolve("bernard", "C2", Some("t.1")).await,
            Some(ResponseMode::Quiet)
        );
    }

    #[tokio::test]
    async fn legacy_object_format_does_not_parse_as_records() {
        let legacy = r#"{"C1":{"mode":"all","set_by":"agent","set_at":"2026-06-17T10:00:00Z"}}"#;
        assert!(serde_json::from_str::<Vec<EngagementRecord>>(legacy).is_err());
    }
}

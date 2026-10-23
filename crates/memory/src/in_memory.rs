use parking_lot::RwLock;
use std::collections::HashMap;

use async_trait::async_trait;
use eyre::Result;

use crate::store::{apply_pollution_default, passes_excluded_tags, MemoryStore, IMPORTANCE_FLOOR};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};

/// In-memory implementation of MemoryStore.
///
/// Uses simple keyword matching for "semantic" search (no actual embeddings).
/// Useful for testing, development, and environments without Weaviate.
pub struct InMemoryStore {
    memories: RwLock<HashMap<MemoryId, Memory>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            memories: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        let id = memory.id.clone();
        self.memories.write().insert(id.clone(), memory);
        Ok(id)
    }

    async fn recall(
        &self,
        query: &str,
        mut filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        if !crate::store::check_and_audit_scope(
            &filter,
            crate::access_audit::AuditOperation::Recall,
        ) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let memories = self.memories.read();
        let query_lower = query.to_lowercase();
        let query_words: Vec<&str> = query_lower.split_whitespace().collect();

        let limit = filter.limit.unwrap_or(10);

        let mut results: Vec<MemorySearchResult> = memories
            .values()
            .filter(|m| !m.deleted)
            .filter(|m| apply_filter(m, &filter))
            .map(|m| {
                let score = keyword_score(&m.content, &query_words);
                MemorySearchResult {
                    memory: m.clone(),
                    score,
                }
            })
            .filter(|r| r.score > 0.0)
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);

        Ok(crate::store::enforce_access_control(
            &filter,
            crate::access_audit::AuditOperation::Recall,
            results,
        ))
    }

    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        Ok(self.memories.read().get(id).cloned())
    }

    async fn forget(&self, id: &MemoryId) -> Result<()> {
        if let Some(mem) = self.memories.write().get_mut(id) {
            mem.soft_delete();
        }
        Ok(())
    }

    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        if let Some(mem) = self.memories.write().get_mut(id) {
            mem.visibility = visibility;
        }
        Ok(())
    }

    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        if !crate::store::check_and_audit_scope(&filter, crate::access_audit::AuditOperation::Count)
        {
            return Ok(0);
        }
        let memories = self.memories.read();
        let count = memories
            .values()
            .filter(|m| !m.deleted)
            .filter(|m| apply_filter(m, &filter))
            .count();
        Ok(count)
    }

    async fn list(&self, mut filter: MemoryFilter) -> Result<Vec<Memory>> {
        if !crate::store::check_and_audit_scope(&filter, crate::access_audit::AuditOperation::List)
        {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let memories = self.memories.read();
        let limit = filter.limit.unwrap_or(100);
        let results: Vec<Memory> = memories
            .values()
            .filter(|m| !m.deleted)
            .filter(|m| apply_filter(m, &filter))
            .take(limit)
            .cloned()
            .collect();
        Ok(crate::store::enforce_access_control(
            &filter,
            crate::access_audit::AuditOperation::List,
            results,
        ))
    }

    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        if let Some(mem) = self.memories.write().get_mut(id) {
            mem.tags = tags;
        }
        Ok(())
    }

    async fn identity_memories(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<Vec<MemoryId>> {
        Ok(self
            .memories
            .read()
            .values()
            .filter(|m| &m.owner == owner && m.identity_id.as_deref() == Some(identity))
            .map(|m| m.id.clone())
            .collect())
    }

    async fn identity_counts(
        &self,
        owner: &workforce_core::types::AgentId,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        Ok(self
            .memories
            .read()
            .values()
            .filter(|m| &m.owner == owner)
            .filter_map(|m| m.identity_id.clone().filter(|id| !id.is_empty()))
            .fold(std::collections::BTreeMap::new(), |mut counts, id| {
                *counts.entry(id).or_default() += 1;
                counts
            }))
    }

    async fn reassign_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        from: &str,
        to: &str,
    ) -> Result<usize> {
        let mut memories = self.memories.write();
        let moved = memories
            .values_mut()
            .filter(|m| &m.owner == owner && m.identity_id.as_deref() == Some(from))
            .map(|m| m.identity_id = Some(to.to_string()))
            .count();
        Ok(moved)
    }

    async fn purge_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<usize> {
        let mut memories = self.memories.write();
        let before = memories.len();
        memories.retain(|_, m| !(&m.owner == owner && m.identity_id.as_deref() == Some(identity)));
        Ok(before - memories.len())
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        let mut memories = self.memories.write();
        Ok(ids
            .iter()
            .filter(|id| memories.remove(id).is_some())
            .count())
    }

    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        let mut guard = self.memories.write();
        let mem = guard
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("memory {} not found", id.0))?;
        let new = (mem.importance + delta).clamp(IMPORTANCE_FLOOR, 1.0);
        mem.importance = new;
        Ok(new)
    }

    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        let mut guard = self.memories.write();
        let mem = guard
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("memory {} not found", id.0))?;
        mem.recurrence_count = mem.recurrence_count.saturating_add(1);
        let now = chrono::Utc::now();
        mem.last_recurrence_at = Some(now);
        mem.last_accessed = now;
        for new_id in additional_message_ids {
            if !new_id.is_empty() && !mem.source_message_ids.contains(&new_id) {
                mem.source_message_ids.push(new_id);
            }
        }
        Ok(mem.recurrence_count)
    }
}

/// Simple keyword-based scoring (no embeddings).
fn keyword_score(content: &str, query_words: &[&str]) -> f32 {
    if query_words.is_empty() {
        return 0.0;
    }
    let content_lower = content.to_lowercase();
    let matches = query_words
        .iter()
        .filter(|w| content_lower.contains(**w))
        .count();
    matches as f32 / query_words.len() as f32
}

/// Apply filter criteria to a memory.
fn apply_filter(memory: &Memory, filter: &MemoryFilter) -> bool {
    if let Some(ref layer) = filter.layer {
        if memory.layer != *layer {
            return false;
        }
    }
    if let Some(ref owner) = filter.owner {
        if memory.owner != *owner {
            return false;
        }
    }
    if let Some(ref vis) = filter.visibility {
        if memory.visibility != *vis {
            return false;
        }
    }
    if let Some(ref tags) = filter.tags {
        if !tags.iter().any(|t| memory.tags.contains(t)) {
            return false;
        }
    }
    if let Some(min_imp) = filter.min_importance {
        if memory.importance < min_imp {
            return false;
        }
    }
    if let Some(ref src) = filter.source {
        if memory.source != *src {
            return false;
        }
    }
    if let Some(ref filter_id) = filter.identity_id {
        let is_own = memory.identity_id.as_ref().is_none_or(|id| id == filter_id);
        let is_team = filter
            .team_id
            .as_ref()
            .is_some_and(|tid| memory.visibility == Visibility::Team(tid.clone()));
        if !is_own && !is_team {
            return false;
        }
    }
    if !passes_excluded_tags(memory, &filter.excluded_tags) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryLayer, MemorySource, Visibility};
    use workforce_core::types::AgentId;

    #[tokio::test]
    async fn identity_memories_are_reassigned_and_purged_forgotten_ones_included() {
        crate::store::identity_operations_contract(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn remember_and_recall() {
        let store = InMemoryStore::new();

        let mem = Memory::new(
            "Rust uses the borrow checker for memory safety",
            MemoryLayer::Semantic,
            AgentId::new("dev"),
        );
        store.remember(mem).await.unwrap();

        let mem2 = Memory::new(
            "PostgreSQL supports JSONB columns for structured data",
            MemoryLayer::Semantic,
            AgentId::new("dev"),
        );
        store.remember(mem2).await.unwrap();

        let results = store
            .recall("borrow checker Rust", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].memory.content.contains("borrow checker"));
    }

    #[tokio::test]
    async fn recall_with_filter() {
        let store = InMemoryStore::new();

        let mem1 = Memory::new("Rust fact", MemoryLayer::Semantic, AgentId::new("dev-1"));
        let mem2 = Memory::new("Rust tip", MemoryLayer::Episodic, AgentId::new("dev-2"));
        store.remember(mem1).await.unwrap();
        store.remember(mem2).await.unwrap();

        let filter = MemoryFilter {
            layer: Some(MemoryLayer::Semantic),
            ..MemoryFilter::system_unscoped()
        };
        let results = store.recall("Rust", filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].memory.content.contains("fact"));
    }

    #[tokio::test]
    async fn forget_excludes_from_recall() {
        let store = InMemoryStore::new();

        let mem = Memory::new("secret info", MemoryLayer::Episodic, AgentId::new("dev"));
        let id = store.remember(mem).await.unwrap();

        let results = store
            .recall("secret", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        store.forget(&id).await.unwrap();

        let results = store
            .recall("secret", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    async fn share_changes_visibility() {
        let store = InMemoryStore::new();
        let mem = Memory::new("private info", MemoryLayer::Episodic, AgentId::new("dev"));
        let id = store.remember(mem).await.unwrap();

        let stored = store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.visibility, Visibility::Private);

        store
            .share(&id, Visibility::Team("backend".to_string()))
            .await
            .unwrap();

        let stored = store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.visibility, Visibility::Team("backend".to_string()));
    }

    #[tokio::test]
    async fn recall_excludes_polluted_kinds_by_default() {
        let store = InMemoryStore::new();
        store
            .remember(Memory::new(
                "user shipped a feature",
                MemoryLayer::Episodic,
                AgentId::new("a"),
            ))
            .await
            .unwrap();
        store
            .remember(
                Memory::new(
                    "user shipped a feature too",
                    MemoryLayer::Episodic,
                    AgentId::new("a"),
                )
                .with_tags(vec!["kind:recap_artifact".to_string()]),
            )
            .await
            .unwrap();

        let results = store
            .recall("user shipped", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "default MemoryFilter must exclude kind:recap_artifact"
        );
        assert!(!results[0]
            .memory
            .tags
            .iter()
            .any(|t| t == "kind:recap_artifact"));
    }

    #[tokio::test]
    async fn recall_returns_polluted_when_include_polluted_set() {
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new("forensic target", MemoryLayer::Episodic, AgentId::new("a"))
                    .with_tags(vec!["kind:consolidation_meta".to_string()]),
            )
            .await
            .unwrap();

        let forensic = store
            .recall(
                "forensic",
                MemoryFilter {
                    include_polluted: true,
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(forensic.len(), 1);
    }

    #[tokio::test]
    async fn recall_fails_closed_when_identity_unset_and_not_system_unscoped() {
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new(
                    "alice's secret",
                    MemoryLayer::Episodic,
                    AgentId::new("agent"),
                )
                .with_identity_id("alice"),
            )
            .await
            .unwrap();

        let leaky = MemoryFilter::default();
        assert!(!crate::store::is_identity_scope_authorised(&leaky));
        let leaked = store.recall("secret", leaky).await.unwrap();
        assert!(
            leaked.is_empty(),
            "fail-closed: no identity should yield empty results"
        );

        let scoped = MemoryFilter {
            identity_id: Some("alice".to_string()),
            ..Default::default()
        };
        let owned = store.recall("secret", scoped).await.unwrap();
        assert_eq!(owned.len(), 1);
    }

    #[tokio::test]
    async fn access_control_drops_other_users_memories_even_if_filter_leaks() {
        // DW-525 defence-in-depth: if a backend bug ever returned a row whose
        // identity_id didn't match the requesting user, the user-centric
        // AccessController applied as a post-filter must still drop it.
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new("alice's note", MemoryLayer::Episodic, AgentId::new("agent"))
                    .with_identity_id("alice"),
            )
            .await
            .unwrap();
        store
            .remember(
                Memory::new("bob's note", MemoryLayer::Episodic, AgentId::new("agent"))
                    .with_identity_id("bob"),
            )
            .await
            .unwrap();

        let filter = MemoryFilter {
            identity_id: Some("alice".to_string()),
            ..Default::default()
        };
        let results = store.recall("note", filter).await.unwrap();
        assert!(results
            .iter()
            .all(|r| r.memory.identity_id.as_deref() == Some("alice")));
    }

    #[tokio::test]
    async fn count_fails_closed_when_identity_unset_and_not_system_unscoped() {
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new("a", MemoryLayer::Episodic, AgentId::new("agent"))
                    .with_identity_id("alice"),
            )
            .await
            .unwrap();

        assert_eq!(store.count(MemoryFilter::default()).await.unwrap(), 0);
        assert_eq!(
            store.count(MemoryFilter::system_unscoped()).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn list_excludes_polluted_kinds_by_default() {
        let store = InMemoryStore::new();
        store
            .remember(Memory::new("a", MemoryLayer::Episodic, AgentId::new("x")))
            .await
            .unwrap();
        store
            .remember(
                Memory::new("b", MemoryLayer::Episodic, AgentId::new("x"))
                    .with_tags(vec!["kind:recap_artifact".to_string()]),
            )
            .await
            .unwrap();

        let visible = store.list(MemoryFilter::system_unscoped()).await.unwrap();
        assert_eq!(visible.len(), 1);
    }

    #[tokio::test]
    async fn caller_supplied_excluded_tags_combine_with_pollution_default() {
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new("alpha", MemoryLayer::Episodic, AgentId::new("x"))
                    .with_tags(vec!["draft".to_string()]),
            )
            .await
            .unwrap();
        store
            .remember(
                Memory::new("alpha topic", MemoryLayer::Episodic, AgentId::new("x"))
                    .with_tags(vec!["kind:recap_artifact".to_string()]),
            )
            .await
            .unwrap();
        store
            .remember(Memory::new(
                "alpha clean",
                MemoryLayer::Episodic,
                AgentId::new("x"),
            ))
            .await
            .unwrap();

        let results = store
            .recall(
                "alpha",
                MemoryFilter {
                    excluded_tags: vec!["draft".to_string()],
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.content, "alpha clean");
    }

    #[tokio::test]
    async fn record_recurrence_increments_count_and_dedups_message_ids() {
        let store = InMemoryStore::new();
        let mem = Memory::new(
            "the user keeps hitting the same wall",
            MemoryLayer::Episodic,
            AgentId::new("a"),
        )
        .with_source_session("sess-1", vec!["m-1".into()]);
        let id = store.remember(mem).await.unwrap();

        let count = store
            .record_recurrence(&id, vec!["m-2".into(), "m-1".into()])
            .await
            .unwrap();
        assert_eq!(count, 1);

        let stored = store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.recurrence_count, 1);
        assert!(stored.last_recurrence_at.is_some());
        assert_eq!(stored.source_message_ids, vec!["m-1", "m-2"]);

        let count = store
            .record_recurrence(&id, vec!["m-3".into()])
            .await
            .unwrap();
        assert_eq!(count, 2);
        let stored = store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.source_message_ids, vec!["m-1", "m-2", "m-3"]);
    }

    #[tokio::test]
    async fn record_recurrence_errors_when_memory_missing() {
        let store = InMemoryStore::new();
        let result = store
            .record_recurrence(&MemoryId("nope".to_string()), vec![])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn source_filter_honoured() {
        let store = InMemoryStore::new();
        store
            .remember(
                Memory::new(
                    "When working with bedrock provider switching, pass aws_profile explicitly.",
                    MemoryLayer::Semantic,
                    AgentId::new("a"),
                )
                .with_source(MemorySource::Lesson),
            )
            .await
            .unwrap();
        store
            .remember(
                Memory::new(
                    "user asked about bedrock today",
                    MemoryLayer::Episodic,
                    AgentId::new("a"),
                )
                .with_source(MemorySource::Passive),
            )
            .await
            .unwrap();

        let only_lessons = store
            .recall(
                "bedrock",
                MemoryFilter {
                    source: Some(MemorySource::Lesson),
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(only_lessons.len(), 1);
        assert_eq!(only_lessons[0].memory.source, MemorySource::Lesson);

        let only_passive = store
            .recall(
                "bedrock",
                MemoryFilter {
                    source: Some(MemorySource::Passive),
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(only_passive.len(), 1);
        assert_eq!(only_passive[0].memory.source, MemorySource::Passive);

        let both = store
            .recall("bedrock", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert_eq!(both.len(), 2);
    }

    #[tokio::test]
    async fn adjust_importance_round_trips_and_clamps() {
        let store = InMemoryStore::new();
        let mem = Memory::new("x", MemoryLayer::Semantic, AgentId::new("a")).with_importance(0.5);
        let id = store.remember(mem).await.unwrap();

        let after = store.adjust_importance(&id, 0.2).await.unwrap();
        assert!((after - 0.7).abs() < 1e-6);
        assert!((store.get(&id).await.unwrap().unwrap().importance - 0.7).abs() < 1e-6);

        let high = store.adjust_importance(&id, 5.0).await.unwrap();
        assert!((high - 1.0).abs() < 1e-6);

        let low = store.adjust_importance(&id, -10.0).await.unwrap();
        assert!((low - 0.05).abs() < 1e-6);
    }

    #[tokio::test]
    async fn adjust_importance_errors_when_missing() {
        let store = InMemoryStore::new();
        let result = store
            .adjust_importance(&MemoryId("nope".to_string()), 0.1)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn adjust_importance_is_atomic_under_concurrent_updates() {
        let store = std::sync::Arc::new(InMemoryStore::new());
        let mem = Memory::new("x", MemoryLayer::Semantic, AgentId::new("a")).with_importance(0.5);
        let id = store.remember(mem).await.unwrap();

        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let store = std::sync::Arc::clone(&store);
                let id = id.clone();
                tokio::spawn(async move { store.adjust_importance(&id, 0.01).await.unwrap() })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }

        let final_importance = store.get(&id).await.unwrap().unwrap().importance;
        assert!(
            (final_importance - 0.70).abs() < 1e-5,
            "expected 0.70 after 20 atomic +0.01 increments, got {final_importance}"
        );
    }

    #[tokio::test]
    async fn count_with_filter() {
        let store = InMemoryStore::new();

        for i in 0..5 {
            let mem = Memory::new(
                format!("Memory {}", i),
                if i < 3 {
                    MemoryLayer::Semantic
                } else {
                    MemoryLayer::Episodic
                },
                AgentId::new("dev"),
            );
            store.remember(mem).await.unwrap();
        }

        let count = store
            .count(MemoryFilter {
                layer: Some(MemoryLayer::Semantic),
                ..MemoryFilter::system_unscoped()
            })
            .await
            .unwrap();
        assert_eq!(count, 3);
    }
}

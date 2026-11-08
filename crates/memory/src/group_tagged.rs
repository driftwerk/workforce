//! A boss's memory, kept to the scope it learned it in (boss-gaps W8).
//!
//! Desk's sidebar groups are client boundaries, and each group's boss is an
//! agent of the hub with a memory of its own. [`GroupTagged`] is the store
//! the hub hands every agent: while the agent is a boss, everything it
//! remembers is tagged with its scope (`desk-group:<id>`, or
//! `desk-group:ungrouped` for the main boss), and it recalls only its own
//! memories with that tag or none. Moved to another group, it no longer
//! recalls what it learned in the first; what it learned before tags
//! existed is tagged with its old scope first ([`MemoryStore::add_tag`]),
//! so nothing goes along. An agent that isn't a boss recalls no memory
//! tagged for a scope.
//!
//! The scope is read on every call ([`FenceSource`]), never kept from when
//! the store was made: a boss is made, moved or switched off by its config,
//! which reloads while the hub runs.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use workforce_core::types::AgentId;

use crate::store::{MemoryStore, MemoryStoreCacheStats, RecallSettings};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};

/// What every scope tag starts with.
pub const GROUP_TAG_PREFIX: &str = "desk-group:";

/// The tag of the scope of `group` (none: everything ungrouped).
pub fn group_tag(group: Option<&str>) -> String {
    format!("{GROUP_TAG_PREFIX}{}", group.unwrap_or("ungrouped"))
}

/// Whose memory an agent's is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fence {
    /// A boss: what it remembers carries this tag, and it recalls only its
    /// own memories with this tag or none.
    Boss(String),
    /// Not a boss: nothing is tagged, and no memory tagged for a scope is
    /// recalled.
    Open,
    /// Its scope can't be told just now (its config doesn't read and none
    /// read before): nothing is remembered, and no memory tagged for a scope
    /// is recalled. Fails closed.
    Unknown,
}

impl Fence {
    /// Whether only the agent's own memories are read: a group's boss's.
    /// The main boss's recall is kept apart by its settings while any group
    /// has a boss (`recall_kept_apart`), as before.
    pub fn own_only(&self) -> bool {
        matches!(self, Fence::Boss(own) if *own != group_tag(None))
    }

    /// Whether a memory with `tags` may be read under this fence.
    pub fn admits(&self, tags: &[String]) -> bool {
        let mut scoped = tags.iter().filter(|t| t.starts_with(GROUP_TAG_PREFIX));
        match self {
            Fence::Boss(own) => scoped.all(|t| t == own),
            Fence::Open | Fence::Unknown => scoped.next().is_none(),
        }
    }
}

/// Where an agent's fence is read from, at each call.
pub trait FenceSource: Send + Sync {
    fn fence(&self) -> Fence;
}

impl<F: Fn() -> Fence + Send + Sync> FenceSource for F {
    fn fence(&self) -> Fence {
        self()
    }
}

/// How many more candidates are asked for than wanted, as fenced-out ones
/// are dropped after the store answers.
fn over_pull(limit: Option<usize>) -> Option<usize> {
    limit.map(|n| n.saturating_mul(3).saturating_add(20))
}

/// `tags` with no scope tag a caller gave: only the store adds one.
fn unscoped(tags: Vec<String>) -> Vec<String> {
    tags.into_iter()
        .filter(|t| !t.starts_with(GROUP_TAG_PREFIX))
        .collect()
}

/// One agent's memory store, fenced to its scope while it is a boss.
pub struct GroupTagged {
    inner: Arc<dyn MemoryStore>,
    agent: AgentId,
    source: Arc<dyn FenceSource>,
}

impl GroupTagged {
    pub fn new(inner: Arc<dyn MemoryStore>, agent: AgentId, source: Arc<dyn FenceSource>) -> Self {
        Self {
            inner,
            agent,
            source,
        }
    }

    /// The filter as the store is asked it: a boss reads only its own
    /// memories, and more are asked for than wanted.
    fn fenced(&self, fence: &Fence, mut filter: MemoryFilter) -> (MemoryFilter, Option<usize>) {
        if fence.own_only() {
            filter.owner = Some(self.agent.clone());
        }
        let wanted = filter.limit;
        filter.limit = over_pull(wanted);
        (filter, wanted)
    }

    /// The memory `id`, when this agent may read it now.
    async fn readable(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let fence = self.source.fence();
        Ok(self
            .inner
            .get(id)
            .await?
            .filter(|m| fence.admits(&m.tags) && (!fence.own_only() || m.owner == self.agent)))
    }

    /// `Err` unless this agent may read `id` now: a memory fenced off is as
    /// one that isn't there.
    async fn must_read(&self, id: &MemoryId) -> Result<Memory> {
        self.readable(id)
            .await?
            .ok_or_else(|| eyre!("memory {} not found", id.0))
    }
}

#[async_trait]
impl MemoryStore for GroupTagged {
    fn cache_stats(&self) -> MemoryStoreCacheStats {
        self.inner.cache_stats()
    }

    async fn remember(&self, mut memory: Memory) -> Result<MemoryId> {
        memory.tags = unscoped(std::mem::take(&mut memory.tags));
        match self.source.fence() {
            Fence::Boss(tag) => memory.tags.push(tag),
            Fence::Open => {}
            Fence::Unknown => {
                return Err(eyre!(
                    "Desk can't tell which group this agent's memory belongs to just now, \
                     so nothing was remembered"
                ))
            }
        }
        self.inner.remember(memory).await
    }

    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        let fence = self.source.fence();
        let (filter, wanted) = self.fenced(&fence, filter);
        let mut hits = self.inner.recall(query, filter).await?;
        hits.retain(|h| fence.admits(&h.memory.tags));
        if let Some(n) = wanted {
            hits.truncate(n);
        }
        Ok(hits)
    }

    fn query_model(&self) -> Option<String> {
        self.inner.query_model()
    }

    fn recall_settings(&self) -> Option<RecallSettings> {
        self.inner.recall_settings()
    }

    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        let fence = self.source.fence();
        let (filter, wanted) = self.fenced(&fence, filter);
        let mut hits = self.inner.recall_with_vector(query, vector, filter).await?;
        hits.retain(|h| fence.admits(&h.memory.tags));
        if let Some(n) = wanted {
            hits.truncate(n);
        }
        Ok(hits)
    }

    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.readable(id).await
    }

    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.must_read(id).await?;
        self.inner.forget(id).await
    }

    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.must_read(id).await?;
        self.inner.share(id, visibility).await
    }

    /// Of what it may read: a fenced-off memory is never counted.
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        let fence = self.source.fence();
        let (mut filter, _) = self.fenced(&fence, filter);
        filter.limit = None;
        Ok(self
            .inner
            .list(filter)
            .await?
            .iter()
            .filter(|m| fence.admits(&m.tags))
            .count())
    }

    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        let fence = self.source.fence();
        let (filter, wanted) = self.fenced(&fence, filter);
        let mut listed = self.inner.list(filter).await?;
        listed.retain(|m| fence.admits(&m.tags));
        if let Some(n) = wanted {
            listed.truncate(n);
        }
        Ok(listed)
    }

    /// The tags given, with the scope tags it had kept: no caller adds or
    /// takes off a scope tag.
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        let held = self.must_read(id).await?;
        let mut tags = unscoped(tags);
        tags.extend(
            held.tags
                .into_iter()
                .filter(|t| t.starts_with(GROUP_TAG_PREFIX)),
        );
        self.inner.update_tags(id, tags).await
    }

    async fn add_tag(
        &self,
        owner: &AgentId,
        tag: &str,
        unless_prefix: &str,
    ) -> Result<Vec<MemoryId>> {
        self.inner.add_tag(owner, tag, unless_prefix).await
    }

    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.must_read(id).await?;
        self.inner.adjust_importance(id, delta).await
    }

    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        self.inner.identity_memories(owner, identity).await
    }

    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
        self.inner.identity_counts(owner).await
    }

    async fn identities(&self, owner: &AgentId) -> Result<Vec<String>> {
        self.inner.identities(owner).await
    }

    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        self.inner.reassign_identity(owner, from, to).await
    }

    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        self.inner.purge_identity(owner, identity).await
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.inner.purge_memories(ids).await
    }

    /// Of what it may read only: a fenced-off memory is never the one a
    /// new one is folded into.
    async fn find_near_duplicate(
        &self,
        content: &str,
        mut filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        filter.limit = Some(1);
        let hits = self.recall(content, filter).await?;
        Ok(hits
            .into_iter()
            .next()
            .filter(|r| r.score >= min_similarity))
    }

    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        self.must_read(id).await?;
        self.inner
            .record_recurrence(id, additional_message_ids)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_memory::InMemoryStore;
    use crate::types::MemoryLayer;
    use parking_lot::Mutex;

    struct Switchable(Mutex<Fence>);

    impl FenceSource for Switchable {
        fn fence(&self) -> Fence {
            self.0.lock().clone()
        }
    }

    fn mem(content: &str, owner: &str, tags: &[&str]) -> Memory {
        let mut m = Memory::new(content, MemoryLayer::Episodic, AgentId::new(owner));
        m.identity_id = Some("sam".into());
        m.tags = tags.iter().map(|t| t.to_string()).collect();
        m
    }

    fn filter() -> MemoryFilter {
        MemoryFilter {
            identity_id: Some("sam".into()),
            limit: Some(10),
            ..MemoryFilter::default()
        }
    }

    fn contents(hits: &[MemorySearchResult]) -> Vec<String> {
        let mut c: Vec<String> = hits.iter().map(|h| h.memory.content.clone()).collect();
        c.sort();
        c
    }

    #[tokio::test]
    async fn a_boss_recalls_only_its_scope_and_a_moved_boss_none_of_the_old_one() {
        let inner: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let source = Arc::new(Switchable(Mutex::new(Fence::Boss(group_tag(Some("acme"))))));
        let sarah = GroupTagged::new(Arc::clone(&inner), AgentId::new("sarah"), source.clone());
        // From before tags existed: untagged.
        inner
            .remember(mem("acme canary from before", "sarah", &[]))
            .await
            .unwrap();
        // A boss can't plant another scope's tag.
        let planted = sarah
            .remember(mem("acme canary planted", "sarah", &["desk-group:atlas"]))
            .await
            .unwrap();
        assert_eq!(
            inner.get(&planted).await.unwrap().unwrap().tags,
            ["desk-group:acme"]
        );
        // Another agent's shared memory doesn't reach a boss.
        inner
            .remember(mem("acme canary of another agent", "scout", &[]))
            .await
            .unwrap();
        assert_eq!(
            contents(&sarah.recall("canary", filter()).await.unwrap()),
            ["acme canary from before", "acme canary planted"]
        );

        // The move: the old scope's untagged memories are tagged first,
        // then the boss oversees Atlas.
        inner
            .add_tag(
                &AgentId::new("sarah"),
                &group_tag(Some("acme")),
                GROUP_TAG_PREFIX,
            )
            .await
            .unwrap();
        *source.0.lock() = Fence::Boss(group_tag(Some("atlas")));
        assert!(sarah.recall("canary", filter()).await.unwrap().is_empty());
        assert!(sarah.list(filter()).await.unwrap().is_empty());
        assert_eq!(sarah.count(filter()).await.unwrap(), 0);
        assert!(sarah.get(&planted).await.unwrap().is_none());
        assert!(sarah.forget(&planted).await.is_err());
        assert!(sarah.adjust_importance(&planted, 0.1).await.is_err());
        assert!(
            sarah.update_tags(&planted, vec![]).await.is_err(),
            "a boss can't untag what it can't read"
        );
        assert!(sarah
            .find_near_duplicate("acme canary planted", filter(), 0.0)
            .await
            .unwrap()
            .is_none());

        let atlas = sarah
            .remember(mem("atlas note", "sarah", &[]))
            .await
            .unwrap();
        assert_eq!(
            contents(&sarah.recall("note", filter()).await.unwrap()),
            ["atlas note"]
        );
        // Tags it changes keep their scope tag.
        sarah
            .update_tags(&atlas, vec!["topic:x".into(), "desk-group:acme".into()])
            .await
            .unwrap();
        assert_eq!(
            inner.get(&atlas).await.unwrap().unwrap().tags,
            ["topic:x", "desk-group:atlas"]
        );

        // No longer a boss: nothing tagged for a scope is read.
        *source.0.lock() = Fence::Open;
        assert!(sarah.recall("note", filter()).await.unwrap().is_empty());
        let mut free = filter();
        free.owner = None;
        assert_eq!(
            contents(&sarah.recall("canary", free).await.unwrap()),
            ["acme canary of another agent"]
        );
    }

    #[tokio::test]
    async fn an_unknown_fence_remembers_nothing_and_counts_only_what_it_may_read() {
        let inner: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        inner
            .remember(mem("acme tagged", "sarah", &["desk-group:acme"]))
            .await
            .unwrap();
        inner.remember(mem("plain", "sarah", &[])).await.unwrap();
        let source = Arc::new(Switchable(Mutex::new(Fence::Unknown)));
        let sarah = GroupTagged::new(Arc::clone(&inner), AgentId::new("sarah"), source.clone());
        assert!(sarah.remember(mem("new", "sarah", &[])).await.is_err());
        assert_eq!(sarah.count(filter()).await.unwrap(), 1);
        *source.0.lock() = Fence::Open;
        assert_eq!(
            sarah.count(filter()).await.unwrap(),
            1,
            "a count under Open skips scoped ones"
        );
    }

    #[test]
    fn a_fence_admits_its_own_scope_and_untagged_memories() {
        let acme = Fence::Boss(group_tag(Some("acme")));
        let tags = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(acme.admits(&tags(&[])));
        assert!(acme.admits(&tags(&["topic:x", "desk-group:acme"])));
        assert!(!acme.admits(&tags(&["desk-group:atlas"])));
        assert!(!acme.admits(&tags(&["desk-group:acme", "desk-group:atlas"])));
        assert!(!acme.admits(&tags(&["desk-group:ungrouped"])));
        assert!(Fence::Open.admits(&tags(&["topic:x"])));
        assert!(!Fence::Open.admits(&tags(&["desk-group:ungrouped"])));
        assert_eq!(group_tag(None), "desk-group:ungrouped");
    }
}

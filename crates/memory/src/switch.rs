//! Moving memories from one collection to another while replicas serve.
//!
//! A process opens the new collection beside the one memories are in. Until
//! the new one holds every memory, it serves from the previous one while a
//! mirror sync fills the new one; the sync marks the switch, and the running
//! process turns to the new collection. Replicas still on a release that
//! writes to the previous collection are caught up with for a while; after
//! that, memories are carried back into the previous collection for as long
//! as it is kept, so that switching back loses nothing. While it is kept,
//! forgetting and erasure reach it too.
//!
//! One process copies at a time ([`Copier`]): a catch-up and a carry-back
//! must never run together.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use parking_lot::RwLock;

use crate::admin::{self, Switchable, SyncMode, Turn};
use crate::store::MemoryStore;
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};

/// The right to run a sync, held for the whole of it.
#[async_trait]
pub trait Copier: Send + Sync {
    /// `None` while another process copies.
    async fn take(&self) -> Result<Option<Box<dyn Turn>>>;
}

/// How often a switch looks, copies and catches up.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Before the first look at the switch marker.
    pub first_look_after: Duration,
    /// A failed mirror sync is retried after this long.
    pub sync_retry: Duration,
    /// How often a process that is not copying looks for the switch marker.
    pub switch_poll: Duration,
    /// After the switch, replicas still on the old release keep writing to
    /// the previous collection; new memories and forgetting are carried over
    /// this often, and, once that is over, carried back this often too...
    pub catch_up_every: Duration,
    /// ...the former for this long after each switch or start.
    pub catch_up_for: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_look_after: Duration::ZERO,
            sync_retry: Duration::from_secs(600),
            switch_poll: Duration::from_secs(30),
            catch_up_every: Duration::from_secs(120),
            catch_up_for: Duration::from_secs(30 * 60),
        }
    }
}

/// A move of memories from `previous` into `current`.
pub struct Switch {
    pub current: Arc<dyn Switchable>,
    pub previous: Arc<dyn Switchable>,
    /// Keep `previous` beside `current` once switched: forgetting and
    /// erasure reach it, and memories are caught up from and carried back
    /// into it. Off, nothing reads or writes it again.
    pub keep_previous: bool,
    pub copier: Arc<dyn Copier>,
    pub timing: Timing,
}

impl Switch {
    /// The store to serve: `current` once memories have switched to it,
    /// otherwise one serving from `previous` that turns to `current` when a
    /// mirror sync has filled it.
    pub async fn open(self) -> Result<Arc<dyn MemoryStore>> {
        if self.current.switched().await? {
            let current: Arc<dyn MemoryStore> = self.current.clone();
            self.keep();
            return Ok(current);
        }
        let store = Arc::new(Switching(RwLock::new(
            Arc::clone(&self.previous) as Arc<dyn MemoryStore>
        )));
        tokio::spawn(self.switch_into(Arc::clone(&store)));
        Ok(store)
    }

    /// Mirror `previous` into `current` once this process may copy, mark the
    /// switch and turn `store` to `current`; a process that may not copy
    /// turns when it sees the marker.
    async fn switch_into(self, store: Arc<Switching>) {
        let collection = self.current.collection().to_string();
        tokio::time::sleep(self.timing.first_look_after).await;
        let mut last_attempt: Option<tokio::time::Instant> = None;
        loop {
            match self.current.switched().await {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(target: "memory_switch", error = %err, "reading the memory switch marker failed")
                }
            }
            let due = last_attempt.is_none_or(|at| at.elapsed() >= self.timing.sync_retry);
            if due {
                if let Some(turn) = self.take().await {
                    last_attempt = Some(tokio::time::Instant::now());
                    tracing::info!(target: "memory_switch", %collection, "memory sync starting");
                    let synced = admin::sync(
                        self.current.as_ref(),
                        self.previous.as_ref(),
                        SyncMode::Mirror,
                    )
                    .await;
                    let switched = match synced {
                        Ok(_) => match self.current.mark_switched().await {
                            Ok(()) => true,
                            Err(err) => {
                                tracing::warn!(target: "memory_switch", error = %err, "recording the memory switch failed");
                                false
                            }
                        },
                        Err(err) => {
                            tracing::warn!(target: "memory_switch", error = %err, %collection, "memory sync failed; still on the previous collection");
                            false
                        }
                    };
                    release(turn).await;
                    if switched {
                        break;
                    }
                }
            }
            tokio::time::sleep(self.timing.switch_poll).await;
        }
        *store.0.write() = Arc::clone(&self.current) as Arc<dyn MemoryStore>;
        tracing::info!(target: "memory_switch", %collection, "memories switched");
        self.keep();
    }

    /// Keep `previous` beside `current`, unless `keep_previous` is off.
    fn keep(self) {
        if !self.keep_previous {
            tracing::info!(target: "memory_switch", collection = self.previous.collection(), "the collection memories switched from is left alone (keep_previous is off)");
            return;
        }
        let kept: Arc<dyn MemoryStore> = self.previous.clone();
        self.current.keep_previous(kept);
        tokio::spawn(self.catch_up());
    }

    /// Carry new memories and forgetting from `previous` into `current`
    /// while replicas may still write to `previous`, then from `current`
    /// back into `previous` for as long as `previous` is kept.
    async fn catch_up(self) {
        let since = chrono::Utc::now();
        let forward_until = tokio::time::Instant::now() + self.timing.catch_up_for;
        loop {
            tokio::time::sleep(self.timing.catch_up_every).await;
            let Some(turn) = self.take().await else {
                continue;
            };
            let forward = tokio::time::Instant::now() < forward_until;
            if forward {
                if let Err(err) = admin::sync(
                    self.current.as_ref(),
                    self.previous.as_ref(),
                    SyncMode::CatchUp,
                )
                .await
                {
                    tracing::warn!(target: "memory_switch", error = %err, "memory catch-up failed");
                }
                release(turn).await;
                continue;
            }
            if let Err(err) = self.previous.doctor().await {
                tracing::info!(target: "memory_switch", collection = self.previous.collection(), error = %err, "the previous collection is gone; no longer carrying memories back");
                release(turn).await;
                return;
            }
            let back = SyncMode::Back { since };
            if let Err(err) = admin::sync(self.previous.as_ref(), self.current.as_ref(), back).await
            {
                tracing::warn!(target: "memory_switch", error = %err, "carrying memories back to the previous collection failed");
            }
            release(turn).await;
        }
    }

    /// A turn of the copier's, or `None` when another process copies or the
    /// copier could not be asked.
    async fn take(&self) -> Option<Box<dyn Turn>> {
        self.copier.take().await.unwrap_or_else(|err| {
            tracing::warn!(target: "memory_switch", error = %err, "asking whether to copy failed");
            None
        })
    }
}

async fn release(turn: Box<dyn Turn>) {
    if let Err(err) = turn.done().await {
        tracing::warn!(target: "memory_switch", error = %err, "releasing the copy turn failed");
    }
}

/// The store in use while a sync runs: the previous collection, replaced by
/// the new one once it holds every memory.
pub struct Switching(pub RwLock<Arc<dyn MemoryStore>>);

impl Switching {
    fn current(&self) -> Arc<dyn MemoryStore> {
        Arc::clone(&self.0.read())
    }
}

#[async_trait]
impl MemoryStore for Switching {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        self.current().remember(memory).await
    }
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.current().recall(query, filter).await
    }
    /// The store in use's: until the switch, the collection memories are
    /// moved from, which may take no vector or another model's.
    fn query_model(&self) -> Option<String> {
        self.current().query_model()
    }
    fn recall_settings(&self) -> Option<crate::store::RecallSettings> {
        self.current().recall_settings()
    }
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        self.current()
            .recall_with_vector(query, vector, filter)
            .await
    }
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.current().get(id).await
    }
    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.current().forget(id).await
    }
    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.current().share(id, visibility).await
    }
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        self.current().count(filter).await
    }
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        self.current().list(filter).await
    }
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        self.current().update_tags(id, tags).await
    }
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.current().adjust_importance(id, delta).await
    }
    async fn identity_memories(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<Vec<MemoryId>> {
        self.current().identity_memories(owner, identity).await
    }
    async fn identity_counts(
        &self,
        owner: &workforce_core::types::AgentId,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        self.current().identity_counts(owner).await
    }
    async fn reassign_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        from: &str,
        to: &str,
    ) -> Result<usize> {
        self.current().reassign_identity(owner, from, to).await
    }
    async fn purge_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<usize> {
        self.current().purge_identity(owner, identity).await
    }
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.current().purge_memories(ids).await
    }
    async fn find_near_duplicate(
        &self,
        content: &str,
        filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        self.current()
            .find_near_duplicate(content, filter, min_similarity)
            .await
    }
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        self.current()
            .record_recurrence(id, additional_message_ids)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::NoTurns;
    use crate::types::MemoryLayer;
    use crate::vellev::node_tests::{Node, Stores};
    use workforce_core::types::AgentId;

    /// A copier that always or never gives a turn.
    struct Gives(bool);

    #[async_trait]
    impl Copier for Gives {
        async fn take(&self) -> Result<Option<Box<dyn Turn>>> {
            Ok(self.0.then(|| Box::new(NoTurns) as Box<dyn Turn>))
        }
    }

    fn quick() -> Timing {
        Timing {
            first_look_after: Duration::ZERO,
            sync_retry: Duration::from_millis(20),
            switch_poll: Duration::from_millis(20),
            catch_up_every: Duration::from_secs(3600),
            catch_up_for: Duration::from_secs(3600),
        }
    }

    fn alices(content: &str) -> Memory {
        Memory::new(content, MemoryLayer::Episodic, AgentId::new("agent")).with_identity_id("alice")
    }

    fn for_alice() -> MemoryFilter {
        MemoryFilter {
            identity_id: Some("alice".into()),
            limit: Some(10),
            ..Default::default()
        }
    }

    async fn opened(node: &Node, copier: bool) -> (Arc<dyn MemoryStore>, Arc<dyn Switchable>) {
        let previous = Arc::new(node.store("Previous").await);
        previous
            .remember(alices("otters hold hands"))
            .await
            .unwrap();
        let current: Arc<dyn Switchable> = Arc::new(node.store("Current").await);
        let store = Switch {
            current: Arc::clone(&current),
            previous,
            keep_previous: true,
            copier: Arc::new(Gives(copier)),
            timing: quick(),
        }
        .open()
        .await
        .unwrap();
        (store, current)
    }

    /// Without a turn nothing is copied: the store serves from the previous
    /// collection, and the new one is never marked.
    #[tokio::test]
    async fn without_a_turn_nothing_is_copied_and_the_previous_collection_serves() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let (store, current) = opened(&node, false).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!current.switched().await.unwrap());
        assert_eq!(current.count(for_alice()).await.unwrap(), 0);
        assert_eq!(store.count(for_alice()).await.unwrap(), 1);
    }

    /// With a turn the previous collection is mirrored, the switch marked,
    /// and the store turns to the new collection: what it remembers next is
    /// there.
    #[tokio::test]
    async fn with_a_turn_memories_are_mirrored_and_the_store_switches() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let (store, current) = opened(&node, true).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !current.switched().await.unwrap() {
            assert!(tokio::time::Instant::now() < deadline, "never switched");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(current.count(for_alice()).await.unwrap(), 1);
        store
            .remember(alices("seals sleep floating"))
            .await
            .unwrap();
        assert_eq!(current.count(for_alice()).await.unwrap(), 2);
    }
}

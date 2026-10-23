use chrono::{DateTime, Utc};
use eyre::Result;
use uuid::Uuid;

use crate::model::{
    DeepNode, ItemQuery, ItemStatus, KnowledgeItem, NodeQuery, NodeStatus, ReviewMark,
};

/// Source-of-truth for self-learned knowledge-item lifecycle, provenance
/// and freshness. The retrieval surface (Memory) lives in a separate
/// store; items reference Memory rows by string UUID
/// ([`KnowledgeItem::memory_id`]), nullable so a forgotten Memory leaves
/// an auditable row behind.
///
/// Sync to match the rest of the workforce store traits.
pub trait SelfLearningStore: Send + Sync {
    /// Insert a new item or update an existing one by primary key.
    /// Idempotent: re-upserting the same item is a no-op-equivalent.
    fn upsert_item(&self, item: &KnowledgeItem) -> Result<()>;
    fn get_item(&self, id: Uuid) -> Result<Option<KnowledgeItem>>;
    /// Dedup lookup: the curation turn checks this before inserting so an
    /// identical claim bumps `verified_at` rather than appending.
    fn find_by_hash(&self, agent_id: &str, content_hash: &str) -> Result<Option<KnowledgeItem>>;
    fn list_items(&self, agent_id: &str, filter: &ItemQuery) -> Result<Vec<KnowledgeItem>>;
    fn items_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<KnowledgeItem>>;
    /// Apply a status transition, recording review metadata
    /// (`reviewed_at`/`reviewed_by`/`review_note`) when a [`ReviewMark`]
    /// is supplied. Returns `false` if no row matched.
    fn set_status(&self, id: Uuid, status: ItemStatus, review: Option<&ReviewMark>)
        -> Result<bool>;
    /// Set or clear (`None`) the backing Memory link — clearing it is the
    /// crash-safe first step of forgetting a Memory.
    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()>;
    /// Every non-null `memory_id` referenced across every agent. Used by
    /// the startup orphan sweep to build the "still-referenced" set.
    fn list_all_memory_ids(&self) -> Result<Vec<String>>;
    fn delete_item(&self, id: Uuid) -> Result<bool>;
    /// Items whose `expires_at` is at or before `now` and whose status is
    /// still surfaceable (`Approved`/`Provisional`) — the freshness sweep
    /// re-verification worklist.
    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<KnowledgeItem>>;
}

/// Source-of-truth for an agent's deep-expertise knowledge tree — the
/// hierarchical counterpart to [`SelfLearningStore`]. Nodes form a tree per
/// plan (`parent_id` is `None` at a root) and reference Memory rows for
/// retrieval by string UUID ([`DeepNode::memory_id`]).
pub trait DeepExpertiseStore: Send + Sync {
    /// Insert a new node or update an existing one by primary key (`id`).
    fn upsert_node(&self, node: &DeepNode) -> Result<()>;
    fn get_node(&self, id: Uuid) -> Result<Option<DeepNode>>;
    /// Dedup lookup by tree position ([`crate::model::node_path_hash`]): the
    /// deepening turn checks this before inserting so re-curating a sub-domain
    /// updates the node in place rather than appending a duplicate.
    fn find_by_path(&self, agent_id: &str, path_hash: &str) -> Result<Option<DeepNode>>;
    /// Nodes for an agent, filtered by plan, tree position (roots / children of
    /// a node / any) and status — see [`NodeQuery`].
    fn list_nodes(&self, agent_id: &str, filter: &NodeQuery) -> Result<Vec<DeepNode>>;
    fn nodes_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<DeepNode>>;
    fn set_status(&self, id: Uuid, status: NodeStatus, review: Option<&ReviewMark>)
        -> Result<bool>;
    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()>;
    fn list_all_memory_ids(&self) -> Result<Vec<String>>;
    /// Delete a node and every descendant beneath it (the whole subtree).
    fn delete_subtree(&self, id: Uuid) -> Result<u64>;
    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<DeepNode>>;
}

//! Self-learning knowledge persistence — agent-curated, primary-cited
//! knowledge items with a governance lifecycle.
//!
//! One table sitting alongside `Memory`: `self_learning_items`. Memory
//! remains the semantic recall surface (Weaviate-indexed); this table
//! holds the lifecycle/provenance/freshness metadata that tags can't
//! carry. Items reference their Memory row by string UUID (no SQL FK —
//! the memory store may live in a different backend), and the link is
//! nullable so a forgotten Memory leaves an auditable row behind.
//!
//! Same SQLite-default + Postgres-feature layering as `workforce-training`.

pub mod model;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod sqlite;
pub mod store;

pub use model::{
    content_hash, node_path_hash, DeepNode, ItemQuery, ItemSource, ItemStatus, KnowledgeItem,
    NodeParent, NodeQuery, NodeStatus, ReviewMark, SourceKind, SurfaceItem,
};
pub use store::{DeepExpertiseStore, SelfLearningStore};

//! Per-agent training corpora persistence — DW-539 Phase 1a.
//!
//! Three tables sitting alongside `Memory`: `training_corpora`,
//! `training_documents`, `training_chunks`. Memory remains the
//! retrieval surface (Weaviate-indexed); these tables hold the
//! audit/citation metadata that tags can't carry. Chunks reference
//! Memory rows by string UUID (no SQL FK — the memory store may
//! live in a different backend).
//!
//! Same SQLite-default + Postgres-feature layering as `workforce-mcp-store`.

pub mod model;
pub mod names;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod sqlite;
pub mod store;

pub use model::{
    Chunk, ChunkIdentity, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind,
};
pub use names::{sanitize_filename, validate_slug, SlugError, MAX_SLUG_LEN};
pub use store::TrainingStore;

//! Hub-side orchestration for per-agent training corpora — DW-539 Phase 1a.
//!
//! Loaders parse raw bytes into `RawChunk`s; the pipeline diffs against the
//! corpus's existing chunk set (by content hash), writes new memories +
//! chunk rows, and deletes the ones that disappeared. The recall helper
//! renders the `## Agent Training` section the prompt assembler injects.

pub mod backend;
pub use crate::hub_core::session_citations as citations;
pub mod cleanup;
pub mod loaders;
pub mod pipeline;
pub mod recall;

pub use backend::{open_training_store, select_backend};
pub use citations::{make_snippet, Citation, SessionCitationStore, TurnCitations};
pub use cleanup::{spawn_sweep, sweep_orphans, CleanupReport, SweepOptions};
pub use loaders::{detect_format, supported_extensions, Format, RawChunk};
pub use pipeline::{
    delete_corpus_cascade, delete_document_cascade, doc_short_id, load_corpus,
    training_recall_filter, DeleteOutcome, LoadOutcome, LoadRequest, VisionContext,
};
pub use recall::format_training_section;

/// Tag attached to every Memory row that backs a training chunk. The
/// recall path uses this tag to (a) build the `## Agent Training` block
/// and (b) exclude these chunks from the generic `## Recalled Memories`.
pub const TRAINING_TAG: &str = "training";

/// Tag prefix carrying the document UUID's first 8 hex chars. The
/// recall helper parses it to resolve a Memory back to its source
/// document for the citation marker.
pub const DOC_TAG_PREFIX: &str = "doc:";

/// Tag prefix carrying the corpus slug. Lets future filters target a
/// single corpus without joining through `training_chunks`.
pub const CORPUS_TAG_PREFIX: &str = "corpus:";

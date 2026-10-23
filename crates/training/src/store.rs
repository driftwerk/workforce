use eyre::Result;
use uuid::Uuid;

use crate::model::{Chunk, Corpus, Document, ProcessingStatus};

/// Per-agent training corpora persistence.
///
/// All three resource types (corpora, documents, chunks) share one
/// backend. The retrieval surface (Memory) lives in a separate store;
/// chunks reference Memory rows by string UUID (`Chunk::memory_id`).
///
/// Sync to match the rest of the workforce store traits.
pub trait TrainingStore: Send + Sync {
    fn create_corpus(&self, corpus: &Corpus) -> Result<()>;
    fn list_corpora(&self, agent_id: &str) -> Result<Vec<Corpus>>;
    /// List every corpus across every agent. Used by the startup
    /// orphan sweep to walk all chunks without a per-agent query.
    fn list_all_corpora(&self) -> Result<Vec<Corpus>>;
    fn get_corpus_by_name(&self, agent_id: &str, name: &str) -> Result<Option<Corpus>>;
    fn get_corpus(&self, corpus_id: Uuid) -> Result<Option<Corpus>>;
    fn delete_corpus(&self, corpus_id: Uuid) -> Result<bool>;
    /// Give a corpus to another scope (a person's corpora when they are
    /// merged into another identity). `false` when there is no such corpus.
    fn set_corpus_scope(&self, corpus_id: Uuid, scope: &crate::model::CorpusScope) -> Result<bool>;

    fn create_document(&self, document: &Document) -> Result<()>;
    fn list_documents(&self, corpus_id: Uuid) -> Result<Vec<Document>>;
    fn get_document(&self, document_id: Uuid) -> Result<Option<Document>>;
    fn find_document_by_filename(
        &self,
        corpus_id: Uuid,
        filename: &str,
    ) -> Result<Option<Document>>;
    fn update_document_status(
        &self,
        document_id: Uuid,
        status: ProcessingStatus,
        error: Option<&str>,
    ) -> Result<()>;
    fn delete_document(&self, document_id: Uuid) -> Result<bool>;

    fn create_chunks(&self, chunks: &[Chunk]) -> Result<()>;
    fn list_chunks_for_document(&self, document_id: Uuid) -> Result<Vec<Chunk>>;
    fn list_chunks_for_corpus(&self, corpus_id: Uuid) -> Result<Vec<Chunk>>;
    /// Return every `memory_id` referenced by `training_chunks`, across
    /// every corpus and document. Used by the startup orphan sweep
    /// to build the "still-referenced" set without loading full chunks.
    fn list_all_chunk_memory_ids(&self) -> Result<Vec<String>>;
    fn delete_chunk(&self, chunk_id: Uuid) -> Result<bool>;
    fn delete_chunks_for_document(&self, document_id: Uuid) -> Result<()>;
    fn find_chunk_in_corpus_by_hash(
        &self,
        corpus_id: Uuid,
        content_hash: &str,
    ) -> Result<Option<Chunk>>;
}

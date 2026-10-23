//! Load orchestration: parse → diff against existing chunks → write/delete
//! memories + chunk rows. Idempotent: re-loading the same file is a no-op.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use eyre::{eyre, Result};
use tracing::info;
use uuid::Uuid;

use workforce_agent::vision::VisionLlm;
use workforce_common::settings::LlmVisionConfig;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{
    Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource, Visibility,
};
use workforce_training::{
    Chunk, ChunkIdentity, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind,
    TrainingStore,
};

use super::loaders::{self, Format, RawChunk};
use super::{CORPUS_TAG_PREFIX, DOC_TAG_PREFIX, TRAINING_TAG};

/// Vision LLM + config bundle passed into the load pipeline so the
/// PDF loader can fall back to OCR when text extraction comes up
/// empty. `None` means "no vision fallback available" — image-only
/// PDFs error out cleanly.
pub struct VisionContext<'a> {
    pub llm: &'a dyn VisionLlm,
    pub config: &'a LlmVisionConfig,
}

const TRAINING_IMPORTANCE: f32 = 0.9;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadOutcome {
    pub corpus_id: Option<Uuid>,
    pub document_id: Option<Uuid>,
    pub written: usize,
    pub unchanged: usize,
    pub deleted: usize,
}

pub struct LoadRequest<'a> {
    pub agent_id: &'a str,
    pub corpus_name: &'a str,
    pub original_filename: &'a str,
    pub source_kind: SourceKind,
    pub source_ref: Option<&'a str>,
    pub uploaded_by: &'a str,
    pub bytes: &'a [u8],
    pub storage_uri: &'a str,
    pub format: Option<Format>,
    /// Vision LLM + config for the OCR fallback on image-only PDFs.
    /// `None` keeps the loader on its sync text-only path.
    pub vision: Option<VisionContext<'a>>,
}

pub async fn load_corpus(
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    req: LoadRequest<'_>,
) -> Result<LoadOutcome> {
    let format = req
        .format
        .or_else(|| loaders::detect_format(req.original_filename))
        .ok_or_else(|| {
            eyre!(
                "no loader for `{}` — supported extensions: {}",
                req.original_filename,
                loaders::supported_extensions(),
            )
        })?;
    let raw_chunks = match (format, req.vision.as_ref()) {
        (Format::Pdf, Some(vision)) => {
            loaders::pdf::parse_with_ocr(req.bytes, Some(vision.llm), vision.config).await?
        }
        _ => loaders::load(format, req.bytes)?,
    };
    info!(
        agent = req.agent_id,
        corpus = req.corpus_name,
        file = req.original_filename,
        chunks = raw_chunks.len(),
        "training: parsed input",
    );

    let corpus = ensure_corpus(
        training.as_ref(),
        req.agent_id,
        req.corpus_name,
        req.uploaded_by,
    )?;
    let document = ensure_document(training.as_ref(), &corpus, &req)?;
    let owner = AgentId(corpus.agent_id.clone());
    let doc_short = doc_short_id(document.id);

    let existing = training.list_chunks_for_document(document.id)?;
    let mut existing_by_hash: HashMap<String, Chunk> = HashMap::with_capacity(existing.len());
    for c in existing {
        existing_by_hash.insert(c.content_hash.clone(), c);
    }

    let mut outcome = LoadOutcome {
        corpus_id: Some(corpus.id),
        document_id: Some(document.id),
        ..Default::default()
    };
    let mut incoming_hashes: HashSet<String> = HashSet::with_capacity(raw_chunks.len());

    for (idx, raw) in raw_chunks.into_iter().enumerate() {
        let body = chunk_body(&raw);
        let hash = ChunkIdentity::hash(corpus.id, &body);
        if !incoming_hashes.insert(hash.clone()) {
            continue;
        }
        if existing_by_hash.contains_key(&hash) {
            outcome.unchanged += 1;
            continue;
        }
        let mem = build_memory(owner.clone(), &corpus, &doc_short, &raw, body);
        let mem_id = mem.id.clone();
        memory.remember(mem).await?;
        training.create_chunks(&[Chunk {
            id: Uuid::new_v4(),
            document_id: document.id,
            chunk_index: idx as i32,
            content_hash: hash,
            memory_id: mem_id.0,
            locator: raw.locator,
            char_start: 0,
            char_end: 0,
        }])?;
        outcome.written += 1;
    }

    for (hash, chunk) in &existing_by_hash {
        if !incoming_hashes.contains(hash) {
            if let Err(e) = memory.forget(&MemoryId(chunk.memory_id.clone())).await {
                tracing::warn!(
                    memory_id = %chunk.memory_id,
                    error = %e,
                    "training: memory.forget failed while pruning removed chunk",
                );
            }
            training.delete_chunk(chunk.id)?;
            outcome.deleted += 1;
        }
    }

    training.update_document_status(document.id, ProcessingStatus::Ready, None)?;
    Ok(outcome)
}

fn ensure_corpus(
    training: &dyn TrainingStore,
    agent_id: &str,
    name: &str,
    created_by: &str,
) -> Result<Corpus> {
    if let Some(existing) = training.get_corpus_by_name(agent_id, name)? {
        return Ok(existing);
    }
    let corpus = Corpus {
        id: Uuid::new_v4(),
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        scope: CorpusScope::Agent,
        created_at: Utc::now(),
        created_by: created_by.to_string(),
    };
    training.create_corpus(&corpus)?;
    Ok(corpus)
}

fn ensure_document(
    training: &dyn TrainingStore,
    corpus: &Corpus,
    req: &LoadRequest<'_>,
) -> Result<Document> {
    if let Some(existing) = training.find_document_by_filename(corpus.id, req.original_filename)? {
        training.update_document_status(existing.id, ProcessingStatus::Processing, None)?;
        return Ok(existing);
    }
    let document = Document {
        id: Uuid::new_v4(),
        corpus_id: corpus.id,
        original_filename: req.original_filename.to_string(),
        mime_type: mime_for(req.original_filename),
        size_bytes: req.bytes.len() as i64,
        storage_uri: req.storage_uri.to_string(),
        source_kind: req.source_kind,
        source_ref: req.source_ref.map(str::to_string),
        processing_status: ProcessingStatus::Processing,
        processing_error: None,
        uploaded_at: Utc::now(),
        uploaded_by: req.uploaded_by.to_string(),
        page_count: None,
    };
    training.create_document(&document)?;
    Ok(document)
}

fn build_memory(
    owner: AgentId,
    corpus: &Corpus,
    doc_short: &str,
    chunk: &RawChunk,
    body: String,
) -> Memory {
    let mut tags = vec![
        TRAINING_TAG.to_string(),
        format!("{CORPUS_TAG_PREFIX}{}", corpus.name),
        format!("{DOC_TAG_PREFIX}{doc_short}"),
    ];
    tags.extend(chunk.tags.iter().cloned());
    let mut m = Memory::new(body, MemoryLayer::Organisation, owner);
    m.visibility = Visibility::Global;
    m.importance = TRAINING_IMPORTANCE;
    m.source = MemorySource::Curated;
    m.tags = tags;
    m
}

fn chunk_body(chunk: &RawChunk) -> String {
    match &chunk.title {
        Some(title) if !title.trim().is_empty() => format!("{title}\n\n{}", chunk.body),
        _ => chunk.body.clone(),
    }
}

pub fn doc_short_id(uuid: Uuid) -> String {
    uuid.simple().to_string().chars().take(8).collect()
}

fn mime_for(filename: &str) -> String {
    match filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .as_deref()
    {
        Some("json") => "application/json",
        Some("md" | "markdown") => "text/markdown",
        Some("csv") => "text/csv",
        Some("txt" | "text") => "text/plain",
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx" | "xlsm") => {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        }
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsb") => "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub chunks_removed: usize,
    pub memories_forgotten: usize,
    pub files_unlinked: usize,
}

/// Delete a corpus, forgetting every backing Memory row and unlinking
/// every original-file artifact.
///
/// SQL `delete_corpus` cascades documents + chunks; this helper walks
/// the chunks first so the Memory rows in the vector store don't leak,
/// and unlinks each document's on-disk file so the archive doesn't
/// grow forever. Memory-forget and file-unlink failures are warned but
/// don't abort — a partial Weaviate outage or stale `storage_uri`
/// shouldn't block reclaiming the corpus name.
pub async fn delete_corpus_cascade(
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    corpus_id: Uuid,
) -> Result<DeleteOutcome> {
    let documents = training.list_documents(corpus_id)?;
    let chunks = training.list_chunks_for_corpus(corpus_id)?;
    let mut memories_forgotten = 0;
    for chunk in &chunks {
        if forget_memory(memory.as_ref(), &chunk.memory_id).await {
            memories_forgotten += 1;
        }
    }
    let mut files_unlinked = 0;
    for doc in &documents {
        if unlink_storage_uri(&doc.storage_uri).await {
            files_unlinked += 1;
        }
    }
    let removed = training.delete_corpus(corpus_id)?;
    if !removed {
        return Err(eyre!("corpus {corpus_id} not found"));
    }
    Ok(DeleteOutcome {
        chunks_removed: chunks.len(),
        memories_forgotten,
        files_unlinked,
    })
}

/// Delete a single document and cascade chunks → memories → file.
///
/// Same partial-failure tolerance as [`delete_corpus_cascade`].
pub async fn delete_document_cascade(
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    document_id: Uuid,
) -> Result<DeleteOutcome> {
    let document = training
        .get_document(document_id)?
        .ok_or_else(|| eyre!("document {document_id} not found"))?;
    let chunks = training.list_chunks_for_document(document_id)?;
    let mut memories_forgotten = 0;
    for chunk in &chunks {
        if forget_memory(memory.as_ref(), &chunk.memory_id).await {
            memories_forgotten += 1;
        }
    }
    let files_unlinked = usize::from(unlink_storage_uri(&document.storage_uri).await);
    let removed = training.delete_document(document_id)?;
    if !removed {
        return Err(eyre!("document {document_id} not found"));
    }
    Ok(DeleteOutcome {
        chunks_removed: chunks.len(),
        memories_forgotten,
        files_unlinked,
    })
}

async fn forget_memory(memory: &dyn MemoryStore, memory_id: &str) -> bool {
    match memory.forget(&MemoryId(memory_id.to_string())).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                memory_id = %memory_id,
                error = %e,
                "training: memory.forget failed during cascade delete",
            );
            false
        }
    }
}

/// Remove the original file from disk for a `file://` storage URI.
///
/// Returns `true` only if a file was actually unlinked. Missing files,
/// non-`file://` schemes, and permission errors are warned but reported
/// as `false` rather than propagated — losing the on-disk archive
/// shouldn't block deleting the SQL/Memory state.
pub(crate) async fn unlink_storage_uri(storage_uri: &str) -> bool {
    let Some(path) = storage_uri.strip_prefix("file://") else {
        tracing::warn!(
            storage_uri,
            "training: skip unlink (non-file:// scheme not supported)",
        );
        return false;
    };
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path, "training: original file already missing");
            false
        }
        Err(e) => {
            tracing::warn!(path, error = %e, "training: failed to unlink original");
            false
        }
    }
}

pub fn training_recall_filter(agent_id: &str, limit: usize, min_importance: f32) -> MemoryFilter {
    MemoryFilter {
        owner: Some(AgentId(agent_id.to_string())),
        tags: Some(vec![TRAINING_TAG.to_string()]),
        limit: Some(limit),
        min_importance: Some(min_importance),
        // Training corpus is agent-owned and Global-visibility with no per-user
        // identity (see `build_memory`), so recall is by design not user-scoped.
        // Without this opt-in the DW-518 isolation gate denies it as unscoped
        // and returns nothing; agent-scoping stays enforced via `owner`.
        system_unscoped: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_short_id_is_eight_hex_chars() {
        let s = doc_short_id(Uuid::from_u128(0xdeadbeef_cafe_babe_1234_567890abcdef));
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn mime_for_known_extensions() {
        assert_eq!(mime_for("x.json"), "application/json");
        assert_eq!(mime_for("X.MD"), "text/markdown");
        assert_eq!(mime_for("notes.txt"), "text/plain");
        assert_eq!(mime_for("doc.pdf"), "application/pdf");
        assert_eq!(
            mime_for("brief.docx"),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(
            mime_for("data.xlsx"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(
            mime_for("sheet.ods"),
            "application/vnd.oasis.opendocument.spreadsheet"
        );
        assert_eq!(mime_for("no-extension"), "application/octet-stream");
    }

    #[test]
    fn chunk_body_prepends_title_when_present() {
        let chunk = RawChunk {
            title: Some("Title".into()),
            body: "Body".into(),
            locator: "x".into(),
            tags: vec![],
        };
        assert_eq!(chunk_body(&chunk), "Title\n\nBody");
    }

    #[test]
    fn chunk_body_omits_blank_title() {
        let chunk = RawChunk {
            title: Some("   ".into()),
            body: "Body".into(),
            locator: "x".into(),
            tags: vec![],
        };
        assert_eq!(chunk_body(&chunk), "Body");
    }

    #[test]
    fn training_filter_includes_required_fields() {
        let f = training_recall_filter("mark", 5, 0.5);
        assert_eq!(f.owner.unwrap().0, "mark");
        assert_eq!(f.tags.unwrap(), vec!["training".to_string()]);
        assert_eq!(f.limit, Some(5));
        assert_eq!(f.min_importance, Some(0.5));
    }

    #[test]
    fn training_filter_opts_out_of_user_scoping() {
        // Training corpus has no per-user identity, so recall must opt into
        // system_unscoped or the DW-518 gate denies it as unscoped.
        let f = training_recall_filter("mark", 5, 0.5);
        assert!(f.system_unscoped);
        assert!(f.identity_id.is_none());
    }

    #[tokio::test]
    async fn unlink_storage_uri_handles_missing_and_non_file_schemes() {
        // Non-file:// schemes are skipped, not errored.
        assert!(!unlink_storage_uri("https://example.test/x").await);
        // Missing file is a warn-and-continue, returns false.
        assert!(!unlink_storage_uri("file:///definitely/not/here/xyz").await);

        // Real round-trip: write a file, unlink it, confirm gone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, b"hello").unwrap();
        let uri = format!("file://{}", path.display());
        assert!(unlink_storage_uri(&uri).await);
        assert!(!path.exists());

        // Second call on the same uri is a no-op (file already gone).
        assert!(!unlink_storage_uri(&uri).await);
    }
}

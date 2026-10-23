//! HTTP handlers for per-agent training corpora (DW-539 Phase 1a, remote-mode).
//!
//! Routes registered in [`crate::gateway::router`]:
//!
//! ```text
//! POST   /api/agents/{id}/training/corpora                    multipart: corpus, file
//! GET    /api/agents/{id}/training/corpora
//! GET    /api/agents/{id}/training/corpora/{name}
//! DELETE /api/agents/{id}/training/corpora/{name}
//! GET    /api/agents/{id}/training/documents/{doc_id}
//! GET    /api/agents/{id}/training/documents/{doc_id}/download
//! DELETE /api/agents/{id}/training/documents/{doc_id}
//! ```
//!
//! Authentication: all routes require the owner token via
//! [`crate::gateway::require_admin_action`]. Training writes are admin
//! actions — they mutate org-wide agent state and bypass private memory
//! access controls.
//!
//! Body limits: the upload route is wrapped in a `DefaultBodyLimit` at
//! registration time. Per-file enforcement uses
//! [`workforce_common::settings::TrainingConfig::max_file_size_mb`].
//!
//! Cross-agent IDOR: every document handler resolves the doc, fetches
//! its corpus, and asserts `corpus.agent_id == agent_id` before
//! returning bytes or metadata — the URL agent scope is the auth scope.

use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use uuid::Uuid;

use workforce_common::settings::TrainingConfig;
use workforce_memory::store::MemoryStore;
use workforce_training::{sanitize_filename, validate_slug, Document, SourceKind, TrainingStore};

use crate::gateway::{require_admin_action, GatewayState, WsQuery};
use crate::serve::AgentService;
use crate::training::{
    delete_corpus_cascade, delete_document_cascade, doc_short_id, load_corpus, LoadRequest,
    VisionContext,
};

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

/// Upper bound for the multipart upload route, applied via per-route
/// `DefaultBodyLimit` in `gateway::router`. Per-file enforcement still
/// reads `TrainingConfig::max_file_size_mb` so installs can tighten
/// (but not loosen above) this.
pub const TRAINING_UPLOAD_BODY_LIMIT: usize = 64 * 1024 * 1024;

// ── handlers ─────────────────────────────────────────────────────────

/// `POST /api/agents/{id}/training/corpora` — multipart upload.
///
/// Form fields:
/// - `corpus` (text, required): corpus slug
/// - `file`   (file,  required): the training document
pub async fn upload_corpus(
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    multipart: Multipart,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, memory, config) = require_training_stack(&service)?;

    let UploadFields {
        corpus_name,
        filename,
        bytes,
    } = read_upload(multipart, config.max_file_size_mb).await?;

    let storage_uri = stash_original(
        &config,
        &service,
        &agent_id,
        &corpus_name,
        &filename,
        &bytes,
    )
    .await
    .map_err(|e| {
        warn!(agent = %agent_id, error = %e, "training: failed to persist original");
        internal_err("failed to persist uploaded file")
    })?;

    let uploaded_by = caller
        .identity_id
        .clone()
        .unwrap_or_else(|| "owner-token".to_string());
    let vision = vision_context_for(&service);
    let req = LoadRequest {
        agent_id: &agent_id,
        corpus_name: &corpus_name,
        original_filename: &filename,
        source_kind: SourceKind::HttpUpload,
        source_ref: None,
        uploaded_by: &uploaded_by,
        bytes: &bytes,
        storage_uri: &storage_uri,
        format: None,
        vision,
    };

    // The uploader is who a scanned PDF's pages are described for.
    let outcome = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            caller.identity_id.clone().unwrap_or_default(),
            load_corpus(Arc::clone(&training), Arc::clone(&memory), req),
        )
        .await
        .map_err(|e| {
            warn!(agent = %agent_id, corpus = %corpus_name, error = %e, "training: load failed");
            bad_request_err(&format!("load failed: {e}"))
        })?;

    info!(
        agent = %agent_id,
        corpus = %corpus_name,
        file = %filename,
        written = outcome.written,
        unchanged = outcome.unchanged,
        deleted = outcome.deleted,
        "training: corpus loaded via HTTP",
    );

    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "corpus": corpus_name,
        "filename": filename,
        "corpus_id": outcome.corpus_id.map(|u| u.to_string()),
        "document_id": outcome.document_id.map(|u| u.to_string()),
        "document_short": outcome.document_id.map(doc_short_id),
        "written": outcome.written,
        "unchanged": outcome.unchanged,
        "deleted": outcome.deleted,
    })))
}

/// `GET /api/agents/{id}/training/corpora` — list every corpus for an agent.
pub async fn list_corpora(
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, _, _) = require_training_stack(&service)?;

    let corpora = training
        .list_corpora(&agent_id)
        .map_err(|e| internal_err(&format!("list corpora failed: {e}")))?;

    let mut out = Vec::with_capacity(corpora.len());
    for corpus in corpora {
        let docs = training
            .list_documents(corpus.id)
            .map_err(|e| internal_err(&format!("list documents failed: {e}")))?;
        let mut total_chunks: usize = 0;
        for doc in &docs {
            let chunks = training
                .list_chunks_for_document(doc.id)
                .map_err(|e| internal_err(&format!("list chunks failed: {e}")))?;
            total_chunks += chunks.len();
        }
        out.push(serde_json::json!({
            "id": corpus.id.to_string(),
            "name": corpus.name,
            "document_count": docs.len(),
            "chunk_count": total_chunks,
            "created_at": corpus.created_at.to_rfc3339(),
            "created_by": corpus.created_by,
        }));
    }
    Ok(Json(serde_json::json!({ "corpora": out })))
}

/// `GET /api/agents/{id}/training/corpora/{name}` — detail for one corpus.
pub async fn get_corpus(
    Path((agent_id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    validate_slug(&name).map_err(|e| bad_request_err(&format!("`name` path segment {e}")))?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, _, _) = require_training_stack(&service)?;

    let corpus = training
        .get_corpus_by_name(&agent_id, &name)
        .map_err(|e| internal_err(&format!("lookup failed: {e}")))?
        .ok_or_else(|| not_found_err(&format!("no corpus `{name}` for agent `{agent_id}`")))?;

    let docs = training
        .list_documents(corpus.id)
        .map_err(|e| internal_err(&format!("list documents failed: {e}")))?;

    let mut doc_json = Vec::with_capacity(docs.len());
    for doc in &docs {
        let chunks = training
            .list_chunks_for_document(doc.id)
            .map_err(|e| internal_err(&format!("list chunks failed: {e}")))?;
        doc_json.push(serde_json::json!({
            "id": doc.id.to_string(),
            "short_id": doc_short_id(doc.id),
            "filename": doc.original_filename,
            "mime_type": doc.mime_type,
            "size_bytes": doc.size_bytes,
            "source_kind": doc.source_kind.as_str(),
            "processing_status": doc.processing_status.as_str(),
            "processing_error": doc.processing_error,
            "uploaded_at": doc.uploaded_at.to_rfc3339(),
            "uploaded_by": doc.uploaded_by,
            "chunk_count": chunks.len(),
        }));
    }

    Ok(Json(serde_json::json!({
        "id": corpus.id.to_string(),
        "name": corpus.name,
        "created_at": corpus.created_at.to_rfc3339(),
        "created_by": corpus.created_by,
        "documents": doc_json,
    })))
}

/// `DELETE /api/agents/{id}/training/corpora/{name}` — cascade delete.
pub async fn delete_corpus(
    Path((agent_id, name)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    validate_slug(&name).map_err(|e| bad_request_err(&format!("`name` path segment {e}")))?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, memory, _) = require_training_stack(&service)?;

    let corpus = training
        .get_corpus_by_name(&agent_id, &name)
        .map_err(|e| internal_err(&format!("lookup failed: {e}")))?
        .ok_or_else(|| not_found_err(&format!("no corpus `{name}` for agent `{agent_id}`")))?;

    let outcome = delete_corpus_cascade(training, memory, corpus.id)
        .await
        .map_err(|e| internal_err(&format!("delete failed: {e}")))?;

    info!(
        agent = %agent_id,
        corpus = %name,
        chunks = outcome.chunks_removed,
        memories = outcome.memories_forgotten,
        files = outcome.files_unlinked,
        "training: corpus deleted via HTTP",
    );

    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "corpus": name,
        "chunks_removed": outcome.chunks_removed,
        "memories_forgotten": outcome.memories_forgotten,
        "files_unlinked": outcome.files_unlinked,
    })))
}

/// `GET /api/agents/{id}/training/documents/{doc_id}` — single-document metadata.
pub async fn get_document(
    Path((agent_id, doc_id_str)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, _, _) = require_training_stack(&service)?;
    let (document, corpus_name) = resolve_document_ref(training.as_ref(), &agent_id, &doc_id_str)?;
    let chunks = training
        .list_chunks_for_document(document.id)
        .map_err(|e| internal_err(&format!("list chunks failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "id": document.id.to_string(),
        "short_id": doc_short_id(document.id),
        "corpus": corpus_name,
        "filename": document.original_filename,
        "mime_type": document.mime_type,
        "size_bytes": document.size_bytes,
        "source_kind": document.source_kind.as_str(),
        "processing_status": document.processing_status.as_str(),
        "processing_error": document.processing_error,
        "uploaded_at": document.uploaded_at.to_rfc3339(),
        "uploaded_by": document.uploaded_by,
        "chunk_count": chunks.len(),
    })))
}

/// `GET /api/agents/{id}/training/documents/{doc_id}/download` — stream the original file.
pub async fn download_document(
    Path((agent_id, doc_id_str)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Response, ApiError> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, _, _) = require_training_stack(&service)?;
    let (document, _) = resolve_document_ref(training.as_ref(), &agent_id, &doc_id_str)?;
    let doc_id = document.id;

    let Some(path_str) = document.storage_uri.strip_prefix("file://") else {
        return Err(internal_err(&format!(
            "unsupported storage scheme for document {doc_id}",
        )));
    };
    let file = tokio::fs::File::open(path_str).await.map_err(|e| {
        warn!(
            agent = %agent_id,
            doc_id = %doc_id,
            path = path_str,
            error = %e,
            "training: failed to open original for download",
        );
        match e.kind() {
            std::io::ErrorKind::NotFound => {
                not_found_err("original file is missing from the archive")
            }
            _ => internal_err("failed to open file"),
        }
    })?;

    let disposition =
        content_disposition_attachment(&document.original_filename).map_err(internal_err)?;
    let content_type = HeaderValue::from_str(&document.mime_type)
        .map_err(|_| internal_err(&format!("invalid mime type stored for document {doc_id}",)))?;
    // Deliberately no Content-Length: the SQL `size_bytes` would lie
    // if the on-disk file got truncated, leaving the browser hung
    // waiting for missing bytes. axum emits Transfer-Encoding: chunked
    // when length is absent, which every modern client handles.
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, content_type);
    response_headers.insert(header::CONTENT_DISPOSITION, disposition);
    Ok(response.into_response())
}

/// `DELETE /api/agents/{id}/training/documents/{doc_id}` — cascade delete one document.
pub async fn delete_document(
    Path((agent_id, doc_id_str)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (training, memory, _) = require_training_stack(&service)?;
    let (document, corpus_name) = resolve_document_ref(training.as_ref(), &agent_id, &doc_id_str)?;
    let doc_id = document.id;

    let outcome = delete_document_cascade(training, memory, document.id)
        .await
        .map_err(|e| internal_err(&format!("delete failed: {e}")))?;

    info!(
        agent = %agent_id,
        doc_id = %doc_id,
        corpus = %corpus_name,
        chunks = outcome.chunks_removed,
        memories = outcome.memories_forgotten,
        files = outcome.files_unlinked,
        "training: document deleted via HTTP",
    );

    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "document_id": document.id.to_string(),
        "corpus": corpus_name,
        "chunks_removed": outcome.chunks_removed,
        "memories_forgotten": outcome.memories_forgotten,
        "files_unlinked": outcome.files_unlinked,
    })))
}

// ── helpers ──────────────────────────────────────────────────────────

/// Resolve a `{doc_id}` path parameter to a `Document`. Accepts either
/// a full 36-char UUID or an 8-char hex short id (the same form the
/// CLI's `doc-delete` and the `[[cite:<short_id>]]` markers use). Short
/// ids are resolved by scanning the agent's documents and erroring
/// with a 409 + the candidate UUIDs when more than one match exists.
fn resolve_document_ref(
    training: &dyn TrainingStore,
    agent_id: &str,
    raw: &str,
) -> Result<(Document, String), ApiError> {
    if let Ok(uuid) = Uuid::parse_str(raw) {
        return resolve_agent_document(training, agent_id, uuid);
    }
    let lower = raw.trim().to_lowercase();
    if lower.is_empty() {
        return Err(bad_request_err("empty document reference"));
    }
    if !lower.chars().all(|c| c.is_ascii_hexdigit()) || lower.len() > 16 {
        return Err(bad_request_err(&format!(
            "invalid document reference `{raw}` (expected 36-char UUID or 8-char short id)",
        )));
    }
    let corpora = training
        .list_corpora(agent_id)
        .map_err(|e| internal_err(&format!("lookup failed: {e}")))?;
    let mut matches: Vec<(Document, String)> = Vec::new();
    for corpus in corpora {
        let docs = training
            .list_documents(corpus.id)
            .map_err(|e| internal_err(&format!("lookup failed: {e}")))?;
        for doc in docs {
            if doc_short_id(doc.id) == lower {
                matches.push((doc, corpus.name.clone()));
            }
        }
    }
    match matches.len() {
        0 => Err(not_found_err(&format!(
            "no document with short id `{lower}` for agent `{agent_id}`",
        ))),
        1 => Ok(matches.into_iter().next().expect("len() == 1")),
        _ => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "short id `{lower}` matches {} documents — supply the full UUID instead",
                    matches.len(),
                ),
                "candidates": matches.iter().map(|(d, _)| d.id.to_string()).collect::<Vec<_>>(),
            })),
        )),
    }
}

/// Look up a document by UUID and assert it belongs to a corpus
/// owned by `agent_id`. Returns `(document, corpus_name)` on success;
/// a 404 on any mismatch so cross-agent IDs leak no information.
fn resolve_agent_document(
    training: &dyn TrainingStore,
    agent_id: &str,
    doc_id: Uuid,
) -> Result<(Document, String), ApiError> {
    let not_found = || not_found_err(&format!("no document `{doc_id}` for agent `{agent_id}`"));
    let document = training
        .get_document(doc_id)
        .map_err(|e| internal_err(&format!("lookup failed: {e}")))?
        .ok_or_else(not_found)?;
    let corpus = training
        .get_corpus(document.corpus_id)
        .map_err(|e| internal_err(&format!("lookup failed: {e}")))?
        .ok_or_else(not_found)?;
    if corpus.agent_id != agent_id {
        return Err(not_found());
    }
    let corpus_name = corpus.name;
    Ok((document, corpus_name))
}

/// Build a `Content-Disposition: attachment` header from an arbitrary
/// filename. Delegates the RFC 6266 formatting to agent-runtime (the single
/// source of truth shared with the S3 store) and wraps the result in a
/// `HeaderValue`.
fn content_disposition_attachment(filename: &str) -> Result<HeaderValue, &'static str> {
    let value = workforce_agent::artifacts::disposition::content_disposition_attachment(filename);
    HeaderValue::from_str(&value).map_err(|_| "invalid filename header bytes")
}

struct UploadFields {
    corpus_name: String,
    filename: String,
    bytes: Vec<u8>,
}

/// Parse a single multipart payload. Requires exactly one `corpus`
/// field and one `file` field. Streams the file body into memory; the
/// router-level `DefaultBodyLimit` provides the first DoS guard,
/// `max_file_size_mb` provides the second.
async fn read_upload(
    mut multipart: Multipart,
    max_file_size_mb: usize,
) -> Result<UploadFields, ApiError> {
    let max_bytes = max_file_size_mb.saturating_mul(1024 * 1024);
    let mut corpus_name: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|e| bad_request_err(&format!("multipart parse error: {e}")))?;
        let Some(field) = field else { break };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "corpus" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| bad_request_err(&format!("read corpus field: {e}")))?;
                let trimmed = text.trim();
                validate_slug(trimmed)
                    .map_err(|e| bad_request_err(&format!("`corpus` field {e}")))?;
                corpus_name = Some(trimmed.to_string());
            }
            "file" => {
                let supplied = field.file_name().map(|s| s.to_string());
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request_err(&format!("read file field: {e}")))?;
                if data.len() > max_bytes {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        Json(serde_json::json!({
                            "error": format!(
                                "file exceeds max size of {} MiB",
                                max_file_size_mb,
                            )
                        })),
                    ));
                }
                let safe = supplied
                    .as_deref()
                    .map(sanitize_filename)
                    .unwrap_or_else(|| "upload.bin".to_string());
                filename = Some(safe);
                bytes = Some(data.to_vec());
            }
            other => {
                warn!(field = %other, "training upload: ignoring unknown multipart field");
            }
        }
    }

    let corpus_name = corpus_name.ok_or_else(|| bad_request_err("missing `corpus` field"))?;
    let filename = filename.ok_or_else(|| bad_request_err("missing `file` field"))?;
    let bytes = bytes.ok_or_else(|| bad_request_err("missing `file` field"))?;
    Ok(UploadFields {
        corpus_name,
        filename,
        bytes,
    })
}

/// Build a `VisionContext` from the per-agent service when both a
/// vision LLM and an enabled `[llm.vision]` config are present. Used
/// by the PDF loader's OCR fallback; `None` here keeps the loader on
/// its text-only path.
fn vision_context_for(service: &Arc<AgentService>) -> Option<VisionContext<'_>> {
    let llm = service.describer.llm()?.as_ref();
    let cfg = service
        .effective_llm
        .vision
        .as_ref()
        .filter(|v| v.enabled)?;
    Some(VisionContext { llm, config: cfg })
}

/// Resolve the per-agent training stack: the training store, the memory
/// store, and the effective `TrainingConfig`. Returns `503` when
/// training is disabled or the underlying stores aren't configured.
fn require_training_stack(
    service: &Arc<AgentService>,
) -> Result<(Arc<dyn TrainingStore>, Arc<dyn MemoryStore>, TrainingConfig), ApiError> {
    let training = service
        .training_store
        .clone()
        .ok_or_else(|| service_unavailable_err("training store not configured"))?;
    let memory = service
        .memory_store
        .clone()
        .ok_or_else(|| service_unavailable_err("memory store not configured"))?;
    if !service.effective_training.enabled {
        return Err(service_unavailable_err("training disabled on this hub"));
    }
    Ok((training, memory, service.effective_training.clone()))
}

/// Stash the original upload under `<hub.data_dir>/<training.data_dir>/<agent>/<corpus>/<uuid>__<filename>`.
///
/// Mirrors the CLI direct-mode layout in `crates/cli/src/training/mod.rs::stash_original`
/// so files written either way live in the same place.
///
/// Preconditions enforced by callers:
/// - `agent` came from the agent registry (already validated at
///   registration time)
/// - `corpus` was passed through [`workforce_training::validate_slug`]
///   in the multipart read
/// - `filename` was passed through
///   [`workforce_training::sanitize_filename`] in the multipart read
async fn stash_original(
    cfg: &TrainingConfig,
    service: &Arc<AgentService>,
    agent: &str,
    corpus: &str,
    filename: &str,
    bytes: &[u8],
) -> eyre::Result<String> {
    let raw = StdPath::new(&cfg.data_dir);
    let root = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        PathBuf::from(&service.config.hub.data_dir).join(raw)
    };
    let dest_dir = root.join(agent).join(corpus);
    tokio::fs::create_dir_all(&dest_dir).await?;
    let stamped = format!("{}__{}", Uuid::new_v4().simple(), filename);
    let dest = dest_dir.join(stamped);
    tokio::fs::write(&dest, bytes).await?;
    Ok(format!("file://{}", dest.display()))
}

fn resolve_agent(state: &GatewayState, id: &str) -> Result<Arc<AgentService>, ApiError> {
    state.registry.get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("agent '{}' not found", id)})),
        )
    })
}

fn bad_request_err(msg: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn not_found_err(msg: &str) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn internal_err(msg: &str) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn service_unavailable_err(msg: &str) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": msg })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_uses_plain_form_for_ascii() {
        let hv = content_disposition_attachment("notes.pdf").unwrap();
        assert_eq!(hv.to_str().unwrap(), "attachment; filename=\"notes.pdf\"");
    }

    #[test]
    fn content_disposition_falls_back_for_quotes_or_backslash() {
        let hv = content_disposition_attachment("weird\"name\\.pdf").unwrap();
        let raw = hv.to_str().unwrap();
        assert!(raw.starts_with("attachment; filename=\""));
        assert!(raw.contains("filename*=UTF-8''"));
        // The ASCII fallback strips quotes and backslashes; only the
        // two boundary quotes around `filename="…"` survive.
        assert_eq!(raw.matches('"').count(), 2, "stray quote in: {raw}");
        assert!(!raw.contains('\\'), "stray backslash in: {raw}");
    }

    #[test]
    fn content_disposition_encodes_unicode_filenames() {
        let hv = content_disposition_attachment("café.pdf").unwrap();
        let raw = hv.to_str().unwrap();
        assert!(
            raw.contains("filename=\"caf_.pdf\""),
            "ASCII fallback: {raw}"
        );
        assert!(
            raw.contains("filename*=UTF-8''caf%C3%A9.pdf"),
            "RFC 6266 form: {raw}",
        );
    }
}

//! Session management and chat enhancement REST handlers (DW-427).
//!
//! Implements 12 REST endpoints for session lifecycle management,
//! context inspection, chat enhancements, file upload, and transcript search.
//!
//! Routes registered in [`crate::gateway::router`]:
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | DELETE | `/api/sessions/{id}` | [`delete_session`] |
//! | POST   | `/api/sessions/{id}/reset` | [`reset_session`] |
//! | POST   | `/api/sessions/{id}/stop` | [`stop_session_turn`] |
//! | POST   | `/api/sessions/{id}/model` | [`update_session_model`] |
//! | PUT    | `/api/sessions/{id}/label` | [`update_session_label`] |
//! | GET    | `/api/sessions/{id}/context` | [`get_session_context`] |
//! | GET    | `/api/sessions/{id}/tools` | [`get_session_tool_calls`] |
//! | POST   | `/api/sessions/{id}/messages/{msg_id}/retry` | [`retry_session_message`] |
//! | DELETE | `/api/sessions/{id}/messages/{msg_id}` | [`delete_session_message`] |
//! | POST   | `/api/sessions/{id}/upload` | [`upload_session_file`] |
//! | GET    | `/api/sessions/search` | [`search_sessions`] |

use std::sync::Arc;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use tracing::{info, warn};
use workforce_common::session::{Session, SessionStore};
use workforce_memory::access_audit::{emit, AccessAuditEvent, AuditDecision, AuditOperation};

use crate::gateway::{resolve_caller_context, CallerContext, GatewayState};
use crate::serve::AgentService;

/// Maximum allowed size for a single uploaded file (1 GiB).
///
/// Uploads stream to disk chunk-by-chunk (never buffered whole in memory),
/// so this bounds disk spend per file — bodies purge on the attachment TTL —
/// not memory. Enforced mid-stream, so a lying `Content-Length` can't
/// overshoot it.
pub(crate) const MAX_FILE_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;

// ── Request / Response Types ─────────────────────────────────────────────

/// Query parameters shared by session management endpoints.
#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub token: Option<String>,
    /// Optional agent_id to select which agent's sessions to operate on.
    pub agent_id: Option<String>,
}

/// Request body for `POST /api/sessions/{id}/model`.
#[derive(Debug, Deserialize)]
pub struct UpdateModelRequest {
    /// New model string. Pass an empty string to clear the override.
    pub model: String,
}

/// Request body for `PUT /api/sessions/{id}/label`.
#[derive(Debug, Deserialize)]
pub struct UpdateLabelRequest {
    /// New label. Pass `null` / absent to clear the label.
    pub label: Option<String>,
}

/// Query parameters for `GET /api/sessions/search`.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub token: Option<String>,
    /// Optional agent_id to scope the search.
    pub agent_id: Option<String>,
    /// Full-text search query string.
    pub q: String,
    /// Maximum number of results to return.
    ///
    /// Default: 20. Maximum enforced by the server: 100. Values above 100 are
    /// silently clamped to 100.
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}

fn default_search_limit() -> usize {
    20
}

// ── Helper Functions ──────────────────────────────────────────────────────

fn unauthorized_err() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "unauthorized"})),
    )
}

fn not_found_err(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": msg})),
    )
}

fn forbidden_err(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": msg})),
    )
}

fn internal_err(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg})),
    )
}

fn service_unavailable_err() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "no agent service available"})),
    )
}

/// Resolve an agent service, falling back to the default if `agent_id` is None.
fn resolve_service(
    state: &GatewayState,
    agent_id: Option<&str>,
) -> Result<Arc<AgentService>, (StatusCode, Json<serde_json::Value>)> {
    state
        .registry
        .resolve(agent_id)
        .ok_or_else(service_unavailable_err)
}

/// What a caller asks of a session. Whoever may see a session may read and
/// tidy it; a turn runs as the session's owner, on the owner's private prompt
/// and credentials, so only the owner may start one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAccess {
    Read,
    Write,
    RunTurn,
}

impl SessionAccess {
    fn operation(self) -> AuditOperation {
        match self {
            Self::Read => AuditOperation::SessionRead,
            Self::Write | Self::RunTurn => AuditOperation::SessionWrite,
        }
    }

    fn admits(self, caller: &CallerContext, owner: &str) -> bool {
        match self {
            Self::Read | Self::Write => caller.can_see_owner(owner),
            Self::RunTurn => caller.owns(owner),
        }
    }
}

/// Load `session_id` and verify the caller has `access` to it.
///
/// Returns the loaded session on success. Returns 404 for both "no such
/// session" and "session exists but the caller may not see it" so non-owners
/// cannot enumerate session IDs by status code; a caller who may see the
/// session but not run a turn in it gets 403. Returns 500 on store failure.
/// Emits an `AccessAuditEvent` for every Allow / DeniedOwnerMismatch outcome;
/// mirrors `authorise_session_read` in `crate::gateway`.
fn lookup_owned_session(
    sessions: &dyn SessionStore,
    caller: &CallerContext,
    session_id: &str,
    access: SessionAccess,
) -> Result<Session, (StatusCode, Json<serde_json::Value>)> {
    let session = match sessions.get(session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return Err(not_found_err("session not found")),
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to load session");
            return Err(internal_err("failed to load session"));
        }
    };
    let admitted = access.admits(caller, &session.owner);
    emit(AccessAuditEvent::new(
        access.operation(),
        if admitted {
            AuditDecision::Allow
        } else {
            AuditDecision::DeniedOwnerMismatch
        },
        caller
            .identity_id
            .as_deref()
            .or(caller.device_id.as_deref()),
        Some(&session.owner),
        Some(session_id),
    ));
    if admitted {
        Ok(session)
    } else if caller.can_see_owner(&session.owner) {
        Err(forbidden_err(
            "only the session's owner can run a turn in it",
        ))
    } else {
        Err(not_found_err("session not found"))
    }
}

/// Compose the per-handler authorization preamble: resolve the caller,
/// require authentication, enforce the `sessions` scope, resolve the target
/// agent service, and verify the caller's `access` to the session. Returns
/// the cloned service handle and the loaded session so handlers that need
/// either can use them without a second lookup.
async fn authorise_session_action(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query: &SessionQuery,
    session_id: &str,
    access: SessionAccess,
) -> Result<(Arc<AgentService>, Session), axum::response::Response> {
    let caller = resolve_caller_context(state, headers, query.token.as_deref()).await;
    if !caller.is_authenticated() {
        return Err(unauthorized_err().into_response());
    }
    if !caller.has_scope(crate::jwt::SCOPE_SESSIONS) {
        return Err(forbidden_err("scope 'sessions' required").into_response());
    }
    // Audit F4 (2026-05-15) — throttle mutations only; reads stay free.
    if access != SessionAccess::Read {
        if let Err(retry) =
            crate::gateway::check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        {
            return Err(crate::gateway::too_many_requests_err(retry).into_response());
        }
    }
    let service =
        resolve_service(state, query.agent_id.as_deref()).map_err(IntoResponse::into_response)?;
    let session = lookup_owned_session(service.sessions.as_ref(), &caller, session_id, access)
        .map_err(IntoResponse::into_response)?;
    Ok((service, session))
}

// ── DELETE /api/sessions/{id} ─────────────────────────────────────────────

/// Delete a session and all its messages.
pub async fn delete_session(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service.sessions.delete(&session_id) {
        Ok(()) => {
            info!(session_id = %session_id, "Session deleted via API");
            Json(serde_json::json!({
                "deleted": true,
                "session_id": session_id,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to delete session");
            not_found_err("session not found").into_response()
        }
    }
}

// ── POST /api/sessions/{id}/reset ────────────────────────────────────────

/// Reset a session — clear all messages while retaining metadata.
pub async fn reset_session(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service.sessions.reset_session(&session_id) {
        Ok(()) => {
            info!(session_id = %session_id, "Session reset via API");
            Json(serde_json::json!({
                "reset": true,
                "session_id": session_id,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to reset session");
            not_found_err("session not found").into_response()
        }
    }
}

// ── POST /api/sessions/{id}/stop ─────────────────────────────────────────

/// Stop the turn running in a session, on whichever instance runs it.
/// Stopping runs nothing as the owner and returns nothing of the turn, so
/// whoever may see the session may stop it: its owner, or an administrator
/// ending a runaway turn. `stopped` is false when no turn was live.
pub async fn stop_session_turn(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    let stopped = crate::gateway::steer(
        state.nats.as_ref(),
        &service,
        &session_id,
        workforce_nats::chat::TurnControl::Cancel,
    )
    .await;
    info!(session_id = %session_id, stopped, "Turn stop requested via API");
    Json(serde_json::json!({
        "session_id": session_id,
        "stopped": stopped,
    }))
    .into_response()
}

// ── POST /api/sessions/{id}/model ────────────────────────────────────────

/// Switch the model override for a session.
pub async fn update_session_model(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateModelRequest>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    // Empty string means "clear override"
    let model = if body.model.trim().is_empty() {
        None
    } else {
        Some(body.model.clone())
    };
    match service
        .sessions
        .set_session_model(&session_id, model.clone())
    {
        Ok(()) => {
            info!(session_id = %session_id, model = ?model, "Session model updated via API");
            Json(serde_json::json!({
                "updated": true,
                "session_id": session_id,
                "model": model,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to update session model");
            not_found_err("session not found").into_response()
        }
    }
}

// ── PUT /api/sessions/{id}/label ─────────────────────────────────────────

/// Set or clear the human-readable label for a session.
pub async fn update_session_label(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<UpdateLabelRequest>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service
        .sessions
        .set_session_label(&session_id, body.label.clone())
    {
        Ok(()) => {
            info!(session_id = %session_id, label = ?body.label, "Session label updated via API");
            Json(serde_json::json!({
                "updated": true,
                "session_id": session_id,
                "label": body.label,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to update session label");
            not_found_err("session not found").into_response()
        }
    }
}

// ── GET /api/sessions/{id}/context ───────────────────────────────────────

/// Get context window usage for a session (token count, capacity, compaction status).
pub async fn get_session_context(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Read)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service.sessions.get_context_usage(&session_id) {
        Ok(usage) => Json(serde_json::json!({
            "session_id": session_id,
            "context": usage,
        }))
        .into_response(),
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to get session context");
            not_found_err("session not found").into_response()
        }
    }
}

// ── GET /api/sessions/{id}/tools ─────────────────────────────────────────

/// Get all tool calls made in a session with timestamps and results.
pub async fn get_session_tool_calls(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Read)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service.sessions.get_session_tool_calls(&session_id) {
        Ok(calls) => Json(serde_json::json!({
            "session_id": session_id,
            "total": calls.len(),
            "tool_calls": calls,
        }))
        .into_response(),
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "Failed to get session tool calls");
            not_found_err("session not found").into_response()
        }
    }
}

// ── POST /api/sessions/{id}/messages/{msg_id}/retry ──────────────────────

/// Retry a specific user message in a session.
///
/// Truncates session history from the target message (inclusive) onwards, then
/// re-processes the message content. This gives the LLM a clean context without
/// the original response or any subsequent messages — producing a genuine retry
/// rather than appending a duplicate to existing history.
///
/// Only user messages can be retried; attempting to retry assistant or tool
/// messages returns 400 Bad Request. The turn runs as the session's owner, so
/// only the owner may retry: an administrator viewing the session gets 403.
pub async fn retry_session_message(
    Path((session_id, message_id)): Path<(String, String)>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, session) = match authorise_session_action(
        &state,
        &headers,
        &query,
        &session_id,
        SessionAccess::RunTurn,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Find the target message and validate it.
    let msg = match session.messages.iter().find(|m| m.id == message_id) {
        Some(m) => m.clone(),
        None => return not_found_err("message not found").into_response(),
    };

    // Only user messages can be retried.
    if msg.role != workforce_common::session::MessageRole::User {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "only user messages can be retried"})),
        )
            .into_response();
    }

    let content = msg.content.clone();
    let channel = session.channel.clone();
    let owner = session.owner.clone();

    // Truncate the session from the target message (inclusive). This removes the
    // user message and everything that followed it (assistant response, tool calls, etc.)
    // so the LLM sees clean history when we re-submit below.
    if let Err(e) = service
        .sessions
        .truncate_from_message(&session_id, &message_id)
    {
        warn!(
            session_id = %session_id,
            message_id = %message_id,
            error = %e,
            "Failed to truncate session for retry"
        );
        return internal_err("failed to truncate session history").into_response();
    }

    // Re-process the original message content against the now-truncated session.
    match service
        .process_message(
            &channel, &owner, &content, None, None, None, None, None, None, None, None, None, None,
        )
        .await
    {
        Ok(response) => Json(serde_json::json!({
            "session_id": session_id,
            "message_id": message_id,
            "response": response,
        }))
        .into_response(),
        Err(e) => {
            warn!(
                session_id = %session_id,
                message_id = %message_id,
                error = %e,
                "Failed to retry message"
            );
            internal_err("failed to process message").into_response()
        }
    }
}

// ── DELETE /api/sessions/{id}/messages/{msg_id} ──────────────────────────

/// Delete a specific message from a session.
pub async fn delete_session_message(
    Path((session_id, message_id)): Path<(String, String)>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    match service.sessions.delete_message(&session_id, &message_id) {
        Ok(()) => {
            info!(
                session_id = %session_id,
                message_id = %message_id,
                "Session message deleted via API"
            );
            Json(serde_json::json!({
                "deleted": true,
                "session_id": session_id,
                "message_id": message_id,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(
                session_id = %session_id,
                message_id = %message_id,
                error = %e,
                "Failed to delete session message"
            );
            not_found_err("session or message not found").into_response()
        }
    }
}

// ── POST /api/sessions/{id}/upload ───────────────────────────────────────

/// Upload one or more files to a session.
///
/// Accepts `multipart/form-data`. Each file field is stored under
/// `<agent_workspace>/uploads/<session_id>/<filename>`.
///
/// Returns a list of uploaded file metadata including paths that can be
/// referenced in subsequent chat messages.
pub async fn upload_session_file(
    Path(session_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let (service, _) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Write)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    // Uploads go to the shared artifact store (not local disk) so any instance
    // can serve them; the caller stamps ownership for the serving route.
    let caller = resolve_caller_context(&state, &headers, query.token.as_deref()).await;
    let store = match workforce_agent::artifacts::global_store() {
        Some(s) => s,
        None => return internal_err("artifact store unavailable").into_response(),
    };
    let agent_id = service.agent_id.as_str().to_string();

    let mut uploaded_files: Vec<serde_json::Value> = Vec::new();
    // Ids stored so far in THIS request — rolled back if a later file trips
    // the cap, so a 413'd retry doesn't strand paid-for gigabytes for a day.
    let mut stored_ids: Vec<workforce_agent::artifacts::ArtifactId> = Vec::new();

    // Where field bytes stream to before entering the store. The store's
    // staging dir keeps the final `write_from_path` a rename; the OS temp
    // dir fallback costs one cross-filesystem copy.
    let staging_root = store.staging_dir(&agent_id);

    // Process each multipart field
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let filename = field
                    .file_name()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("upload_{}", uuid::Uuid::new_v4()));

                let content_type = field
                    .content_type()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());

                // Sanitize filename — prevent path traversal
                let safe_filename = std::path::Path::new(&filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("upload")
                    .to_string();

                // Stream the field to disk, enforcing the per-file cap
                // mid-stream — the whole point of this endpoint is files
                // too large to hold in memory.
                let spooled =
                    match spool_field_to_disk(field, staging_root.as_deref(), &safe_filename).await
                    {
                        Ok(s) => s,
                        Err(SpoolError::TooLarge) => {
                            warn!(
                                session_id = %session_id,
                                filename = %safe_filename,
                                limit = MAX_FILE_UPLOAD_BYTES,
                                "Uploaded file exceeds size limit, rejecting"
                            );
                            for id in &stored_ids {
                                let _ = store.delete(&agent_id, id);
                            }
                            return (
                                StatusCode::PAYLOAD_TOO_LARGE,
                                Json(serde_json::json!({
                                    "error": format!(
                                        "file '{}' exceeds the maximum allowed size of {} MiB",
                                        safe_filename,
                                        MAX_FILE_UPLOAD_BYTES / (1024 * 1024)
                                    )
                                })),
                            )
                                .into_response();
                        }
                        Err(SpoolError::Io(e)) => {
                            warn!(
                                session_id = %session_id,
                                filename = %safe_filename,
                                error = %e,
                                "Failed to spool multipart field to disk"
                            );
                            continue;
                        }
                    };

                let mut descriptor = workforce_agent::artifacts::ArtifactDescriptor::from_context(
                    "session.upload",
                    workforce_agent::artifacts::ContentType::OctetStream,
                    agent_id.clone(),
                )
                .with_file_meta(safe_filename.clone(), content_type.clone());
                descriptor.session_id = Some(session_id.clone());
                descriptor.user_id = caller.identity_id.clone();
                descriptor.ttl_seconds =
                    workforce_agent::artifacts::store::ATTACHMENT_TTL.as_secs();

                let handle = match store.write_from_path(&agent_id, descriptor, &spooled.path) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(
                            session_id = %session_id,
                            filename = %safe_filename,
                            error = %e,
                            "Failed to store uploaded file"
                        );
                        spooled.cleanup();
                        continue;
                    }
                };

                stored_ids.push(handle.id.clone());
                info!(
                    session_id = %session_id,
                    filename = %safe_filename,
                    bytes = spooled.bytes,
                    artifact_id = %handle.id,
                    "File uploaded via API"
                );

                // id-keyed URL served cross-instance by `session_file_get`.
                let relative_url =
                    format!("/api/sessions/{}/files/{}", session_id, handle.id.as_str());
                uploaded_files.push(serde_json::json!({
                    "filename": safe_filename,
                    "content_type": content_type,
                    "size_bytes": spooled.bytes,
                    "url": relative_url,
                    // Chat messages reference the payload with this id
                    // (`attachments[].artifact_id`) instead of inlining bytes.
                    "artifact_id": handle.id.as_str(),
                }));
            }
            Ok(None) => break,
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "Multipart read error");
                break;
            }
        }
    }

    if uploaded_files.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "no files were successfully uploaded"})),
        )
            .into_response();
    }

    Json(serde_json::json!({
        "session_id": session_id,
        "uploaded": uploaded_files.len(),
        "files": uploaded_files,
    }))
    .into_response()
}

/// A multipart field streamed to disk, plus enough to hand it to
/// [`workforce_agent::artifacts::ArtifactStore::write_from_path`] (which
/// consumes the file by rename) or clean it up on a failed store write.
struct SpooledUpload {
    path: std::path::PathBuf,
    bytes: u64,
    /// Owns the tempdir in the OS-temp fallback so it outlives the store
    /// write; `None` when spooling into the store's staging dir.
    _tmpdir: Option<tempfile::TempDir>,
}

impl SpooledUpload {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum SpoolError {
    TooLarge,
    Io(eyre::Report),
}

impl From<std::io::Error> for SpoolError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.into())
    }
}

/// Removes the spool file on drop unless disarmed — a client that aborts
/// mid-upload drops the handler future between chunks, and nothing else
/// would ever clean a half-written `.upload-*.tmp` out of the staging dir.
struct SpoolGuard(Option<std::path::PathBuf>);

impl SpoolGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for SpoolGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Stream one multipart field to disk chunk-by-chunk, failing at
/// [`MAX_FILE_UPLOAD_BYTES`] without reading further — the request body is
/// never buffered in memory, which is what lets the upload cap sit at
/// video scale.
async fn spool_field_to_disk(
    mut field: axum::extract::multipart::Field<'_>,
    staging_root: Option<&std::path::Path>,
    filename: &str,
) -> Result<SpooledUpload, SpoolError> {
    use tokio::io::AsyncWriteExt;

    let (dir, tmpdir) = match staging_root {
        Some(root) => (root.to_path_buf(), None),
        None => {
            let t = tempfile::TempDir::new().map_err(|e| SpoolError::Io(e.into()))?;
            (t.path().to_path_buf(), Some(t))
        }
    };
    let path = dir.join(format!(".upload-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&path).await?;
    let mut guard = SpoolGuard(Some(path.clone()));
    let mut written: u64 = 0;

    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                written += chunk.len() as u64;
                if written > MAX_FILE_UPLOAD_BYTES {
                    return Err(SpoolError::TooLarge);
                }
                file.write_all(&chunk).await?;
            }
            Ok(None) => break,
            Err(e) => {
                return Err(SpoolError::Io(eyre::eyre!(
                    "multipart read failed for `{filename}`: {e}"
                )));
            }
        }
    }
    file.flush().await?;
    drop(file);
    guard.disarm();
    Ok(SpooledUpload {
        path,
        bytes: written,
        _tmpdir: tmpdir,
    })
}

// ── GET /api/sessions/{id}/files/{name} ──────────────────────────────────

/// An uploaded artifact must carry the session it was uploaded to, so a session
/// owner cannot read another session's upload by guessing/replaying an id.
fn artifact_belongs_to_session(
    descriptor: &workforce_agent::artifacts::ArtifactDescriptor,
    session_id: &str,
) -> bool {
    descriptor.session_id.as_deref() == Some(session_id)
}

/// Serve a session upload by artifact id, cross-instance. Session ownership is
/// proven by `authorise_session_action`; the artifact must additionally belong
/// to this session (a session owner must not read another session's upload by
/// id). Reuses the shared-store dispatch (S3 presigned redirect / Fs stream).
pub async fn session_file_get(
    Path((session_id, name)): Path<(String, String)>,
    Query(query): Query<SessionQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> axum::response::Response {
    let (service, _session) =
        match authorise_session_action(&state, &headers, &query, &session_id, SessionAccess::Read)
            .await
        {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    let art_id = match workforce_agent::artifacts::ArtifactId::parse(&name) {
        Ok(id) => id,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let store = match workforce_agent::artifacts::global_store() {
        Some(s) => s,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let agent_id = service.agent_id.as_str();
    let descriptor = match store.metadata(agent_id, &art_id) {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if !artifact_belongs_to_session(&descriptor, &session_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match crate::gateway::serve_runtime_artifact(
        store.as_ref(),
        agent_id,
        &art_id,
        &descriptor,
        std::time::Duration::from_secs(300),
    ) {
        Ok(resp) => resp,
        Err(code) => code.into_response(),
    }
}

// ── GET /api/sessions/search ─────────────────────────────────────────────

/// Full-text search across session transcripts.
///
/// Returns sessions and messages whose content contains the query string.
/// Results are ordered by message timestamp (most recent first).
pub async fn search_sessions(
    Query(query): Query<SearchQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let caller = match crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_SESSIONS,
    )
    .await
    {
        Ok(c) => c,
        Err(rejection) => return rejection.into_response(),
    };
    let service = match resolve_service(&state, query.agent_id.as_deref()) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let q = query.q.trim().to_string();
    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "search query 'q' is required"})),
        )
            .into_response();
    }

    let limit = query.limit.min(100);

    match service.sessions.search_sessions(&q, limit) {
        Ok(results) => {
            let visible: Vec<_> = results
                .into_iter()
                .filter(|r| {
                    service
                        .sessions
                        .get(&r.session_id)
                        .ok()
                        .flatten()
                        .is_some_and(|s| caller.can_see_owner(&s.owner))
                })
                .collect();
            Json(serde_json::json!({
                "query": q,
                "total": visible.len(),
                "results": visible,
            }))
            .into_response()
        }
        Err(e) => {
            warn!(query = %q, error = %e, "Session search failed");
            internal_err("search failed").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;
    use workforce_common::session::InMemorySessionStore;

    fn caller_with(identity: Option<&str>, device: Option<&str>, is_admin: bool) -> CallerContext {
        CallerContext {
            identity_id: identity.map(str::to_owned),
            device_id: device.map(str::to_owned),
            is_admin,
            scopes: vec![crate::jwt::SCOPE_WILDCARD.to_string()],
            auth_failure: None,
        }
    }

    fn store_with_session(owner: &str) -> (InMemorySessionStore, String) {
        let store = InMemorySessionStore::default();
        let session = store
            .get_or_create("test-channel", owner, "test-agent")
            .expect("create test session");
        (store, session.id)
    }

    #[test]
    fn owner_can_access_own_session() {
        let (store, session_id) = store_with_session("alice");
        let caller = caller_with(Some("alice"), None, false);

        let session = lookup_owned_session(&store, &caller, &session_id, SessionAccess::Write)
            .expect("owner must be allowed");
        assert_eq!(session.owner, "alice");
        assert_eq!(session.id, session_id);
    }

    /// Security-critical: a non-owner must receive 404, not 403. A 403 would
    /// confirm session-ID existence and let an attacker enumerate IDs.
    #[test]
    fn non_owner_gets_not_found_not_forbidden() {
        let (store, session_id) = store_with_session("alice");
        let caller = caller_with(Some("bob"), None, false);

        let (status, _) = lookup_owned_session(&store, &caller, &session_id, SessionAccess::Write)
            .expect_err("non-owner must be rejected");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn admin_can_access_any_session() {
        let (store, session_id) = store_with_session("alice");
        let caller = caller_with(None, None, true);

        let session = lookup_owned_session(&store, &caller, &session_id, SessionAccess::Read)
            .expect("admin must be allowed");
        assert_eq!(session.owner, "alice");
    }

    #[test]
    fn missing_session_returns_not_found() {
        let store = InMemorySessionStore::default();
        let caller = caller_with(Some("alice"), None, false);

        let (status, _) = lookup_owned_session(&store, &caller, "missing-id", SessionAccess::Write)
            .expect_err("missing session must error");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn paired_device_can_access_session_owned_by_device_id() {
        let (store, session_id) = store_with_session("device-abc");
        let caller = caller_with(None, Some("device-abc"), false);

        lookup_owned_session(&store, &caller, &session_id, SessionAccess::Read)
            .expect("device owner must be allowed");
    }

    /// Anonymous callers should be filtered by the handler's `is_authenticated`
    /// gate, but the helper itself must also deny — defence in depth — by
    /// returning the same 404 a non-owner sees.
    #[test]
    fn anonymous_caller_gets_not_found() {
        let (store, session_id) = store_with_session("alice");
        let caller = CallerContext::default();

        let (status, _) = lookup_owned_session(&store, &caller, &session_id, SessionAccess::Write)
            .expect_err("anonymous must be rejected");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn run_turn_status(owner: &str, caller: &CallerContext) -> Option<StatusCode> {
        let (store, session_id) = store_with_session(owner);
        lookup_owned_session(&store, caller, &session_id, SessionAccess::RunTurn)
            .err()
            .map(|(status, _)| status)
    }

    #[test]
    fn only_the_owner_may_run_a_turn_in_a_session() {
        assert_eq!(
            run_turn_status("alice", &caller_with(Some("alice"), None, false)),
            None
        );
        assert_eq!(
            run_turn_status("device-abc", &caller_with(None, Some("device-abc"), false)),
            None
        );
        assert_eq!(
            run_turn_status("alice", &caller_with(Some("alice"), None, true)),
            None,
            "an administrator still runs turns in their own session"
        );
    }

    #[test]
    fn an_administrator_may_view_but_not_run_a_turn_in_anothers_session() {
        for admin in [
            caller_with(None, None, true),
            caller_with(Some("root"), None, true),
        ] {
            assert_eq!(
                run_turn_status("alice", &admin),
                Some(StatusCode::FORBIDDEN)
            );
            let (store, session_id) = store_with_session("alice");
            lookup_owned_session(&store, &admin, &session_id, SessionAccess::Read)
                .expect("viewing is unchanged");
            lookup_owned_session(&store, &admin, &session_id, SessionAccess::Write)
                .expect("housekeeping is unchanged");
        }
    }

    #[test]
    fn another_member_cannot_tell_a_session_exists_by_running_a_turn() {
        assert_eq!(
            run_turn_status("alice", &caller_with(Some("bob"), None, false)),
            Some(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            run_turn_status("alice", &CallerContext::default()),
            Some(StatusCode::NOT_FOUND)
        );
    }

    /// A shared conversation belongs to its thread, not to a person: each
    /// speaker's turn enters through the channel as that speaker, and nobody
    /// re-runs one from here.
    #[test]
    fn nobody_runs_a_turn_in_a_shared_conversation_by_session_id() {
        let shared = format!(
            "{}1700000000.0001",
            workforce_common::session::SHARED_OWNER_PREFIX
        );
        assert_eq!(
            run_turn_status(&shared, &caller_with(None, None, true)),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            run_turn_status(&shared, &caller_with(Some("alice"), None, false)),
            Some(StatusCode::NOT_FOUND)
        );
    }

    /// A gateway serving one agent in which alice holds a two-message
    /// session, with the administrator's headers and the id of her message.
    async fn gateway_with_alices_session() -> (
        tempfile::TempDir,
        Arc<GatewayState>,
        Arc<AgentService>,
        Session,
        String,
    ) {
        let mut fixture = crate::remote_mcp::testing::fixture(false);
        let tmp = tempfile::tempdir().unwrap();
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = tmp.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let service = AgentService::from_config(config, None)
            .await
            .expect("test service");
        let mut session = service
            .sessions
            .get_or_create("rest", "alice", service.agent_id.as_str())
            .unwrap();
        let message_id = session.add_user_message("what is in my notes?", None);
        session.add_assistant_message("three things", 0);
        service.sessions.save(&session).unwrap();
        let state = Arc::get_mut(&mut fixture.state).expect("sole owner of the state");
        state.registry = Arc::new(crate::agent_registry::AgentRegistry::single(
            service.clone(),
        ));
        (tmp, fixture.state, service, session, message_id)
    }

    fn default_agent() -> Query<SessionQuery> {
        Query(SessionQuery {
            token: None,
            agent_id: None,
        })
    }

    /// Through the route itself: the refusal lands before the history is
    /// truncated, so an administrator's retry leaves the owner's session whole.
    #[tokio::test]
    async fn an_administrators_retry_is_refused_and_leaves_the_session_whole() {
        let (_tmp, state, service, session, message_id) = gateway_with_alices_session().await;
        let admin = crate::remote_mcp::testing::bearer(&state.auth_token);

        let response = retry_session_message(
            Path((session.id.clone(), message_id)),
            default_agent(),
            admin,
            State(state),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let kept = service.sessions.get(&session.id).unwrap().unwrap();
        assert_eq!(kept.messages.len(), session.messages.len());
    }

    async fn stop(
        state: &Arc<GatewayState>,
        session_id: &str,
        headers: axum::http::HeaderMap,
    ) -> (StatusCode, serde_json::Value) {
        let response = stop_session_turn(
            Path(session_id.to_string()),
            default_agent(),
            headers,
            State(Arc::clone(state)),
        )
        .await
        .into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    /// Stopping is not acting as the owner: an administrator ends a turn
    /// running in another person's session, and nobody unauthenticated does.
    #[tokio::test]
    async fn an_administrator_stops_a_turn_running_in_anothers_session() {
        let (_tmp, state, service, session, _) = gateway_with_alices_session().await;
        let admin = || crate::remote_mcp::testing::bearer(&state.auth_token);

        let (status, idle) = stop(&state, &session.id, admin()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(idle["stopped"], false, "no turn was running");

        let (turn, _inbox) = service
            .live_turns
            .begin(
                &session.id,
                workforce_agent::tools::approval::SelfServe::Unanswerable,
                None,
            )
            .await
            .expect("no live turn yet");
        let stopping = turn.cancel_token();
        let running = tokio::spawn(async move {
            stopping.cancelled().await;
            drop(turn);
        });

        let (status, _) = stop(&state, &session.id, axum::http::HeaderMap::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!running.is_finished(), "an anonymous stop reaches no turn");

        let (status, stopped) = stop(&state, &session.id, admin()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stopped["stopped"], true);
        running.await.unwrap();
    }

    #[test]
    fn artifact_session_scope_rejects_foreign_and_unstamped() {
        let mut d = workforce_agent::artifacts::ArtifactDescriptor::from_context(
            "session.upload",
            workforce_agent::artifacts::ContentType::OctetStream,
            "agent-a",
        );
        d.session_id = Some("sess-1".to_string());
        assert!(artifact_belongs_to_session(&d, "sess-1"));
        assert!(!artifact_belongs_to_session(&d, "sess-2"));
        d.session_id = None;
        assert!(!artifact_belongs_to_session(&d, "sess-1"));
    }
}

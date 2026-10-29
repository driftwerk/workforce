//! Memory browser API endpoints (DW-425).
//!
//! - `GET /api/agents/{id}/memory/files` — List memory files.
//! - `GET /api/agents/{id}/memory/files/{*path}` — Read a memory file.
//! - `PUT /api/agents/{id}/memory/files/{*path}` — Write a memory file (owner-only).
//! - `GET /api/agents/{id}/memory/tasks` — List session tasks.
//! - `GET /api/agents/{id}/memory/stats` — Memory usage statistics.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use tracing::info;

use crate::gateway::GatewayState;

use super::errors::ApiError;

// ── Constants ─────────────────────────────────────────────────────────────

/// Safe file names allowed for memory operations.
const ALLOWED_MEMORY_FILES: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "TOOLS.md",
    "IDENTITY.md",
    "DEBUG_CHECKLIST.md",
];

// ── Request types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MemoryWriteBody {
    pub content: String,
}

// ── Path validation ───────────────────────────────────────────────────────

/// Validate a memory file path — prevent directory traversal and restrict
/// to the known allowlist of identity files + daily notes.
fn validate_memory_path(path: &str) -> bool {
    // Block path traversal
    if path.contains("..") || path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    // Allow known top-level files
    if ALLOWED_MEMORY_FILES.contains(&path) {
        return true;
    }
    // Shared notes directly under `memory/` only: below it sit each user's own
    // notes (`memory/users/<id>/`), which no caller of this API may address.
    path.strip_prefix("memory/")
        .is_some_and(|name| name.ends_with(".md") && !name.contains(['/', '\\']))
}

/// Whether `caller` may see the shared note `name` directly under `memory/`.
/// A dated one was written before notes were kept per person, or by a turn
/// bound to nobody, and mixes whoever spoke that day: only an administrator
/// sees it.
fn shared_note_visible(name: &str, caller: &crate::gateway::CallerContext) -> bool {
    caller.is_admin || !workforce_common::identity::is_daily_note_name(name)
}

// ── Endpoints ─────────────────────────────────────────────────────────────

/// GET /api/agents/{id}/memory/files — List memory files.
pub async fn memory_list_files(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (service, caller) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;
    let workspace = &service.workspace;

    let mut files = Vec::new();

    // List top-level memory files
    for &name in ALLOWED_MEMORY_FILES {
        let path = workspace.join(name);
        if path.is_file() {
            let meta = std::fs::metadata(&path).ok();
            files.push(serde_json::json!({
                "path": name,
                "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                "modified": meta.and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.to_rfc3339()
                    }),
            }));
        }
    }

    // List daily notes in memory/ directory
    let memory_dir = workspace.join("memory");
    if memory_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") && shared_note_visible(&name, &caller) {
                    let meta = entry.metadata().ok();
                    files.push(serde_json::json!({
                        "path": format!("memory/{}", name),
                        "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                        "modified": meta.and_then(|m| m.modified().ok())
                            .map(|t| {
                                let dt: chrono::DateTime<chrono::Utc> = t.into();
                                dt.to_rfc3339()
                            }),
                    }));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "agent_id": id,
        "files": files,
        "count": files.len(),
    })))
}

/// GET /api/agents/{id}/memory/files/{*path} — Read a memory file.
pub async fn memory_read_file(
    Path((id, file_path)): Path<(String, String)>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (service, caller) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;

    if !validate_memory_path(&file_path) {
        return Err(ApiError::forbidden(format!(
            "access denied: '{}' is not an allowed memory file path",
            file_path
        )));
    }

    let not_found = || ApiError::not_found(format!("file '{}' not found", file_path));
    if file_path
        .strip_prefix("memory/")
        .is_some_and(|name| !shared_note_visible(name, &caller))
    {
        return Err(not_found());
    }
    let full_path = service.workspace.join(&file_path);
    let content = std::fs::read_to_string(&full_path).map_err(|_| not_found())?;

    let meta = std::fs::metadata(&full_path).ok();
    Ok(Json(serde_json::json!({
        "agent_id": id,
        "path": file_path,
        "content": content,
        "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
    })))
}

/// PUT /api/agents/{id}/memory/files/{*path} — Write a memory file (owner-only).
pub async fn memory_write_file(
    Path((id, file_path)): Path<(String, String)>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<MemoryWriteBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !super::check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err(ApiError::unauthorized());
    }
    let service = super::resolve_agent(&state, &id)?;

    if !validate_memory_path(&file_path) {
        return Err(ApiError::forbidden(format!(
            "access denied: '{}' is not an allowed memory file path",
            file_path
        )));
    }

    let full_path = service.workspace.join(&file_path);

    // Ensure parent directory exists
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal("failed to create directory").with_detail(e.to_string())
        })?;
    }

    let written = match workforce_common::config_files::is_agent_config_name(std::ffi::OsStr::new(
        &file_path,
    )) {
        true => workforce_common::config_files::replace(&full_path, body.content.as_bytes()),
        false => std::fs::write(&full_path, &body.content),
    };
    written.map_err(|e| ApiError::internal("failed to write file").with_detail(e.to_string()))?;

    info!(agent = %id, path = %file_path, "Memory file written via API");
    Ok(Json(serde_json::json!({
        "written": file_path,
        "size": body.content.len(),
    })))
}

/// GET /api/agents/{id}/memory/tasks — List session tasks.
pub async fn memory_list_tasks(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (service, _) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;
    let workspace = &service.workspace;

    // Tasks are stored in .local/session-tasks/ as JSON files
    let tasks_dir = workspace.join(".local/session-tasks");
    let mut all_tasks = Vec::new();

    if tasks_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        if let Ok(tasks) = serde_json::from_str::<serde_json::Value>(&content) {
                            let session_id = name.trim_end_matches(".json");
                            if let Some(arr) = tasks.as_array() {
                                for task in arr {
                                    let mut t = task.clone();
                                    if let Some(obj) = t.as_object_mut() {
                                        obj.insert(
                                            "session_id".to_string(),
                                            serde_json::Value::String(session_id.to_string()),
                                        );
                                    }
                                    all_tasks.push(t);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "agent_id": id,
        "tasks": all_tasks,
        "count": all_tasks.len(),
    })))
}

/// GET /api/agents/{id}/memory/stats — Memory usage statistics.
pub async fn memory_stats(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (service, caller) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;
    let workspace = &service.workspace;

    let mut file_count: usize = 0;
    let mut total_size: u64 = 0;
    let mut daily_note_count: usize = 0;

    // Count top-level memory files
    for &name in ALLOWED_MEMORY_FILES {
        let path = workspace.join(name);
        if path.is_file() {
            file_count += 1;
            total_size += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
    }

    // Count daily notes
    let memory_dir = workspace.join("memory");
    if memory_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") && shared_note_visible(&name, &caller) {
                    daily_note_count += 1;
                    file_count += 1;
                    total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }

    // Count session tasks
    let tasks_dir = workspace.join(".local/session-tasks");
    let task_file_count = if tasks_dir.is_dir() {
        std::fs::read_dir(&tasks_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .count()
            })
            .unwrap_or(0)
    } else {
        0
    };

    Ok(Json(serde_json::json!({
        "agent_id": id,
        "file_count": file_count,
        "total_size_bytes": total_size,
        "daily_note_count": daily_note_count,
        "task_session_count": task_file_count,
    })))
}

/// `GET /api/agents/{id}/memory/summary`: how much the agent remembers of
/// the caller, how many lessons it has learned, and when the caller's
/// memories were last tidied (consolidated). A caller who is no one (the
/// install's own token, as Desk uses for the hub on its Mac) is told about
/// everyone's. For Desk's Settings tab; the memories themselves are
/// reached through the agent.
pub async fn memory_summary(
    Path(id): Path<String>,
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use workforce_memory::types::{MemoryFilter, MemorySource};
    let (service, caller) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;
    let identity = caller.identity_id.clone();
    let tidied =
        crate::memory_consolidation::last_consolidated(&service.workspace, identity.as_deref());
    let Some(store) = service.memory_store.as_ref() else {
        return Ok(Json(serde_json::json!({
            "agent_id": id, "memories": 0, "lessons": 0, "tidied_at": null,
        })));
    };
    let unavailable = |e: eyre::Report| {
        tracing::warn!(agent = %id, error = %e, "memory summary: the memory store did not answer");
        ApiError::from((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "the memory store did not answer"})),
        ))
    };
    let count = |identity: Option<String>, source: Option<MemorySource>| {
        store.count(MemoryFilter {
            owner: Some(service.agent_id.clone()),
            identity_id: identity,
            source,
            ..Default::default()
        })
    };
    let all = count(identity.clone(), None).await.map_err(unavailable)?;
    let their_lessons = count(identity.clone(), Some(MemorySource::Lesson))
        .await
        .map_err(unavailable)?;
    let lessons = count(None, Some(MemorySource::Lesson))
        .await
        .map_err(unavailable)?;
    Ok(Json(serde_json::json!({
        "agent_id": id,
        "memories": all.saturating_sub(their_lessons),
        "lessons": lessons,
        "tidied_at": tidied.map(|t| t.to_rfc3339()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_memory_path_allows_known_files() {
        assert!(validate_memory_path("SOUL.md"));
        assert!(validate_memory_path("IDENTITY.md"));
        assert!(!validate_memory_path("PROJECT.md"));
        assert!(!validate_memory_path("USER.md"));
        assert!(!validate_memory_path("MEMORY.md"));
    }

    #[test]
    fn a_shared_dated_note_is_an_administrator_s_alone() {
        let member = crate::gateway::CallerContext {
            identity_id: Some("ada".into()),
            ..Default::default()
        };
        let admin = crate::gateway::CallerContext {
            is_admin: true,
            ..Default::default()
        };
        assert!(!shared_note_visible("2026-03-14.md", &member));
        assert!(shared_note_visible("2026-03-14.md", &admin));
        assert!(shared_note_visible("active-task.md", &member));
        assert!(shared_note_visible("projects.md", &member));
    }

    #[test]
    fn test_validate_memory_path_allows_daily_notes() {
        assert!(validate_memory_path("memory/2026-03-14.md"));
        assert!(validate_memory_path("memory/notes.md"));
    }

    #[test]
    fn test_validate_memory_path_blocks_traversal() {
        assert!(!validate_memory_path("../etc/passwd"));
        assert!(!validate_memory_path("memory/../../secret"));
        assert!(!validate_memory_path("/etc/passwd"));
        assert!(!validate_memory_path("\\windows\\system32"));
    }

    #[test]
    fn another_users_notes_are_not_a_memory_file_path() {
        assert!(!validate_memory_path("memory/users/uid-b/2026-10-01.md"));
        assert!(!validate_memory_path("memory/users/uid-b/profile.md"));
        assert!(!validate_memory_path("memory/compaction/2026-10-01.md"));
    }

    #[test]
    fn test_validate_memory_path_blocks_unknown_files() {
        assert!(!validate_memory_path(""));
        assert!(!validate_memory_path("MEMORY\0.md"));
        assert!(!validate_memory_path("memory/secret.json"));
        assert!(!validate_memory_path("config.toml"));
        assert!(!validate_memory_path("src/main.rs"));
        assert!(!validate_memory_path(".env"));
    }
}

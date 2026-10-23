//! `/api/auth/{provider}/callback` axum route.
//!
//! Vendors redirect the user here after consent with `?state=…&code=…`.
//! We look up the pending state in the shared `OAuthStateStore`,
//! find the agent that owns it (state entry `workspace` field
//! carries the agent_id), and dispatch into that agent's
//! [`super::manager::McpServerManager::authorize_complete`].
//!
//! The route is public — only the random opaque `state` token gates
//! callback handling, mirroring the pattern Google, Linear, and
//! Atlassian all assume. State entries are single-use + 10-minute
//! TTL so a leaked URL has a tight window of usefulness.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::{info, warn};

use super::manager::McpServerManager;

/// Application state mounted under `/api/auth`. Holds a closure
/// that resolves an agent_id back to its [`McpServerManager`] —
/// closure rather than direct `HashMap<&str, Arc<...>>` so the
/// owner (the agent registry) can keep adding agents at runtime.
#[derive(Clone)]
pub struct CallbackState {
    pub managers: Arc<dyn ManagerLookup>,
    pub state_store: Arc<dyn workforce_oauth_client::OAuthStateStore>,
}

/// Resolution boundary between the callback handler and the per-agent
/// `McpServerManager`. Returning `None` means "no manager for this
/// agent" — the callback responds 404.
pub trait ManagerLookup: Send + Sync {
    fn manager_for(&self, agent_id: &str) -> Option<Arc<McpServerManager>>;
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub state: Option<String>,
    pub code: Option<String>,
    /// Vendors include `error` + `error_description` when consent is denied.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Axum handler. Mounted as
/// `GET /api/auth/{provider}/callback` from `serve.rs`.
pub async fn handle(
    Path(provider): Path<String>,
    Query(q): Query<CallbackQuery>,
    State(state): State<CallbackState>,
) -> Response {
    if let Some(err) = q.error {
        let desc = q.error_description.unwrap_or_default();
        warn!(provider = %provider, error = %err, description = %desc, "OAuth callback returned vendor error");
        return error_html(
            StatusCode::BAD_REQUEST,
            &format!("Authorisation declined ({err}). {desc}"),
        );
    }
    let (Some(state_token), Some(code)) = (q.state, q.code) else {
        return error_html(
            StatusCode::BAD_REQUEST,
            "Missing `state` or `code` query parameters",
        );
    };

    // Consume the state exactly once. `entry.workspace` carries the
    // agent_id so we can dispatch to the correct per-agent manager
    // even when several share one hub. The manager's
    // `complete_with_entry` takes the entry directly so we don't need
    // to re-insert it (avoiding both the TTL-reset corner case and
    // the brief race window of a take-then-reinsert dance).
    let entry = match state.state_store.take(&state_token) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return error_html(
                StatusCode::GONE,
                "OAuth state has expired or was already used. Please restart the install.",
            );
        }
        Err(e) => {
            warn!(error = %e, "OAuth state store lookup failed");
            return error_html(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OAuth state lookup failed",
            );
        }
    };
    let agent_id = entry.workspace.clone();
    let Some(manager) = state.managers.manager_for(&agent_id) else {
        return error_html(
            StatusCode::NOT_FOUND,
            &format!("No active agent for `{agent_id}` — install was orphaned"),
        );
    };

    match manager.complete_with_entry(entry, &code).await {
        Ok(server_id) => {
            info!(
                provider = %provider,
                agent = %agent_id,
                server = %server_id,
                "MCP server authorised via OAuth callback"
            );
            success_html(&server_id)
        }
        Err(e) => {
            warn!(provider = %provider, agent = %agent_id, error = %e, "OAuth callback exchange failed");
            error_html(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("OAuth exchange failed: {e}"),
            )
        }
    }
}

fn success_html(server_id: &str) -> Response {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorised</title>\
         <style>body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; \
         margin: 4rem auto; max-width: 32rem; padding: 0 1rem; color: #111; }}</style>\
         </head><body><h1>✅ Authorised</h1>\
         <p>MCP server <code>{}</code> is now connected. You can close this window and return to your chat.</p>\
         </body></html>",
        html_escape(server_id)
    );
    response_html(StatusCode::OK, body)
}

fn error_html(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorisation failed</title>\
         <style>body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; \
         margin: 4rem auto; max-width: 32rem; padding: 0 1rem; color: #111; }} \
         .err {{ color: #b00; }}</style>\
         </head><body><h1 class=\"err\">⚠️ Authorisation failed</h1>\
         <p>{}</p></body></html>",
        html_escape(message)
    );
    response_html(status, body)
}

fn response_html(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

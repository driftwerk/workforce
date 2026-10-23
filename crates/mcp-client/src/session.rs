//! High-level `McpClient` — what callers actually use.
//!
//! Owns the session id and protocol version negotiated at
//! `initialize`, the JSON-RPC id generator, and the configured
//! schema-trust limits. Stateless across calls in every other
//! respect: each `tools_list` / `tools_call` builds its own
//! request fresh.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use crate::auth::AuthProvider;
use crate::error::McpError;
use crate::jsonrpc::{method, Notification, Request, RequestIdGen, Response};
use crate::limits::McpLimits;
use crate::transport::{PostOutcome, Transport};
use crate::types::{
    ClientCapabilities, ClientInfo, InitializeResult, McpToolDef, ToolNameInfo, ToolNameSurvey,
    ToolsListResult, PROTOCOL_VERSION,
};

/// JSON-RPC error code used by Streamable-HTTP MCP for "session
/// expired or invalid". Distinct from the spec's `-32601` (method
/// not found); some servers also surface session expiry via HTTP
/// 404, which `Transport::post_request` already handles.
const SESSION_EXPIRED_CODE: i32 = -32001;

pub struct McpClient {
    transport: Transport,
    id_gen: RequestIdGen,
    state: Mutex<SessionState>,
    limits: McpLimits,
}

#[derive(Default)]
struct SessionState {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl McpClient {
    pub fn new(
        endpoint: Url,
        http: Arc<reqwest::Client>,
        auth: Arc<dyn AuthProvider>,
        limits: McpLimits,
    ) -> Self {
        Self {
            transport: Transport::new(endpoint, http, auth, limits.max_content_bytes),
            id_gen: RequestIdGen::default(),
            state: Mutex::new(SessionState::default()),
            limits,
        }
    }

    /// Negotiate protocol version + session id with the server.
    /// Sends `notifications/initialized` immediately on success
    /// (some servers reject the next request without it). Idempotent:
    /// calling again after a 404 / session expiry refreshes the
    /// stored session and protocol version.
    pub async fn initialize(&self) -> Result<InitializeResult, McpError> {
        let id = self.id_gen.next();
        let req = Request::new(
            id,
            method::INITIALIZE,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": ClientCapabilities::default(),
                "clientInfo": ClientInfo::workforce(),
            }),
        );
        let outcome = self.transport.post_request(&req, None, None, id).await?;
        let (response, session_id) = match outcome {
            PostOutcome::Response {
                response,
                session_id,
            } => (response, session_id),
            PostOutcome::SessionExpired => {
                return Err(McpError::Protocol(
                    "server returned 404 on initialize — endpoint URL likely wrong".into(),
                ));
            }
        };
        let result = parse_result::<InitializeResult>(response, "initialize")?;
        {
            let mut state = self.state.lock().await;
            state.session_id = session_id;
            state.protocol_version = Some(result.protocol_version.clone());
        }
        info!(
            protocol_version = %result.protocol_version,
            server = ?result.server_info.as_ref().map(|i| &i.name),
            "MCP initialize succeeded"
        );
        self.send_notification(method::NOTIFICATIONS_INITIALIZED, json!({}))
            .await?;
        Ok(result)
    }

    /// Page through `tools/list`, validating each tool against
    /// [`McpLimits`]. Returns the full list once all pages are
    /// drained; rejects the whole call if any tool violates the
    /// limits (an all-or-nothing trust posture is easier to reason
    /// about than partial registration).
    pub async fn tools_list(&self) -> Result<Vec<McpToolDef>, McpError> {
        self.tools_list_filtered(|_| true).await
    }

    /// Page through `tools/list`, keeping only tools for which `keep`
    /// returns true, and validating each *kept* tool against
    /// [`McpLimits`]. The registration cap counts only kept tools, so a
    /// server far larger than the cap still registers cleanly once a
    /// `keep` filter (an allowlist) reduces the set below it — this is
    /// what lets an over-cap server connect at all. A separate
    /// `max_raw_tools` bound caps the raw tools paged through, so a
    /// filter that discards most tools can't be exploited to page
    /// forever. Rejects the whole call if any kept tool violates the
    /// limits (all-or-nothing trust posture).
    pub async fn tools_list_filtered(
        &self,
        keep: impl Fn(&str) -> bool,
    ) -> Result<Vec<McpToolDef>, McpError> {
        let mut accumulated: Vec<McpToolDef> = Vec::new();
        let mut raw_seen: usize = 0;
        let mut cursor: Option<String> = None;
        loop {
            let id = self.id_gen.next();
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let req = Request::new(id, method::TOOLS_LIST, params);
            let response = self.send_request(req, id).await?;
            let page = parse_result::<ToolsListResult>(response, method::TOOLS_LIST)?;
            for tool in page.tools {
                raw_seen += 1;
                self.limits.check_raw_count(raw_seen)?;
                if !keep(&tool.name) {
                    continue;
                }
                self.limits.check_name(&tool.name)?;
                self.limits
                    .check_description(&tool.name, &tool.description)?;
                self.limits
                    .check_input_schema(&tool.name, &tool.input_schema)?;
                accumulated.push(tool);
                self.limits.check_tool_count(accumulated.len())?;
            }
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(accumulated)
    }

    /// Lightweight discovery probe: page through `tools/list` and return
    /// every tool's name + description, dropping the input schemas. Does
    /// NOT apply the registration cap — its job is to let a caller review
    /// a server's surface (and curate an allowlist) even when the server
    /// is far too large to register. Paging still stops at
    /// `max_raw_tools`; when it does, [`ToolNameSurvey::truncated`] is set
    /// so the caller knows the list is a floor, not the whole surface.
    pub async fn list_tool_names(&self) -> Result<ToolNameSurvey, McpError> {
        let mut names: Vec<ToolNameInfo> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut truncated = false;
        'paging: loop {
            let id = self.id_gen.next();
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let req = Request::new(id, method::TOOLS_LIST, params);
            let response = self.send_request(req, id).await?;
            let page = parse_result::<ToolsListResult>(response, method::TOOLS_LIST)?;
            for tool in page.tools {
                if names.len() >= self.limits.max_raw_tools {
                    truncated = true;
                    break 'paging;
                }
                self.limits.check_name(&tool.name)?;
                names.push(ToolNameInfo {
                    name: tool.name,
                    description: tool.description,
                });
            }
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(ToolNameSurvey { names, truncated })
    }

    /// Invoke a tool. The `cancel` token, when triggered before the
    /// HTTP response arrives, aborts the in-flight request and
    /// fire-and-forget-sends `notifications/cancelled` so the
    /// upstream can stop work on its side.
    pub async fn tools_call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, McpError> {
        let id = self.id_gen.next();
        let req = Request::new(
            id,
            method::TOOLS_CALL,
            json!({"name": name, "arguments": arguments}),
        );

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.send_cancellation(id).await;
                Err(McpError::Cancelled)
            }
            result = self.send_request(req, id) => {
                let response = result?;
                parse_result::<Value>(response, method::TOOLS_CALL)
            }
        }
    }

    /// Send a request, transparently re-initializing once if the
    /// server says our session id is stale (HTTP 404 or JSON-RPC
    /// `-32001`). One retry only — if re-init also fails, propagate.
    async fn send_request<P: serde::Serialize>(
        &self,
        req: Request<'_, P>,
        request_id: u64,
    ) -> Result<Response, McpError> {
        let (sid, pv) = self.snapshot_state().await;
        match self
            .transport
            .post_request(&req, sid.as_deref(), pv.as_deref(), request_id)
            .await?
        {
            PostOutcome::Response {
                response,
                session_id,
            } => {
                if let Some(sid) = session_id {
                    self.state.lock().await.session_id = Some(sid);
                }
                if response_is_session_expired(&response) {
                    return self.retry_after_reinit(req, request_id).await;
                }
                Ok(response)
            }
            PostOutcome::SessionExpired => self.retry_after_reinit(req, request_id).await,
        }
    }

    async fn retry_after_reinit<P: serde::Serialize>(
        &self,
        req: Request<'_, P>,
        request_id: u64,
    ) -> Result<Response, McpError> {
        warn!("MCP session expired; re-initializing and retrying once");
        self.initialize().await?;
        let (sid, pv) = self.snapshot_state().await;
        match self
            .transport
            .post_request(&req, sid.as_deref(), pv.as_deref(), request_id)
            .await?
        {
            PostOutcome::Response {
                response,
                session_id,
            } => {
                if let Some(sid) = session_id {
                    self.state.lock().await.session_id = Some(sid);
                }
                Ok(response)
            }
            PostOutcome::SessionExpired => Err(McpError::Protocol(
                "session expired immediately after re-initialize".into(),
            )),
        }
    }

    async fn send_notification<P: serde::Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<(), McpError> {
        let (sid, pv) = self.snapshot_state().await;
        let n = Notification::new(method, params);
        self.transport
            .post_notification(&n, sid.as_deref(), pv.as_deref())
            .await
    }

    /// Best-effort cancellation — failures here are logged but never
    /// surfaced. The caller already saw `McpError::Cancelled`.
    async fn send_cancellation(&self, request_id: u64) {
        let (sid, pv) = self.snapshot_state().await;
        let n = Notification::new(
            method::NOTIFICATIONS_CANCELLED,
            json!({"requestId": request_id, "reason": "client_cancelled"}),
        );
        if let Err(e) = self
            .transport
            .post_notification(&n, sid.as_deref(), pv.as_deref())
            .await
        {
            debug!(error = %e, request_id, "cancellation notification failed");
        }
    }

    async fn snapshot_state(&self) -> (Option<String>, Option<String>) {
        let s = self.state.lock().await;
        (s.session_id.clone(), s.protocol_version.clone())
    }
}

fn parse_result<T: serde::de::DeserializeOwned>(
    response: Response,
    method: &str,
) -> Result<T, McpError> {
    if let Some(err) = response.error {
        return Err(McpError::ServerError {
            code: err.code,
            message: err.message,
            data: err.data,
        });
    }
    let result = response.result.ok_or_else(|| {
        McpError::Protocol(format!("response to {method} had neither result nor error"))
    })?;
    serde_json::from_value(result)
        .map_err(|e| McpError::Protocol(format!("malformed result for {method}: {e}")))
}

fn response_is_session_expired(response: &Response) -> bool {
    response
        .error
        .as_ref()
        .map(|e| e.code == SESSION_EXPIRED_CODE)
        .unwrap_or(false)
}

//! MCP (Model Context Protocol) — JSON-RPC 2.0 handlers and transports.
//!
//! Every CLI the hub spawns is answered in process by an [`McpEndpoint`]: over
//! a loopback HTTP listener for a round on this host, over the runner tunnel
//! for a round on the remote worker. The `mcp-serve` operator subcommand
//! serves the same handlers over stdio or HTTP for clients outside the hub.
//!
//! This module provides:
//!   * Pure JSON-RPC handlers (`handle_initialize`, `handle_tools_list`,
//!     `handle_tools_call`) that operate on a [`Surface`], with the tool run
//!     through a [`ToolCallExecutor`] so the operator helper (direct) and the
//!     hub's rounds and remote MCP (gated, [`crate::mcp_gate`]) share one
//!     dispatch.
//!   * `run_stdio_server` / `run_http_server` — the `mcp-serve` transports.
//!   * `build_mcp_registry` — pattern-based tool filtering.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};

use workforce_agent::security::secret_egress::redact_text;
use workforce_agent::tools::loading::{unwrap_call, CALL_TOOL};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolDefinition, ToolResult};

/// Protocol revisions this server speaks; a client asking for one of them is
/// answered in kind, anything else gets the latest.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

fn negotiated_protocol_version(requested: Option<&str>) -> &'static str {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|v| Some(*v) == requested)
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

/// One item of a tool result as the client receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl McpContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// What a served `tools/call` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpReply {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

impl McpReply {
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: vec![McpContent::text(text)],
            is_error,
        }
    }

    /// A tool's result as text: its error, or its output — a string as is,
    /// anything else as pretty JSON.
    pub fn from_result(result: ToolResult) -> Self {
        match result.success {
            false => Self::text(
                result
                    .error
                    .unwrap_or_else(|| "Tool execution failed".into()),
                true,
            ),
            true => Self::text(result_text(result.output), false),
        }
    }

    /// The reply as a tool result for a loop that reads text: its text
    /// items, with each image named in its place.
    pub fn into_result(self) -> ToolResult {
        let text = self
            .content
            .into_iter()
            .map(|item| match item {
                McpContent::Text { text } => text,
                McpContent::Image { mime_type, .. } => {
                    format!("[a {mime_type} image, which this loop cannot show]")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        match self.is_error {
            true => ToolResult::failure(text, 0),
            false => ToolResult::success(serde_json::Value::String(text), 0),
        }
    }

    /// Redact any cleartext credential left in the reply's text: the last
    /// check before a result reaches a model no provider wrapper sanitises.
    fn redacted(mut self) -> Self {
        for item in &mut self.content {
            if let McpContent::Text { text } = item {
                redact_text(text, "mcp_result");
            }
        }
        self
    }
}

/// A tool output as the text a client reads.
pub fn result_text(output: serde_json::Value) -> String {
    match output {
        serde_json::Value::String(s) => s,
        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
    }
}

/// How a served `tools/call` runs the tool it resolved. The operator helper
/// runs it directly; the hub's rounds and remote MCP gate every call as the
/// principal they serve.
#[async_trait::async_trait]
pub trait ToolCallExecutor: Send + Sync {
    async fn call(
        &self,
        tool: Arc<dyn Tool>,
        name: &str,
        input: serde_json::Value,
    ) -> eyre::Result<McpReply>;
}

pub struct DirectExecutor;

#[async_trait::async_trait]
impl ToolCallExecutor for DirectExecutor {
    async fn call(
        &self,
        tool: Arc<dyn Tool>,
        _name: &str,
        input: serde_json::Value,
    ) -> eyre::Result<McpReply> {
        tool.execute(input).await.map(McpReply::from_result)
    }
}

/// The tools an endpoint serves: a registry fixed when the endpoint was built,
/// or a live view of one, read afresh on every request so a tool registered
/// mid-round (`system.enable_skill`) is reachable without a restart.
#[derive(Clone)]
pub enum Surface {
    Fixed(Arc<ToolRegistry>),
    Live {
        source: Arc<ToolRegistry>,
        admits: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    },
}

impl Surface {
    /// The tools served right now.
    pub fn now(&self) -> Arc<ToolRegistry> {
        match self {
            Self::Fixed(registry) => Arc::clone(registry),
            Self::Live { source, admits } => {
                let now = ToolRegistry::new();
                source
                    .names()
                    .into_iter()
                    .filter(|name| admits(name))
                    .filter_map(|name| source.get(&name))
                    .for_each(|tool| now.register(tool));
                Arc::new(now)
            }
        }
    }

    /// What `tools/list` declares: the tools served now, and on a live
    /// surface the [`CALL_TOOL`] that reaches a tool the client's list
    /// predates. The HTTP transport cannot push `list_changed`.
    fn definitions(&self) -> Vec<ToolDefinition> {
        let call = matches!(self, Self::Live { .. }).then(call_tool_definition);
        self.now()
            .mcp_definitions()
            .into_iter()
            .chain(call)
            .collect()
    }

    fn names(&self) -> Vec<String> {
        let call = matches!(self, Self::Live { .. }).then(|| CALL_TOOL.to_string());
        self.now().names().into_iter().chain(call).collect()
    }
}

fn call_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CALL_TOOL.to_string(),
        description: "Call a tool that an earlier result in this session made available: \
                      its exact name, and arguments matching the definition that result \
                      returned."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact name of the tool."
                },
                "arguments": {
                    "type": "object",
                    "description": "The tool's arguments, matching its definition."
                }
            },
            "required": ["name", "arguments"]
        }),
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INTERNAL_ERROR: i32 = -32603;

#[derive(Debug, Serialize)]
struct McpToolSchema {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ToolsCallParams {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

fn handle_initialize(
    server_name: &str,
    instructions: Option<&str>,
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
) -> JsonRpcResponse {
    info!(server = server_name, "MCP initialize");
    let mut result = serde_json::json!({
        "protocolVersion": negotiated_protocol_version(params["protocolVersion"].as_str()),
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": server_name,
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    // MCP clients (Claude Code included) inject server `instructions` into
    // the model's context: which tools are served and which exposed
    // namespaces have none on this install — without it the model sees the
    // config's optimistic namespace list and confabulates ("jira is
    // available but not loaded").
    if let Some(text) = instructions {
        result["instructions"] = serde_json::Value::String(text.to_string());
    }
    JsonRpcResponse::success(id, result)
}

fn handle_tools_list(tools: &Surface, id: Option<serde_json::Value>) -> JsonRpcResponse {
    // Downstream is Claude Code → Anthropic, which 400s on top-level
    // `oneOf` / `allOf` / `anyOf` — `mcp_definitions()` folds them.
    let schemas: Vec<McpToolSchema> = tools
        .definitions()
        .into_iter()
        .map(|d| McpToolSchema {
            // Claude Code's --allowedTools uses `mcp__<server>__<name>` with
            // underscore-only identifiers. Normalise dots → underscores so the
            // names Claude's model emits (e.g. `mcp__workforce__code_git`) map
            // cleanly to what we serve.
            name: normalise_tool_name(&d.name),
            description: d.description,
            input_schema: d.input_schema,
        })
        .collect();

    info!(count = schemas.len(), "MCP tools/list");
    JsonRpcResponse::success(id, serde_json::json!({ "tools": schemas }))
}

/// The tool a `tools/call` names on `tools` and the input it runs with: a
/// [`CALL_TOOL`] call on a live surface is unwrapped first, and every name
/// goes through the resolution the agent runner applies to a model-emitted
/// one, so a client that flattens an action-dispatch tool into
/// `notes_write_human` reaches `notes` here exactly as it would in-process.
fn resolve_call(
    tools: &Surface,
    call: &ToolsCallParams,
) -> Result<(Arc<dyn Tool>, String, serde_json::Value), String> {
    let registry = tools.now();
    let unwrapped = matches!(tools, Surface::Live { .. })
        .then(|| unwrap_call(&call.name, &call.arguments))
        .flatten()
        .transpose()?;
    let (name, arguments) = unwrapped.map_or_else(
        || (call.name.clone(), call.arguments.clone()),
        |unwrapped| (unwrapped.tool, unwrapped.arguments),
    );
    let resolved = workforce_agent::runner::resolve_llm_tool_call(&name, &arguments, &registry);
    let tool = registry
        .get(&resolved.name)
        .ok_or_else(|| workforce_agent::runner::tool_not_found_message(&name, &registry))?;
    if let Some(action) = &resolved.repaired_action {
        warn!(
            mcp_tool = %name,
            tool = %resolved.name,
            action = %action,
            "repaired flattened action-dispatch tool call"
        );
    }
    Ok((tool, resolved.name, resolved.input.into_owned()))
}

/// Why a `tools/call` came back without a tool result.
enum Unanswered {
    NotFound(String),
    Failed(String),
}

/// Resolve `call` on `tools` and run it through `executor`: the one path
/// every served call takes, over a transport or in process.
async fn run_call(
    tools: &Surface,
    executor: &dyn ToolCallExecutor,
    call: &ToolsCallParams,
) -> Result<McpReply, Unanswered> {
    let (tool, name, input) = resolve_call(tools, call).map_err(Unanswered::NotFound)?;
    info!(tool = %name, "MCP tools/call");
    executor
        .call(tool, &name, input)
        .await
        .map(McpReply::redacted)
        .map_err(|e| {
            warn!(tool = %call.name, error = %e, "MCP tool execution error");
            let mut message = format!("Tool error: {e}");
            redact_text(&mut message, "mcp_error");
            Unanswered::Failed(message)
        })
}

async fn handle_tools_call(
    tools: &Surface,
    executor: &dyn ToolCallExecutor,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
) -> JsonRpcResponse {
    let call: ToolsCallParams = match serde_json::from_value(params) {
        Ok(c) => c,
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                INVALID_REQUEST,
                format!("Invalid tools/call params: {e}"),
            );
        }
    };
    match run_call(tools, executor, &call).await {
        Ok(reply) => JsonRpcResponse::success(
            id,
            serde_json::to_value(reply).unwrap_or_else(|e| {
                serde_json::json!({
                    "content": [{"type": "text", "text": format!("Serialization error: {e}")}],
                    "isError": true
                })
            }),
        ),
        Err(Unanswered::NotFound(message)) => JsonRpcResponse::error(id, METHOD_NOT_FOUND, message),
        Err(Unanswered::Failed(message)) => JsonRpcResponse::error(id, INTERNAL_ERROR, message),
    }
}

/// Dispatch a single JSON-RPC request against a tool surface.
/// Used by every transport and by tests.
async fn dispatch(
    tools: &Surface,
    server_name: &str,
    instructions: Option<&str>,
    executor: &dyn ToolCallExecutor,
    request: JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    if request.jsonrpc != "2.0" {
        return Some(JsonRpcResponse::error(
            request.id,
            INVALID_REQUEST,
            "Expected jsonrpc 2.0",
        ));
    }

    match request.method.as_str() {
        "initialize" => Some(handle_initialize(
            server_name,
            instructions,
            request.id,
            &request.params,
        )),
        "ping" => Some(JsonRpcResponse::success(request.id, serde_json::json!({}))),
        method if method.starts_with("notifications/") && request.id.is_none() => None,
        "tools/list" => Some(handle_tools_list(tools, request.id)),
        "tools/call" => Some(handle_tools_call(tools, executor, request.id, request.params).await),
        other => Some(JsonRpcResponse::error(
            request.id,
            METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        )),
    }
}

/// Run the MCP server over stdin/stdout. Each line on stdin is a JSON-RPC
/// request; each response is written as a single line on stdout.
///
/// `instructions`, when set, is surfaced in the `initialize` response —
/// MCP clients inject it into the model's context (skill availability,
/// failure reasons).
///
/// Exits cleanly on EOF. Logs to stderr (never stdout — that belongs to the
/// JSON-RPC protocol).
pub async fn run_stdio_server(
    tools: Arc<ToolRegistry>,
    server_name: String,
    instructions: Option<String>,
) -> eyre::Result<()> {
    let endpoint = McpEndpoint::new(
        Surface::Fixed(tools),
        server_name,
        instructions,
        Arc::new(DirectExecutor),
    );
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    info!(
        server = %endpoint.server_name,
        tools = endpoint.tools.now().len(),
        "MCP stdio server started"
    );

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = endpoint.handle(trimmed).await {
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    info!("MCP stdio server shutting down (stdin closed)");
    Ok(())
}

/// The key a client's `mcpServers` config names the hub under, and so the
/// prefix Claude Code gives every tool served through it.
pub const MCP_SERVER_KEY: &str = "workforce";

/// One MCP surface answered in-process: the tools it serves, how each call
/// runs, and what a client is told on `initialize`.
///
/// One hub serves many sessions, so whose call it is belongs to the executor
/// rather than the process. Every transport — stdio, the loopback HTTP
/// listener, the tunnel from a remote worker — answers through
/// [`McpEndpoint::handle`], so they cannot answer the same request
/// differently.
#[derive(Clone)]
pub struct McpEndpoint {
    tools: Surface,
    server_name: String,
    instructions: Option<String>,
    executor: Arc<dyn ToolCallExecutor>,
}

impl McpEndpoint {
    /// An endpoint serving `tools`, every call run through `executor`: the
    /// gate a call must pass, or [`DirectExecutor`] for a surface that needs
    /// none.
    pub fn new(
        tools: Surface,
        server_name: String,
        instructions: Option<String>,
        executor: Arc<dyn ToolCallExecutor>,
    ) -> Self {
        Self {
            tools,
            server_name,
            instructions,
            executor,
        }
    }

    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// The `--allowedTools` names Claude Code knows this surface's tools by.
    pub fn allowed_tools(&self) -> Vec<String> {
        self.tools
            .names()
            .into_iter()
            .map(|name| format!("mcp__{MCP_SERVER_KEY}__{}", normalise_tool_name(&name)))
            .collect()
    }

    /// Answer one JSON-RPC body, or nothing for a notification. A body that is
    /// not a request is answered with a parse error rather than dropped, so the
    /// client learns what it sent.
    pub async fn handle(&self, body: &str) -> Option<String> {
        let request: JsonRpcRequest = match serde_json::from_str(body) {
            Ok(request) => request,
            Err(e) => {
                warn!(error = %e, "Invalid JSON-RPC input");
                let err =
                    JsonRpcResponse::error(None, INVALID_REQUEST, format!("Parse error: {e}"));
                return serde_json::to_string(&err).ok();
            }
        };
        dispatch(
            &self.tools,
            &self.server_name,
            self.instructions.as_deref(),
            self.executor.as_ref(),
            request,
        )
        .await
        .and_then(|response| serde_json::to_string(&response).ok())
    }

    /// Run `name` with `arguments` in process, exactly as a served
    /// `tools/call` for it would run; a call that could not be answered is
    /// an error reply.
    pub async fn call(&self, name: &str, arguments: serde_json::Value) -> McpReply {
        let call = ToolsCallParams {
            name: name.to_string(),
            arguments,
        };
        run_call(&self.tools, self.executor.as_ref(), &call)
            .await
            .unwrap_or_else(
                |(Unanswered::NotFound(message) | Unanswered::Failed(message))| {
                    McpReply::text(message, true)
                },
            )
    }

    /// Whether `body` asks to run a tool, the one request that acts.
    pub fn is_tool_call(body: &str) -> bool {
        serde_json::from_str::<JsonRpcRequest>(body)
            .is_ok_and(|request| request.method == "tools/call")
    }

    /// A JSON-RPC error answering `body` with `message`, run nowhere.
    pub fn refusal(body: &str, message: &str) -> String {
        let id = serde_json::from_str::<JsonRpcRequest>(body)
            .ok()
            .and_then(|request| request.id);
        serde_json::to_string(&JsonRpcResponse::error(id, INTERNAL_ERROR, message))
            .unwrap_or_default()
    }
}

/// Serve MCP over HTTP on an already-bound listener, authenticating every
/// request against `token`. The listener is bound by the caller, which is what
/// lets the hub take an ephemeral port and know which one it got before any
/// client is told where to connect.
pub async fn run_http_server(
    tools: Arc<ToolRegistry>,
    server_name: String,
    instructions: Option<String>,
    listener: tokio::net::TcpListener,
    token: String,
) -> eyre::Result<()> {
    serve_http(
        McpEndpoint::new(
            Surface::Fixed(tools),
            server_name,
            instructions,
            Arc::new(DirectExecutor),
        ),
        listener,
        token,
    )
    .await
}

/// Serve `endpoint` over HTTP on an already-bound listener.
pub async fn serve_http(
    endpoint: McpEndpoint,
    listener: tokio::net::TcpListener,
    token: String,
) -> eyre::Result<()> {
    info!(server = %endpoint.server_name, "MCP http server started");
    let state = Arc::new(HttpState { endpoint, token });
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(handle_http_rpc))
        .with_state(state);
    axum::serve(listener, app).await?;
    Ok(())
}

struct HttpState {
    endpoint: McpEndpoint,
    token: String,
}

/// One JSON-RPC request per POST. A notification (no response from `dispatch`)
/// answers 202 with an empty body, as the protocol expects.
async fn handle_http_rpc(
    axum::extract::State(state): axum::extract::State<Arc<HttpState>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    if !bearer_matches(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.endpoint.handle(&body).await {
        Some(response) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            response,
        )
            .into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Whether the request carries exactly this server's bearer token. Compared in
/// constant time: the token is the only thing standing between one session's
/// port and another session's tools.
fn bearer_matches(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| crate::auth::constant_time_token_eq(presented, expected))
}

/// Build a curated MCP tool registry from a source registry, filtering by
/// patterns. Patterns support trailing `*` for prefix matching
/// (e.g. `"knowledge.*"` matches `"knowledge.ask"`). Empty patterns clones all.
pub fn build_mcp_registry(source: &ToolRegistry, patterns: &[String]) -> ToolRegistry {
    if patterns.is_empty() {
        return source.clone_all();
    }

    let registry = ToolRegistry::new();
    for name in source.names() {
        if matches_any(&name, patterns) {
            if let Some(tool) = source.get(&name) {
                registry.register(tool);
            }
        }
    }
    registry
}

/// Normalise a tool name for MCP wire format. Claude Code requires tool names
/// matchable by its `mcp__<server>__<identifier>` convention, which uses only
/// `[a-zA-Z0-9_]`. Tools in the workforce registry use dot-separated names
/// (`code.git`); we expose them as `code_git` over MCP.
pub(crate) fn normalise_tool_name(name: &str) -> String {
    name.replace('.', "_")
}

/// What a client is told about the surface it just connected to.
///
/// Built from the registry actually being served, so it cannot claim a tool the
/// client will not find. A pattern that admitted nothing is named: that signal
/// is what let Claude Code say `wasm_skill — unavailable` rather than silently
/// behaving as though the namespace did not exist, and it stays worth having
/// for a pattern that is simply misspelled.
pub fn serving_instructions(tools: &ToolRegistry, patterns: &[String]) -> String {
    let served = tools.names();
    let unmatched: Vec<&str> = patterns
        .iter()
        .filter(|p| !served.iter().any(|name| matches_any(name, &[(*p).clone()])))
        .map(String::as_str)
        .collect();
    let head = format!(
        "Workforce tools ({} available). Tool schemas may be deferred — load them via ToolSearch before calling.",
        served.len()
    );
    match unmatched.is_empty() {
        true => head,
        false => format!(
            "{head} No tools are served for: {} — unavailable in this session.",
            unmatched.join(", ")
        ),
    }
}

/// Match `name` against exact patterns or `prefix.*` globs. Empty
/// `patterns` matches nothing — callers that mean "no filter" must branch
/// on `patterns.is_empty()` themselves.
pub fn matches_any(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == p
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(tools: &ToolRegistry) -> Surface {
        Surface::Fixed(Arc::new(tools.clone_all()))
    }

    fn auth_header(value: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            value.parse().expect("header value"),
        );
        headers
    }

    /// The transport end to end: a real request over a real socket returns the
    /// same `tools/list` the stdio path serves, and an unauthenticated one is
    /// refused before reaching dispatch.
    #[tokio::test]
    async fn the_http_transport_serves_tools_and_refuses_bad_tokens() {
        let registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(EchoTool));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run_http_server(
            std::sync::Arc::new(registry),
            "workforce-test".to_string(),
            None,
            listener,
            "right-token".to_string(),
        ));

        let url = format!("http://127.0.0.1:{port}/mcp");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let client = reqwest::Client::new();

        let ok = client
            .post(&url)
            .header("Authorization", "Bearer right-token")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let parsed: serde_json::Value = ok.json().await.unwrap();
        let names: Vec<String> = parsed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["test_echo"]);

        for header in ["Bearer wrong-token", "Bearer ", "right-token"] {
            let refused = client
                .post(&url)
                .header("Authorization", header)
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), 401, "header {header:?} must be refused");
        }

        let anonymous = client.post(&url).body(body).send().await.unwrap();
        assert_eq!(anonymous.status(), 401);
    }

    /// Claude Code, even sandboxed as the agent uid, writes through the hub's
    /// own file tools over MCP, which run as the hub: the refusal has to hold
    /// on that path as it does in a turn.
    #[tokio::test]
    async fn the_agents_configuration_is_refused_over_mcp() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::write(root.join("config.toml"), "[access]\nmode = \"allowlist\"\n").unwrap();
        let hub = workforce_agent::runner::default_tools(vec![root.clone()], Default::default());
        let registry = build_mcp_registry(&hub, &["filesystem.*".to_string()]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run_http_server(
            std::sync::Arc::new(registry),
            "workforce-test".to_string(),
            None,
            listener,
            "token".to_string(),
        ));

        let call = |path: &str| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "filesystem_write",
                    "arguments": { "path": path, "content": "[access]\nmode = \"open\"\n" },
                },
            })
        };
        let client = reqwest::Client::new();
        let send = |body: serde_json::Value| {
            client
                .post(format!("http://127.0.0.1:{port}/mcp"))
                .header("Authorization", "Bearer token")
                .json(&body)
                .send()
        };

        let refused: serde_json::Value = send(call("config.toml"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert!(refused
            .to_string()
            .contains(workforce_common::config_files::WRITE_REFUSED));
        assert!(std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("allowlist"));

        let own: serde_json::Value = send(call("notes.md")).await.unwrap().json().await.unwrap();
        assert_ne!(own["result"]["isError"], true, "{own}");
        assert!(root.join("notes.md").is_file());
    }

    /// A notification carries no id and expects no body.
    #[tokio::test]
    async fn a_notification_is_accepted_without_a_response_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run_http_server(
            std::sync::Arc::new(ToolRegistry::new()),
            "workforce-test".to_string(),
            None,
            listener,
            "t".to_string(),
        ));
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("Authorization", "Bearer t")
            .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        assert!(resp.bytes().await.unwrap().is_empty());
    }

    #[test]
    fn instructions_name_a_pattern_that_admitted_nothing() {
        let tools = ToolRegistry::new();
        tools.register(std::sync::Arc::new(EchoTool));
        let text =
            serving_instructions(&tools, &["test.*".to_string(), "wasm_skill.*".to_string()]);
        assert!(text.contains("wasm_skill.*"), "{text}");
        assert!(text.contains("unavailable"), "{text}");
        assert!(
            !text.contains("test.*"),
            "a matched pattern must not be named"
        );
    }

    #[test]
    fn instructions_stay_quiet_when_everything_matched() {
        let tools = ToolRegistry::new();
        tools.register(std::sync::Arc::new(EchoTool));
        let text = serving_instructions(&tools, &["test.*".to_string()]);
        assert!(!text.contains("unavailable"), "{text}");
        assert!(text.contains("1 available"), "{text}");
    }

    #[test]
    fn only_the_exact_bearer_is_accepted() {
        assert!(bearer_matches(&auth_header("Bearer right"), "right"));
        assert!(!bearer_matches(&auth_header("Bearer wrong"), "right"));
        assert!(!bearer_matches(&auth_header("Bearer "), "right"));
        assert!(!bearer_matches(&auth_header("right"), "right"));
        assert!(!bearer_matches(&auth_header("Basic right"), "right"));
        assert!(!bearer_matches(&axum::http::HeaderMap::new(), "right"));
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "test.echo"
        }
        fn description(&self) -> &str {
            "Echo the input"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "message": { "type": "string" } }
            })
        }
        async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
            let msg = input
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");
            Ok(ToolResult::success(serde_json::json!({ "echo": msg }), 1))
        }
    }

    /// Mixed dot+underscore name like real tools `code.find_replace`,
    /// `stats.my_usage`. Guards against the dot↔underscore mapping in
    /// name resolution mis-resolving these.
    struct MixedTool;

    #[async_trait::async_trait]
    impl Tool for MixedTool {
        fn name(&self) -> &str {
            "group.mixed_name"
        }
        fn description(&self) -> &str {
            "Mixed separator name"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(serde_json::json!({"ok": true}), 1))
        }
    }

    /// Root-level `oneOf` (same shape as `google.drive.upload`). Guards
    /// the workforce → MCP → Claude Code → Anthropic chain: `tools/list`
    /// must strip it before sending or Anthropic 400s the whole array.
    struct OneOfTool;

    #[async_trait::async_trait]
    impl Tool for OneOfTool {
        fn name(&self) -> &str {
            "test.one_of"
        }
        fn description(&self) -> &str {
            "Tool with root-level oneOf — Anthropic rejects this shape"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": { "type": "string" },
                    "b": { "type": "string" }
                },
                "oneOf": [{ "required": ["a"] }, { "required": ["b"] }]
            })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(serde_json::json!({"ok": true}), 1))
        }
    }

    struct FailTool;

    #[async_trait::async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "test.fail"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::failure("deliberate failure", 1))
        }
    }

    fn test_registry() -> ToolRegistry {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(FailTool));
        registry.register(Arc::new(MixedTool));
        registry
    }

    #[test]
    fn matches_any_exact() {
        let patterns = vec!["knowledge.ask".into(), "vault.get".into()];
        assert!(matches_any("knowledge.ask", &patterns));
        assert!(matches_any("vault.get", &patterns));
        assert!(!matches_any("knowledge.explain", &patterns));
    }

    #[test]
    fn matches_any_prefix_glob() {
        let patterns = vec!["knowledge.*".into()];
        assert!(matches_any("knowledge.ask", &patterns));
        assert!(matches_any("knowledge.explain", &patterns));
        assert!(!matches_any("vault.get", &patterns));
    }

    #[test]
    fn matches_any_empty_patterns() {
        let patterns: Vec<String> = vec![];
        assert!(!matches_any("anything", &patterns));
    }

    #[test]
    fn build_registry_empty_patterns_clones_all() {
        let source = test_registry();
        let filtered = build_mcp_registry(&source, &[]);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn build_registry_filters_by_pattern() {
        let source = test_registry();
        let filtered = build_mcp_registry(&source, &["test.echo".into()]);
        assert_eq!(filtered.len(), 1);
        assert!(filtered.has("test.echo"));
        assert!(!filtered.has("test.fail"));
    }

    #[test]
    fn build_registry_prefix_glob() {
        let source = test_registry();
        let filtered = build_mcp_registry(&source, &["test.*".into()]);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.has("test.echo"));
        assert!(filtered.has("test.fail"));
    }

    #[tokio::test]
    async fn dispatch_initialize() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: "initialize".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "test");
    }

    async fn initialize_with(version: Option<&str>) -> String {
        let params = version
            .map(|v| serde_json::json!({ "protocolVersion": v }))
            .unwrap_or(serde_json::Value::Null);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: "initialize".into(),
            params,
        };
        let resp = dispatch(
            &fixed(&test_registry()),
            "test",
            None,
            &DirectExecutor,
            request,
        )
        .await
        .unwrap();
        resp.result.unwrap()["protocolVersion"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A client is answered in the revision it asked for when we speak it,
    /// and in the latest one otherwise — never in a revision it never named.
    #[tokio::test]
    async fn initialize_negotiates_the_protocol_version() {
        assert_eq!(initialize_with(Some("2024-11-05")).await, "2024-11-05");
        assert_eq!(initialize_with(Some("2025-03-26")).await, "2025-03-26");
        assert_eq!(initialize_with(Some("2025-06-18")).await, "2025-06-18");
        assert_eq!(initialize_with(Some("2099-01-01")).await, "2025-06-18");
        assert_eq!(initialize_with(None).await, "2025-06-18");
    }

    #[tokio::test]
    async fn ping_answers_an_empty_result() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(7)),
            method: "ping".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(
            &fixed(&test_registry()),
            "test",
            None,
            &DirectExecutor,
            request,
        )
        .await
        .unwrap();
        assert_eq!(resp.id, Some(serde_json::json!(7)));
        assert_eq!(resp.result, Some(serde_json::json!({})));
        assert!(resp.error.is_none());
    }

    /// `notifications/cancelled` is a real client message; answering it with
    /// an error envelope (null id) is a protocol violation. A message that
    /// carries an id is a request, whatever its method is called.
    #[tokio::test]
    async fn any_notification_is_silent() {
        for method in ["notifications/cancelled", "notifications/progress"] {
            let request = JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: None,
                method: method.into(),
                params: serde_json::json!({ "requestId": 1 }),
            };
            assert!(
                dispatch(
                    &fixed(&test_registry()),
                    "test",
                    None,
                    &DirectExecutor,
                    request
                )
                .await
                .is_none(),
                "{method} must produce no response"
            );
        }
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(3)),
            method: "notifications/cancelled".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(
            &fixed(&test_registry()),
            "test",
            None,
            &DirectExecutor,
            request,
        )
        .await
        .unwrap();
        assert_eq!(resp.id, Some(serde_json::json!(3)));
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    /// The executor decides how a resolved tool runs; the transport only
    /// resolves the name and shapes the result.
    #[tokio::test]
    async fn tools_call_runs_through_the_executor() {
        struct Stamping;

        #[async_trait::async_trait]
        impl ToolCallExecutor for Stamping {
            async fn call(
                &self,
                _tool: Arc<dyn Tool>,
                name: &str,
                _input: serde_json::Value,
            ) -> eyre::Result<McpReply> {
                Ok(McpReply::from_result(ToolResult::success(
                    serde_json::json!({ "via": name }),
                    0,
                )))
            }
        }

        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".into(),
            params: serde_json::json!({ "name": "test_echo", "arguments": {} }),
        };
        let resp = dispatch(&fixed(&test_registry()), "test", None, &Stamping, request)
            .await
            .unwrap();
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["via"],
            "test.echo"
        );
    }

    /// Regression: a registered tool with root-level `oneOf` must NOT appear
    /// in the `tools/list` response with that key intact. If it does, Claude
    /// Code forwards the schema to Anthropic, which rejects the whole tools
    /// array (`tools.N.custom.input_schema: input_schema does not support
    /// oneOf, allOf, or anyOf at the top level`).
    #[tokio::test]
    async fn tools_list_strips_top_level_constraint_keys() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(OneOfTool));
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(100)),
            method: "tools/list".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&registry), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let tools_list = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools_list.len(), 1);
        let schema = &tools_list[0]["inputSchema"];
        assert!(
            schema.get("oneOf").is_none(),
            "tools/list must not expose top-level oneOf — Anthropic rejects it"
        );
        assert!(schema.get("allOf").is_none());
        assert!(schema.get("anyOf").is_none());
    }

    #[tokio::test]
    async fn dispatch_tools_list() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(2)),
            method: "tools/list".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        let tools_list = result["tools"].as_array().unwrap();
        assert_eq!(tools_list.len(), 3);
    }

    #[tokio::test]
    async fn tools_list_returns_underscore_names() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(20)),
            method: "tools/list".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let tools_list = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools_list
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"test_echo"));
        assert!(names.contains(&"test_fail"));
        assert!(!names.iter().any(|n| n.contains('.')));
    }

    #[tokio::test]
    async fn tools_call_accepts_underscore_alias() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(21)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "test_echo",
                "arguments": { "message": "via underscore" }
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("via underscore"));
    }

    /// An action-dispatch tool flattened into `tool_action` by the client must
    /// resolve over MCP exactly as it does in-process — the two transports
    /// cannot disagree about which names are callable.
    #[tokio::test]
    async fn tools_call_repairs_flattened_action_name() {
        struct ActionTool;

        #[async_trait::async_trait]
        impl Tool for ActionTool {
            fn name(&self) -> &str {
                "notes"
            }
            fn description(&self) -> &str {
                "Action-dispatch notes tool"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "action": { "type": "string", "enum": ["write_human"] } }
                })
            }
            async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::success(
                    serde_json::json!({ "performed": input["action"] }),
                    1,
                ))
            }
        }

        let tools = test_registry();
        tools.register(Arc::new(ActionTool));
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(23)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "notes_write_human",
                "arguments": { "content": "a moment worth keeping" }
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("write_human"));
    }

    /// An unresolvable name still errors — and names what could have been meant.
    #[tokio::test]
    async fn tools_call_unknown_name_suggests_relatives() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(24)),
            method: "tools/call".into(),
            params: serde_json::json!({ "name": "test_invented", "arguments": {} }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let message = resp.error.unwrap().message;
        assert!(message.contains("test_invented"), "{message}");
        assert!(message.contains("test_echo"), "{message}");
    }

    #[tokio::test]
    async fn tools_call_still_accepts_dot_form() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(22)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "test.echo",
                "arguments": { "message": "via dot" }
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("via dot"));
    }

    #[tokio::test]
    async fn tools_call_resolves_mixed_dot_and_underscore_name() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(24)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "group_mixed_name",
                "arguments": {}
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(
            result["isError"], false,
            "wire name `group_mixed_name` must resolve to canonical `group.mixed_name`, \
             not be mis-decoded as `group.mixed.name`"
        );
    }

    #[tokio::test]
    async fn tools_call_unknown_in_both_forms_returns_error() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(23)),
            method: "tools/call".into(),
            params: serde_json::json!({ "name": "nothing_here", "arguments": {} }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_tools_call_success() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(3)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "test.echo",
                "arguments": { "message": "hello" }
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn dispatch_tools_call_failure() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(4)),
            method: "tools/call".into(),
            params: serde_json::json!({
                "name": "test.fail",
                "arguments": {}
            }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[tokio::test]
    async fn dispatch_tools_call_unknown() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(5)),
            method: "tools/call".into(),
            params: serde_json::json!({ "name": "nonexistent", "arguments": {} }),
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_unknown_method() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(6)),
            method: "unknown/method".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_invalid_jsonrpc_version() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "1.0".into(),
            id: Some(serde_json::json!(7)),
            method: "initialize".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request)
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, INVALID_REQUEST);
    }

    #[tokio::test]
    async fn dispatch_notifications_return_none() {
        let tools = test_registry();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "notifications/initialized".into(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch(&fixed(&tools), "test", None, &DirectExecutor, request).await;
        assert!(resp.is_none());
    }

    /// Fails with a credential in its error, by result or by error.
    struct Leaky {
        errs: bool,
    }

    const PAT: &str = concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");

    #[async_trait::async_trait]
    impl Tool for Leaky {
        fn name(&self) -> &str {
            match self.errs {
                true => "leak.error",
                false => "leak.failure",
            }
        }
        fn description(&self) -> &str {
            "leaks"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            match self.errs {
                true => Err(eyre::eyre!("upstream refused {PAT}")),
                false => Ok(ToolResult::failure(format!("bad token {PAT}"), 0)),
            }
        }
    }

    #[tokio::test]
    async fn mcp_error_text_is_redacted() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(Leaky { errs: true }));
        registry.register(Arc::new(Leaky { errs: false }));
        let endpoint = McpEndpoint::new(
            Surface::Fixed(Arc::new(registry)),
            "t".into(),
            None,
            Arc::new(DirectExecutor),
        );
        for tool in ["leak_error", "leak_failure"] {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": {} }
            })
            .to_string();
            let answered = endpoint.handle(&body).await.unwrap();
            assert!(!answered.contains(PAT), "{tool}: {answered}");
            assert!(
                answered.contains("ghp_AB"),
                "{tool} is redacted, not dropped: {answered}"
            );
        }
    }

    /// Every transport answers through the endpoint, so this is the one place
    /// the request/notification contract is pinned.
    #[tokio::test]
    async fn the_endpoint_answers_requests_and_swallows_notifications() {
        let registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(EchoTool));
        let endpoint = McpEndpoint::new(
            Surface::Fixed(std::sync::Arc::new(registry)),
            "workforce-test".to_string(),
            Some("served for a test".to_string()),
            std::sync::Arc::new(DirectExecutor),
        );

        let listed = endpoint
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .expect("a request is answered");
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(listed["result"]["tools"][0]["name"], "test_echo");

        let initialised = endpoint
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#)
            .await
            .unwrap();
        assert!(initialised.contains("served for a test"), "{initialised}");

        assert!(endpoint
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none());

        let garbage = endpoint.handle("not json").await.unwrap();
        assert!(garbage.contains("Parse error"), "{garbage}");

        assert_eq!(
            endpoint.allowed_tools(),
            vec![format!("mcp__{MCP_SERVER_KEY}__test_echo")]
        );
    }
}

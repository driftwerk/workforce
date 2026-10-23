use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP protocol version this client speaks. Sent in the
/// `MCP-Protocol-Version` HTTP header on every request after
/// initialize. Bumped when the spec ships a breaking change.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Result of `initialize` — what the server told us it supports.
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    /// Server-chosen protocol version. May be older than [`PROTOCOL_VERSION`]
    /// if the server is older than us.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default, rename = "serverInfo")]
    pub server_info: Option<ServerInfo>,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Server-advertised capability flags. We only inspect a few; the
/// rest pass through as JSON for forward compatibility.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub resources: Option<Value>,
    #[serde(default)]
    pub prompts: Option<Value>,
    #[serde(default)]
    pub logging: Option<Value>,
}

/// One tool from `tools/list`. Validated against [`crate::McpLimits`]
/// before being surfaced to callers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "default_schema")]
    pub input_schema: Value,
}

fn default_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// A tool's name + description, without its (potentially large) input
/// schema. Used by the lightweight discovery probe
/// [`crate::McpClient::list_tool_names`] so a caller can review what a
/// server offers — even one too large to register — without paying the
/// schema cost or tripping the registration cap.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolNameInfo {
    pub name: String,
    pub description: String,
}

/// Outcome of [`crate::McpClient::list_tool_names`]: the names gathered
/// and whether paging stopped at `max_raw_tools` (so the list is a
/// floor, not the full surface). `truncated` must be surfaced to the
/// caller — a partial list curated against silently reads as complete.
#[derive(Debug, Clone)]
pub struct ToolNameSurvey {
    pub names: Vec<ToolNameInfo>,
    pub truncated: bool,
}

/// Wire-shape for `tools/list` response.
#[derive(Debug, Deserialize)]
pub(crate) struct ToolsListResult {
    pub tools: Vec<McpToolDef>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// Our caller-facing client info, sent on `initialize`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClientInfo {
    pub name: &'static str,
    pub version: &'static str,
}

impl ClientInfo {
    pub fn workforce() -> Self {
        Self {
            name: "workforce-hub",
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Capability flags we advertise on `initialize`. Tools-only for
/// v1; resources/prompts deliberately out of scope (see plan).
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
}

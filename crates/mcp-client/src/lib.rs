//! Streamable-HTTP MCP (Model Context Protocol) client.
//!
//! Implements the consumer side of the spec at
//! <https://modelcontextprotocol.io>: JSON-RPC 2.0 over HTTP POST,
//! `Mcp-Session-Id` lifecycle, `MCP-Protocol-Version` header
//! negotiation, SSE-on-POST response handling, and `notifications/
//! initialized` + `notifications/cancelled` framing.
//!
//! Designed to be vendor-agnostic. The hub layers on top: registry,
//! tool wrapper, OAuth bridge, periodic refresh — all in
//! `crates/hub/src/mcp_servers/` (PR3). This crate has no hub
//! dependencies and is fully exercised against `wiremock`.
//!
//! # Schema-trust limits
//!
//! Tool definitions returned by `tools/list` come from third-party
//! servers. To bound the trust surface this crate enforces
//! [`McpLimits`] (name regex, description size, schema size, tool
//! count, result content cap). Violations are rejected with a
//! structured error rather than silently truncated.
//!
//! # Defence-in-depth redaction
//!
//! Errors from upstream pass through
//! `workforce_common::redact::redact_secrets` before bubbling to
//! callers, so a vendor that echoes the bearer token in an error
//! body never leaks it into our logs. The endpoint appears in errors
//! only as `workforce_common::redact::redact_url` renders it (scheme,
//! host and port); the rest is never shown. The agent runner's egress
//! sanitiser still scrubs LLM-bound content; this is the inner
//! belt-and-braces layer.

mod auth;
mod error;
mod jsonrpc;
mod limits;
mod session;
mod sse;
mod transport;
mod types;

pub use auth::{AuthProvider, NoAuth, StaticBearer};
pub use error::McpError;
pub use limits::{
    McpLimits, DEFAULT_MAX_CONTENT_BYTES, DEFAULT_MAX_DESCRIPTION_BYTES,
    DEFAULT_MAX_INPUT_SCHEMA_BYTES, DEFAULT_MAX_RAW_TOOLS, DEFAULT_MAX_TOOLS_PER_SERVER,
};
pub use session::McpClient;
pub use types::{
    InitializeResult, McpToolDef, ServerCapabilities, ServerInfo, ToolAnnotations, ToolNameInfo,
    ToolNameSurvey, PROTOCOL_VERSION,
};

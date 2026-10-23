//! Hub-side wiring for the [`workforce_mcp_client`] crate.
//!
//! Reads `[mcp.servers.*]` from `workforce.toml`, opens an
//! `McpClient` per entry, registers every upstream tool as
//! `mcp.<server_id>.<tool_name>` in the agent's tool registry,
//! and runs a periodic `tools/list` refresh per server.
//!
//! Failure isolation: one server failing `initialize()` at startup
//! does not block the hub — that server is skipped, others
//! continue, and a `tracing` error captures the cause.

pub mod access;
pub mod api;
mod auth;
pub mod callback;
mod discovery;
pub mod manager;
pub mod manager_tools;
pub mod oauth_bridge;
pub mod presets;
mod refresh;
mod registry;
mod server;
mod shadows;
mod tool;
mod tool_allowlist;

pub use access::McpToolAccess;
pub use auth::{PatAuth, TokenStoreAuth};
pub use manager::{
    AuthorizeStart, InstallReport, McpServerManager, RefreshReport, ServerDetail, UninstallReport,
    DEFAULT_CALLBACK_PATH,
};
pub use manager_tools::register_manager_tools;
pub use presets::SetupStep;
pub use registry::{McpServerRegistry, ServerSummary};
pub use server::{spawn_all, McpServer, ShadowPlan, TokenProbe};
pub use tool::McpTool;

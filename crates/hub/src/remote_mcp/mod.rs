//! Remote MCP: the hub as an MCP server for external clients (Claude Code,
//! `workforce-local-mcp`) acting as a workforce user under a per-user `wfr_`
//! token. Distinct from the session MCP that serves hub-spawned Claude Code
//! (`crate::mcp_session`) and from the upstream MCP client (`crate::mcp_servers`).

pub mod access;
pub mod agent_tools;
pub mod approvals;
pub mod auth;
pub mod command;
pub mod help;
pub mod server;
pub(crate) mod session_context;
pub mod skill_tools;
mod stream;
#[cfg(test)]
pub(crate) mod testing;
pub mod tokens;

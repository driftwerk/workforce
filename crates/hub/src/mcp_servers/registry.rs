//! In-memory registry of live MCP servers. Owned per-`AgentService`
//! and mutated at runtime by [`super::manager::McpServerManager`] —
//! startup hydration, agent-tool installs, REST-API installs.
//!
//! Concurrency: writers serialise on a single `RwLock`. Readers
//! ([`get`], [`summaries`], [`ids`]) take a read lock that briefly
//! blocks installs; this is fine since installs are operator-driven
//! (handful per hour at most) while reads happen on every
//! `mcp.list` / `mcp.show` agent tool call.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;

use super::server::McpServer;

#[derive(Default)]
pub struct McpServerRegistry {
    servers: RwLock<HashMap<String, Arc<McpServer>>>,
}

impl McpServerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, id: impl Into<String>, server: Arc<McpServer>) {
        self.servers.write().insert(id.into(), server);
    }

    pub fn get(&self, id: &str) -> Option<Arc<McpServer>> {
        self.servers.read().get(id).cloned()
    }

    /// Pull the entry out of the registry. Returns the live server
    /// (with its `Arc<McpClient>` + refresh `JoinHandle`) so the
    /// caller can abort the refresh task and let the client drop
    /// when the last in-flight tool call finishes.
    pub fn remove(&self, id: &str) -> Option<Arc<McpServer>> {
        self.servers.write().remove(id)
    }

    /// Snapshot of currently-installed server IDs. Used by the
    /// manager when iterating for uninstall / TOML reconciliation.
    pub fn ids(&self) -> Vec<String> {
        self.servers.read().keys().cloned().collect()
    }

    pub fn summaries(&self) -> Vec<ServerSummary> {
        self.servers
            .read()
            .iter()
            .map(|(id, srv)| srv.summary(id))
            .collect()
    }

    /// Snapshot of `(server_id, shadow_plan)` for every server with
    /// non-empty shadow patterns. Used by [`super::access::McpToolAccess`]
    /// to compute per-user visibility every LLM round; trivially
    /// cheap because there are typically O(1) — O(handful) servers.
    pub fn shadow_plans(&self) -> Vec<(String, super::server::ShadowPlan)> {
        self.servers
            .read()
            .iter()
            .filter(|(_, srv)| !srv.shadow_plan().patterns.is_empty())
            .map(|(id, srv)| (id.clone(), srv.shadow_plan().clone()))
            .collect()
    }
}

/// Public diagnostic snapshot of one server. Surfaced by future
/// `mcp.list` agent tool / UI; doesn't expose the live `McpClient`
/// or the JoinHandle.
#[derive(Debug, Clone, Serialize)]
pub struct ServerSummary {
    pub id: String,
    pub endpoint: String,
    pub tool_count: usize,
    pub auth_kind: &'static str,
}

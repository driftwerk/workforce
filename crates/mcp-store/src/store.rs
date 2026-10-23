use eyre::Result;

use crate::model::McpServerRecord;

/// Per-agent MCP server persistence.
///
/// One store instance serves every agent on the hub; methods take
/// `agent_id` explicitly so the same backend (one SQLite file or one
/// Postgres schema) holds installs for all agents under a composite
/// `(agent_id, server_id)` key. Sync to match the rest of the
/// workforce store traits (`ProjectStore`, `IdentityStore`,
/// `PairingStore`, `SessionStore`).
pub trait McpServerStore: Send + Sync {
    /// Every server for one agent, ordered by `server_id`.
    fn list(&self, agent_id: &str) -> Result<Vec<McpServerRecord>>;

    /// Every server for every agent — used by the hub-startup
    /// hydration sweep so we don't make N round-trips per agent.
    fn list_all(&self) -> Result<Vec<McpServerRecord>>;

    fn get(&self, agent_id: &str, server_id: &str) -> Result<Option<McpServerRecord>>;

    /// Upsert. The implementation preserves `created_at` on
    /// existing rows and only refreshes `updated_at` from the
    /// supplied record.
    fn put(&self, record: &McpServerRecord) -> Result<()>;

    /// `Ok(false)` when no row existed.
    fn delete(&self, agent_id: &str, server_id: &str) -> Result<bool>;
}

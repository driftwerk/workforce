use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;

use crate::model::{InstalledVia, McpServerRecord};
use crate::store::McpServerStore;

pub struct SqliteMcpServerStore {
    conn: Mutex<Connection>,
}

impl SqliteMcpServerStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("Failed to open MCP server database: {:?}", path.as_ref()))?;
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "MCP server store opened");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS mcp_servers (
                agent_id      TEXT NOT NULL,
                server_id     TEXT NOT NULL,
                config        TEXT NOT NULL DEFAULT '{}',
                installed_via TEXT NOT NULL DEFAULT 'agent',
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                PRIMARY KEY (agent_id, server_id)
            );

            CREATE INDEX IF NOT EXISTS idx_mcp_servers_agent ON mcp_servers(agent_id);
            ",
        )
        .wrap_err("Failed to initialise MCP server schema")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre!("Failed to lock MCP server database: {}", e))
    }

    fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRecord> {
        let agent_id: String = row.get("agent_id")?;
        let server_id: String = row.get("server_id")?;
        let config_json: String = row.get("config")?;
        let installed_via_str: String = row.get("installed_via")?;
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let config: serde_json::Value = serde_json::from_str(&config_json)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let installed_via = InstalledVia::parse(&installed_via_str);

        Ok(McpServerRecord {
            agent_id,
            server_id,
            config,
            installed_via,
            created_at: parse_ts(&created_at_str),
            updated_at: parse_ts(&updated_at_str),
        })
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl McpServerStore for SqliteMcpServerStore {
    fn list(&self, agent_id: &str) -> Result<Vec<McpServerRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                 FROM mcp_servers WHERE agent_id = ?1 ORDER BY server_id",
            )
            .wrap_err("Failed to prepare mcp server list")?;
        let rows = stmt
            .query_map(params![agent_id], Self::parse_row)
            .wrap_err("Failed to execute mcp server list")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.wrap_err("Failed to parse mcp server row")?);
        }
        Ok(out)
    }

    fn list_all(&self) -> Result<Vec<McpServerRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                 FROM mcp_servers ORDER BY agent_id, server_id",
            )
            .wrap_err("Failed to prepare mcp server list_all")?;
        let rows = stmt
            .query_map([], Self::parse_row)
            .wrap_err("Failed to execute mcp server list_all")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.wrap_err("Failed to parse mcp server row")?);
        }
        Ok(out)
    }

    fn get(&self, agent_id: &str, server_id: &str) -> Result<Option<McpServerRecord>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT agent_id, server_id, config, installed_via, created_at, updated_at \
                 FROM mcp_servers WHERE agent_id = ?1 AND server_id = ?2",
                params![agent_id, server_id],
                Self::parse_row,
            )
            .optional()
            .wrap_err("Failed to fetch mcp server")?;
        Ok(row)
    }

    fn put(&self, record: &McpServerRecord) -> Result<()> {
        let conn = self.lock()?;
        let config_json = serde_json::to_string(&record.config)?;
        // Upsert preserves `created_at` from the original row when
        // present so a re-install bump doesn't lie about install date.
        conn.execute(
            "INSERT INTO mcp_servers \
                 (agent_id, server_id, config, installed_via, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(agent_id, server_id) DO UPDATE SET \
                 config        = excluded.config, \
                 installed_via = excluded.installed_via, \
                 updated_at    = excluded.updated_at",
            params![
                record.agent_id,
                record.server_id,
                config_json,
                record.installed_via.as_str(),
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )
        .wrap_err("Failed to upsert mcp server")?;
        Ok(())
    }

    fn delete(&self, agent_id: &str, server_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM mcp_servers WHERE agent_id = ?1 AND server_id = ?2",
                params![agent_id, server_id],
            )
            .wrap_err("Failed to delete mcp server")?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> SqliteMcpServerStore {
        SqliteMcpServerStore::in_memory().unwrap()
    }

    fn sample_config(endpoint: &str) -> serde_json::Value {
        json!({
            "endpoint": endpoint,
            "transport": "streamable-http",
            "auth": { "kind": "pat", "vault_path": "mcp/v1/test/token" },
            "allowed_hosts": [],
            "refresh_interval_secs": 3600,
            "shadows": [],
        })
    }

    #[test]
    fn put_and_get_round_trips() {
        let s = store();
        let r = McpServerRecord::new(
            "allan",
            "linear",
            sample_config("https://mcp.linear.app/mcp"),
            InstalledVia::Agent,
        );
        s.put(&r).unwrap();

        let fetched = s.get("allan", "linear").unwrap().unwrap();
        assert_eq!(fetched.agent_id, "allan");
        assert_eq!(fetched.server_id, "linear");
        assert_eq!(fetched.installed_via, InstalledVia::Agent);
        assert_eq!(fetched.config["endpoint"], "https://mcp.linear.app/mcp");
    }

    #[test]
    fn get_returns_none_when_missing() {
        let s = store();
        assert!(s.get("nobody", "missing").unwrap().is_none());
    }

    #[test]
    fn list_filters_by_agent_and_orders_by_server_id() {
        let s = store();
        s.put(&McpServerRecord::new(
            "allan",
            "z-server",
            sample_config("u1"),
            InstalledVia::Agent,
        ))
        .unwrap();
        s.put(&McpServerRecord::new(
            "allan",
            "a-server",
            sample_config("u2"),
            InstalledVia::Agent,
        ))
        .unwrap();
        s.put(&McpServerRecord::new(
            "rita",
            "a-server",
            sample_config("u3"),
            InstalledVia::Agent,
        ))
        .unwrap();

        let allans = s.list("allan").unwrap();
        assert_eq!(allans.len(), 2);
        assert_eq!(allans[0].server_id, "a-server");
        assert_eq!(allans[1].server_id, "z-server");

        let ritas = s.list("rita").unwrap();
        assert_eq!(ritas.len(), 1);
    }

    #[test]
    fn put_is_upsert_overwriting_config_and_provenance() {
        let s = store();
        let mut r = McpServerRecord::new(
            "allan",
            "linear",
            sample_config("https://old"),
            InstalledVia::Toml,
        );
        s.put(&r).unwrap();

        // Simulate the agent-driven re-install path: same key, new
        // config, new provenance.
        r.config = sample_config("https://new");
        r.installed_via = InstalledVia::Agent;
        r.updated_at = Utc::now();
        s.put(&r).unwrap();

        let fetched = s.get("allan", "linear").unwrap().unwrap();
        assert_eq!(fetched.config["endpoint"], "https://new");
        assert_eq!(fetched.installed_via, InstalledVia::Agent);
    }

    #[test]
    fn delete_returns_true_on_hit_false_on_miss() {
        let s = store();
        s.put(&McpServerRecord::new(
            "allan",
            "linear",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        assert!(s.delete("allan", "linear").unwrap());
        assert!(!s.delete("allan", "linear").unwrap());
        assert!(s.get("allan", "linear").unwrap().is_none());
    }

    #[test]
    fn delete_is_scoped_to_agent() {
        let s = store();
        s.put(&McpServerRecord::new(
            "allan",
            "linear",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        s.put(&McpServerRecord::new(
            "rita",
            "linear",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        assert!(s.delete("allan", "linear").unwrap());
        assert!(s.get("allan", "linear").unwrap().is_none());
        assert!(s.get("rita", "linear").unwrap().is_some());
    }

    #[test]
    fn list_all_returns_every_agents_servers_ordered_by_agent_then_server() {
        let s = store();
        s.put(&McpServerRecord::new(
            "rita",
            "alpha",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        s.put(&McpServerRecord::new(
            "allan",
            "zeta",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        s.put(&McpServerRecord::new(
            "allan",
            "alpha",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();

        let all = s.list_all().unwrap();
        let pairs: Vec<(&str, &str)> = all
            .iter()
            .map(|r| (r.agent_id.as_str(), r.server_id.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("allan", "alpha"), ("allan", "zeta"), ("rita", "alpha")]
        );
    }

    #[test]
    fn schema_persists_across_open_close() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("mcp.db");

        let s1 = SqliteMcpServerStore::new(&db).unwrap();
        s1.put(&McpServerRecord::new(
            "allan",
            "linear",
            sample_config("u"),
            InstalledVia::Agent,
        ))
        .unwrap();
        drop(s1);

        let s2 = SqliteMcpServerStore::new(&db).unwrap();
        assert!(s2.get("allan", "linear").unwrap().is_some());
    }
}

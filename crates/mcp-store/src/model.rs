use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One installed MCP server for a given agent.
///
/// `config` is held as opaque JSON so this crate doesn't need to
/// depend on `workforce-common` (which would create a dep cycle —
/// `workforce-common` already pulls in `workforce-pairing`, and the
/// pairing/identity/projects crates deliberately avoid depending on
/// `workforce-common` for the same reason). The consumer
/// (`workforce-hub`) deserialises this into `McpServerConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerRecord {
    pub agent_id: String,
    pub server_id: String,
    pub config: serde_json::Value,
    pub installed_via: InstalledVia,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl McpServerRecord {
    pub fn new(
        agent_id: impl Into<String>,
        server_id: impl Into<String>,
        config: serde_json::Value,
        installed_via: InstalledVia,
    ) -> Self {
        let now = Utc::now();
        Self {
            agent_id: agent_id.into(),
            server_id: server_id.into(),
            config,
            installed_via,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Provenance of the install. Used by the manager when reconciling
/// TOML config against the store on boot — TOML-originated rows can
/// be cleaned up if the operator removes the TOML stanza, while
/// agent/api rows are preserved across restarts even if no
/// corresponding TOML exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledVia {
    Toml,
    Agent,
    Api,
}

impl InstalledVia {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstalledVia::Toml => "toml",
            InstalledVia::Agent => "agent",
            InstalledVia::Api => "api",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "toml" => InstalledVia::Toml,
            "api" => InstalledVia::Api,
            _ => InstalledVia::Agent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_round_trips_through_json() {
        let r = McpServerRecord::new(
            "allan",
            "linear",
            json!({"endpoint": "https://mcp.linear.app/mcp"}),
            InstalledVia::Agent,
        );
        let s = serde_json::to_string(&r).unwrap();
        let back: McpServerRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn installed_via_parses_known_strings() {
        assert_eq!(InstalledVia::parse("toml"), InstalledVia::Toml);
        assert_eq!(InstalledVia::parse("agent"), InstalledVia::Agent);
        assert_eq!(InstalledVia::parse("api"), InstalledVia::Api);
        // Unknown defaults to Agent (the most common runtime path)
        // rather than panicking on legacy DB rows.
        assert_eq!(InstalledVia::parse("future"), InstalledVia::Agent);
    }

    #[test]
    fn installed_via_round_trips_via_serde() {
        for v in [InstalledVia::Toml, InstalledVia::Agent, InstalledVia::Api] {
            let s = serde_json::to_string(&v).unwrap();
            let back: InstalledVia = serde_json::from_str(&s).unwrap();
            assert_eq!(v, back);
        }
    }
}

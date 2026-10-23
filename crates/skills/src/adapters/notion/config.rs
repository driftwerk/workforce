//! Notion skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Hard upper bound on `max_results`. Protects agent context from huge result sets.
pub const DEFAULT_MAX_RESULTS: u64 = 100;

/// The Notion API version header value.
pub const NOTION_VERSION: &str = "2022-06-28";

fn default_max_results() -> u64 {
    DEFAULT_MAX_RESULTS
}

/// Configuration for a single Notion workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionWorkspaceConfig {
    /// Notion internal integration token. Sent as `Authorization: Bearer <token>`.
    pub token: String,
    /// Cap on results returned by list/query tools for this workspace.
    #[serde(default = "default_max_results")]
    pub max_results: u64,
}

/// Configuration for the Notion skill.
///
/// Supports both single-workspace (backward compatible) and multi-workspace modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionSkillConfig {
    /// Single-workspace token (backward compatible).
    #[serde(default)]
    pub token: Option<String>,
    /// Single-workspace result cap.
    #[serde(default = "default_max_results")]
    pub max_results: u64,
    /// Named workspaces for multi-workspace mode.
    #[serde(default)]
    pub workspaces: HashMap<String, NotionWorkspaceConfig>,
    /// Which workspace to use when `workspace` param is omitted.
    #[serde(default)]
    pub default_workspace: Option<String>,
}

impl NotionSkillConfig {
    /// Resolve all configured workspaces. If flat `token` is set, it becomes "default".
    pub fn resolve(&self) -> Vec<(String, NotionWorkspaceConfig)> {
        let mut result = Vec::new();
        if let Some(token) = self.token.as_ref() {
            result.push((
                "default".to_string(),
                NotionWorkspaceConfig {
                    token: token.clone(),
                    max_results: self.max_results,
                },
            ));
        }
        for (name, ws) in &self.workspaces {
            result.push((name.clone(), ws.clone()));
        }
        result
    }
}

impl Default for NotionSkillConfig {
    fn default() -> Self {
        Self {
            token: None,
            max_results: DEFAULT_MAX_RESULTS,
            workspaces: HashMap::new(),
            default_workspace: None,
        }
    }
}

/// Clamp a caller-supplied `limit` to the effective result cap for a workspace.
pub fn clamp_limit(requested: Option<u64>, max_results: u64) -> u64 {
    let cap = max_results.clamp(1, DEFAULT_MAX_RESULTS);
    match requested {
        None | Some(0) => cap,
        Some(n) => n.min(cap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal_single() {
        let json = r#"{ "token": "ntn_test123" }"#;
        let config: NotionSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.token.as_deref(), Some("ntn_test123"));
        assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn config_deserializes_multi_workspace() {
        let json = r#"{
            "default_workspace": "prod",
            "workspaces": {
                "prod":    { "token": "ntn_prod" },
                "sandbox": { "token": "ntn_sandbox", "max_results": 50 }
            }
        }"#;
        let config: NotionSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("prod"));
        assert_eq!(config.workspaces["sandbox"].max_results, 50);
        assert_eq!(config.resolve().len(), 2);
    }

    #[test]
    fn resolve_flat_becomes_default() {
        let json = r#"{ "token": "ntn_a" }"#;
        let config: NotionSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.token, "ntn_a");
    }

    #[test]
    fn resolve_combined_flat_and_workspaces() {
        let json = r#"{
            "token": "ntn_flat",
            "workspaces": { "extra": { "token": "ntn_extra" } }
        }"#;
        let config: NotionSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.resolve().len(), 2);
    }

    #[test]
    fn clamp_limit_enforces_hard_cap() {
        assert_eq!(clamp_limit(Some(500), 500), DEFAULT_MAX_RESULTS);
        assert_eq!(clamp_limit(Some(10), 100), 10);
        assert_eq!(clamp_limit(None, 50), 50);
        assert_eq!(clamp_limit(Some(0), 50), 50);
        assert_eq!(clamp_limit(Some(80), 30), 30);
    }
}

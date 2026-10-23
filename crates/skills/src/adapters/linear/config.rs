//! Linear skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://api.linear.app/graphql".to_string()
}

/// Configuration for a single Linear workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearWorkspaceConfig {
    /// Linear API key.
    pub api_key: String,
    /// Linear GraphQL API endpoint.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Default team ID for this workspace.
    #[serde(default)]
    pub default_team_id: Option<String>,
}

/// Configuration for the Linear skill.
///
/// Supports both single-workspace (backward compatible) and multi-workspace modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearSkillConfig {
    /// Single-workspace API key (backward compatible).
    /// Use `${vault:linear.api_token}` or `${LINEAR_API_TOKEN}`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Single-workspace base URL.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Single-workspace default team ID.
    #[serde(default)]
    pub default_team_id: Option<String>,
    /// Named workspaces for multi-workspace mode.
    #[serde(default)]
    pub workspaces: HashMap<String, LinearWorkspaceConfig>,
    /// Which workspace to use by default when `workspace` param is omitted.
    #[serde(default)]
    pub default_workspace: Option<String>,
    /// Mapping of team key prefixes to workspace names (e.g. "STA" → "contoso").
    /// Built by the skill loader from `team_keys` vault entries.
    #[serde(default)]
    pub prefix_map: HashMap<String, String>,
}

impl LinearSkillConfig {
    /// Resolve all configured workspaces. If flat `api_key` is set, it becomes "default".
    pub fn resolve(&self) -> Vec<(String, LinearWorkspaceConfig)> {
        let mut result = Vec::new();
        // Flat config -> "default" workspace
        if let Some(ref api_key) = self.api_key {
            result.push((
                "default".to_string(),
                LinearWorkspaceConfig {
                    api_key: api_key.clone(),
                    base_url: self.base_url.clone(),
                    default_team_id: self.default_team_id.clone(),
                },
            ));
        }
        // Named workspaces
        for (name, ws) in &self.workspaces {
            result.push((name.clone(), ws.clone()));
        }
        result
    }

    /// Generate a default `config.toml` for the Linear skill.
    ///
    /// Used by `workforce skill init linear-integration`.
    pub fn default_toml() -> String {
        r#"# Linear Integration Configuration
# Place this file at: {skills_dir}/linear-integration/config.toml
#
# API key can use vault references: ${vault:linear.api_token}
# Or environment variables: ${LINEAR_API_TOKEN}

# ── Single-workspace mode (backward compatible) ──
api_key = "${LINEAR_API_TOKEN}"

# Linear GraphQL API endpoint (default: https://api.linear.app/graphql)
# base_url = "https://api.linear.app/graphql"

# Default team ID for team-scoped operations when 'team_id' is not specified
# default_team_id = "your-team-uuid"

# ── Multi-workspace mode ──
# Uncomment to configure multiple workspaces. When workspaces are configured,
# tools accept an optional `workspace` parameter to target a specific workspace.
#
# default_workspace = "main"
#
# [workspaces.main]
# api_key = "${LINEAR_API_TOKEN_MAIN}"
# # base_url = "https://api.linear.app/graphql"
# # default_team_id = "team-uuid-main"
#
# [workspaces.secondary]
# api_key = "${LINEAR_API_TOKEN_SECONDARY}"
# # base_url = "https://api.linear.app/graphql"
# # default_team_id = "team-uuid-secondary"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{ "api_key": "lin_api_test123" }"#;
        let config: LinearSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("lin_api_test123"));
        assert_eq!(config.base_url, "https://api.linear.app/graphql");
        assert!(config.default_team_id.is_none());
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "api_key": "lin_api_test123",
            "base_url": "https://custom.linear.app/graphql",
            "default_team_id": "team-uuid-123"
        }"#;
        let config: LinearSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("lin_api_test123"));
        assert_eq!(config.base_url, "https://custom.linear.app/graphql");
        assert_eq!(config.default_team_id.as_deref(), Some("team-uuid-123"));
    }

    #[test]
    fn config_deserializes_multi_workspace() {
        let json = r#"{
            "workspaces": {
                "main": {
                    "api_key": "lin_api_main",
                    "default_team_id": "team-main"
                },
                "secondary": {
                    "api_key": "lin_api_secondary"
                }
            },
            "default_workspace": "main"
        }"#;
        let config: LinearSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.api_key.is_none());
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("main"));

        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_flat_config_becomes_default() {
        let json = r#"{ "api_key": "lin_api_test123" }"#;
        let config: LinearSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.api_key, "lin_api_test123");
    }

    #[test]
    fn resolve_combined_flat_and_workspaces() {
        let json = r#"{
            "api_key": "lin_api_flat",
            "workspaces": {
                "extra": { "api_key": "lin_api_extra" }
            }
        }"#;
        let config: LinearSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn default_toml_is_not_empty() {
        let toml = LinearSkillConfig::default_toml();
        assert!(toml.contains("api_key"));
        assert!(toml.contains("LINEAR_API_TOKEN"));
        assert!(toml.contains("workspaces"));
    }
}

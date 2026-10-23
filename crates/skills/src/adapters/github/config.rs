//! GitHub skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://api.github.com".to_string()
}

/// Configuration for a single GitHub workspace (org/account).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubWorkspaceConfig {
    /// Personal Access Token for this workspace.
    pub token: String,
    /// GitHub API base URL. Defaults to `https://api.github.com`.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Default owner (org or user) for this workspace.
    #[serde(default)]
    pub default_owner: Option<String>,
}

/// Configuration for the GitHub skill.
///
/// Supports both single-token (backward compatible) and multi-workspace modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubSkillConfig {
    /// Single-workspace token (backward compatible).
    #[serde(default)]
    pub token: Option<String>,
    /// Single-workspace base URL.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Single-workspace default owner.
    #[serde(default)]
    pub default_owner: Option<String>,
    /// Named workspaces for multi-org mode.
    #[serde(default)]
    pub workspaces: HashMap<String, GithubWorkspaceConfig>,
    /// Which workspace to use by default when owner doesn't match any workspace.
    #[serde(default)]
    pub default_workspace: Option<String>,
}

impl GithubSkillConfig {
    /// Resolve all configured workspaces. If flat `token` is set, it becomes "default".
    pub fn resolve(&self) -> Vec<(String, GithubWorkspaceConfig)> {
        let mut result = Vec::new();
        // Flat config -> "default" workspace
        if let Some(ref token) = self.token {
            result.push((
                "default".to_string(),
                GithubWorkspaceConfig {
                    token: token.clone(),
                    base_url: self.base_url.clone(),
                    default_owner: self.default_owner.clone(),
                },
            ));
        }
        // Named workspaces
        for (name, ws) in &self.workspaces {
            result.push((name.clone(), ws.clone()));
        }
        result
    }

    /// Generate a default `config.toml` for the GitHub skill.
    ///
    /// Used by `workforce skill init github-integration`.
    pub fn default_toml() -> String {
        r#"# GitHub Integration Configuration
# Place this file at: {skills_dir}/github-integration/config.toml
#
# Token can use vault references: ${vault:github.token}
# Or environment variables: ${GITHUB_TOKEN}

# ── Single-token mode (backward compatible) ──
token = "${GITHUB_TOKEN}"

# GitHub API base URL (default: https://api.github.com)
# For GitHub Enterprise, set to your instance URL:
# base_url = "https://github.example.com/api/v3"

# Default owner (org or user) for tools when 'owner' is not specified
# default_owner = "your-org"

# ── Multi-org mode ──
# Use fine-grained PATs scoped per org. The owner parameter on tools
# auto-selects the right token when it matches a workspace name.
#
# default_workspace = "my-org"
#
# [workspaces.my-org]
# token = "${GITHUB_TOKEN_MY_ORG}"
# # base_url = "https://api.github.com"
#
# [workspaces.client-org]
# token = "${GITHUB_TOKEN_CLIENT}"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{ "token": "ghp_test123" }"#;
        let config: GithubSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.token.as_deref(), Some("ghp_test123"));
        assert_eq!(config.base_url, "https://api.github.com");
        assert!(config.default_owner.is_none());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "token": "ghp_test123",
            "base_url": "https://github.example.com/api/v3",
            "default_owner": "acme-corp"
        }"#;
        let config: GithubSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.token.as_deref(), Some("ghp_test123"));
        assert_eq!(config.base_url, "https://github.example.com/api/v3");
        assert_eq!(config.default_owner.as_deref(), Some("acme-corp"));
    }

    #[test]
    fn config_deserializes_multi_workspace() {
        let json = r#"{
            "workspaces": {
                "driftwerk": {
                    "token": "ghp_driftwerk"
                },
                "contoso": {
                    "token": "ghp_contoso"
                }
            },
            "default_workspace": "driftwerk"
        }"#;
        let config: GithubSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.token.is_none());
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("driftwerk"));

        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_flat_config_becomes_default() {
        let json = r#"{ "token": "ghp_test123" }"#;
        let config: GithubSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.token, "ghp_test123");
    }

    #[test]
    fn default_toml_is_not_empty() {
        let toml = GithubSkillConfig::default_toml();
        assert!(toml.contains("token"));
        assert!(toml.contains("GITHUB_TOKEN"));
        assert!(toml.contains("workspaces"));
    }
}

//! Jira Cloud skill configuration.
//!
//! Mirrors the multi-workspace pattern used by GitHub and Linear: a flat
//! single-tenant config (backward compatible) plus an optional `workspaces`
//! map for multi-tenant setups, with project-key prefixes that auto-route
//! issue keys like `ENG-123` to the correct tenant.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Configuration for a single Jira Cloud tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraWorkspaceConfig {
    /// Atlassian account email — used as the Basic-auth username.
    pub email: String,
    /// Jira Cloud API token from id.atlassian.com/manage-profile/security/api-tokens.
    pub api_token: String,
    /// Tenant URL, e.g. `https://acme.atlassian.net` (no trailing slash).
    /// Optional under the hub: a per-user site resolves per request.
    #[serde(default)]
    pub site: Option<String>,
    /// Default project key (e.g. `ENG`) used when a tool's `project_key` is omitted.
    #[serde(default)]
    pub default_project: Option<String>,
    /// Project-key prefixes owned by this workspace. Used to auto-route issue
    /// keys like `ENG-123` to the right tenant. Populated from the vault
    /// `skills/jira/workspaces/<name>/project_keys` entry (comma-separated).
    #[serde(default)]
    pub project_keys: Vec<String>,
}

/// Configuration for the Jira skill.
///
/// Supports both single-tenant (backward compatible) and multi-tenant modes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraSkillConfig {
    /// Single-tenant email (backward compatible).
    #[serde(default)]
    pub email: Option<String>,
    /// Single-tenant API token (backward compatible).
    #[serde(default)]
    pub api_token: Option<String>,
    /// Single-tenant site URL (backward compatible).
    #[serde(default)]
    pub site: Option<String>,
    /// Single-tenant default project key.
    #[serde(default)]
    pub default_project: Option<String>,
    /// Named workspaces for multi-tenant mode.
    #[serde(default)]
    pub workspaces: HashMap<String, JiraWorkspaceConfig>,
    /// Which workspace to use by default when no project/issue key matches.
    #[serde(default)]
    pub default_workspace: Option<String>,
    /// Mapping of project-key prefixes (uppercase) to workspace names. Built
    /// by the skill loader from `project_keys` vault entries.
    #[serde(default)]
    pub prefix_map: HashMap<String, String>,
}

impl JiraSkillConfig {
    /// Resolve all configured workspaces. If flat fields are set, they become
    /// a workspace named `"default"`.
    pub fn resolve(&self) -> Vec<(String, JiraWorkspaceConfig)> {
        let mut result = Vec::new();
        if let (Some(email), Some(api_token)) = (self.email.as_ref(), self.api_token.as_ref()) {
            result.push((
                "default".to_string(),
                JiraWorkspaceConfig {
                    email: email.clone(),
                    api_token: api_token.clone(),
                    site: self.site.clone(),
                    default_project: self.default_project.clone(),
                    project_keys: Vec::new(),
                },
            ));
        }
        for (name, ws) in &self.workspaces {
            result.push((name.clone(), ws.clone()));
        }
        result
    }

    /// Generate a default `config.toml` for the Jira skill.
    ///
    /// Used by `workforce skill init jira-integration`.
    pub fn default_toml() -> String {
        r#"# Jira Cloud Integration Configuration
# Place this file at: {skills_dir}/jira-integration/config.toml
#
# API tokens are generated at:
#   https://id.atlassian.com/manage-profile/security/api-tokens
# Values can use vault references (${vault:jira/api_token}) or env vars (${JIRA_API_TOKEN}).

# ── Single-tenant mode (backward compatible) ──
email     = "${JIRA_EMAIL}"
api_token = "${JIRA_API_TOKEN}"
site      = "https://your-tenant.atlassian.net"

# Optional: default project key for tools when 'project_key' is not specified
# default_project = "ENG"

# ── Multi-tenant mode ──
# Configure multiple Atlassian tenants. The `project_keys` list enables
# auto-routing: an `issue_key` like `ENG-123` is routed to the workspace
# that owns the `ENG` prefix.
#
# default_workspace = "acme"
#
# [workspaces.acme]
# email        = "${JIRA_EMAIL_ACME}"
# api_token    = "${JIRA_TOKEN_ACME}"
# site         = "https://acme.atlassian.net"
# default_project = "ENG"
# project_keys = ["ENG", "INFRA"]
#
# [workspaces.client]
# email        = "${JIRA_EMAIL_CLIENT}"
# api_token    = "${JIRA_TOKEN_CLIENT}"
# site         = "https://client.atlassian.net"
# project_keys = ["OPS"]
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{
            "email": "you@acme.com",
            "api_token": "abcd1234",
            "site": "https://acme.atlassian.net"
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.email.as_deref(), Some("you@acme.com"));
        assert_eq!(config.api_token.as_deref(), Some("abcd1234"));
        assert_eq!(config.site.as_deref(), Some("https://acme.atlassian.net"));
        assert!(config.default_project.is_none());
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "email": "you@acme.com",
            "api_token": "abcd1234",
            "site": "https://acme.atlassian.net",
            "default_project": "ENG"
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.default_project.as_deref(), Some("ENG"));
    }

    #[test]
    fn config_deserializes_multi_workspace() {
        let json = r#"{
            "workspaces": {
                "acme": {
                    "email": "you@acme.com",
                    "api_token": "tokA",
                    "site": "https://acme.atlassian.net",
                    "project_keys": ["ENG", "INFRA"]
                },
                "client": {
                    "email": "you@client.com",
                    "api_token": "tokB",
                    "site": "https://client.atlassian.net",
                    "project_keys": ["OPS"]
                }
            },
            "default_workspace": "acme"
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.email.is_none());
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("acme"));
        let acme = &config.workspaces["acme"];
        assert_eq!(acme.project_keys, vec!["ENG", "INFRA"]);

        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_flat_config_becomes_default() {
        let json = r#"{
            "email": "you@acme.com",
            "api_token": "abcd1234",
            "site": "https://acme.atlassian.net"
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.api_token, "abcd1234");
        assert_eq!(
            resolved[0].1.site.as_deref(),
            Some("https://acme.atlassian.net")
        );
    }

    #[test]
    fn resolve_combined_flat_and_workspaces() {
        let json = r#"{
            "email": "you@acme.com",
            "api_token": "tokA",
            "site": "https://acme.atlassian.net",
            "workspaces": {
                "client": {
                    "email": "you@client.com",
                    "api_token": "tokB",
                    "site": "https://client.atlassian.net"
                }
            }
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().any(|(n, _)| n == "default"));
        assert!(resolved.iter().any(|(n, _)| n == "client"));
    }

    #[test]
    fn resolve_flat_without_site_creates_default_with_no_baseline() {
        let json = r#"{
            "email": "you@acme.com",
            "api_token": "abcd1234"
        }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].1.site.is_none());

        let json = r#"{ "email": "you@acme.com" }"#;
        let config: JiraSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.resolve().is_empty());
    }

    #[test]
    fn default_toml_is_not_empty() {
        let toml = JiraSkillConfig::default_toml();
        assert!(toml.contains("email"));
        assert!(toml.contains("api_token"));
        assert!(toml.contains("site"));
        assert!(toml.contains("workspaces"));
        assert!(toml.contains("project_keys"));
    }
}

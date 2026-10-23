//! Postmark skill configuration.
//!
//! Mirrors the multi-workspace pattern used by Jira / Linear / Datadog /
//! Metabase: a flat single-tenant config (backward compatible) plus a map of
//! named workspaces. Each workspace carries a required `server_token` (used
//! for per-server Postmark APIs: send, messages, stats, bounces, templates,
//! streams, suppressions) and an optional `account_token` (used for
//! account-wide APIs: servers list, domains, sender signatures, data
//! removal). An optional `base_url` supports staging/sandbox deployments.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Configuration for a single Postmark workspace.
///
/// Note: Postmark itself calls each tenant a "server" in its UI, but we keep
/// the cross-adapter "workspaces" vocabulary here to avoid colliding with
/// `postmark.servers_list` (GET /servers) semantics in the tool namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostmarkWorkspaceConfig {
    /// Postmark Server API token — required. Sent as `X-Postmark-Server-Token`.
    pub server_token: String,
    /// Postmark Account API token — optional. Sent as
    /// `X-Postmark-Account-Token`. Without this, `servers_list`,
    /// `domains_list`, `domain_get`, and `postmark.api` with
    /// `token_type=account` return a clear error at call time.
    #[serde(default)]
    pub account_token: Option<String>,
    /// Override the Postmark base URL. Leave unset for the default
    /// `https://api.postmarkapp.com`. Useful only for staging or a custom
    /// reverse proxy.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Configuration for the Postmark skill.
///
/// Supports both single-tenant (backward compatible, flat fields) and
/// multi-tenant (`workspaces` map) modes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostmarkSkillConfig {
    /// Single-workspace server token (backward compatible).
    #[serde(default)]
    pub server_token: Option<String>,
    /// Single-workspace account token (backward compatible).
    #[serde(default)]
    pub account_token: Option<String>,
    /// Single-workspace base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Named workspaces for multi-workspace mode.
    #[serde(default)]
    pub workspaces: HashMap<String, PostmarkWorkspaceConfig>,
    /// Which workspace to use by default when no `workspace` parameter is
    /// passed to a tool.
    #[serde(default)]
    pub default_workspace: Option<String>,
}

impl PostmarkSkillConfig {
    /// Resolve all configured workspaces. If the flat `server_token` is set,
    /// it becomes a workspace named `"default"`. Missing flat `server_token`
    /// means no default workspace is synthesised — even if `account_token` or
    /// `base_url` alone is set (an account token is useless without a server
    /// token to pair with).
    pub fn resolve(&self) -> Vec<(String, PostmarkWorkspaceConfig)> {
        let mut result = Vec::new();
        if let Some(ref server_token) = self.server_token {
            result.push((
                "default".to_string(),
                PostmarkWorkspaceConfig {
                    server_token: server_token.clone(),
                    account_token: self.account_token.clone(),
                    base_url: self.base_url.clone(),
                },
            ));
        }
        for (name, ws) in &self.workspaces {
            result.push((name.clone(), ws.clone()));
        }
        result
    }

    /// Generate a default `config.toml` for the Postmark skill.
    pub fn default_toml() -> String {
        r#"# Postmark Integration Configuration
# Place this file at: {skills_dir}/postmark-integration/config.toml
#
# Tokens are generated in the Postmark UI:
#   Server token  → Servers → <server> → API Tokens
#   Account token → Account → API Tokens (optional, only needed for
#                   servers_list, domains_list, domain_get, and
#                   postmark.api with token_type=account)
# Values can use vault references (${vault:postmark/server_token}) or
# env vars (${POSTMARK_SERVER_TOKEN}).

# ── Single-workspace mode (backward compatible) ──
server_token = "${POSTMARK_SERVER_TOKEN}"
# account_token = "${POSTMARK_ACCOUNT_TOKEN}"   # optional
# base_url      = "https://api.postmarkapp.com" # default; only set for staging

# ── Multi-workspace mode ──
# Configure multiple Postmark servers. Each workspace has its own server_token
# (required) and optional account_token. Tools accept an optional `workspace`
# parameter to target a specific workspace.
#
# default_workspace = "prod"
#
# [workspaces.prod]
# server_token  = "${POSTMARK_SERVER_TOKEN_PROD}"
# account_token = "${POSTMARK_ACCOUNT_TOKEN}"
#
# [workspaces.staging]
# server_token  = "${POSTMARK_SERVER_TOKEN_STAGING}"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{ "server_token": "pm_server_abc" }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.server_token.as_deref(), Some("pm_server_abc"));
        assert!(config.account_token.is_none());
        assert!(config.base_url.is_none());
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "server_token":  "pm_server_abc",
            "account_token": "pm_account_xyz",
            "base_url":      "https://api.postmarkapp.com"
        }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.server_token.as_deref(), Some("pm_server_abc"));
        assert_eq!(config.account_token.as_deref(), Some("pm_account_xyz"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://api.postmarkapp.com")
        );
    }

    #[test]
    fn config_deserializes_multi_workspace() {
        let json = r#"{
            "workspaces": {
                "prod": {
                    "server_token":  "pm_prod",
                    "account_token": "pm_acct"
                },
                "staging": {
                    "server_token": "pm_staging"
                }
            },
            "default_workspace": "prod"
        }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.server_token.is_none());
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("prod"));
        let prod = &config.workspaces["prod"];
        assert_eq!(prod.account_token.as_deref(), Some("pm_acct"));
        let staging = &config.workspaces["staging"];
        assert!(staging.account_token.is_none());

        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_flat_config_becomes_default() {
        let json = r#"{ "server_token": "pm_flat", "account_token": "pm_acct" }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.server_token, "pm_flat");
        assert_eq!(resolved[0].1.account_token.as_deref(), Some("pm_acct"));
    }

    #[test]
    fn resolve_combined_flat_and_workspaces() {
        let json = r#"{
            "server_token": "pm_flat",
            "workspaces": {
                "client": { "server_token": "pm_client" }
            }
        }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().any(|(n, _)| n == "default"));
        assert!(resolved.iter().any(|(n, _)| n == "client"));
    }

    #[test]
    fn resolve_partial_flat_does_not_create_default() {
        // Only account_token, missing server_token → no default workspace.
        let json = r#"{ "account_token": "pm_acct" }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.resolve().is_empty());
    }

    #[test]
    fn resolve_flat_without_account_token_is_fine() {
        // Server token alone is a valid single-workspace config.
        let json = r#"{ "server_token": "pm_server_only" }"#;
        let config: PostmarkSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].1.account_token.is_none());
    }

    #[test]
    fn default_toml_is_not_empty() {
        let toml = PostmarkSkillConfig::default_toml();
        assert!(toml.contains("server_token"));
        assert!(toml.contains("account_token"));
        assert!(toml.contains("workspaces"));
        assert!(toml.contains("POSTMARK_SERVER_TOKEN"));
    }
}

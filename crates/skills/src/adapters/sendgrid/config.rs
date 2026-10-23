//! SendGrid skill configuration.
//!
//! Mirrors the multi-workspace pattern used by Postmark / Jira / Linear: a
//! flat single-workspace config (backward compatible) plus a map of named
//! workspaces. Each workspace carries one `api_key` (sent as a bearer
//! token), an optional default sender (`from_email` / `from_name`) and an
//! optional `base_url` for regional endpoints such as
//! `https://api.eu.sendgrid.com`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Configuration for a single SendGrid workspace (one API key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendGridWorkspaceConfig {
    /// SendGrid API key (`SG.…`), sent as `Authorization: Bearer`.
    pub api_key: String,
    /// Default From address when a send omits `from`. Must be a verified
    /// sender identity or on an authenticated domain in the account.
    #[serde(default)]
    pub from_email: Option<String>,
    /// Display name for the default From address.
    #[serde(default)]
    pub from_name: Option<String>,
    /// Override the API base URL. Leave unset for the global
    /// `https://api.sendgrid.com`; set for EU data residency.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Configuration for the SendGrid skill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendGridSkillConfig {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub from_name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Named workspaces for multi-workspace mode.
    #[serde(default)]
    pub workspaces: HashMap<String, SendGridWorkspaceConfig>,
    /// Workspace used when a tool call omits `workspace`.
    #[serde(default)]
    pub default_workspace: Option<String>,
}

impl SendGridSkillConfig {
    /// Resolve all configured workspaces. The flat `api_key`, when set,
    /// becomes a workspace named `"default"`; a flat sender without a key
    /// is not a workspace.
    pub fn resolve(&self) -> Vec<(String, SendGridWorkspaceConfig)> {
        self.api_key
            .as_ref()
            .map(|api_key| {
                (
                    "default".to_string(),
                    SendGridWorkspaceConfig {
                        api_key: api_key.clone(),
                        from_email: self.from_email.clone(),
                        from_name: self.from_name.clone(),
                        base_url: self.base_url.clone(),
                    },
                )
            })
            .into_iter()
            .chain(
                self.workspaces
                    .iter()
                    .map(|(name, ws)| (name.clone(), ws.clone())),
            )
            .collect()
    }

    /// Generate a default `config.toml` for the SendGrid skill.
    pub fn default_toml() -> String {
        r#"# SendGrid Integration Configuration
# Place this file at: {skills_dir}/sendgrid-integration/config.toml
#
# Create an API key in SendGrid: Settings → API Keys → Create API Key.
# A restricted key with Mail Send + Template Engine (read) + Stats (read) +
# Suppressions is enough for every tool; `sendgrid.api` needs whatever the
# endpoint you call requires.
# Values can use vault references (${vault:sendgrid/api_key}) or env vars
# (${SENDGRID_API_KEY}).

# ── Single-workspace mode (backward compatible) ──
api_key = "${SENDGRID_API_KEY}"
# from_email = "noreply@example.com"       # default From; must be a verified sender
# from_name  = "Example"
# base_url   = "https://api.eu.sendgrid.com" # only for EU-resident accounts

# ── Multi-workspace mode ──
# Configure multiple SendGrid accounts or subusers. Tools accept an optional
# `workspace` parameter to target a specific one.
#
# default_workspace = "prod"
#
# [workspaces.prod]
# api_key    = "${SENDGRID_API_KEY_PROD}"
# from_email = "noreply@example.com"
#
# [workspaces.staging]
# api_key = "${SENDGRID_API_KEY_STAGING}"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let config: SendGridSkillConfig =
            serde_json::from_str(r#"{ "api_key": "SG.abc" }"#).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("SG.abc"));
        assert!(config.from_email.is_none());
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn config_deserializes_full_and_multi_workspace() {
        let config: SendGridSkillConfig = serde_json::from_str(
            r#"{
                "api_key": "SG.flat",
                "from_email": "a@b.com",
                "from_name": "A",
                "base_url": "https://api.eu.sendgrid.com",
                "workspaces": {
                    "prod": { "api_key": "SG.prod", "from_email": "p@b.com" },
                    "staging": { "api_key": "SG.staging" }
                },
                "default_workspace": "prod"
            }"#,
        )
        .unwrap();
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.default_workspace.as_deref(), Some("prod"));
        assert_eq!(
            config.workspaces["prod"].from_email.as_deref(),
            Some("p@b.com")
        );
        assert!(config.workspaces["staging"].from_email.is_none());

        let resolved = config.resolve();
        assert_eq!(resolved.len(), 3);
        let (name, default) = resolved.first().unwrap();
        assert_eq!(name, "default");
        assert_eq!(default.api_key, "SG.flat");
        assert_eq!(default.from_name.as_deref(), Some("A"));
        assert_eq!(
            default.base_url.as_deref(),
            Some("https://api.eu.sendgrid.com")
        );
    }

    #[test]
    fn sender_without_key_is_not_a_workspace() {
        let config: SendGridSkillConfig =
            serde_json::from_str(r#"{ "from_email": "a@b.com" }"#).unwrap();
        assert!(config.resolve().is_empty());
    }

    #[test]
    fn default_toml_mentions_every_field() {
        let toml = SendGridSkillConfig::default_toml();
        for needle in [
            "api_key",
            "from_email",
            "from_name",
            "base_url",
            "workspaces",
        ] {
            assert!(toml.contains(needle), "{needle}");
        }
    }
}

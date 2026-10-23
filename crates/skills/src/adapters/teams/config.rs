//! Teams skill configuration — mirrors `slack-integration`'s shape.
//!
//! Read from `skills/teams-integration/config.toml`; `app_id` / `app_password`
//! are resolved from the vault at startup (keys `teams.<id>.app_id` /
//! `teams.<id>.app_password`), never stored in the toml.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Top-level Teams skill config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamsSkillConfig {
    /// One entry per bot registration. Usually one; multiple only when grouping
    /// distinct app registrations behind the same hub.
    #[serde(default)]
    pub apps: Vec<TeamsAppConfig>,
    /// `app id` → agent id. Empty binds all agents (mirrors Slack `agent_bindings`).
    #[serde(default)]
    pub agent_bindings: HashMap<String, String>,
}

/// Credentials + identity for one bot registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamsAppConfig {
    /// Config id; matches the vault key prefix `teams.<id>.*` and `agent_bindings`.
    pub id: String,
    /// Microsoft App ID (the bot's Entra client id). Resolved from the vault at
    /// startup. Also the JWT `aud` value the inbound auth enforces.
    #[serde(default)]
    pub app_id: String,
    /// Client secret used to acquire connector tokens for outbound calls.
    /// Resolved from the vault at startup.
    #[serde(default)]
    pub app_password: String,
}

impl TeamsAppConfig {
    /// Both credentials present — required before the app can connect.
    pub fn is_usable(&self) -> bool {
        !self.app_id.is_empty() && !self.app_password.is_empty()
    }
}

impl TeamsSkillConfig {
    /// Apps with credentials resolved (mirrors Slack's `partition_workspaces`).
    pub fn usable_apps(&self) -> Vec<&TeamsAppConfig> {
        self.apps.iter().filter(|a| a.is_usable()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_from_toml() {
        let toml = r#"
            [[apps]]
            id = "default"

            [agent_bindings]
            default = "agent-123"
        "#;
        let cfg: TeamsSkillConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.apps.len(), 1);
        assert_eq!(cfg.apps[0].id, "default");
        assert_eq!(
            cfg.agent_bindings.get("default").map(String::as_str),
            Some("agent-123")
        );
    }

    #[test]
    fn usable_requires_both_credentials() {
        let mut app = TeamsAppConfig {
            id: "default".into(),
            app_id: String::new(),
            app_password: String::new(),
        };
        assert!(!app.is_usable());
        app.app_id = "app".into();
        assert!(!app.is_usable());
        app.app_password = "secret".into();
        assert!(app.is_usable());
    }

    #[test]
    fn usable_apps_filters_unconfigured() {
        let cfg = TeamsSkillConfig {
            apps: vec![
                TeamsAppConfig {
                    id: "a".into(),
                    app_id: "x".into(),
                    app_password: "y".into(),
                },
                TeamsAppConfig {
                    id: "b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let usable = cfg.usable_apps();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].id, "a");
    }
}

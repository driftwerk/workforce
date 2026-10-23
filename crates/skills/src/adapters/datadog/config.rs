//! Datadog skill configuration.
//!
//! Mirrors the multi-account pattern used by `github`, `linear`, and `jira`:
//! a flat single-account config (backward compatible) plus an optional
//! `accounts` map for multi-tenant setups. Each account is bound to a
//! Datadog site (region) which determines the API base URL.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_site() -> String {
    "us1".to_string()
}

/// Configuration for a single Datadog account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatadogAccountConfig {
    /// Org-scoped API key (`DD-API-KEY` header).
    pub api_key: String,
    /// User-scoped Application key (`DD-APPLICATION-KEY` header).
    pub app_key: String,
    /// Datadog site key: `us1` | `us3` | `us5` | `eu1` | `ap1`. Defaults to `us1`.
    #[serde(default = "default_site")]
    pub site: String,
    /// Optional explicit base URL override (FedRAMP, custom domain).
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Configuration for the Datadog skill.
///
/// Supports both single-account (backward compatible) and multi-account modes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatadogSkillConfig {
    /// Single-account API key (backward compatible).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Single-account Application key.
    #[serde(default)]
    pub app_key: Option<String>,
    /// Single-account site key.
    #[serde(default)]
    pub site: Option<String>,
    /// Single-account explicit base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Named accounts for multi-tenant mode.
    #[serde(default)]
    pub accounts: HashMap<String, DatadogAccountConfig>,
    /// Which account to use by default when `account` is omitted.
    #[serde(default)]
    pub default_account: Option<String>,
}

impl DatadogSkillConfig {
    /// Resolve all configured accounts. If both flat `api_key` and `app_key`
    /// are set, they become an account named `"default"`.
    pub fn resolve(&self) -> Vec<(String, DatadogAccountConfig)> {
        let flat = self
            .api_key
            .as_ref()
            .zip(self.app_key.as_ref())
            .map(|(api, app)| {
                (
                    "default".to_string(),
                    DatadogAccountConfig {
                        api_key: api.clone(),
                        app_key: app.clone(),
                        site: self.site.clone().unwrap_or_else(default_site),
                        base_url: self.base_url.clone(),
                    },
                )
            });
        flat.into_iter()
            .chain(
                self.accounts
                    .iter()
                    .map(|(name, cfg)| (name.clone(), cfg.clone())),
            )
            .collect()
    }

    /// Generate a default `config.toml` for the Datadog skill.
    pub fn default_toml() -> String {
        r#"# Datadog Integration Configuration
# Place this file at: {skills_dir}/datadog-integration/config.toml
#
# API + Application keys are generated at:
#   https://app.datadoghq.com/organization-settings/api-keys
#   https://app.datadoghq.com/personal-settings/application-keys
# Values can use vault references (${vault:datadog/api_key}) or env vars (${DD_API_KEY}).

# ── Single-account mode (backward compatible) ──
api_key = "${DD_API_KEY}"
app_key = "${DD_APP_KEY}"

# Datadog site: short key (us1, us3, us5, eu1, ap1) or hostname form
# (datadoghq.com, datadoghq.eu, us3.datadoghq.com, etc.)
# site = "us1"

# Optional explicit base URL (FedRAMP, custom domain). Overrides 'site'.
# base_url = "https://api.ddog-gov.com"

# ── Multi-account mode ──
# Configure multiple Datadog accounts. Each is bound to one site.
#
# default_account = "prod-us"
#
# [accounts.prod-us]
# api_key = "${DD_API_KEY_PROD_US}"
# app_key = "${DD_APP_KEY_PROD_US}"
# site    = "us1"
#
# [accounts.prod-eu]
# api_key = "${DD_API_KEY_PROD_EU}"
# app_key = "${DD_APP_KEY_PROD_EU}"
# site    = "eu1"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{ "api_key": "dd_api", "app_key": "dd_app" }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("dd_api"));
        assert_eq!(config.app_key.as_deref(), Some("dd_app"));
        assert!(config.site.is_none());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "api_key": "dd_api",
            "app_key": "dd_app",
            "site": "eu1",
            "base_url": "https://api.example.com"
        }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.site.as_deref(), Some("eu1"));
        assert_eq!(config.base_url.as_deref(), Some("https://api.example.com"));
    }

    #[test]
    fn config_deserializes_multi_account() {
        let json = r#"{
            "accounts": {
                "prod-us": { "api_key": "a1", "app_key": "b1", "site": "us1" },
                "prod-eu": { "api_key": "a2", "app_key": "b2", "site": "eu1" }
            },
            "default_account": "prod-us"
        }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.api_key.is_none());
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.default_account.as_deref(), Some("prod-us"));
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn account_config_uses_default_site_when_omitted() {
        let json = r#"{ "api_key": "a", "app_key": "b" }"#;
        let acct: DatadogAccountConfig = serde_json::from_str(json).unwrap();
        assert_eq!(acct.site, "us1");
    }

    #[test]
    fn resolve_flat_config_becomes_default() {
        let json = r#"{ "api_key": "a", "app_key": "b", "site": "eu1" }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.api_key, "a");
        assert_eq!(resolved[0].1.app_key, "b");
        assert_eq!(resolved[0].1.site, "eu1");
    }

    #[test]
    fn resolve_combined_flat_and_accounts() {
        let json = r#"{
            "api_key": "a",
            "app_key": "b",
            "accounts": {
                "client": { "api_key": "c", "app_key": "d", "site": "eu1" }
            }
        }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().any(|(n, _)| n == "default"));
        assert!(resolved.iter().any(|(n, _)| n == "client"));
    }

    #[test]
    fn resolve_partial_flat_does_not_create_default() {
        // api_key without app_key → no synthesised default account.
        let json = r#"{ "api_key": "a" }"#;
        let config: DatadogSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.resolve().is_empty());
    }

    #[test]
    fn default_toml_is_not_empty() {
        let toml = DatadogSkillConfig::default_toml();
        assert!(toml.contains("api_key"));
        assert!(toml.contains("app_key"));
        assert!(toml.contains("site"));
        assert!(toml.contains("accounts"));
    }
}

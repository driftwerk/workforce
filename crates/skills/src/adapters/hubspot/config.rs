//! HubSpot skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Hard upper bound on `max_results`. Protects agent context from huge result sets.
pub const DEFAULT_MAX_RESULTS: u64 = 100;

fn default_max_results() -> u64 {
    DEFAULT_MAX_RESULTS
}

/// Configuration for a single HubSpot portal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubspotPortalConfig {
    /// HubSpot Private App access token. Sent as `Authorization: Bearer <token>`.
    pub access_token: String,
    /// Cap on results returned by list/search tools for this portal.
    #[serde(default = "default_max_results")]
    pub max_results: u64,
}

/// Configuration for the HubSpot skill.
///
/// Supports both single-portal (backward compatible) and multi-portal modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubspotSkillConfig {
    /// Single-portal access token (backward compatible).
    #[serde(default)]
    pub access_token: Option<String>,
    /// Single-portal result cap.
    #[serde(default = "default_max_results")]
    pub max_results: u64,
    /// Named portals for multi-portal mode.
    #[serde(default)]
    pub portals: HashMap<String, HubspotPortalConfig>,
    /// Which portal to use when `portal` param is omitted.
    #[serde(default)]
    pub default_portal: Option<String>,
}

impl HubspotSkillConfig {
    /// Resolve all configured portals. If flat `access_token` is set, it becomes "default".
    pub fn resolve(&self) -> Vec<(String, HubspotPortalConfig)> {
        let mut result = Vec::new();
        if let Some(access_token) = self.access_token.as_ref() {
            result.push((
                "default".to_string(),
                HubspotPortalConfig {
                    access_token: access_token.clone(),
                    max_results: self.max_results,
                },
            ));
        }
        for (name, portal) in &self.portals {
            result.push((name.clone(), portal.clone()));
        }
        result
    }
}

impl Default for HubspotSkillConfig {
    fn default() -> Self {
        Self {
            access_token: None,
            max_results: DEFAULT_MAX_RESULTS,
            portals: HashMap::new(),
            default_portal: None,
        }
    }
}

/// Clamp a caller-supplied `limit` to the effective result cap for a portal.
///
/// The cap is `max_results` clamped into `[1, DEFAULT_MAX_RESULTS]`. A
/// requested limit of 0 or `None` means "use the cap".
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
        let json = r#"{ "access_token": "pat-test-123" }"#;
        let config: HubspotSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.access_token.as_deref(), Some("pat-test-123"));
        assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
        assert!(config.portals.is_empty());
    }

    #[test]
    fn config_deserializes_multi_portal() {
        let json = r#"{
            "default_portal": "prod",
            "portals": {
                "prod":    { "access_token": "pat-prod" },
                "sandbox": { "access_token": "pat-sandbox", "max_results": 50 }
            }
        }"#;
        let config: HubspotSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.portals.len(), 2);
        assert_eq!(config.default_portal.as_deref(), Some("prod"));
        assert_eq!(config.portals["sandbox"].max_results, 50);
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_flat_becomes_default() {
        let json = r#"{ "access_token": "pat-a" }"#;
        let config: HubspotSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.access_token, "pat-a");
    }

    #[test]
    fn resolve_combined_flat_and_portals() {
        let json = r#"{
            "access_token": "pat-flat",
            "portals": {
                "extra": { "access_token": "pat-extra" }
            }
        }"#;
        let config: HubspotSkillConfig = serde_json::from_str(json).unwrap();
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

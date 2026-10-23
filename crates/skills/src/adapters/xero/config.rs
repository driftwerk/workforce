//! Xero skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Default page size for list endpoints; also the hard upper bound. Keeps
/// agent context bounded for large ledgers.
pub const DEFAULT_PAGE_SIZE: u64 = 100;

/// Credentials for a single Xero entity (one custom connection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XeroEntityConfig {
    /// Custom-connection client id.
    pub client_id: String,
    /// Custom-connection client secret.
    pub client_secret: String,
    /// Optional fixed tenant id. When absent the client discovers it once via
    /// `/connections` and caches it.
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// Configuration for the Xero skill.
///
/// Supports both single-entity (flat fields → `default`) and multi-entity
/// (`entities`) modes, mirroring the other adapters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XeroSkillConfig {
    /// Single-entity client id (backward compatible).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Single-entity client secret.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Single-entity fixed tenant id.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Named entities for multi-entity mode.
    #[serde(default)]
    pub entities: HashMap<String, XeroEntityConfig>,
    /// Which entity to use when the `entity` param is omitted.
    #[serde(default)]
    pub default_entity: Option<String>,
}

impl XeroSkillConfig {
    /// Resolve all configured entities. Flat `client_id`/`client_secret`
    /// become the `default` entity.
    pub fn resolve(&self) -> Vec<(String, XeroEntityConfig)> {
        let flat = self
            .client_id
            .as_ref()
            .zip(self.client_secret.as_ref())
            .map(|(id, secret)| {
                (
                    "default".to_string(),
                    XeroEntityConfig {
                        client_id: id.clone(),
                        client_secret: secret.clone(),
                        tenant_id: self.tenant_id.clone(),
                    },
                )
            });
        flat.into_iter()
            .chain(self.entities.iter().map(|(n, e)| (n.clone(), e.clone())))
            .collect()
    }

    /// Generate a default `config.toml` for the Xero skill.
    pub fn default_toml() -> String {
        r#"# Xero Integration Configuration
# Place this file at: {skills_dir}/xero-integration/config.toml
#
# Credentials come from a Xero "Custom Connection" (client-credentials grant).
# Create one per entity at https://developer.xero.com/app/manage and subscribe
# it to the paid custom-connection plan. Each connection is single-tenant.

# ── Single-entity mode ──
client_id     = "${XERO_CLIENT_ID}"
client_secret = "${XERO_CLIENT_SECRET}"
# tenant_id is discovered automatically; set it only to pin a specific org.
# tenant_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

# ── Multi-entity mode ──
# Uncomment to configure multiple Xero organisations. Tools accept an optional
# `entity` parameter to select one.
#
# default_entity = "harbor"
#
# [entities.harbor]
# client_id     = "${XERO_CLIENT_ID_HARBOR}"
# client_secret = "${XERO_CLIENT_SECRET_HARBOR}"
#
# [entities.beacon]
# client_id     = "${XERO_CLIENT_ID_BEACON}"
# client_secret = "${XERO_CLIENT_SECRET_BEACON}"
"#
        .to_string()
    }
}

/// Clamp a caller-supplied `page_size` into `[1, DEFAULT_PAGE_SIZE]`. A value
/// of 0 or `None` means "use the default".
pub fn clamp_page_size(requested: Option<u64>) -> u64 {
    match requested {
        None | Some(0) => DEFAULT_PAGE_SIZE,
        Some(n) => n.clamp(1, DEFAULT_PAGE_SIZE),
    }
}

/// Clamp a caller-supplied `page` to a minimum of 1.
pub fn clamp_page(requested: Option<u64>) -> u64 {
    requested.unwrap_or(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal_single() {
        let json = r#"{ "client_id": "cid", "client_secret": "secret" }"#;
        let config: XeroSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.client_id.as_deref(), Some("cid"));
        assert_eq!(config.client_secret.as_deref(), Some("secret"));
        assert!(config.tenant_id.is_none());
        assert!(config.entities.is_empty());
    }

    #[test]
    fn resolve_flat_becomes_default() {
        let json = r#"{ "client_id": "cid", "client_secret": "secret", "tenant_id": "tid" }"#;
        let config: XeroSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.client_id, "cid");
        assert_eq!(resolved[0].1.tenant_id.as_deref(), Some("tid"));
    }

    #[test]
    fn resolve_named_entities() {
        let json = r#"{
            "default_entity": "harbor",
            "entities": {
                "harbor": { "client_id": "a", "client_secret": "b" },
                "beacon":      { "client_id": "c", "client_secret": "d", "tenant_id": "t" }
            }
        }"#;
        let config: XeroSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.default_entity.as_deref(), Some("harbor"));
        assert_eq!(config.resolve().len(), 2);
        assert_eq!(config.entities["beacon"].tenant_id.as_deref(), Some("t"));
    }

    #[test]
    fn resolve_combines_flat_and_named() {
        let json = r#"{
            "client_id": "flat", "client_secret": "flat",
            "entities": { "extra": { "client_id": "e", "client_secret": "e" } }
        }"#;
        let config: XeroSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.resolve().len(), 2);
    }

    #[test]
    fn flat_without_secret_yields_no_default() {
        let json = r#"{ "client_id": "cid" }"#;
        let config: XeroSkillConfig = serde_json::from_str(json).unwrap();
        assert!(config.resolve().is_empty());
    }

    #[test]
    fn clamp_page_size_bounds() {
        assert_eq!(clamp_page_size(None), DEFAULT_PAGE_SIZE);
        assert_eq!(clamp_page_size(Some(0)), DEFAULT_PAGE_SIZE);
        assert_eq!(clamp_page_size(Some(50)), 50);
        assert_eq!(clamp_page_size(Some(10_000)), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn clamp_page_floors_at_one() {
        assert_eq!(clamp_page(None), 1);
        assert_eq!(clamp_page(Some(0)), 1);
        assert_eq!(clamp_page(Some(5)), 5);
    }

    #[test]
    fn default_toml_includes_expected_keys() {
        let toml = XeroSkillConfig::default_toml();
        assert!(toml.contains("client_id"));
        assert!(toml.contains("client_secret"));
        assert!(toml.contains("entities.harbor"));
        assert!(toml.contains("default_entity"));
    }
}

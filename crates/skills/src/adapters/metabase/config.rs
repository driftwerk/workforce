//! Metabase skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Default row cap when a call supplies no explicit `limit`. Keeps casual
/// queries polite to the Metabase database; context safety is owned by the
/// artifact staging layer (results above the inline threshold never enter
/// the model's context).
pub const DEFAULT_MAX_QUERY_ROWS: u64 = 2000;

/// Hard ceiling on any explicit `limit` (and the default instance cap).
/// Large pulls land as staged tabular artifacts, so this bounds Metabase
/// DB load and artifact size, not context.
pub const STAGED_MAX_QUERY_ROWS: u64 = 50_000;

fn default_max_rows() -> u64 {
    STAGED_MAX_QUERY_ROWS
}

/// Configuration for a single Metabase instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetabaseInstanceConfig {
    /// Metabase API key (mb_...). Sent as `X-API-KEY`.
    pub api_key: String,
    /// Site URL (e.g. `https://mb.prod.example.com`). The `/api` path is appended by the client.
    /// Optional under the hub: a per-user site resolves per request.
    #[serde(default)]
    pub site_url: Option<String>,
    /// Cap on rows returned by query tools for this instance.
    #[serde(default = "default_max_rows")]
    pub max_query_rows: u64,
}

/// Configuration for the Metabase skill.
///
/// Supports both single-instance (backward compatible) and multi-instance modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetabaseSkillConfig {
    /// Single-instance API key (backward compatible).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Single-instance site URL.
    #[serde(default)]
    pub site_url: Option<String>,
    /// Single-instance row cap.
    #[serde(default = "default_max_rows")]
    pub max_query_rows: u64,
    /// Named instances for multi-instance mode.
    #[serde(default)]
    pub instances: HashMap<String, MetabaseInstanceConfig>,
    /// Which instance to use when `instance` param is omitted.
    #[serde(default)]
    pub default_instance: Option<String>,
}

impl MetabaseSkillConfig {
    /// Resolve all configured instances. If flat `api_key`/`site_url` are set, they become "default".
    pub fn resolve(&self) -> Vec<(String, MetabaseInstanceConfig)> {
        let mut result = Vec::new();
        if let Some(api_key) = self.api_key.as_ref() {
            result.push((
                "default".to_string(),
                MetabaseInstanceConfig {
                    api_key: api_key.clone(),
                    site_url: self.site_url.clone(),
                    max_query_rows: self.max_query_rows,
                },
            ));
        }
        for (name, inst) in &self.instances {
            result.push((name.clone(), inst.clone()));
        }
        result
    }

    /// Generate a default `config.toml` for the Metabase skill.
    pub fn default_toml() -> String {
        r#"# Metabase Integration Configuration
# Place this file at: {skills_dir}/metabase-integration/config.toml
#
# API keys use vault references: ${vault:metabase.api_key}
# Or environment variables: ${METABASE_API_KEY}

# ── Single-instance mode (backward compatible) ──
site_url = "${METABASE_URL}"
api_key  = "${METABASE_API_KEY}"

# Row cap for query tools (no-limit default 2000; explicit limits up to 50000)
# max_query_rows = 50000

# ── Multi-instance mode ──
# Uncomment to configure multiple Metabase instances. Tools accept an optional
# `instance` parameter to target a specific one.
#
# default_instance = "prod"
#
# [instances.prod]
# site_url       = "https://mb.prod.example.com"
# api_key        = "${METABASE_API_KEY_PROD}"
# # max_query_rows = 2000
#
# [instances.staging]
# site_url       = "https://mb.staging.example.com"
# api_key        = "${METABASE_API_KEY_STAGING}"
# # max_query_rows = 500
"#
        .to_string()
    }
}

impl Default for MetabaseSkillConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            site_url: None,
            max_query_rows: DEFAULT_MAX_QUERY_ROWS,
            instances: HashMap::new(),
            default_instance: None,
        }
    }
}

/// Clamp a caller-supplied `limit` to the effective row cap for an instance.
///
/// The instance cap is `max_rows` clamped into `[1, STAGED_MAX_QUERY_ROWS]`.
/// No `limit` (or 0) → the polite default ([`DEFAULT_MAX_QUERY_ROWS`], unless
/// the instance cap is smaller); an explicit `limit` is honored up to the
/// instance cap — large results stage as artifacts, so explicit big pulls are
/// context-safe.
pub fn clamp_limit(requested: Option<u64>, max_rows: u64) -> u64 {
    let cap = max_rows.clamp(1, STAGED_MAX_QUERY_ROWS);
    match requested {
        None | Some(0) => cap.min(DEFAULT_MAX_QUERY_ROWS),
        Some(n) => n.min(cap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal_single() {
        let json = r#"{ "api_key": "mb_test", "site_url": "https://mb.example.com" }"#;
        let config: MetabaseSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("mb_test"));
        assert_eq!(config.site_url.as_deref(), Some("https://mb.example.com"));
        assert_eq!(config.max_query_rows, STAGED_MAX_QUERY_ROWS);
        assert!(config.instances.is_empty());
    }

    #[test]
    fn config_deserializes_multi_instance() {
        let json = r#"{
            "default_instance": "prod",
            "instances": {
                "prod":    { "api_key": "mb_prod",    "site_url": "https://mb.prod" },
                "staging": { "api_key": "mb_staging", "site_url": "https://mb.staging", "max_query_rows": 500 }
            }
        }"#;
        let config: MetabaseSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.instances.len(), 2);
        assert_eq!(config.default_instance.as_deref(), Some("prod"));
        assert_eq!(config.instances["staging"].max_query_rows, 500);
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_flat_becomes_default() {
        let json = r#"{ "api_key": "mb_a", "site_url": "https://mb.a" }"#;
        let config: MetabaseSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "default");
        assert_eq!(resolved[0].1.api_key, "mb_a");
    }

    #[test]
    fn resolve_combined_flat_and_instances() {
        let json = r#"{
            "api_key":  "mb_flat",
            "site_url": "https://mb.flat",
            "instances": {
                "extra": { "api_key": "mb_extra", "site_url": "https://mb.extra" }
            }
        }"#;
        let config: MetabaseSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.resolve().len(), 2);
    }

    #[test]
    fn default_toml_includes_expected_keys() {
        let toml = MetabaseSkillConfig::default_toml();
        assert!(toml.contains("site_url"));
        assert!(toml.contains("api_key"));
        assert!(toml.contains("instances.prod"));
        assert!(toml.contains("default_instance"));
    }

    #[test]
    fn clamp_limit_enforces_caps() {
        // Explicit limits beyond the old 2000 ceiling are honored up to the
        // instance cap (results stage as artifacts, so context is safe).
        assert_eq!(clamp_limit(Some(10_000), 10_000), 10_000);
        assert_eq!(clamp_limit(Some(50_000), STAGED_MAX_QUERY_ROWS), 50_000);
        // …but never beyond the staged hard ceiling.
        assert_eq!(clamp_limit(Some(100_000), 100_000), STAGED_MAX_QUERY_ROWS);
        // Requested under cap returns requested.
        assert_eq!(clamp_limit(Some(100), 2000), 100);
        // No limit stays at the polite default, even on big-cap instances.
        assert_eq!(
            clamp_limit(None, STAGED_MAX_QUERY_ROWS),
            DEFAULT_MAX_QUERY_ROWS
        );
        // None uses the instance cap when it is smaller than the default.
        assert_eq!(clamp_limit(None, 500), 500);
        // Zero treated as "no limit".
        assert_eq!(clamp_limit(Some(0), 500), 500);
        // Per-instance max smaller than requested wins.
        assert_eq!(clamp_limit(Some(1000), 200), 200);
    }
}

use serde::{Deserialize, Serialize};

fn default_obscura_binary() -> String {
    "obscura".to_string()
}
fn default_startup_timeout_ms() -> u64 {
    10_000
}
fn default_call_timeout_ms() -> u64 {
    30_000
}
fn default_network_log_capacity() -> usize {
    256
}
fn default_console_log_capacity() -> usize {
    256
}

/// Configuration for the Obscura headless-browser skill.
///
/// One Obscura subprocess is launched per agent, lazily on first tool call,
/// in stdio-MCP mode. `obscura_binary`, when unset, falls back to PATH lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObscuraSkillConfig {
    #[serde(default = "default_obscura_binary")]
    pub obscura_binary: String,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u64,
    #[serde(default)]
    pub stealth: bool,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "default_network_log_capacity")]
    pub network_log_capacity: usize,
    #[serde(default = "default_console_log_capacity")]
    pub console_log_capacity: usize,
}

impl Default for ObscuraSkillConfig {
    fn default() -> Self {
        Self {
            obscura_binary: default_obscura_binary(),
            startup_timeout_ms: default_startup_timeout_ms(),
            call_timeout_ms: default_call_timeout_ms(),
            stealth: false,
            proxy: None,
            user_agent: None,
            network_log_capacity: default_network_log_capacity(),
            console_log_capacity: default_console_log_capacity(),
        }
    }
}

impl ObscuraSkillConfig {
    pub fn default_toml() -> String {
        r#"# Obscura skill — stdio MCP backend (no screenshot / PDF; lighter than browser)
obscura_binary = "obscura"
startup_timeout_ms = 10000
call_timeout_ms = 30000
stealth = false
# proxy = "http://proxy.example:8080"
# user_agent = "Mozilla/5.0 ..."
network_log_capacity = 256
console_log_capacity = 256
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_empty_object_to_defaults() {
        let cfg: ObscuraSkillConfig = serde_json::from_str("{}").unwrap();
        let def = ObscuraSkillConfig::default();
        assert_eq!(cfg.obscura_binary, def.obscura_binary);
        assert_eq!(cfg.startup_timeout_ms, def.startup_timeout_ms);
        assert_eq!(cfg.call_timeout_ms, def.call_timeout_ms);
        assert!(!cfg.stealth);
        assert_eq!(cfg.network_log_capacity, def.network_log_capacity);
    }

    #[test]
    fn deserialises_explicit_overrides() {
        let json = r#"{
            "obscura_binary": "/opt/obscura",
            "startup_timeout_ms": 5000,
            "call_timeout_ms": 7000,
            "stealth": true,
            "proxy": "http://p:8080",
            "user_agent": "ua",
            "network_log_capacity": 100,
            "console_log_capacity": 50
        }"#;
        let cfg: ObscuraSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.obscura_binary, "/opt/obscura");
        assert_eq!(cfg.startup_timeout_ms, 5000);
        assert_eq!(cfg.call_timeout_ms, 7000);
        assert!(cfg.stealth);
        assert_eq!(cfg.proxy.as_deref(), Some("http://p:8080"));
        assert_eq!(cfg.user_agent.as_deref(), Some("ua"));
        assert_eq!(cfg.network_log_capacity, 100);
        assert_eq!(cfg.console_log_capacity, 50);
    }

    #[test]
    fn default_toml_contains_documented_keys() {
        let toml = ObscuraSkillConfig::default_toml();
        for key in [
            "obscura_binary",
            "startup_timeout_ms",
            "call_timeout_ms",
            "stealth",
            "proxy",
            "user_agent",
            "network_log_capacity",
            "console_log_capacity",
        ] {
            assert!(toml.contains(key), "missing key in default_toml: {key}");
        }
    }
}

use serde::{Deserialize, Serialize};

fn default_launch_timeout_ms() -> u64 {
    20_000
}
fn default_call_timeout_ms() -> u64 {
    30_000
}
fn default_viewport_width() -> u32 {
    1280
}
fn default_viewport_height() -> u32 {
    720
}
fn default_network_log_capacity() -> usize {
    256
}
fn default_console_log_capacity() -> usize {
    256
}
fn default_max_live_sessions() -> usize {
    6
}
fn default_session_idle_ttl_ms() -> u64 {
    600_000
}
fn default_profile_retention_ms() -> u64 {
    604_800_000
}
fn default_reap_interval_ms() -> u64 {
    3_600_000
}

/// Longest a single `browser.wait` call may poll for a selector, whatever
/// `timeout_ms` asks for: a wait must return well inside the runner's check-in
/// interval, so a page that never renders the element cannot freeze a turn.
pub const MAX_WAIT_MS: u64 = 120_000;

/// Configuration for the headless-browser skill.
///
/// One Chromium subprocess is launched per (agent, user) identity, lazily on
/// first tool call, via chromiumoxide; each gets its own on-disk profile under
/// `profile_root` so login state never crosses users. `chrome_binary`, when
/// unset, lets chromiumoxide auto-detect Chrome/Chromium on PATH.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserSkillConfig {
    #[serde(default)]
    pub chrome_binary: Option<String>,
    #[serde(default = "default_launch_timeout_ms")]
    pub launch_timeout_ms: u64,
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u64,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "default_viewport_width")]
    pub viewport_width: u32,
    #[serde(default = "default_viewport_height")]
    pub viewport_height: u32,
    #[serde(default = "default_network_log_capacity")]
    pub network_log_capacity: usize,
    #[serde(default = "default_console_log_capacity")]
    pub console_log_capacity: usize,
    #[serde(default)]
    pub profile_root: Option<String>,
    #[serde(default = "default_max_live_sessions")]
    pub max_live_sessions: usize,
    #[serde(default = "default_session_idle_ttl_ms")]
    pub session_idle_ttl_ms: u64,
    #[serde(default = "default_profile_retention_ms")]
    pub profile_retention_ms: u64,
    #[serde(default = "default_reap_interval_ms")]
    pub reap_interval_ms: u64,
}

impl Default for BrowserSkillConfig {
    fn default() -> Self {
        Self {
            chrome_binary: None,
            launch_timeout_ms: default_launch_timeout_ms(),
            call_timeout_ms: default_call_timeout_ms(),
            user_agent: None,
            viewport_width: default_viewport_width(),
            viewport_height: default_viewport_height(),
            network_log_capacity: default_network_log_capacity(),
            console_log_capacity: default_console_log_capacity(),
            profile_root: None,
            max_live_sessions: default_max_live_sessions(),
            session_idle_ttl_ms: default_session_idle_ttl_ms(),
            profile_retention_ms: default_profile_retention_ms(),
            reap_interval_ms: default_reap_interval_ms(),
        }
    }
}

impl BrowserSkillConfig {
    pub fn default_toml() -> String {
        r#"# Browser skill — headless Chromium driven via the Chrome DevTools Protocol
# chrome_binary = "/usr/bin/chromium"   # auto-detected on PATH if omitted
launch_timeout_ms = 20000
call_timeout_ms = 30000
# user_agent = "Mozilla/5.0 ..."
viewport_width = 1280
viewport_height = 720
network_log_capacity = 256
console_log_capacity = 256
# profile_root = "/var/lib/workforce/browser"   # per-user profiles; defaults under the system temp dir
max_live_sessions = 6
session_idle_ttl_ms = 600000
profile_retention_ms = 604800000
reap_interval_ms = 3600000
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_empty_object_to_defaults() {
        let cfg: BrowserSkillConfig = serde_json::from_str("{}").unwrap();
        let def = BrowserSkillConfig::default();
        assert_eq!(cfg.launch_timeout_ms, def.launch_timeout_ms);
        assert_eq!(cfg.call_timeout_ms, def.call_timeout_ms);
        assert_eq!(cfg.viewport_width, def.viewport_width);
        assert_eq!(cfg.network_log_capacity, def.network_log_capacity);
        assert!(cfg.chrome_binary.is_none());
        assert!(cfg.profile_root.is_none());
        assert_eq!(cfg.max_live_sessions, def.max_live_sessions);
        assert_eq!(cfg.session_idle_ttl_ms, def.session_idle_ttl_ms);
        assert_eq!(cfg.profile_retention_ms, def.profile_retention_ms);
        assert_eq!(cfg.reap_interval_ms, def.reap_interval_ms);
    }

    #[test]
    fn deserialises_explicit_overrides() {
        let json = r#"{
            "chrome_binary": "/usr/bin/chromium",
            "launch_timeout_ms": 5000,
            "call_timeout_ms": 7000,
            "user_agent": "ua",
            "viewport_width": 1920,
            "viewport_height": 1080,
            "network_log_capacity": 100,
            "console_log_capacity": 50,
            "profile_root": "/tmp/wf-browser",
            "max_live_sessions": 3,
            "session_idle_ttl_ms": 1000,
            "profile_retention_ms": 2000,
            "reap_interval_ms": 3000
        }"#;
        let cfg: BrowserSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.chrome_binary.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(cfg.launch_timeout_ms, 5000);
        assert_eq!(cfg.call_timeout_ms, 7000);
        assert_eq!(cfg.user_agent.as_deref(), Some("ua"));
        assert_eq!(cfg.viewport_width, 1920);
        assert_eq!(cfg.viewport_height, 1080);
        assert_eq!(cfg.network_log_capacity, 100);
        assert_eq!(cfg.console_log_capacity, 50);
        assert_eq!(cfg.profile_root.as_deref(), Some("/tmp/wf-browser"));
        assert_eq!(cfg.max_live_sessions, 3);
        assert_eq!(cfg.session_idle_ttl_ms, 1000);
        assert_eq!(cfg.profile_retention_ms, 2000);
        assert_eq!(cfg.reap_interval_ms, 3000);
    }

    #[test]
    fn default_toml_contains_documented_keys() {
        let toml = BrowserSkillConfig::default_toml();
        for key in [
            "chrome_binary",
            "launch_timeout_ms",
            "call_timeout_ms",
            "user_agent",
            "viewport_width",
            "viewport_height",
            "network_log_capacity",
            "console_log_capacity",
            "profile_root",
            "max_live_sessions",
            "session_idle_ttl_ms",
            "profile_retention_ms",
            "reap_interval_ms",
        ] {
            assert!(toml.contains(key), "missing key in default_toml: {key}");
        }
    }
}

use serde::{Deserialize, Serialize};

fn default_call_timeout_ms() -> u64 {
    30_000
}
fn default_session_idle_ttl_ms() -> u64 {
    600_000
}

/// Longest a single `browser.wait` call may poll for a selector, whatever
/// `timeout_ms` asks for: a wait must return well inside the runner's check-in
/// interval, so a page that never renders the element cannot freeze a turn.
pub const MAX_WAIT_MS: u64 = 120_000;

/// The browser skill, as `[skills.browser]` sets it. Desk browses in the
/// person's own Chrome, through Desk's extension; nothing here launches a
/// browser. Keys from the old Chromium skill (`chrome_binary`,
/// `profile_root`, `show` …) are read and ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserSkillConfig {
    /// How long one call in the person's Chrome may take.
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u64,
    /// How long an agent's tab stays open without a call before it closes.
    #[serde(default = "default_session_idle_ttl_ms")]
    pub session_idle_ttl_ms: u64,
}

impl Default for BrowserSkillConfig {
    fn default() -> Self {
        Self {
            call_timeout_ms: default_call_timeout_ms(),
            session_idle_ttl_ms: default_session_idle_ttl_ms(),
        }
    }
}

impl BrowserSkillConfig {
    pub fn default_toml() -> String {
        r#"# Browser skill: the person's own Chrome, through Desk's Chrome extension
call_timeout_ms = 30000
session_idle_ttl_ms = 600000
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_empty_object_to_defaults_and_ignores_the_old_chromium_keys() {
        let cfg: BrowserSkillConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.call_timeout_ms, 30_000);
        assert_eq!(cfg.session_idle_ttl_ms, 600_000);
        let old: BrowserSkillConfig = serde_json::from_str(
            r#"{ "chrome_binary": "/usr/bin/chromium", "profile_root": "/tmp/x", "show": true, "call_timeout_ms": 7000 }"#,
        )
        .unwrap();
        assert_eq!(old.call_timeout_ms, 7000);
    }

    #[test]
    fn default_toml_reads_back() {
        let cfg: BrowserSkillConfig = toml::from_str(&BrowserSkillConfig::default_toml()).unwrap();
        assert_eq!(cfg.call_timeout_ms, 30_000);
    }
}

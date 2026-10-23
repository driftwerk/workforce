//! Email skill configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    587
}
fn default_tls() -> bool {
    true
}
fn default_connect_timeout_secs() -> u16 {
    30
}
fn default_command_timeout_secs() -> u16 {
    60
}

/// Configuration for a single email account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAccountConfig {
    /// IMAP server hostname.
    pub imap_host: String,
    /// IMAP server port (default: 993).
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// SMTP server hostname.
    pub smtp_host: String,
    /// SMTP server port (default: 587).
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Account username / email address.
    pub username: String,
    /// Account password or app-specific password.
    pub password: String,
    /// Display name for outgoing mail.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Use TLS for connections (default: true).
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Connect timeout in seconds (default: 30).
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u16,
    /// Command timeout in seconds (default: 60).
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u16,
}

/// Configuration for the email skill (multi-account).
///
/// Supports multiple named email accounts with a configurable default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSkillConfig {
    /// Named email accounts.
    #[serde(default)]
    pub accounts: HashMap<String, EmailAccountConfig>,
    /// Default account name when none specified.
    #[serde(default)]
    pub default_account: Option<String>,
}

impl EmailSkillConfig {
    /// Resolve all configured accounts as (name, config) pairs.
    pub fn resolve(&self) -> Vec<(String, &EmailAccountConfig)> {
        self.accounts.iter().map(|(k, v)| (k.clone(), v)).collect()
    }

    /// Get the default account name, falling back to "default".
    pub fn default_account_name(&self) -> String {
        self.default_account
            .clone()
            .unwrap_or_else(|| "default".to_string())
    }

    /// Generate a default TOML template showing the vault.set pattern.
    pub fn default_toml() -> String {
        r#"# Email Skill Configuration
# Store credentials in the vault using:
#
# For each email account:
#   vault.set('skills/email/accounts/<name>/username', '<email>')
#   vault.set('skills/email/accounts/<name>/password', '<password>')
#   vault.set('skills/email/accounts/<name>/imap_host', '<host>')
#   vault.set('skills/email/accounts/<name>/smtp_host', '<host>')
#
# Optional (have defaults):
#   vault.set('skills/email/accounts/<name>/imap_port', '993')
#   vault.set('skills/email/accounts/<name>/smtp_port', '587')
#   vault.set('skills/email/accounts/<name>/tls', 'true')
#   vault.set('skills/email/accounts/<name>/display_name', 'My Name')
#   vault.set('skills/email/accounts/<name>/connect_timeout_secs', '30')
#   vault.set('skills/email/accounts/<name>/command_timeout_secs', '60')
#
# Set default account:
#   vault.set('skills/email/default_account', '<name>')
#
# After adding credentials, call system.enable_skill('email') to load.
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_minimal() {
        let json = r#"{
            "imap_host": "imap.example.com",
            "smtp_host": "smtp.example.com",
            "username": "user@example.com",
            "password": "secret"
        }"#;
        let config: EmailAccountConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.imap_host, "imap.example.com");
        assert_eq!(config.smtp_host, "smtp.example.com");
        assert_eq!(config.username, "user@example.com");
        assert_eq!(config.password, "secret");
        assert_eq!(config.imap_port, 993);
        assert_eq!(config.smtp_port, 587);
        assert!(config.tls);
        assert!(config.display_name.is_none());
    }

    #[test]
    fn config_deserializes_full() {
        let json = r#"{
            "imap_host": "imap.gmail.com",
            "imap_port": 995,
            "smtp_host": "smtp.gmail.com",
            "smtp_port": 465,
            "username": "user@gmail.com",
            "password": "app-password",
            "display_name": "Test User",
            "tls": false
        }"#;
        let config: EmailAccountConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.imap_host, "imap.gmail.com");
        assert_eq!(config.imap_port, 995);
        assert_eq!(config.smtp_host, "smtp.gmail.com");
        assert_eq!(config.smtp_port, 465);
        assert_eq!(config.username, "user@gmail.com");
        assert_eq!(config.password, "app-password");
        assert_eq!(config.display_name.as_deref(), Some("Test User"));
        assert!(!config.tls);
    }

    #[test]
    fn config_multi_account() {
        let json = r#"{
            "accounts": {
                "work": {
                    "imap_host": "imap.work.com",
                    "smtp_host": "smtp.work.com",
                    "username": "me@work.com",
                    "password": "work-pass"
                },
                "personal": {
                    "imap_host": "imap.personal.com",
                    "smtp_host": "smtp.personal.com",
                    "username": "me@personal.com",
                    "password": "personal-pass"
                }
            },
            "default_account": "work"
        }"#;
        let config: EmailSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.default_account.as_deref(), Some("work"));
    }

    #[test]
    fn resolve_returns_all_accounts() {
        let json = r#"{
            "accounts": {
                "a": {
                    "imap_host": "imap.a.com",
                    "smtp_host": "smtp.a.com",
                    "username": "a@a.com",
                    "password": "pa"
                },
                "b": {
                    "imap_host": "imap.b.com",
                    "smtp_host": "smtp.b.com",
                    "username": "b@b.com",
                    "password": "pb"
                }
            }
        }"#;
        let config: EmailSkillConfig = serde_json::from_str(json).unwrap();
        let resolved = config.resolve();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn default_account_name_fallback() {
        let json = r#"{ "accounts": {} }"#;
        let config: EmailSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.default_account_name(), "default");
    }

    #[test]
    fn default_account_name_configured() {
        let json = r#"{ "accounts": {}, "default_account": "work" }"#;
        let config: EmailSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.default_account_name(), "work");
    }

    #[test]
    fn default_toml_not_empty() {
        let toml = EmailSkillConfig::default_toml();
        assert!(toml.contains("vault.set"));
        assert!(toml.contains("username"));
        assert!(toml.contains("password"));
        assert!(toml.contains("imap_host"));
        assert!(toml.contains("smtp_host"));
    }

    #[test]
    fn default_values_correct() {
        let json = r#"{
            "imap_host": "h",
            "smtp_host": "h",
            "username": "u",
            "password": "p"
        }"#;
        let config: EmailAccountConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.imap_port, 993);
        assert_eq!(config.smtp_port, 587);
        assert!(config.tls);
    }
}

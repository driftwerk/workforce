/// Configuration for the sentinel security system.
///
/// Loaded from the `[sentinel]` section of `workforce.toml`.
/// Used by the trust classification system to identify owner-level users.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct SentinelConfig {
    /// Owner Slack user IDs (e.g. `["U12345"]`).
    #[serde(default)]
    pub owner_slack_ids: Vec<String>,
    /// Owner device IDs for gateway authentication.
    #[serde(default)]
    pub owner_device_ids: Vec<String>,
    /// Gating of Slack senders from outside the bot's home workspace.
    #[serde(default)]
    pub external_users: ExternalUsersConfig,
}

/// `[sentinel.external_users]`: how to treat Slack senders from outside the
/// bot's home workspace (Slack Connect / shared-channel users).
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ExternalUsersConfig {
    /// Whether to allow Slack senders from outside the bot's home workspace.
    /// `false` (default) blocks them; flip to `true` if cross-org access is ever
    /// wanted. A per-agent `allow_external_users` overrides this.
    #[serde(default)]
    pub allow: bool,
    /// Message sent to a denied external sender. `None` denies silently.
    #[serde(default)]
    pub deny_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_users_defaults_to_deny() {
        let cfg = SentinelConfig::default();
        assert!(!cfg.external_users.allow);
        assert!(cfg.external_users.deny_message.is_none());
    }

    #[test]
    fn external_users_parses() {
        let toml = r#"
            [external_users]
            allow = true
            deny_message = "Sorry, this assistant is internal-only."
        "#;
        let cfg: SentinelConfig = toml::from_str(toml).unwrap();
        assert!(cfg.external_users.allow);
        assert_eq!(
            cfg.external_users.deny_message.as_deref(),
            Some("Sorry, this assistant is internal-only.")
        );
    }
}

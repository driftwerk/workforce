//! Multi-account IMAP connection manager.
//!
//! Holds configurations for multiple email accounts and lazily establishes
//! connections on first use. Connections are reused across operations.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::{debug, info};
use workforce_common::settings::SecretStore;

use crate::adapters::auth::{companion_at, resolve_at};

use super::client::ImapClient;
use super::config::{EmailAccountConfig, EmailSkillConfig};
use super::error::EmailError;

/// Multi-account IMAP connection manager.
///
/// Stores account configurations and lazily creates `ImapClient` connections
/// on first access. Connections are cached and reused — each call to `get()`
/// returns the existing client (with a health check) or creates a new one.
///
/// When a [`SecretStore`] is provided, credentials are re-resolved on each
/// `get()` call, enabling per-user credential resolution. If the resolved
/// credentials differ from the cached connection, the old connection is
/// dropped and a new one is established.
pub struct ImapConnector {
    configs: HashMap<String, EmailAccountConfig>,
    /// Active clients keyed by (account_name, username) for multi-user support.
    clients: HashMap<(String, String), ImapClient>,
    default_account: String,
    store: Option<Arc<dyn SecretStore>>,
}

impl ImapConnector {
    pub fn new(config: &EmailSkillConfig) -> Self {
        let default_account = config.default_account_name();
        info!(
            accounts = config.accounts.len(),
            default = %default_account,
            "ImapConnector created (connections are lazy)"
        );
        Self {
            configs: config.accounts.clone(),
            clients: HashMap::new(),
            default_account,
            store: None,
        }
    }

    /// Create a connector with a `SecretStore` for per-user credential resolution.
    pub fn with_store(config: &EmailSkillConfig, store: Arc<dyn SecretStore>) -> Self {
        let mut connector = Self::new(config);
        connector.store = Some(store);
        connector
    }

    /// The load-time snapshot is never consulted in store mode: it was baked
    /// under the loading user's scope.
    fn resolve_config(&self, name: &str) -> Option<EmailAccountConfig> {
        let Some(store) = self.store.as_ref() else {
            return self.configs.get(name).cloned();
        };
        let path = |field: &str| format!("skills/email/accounts/{name}/{field}");
        let password = resolve_at(store.as_ref(), &path("password"))?;
        let field = |field: &str| companion_at(store.as_ref(), password.per_user, &path(field));
        let parsed =
            |name: &str, default| field(name).and_then(|v| v.parse().ok()).unwrap_or(default);
        let username = field("username")?;
        let imap_host = field("imap_host")?;
        let smtp_host = field("smtp_host").unwrap_or_else(|| imap_host.clone());
        let display_name = field("display_name");
        let tls = field("tls").is_none_or(|v| v != "false" && v != "0");
        let (imap_port, smtp_port) = (parsed("imap_port", 993), parsed("smtp_port", 587));
        let (connect_timeout_secs, command_timeout_secs) = (
            parsed("connect_timeout_secs", 30),
            parsed("command_timeout_secs", 60),
        );
        Some(EmailAccountConfig {
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            username,
            password: password.value,
            display_name,
            tls,
            connect_timeout_secs,
            command_timeout_secs,
        })
    }

    /// Store-backed, per-user-safe account config for the SMTP send/forward
    /// paths. Resolves under the current `CURRENT_USER_ID` scope (see
    /// [`Self::resolve_config`]) instead of a load-time-baked snapshot, so one
    /// user's credentials can't be used by another on a shared agent.
    pub fn resolve_account_config(&self, name: &str) -> Option<EmailAccountConfig> {
        self.resolve_config(name)
    }

    /// Account names visible to the CURRENT caller, listed live from the store
    /// (`skills/email/accounts/<name>/...`) under `CURRENT_USER_ID`. Use this for
    /// any caller-facing enumeration (account listing, "all accounts"
    /// fetch/search): the baked [`Self::account_names`] reflects the LOADING user
    /// on a shared agent and would leak their account set to other users.
    pub fn resolve_account_names(&self) -> Vec<String> {
        if let Some(ref store) = self.store {
            let prefix = "skills/email/accounts/";
            let mut names: Vec<String> = store
                .list(Some(prefix))
                .into_iter()
                .filter_map(|p| {
                    p.strip_prefix(prefix)
                        .and_then(|rest| rest.split('/').next())
                        .map(str::to_string)
                })
                .collect();
            names.sort();
            names.dedup();
            return names;
        }
        // Static mode: process-config accounts, shared by definition.
        let mut names: Vec<String> = self.configs.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get a connected, health-checked IMAP client for the given account.
    ///
    /// If `account` is `None`, uses the default account. Credentials are
    /// re-resolved from the store (if available) on each call to support
    /// per-user credential switching.
    pub async fn get(&mut self, account: Option<&str>) -> Result<&mut ImapClient, EmailError> {
        let name = account.unwrap_or(&self.default_account).to_string();

        let config = self
            .resolve_config(&name)
            .ok_or_else(|| EmailError::AccountNotFound { name: name.clone() })?;

        let cache_key = (name, config.username.clone());

        if !self.clients.contains_key(&cache_key) {
            debug!(account = %cache_key.0, user = %cache_key.1, "Connecting to IMAP account");
            let client = ImapClient::connect(config).await?;
            self.clients.insert(cache_key.clone(), client);
        } else {
            let client = self.clients.get_mut(&cache_key).unwrap();
            client.ensure_connected().await?;
        }

        Ok(self.clients.get_mut(&cache_key).unwrap())
    }

    /// List all configured account names.
    pub fn account_names(&self) -> Vec<&str> {
        self.configs.keys().map(|s| s.as_str()).collect()
    }

    /// Returns the default account name.
    pub fn default_account(&self) -> &str {
        &self.default_account
    }

    /// Returns the number of configured accounts.
    pub fn account_count(&self) -> usize {
        self.configs.len()
    }

    /// Returns the number of active (connected) clients.
    pub fn connected_count(&self) -> usize {
        self.clients.len()
    }

    /// Disconnect all active sessions gracefully.
    pub async fn disconnect_all(&mut self) {
        for ((account, user), mut client) in self.clients.drain() {
            debug!(account = %account, user = %user, "Disconnecting IMAP client");
            client.disconnect().await;
        }
    }

    /// Check if a client is currently connected without triggering a lazy connect.
    pub fn is_connected(&self, name: &str) -> bool {
        self.clients
            .iter()
            .any(|((n, _), c)| n == name && c.is_connected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_skill_config() -> EmailSkillConfig {
        let mut accounts = HashMap::new();
        accounts.insert(
            "work".into(),
            EmailAccountConfig {
                imap_host: "imap.work.com".into(),
                imap_port: 993,
                smtp_host: "smtp.work.com".into(),
                smtp_port: 587,
                username: "me@work.com".into(),
                password: "work-pass".into(),
                display_name: Some("Work Me".into()),
                tls: true,
                connect_timeout_secs: 30,
                command_timeout_secs: 60,
            },
        );
        accounts.insert(
            "personal".into(),
            EmailAccountConfig {
                imap_host: "imap.personal.com".into(),
                imap_port: 993,
                smtp_host: "smtp.personal.com".into(),
                smtp_port: 587,
                username: "me@personal.com".into(),
                password: "personal-pass".into(),
                display_name: None,
                tls: true,
                connect_timeout_secs: 15,
                command_timeout_secs: 30,
            },
        );
        EmailSkillConfig {
            accounts,
            default_account: Some("work".into()),
        }
    }

    #[test]
    fn connector_new_creates_no_connections() {
        let config = test_skill_config();
        let connector = ImapConnector::new(&config);
        assert_eq!(connector.account_count(), 2);
        assert_eq!(connector.connected_count(), 0);
    }

    #[test]
    fn connector_resolves_default_account() {
        let config = test_skill_config();
        let connector = ImapConnector::new(&config);
        assert_eq!(connector.default_account(), "work");
    }

    #[test]
    fn connector_resolves_default_fallback() {
        let config = EmailSkillConfig {
            accounts: HashMap::new(),
            default_account: None,
        };
        let connector = ImapConnector::new(&config);
        assert_eq!(connector.default_account(), "default");
    }

    #[tokio::test]
    async fn connector_rejects_unknown_account() {
        let config = test_skill_config();
        let mut connector = ImapConnector::new(&config);
        let result = connector.get(Some("nonexistent")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            EmailError::AccountNotFound { name } => assert_eq!(name, "nonexistent"),
            other => panic!("Expected AccountNotFound, got: {other}"),
        }
    }

    #[test]
    fn connector_lists_account_names() {
        let config = test_skill_config();
        let connector = ImapConnector::new(&config);
        let mut names = connector.account_names();
        names.sort();
        assert_eq!(names, vec!["personal", "work"]);
    }

    /// Regression: SMTP/forward paths resolve account config through the store,
    /// NOT the load-time-baked `self.configs`. On a shared agent, user A enabling
    /// email bakes A's account into `self.configs`; without store-only resolution
    /// user B's `email.send` reused A's SMTP username+password. So in store mode
    /// a baked account must resolve ONLY when the store (the caller's scoped view)
    /// also has it.
    #[test]
    fn resolve_account_config_does_not_fall_back_to_baked_in_store_mode() {
        use crate::adapters::test_support::InMem;

        // Baked configs hold "work"/"personal" (as if user A loaded them); the
        // store (the current caller's scoped view) holds only a shared account.
        let config = test_skill_config();
        let store = InMem::from(&[
            ("skills/email/accounts/shared/username", "shared@co.com"),
            ("skills/email/accounts/shared/password", "shared-pw"),
            ("skills/email/accounts/shared/imap_host", "imap.co.com"),
        ]);
        let connector = ImapConnector::with_store(&config, store);

        // Baked-but-not-in-store account must NOT leak A's credentials.
        assert!(
            connector.resolve_account_config("work").is_none(),
            "store mode must not serve the load-time-baked 'work' account"
        );
        // Store-resolved account works and carries the store's credentials.
        let resolved = connector
            .resolve_account_config("shared")
            .expect("shared account resolves from the store");
        assert_eq!(resolved.username, "shared@co.com");
        assert_eq!(resolved.password, "shared-pw");
    }

    #[test]
    fn a_per_user_password_pairs_with_per_user_hosts_only() {
        use crate::adapters::test_support::LayeredInMem;

        let store = LayeredInMem::new(
            &[
                ("skills/email/accounts/co/username", "shared@co.com"),
                ("skills/email/accounts/co/password", "shared-pw"),
                ("skills/email/accounts/co/imap_host", "imap.co.com"),
                ("skills/email/accounts/co/smtp_host", "smtp.co.com"),
            ],
            &[
                (
                    "skills/email/accounts/co/imap_host",
                    "imap.attacker.example",
                ),
                (
                    "skills/email/accounts/co/smtp_host",
                    "smtp.attacker.example",
                ),
                ("skills/email/accounts/co/smtp_port", "2525"),
            ],
        );
        let shared = ImapConnector::with_store(&test_skill_config(), store)
            .resolve_account_config("co")
            .expect("shared account resolves");
        assert_eq!(shared.password, "shared-pw");
        assert_eq!(shared.imap_host, "imap.co.com");
        assert_eq!(shared.smtp_host, "smtp.co.com");
        assert_eq!(shared.smtp_port, 587);

        let store = LayeredInMem::new(
            &[
                ("skills/email/accounts/co/username", "shared@co.com"),
                ("skills/email/accounts/co/password", "shared-pw"),
                ("skills/email/accounts/co/imap_host", "imap.co.com"),
            ],
            &[
                ("skills/email/accounts/co/password", "my-pw"),
                ("skills/email/accounts/co/username", "me@co.com"),
                ("skills/email/accounts/co/imap_host", "imap.me.example"),
                ("skills/email/accounts/co/tls", "false"),
            ],
        );
        let mine = ImapConnector::with_store(&test_skill_config(), store)
            .resolve_account_config("co")
            .expect("per-user account resolves");
        assert_eq!(mine.password, "my-pw");
        assert_eq!(mine.username, "me@co.com");
        assert_eq!(mine.imap_host, "imap.me.example");
        assert_eq!(mine.smtp_host, "imap.me.example");
        assert!(!mine.tls);

        let store = LayeredInMem::new(
            &[
                ("skills/email/accounts/co/username", "shared@co.com"),
                ("skills/email/accounts/co/imap_host", "imap.co.com"),
            ],
            &[("skills/email/accounts/co/password", "my-pw")],
        );
        let fallback = ImapConnector::with_store(&test_skill_config(), store)
            .resolve_account_config("co")
            .expect("per-user password with shared hosts resolves");
        assert_eq!(fallback.password, "my-pw");
        assert_eq!(fallback.username, "shared@co.com");
        assert_eq!(fallback.imap_host, "imap.co.com");
    }

    /// Static mode (no store) keeps serving process-config accounts unchanged.
    #[test]
    fn resolve_account_config_uses_baked_config_in_static_mode() {
        let connector = ImapConnector::new(&test_skill_config());
        let work = connector
            .resolve_account_config("work")
            .expect("static-mode account resolves from process config");
        assert_eq!(work.username, "me@work.com");
    }
}

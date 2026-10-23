//! Async IMAP client wrapper.
//!
//! Provides a thin wrapper around `async-imap` + `async-native-tls` with:
//! - Implicit TLS (port 993) and STARTTLS support
//! - LOGIN authentication
//! - Connection health checking via NOOP
//! - Automatic reconnection on stale/dropped sessions
//! - Configurable connect and command timeouts
//!
//! Follows the `GithubClient` pattern — a protocol wrapper used by
//! `EmailSkill`, with `Arc` references passed to tools.

use std::time::Duration;

use async_imap::Session;
use async_native_tls::TlsConnector;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use super::config::EmailAccountConfig;
use super::error::EmailError;

/// Type alias for an IMAP session over a TLS stream.
pub type ImapSession = Session<async_native_tls::TlsStream<TcpStream>>;

/// Type alias for an IMAP session over a plain TCP stream (no TLS).
pub type ImapSessionPlain = Session<TcpStream>;

/// The TLS mode to use for the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Implicit TLS — connect directly over TLS (typical on port 993).
    Implicit,
    /// STARTTLS — connect plain, then upgrade to TLS.
    StartTls,
    /// No TLS — plain TCP connection.
    None,
}

impl TlsMode {
    /// Determine the TLS mode from config.
    pub fn from_config(config: &EmailAccountConfig) -> Self {
        if !config.tls {
            Self::None
        } else if config.imap_port == 993 {
            Self::Implicit
        } else {
            Self::StartTls
        }
    }
}

/// Wrapper around the two possible session types (TLS vs plain).
enum SessionInner {
    /// TLS-secured session (implicit TLS or STARTTLS).
    Tls(ImapSession),
    /// Plain TCP session (no TLS). Discouraged for production use.
    Plain(ImapSessionPlain),
}

/// Async IMAP client with connection lifecycle management.
///
/// Wraps a single IMAP account's connection. Handles TLS negotiation,
/// authentication, health checking, and automatic reconnection.
pub struct ImapClient {
    config: EmailAccountConfig,
    session: Option<SessionInner>,
    connect_timeout: Duration,
    command_timeout: Duration,
}

impl std::fmt::Debug for ImapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapClient")
            .field("host", &self.config.imap_host)
            .field("port", &self.config.imap_port)
            .field("account", &self.config.username)
            .field("connected", &self.session.is_some())
            .finish()
    }
}

impl ImapClient {
    /// Connect to an IMAP server using the provided account config.
    ///
    /// Establishes TCP connection, negotiates TLS if configured,
    /// and authenticates via LOGIN. All steps are subject to timeouts.
    pub async fn connect(config: EmailAccountConfig) -> Result<Self, EmailError> {
        let connect_timeout = Duration::from_secs(config.connect_timeout_secs as u64);
        let command_timeout = Duration::from_secs(config.command_timeout_secs as u64);

        let mut client = Self {
            config,
            session: None,
            connect_timeout,
            command_timeout,
        };

        client.establish_session().await?;
        Ok(client)
    }

    /// Establish (or re-establish) the IMAP session.
    async fn establish_session(&mut self) -> Result<(), EmailError> {
        let host = self.config.imap_host.clone();
        let port = self.config.imap_port;
        let addr = format!("{host}:{port}");
        let tls_mode = TlsMode::from_config(&self.config);

        debug!(%host, port, ?tls_mode, "Connecting to IMAP server");

        // Step 1: TCP connect with timeout
        let tcp_stream = tokio::time::timeout(self.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| EmailError::Timeout {
                operation: "connect".into(),
                duration: self.connect_timeout,
            })?
            .map_err(|_e| EmailError::ConnectionRefused {
                host: host.clone(),
                port,
            })?;

        debug!("TCP connected to {addr}");

        match tls_mode {
            TlsMode::Implicit => {
                self.connect_implicit_tls(tcp_stream, &host).await?;
            }
            TlsMode::StartTls => {
                self.connect_starttls(tcp_stream, &host).await?;
            }
            TlsMode::None => {
                self.connect_plain(tcp_stream).await?;
            }
        }

        info!(
            %host,
            port,
            account = %self.config.username,
            "IMAP session established"
        );
        Ok(())
    }

    /// Connect with implicit TLS (port 993 style).
    async fn connect_implicit_tls(
        &mut self,
        tcp_stream: TcpStream,
        host: &str,
    ) -> Result<(), EmailError> {
        let tls_connector = TlsConnector::new();

        let tls_stream = tokio::time::timeout(
            self.connect_timeout,
            tls_connector.connect(host, tcp_stream),
        )
        .await
        .map_err(|_| EmailError::Timeout {
            operation: "TLS handshake".into(),
            duration: self.connect_timeout,
        })?
        .map_err(|e| EmailError::TlsHandshake {
            host: host.to_string(),
            detail: e.to_string(),
        })?;

        debug!("TLS handshake completed (implicit)");

        let mut imap_client = async_imap::Client::new(tls_stream);

        // Read server greeting
        let _greeting = tokio::time::timeout(self.command_timeout, imap_client.read_response())
            .await
            .map_err(|_| EmailError::Timeout {
                operation: "read greeting".into(),
                duration: self.command_timeout,
            })?;

        debug!("IMAP greeting received");

        // Login
        let username = self.config.username.clone();
        let password = self.config.password.clone();
        let session = tokio::time::timeout(
            self.command_timeout,
            imap_client.login(&username, &password),
        )
        .await
        .map_err(|_| EmailError::Timeout {
            operation: "login".into(),
            duration: self.command_timeout,
        })?
        .map_err(|(e, _client)| {
            let msg = e.to_string().to_lowercase();
            if msg.contains("authentication")
                || msg.contains("login")
                || msg.contains("credentials")
                || msg.contains("invalid")
            {
                EmailError::AuthFailed {
                    account: username.clone(),
                }
            } else {
                EmailError::Imap(e.to_string())
            }
        })?;

        debug!("IMAP login successful (TLS)");
        self.session = Some(SessionInner::Tls(session));
        Ok(())
    }

    /// Connect with STARTTLS upgrade.
    ///
    /// Following the async-imap docs:
    /// 1. Create plain client, read greeting
    /// 2. Issue STARTTLS command
    /// 3. Extract inner stream and TLS-wrap it
    /// 4. Create new Client on the TLS stream
    /// 5. Login
    async fn connect_starttls(
        &mut self,
        tcp_stream: TcpStream,
        host: &str,
    ) -> Result<(), EmailError> {
        // Create plain IMAP client
        let mut imap_client = async_imap::Client::new(tcp_stream);

        // Read server greeting
        let _greeting = tokio::time::timeout(self.command_timeout, imap_client.read_response())
            .await
            .map_err(|_| EmailError::Timeout {
                operation: "read greeting".into(),
                duration: self.command_timeout,
            })?;

        debug!("IMAP greeting received (pre-STARTTLS)");

        // Issue STARTTLS command (via Connection deref, needs unsolicited sender)
        tokio::time::timeout(
            self.command_timeout,
            imap_client.run_command_and_check_ok("STARTTLS", None),
        )
        .await
        .map_err(|_| EmailError::Timeout {
            operation: "STARTTLS".into(),
            duration: self.command_timeout,
        })?
        .map_err(|e| EmailError::TlsHandshake {
            host: host.to_string(),
            detail: format!("STARTTLS command failed: {e}"),
        })?;

        debug!("STARTTLS command accepted");

        // Extract inner TCP stream and TLS-wrap it
        let inner_stream = imap_client.into_inner();
        let tls_connector = TlsConnector::new();

        let tls_stream = tokio::time::timeout(
            self.connect_timeout,
            tls_connector.connect(host, inner_stream),
        )
        .await
        .map_err(|_| EmailError::Timeout {
            operation: "TLS handshake (STARTTLS)".into(),
            duration: self.connect_timeout,
        })?
        .map_err(|e| EmailError::TlsHandshake {
            host: host.to_string(),
            detail: e.to_string(),
        })?;

        debug!("STARTTLS TLS upgrade completed");

        // Create new client on TLS stream — no greeting after STARTTLS
        let tls_client = async_imap::Client::new(tls_stream);

        // Login
        let username = self.config.username.clone();
        let password = self.config.password.clone();
        let session =
            tokio::time::timeout(self.command_timeout, tls_client.login(&username, &password))
                .await
                .map_err(|_| EmailError::Timeout {
                    operation: "login".into(),
                    duration: self.command_timeout,
                })?
                .map_err(|(e, _client)| {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("authentication")
                        || msg.contains("login")
                        || msg.contains("credentials")
                        || msg.contains("invalid")
                    {
                        EmailError::AuthFailed {
                            account: username.clone(),
                        }
                    } else {
                        EmailError::Imap(e.to_string())
                    }
                })?;

        debug!("IMAP login successful (STARTTLS)");
        self.session = Some(SessionInner::Tls(session));
        Ok(())
    }

    /// Connect without TLS (plain TCP).
    async fn connect_plain(&mut self, tcp_stream: TcpStream) -> Result<(), EmailError> {
        warn!(
            host = %self.config.imap_host,
            "Connecting without TLS — credentials will be sent in plaintext!"
        );

        let mut client = async_imap::Client::new(tcp_stream);

        // Read greeting
        let _greeting = tokio::time::timeout(self.command_timeout, client.read_response())
            .await
            .map_err(|_| EmailError::Timeout {
                operation: "read greeting".into(),
                duration: self.command_timeout,
            })?;

        // Login on plain connection
        let username = self.config.username.clone();
        let password = self.config.password.clone();
        let session =
            tokio::time::timeout(self.command_timeout, client.login(&username, &password))
                .await
                .map_err(|_| EmailError::Timeout {
                    operation: "login".into(),
                    duration: self.command_timeout,
                })?
                .map_err(|(e, _client)| {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains("authentication")
                        || msg.contains("login")
                        || msg.contains("credentials")
                        || msg.contains("invalid")
                    {
                        EmailError::AuthFailed {
                            account: username.clone(),
                        }
                    } else {
                        EmailError::Imap(e.to_string())
                    }
                })?;

        debug!("Plain login successful");
        self.session = Some(SessionInner::Plain(session));
        Ok(())
    }

    /// Ensure the session is alive, reconnecting if needed.
    ///
    /// Sends a NOOP command to health-check the connection.
    /// If NOOP fails, drops the session and reconnects from scratch.
    pub async fn ensure_connected(&mut self) -> Result<(), EmailError> {
        if self.session.is_none() {
            debug!(
                account = %self.config.username,
                "No active session, connecting"
            );
            return self.establish_session().await;
        }

        // Health check via NOOP
        let noop_result = match &mut self.session {
            Some(SessionInner::Tls(s)) => {
                tokio::time::timeout(self.command_timeout, s.noop()).await
            }
            Some(SessionInner::Plain(s)) => {
                tokio::time::timeout(self.command_timeout, s.noop()).await
            }
            None => unreachable!(),
        };

        match noop_result {
            Ok(Ok(_)) => {
                debug!(account = %self.config.username, "NOOP health check passed");
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(
                    account = %self.config.username,
                    error = %e,
                    "NOOP failed, reconnecting"
                );
                self.session = None;
                self.establish_session().await
            }
            Err(_) => {
                warn!(
                    account = %self.config.username,
                    "NOOP timed out, reconnecting"
                );
                self.session = None;
                self.establish_session().await
            }
        }
    }

    /// Get a mutable reference to the TLS session.
    ///
    /// This is the primary entry point for tools (DW-381+). Calls
    /// `ensure_connected()` internally to guarantee a live session.
    ///
    /// Returns an error if the connection is plain TCP (not TLS).
    /// Most real-world IMAP servers use TLS, so this is the common path.
    pub async fn session(&mut self) -> Result<&mut ImapSession, EmailError> {
        self.ensure_connected().await?;
        match &mut self.session {
            Some(SessionInner::Tls(s)) => Ok(s),
            Some(SessionInner::Plain(_)) => Err(EmailError::Imap(
                "Expected TLS session but have plain TCP — use session_plain() instead".into(),
            )),
            None => Err(EmailError::SessionExpired {
                account: self.config.username.clone(),
            }),
        }
    }

    /// Get a mutable reference to the plain TCP session.
    ///
    /// For non-TLS connections. Most tools should use `session()` instead.
    pub async fn session_plain(&mut self) -> Result<&mut ImapSessionPlain, EmailError> {
        self.ensure_connected().await?;
        match &mut self.session {
            Some(SessionInner::Plain(s)) => Ok(s),
            Some(SessionInner::Tls(_)) => Err(EmailError::Imap(
                "Expected plain session but have TLS — use session() instead".into(),
            )),
            None => Err(EmailError::SessionExpired {
                account: self.config.username.clone(),
            }),
        }
    }

    /// Whether the current session is TLS-secured.
    pub fn is_tls(&self) -> bool {
        matches!(&self.session, Some(SessionInner::Tls(_)))
    }

    /// Gracefully disconnect from the IMAP server.
    ///
    /// Sends LOGOUT if a session is alive, then drops the connection.
    pub async fn disconnect(&mut self) {
        if let Some(session) = self.session.take() {
            let account = self.config.username.clone();
            let result = match session {
                SessionInner::Tls(mut s) => {
                    tokio::time::timeout(Duration::from_secs(5), s.logout()).await
                }
                SessionInner::Plain(mut s) => {
                    tokio::time::timeout(Duration::from_secs(5), s.logout()).await
                }
            };
            match result {
                Ok(Ok(_)) => debug!(%account, "IMAP logout successful"),
                Ok(Err(e)) => {
                    debug!(%account, error = %e, "IMAP logout error (non-fatal)")
                }
                Err(_) => {
                    debug!(%account, "IMAP logout timed out (non-fatal)")
                }
            }
        }
    }

    /// Returns the account name (username/email) for this client.
    pub fn account_name(&self) -> &str {
        &self.config.username
    }

    /// Returns the IMAP host for this client.
    pub fn host(&self) -> &str {
        &self.config.imap_host
    }

    /// Returns the IMAP port for this client.
    pub fn port(&self) -> u16 {
        self.config.imap_port
    }

    /// Returns whether this client has an active session.
    pub fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    /// Returns the TLS mode determined from the config.
    pub fn tls_mode(&self) -> TlsMode {
        TlsMode::from_config(&self.config)
    }
}

impl Drop for ImapClient {
    fn drop(&mut self) {
        if self.session.is_some() {
            debug!(
                account = %self.config.username,
                "ImapClient dropped with active session"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::email::config::EmailAccountConfig;

    fn test_config() -> EmailAccountConfig {
        EmailAccountConfig {
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "user@example.com".into(),
            password: "secret123".into(),
            display_name: None,
            tls: true,
            connect_timeout_secs: 30,
            command_timeout_secs: 60,
        }
    }

    #[test]
    fn tls_mode_implicit_on_993() {
        let config = test_config();
        assert_eq!(TlsMode::from_config(&config), TlsMode::Implicit);
    }

    #[test]
    fn tls_mode_starttls_on_143() {
        let mut config = test_config();
        config.imap_port = 143;
        assert_eq!(TlsMode::from_config(&config), TlsMode::StartTls);
    }

    #[test]
    fn tls_mode_starttls_on_587() {
        let mut config = test_config();
        config.imap_port = 587;
        assert_eq!(TlsMode::from_config(&config), TlsMode::StartTls);
    }

    #[test]
    fn tls_mode_none_when_disabled() {
        let mut config = test_config();
        config.tls = false;
        assert_eq!(TlsMode::from_config(&config), TlsMode::None);
    }

    #[test]
    fn tls_mode_none_regardless_of_port() {
        let mut config = test_config();
        config.tls = false;
        config.imap_port = 993;
        assert_eq!(TlsMode::from_config(&config), TlsMode::None);
    }
}

use async_nats::jetstream::Context;
use async_trait::async_trait;
use bytes::Bytes;
use eyre::Result;
use parking_lot::Mutex;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Interface for publishing messages to NATS JetStream.
#[async_trait]
pub trait JetstreamPublisher: Send + Sync {
    /// Publish a raw payload to a subject.
    async fn publish(&self, subject: String, payload: Bytes) -> Result<()>;
}

/// Extension trait for publishing JSON-serialisable messages.
/// Kept separate to maintain dyn-compatibility of JetstreamPublisher.
pub async fn publish_json<T: Serialize>(
    publisher: &dyn JetstreamPublisher,
    subject: String,
    message: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(message)
        .map_err(|e| eyre::eyre!("Failed to serialise message: {}", e))?;
    publisher.publish(subject, Bytes::from(payload)).await
}

/// Production NATS JetStream publisher.
pub struct NatsPublisher {
    context: Context,
}

impl NatsPublisher {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[async_trait]
impl JetstreamPublisher for NatsPublisher {
    async fn publish(&self, subject: String, payload: Bytes) -> Result<()> {
        let ack_future = self
            .context
            .publish(subject.clone(), payload)
            .await
            .map_err(|e| eyre::eyre!("Failed to publish to '{}': {}", subject, e))?;

        let ack = ack_future
            .await
            .map_err(|e| eyre::eyre!("Failed to get ack for '{}': {}", subject, e))?;

        tracing::debug!(
            stream = %ack.stream,
            sequence = ack.sequence,
            subject = %subject,
            "Message published"
        );
        Ok(())
    }
}

/// Configuration for connecting to NATS.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NatsConfig {
    /// NATS server URL(s). Comma-separated for clusters.
    pub url: String,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Maximum reconnection attempts (`None` = unlimited).
    #[serde(default)]
    pub max_reconnects: Option<usize>,
    /// Path to a NATS `.creds` file (NKey + JWT auth). When set, wins
    /// over `token`. Mirrors `nsc`'s output format.
    #[serde(default)]
    pub credentials_file: Option<std::path::PathBuf>,
    /// Bearer token for simple shared-secret auth. Use `credentials_file`
    /// for production deployments — it supports rotation and per-component
    /// identities.
    #[serde(default)]
    pub token: Option<String>,
    /// Require TLS regardless of URL scheme.
    #[serde(default)]
    pub require_tls: bool,
}

// Manual `Default` (not derived) so `connect_timeout_secs` matches the serde
// default. A derived `Default` would zero it, and any `..Default::default()`
// caller would then connect with a 0s timeout that always fails immediately.
impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            connect_timeout_secs: default_connect_timeout(),
            max_reconnects: None,
            credentials_file: None,
            token: None,
            require_tls: false,
        }
    }
}

fn default_connect_timeout() -> u64 {
    10
}

/// Split the configured URL on commas into async-nats server addresses —
/// the "comma-separated for clusters" contract the config docs promise.
/// A single URL is the one-element case; entries are trimmed and empties
/// dropped so `"a, b,"` parses cleanly.
fn parse_server_addrs(url: &str) -> Result<Vec<async_nats::ServerAddr>> {
    let addrs = url
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<async_nats::ServerAddr>()
                .map_err(|e| eyre::eyre!("Invalid NATS URL '{s}': {e}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if addrs.is_empty() {
        return Err(eyre::eyre!("NATS URL is empty"));
    }
    Ok(addrs)
}

/// Connect to NATS and return a JetStream context.
pub async fn connect(config: &NatsConfig) -> Result<(async_nats::Client, Context)> {
    let servers = parse_server_addrs(&config.url)?;
    let mut options = async_nats::ConnectOptions::new()
        .connection_timeout(Duration::from_secs(config.connect_timeout_secs));

    if let Some(max) = config.max_reconnects {
        options = options.max_reconnects(max);
    }
    if let Some(ref path) = config.credentials_file {
        options = options
            .credentials_file(path)
            .await
            .map_err(|e| eyre::eyre!("Failed to load NATS credentials from {path:?}: {e}"))?;
    } else if let Some(ref token) = config.token {
        options = options.token(token.clone());
    }
    if config.require_tls {
        options = options.require_tls(true);
    }

    tracing::info!(
        url = %config.url,
        auth = %auth_label(config),
        tls = config.require_tls,
        "Connecting to NATS",
    );

    // `connection_timeout` bounds the TCP dial, but a half-provisioned
    // endpoint that accepts TCP and never sends the NATS INFO greeting hangs
    // the handshake indefinitely — which on a boot path means the hub never
    // binds its gateway (seen in production). The outer timeout
    // bounds the whole establishment deterministically.
    let client = tokio::time::timeout(
        Duration::from_secs(config.connect_timeout_secs.saturating_add(5)),
        options.connect(servers),
    )
    .await
    .map_err(|_| {
        eyre::eyre!(
            "Timed out connecting to NATS at '{}' after {}s — endpoint accepted TCP but never \
             completed the handshake",
            config.url,
            config.connect_timeout_secs.saturating_add(5)
        )
    })?
    .map_err(|e| eyre::eyre!("Failed to connect to NATS at '{}': {}", config.url, e))?;

    let jetstream = async_nats::jetstream::new(client.clone());

    tracing::info!("Connected to NATS, JetStream context created");
    Ok((client, jetstream))
}

fn auth_label(config: &NatsConfig) -> &'static str {
    match (&config.credentials_file, &config.token) {
        (Some(_), _) => "credentials_file",
        (None, Some(_)) => "token",
        (None, None) => "anonymous",
    }
}

/// Create a JetstreamPublisher from a JetStream context.
pub fn create_publisher(context: Context) -> Arc<dyn JetstreamPublisher> {
    Arc::new(NatsPublisher::new(context))
}

/// Publish a JSON-serialised message directly via a JetStream context.
pub async fn publish_json_direct<T: Serialize>(
    context: &Context,
    subject: &str,
    message: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(message)
        .map_err(|e| eyre::eyre!("Failed to serialise message: {}", e))?;

    let ack_future = context
        .publish(subject.to_string(), Bytes::from(payload))
        .await
        .map_err(|e| eyre::eyre!("Failed to publish to '{}': {}", subject, e))?;

    ack_future
        .await
        .map_err(|e| eyre::eyre!("Failed to get ack for '{}': {}", subject, e))?;

    Ok(())
}

/// Subscribe to a subject and deserialise messages.
pub async fn subscribe_json<T: DeserializeOwned>(
    client: &async_nats::Client,
    subject: &str,
) -> Result<async_nats::Subscriber> {
    client
        .subscribe(subject.to_string())
        .await
        .map_err(|e| eyre::eyre!("Failed to subscribe to '{}': {}", subject, e))
}

// ──────────────────────────────────────
// Test double
// ──────────────────────────────────────

/// In-memory test publisher that captures all published messages.
#[derive(Debug, Clone, Default)]
pub struct TestPublisher {
    messages: Arc<Mutex<VecDeque<(String, Bytes)>>>,
}

impl TestPublisher {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn get_messages(&self) -> Vec<(String, Bytes)> {
        self.messages.lock().iter().cloned().collect()
    }

    pub fn get_last_message(&self) -> Option<(String, Bytes)> {
        self.messages.lock().back().cloned()
    }

    pub fn message_count(&self) -> usize {
        self.messages.lock().len()
    }

    pub fn clear(&self) {
        self.messages.lock().clear();
    }

    /// Get the last published message deserialised as JSON.
    pub fn last_json<T: DeserializeOwned>(&self) -> Option<T> {
        self.get_last_message()
            .and_then(|(_, bytes)| serde_json::from_slice(&bytes).ok())
    }
}

#[async_trait]
impl JetstreamPublisher for TestPublisher {
    async fn publish(&self, subject: String, payload: Bytes) -> Result<()> {
        self.messages.lock().push_back((subject, payload));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_addrs_single_url() {
        let addrs = parse_server_addrs("nats://nats.prod.local:4222").unwrap();
        assert_eq!(addrs.len(), 1);
    }

    #[test]
    fn parse_server_addrs_comma_separated_with_whitespace() {
        let addrs =
            parse_server_addrs("nats://a.local:4222, nats://b.local:4222 ,nats://c.local:4222,")
                .unwrap();
        assert_eq!(addrs.len(), 3);
    }

    #[test]
    fn parse_server_addrs_rejects_empty_and_invalid() {
        assert!(parse_server_addrs("").is_err());
        assert!(parse_server_addrs(" , ").is_err());
        assert!(parse_server_addrs("nats://ok:4222,http://\nbad").is_err());
    }

    #[test]
    fn default_uses_nonzero_connect_timeout() {
        assert_eq!(
            NatsConfig::default().connect_timeout_secs,
            default_connect_timeout()
        );
    }

    #[test]
    fn auth_label_reflects_configured_credentials() {
        let anon = NatsConfig::default();
        assert_eq!(auth_label(&anon), "anonymous");

        let token = NatsConfig {
            token: Some("s3cret".into()),
            ..Default::default()
        };
        assert_eq!(auth_label(&token), "token");

        let creds = NatsConfig {
            credentials_file: Some("/tmp/user.creds".into()),
            ..Default::default()
        };
        assert_eq!(auth_label(&creds), "credentials_file");
    }

    #[test]
    fn credentials_file_wins_over_token_in_auth_label() {
        let both = NatsConfig {
            credentials_file: Some("/tmp/user.creds".into()),
            token: Some("ignored".into()),
            ..Default::default()
        };
        assert_eq!(auth_label(&both), "credentials_file");
    }

    #[test]
    fn default_is_anonymous_cleartext() {
        let cfg = NatsConfig::default();
        assert!(cfg.credentials_file.is_none());
        assert!(cfg.token.is_none());
        assert!(!cfg.require_tls);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn silent_accept_endpoint_times_out_instead_of_hanging() {
        // A half-provisioned endpoint: accepts TCP, never speaks NATS.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _hold = tokio::spawn(async move {
            let mut socks = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                socks.push(sock);
            }
        });

        let cfg = NatsConfig {
            url: format!("nats://{addr}"),
            connect_timeout_secs: 1,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let result = connect(&cfg).await;
        assert!(result.is_err(), "must not hang on a silent endpoint");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "bounded by the outer timeout"
        );
    }
}

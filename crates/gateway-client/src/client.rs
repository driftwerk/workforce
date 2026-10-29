//! WebSocket-based gateway client.

use std::sync::Arc;
use std::time::Duration;

use eyre::{bail, Result, WrapErr};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::protocol::{Attachment, ClientMessage, ServerMessage, ServerMessageKind};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsSource = SplitStream<WsStream>;

/// Configuration for connecting to the gateway.
#[derive(Debug, Clone)]
pub struct GatewayClientConfig {
    /// Gateway URL (e.g. "ws://localhost:8091").
    pub url: String,
    /// Authentication token.
    pub token: String,
    /// Auto-reconnect on disconnect.
    pub auto_reconnect: bool,
    /// Ping interval for keepalive.
    pub ping_interval: Duration,
    /// Maximum reconnection attempts (0 = unlimited).
    pub max_reconnect_attempts: u32,
    /// Base delay between reconnection attempts.
    pub reconnect_delay: Duration,
    /// WebSocket path (default: "/ws"). Use "/ws/agent" for local agent connections.
    pub ws_path: String,
}

impl Default for GatewayClientConfig {
    fn default() -> Self {
        Self {
            url: "ws://localhost:8091".to_string(),
            token: String::new(),
            auto_reconnect: true,
            ping_interval: Duration::from_secs(30),
            max_reconnect_attempts: 10,
            reconnect_delay: Duration::from_secs(1),
            ws_path: "/ws".to_string(),
        }
    }
}

/// Events emitted by the gateway client.
#[derive(Debug, Clone)]
pub enum GatewayEvent {
    /// Successfully connected to gateway.
    Connected,
    /// Received a server message. Boxed to keep the enum size from being
    /// dominated by the wide-ish `ServerMessage` variant.
    Message(Box<ServerMessage>),
    /// Disconnected from gateway.
    Disconnected { reason: String },
    /// Reconnecting (attempt number).
    Reconnecting { attempt: u32 },
    /// Failed to reconnect after max attempts.
    ReconnectFailed,
}

/// Gateway client that maintains a WebSocket connection.
pub struct GatewayClient {
    config: GatewayClientConfig,
    sender: Arc<Mutex<Option<WsSink>>>,
    event_tx: mpsc::Sender<GatewayEvent>,
    event_rx: Option<mpsc::Receiver<GatewayEvent>>,
    shutdown_tx: Option<tokio::sync::broadcast::Sender<()>>,
    session_id: Arc<Mutex<Option<String>>>,
    agent_id: Option<String>,
}

impl GatewayClient {
    /// Create a new gateway client.
    pub fn new(config: GatewayClientConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        Self {
            config,
            sender: Arc::new(Mutex::new(None)),
            event_tx,
            event_rx: Some(event_rx),
            shutdown_tx: None,
            session_id: Arc::new(Mutex::new(None)),
            agent_id: None,
        }
    }

    /// Set the target agent ID (for multi-agent routing).
    pub fn set_agent_id(&mut self, agent_id: &str) {
        self.agent_id = Some(agent_id.to_string());
    }

    /// Take the event receiver (can only be called once).
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<GatewayEvent>> {
        self.event_rx.take()
    }

    /// Connect to the gateway.
    pub async fn connect(&mut self) -> Result<()> {
        let ws_url = self.build_ws_url()?;
        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .wrap_err("Failed to connect to gateway")?;

        info!(url = %ws_url, "Connected to gateway");

        let (sink, stream) = ws_stream.split();
        *self.sender.lock().await = Some(sink);

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        self.shutdown_tx = Some(shutdown_tx.clone());

        // Spawn the reader task
        let event_tx = self.event_tx.clone();
        let session_id = Arc::clone(&self.session_id);
        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = Self::read_loop(stream, event_tx.clone(), session_id) => {}
                _ = shutdown_rx.recv() => {
                    debug!("Reader shutdown signal received");
                }
            }
        });

        // Spawn the ping task
        let sender = Arc::clone(&self.sender);
        let ping_interval = self.config.ping_interval;
        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = Self::ping_loop(sender, ping_interval) => {}
                _ = shutdown_rx.recv() => {
                    debug!("Ping shutdown signal received");
                }
            }
        });

        Ok(())
    }

    /// Send a chat message.
    pub async fn send_chat(&self, content: &str) -> Result<()> {
        let session_id = self.session_id.lock().await.clone();
        let mut msg = if let Some(sid) = session_id {
            ClientMessage::chat_with_session(content, &sid)
        } else {
            ClientMessage::chat(content)
        };
        if let Some(ref agent_id) = self.agent_id {
            msg = msg.with_agent(agent_id);
        }
        self.send_raw(msg).await
    }

    /// Send a chat message with file/image attachments.
    pub async fn send_chat_with_attachments(
        &self,
        content: &str,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        let session_id = self.session_id.lock().await.clone();
        let mut msg =
            ClientMessage::chat_with_attachments(content, attachments, session_id.as_deref());
        if let Some(ref agent_id) = self.agent_id {
            msg = msg.with_agent(agent_id);
        }
        self.send_raw(msg).await
    }

    /// Send a cancel message to abort the current stream.
    pub async fn send_cancel(&self) -> Result<()> {
        let session_id = self.session_id.lock().await.clone();
        if let Some(sid) = session_id {
            self.send_raw(ClientMessage::cancel(&sid)).await
        } else {
            Ok(())
        }
    }

    /// Send a raw client message.
    pub async fn send_raw(&self, msg: ClientMessage) -> Result<()> {
        let json = serde_json::to_string(&msg)?;
        let mut sender = self.sender.lock().await;
        if let Some(ref mut sink) = *sender {
            sink.send(Message::Text(json.into()))
                .await
                .wrap_err("Failed to send message")?;
            Ok(())
        } else {
            bail!("Not connected to gateway")
        }
    }

    /// Disconnect from the gateway.
    pub async fn disconnect(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let mut sender = self.sender.lock().await;
        if let Some(ref mut sink) = *sender {
            let _ = sink.close().await;
        }
        *sender = None;
        info!("Disconnected from gateway");
    }

    /// Check if connected.
    pub async fn is_connected(&self) -> bool {
        self.sender.lock().await.is_some()
    }

    /// Get the current session ID.
    pub async fn session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }

    /// Set the session ID (e.g., when switching sessions).
    pub async fn set_session_id(&self, id: &str) {
        *self.session_id.lock().await = Some(id.to_string());
    }

    /// Clear the session ID (starts a new session on next message).
    pub async fn clear_session_id(&self) {
        *self.session_id.lock().await = None;
    }

    /// Build the WebSocket URL with auth token.
    fn build_ws_url(&self) -> Result<String> {
        let base = &self.config.url;

        // Normalize to ws URL
        let ws_base = if base.starts_with("http://") {
            base.replace("http://", "ws://")
        } else if base.starts_with("https://") {
            base.replace("https://", "wss://")
        } else if !base.starts_with("ws://") && !base.starts_with("wss://") {
            format!("ws://{}", base)
        } else {
            base.to_string()
        };

        let mut url = Url::parse(&ws_base).wrap_err("Invalid gateway URL")?;

        // Ensure path ends with the configured ws_path (default "/ws").
        let ws_suffix = &self.config.ws_path;
        if !url.path().ends_with(ws_suffix.as_str()) {
            let path = format!("{}{}", url.path().trim_end_matches('/'), ws_suffix);
            url.set_path(&path);
        }

        // Add auth token and optional agent ID
        url.query_pairs_mut()
            .append_pair("token", &self.config.token);

        if let Some(ref agent_id) = self.agent_id {
            url.query_pairs_mut().append_pair("agent_id", agent_id);
        }

        Ok(url.to_string())
    }

    /// Read messages from the WebSocket.
    async fn read_loop(
        mut stream: WsSource,
        event_tx: mpsc::Sender<GatewayEvent>,
        session_id: Arc<Mutex<Option<String>>>,
    ) {
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(server_msg) => {
                            // Track session ID from responses (skip empty IDs
                            // from pong/typing broadcasts to avoid overwriting
                            // the real session ID)
                            if let Some(ref sid) = server_msg.session_id {
                                if !sid.is_empty() {
                                    *session_id.lock().await = Some(sid.clone());
                                }
                            }

                            let kind = server_msg.kind();
                            let event = if kind == ServerMessageKind::Connected {
                                GatewayEvent::Connected
                            } else {
                                GatewayEvent::Message(Box::new(server_msg))
                            };

                            if event_tx.send(event).await.is_err() {
                                debug!("Event receiver dropped, stopping reader");
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse server message");
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    let reason = frame
                        .map(|f| f.reason.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    info!(reason = %reason, "WebSocket closed by server");
                    let _ = event_tx.send(GatewayEvent::Disconnected { reason }).await;
                    break;
                }
                Ok(Message::Ping(_)) => {
                    // tungstenite handles pong automatically
                }
                Ok(_) => {} // Binary, Pong, Frame
                Err(e) => {
                    error!(error = %e, "WebSocket read error");
                    let _ = event_tx
                        .send(GatewayEvent::Disconnected {
                            reason: e.to_string(),
                        })
                        .await;
                    break;
                }
            }
        }
    }

    /// Send periodic pings to keep the connection alive.
    async fn ping_loop(sender: Arc<Mutex<Option<WsSink>>>, interval: Duration) {
        let mut ticker = time::interval(interval);
        ticker.tick().await; // Skip the first immediate tick
        loop {
            ticker.tick().await;
            let json = serde_json::to_string(&ClientMessage::ping()).unwrap_or_default();
            let mut sender = sender.lock().await;
            if let Some(ref mut sink) = *sender {
                if sink.send(Message::Text(json.into())).await.is_err() {
                    debug!("Ping failed, connection likely closed");
                    break;
                }
            } else {
                break;
            }
        }
    }
}

/// Try to connect to the gateway, returning None if unavailable.
pub async fn try_connect(config: GatewayClientConfig) -> Option<GatewayClient> {
    let mut client = GatewayClient::new(config);
    match client.connect().await {
        Ok(()) => Some(client),
        Err(e) => {
            debug!(error = %e, "Gateway not available");
            None
        }
    }
}

/// Quick check if the gateway is reachable (HTTP health check).
pub async fn gateway_available(url: &str) -> bool {
    let check_url = if url.starts_with("ws://") {
        url.replace("ws://", "http://")
    } else if url.starts_with("wss://") {
        url.replace("wss://", "https://")
    } else {
        url.to_string()
    };

    // Try to reach the root endpoint (admin UI)
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok();

    if let Some(client) = client {
        client.get(&check_url).send().await.is_ok()
    } else {
        false
    }
}

/// REST client for non-WebSocket gateway access.
pub struct GatewayRestClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl GatewayRestClient {
    /// Create a new REST client.
    pub fn new(base_url: &str, token: &str) -> Self {
        let base = if base_url.starts_with("ws://") {
            base_url.replace("ws://", "http://")
        } else if base_url.starts_with("wss://") {
            base_url.replace("wss://", "https://")
        } else {
            base_url.to_string()
        };

        Self {
            base_url: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Send a chat message and get the response (blocking, non-streaming).
    pub async fn chat(&self, message: &str) -> Result<crate::protocol::ChatResponse> {
        let url = format!("{}/api/chat", self.base_url);
        let req = crate::protocol::ChatRequest {
            message: message.to_string(),
            channel: Some("terminal".to_string()),
            session_id: None,
            agent_id: None,
            turn_id: None,
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .wrap_err("Failed to send chat request")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse chat response")
    }

    /// List active sessions.
    pub async fn sessions(&self) -> Result<crate::protocol::SessionsResponse> {
        let url = format!("{}/api/sessions", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch sessions")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json()
            .await
            .wrap_err("Failed to parse sessions response")
    }

    /// List available agents.
    pub async fn agents(&self) -> Result<crate::protocol::AgentsResponse> {
        let url = format!("{}/api/agents", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch agents")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json()
            .await
            .wrap_err("Failed to parse agents response")
    }

    /// Fetch the full transcript for a session.
    pub async fn transcript(
        &self,
        session_id: &str,
    ) -> Result<crate::protocol::TranscriptResponse> {
        let url = format!("{}/api/sessions/{}/transcript", self.base_url, session_id);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch transcript")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json()
            .await
            .wrap_err("Failed to parse transcript response")
    }

    // ── Pairing (remote gateway) ──────────────────────────────────────

    /// List pending pairing requests.
    pub async fn pair_pending(&self, kind: Option<&str>) -> Result<serde_json::Value> {
        let mut url = format!("{}/api/pair/pending", self.base_url);
        if let Some(k) = kind {
            url = format!("{}?kind={}", url, k);
        }
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch pending pairing requests")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// Approve a pairing request by code.
    pub async fn pair_approve(&self, code: &str) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/approve/{}", self.base_url, code);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to approve pairing request")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Unknown pairing code: {}", code);
            }
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// Reject a pairing request by code.
    pub async fn pair_reject(&self, code: &str) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/reject/{}", self.base_url, code);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to reject pairing request")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Unknown pairing code: {}", code);
            }
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// List paired devices.
    pub async fn pair_devices(&self) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/devices", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch paired devices")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// Revoke a paired device.
    pub async fn pair_revoke_device(&self, device_id: &str) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/devices/{}/revoke", self.base_url, device_id);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to revoke device")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// List channel allowlist.
    pub async fn pair_allowlist(&self, platform: Option<&str>) -> Result<serde_json::Value> {
        let mut url = format!("{}/api/pair/allowlist", self.base_url);
        if let Some(p) = platform {
            url = format!("{}?platform={}", url, p);
        }
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch allowlist")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    // ── Unauthenticated pairing (self-service) ───────────────────────

    /// Request a device pairing code (unauthenticated).
    ///
    /// The CLI is an operator tool and requests the wildcard scope `["*"]`
    /// — the resulting token can call every REST endpoint the gateway
    /// exposes. Narrower scopes are appropriate for end-user devices like
    /// the iOS app, which request only what they need.
    pub async fn pair_request_device(
        &self,
        device_name: &str,
        client_id: Option<&str>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/device/request", self.base_url);
        let mut body = serde_json::json!({
            "device_name": device_name,
            "scopes": ["*"],
        });
        if let Some(cid) = client_id {
            body["client_id"] = serde_json::json!(cid);
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .wrap_err("Failed to request device pairing")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    /// Check device pairing status by code (unauthenticated).
    ///
    /// Audit F9 (2026-05-15): the gateway returns 404 for both
    /// never-existed and expired codes — distinguishing them would let
    /// external probes enumerate which codes had been issued. Synthesise
    /// `{"status":"expired"}` on 404 so callers see the same shape they
    /// always have. Other non-2xx still bubbles as an error.
    pub async fn pair_check_status(&self, code: &str) -> Result<serde_json::Value> {
        let url = format!("{}/api/pair/device/status/{}", self.base_url, code);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to check pairing status")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::json!({ "status": "expired" }));
        }
        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json().await.wrap_err("Failed to parse response")
    }

    // ── Workflows (remote gateway) ───────────────────────────────────

    /// Get the raw TOML source of a workflow definition.
    pub async fn get_workflow_source(&self, id: &str) -> Result<String> {
        let url = format!("{}/api/workflows/{}/source", self.base_url, id);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to fetch workflow source")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Workflow '{}' not found on server", id);
            }
            bail!("Gateway returned {}", resp.status());
        }

        resp.text().await.wrap_err("Failed to read workflow source")
    }

    /// Export all workflow definitions as a map of id → TOML source.
    pub async fn export_workflows(&self) -> Result<std::collections::HashMap<String, String>> {
        let url = format!("{}/api/workflows/export", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to export workflows")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json()
            .await
            .wrap_err("Failed to parse workflow export response")
    }

    /// Upload a workflow definition (raw TOML) to the server.
    pub async fn put_workflow(&self, id: &str, toml_content: &str) -> Result<()> {
        let url = format!("{}/api/workflows/{}", self.base_url, id);
        let resp = self
            .http
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "text/toml")
            .body(toml_content.to_string())
            .send()
            .await
            .wrap_err("Failed to upload workflow")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Failed to upload workflow '{}': {}", id, body);
        }

        Ok(())
    }

    /// Delete a workflow definition from the server.
    pub async fn delete_workflow(&self, id: &str) -> Result<()> {
        let url = format!("{}/api/workflows/{}", self.base_url, id);
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to delete workflow")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Workflow '{}' not found on server", id);
            }
            bail!("Gateway returned {}", resp.status());
        }

        Ok(())
    }

    /// Get the raw TOML source of a playbook definition.
    /// `owner` addresses a member-owned definition (`None` = system/global).
    pub async fn get_playbook_source(&self, id: &str, owner: Option<&str>) -> Result<String> {
        let url = format!("{}/api/playbooks/{}/source", self.base_url, id);
        let mut req = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(owner) = owner {
            req = req.query(&[("owner", owner)]);
        }
        let resp = req
            .send()
            .await
            .wrap_err("Failed to fetch playbook source")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Playbook '{}' not found on server", id);
            }
            bail!("Gateway returned {}", resp.status());
        }

        resp.text().await.wrap_err("Failed to read playbook source")
    }

    /// Export all playbook definitions as a map of id → TOML source.
    pub async fn export_playbooks(&self) -> Result<std::collections::HashMap<String, String>> {
        let url = format!("{}/api/playbooks/export", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to export playbooks")?;

        if !resp.status().is_success() {
            bail!("Gateway returned {}", resp.status());
        }

        resp.json()
            .await
            .wrap_err("Failed to parse playbook export response")
    }

    /// Upload a playbook definition (raw TOML) to the server.
    pub async fn put_playbook(&self, id: &str, toml_content: &str) -> Result<()> {
        let url = format!("{}/api/playbooks/{}", self.base_url, id);
        let resp = self
            .http
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "text/toml")
            .body(toml_content.to_string())
            .send()
            .await
            .wrap_err("Failed to upload playbook")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Failed to upload playbook '{}': {}", id, body);
        }

        Ok(())
    }

    /// Delete a playbook definition from the server.
    pub async fn delete_playbook(&self, id: &str) -> Result<()> {
        let url = format!("{}/api/playbooks/{}", self.base_url, id);
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to delete playbook")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Playbook '{}' not found on server", id);
            }
            bail!("Gateway returned {}", resp.status());
        }

        Ok(())
    }

    // ── Vault (remote gateway) ────────────────────────────────────────

    /// List vault keys with optional prefix filter.
    pub async fn vault_list(&self, prefix: Option<&str>) -> Result<Vec<String>> {
        let mut url = format!("{}/api/vault", self.base_url);
        if let Some(p) = prefix {
            url = format!("{}?prefix={}", url, urlencoding::encode(p));
        }
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to list vault keys")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Authentication failed - invalid or missing token");
            }
            bail!("Gateway returned {}", resp.status());
        }

        #[derive(serde::Deserialize)]
        struct ListResponse {
            keys: Vec<String>,
        }

        let result: ListResponse = resp
            .json()
            .await
            .wrap_err("Failed to parse vault list response")?;
        Ok(result.keys)
    }

    /// Get a vault secret by path.
    pub async fn vault_get(&self, path: &str) -> Result<String> {
        let url = format!("{}/api/vault/{}", self.base_url, urlencoding::encode(path));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to get vault secret")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("Secret not found: {}", path);
            }
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Authentication failed - invalid or missing token");
            }
            bail!("Gateway returned {}", resp.status());
        }

        #[derive(serde::Deserialize)]
        struct GetResponse {
            value: String,
        }

        let result: GetResponse = resp
            .json()
            .await
            .wrap_err("Failed to parse vault get response")?;
        Ok(result.value)
    }

    /// Set a vault secret at path.
    pub async fn vault_set(&self, path: &str, value: &str) -> Result<()> {
        let url = format!("{}/api/vault/{}", self.base_url, urlencoding::encode(path));
        let body = serde_json::json!({ "value": value });

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await
            .wrap_err("Failed to set vault secret")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Authentication failed - invalid or missing token");
            }
            bail!("Gateway returned {}", resp.status());
        }

        Ok(())
    }

    /// Delete a vault secret by path.
    pub async fn vault_delete(&self, path: &str) -> Result<()> {
        let url = format!("{}/api/vault/{}", self.base_url, urlencoding::encode(path));
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .wrap_err("Failed to delete vault secret")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                // Already deleted is ok
                return Ok(());
            }
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Authentication failed - invalid or missing token");
            }
            bail!("Gateway returned {}", resp.status());
        }

        Ok(())
    }

    /// Copy an agent-vault credential into one or more destination users'
    /// per-user slots (system-admin only). Never returns the secret value.
    pub async fn agent_vault_copy(
        &self,
        agent_id: &str,
        path: &str,
        from_tier: &str,
        to_tier: &str,
        from_user: Option<&str>,
        to_user_ids: &[String],
        overwrite: bool,
    ) -> Result<AgentVaultCopyResponse> {
        let url = format!(
            "{}/api/agents/{}/vault/copy",
            self.base_url,
            urlencoding::encode(agent_id)
        );
        let mut body = serde_json::json!({
            "path": path,
            "from_tier": from_tier,
            "to_tier": to_tier,
            "to_user_ids": to_user_ids,
            "overwrite": overwrite,
        });
        if let Some(user) = from_user {
            body["user_id"] = serde_json::Value::String(user.to_string());
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await
            .wrap_err("Failed to copy agent vault credential")?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Authentication failed - invalid or missing token");
            }
            let status = resp.status();
            let detail = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| status.to_string());
            bail!("Gateway returned {}: {}", status, detail);
        }

        resp.json::<AgentVaultCopyResponse>()
            .await
            .wrap_err("Failed to parse agent vault copy response")
    }
}

/// One destination user's result from [`GatewayRestClient::agent_vault_copy`].
#[derive(Debug, serde::Deserialize)]
pub struct AgentVaultCopyOutcome {
    pub user_id: String,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// Response from [`GatewayRestClient::agent_vault_copy`].
#[derive(Debug, serde::Deserialize)]
pub struct AgentVaultCopyResponse {
    pub copied: usize,
    pub results: Vec<AgentVaultCopyOutcome>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ws_url_basic() {
        let config = GatewayClientConfig {
            url: "ws://localhost:8091".to_string(),
            token: "test-token".to_string(),
            ..Default::default()
        };
        let client = GatewayClient::new(config);
        let url = client.build_ws_url().unwrap();
        assert_eq!(url, "ws://localhost:8091/ws?token=test-token");
    }

    #[test]
    fn test_build_ws_url_from_http() {
        let config = GatewayClientConfig {
            url: "http://myhost:9000".to_string(),
            token: "abc123".to_string(),
            ..Default::default()
        };
        let client = GatewayClient::new(config);
        let url = client.build_ws_url().unwrap();
        assert_eq!(url, "ws://myhost:9000/ws?token=abc123");
    }

    #[test]
    fn test_build_ws_url_with_trailing_slash() {
        let config = GatewayClientConfig {
            url: "ws://localhost:8091/".to_string(),
            token: "tok".to_string(),
            ..Default::default()
        };
        let client = GatewayClient::new(config);
        let url = client.build_ws_url().unwrap();
        assert_eq!(url, "ws://localhost:8091/ws?token=tok");
    }

    #[test]
    fn test_build_ws_url_already_has_ws_path() {
        let config = GatewayClientConfig {
            url: "ws://localhost:8091/ws".to_string(),
            token: "tok".to_string(),
            ..Default::default()
        };
        let client = GatewayClient::new(config);
        let url = client.build_ws_url().unwrap();
        assert_eq!(url, "ws://localhost:8091/ws?token=tok");
    }

    #[test]
    fn test_client_message_chat() {
        let msg = ClientMessage::chat("hello");
        assert_eq!(msg.msg_type, "chat");
        assert_eq!(msg.content.as_deref(), Some("hello"));
        assert_eq!(msg.channel.as_deref(), Some("terminal"));
        assert!(msg.session_id.is_none());
    }

    #[test]
    fn test_client_message_chat_with_session() {
        let msg = ClientMessage::chat_with_session("hi", "sess-1");
        assert_eq!(msg.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_client_message_ping() {
        let msg = ClientMessage::ping();
        assert_eq!(msg.msg_type, "ping");
        assert!(msg.content.is_none());
    }

    #[test]
    fn test_server_message_kind() {
        let msg = ServerMessage {
            msg_type: "stream_delta".to_string(),
            content: Some("hello".to_string()),
            session_id: None,
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert_eq!(msg.kind(), ServerMessageKind::StreamDelta);
        assert!(msg.is_stream_delta());
        assert!(!msg.is_message());
    }

    #[test]
    fn test_server_message_roundtrip() {
        let msg = ServerMessage {
            msg_type: "message".to_string(),
            content: Some("response text".to_string()),
            session_id: Some("sess-1".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.msg_type, "message");
        assert_eq!(parsed.text(), Some("response text"));
        assert_eq!(parsed.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_server_message_tool_events() {
        let start = ServerMessage {
            msg_type: "tool_start".to_string(),
            content: Some("filesystem.read".to_string()),
            session_id: Some("default".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert_eq!(start.kind(), ServerMessageKind::ToolStart);
        assert_eq!(start.text(), Some("filesystem.read"));

        let end = ServerMessage {
            msg_type: "tool_end".to_string(),
            content: Some("filesystem.read (12ms, ok)".to_string()),
            session_id: Some("default".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert_eq!(end.kind(), ServerMessageKind::ToolEnd);
    }

    #[test]
    fn test_server_message_error() {
        let msg = ServerMessage {
            msg_type: "error".to_string(),
            content: None,
            session_id: None,
            error: Some("Rate limit exceeded".to_string()),
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert!(msg.is_error());
        assert_eq!(msg.error.as_deref(), Some("Rate limit exceeded"));
    }

    #[test]
    fn test_server_message_reminder() {
        let msg = ServerMessage {
            msg_type: "reminder".to_string(),
            content: Some("⏰ Daily standup in 5 minutes".to_string()),
            session_id: Some("scheduler".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert_eq!(msg.kind(), ServerMessageKind::Reminder);
    }

    #[test]
    fn route_and_connect_prompt_are_known_and_route_carries_no_content() {
        let route: ServerMessage = serde_json::from_value(serde_json::json!({
            "type": "route",
            "content": "",
            "route": "Claude Code · sonnet · Alice's plan",
        }))
        .unwrap();
        assert_eq!(route.kind(), ServerMessageKind::Route);
        assert_eq!(
            route.route.as_deref(),
            Some("Claude Code · sonnet · Alice's plan")
        );
        let prompt: ServerMessage = serde_json::from_value(serde_json::json!({
            "type": "connect_prompt",
            "content": "",
            "connect_prompt": {
                "kind": "chatgpt",
                "agent_id": "allan",
                "reason": "no ChatGPT login of yours",
                "private": true,
            },
        }))
        .unwrap();
        assert_eq!(prompt.kind(), ServerMessageKind::ConnectPrompt);
        assert_eq!(prompt.connect_prompt.unwrap().agent_id, "allan");
    }

    #[test]
    fn test_rest_client_url_normalization() {
        let client = GatewayRestClient::new("ws://localhost:8091", "tok");
        assert_eq!(client.base_url, "http://localhost:8091");

        let client = GatewayRestClient::new("wss://api.example.com:443/", "tok");
        assert_eq!(client.base_url, "https://api.example.com:443");

        let client = GatewayRestClient::new("http://localhost:8091", "tok");
        assert_eq!(client.base_url, "http://localhost:8091");
    }

    #[test]
    fn test_default_config() {
        let config = GatewayClientConfig::default();
        assert_eq!(config.url, "ws://localhost:8091");
        assert!(config.token.is_empty());
        assert!(config.auto_reconnect);
        assert_eq!(config.ping_interval, std::time::Duration::from_secs(30));
        assert_eq!(config.max_reconnect_attempts, 10);
    }

    #[tokio::test]
    async fn test_session_id_management() {
        let config = GatewayClientConfig {
            url: "ws://localhost:9999".to_string(),
            token: "test".to_string(),
            ..Default::default()
        };
        let client = GatewayClient::new(config);

        // Initially no session
        assert!(client.session_id().await.is_none());

        // Set a session
        client.set_session_id("sess-abc").await;
        assert_eq!(client.session_id().await.as_deref(), Some("sess-abc"));

        // Clear it
        client.clear_session_id().await;
        assert!(client.session_id().await.is_none());
    }

    #[test]
    fn test_stream_start_message() {
        let msg = ServerMessage {
            msg_type: "stream_start".to_string(),
            content: None,
            session_id: Some("sess-1".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert!(msg.is_stream_start());
        assert_eq!(msg.kind(), ServerMessageKind::StreamStart);
        assert!(!msg.is_stream_delta());
        assert!(!msg.is_stream_end());
    }

    #[test]
    fn test_stream_end_message() {
        let msg = ServerMessage {
            msg_type: "stream_end".to_string(),
            content: None,
            session_id: Some("sess-1".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        assert!(msg.is_stream_end());
        assert_eq!(msg.kind(), ServerMessageKind::StreamEnd);
        assert!(!msg.is_stream_start());
        assert!(!msg.is_stream_delta());
    }

    #[test]
    fn test_stream_sequence_message_types() {
        // Simulate a full streaming sequence and verify message parsing
        let sequence = vec![
            ("stream_start", "", ServerMessageKind::StreamStart),
            ("stream_delta", "Hello", ServerMessageKind::StreamDelta),
            ("stream_delta", " world", ServerMessageKind::StreamDelta),
            ("stream_delta", "!", ServerMessageKind::StreamDelta),
            ("stream_end", "", ServerMessageKind::StreamEnd),
            ("message", "Hello world!", ServerMessageKind::Message),
        ];

        for (msg_type, content, expected_kind) in sequence {
            let msg = ServerMessage {
                msg_type: msg_type.to_string(),
                content: if content.is_empty() {
                    None
                } else {
                    Some(content.to_string())
                },
                session_id: Some("sess-1".to_string()),
                error: None,
                agent_id: None,
                context_tokens: None,
                context_max: None,
                team_id: None,
                sender_role: None,
                next_speaker: None,
                speakable: true,
                token_match: None,
                request_id: None,
                tool_name: None,
                prompt_options: None,
                attachments: None,
                code_citations: None,
                connect_prompt: None,
                route: None,
                desk: None,
                id: None,
                author: None,
                features: None,
                turn_ref: None,
            };
            assert_eq!(
                msg.kind(),
                expected_kind,
                "Wrong kind for msg_type={}",
                msg_type
            );
        }
    }

    #[test]
    fn test_stream_delta_serialization_roundtrip() {
        let msg = ServerMessage {
            msg_type: "stream_delta".to_string(),
            content: Some("token chunk".to_string()),
            session_id: Some("sess-42".to_string()),
            error: None,
            agent_id: None,
            context_tokens: None,
            context_max: None,
            team_id: None,
            sender_role: None,
            next_speaker: None,
            speakable: true,
            token_match: None,
            request_id: None,
            tool_name: None,
            prompt_options: None,
            attachments: None,
            code_citations: None,
            connect_prompt: None,
            route: None,
            desk: None,
            id: None,
            author: None,
            features: None,
            turn_ref: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_stream_delta());
        assert_eq!(parsed.text(), Some("token chunk"));
        assert_eq!(parsed.session_id.as_deref(), Some("sess-42"));
    }
}

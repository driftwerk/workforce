//! Slack socket mode transport — WebSocket connection to Slack Events API.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use futures::{SinkExt, StreamExt};
use lru::LruCache;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::config::SlackWorkspaceConfig;

/// Max silence from Slack before we consider the WebSocket dead and reconnect.
/// Slack sends some traffic (event, keepalive, Ping) at least every ~30 s when healthy,
/// so a full minute of nothing is a reliable zombie signal.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounded set of recently-seen envelope-ids, used to drop Socket Mode
/// redeliveries. Slack redelivers an event when the prior ACK didn't reach
/// it (e.g. the WebSocket dropped before/while we acked). Capacity is sized
/// well above any sustained redelivery burst inside a single transport
/// lifetime; the LRU naturally bounds memory.
const ENVELOPE_DEDUP_CAPACITY: usize = 8192;

/// Slack API response wrapper.
#[derive(Debug, Deserialize)]
struct SlackApiResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
}

/// Bot identity returned by `auth.test`. Exposed so [`SlackSkill`] can
/// hand the pair through the registration path to the agent's prompt
/// builder, which uses it to teach the LLM its own Slack user_id.
#[derive(Debug, Clone)]
pub struct BotIdentity {
    pub user_id: String,
    pub display_name: String,
    /// Slack bot id (`B…`), present for bot tokens. Carried so the channel
    /// monitor loop guard recognises the bot's own posts on the catch-up read
    /// path, where `conversations.history` may report only `bot_id`.
    pub bot_id: Option<String>,
    /// The bot's home workspace team id (`T…`). The anchor for classifying a
    /// sender as external (a different team than the bot's own).
    pub team_id: Option<String>,
}

/// Slack socket mode envelope.
#[derive(Debug, Deserialize)]
struct SocketModeMessage {
    #[serde(rename = "type")]
    msg_type: String,
    envelope_id: Option<String>,
    payload: Option<Value>,
    #[serde(default)]
    reason: Option<String>,
}

/// Why a single `connect` session ended — drives the reconnect policy in `run`.
#[derive(Debug)]
enum DisconnectKind {
    /// Slack asked us to reconnect (planned rotation) or closed the stream cleanly.
    /// Reconnect immediately and reset the error backoff.
    Graceful(String),
    /// No frame arrived within [`IDLE_TIMEOUT`]; the socket is a zombie.
    /// Short sleep then reconnect, with the error backoff reset so a following
    /// real error still gets the full backoff ramp.
    Idle,
    /// WebSocket / transport error. Exponential backoff 5 s → 60 s.
    Error(String),
    /// Unrecoverable: `disconnect reason=link_disabled` or the caller's event
    /// receiver dropped. Stop the loop; retrying won't help.
    Terminal(String),
}

/// Per-frame decision from [`classify_envelope`]. Pure so it can be unit-tested
/// without a live WebSocket.
#[derive(Debug, PartialEq, Eq)]
enum FrameOutcome {
    /// Envelope is informational (`hello`, unknown types) — ignore.
    Continue,
    /// Forward this payload onto the event channel.
    ForwardEvent(Value),
    /// Slack signalled a planned disconnect — exit `connect` with `Graceful`.
    GracefulDisconnect(String),
    /// Slack signalled a terminal condition (`link_disabled`) — stop retrying.
    TerminalDisconnect(String),
}

#[derive(Debug, PartialEq, Eq)]
enum EnvelopeAction {
    Forward(FrameOutcome),
    Drop,
}

/// Pure decision: classify the envelope and consult the dedup set. Only
/// event-forwarding envelopes with a present envelope-id participate in
/// dedup; control frames (`hello`, `disconnect`) and missing-id frames pass
/// through unchanged. On a first sight the id is recorded; on a repeat the
/// outcome is `Drop`.
fn envelope_action(env: &SocketModeMessage, seen: &mut LruCache<String, ()>) -> EnvelopeAction {
    let outcome = classify_envelope(env);
    match (&outcome, env.envelope_id.as_deref()) {
        (FrameOutcome::ForwardEvent(_), Some(id)) => {
            if seen.contains(id) {
                EnvelopeAction::Drop
            } else {
                seen.put(id.to_string(), ());
                EnvelopeAction::Forward(outcome)
            }
        }
        _ => EnvelopeAction::Forward(outcome),
    }
}

fn classify_envelope(env: &SocketModeMessage) -> FrameOutcome {
    match env.msg_type.as_str() {
        "events_api" => env
            .payload
            .clone()
            .map(FrameOutcome::ForwardEvent)
            .unwrap_or(FrameOutcome::Continue),
        "slash_commands" => env
            .payload
            .clone()
            .map(|mut p| {
                p["_driftwerk_type"] = Value::String("slash_command".to_string());
                FrameOutcome::ForwardEvent(p)
            })
            .unwrap_or(FrameOutcome::Continue),
        "interactive" => env
            .payload
            .clone()
            .map(|mut p| {
                let subtype = p
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                p["_driftwerk_type"] = Value::String(format!("interactive:{subtype}"));
                FrameOutcome::ForwardEvent(p)
            })
            .unwrap_or(FrameOutcome::Continue),
        "disconnect" => {
            let reason = env.reason.clone().unwrap_or_else(|| "unknown".to_string());
            if reason == "link_disabled" {
                FrameOutcome::TerminalDisconnect(reason)
            } else {
                FrameOutcome::GracefulDisconnect(reason)
            }
        }
        _ => FrameOutcome::Continue,
    }
}

/// Default base URL for Slack REST endpoints. Tests override via
/// [`SlackTransport::with_base_url`] to point at a wiremock server;
/// production callers never change it.
pub const DEFAULT_API_BASE_URL: &str = "https://slack.com/api";

/// Handles the WebSocket connection to Slack's socket mode API.
/// Fired after each successful socket (re)connect — used to trigger a
/// catch-up sweep so a gap during a disconnect/deploy is reconciled at once.
pub type OnConnect = Arc<dyn Fn() + Send + Sync>;

pub struct SlackTransport {
    http: reqwest::Client,
    workspace: SlackWorkspaceConfig,
    api_base_url: String,
    seen_envelopes: Arc<Mutex<LruCache<String, ()>>>,
    on_connect: Option<OnConnect>,
}

impl SlackTransport {
    pub fn new(workspace: SlackWorkspaceConfig) -> Self {
        Self::with_base_url(workspace, DEFAULT_API_BASE_URL)
    }

    pub fn with_base_url(workspace: SlackWorkspaceConfig, api_base_url: impl Into<String>) -> Self {
        let cap = NonZeroUsize::new(ENVELOPE_DEDUP_CAPACITY).expect("capacity ≥ 1");
        Self {
            http: reqwest::Client::new(),
            workspace,
            api_base_url: api_base_url.into(),
            seen_envelopes: Arc::new(Mutex::new(LruCache::new(cap))),
            on_connect: None,
        }
    }

    /// Register a callback fired after every successful (re)connect.
    pub fn with_on_connect(mut self, on_connect: OnConnect) -> Self {
        self.on_connect = Some(on_connect);
        self
    }

    fn url(&self, method: &str) -> String {
        format!("{}/{}", self.api_base_url, method)
    }

    /// Resolve the bot's Slack identity (user_id + display name) via auth.test.
    /// Older Slack apps may omit the `user` field; fall back to the user_id.
    pub async fn resolve_bot_identity(&self) -> Result<BotIdentity> {
        let resp: SlackApiResponse = self
            .http
            .post(self.url("auth.test"))
            .bearer_auth(&self.workspace.bot_token)
            .send()
            .await?
            .json()
            .await?;

        if !resp.ok {
            return Err(eyre::eyre!(
                "Slack auth.test failed: {}",
                resp.error.unwrap_or_default()
            ));
        }

        let user_id = resp
            .user_id
            .ok_or_else(|| eyre::eyre!("No user_id in auth.test response"))?;
        let display_name = resp.user.unwrap_or_else(|| user_id.clone());
        Ok(BotIdentity {
            user_id,
            display_name,
            bot_id: resp.bot_id,
            team_id: resp.team_id,
        })
    }

    /// Prove both tokens work before they are stored: the bot token must pass
    /// `auth.test` and the app token must be able to open a Socket Mode
    /// connection. Opening the URL is left to the transport, so nothing
    /// connects here.
    pub async fn verify_credentials(&self) -> Result<BotIdentity> {
        let identity = self.resolve_bot_identity().await?;
        self.get_ws_url().await?;
        Ok(identity)
    }

    async fn get_ws_url(&self) -> Result<String> {
        let resp: Value = self
            .http
            .post(self.url("apps.connections.open"))
            .bearer_auth(&self.workspace.app_token)
            .send()
            .await?
            .json()
            .await?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(eyre::eyre!(
                "apps.connections.open failed: {}",
                resp["error"].as_str().unwrap_or("unknown")
            ));
        }

        resp["url"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("No URL in connections.open response"))
    }

    /// Open one Socket Mode WebSocket and pump frames until it disconnects.
    ///
    /// Returns a [`DisconnectKind`] telling [`run`] how to reconnect:
    /// `Graceful` = go right back, `Idle` = brief pause, `Error` = exponential
    /// backoff, `Terminal` = stop.
    async fn connect(&self, event_tx: mpsc::Sender<Value>) -> DisconnectKind {
        let ws_url = match self.get_ws_url().await {
            Ok(u) => u,
            Err(e) => return DisconnectKind::Error(format!("get_ws_url: {e}")),
        };
        info!(workspace = %self.workspace.id, "Connecting to Slack socket mode...");
        let (ws, _) = match tokio_tungstenite::connect_async(&ws_url).await {
            Ok(pair) => pair,
            Err(e) => return DisconnectKind::Error(format!("connect_async: {e}")),
        };
        let (mut ws_tx, mut ws_rx) = ws.split();
        info!(workspace = %self.workspace.id, "Slack socket mode connected");
        if let Some(cb) = self.on_connect.as_ref() {
            cb();
        }

        loop {
            let next = match tokio::time::timeout(IDLE_TIMEOUT, ws_rx.next()).await {
                Ok(n) => n,
                Err(_) => {
                    warn!(
                        workspace = %self.workspace.id,
                        idle_secs = IDLE_TIMEOUT.as_secs(),
                        "No Slack traffic within idle timeout; treating socket as dead"
                    );
                    return DisconnectKind::Idle;
                }
            };

            let msg = match next {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    error!(workspace = %self.workspace.id, "WebSocket error: {e}");
                    return DisconnectKind::Error(e.to_string());
                }
                None => {
                    return DisconnectKind::Graceful("stream_ended".to_string());
                }
            };

            match msg {
                Message::Text(text) => {
                    let env: SocketModeMessage = match serde_json::from_str(&text) {
                        Ok(e) => e,
                        Err(err) => {
                            warn!(
                                workspace = %self.workspace.id,
                                "Failed to parse socket mode message: {err}"
                            );
                            continue;
                        }
                    };
                    if let Some(ref envelope_id) = env.envelope_id {
                        let ack = serde_json::json!({ "envelope_id": envelope_id });
                        if let Err(e) = ws_tx.send(Message::Text(ack.to_string().into())).await {
                            error!(workspace = %self.workspace.id, "Failed to send ack: {e}");
                            return DisconnectKind::Error(format!("ack: {e}"));
                        }
                    }
                    let action = {
                        let mut seen = self.seen_envelopes.lock().await;
                        envelope_action(&env, &mut seen)
                    };
                    let outcome = match action {
                        EnvelopeAction::Drop => {
                            info!(
                                workspace = %self.workspace.id,
                                envelope_id = env.envelope_id.as_deref().unwrap_or(""),
                                "Dropping redelivered Slack envelope"
                            );
                            continue;
                        }
                        EnvelopeAction::Forward(o) => o,
                    };
                    match outcome {
                        FrameOutcome::Continue => {}
                        FrameOutcome::ForwardEvent(payload) => {
                            if event_tx.send(payload).await.is_err() {
                                info!(
                                    workspace = %self.workspace.id,
                                    "Event receiver dropped, stopping transport"
                                );
                                return DisconnectKind::Terminal("receiver dropped".to_string());
                            }
                        }
                        FrameOutcome::GracefulDisconnect(reason) => {
                            info!(
                                workspace = %self.workspace.id,
                                reason = %reason,
                                "Slack sent disconnect envelope"
                            );
                            return DisconnectKind::Graceful(reason);
                        }
                        FrameOutcome::TerminalDisconnect(reason) => {
                            error!(
                                workspace = %self.workspace.id,
                                reason = %reason,
                                "Slack sent terminal disconnect — will not retry"
                            );
                            return DisconnectKind::Terminal(reason);
                        }
                    }
                }
                Message::Ping(payload) => {
                    if let Err(e) = ws_tx.send(Message::Pong(payload)).await {
                        error!(workspace = %self.workspace.id, "Failed to send Pong: {e}");
                        return DisconnectKind::Error(format!("pong: {e}"));
                    }
                }
                Message::Close(frame) => {
                    let reason = frame
                        .as_ref()
                        .map(|f| format!("{:?}/{}", f.code, f.reason))
                        .unwrap_or_else(|| "no frame".to_string());
                    info!(
                        workspace = %self.workspace.id,
                        reason = %reason,
                        "Slack socket closed by server"
                    );
                    return DisconnectKind::Graceful(format!("closed:{reason}"));
                }
                // Pong from our side (we don't currently originate Pings), plus any
                // unexpected frame types — treat as keepalive and keep reading.
                _ => {}
            }
        }
    }

    /// Run the transport forever, reconnecting according to [`DisconnectKind`].
    pub async fn run(self, event_tx: mpsc::Sender<Value>) {
        let mut backoff_secs = 5u64;
        loop {
            if event_tx.is_closed() {
                info!(
                    workspace = %self.workspace.id,
                    "Event receiver closed, transport exiting"
                );
                return;
            }

            match self.connect(event_tx.clone()).await {
                DisconnectKind::Graceful(reason) => {
                    info!(
                        workspace = %self.workspace.id,
                        reason = %reason,
                        "Slack socket reconnecting: graceful"
                    );
                    backoff_secs = 5;
                }
                DisconnectKind::Idle => {
                    info!(
                        workspace = %self.workspace.id,
                        "Slack socket reconnecting: idle timeout"
                    );
                    backoff_secs = 5;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                DisconnectKind::Error(err) => {
                    error!(
                        workspace = %self.workspace.id,
                        error = %err,
                        backoff_secs,
                        "Slack socket error; reconnecting after backoff"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
                DisconnectKind::Terminal(reason) => {
                    error!(
                        workspace = %self.workspace.id,
                        reason = %reason,
                        "Slack socket terminal disconnect; stopping transport"
                    );
                    return;
                }
            }
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn bot_token(&self) -> &str {
        &self.workspace.bot_token
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(v: serde_json::Value) -> SocketModeMessage {
        serde_json::from_value(v).expect("valid SocketModeMessage")
    }

    #[test]
    fn classify_events_api_forwards_payload_as_is() {
        let m = env(json!({
            "type": "events_api",
            "envelope_id": "e1",
            "payload": { "event": { "type": "message" } }
        }));
        match classify_envelope(&m) {
            FrameOutcome::ForwardEvent(p) => {
                assert_eq!(p["event"]["type"], "message");
                assert!(p.get("_driftwerk_type").is_none());
            }
            other => panic!("expected ForwardEvent, got {other:?}"),
        }
    }

    #[test]
    fn classify_slash_commands_tags_payload() {
        let m = env(json!({
            "type": "slash_commands",
            "envelope_id": "e2",
            "payload": { "command": "/cc", "text": "workforce" }
        }));
        match classify_envelope(&m) {
            FrameOutcome::ForwardEvent(p) => {
                assert_eq!(p["_driftwerk_type"], "slash_command");
                assert_eq!(p["command"], "/cc");
            }
            other => panic!("expected ForwardEvent, got {other:?}"),
        }
    }

    #[test]
    fn classify_interactive_tags_subtype() {
        let m = env(json!({
            "type": "interactive",
            "envelope_id": "e3",
            "payload": { "type": "block_actions", "actions": [] }
        }));
        match classify_envelope(&m) {
            FrameOutcome::ForwardEvent(p) => {
                assert_eq!(p["_driftwerk_type"], "interactive:block_actions");
            }
            other => panic!("expected ForwardEvent, got {other:?}"),
        }
    }

    #[test]
    fn classify_disconnect_warning_is_graceful() {
        let m = env(json!({ "type": "disconnect", "reason": "warning" }));
        match classify_envelope(&m) {
            FrameOutcome::GracefulDisconnect(r) => assert_eq!(r, "warning"),
            other => panic!("expected GracefulDisconnect, got {other:?}"),
        }
    }

    #[test]
    fn classify_disconnect_refresh_requested_is_graceful() {
        let m = env(json!({ "type": "disconnect", "reason": "refresh_requested" }));
        assert!(matches!(
            classify_envelope(&m),
            FrameOutcome::GracefulDisconnect(_)
        ));
    }

    #[test]
    fn classify_disconnect_link_disabled_is_terminal() {
        let m = env(json!({ "type": "disconnect", "reason": "link_disabled" }));
        match classify_envelope(&m) {
            FrameOutcome::TerminalDisconnect(r) => assert_eq!(r, "link_disabled"),
            other => panic!("expected TerminalDisconnect, got {other:?}"),
        }
    }

    #[test]
    fn classify_disconnect_without_reason_defaults_to_graceful_unknown() {
        let m = env(json!({ "type": "disconnect" }));
        match classify_envelope(&m) {
            FrameOutcome::GracefulDisconnect(r) => assert_eq!(r, "unknown"),
            other => panic!("expected GracefulDisconnect, got {other:?}"),
        }
    }

    #[test]
    fn classify_hello_is_continue() {
        let m = env(json!({ "type": "hello" }));
        assert!(matches!(classify_envelope(&m), FrameOutcome::Continue));
    }

    #[test]
    fn classify_unknown_type_is_continue() {
        let m = env(json!({ "type": "future_envelope_type" }));
        assert!(matches!(classify_envelope(&m), FrameOutcome::Continue));
    }

    #[test]
    fn classify_payload_missing_is_continue() {
        let m = env(json!({ "type": "events_api", "envelope_id": "e4" }));
        assert!(matches!(classify_envelope(&m), FrameOutcome::Continue));
    }

    fn dedup(cap: usize) -> LruCache<String, ()> {
        LruCache::new(NonZeroUsize::new(cap).expect("cap ≥ 1"))
    }

    #[test]
    fn envelope_action_first_seen_forwards_and_remembers() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "events_api",
            "envelope_id": "e-first",
            "payload": { "event": { "type": "message", "text": "hi" } }
        }));
        match envelope_action(&m, &mut seen) {
            EnvelopeAction::Forward(FrameOutcome::ForwardEvent(p)) => {
                assert_eq!(p["event"]["text"], "hi");
            }
            other => panic!("expected Forward(ForwardEvent), got {other:?}"),
        }
        assert!(seen.contains("e-first"));
    }

    #[test]
    fn envelope_action_redelivery_drops() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "events_api",
            "envelope_id": "e-dup",
            "payload": { "event": { "type": "message" } }
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::ForwardEvent(_))
        ));
        assert_eq!(envelope_action(&m, &mut seen), EnvelopeAction::Drop);
        assert_eq!(envelope_action(&m, &mut seen), EnvelopeAction::Drop);
    }

    #[test]
    fn envelope_action_missing_envelope_id_passes_through_uncached() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "events_api",
            "payload": { "event": { "type": "message" } }
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::ForwardEvent(_))
        ));
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn envelope_action_continue_envelope_not_cached() {
        let mut seen = dedup(4);
        let m = env(json!({ "type": "hello", "envelope_id": "e-hello" }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::Continue)
        ));
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn envelope_action_graceful_disconnect_not_cached() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "disconnect",
            "reason": "warning",
            "envelope_id": "e-disc"
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::GracefulDisconnect(_))
        ));
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn envelope_action_terminal_disconnect_not_cached() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "disconnect",
            "reason": "link_disabled",
            "envelope_id": "e-term"
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::TerminalDisconnect(_))
        ));
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn envelope_action_slash_command_dedups() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "slash_commands",
            "envelope_id": "e-cmd",
            "payload": { "command": "/cc" }
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::ForwardEvent(_))
        ));
        assert_eq!(envelope_action(&m, &mut seen), EnvelopeAction::Drop);
    }

    #[test]
    fn envelope_action_interactive_dedups() {
        let mut seen = dedup(4);
        let m = env(json!({
            "type": "interactive",
            "envelope_id": "e-int",
            "payload": { "type": "block_actions", "actions": [] }
        }));
        assert!(matches!(
            envelope_action(&m, &mut seen),
            EnvelopeAction::Forward(FrameOutcome::ForwardEvent(_))
        ));
        assert_eq!(envelope_action(&m, &mut seen), EnvelopeAction::Drop);
    }

    #[test]
    fn envelope_action_lru_evicts_oldest_at_capacity() {
        let mut seen = dedup(2);
        let mk = |id: &str| {
            env(json!({
                "type": "events_api",
                "envelope_id": id,
                "payload": { "event": { "type": "message" } }
            }))
        };
        envelope_action(&mk("a"), &mut seen);
        envelope_action(&mk("b"), &mut seen);
        envelope_action(&mk("c"), &mut seen);
        assert!(!seen.contains("a"));
        assert!(seen.contains("b"));
        assert!(seen.contains("c"));
        assert!(matches!(
            envelope_action(&mk("a"), &mut seen),
            EnvelopeAction::Forward(_)
        ));
        assert_eq!(envelope_action(&mk("c"), &mut seen), EnvelopeAction::Drop);
    }

    use crate::adapters::slack::config::SlackWorkspaceConfig;

    fn test_workspace() -> SlackWorkspaceConfig {
        SlackWorkspaceConfig {
            id: "test-ws".into(),
            bot_token: "xoxb-test".into(),
            app_token: "xapp-test".into(),
            channels: Vec::new(),
            response_mode: None,
            thread_isolation: None,
            activation_keywords: None,
            paragraph_spacing: None,
            bot_user_id: None,
        }
    }

    #[tokio::test]
    async fn resolve_bot_identity_returns_user_id_and_display_name() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "user_id": "U_BOT",
                "user": "Atlas",
                "team_id": "T_HOME",
            })))
            .mount(&server)
            .await;

        let transport = SlackTransport::with_base_url(test_workspace(), server.uri());
        let id = transport.resolve_bot_identity().await.unwrap();
        assert_eq!(id.user_id, "U_BOT");
        assert_eq!(id.display_name, "Atlas");
        assert_eq!(id.team_id.as_deref(), Some("T_HOME"));
    }

    #[tokio::test]
    async fn resolve_bot_identity_falls_back_to_user_id_when_user_missing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "user_id": "U_BOT",
            })))
            .mount(&server)
            .await;

        let transport = SlackTransport::with_base_url(test_workspace(), server.uri());
        let id = transport.resolve_bot_identity().await.unwrap();
        assert_eq!(id.user_id, "U_BOT");
        assert_eq!(id.display_name, "U_BOT");
    }

    #[tokio::test]
    async fn resolve_bot_identity_errors_when_slack_reports_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false,
                "error": "invalid_auth",
            })))
            .mount(&server)
            .await;

        let transport = SlackTransport::with_base_url(test_workspace(), server.uri());
        let err = transport
            .resolve_bot_identity()
            .await
            .expect_err("invalid_auth must surface as Err");
        assert!(err.to_string().contains("invalid_auth"));
    }

    async fn verify_against(app_response: Value) -> Result<BotIdentity> {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "user_id": "U_BOT",
                "user": "minny",
                "team_id": "T_HOME",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(app_response))
            .mount(&server)
            .await;
        SlackTransport::with_base_url(test_workspace(), server.uri())
            .verify_credentials()
            .await
    }

    #[tokio::test]
    async fn verify_credentials_returns_the_bot_when_both_tokens_work() {
        let identity = verify_against(json!({ "ok": true, "url": "wss://example/socket" }))
            .await
            .unwrap();
        assert_eq!(identity.user_id, "U_BOT");
        assert_eq!(identity.display_name, "minny");
    }

    #[tokio::test]
    async fn verify_credentials_rejects_an_app_token_slack_refuses() {
        let err = verify_against(json!({ "ok": false, "error": "invalid_auth" }))
            .await
            .expect_err("a refused app token must fail verification");
        assert!(err
            .to_string()
            .contains("apps.connections.open failed: invalid_auth"));
    }
}

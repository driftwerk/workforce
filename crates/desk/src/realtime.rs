//! Cloud voice sessions for the Desk app without the app ever holding a
//! provider key. The key stays in the hub's vault; the app gets something
//! short-lived instead:
//!
//! - Gemini Live: an ephemeral token minted from the vault key
//!   (`v1beta/auth_tokens`, one use, a minute to connect), used against the
//!   constrained Live endpoint.
//! - GPT-Live: OpenAI publishes no client credential for Live sessions (its
//!   WebSocket takes the project key), so the hub relays the socket. The app
//!   gets a single-use relay ticket; the hub dials the fixed OpenAI endpoint
//!   with the key read at that moment and passes frames through unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

/// Whose key a session needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Gemini,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Gemini => "gemini",
        }
    }
}

/// Where provider keys come from. The hub implements it over its layered
/// vault; nothing here reads the environment or config.
pub trait KeySource: Send + Sync {
    fn api_key(&self, provider: Provider) -> Option<String>;
    /// What to tell someone whose hub holds no key for `provider`.
    fn how_to_add(&self, provider: Provider) -> String;
}

/// A cloud voice engine the app can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    GptLive,
    GeminiLive,
}

impl Engine {
    fn provider(self) -> Provider {
        match self {
            Engine::GptLive => Provider::OpenAi,
            Engine::GeminiLive => Provider::Gemini,
        }
    }

    fn model(self) -> &'static str {
        match self {
            Engine::GptLive => GPT_LIVE_MODEL,
            Engine::GeminiLive => GEMINI_LIVE_MODEL,
        }
    }
}

pub const GPT_LIVE_MODEL: &str = "gpt-live-1";
pub const GEMINI_LIVE_MODEL: &str = "gemini-3.8-live";

/// What the app connects with: `url` already carries the credential.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub model: String,
    pub url: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RealtimeError {
    #[error("{0}")]
    NoKey(String),
    #[error("the voice provider refused: {0}")]
    Upstream(String),
}

/// The provider endpoints, fixed in production and pointed at fakes in tests.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub openai_live: String,
    pub gemini_tokens: String,
    pub gemini_live: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            openai_live: "wss://api.openai.com/v1/live/sessions".into(),
            gemini_tokens: "https://generativelanguage.googleapis.com/v1beta/auth_tokens".into(),
            gemini_live: "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContentConstrained".into(),
        }
    }
}

const TICKET_LIFETIME: Duration = Duration::from_secs(60);
const GEMINI_SESSION_LIFETIME: Duration = Duration::from_secs(30 * 60);

pub struct Realtime {
    keys: Arc<dyn KeySource>,
    endpoints: Endpoints,
    http: reqwest::Client,
    tickets: Mutex<HashMap<String, DateTime<Utc>>>,
}

#[derive(Deserialize)]
struct AuthToken {
    name: String,
}

impl Realtime {
    pub fn new(keys: Arc<dyn KeySource>) -> Self {
        Self::with_endpoints(keys, Endpoints::default())
    }

    pub fn with_endpoints(keys: Arc<dyn KeySource>, endpoints: Endpoints) -> Self {
        Self {
            keys,
            endpoints,
            http: reqwest::Client::new(),
            tickets: Mutex::new(HashMap::new()),
        }
    }

    fn key(&self, provider: Provider) -> Result<String, RealtimeError> {
        self.keys
            .api_key(provider)
            .ok_or_else(|| RealtimeError::NoKey(self.keys.how_to_add(provider)))
    }

    /// A short-lived way in for `engine`. `host` is how the app reached this
    /// hub, for the relay's address.
    pub async fn grant(&self, engine: Engine, host: &str) -> Result<Grant, RealtimeError> {
        let key = self.key(engine.provider())?;
        match engine {
            Engine::GptLive => Ok(self.ticket(host)),
            Engine::GeminiLive => self.gemini_token(&key).await,
        }
    }

    fn ticket(&self, host: &str) -> Grant {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let expires_at = Utc::now() + TICKET_LIFETIME;
        let mut tickets = self.tickets.lock();
        let now = Utc::now();
        tickets.retain(|_, until| *until > now);
        tickets.insert(token.clone(), expires_at);
        Grant {
            url: format!("ws://{host}/api/desk/voice/relay?ticket={token}"),
            token,
            expires_at,
            model: Engine::GptLive.model().into(),
        }
    }

    async fn gemini_token(&self, key: &str) -> Result<Grant, RealtimeError> {
        let now = Utc::now();
        let expires_at = now + GEMINI_SESSION_LIFETIME;
        let response = self
            .http
            .post(&self.endpoints.gemini_tokens)
            .header("x-goog-api-key", key)
            .json(&json!({
                "uses": 1,
                "expireTime": expires_at.to_rfc3339(),
                "newSessionExpireTime": (now + TICKET_LIFETIME).to_rfc3339(),
            }))
            .send()
            .await
            .map_err(|e| RealtimeError::Upstream(e.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(RealtimeError::Upstream(format!("Gemini answered {status}")));
        }
        let token: AuthToken = response
            .json()
            .await
            .map_err(|e| RealtimeError::Upstream(e.without_url().to_string()))?;
        Ok(Grant {
            url: format!("{}?access_token={}", self.endpoints.gemini_live, token.name),
            token: token.name,
            expires_at,
            model: Engine::GeminiLive.model().into(),
        })
    }

    /// Spends a relay ticket: true once, while it is fresh.
    pub fn redeem(&self, ticket: &str) -> bool {
        self.tickets
            .lock()
            .remove(ticket)
            .is_some_and(|until| until > Utc::now())
    }

    /// Joins the app's socket to a GPT-Live session dialled with the vault
    /// key, until either side closes.
    pub async fn relay(&self, mut app: WebSocket) {
        match self.dial_openai().await {
            Ok(upstream) => pump(app, upstream).await,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't open a GPT-Live session");
                let _ = app.send(AxumMessage::Close(None)).await;
            }
        }
    }

    async fn dial_openai(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        RealtimeError,
    > {
        let key = self.key(Provider::OpenAi)?;
        let mut request = self
            .endpoints
            .openai_live
            .as_str()
            .into_client_request()
            .map_err(|e| RealtimeError::Upstream(e.to_string()))?;
        let auth = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| RealtimeError::Upstream("the OpenAI key isn't a valid header".into()))?;
        request.headers_mut().insert("Authorization", auth);
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| RealtimeError::Upstream(e.to_string()))?;
        Ok(socket)
    }
}

enum Step<M> {
    Send(M),
    Skip,
    End,
}

async fn pump<S>(app: WebSocket, upstream: tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut app_tx, mut app_rx) = app.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    loop {
        tokio::select! {
            from_app = app_rx.next() => match from_app.and_then(Result::ok).map_or(Step::End, to_upstream) {
                Step::Send(m) => if up_tx.send(m).await.is_err() { break },
                Step::Skip => {}
                Step::End => break,
            },
            from_up = up_rx.next() => match from_up.and_then(Result::ok).map_or(Step::End, to_app) {
                Step::Send(m) => if app_tx.send(m).await.is_err() { break },
                Step::Skip => {}
                Step::End => break,
            },
        }
    }
    let _ = up_tx.send(WsMessage::Close(None)).await;
    let _ = app_tx.send(AxumMessage::Close(None)).await;
}

fn to_upstream(m: AxumMessage) -> Step<WsMessage> {
    match m {
        AxumMessage::Text(t) => Step::Send(WsMessage::text(t.as_str())),
        AxumMessage::Binary(b) => Step::Send(WsMessage::binary(b)),
        AxumMessage::Ping(p) => Step::Send(WsMessage::Ping(p)),
        AxumMessage::Pong(p) => Step::Send(WsMessage::Pong(p)),
        AxumMessage::Close(_) => Step::End,
    }
}

fn to_app(m: WsMessage) -> Step<AxumMessage> {
    match m {
        WsMessage::Text(t) => Step::Send(AxumMessage::text(t.as_str())),
        WsMessage::Binary(b) => Step::Send(AxumMessage::Binary(b)),
        WsMessage::Ping(p) => Step::Send(AxumMessage::Ping(p)),
        WsMessage::Pong(p) => Step::Send(AxumMessage::Pong(p)),
        WsMessage::Frame(_) => Step::Skip,
        WsMessage::Close(_) => Step::End,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::voice_router;
    use crate::testing::{Bearer, Mute};
    use axum::body::{to_bytes, Body};
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    struct Keys(Option<&'static str>);

    impl KeySource for Keys {
        fn api_key(&self, _: Provider) -> Option<String> {
            self.0.map(String::from)
        }
        fn how_to_add(&self, provider: Provider) -> String {
            format!("add speech/{}/api_key", provider.name())
        }
    }

    async fn serve(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr.to_string()
    }

    async fn ask(app: Router, engine: &str, token: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::post("/api/desk/voice/session")
            .header("authorization", format!("Bearer {token}"))
            .header("host", "127.0.0.1:4100")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "engine": engine }).to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn router(realtime: Realtime) -> Router {
        voice_router(Arc::new(Bearer), Arc::new(realtime), Arc::new(Mute))
    }

    #[tokio::test]
    async fn gpt_live_gets_a_relay_ticket_not_the_key() {
        let realtime = Arc::new(Realtime::new(Arc::new(Keys(Some("sk-vault")))));
        let app = voice_router(Arc::new(Bearer), Arc::clone(&realtime), Arc::new(Mute));
        let (status, grant) = ask(app, "gpt-live", "hub-token").await;
        assert_eq!(status, StatusCode::OK);
        let token = grant["token"].as_str().unwrap();
        assert_eq!(grant["model"], GPT_LIVE_MODEL);
        assert_eq!(
            grant["url"],
            format!("ws://127.0.0.1:4100/api/desk/voice/relay?ticket={token}")
        );
        assert!(!grant.to_string().contains("sk-vault"));
        assert!(
            grant["expiresAt"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap()
                > Utc::now()
        );
        assert!(realtime.redeem(token));
        assert!(!realtime.redeem(token), "a ticket works once");
        assert!(!realtime.redeem("guessed"));
    }

    #[tokio::test]
    async fn needs_the_hub_token_and_a_vault_key() {
        let (status, _) = ask(
            router(Realtime::new(Arc::new(Keys(Some("k"))))),
            "gpt-live",
            "wrong",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = ask(
            router(Realtime::new(Arc::new(Keys(None)))),
            "gemini-live",
            "hub-token",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "add speech/gemini/api_key");
    }

    #[tokio::test]
    async fn gemini_gets_an_ephemeral_token_minted_with_the_vault_key() {
        let seen = Arc::new(Mutex::new(None::<(Option<String>, serde_json::Value)>));
        let fake = {
            let seen = Arc::clone(&seen);
            Router::new().route(
                "/v1beta/auth_tokens",
                post(
                    move |h: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let key = h
                                .get("x-goog-api-key")
                                .and_then(|v| v.to_str().ok())
                                .map(String::from);
                            *seen.lock() = Some((key, body));
                            axum::Json(json!({ "name": "auth_tokens/eph-123" }))
                        }
                    },
                ),
            )
        };
        let addr = serve(fake).await;
        let endpoints = Endpoints {
            gemini_tokens: format!("http://{addr}/v1beta/auth_tokens"),
            ..Endpoints::default()
        };
        let realtime =
            Realtime::with_endpoints(Arc::new(Keys(Some("gm-vault"))), endpoints.clone());
        let (status, grant) = ask(router(realtime), "gemini-live", "hub-token").await;
        assert_eq!(status, StatusCode::OK, "{grant}");
        assert_eq!(grant["token"], "auth_tokens/eph-123");
        assert_eq!(grant["model"], GEMINI_LIVE_MODEL);
        assert_eq!(
            grant["url"],
            format!("{}?access_token=auth_tokens/eph-123", endpoints.gemini_live)
        );
        let (key, body) = seen.lock().clone().unwrap();
        assert_eq!(key.as_deref(), Some("gm-vault"));
        assert_eq!(body["uses"], 1);
        assert!(body["expireTime"].is_string() && body["newSessionExpireTime"].is_string());
    }

    #[tokio::test]
    async fn a_refused_mint_is_a_bad_gateway() {
        let fake = Router::new().route(
            "/v1beta/auth_tokens",
            post(|| async { (StatusCode::FORBIDDEN, "no") }),
        );
        let addr = serve(fake).await;
        let endpoints = Endpoints {
            gemini_tokens: format!("http://{addr}/v1beta/auth_tokens"),
            ..Endpoints::default()
        };
        let realtime = Realtime::with_endpoints(Arc::new(Keys(Some("gm"))), endpoints);
        let (status, body) = ask(router(realtime), "gemini-live", "hub-token").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body["error"].as_str().unwrap().contains("403"));
    }

    #[tokio::test]
    async fn the_relay_dials_openai_with_the_vault_key_and_passes_frames_through() {
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as WsRequest, Response as WsResponse,
        };
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let auth_seen = Arc::new(Mutex::new(None::<String>));
        {
            let auth_seen = Arc::clone(&auth_seen);
            tokio::spawn(async move {
                let (stream, _) = upstream.accept().await.unwrap();
                let callback = |req: &WsRequest, res: WsResponse| {
                    *auth_seen.lock() = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    Ok(res)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback)
                    .await
                    .unwrap();
                while let Some(Ok(m)) = ws.next().await {
                    if let WsMessage::Text(t) = m {
                        ws.send(WsMessage::text(format!("echo {}", t.as_str())))
                            .await
                            .unwrap();
                    }
                }
            });
        }
        let endpoints = Endpoints {
            openai_live: format!("ws://{upstream_addr}/v1/live/sessions"),
            ..Endpoints::default()
        };
        let realtime = Arc::new(Realtime::with_endpoints(
            Arc::new(Keys(Some("sk-vault"))),
            endpoints,
        ));
        let hub = serve(voice_router(
            Arc::new(Bearer),
            Arc::clone(&realtime),
            Arc::new(Mute),
        ))
        .await;
        let grant = realtime.grant(Engine::GptLive, &hub).await.unwrap();

        let (mut app, _) = tokio_tungstenite::connect_async(grant.url.as_str())
            .await
            .unwrap();
        app.send(WsMessage::text(r#"{"type":"session.start"}"#))
            .await
            .unwrap();
        let reply = app.next().await.unwrap().unwrap();
        assert_eq!(reply, WsMessage::text(r#"echo {"type":"session.start"}"#));
        assert_eq!(auth_seen.lock().as_deref(), Some("Bearer sk-vault"));

        let again = tokio_tungstenite::connect_async(grant.url.as_str()).await;
        assert!(again.is_err(), "the ticket was spent");
    }

    #[tokio::test]
    #[ignore = "reaches api.openai.com"]
    async fn openai_live_rejects_a_bad_key_rather_than_failing_tls() {
        let realtime = Realtime::new(Arc::new(Keys(Some("sk-not-a-real-key"))));
        match realtime.dial_openai().await {
            Err(RealtimeError::Upstream(e)) => {
                assert!(e.contains("401") || e.contains("403"), "{e}")
            }
            other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    #[ignore = "reaches generativelanguage.googleapis.com"]
    async fn gemini_refuses_to_mint_with_a_bad_key() {
        let realtime = Realtime::new(Arc::new(Keys(Some("not-a-real-key"))));
        match realtime.grant(Engine::GeminiLive, "localhost").await {
            Err(RealtimeError::Upstream(e)) => {
                assert!(e.contains("400") || e.contains("403"), "{e}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

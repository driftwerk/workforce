//! `/api/desk/*` for the Desk app (REST + two websockets), and the loopback
//! endpoint Claude Code's hooks post to.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::conversations::{self, Conversations};
use crate::model::{Decision, NewSession, SessionPatch};
use crate::providers::{self, AccountError, Accounts, Catalog, ConnectBody, Entry};
use crate::realtime::{Engine, Realtime, RealtimeError};
use crate::rooms::{NewMessage, NewRoom, RoomPatch};
use crate::service::{Desk, DeskError};
use crate::speech::{Speak, SpeakError, Speech};

/// Decides whether a request may use the desk. Implemented by the hub.
#[async_trait]
pub trait Authorize: Send + Sync {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<(), StatusCode>;
}

struct ApiState {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

impl IntoResponse for DeskError {
    fn into_response(self) -> Response {
        let status = match &self {
            DeskError::NotFound => StatusCode::NOT_FOUND,
            DeskError::Conflict(_) => StatusCode::CONFLICT,
            DeskError::Invalid(_) => StatusCode::BAD_REQUEST,
            DeskError::Internal(e) => {
                tracing::error!(error = %e, "desk request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

async fn guard(auth: &dyn Authorize, headers: &HeaderMap, q: &TokenQuery) -> Result<(), Response> {
    auth.authorize(headers, q.token.as_deref())
        .await
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

macro_rules! guarded {
    ($s:expr, $h:expr, $q:expr) => {
        if let Err(r) = guard(&*$s.auth, &$h, &$q).await {
            return r;
        }
    };
}

type S = State<Arc<ApiState>>;
type Q = Query<TokenQuery>;

async fn list(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.list()).into_response()
}

async fn create(State(s): S, h: HeaderMap, Query(q): Q, Json(new): Json<NewSession>) -> Response {
    guarded!(s, h, q);
    s.desk.create(new).await.map(Json).into_response()
}

async fn update(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<SessionPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update(id, p).map(Json).into_response()
}

async fn forget(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .forget(id)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn stop(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.stop(id).map(Json).into_response()
}

#[derive(Deserialize, Default)]
struct ResumeBody {
    prompt: Option<String>,
}

async fn resume(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    body: Option<Json<ResumeBody>>,
) -> Response {
    guarded!(s, h, q);
    let prompt = body
        .and_then(|Json(b)| b.prompt)
        .filter(|p| !p.trim().is_empty());
    s.desk.resume(id, prompt).await.map(Json).into_response()
}

#[derive(Deserialize)]
struct DecideBody {
    decision: Decision,
}

async fn decide(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, approval)): Path<(Uuid, Uuid)>,
    Json(b): Json<DecideBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .decide(id, approval, b.decision)
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct RestoreBody {
    ids: Vec<Uuid>,
}

async fn restore(State(s): S, h: HeaderMap, Query(q): Q, Json(b): Json<RestoreBody>) -> Response {
    guarded!(s, h, q);
    Json(s.desk.restore(&b.ids).await).into_response()
}

async fn dismiss_restore(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    s.desk.dismiss_restore();
    StatusCode::NO_CONTENT.into_response()
}

async fn events(State(s): S, h: HeaderMap, Query(q): Q, ws: WebSocketUpgrade) -> Response {
    guarded!(s, h, q);
    let desk = s.desk.clone();
    ws.on_upgrade(move |socket| stream_events(socket, desk))
}

async fn stream_events(socket: WebSocket, desk: Desk) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut changes) = desk.subscribe();
    let send = |e: &crate::model::DeskEvent| {
        Message::text(serde_json::to_string(e).unwrap_or_else(|_| "{}".into()))
    };
    if tx.send(send(&snapshot)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            change = changes.recv() => match change {
                Ok(e) => if tx.send(send(&e)).await.is_err() { return },
                // Fell behind: start again from a fresh snapshot.
                Err(RecvError::Lagged(_)) => {
                    let (snapshot, fresh) = desk.subscribe();
                    changes = fresh;
                    if tx.send(send(&snapshot)).await.is_err() { return }
                }
                Err(RecvError::Closed) => return,
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn pty(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Response {
    guarded!(s, h, q);
    match s.desk.attach(id) {
        Ok(pty) => ws.on_upgrade(move |socket| stream_pty(socket, pty)),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

/// Binary frames carry terminal bytes both ways; text frames carry control
/// messages (resize).
async fn stream_pty(socket: WebSocket, pty: Arc<crate::pty::Pty>) {
    let (mut tx, mut rx) = socket.split();
    let (scrollback, mut output) = pty.attach();
    if !scrollback.is_empty() && tx.send(Message::Binary(scrollback)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            out = output.recv() => match out {
                Ok(bytes) => if tx.send(Message::Binary(bytes)).await.is_err() { return },
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    let _ = tx.send(Message::Close(None)).await;
                    return;
                }
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Err(e) = pty.write(&bytes) {
                        tracing::debug!(error = %e, "terminal input dropped");
                        return;
                    }
                }
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Control>(&text) {
                    Ok(Control::Resize { cols, rows }) => {
                        if let Err(e) = pty.resize(cols, rows) {
                            tracing::debug!(error = %e, "terminal resize failed");
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "unknown terminal control message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn rooms(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.rooms()).into_response()
}

async fn create_room(State(s): S, h: HeaderMap, Query(q): Q, Json(new): Json<NewRoom>) -> Response {
    guarded!(s, h, q);
    s.desk.create_room(new).await.map(Json).into_response()
}

async fn update_room(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<RoomPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update_room(id, p).await.map(Json).into_response()
}

async fn delete_room(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .delete_room(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineQuery {
    token: Option<String>,
    thread: Option<Uuid>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    limit: Option<i64>,
}

async fn room_messages(
    State(s): S,
    h: HeaderMap,
    Query(t): Query<TimelineQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let q = TokenQuery { token: t.token };
    guarded!(s, h, q);
    s.desk
        .room_messages(id, t.thread, t.before, t.limit.unwrap_or(100))
        .await
        .map(Json)
        .into_response()
}

async fn post_message(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(m): Json<NewMessage>,
) -> Response {
    guarded!(s, h, q);
    s.desk.post(id, m).await.map(Json).into_response()
}

#[derive(Deserialize)]
struct SearchQuery {
    token: Option<String>,
    q: String,
}

async fn search(State(s): S, h: HeaderMap, Query(sq): Query<SearchQuery>) -> Response {
    let q = TokenQuery { token: sq.token };
    guarded!(s, h, q);
    s.desk.search(&sq.q).await.map(Json).into_response()
}

pub fn router(
    desk: Desk,
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
    accounts: Arc<dyn Accounts>,
    conversations: Arc<dyn Conversations>,
) -> Router {
    Router::new()
        .route("/api/desk/sessions", get(list).post(create))
        .route("/api/desk/sessions/{id}", patch(update).delete(forget))
        .route("/api/desk/sessions/{id}/stop", post(stop))
        .route("/api/desk/sessions/{id}/resume", post(resume))
        .route("/api/desk/sessions/{id}/approvals/{approval}", post(decide))
        .route("/api/desk/sessions/{id}/pty", get(pty))
        .route("/api/desk/restore", post(restore))
        .route("/api/desk/restore/dismiss", post(dismiss_restore))
        .route("/api/desk/events", get(events))
        .route("/api/desk/rooms", get(rooms).post(create_room))
        .route("/api/desk/rooms/search", get(search))
        .route(
            "/api/desk/rooms/{id}",
            patch(update_room).delete(delete_room),
        )
        .route(
            "/api/desk/rooms/{id}/messages",
            get(room_messages).post(post_message),
        )
        .with_state(Arc::new(ApiState {
            desk,
            auth: Arc::clone(&auth),
        }))
        .merge(voice_router(Arc::clone(&auth), realtime, speech))
        .merge(providers_router(auth, accounts))
        .merge(conversations::router(conversations))
}

struct ProvidersApi {
    auth: Arc<dyn Authorize>,
    accounts: Arc<dyn Accounts>,
}

type P = State<Arc<ProvidersApi>>;

impl IntoResponse for AccountError {
    fn into_response(self) -> Response {
        let status = match &self {
            AccountError::Invalid(_) => StatusCode::BAD_REQUEST,
            AccountError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AccountError::Internal(e) => {
                tracing::error!(error = %e, "desk provider credentials failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

fn unknown_provider(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("no provider named {id:?}") })),
    )
        .into_response()
}

async fn list_providers(State(s): P, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(Catalog::of(&*s.accounts).await).into_response()
}

/// Saves the credentials a provider's fields hold where the hub reads them,
/// and answers the provider as it now stands. What was typed is never logged
/// or echoed.
async fn connect_provider(
    State(s): P,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    body: Result<Json<ConnectBody>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let Some(spec) = providers::spec(&id) else {
        return unknown_provider(&id);
    };
    let fields = body
        .map_err(|_| AccountError::Invalid("the body is {\"fields\": {name: value}}".into()))
        .and_then(|Json(b)| providers::checked(spec, b.fields));
    let fields = match fields {
        Ok(fields) => fields,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = s.accounts.connect(spec, &fields).await {
        return e.into_response();
    }
    tracing::info!(provider = spec.id, "desk provider connected");
    Json(Entry::of(spec, &*s.accounts).await).into_response()
}

/// Removes a provider's credentials (a plan's login included).
async fn disconnect_provider(
    State(s): P,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
) -> Response {
    guarded!(s, h, q);
    let Some(spec) = providers::spec(&id) else {
        return unknown_provider(&id);
    };
    if let Err(e) = s.accounts.disconnect(spec).await {
        return e.into_response();
    }
    tracing::info!(provider = spec.id, "desk provider disconnected");
    Json(Entry::of(spec, &*s.accounts).await).into_response()
}

pub(crate) fn providers_router(auth: Arc<dyn Authorize>, accounts: Arc<dyn Accounts>) -> Router {
    Router::new()
        .route("/api/desk/providers", get(list_providers))
        .route(
            "/api/desk/providers/{id}",
            axum::routing::delete(disconnect_provider),
        )
        .route("/api/desk/providers/{id}/connect", post(connect_provider))
        .with_state(Arc::new(ProvidersApi { auth, accounts }))
}

struct VoiceApi {
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
}

#[derive(Deserialize)]
struct SessionBody {
    engine: Engine,
}

/// A short-lived way into a cloud voice session for the Desk app; the
/// provider key never leaves the vault.
async fn voice_session(
    State(s): State<Arc<VoiceApi>>,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<SessionBody>,
) -> Response {
    guarded!(s, h, q);
    let host = h
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    match s.realtime.grant(b.engine, host).await {
        Ok(grant) => Json(grant).into_response(),
        Err(e) => {
            let status = match e {
                RealtimeError::NoKey(_) => StatusCode::SERVICE_UNAVAILABLE,
                RealtimeError::Upstream(_) => StatusCode::BAD_GATEWAY,
            };
            (status, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct RelayQuery {
    ticket: String,
}

/// The GPT-Live relay; the single-use ticket from `voice_session` is the
/// credential.
async fn voice_relay(
    State(s): State<Arc<VoiceApi>>,
    Query(r): Query<RelayQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !s.realtime.redeem(&r.ticket) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let realtime = Arc::clone(&s.realtime);
    ws.on_upgrade(move |socket| async move { realtime.relay(socket).await })
}

impl IntoResponse for SpeakError {
    fn into_response(self) -> Response {
        let status = match &self {
            SpeakError::Invalid(_) => StatusCode::BAD_REQUEST,
            SpeakError::NoKey(_) => StatusCode::SERVICE_UNAVAILABLE,
            SpeakError::Upstream(e) => {
                tracing::warn!(error = %e, "desk speech synthesis failed");
                StatusCode::BAD_GATEWAY
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Speaks `text` with a vault key; answers the audio itself.
async fn voice_speak(
    State(s): State<Arc<VoiceApi>>,
    h: HeaderMap,
    Query(q): Q,
    body: Result<Json<Speak>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let speak = body
        .map_err(|e| SpeakError::Invalid(e.body_text()))
        .and_then(|Json(speak)| speak.validated());
    let speak = match speak {
        Ok(speak) => speak,
        Err(e) => return e.into_response(),
    };
    match s.speech.speak(&speak).await {
        Ok(audio) => ([(header::CONTENT_TYPE, audio.media_type)], audio.bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(crate) fn voice_router(
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
) -> Router {
    Router::new()
        .route("/api/desk/voice/session", post(voice_session))
        .route("/api/desk/voice/relay", get(voice_relay))
        .route("/api/desk/voice/speak", post(voice_speak))
        .with_state(Arc::new(VoiceApi {
            auth,
            realtime,
            speech,
        }))
}

async fn hook(
    State(desk): State<Desk>,
    Path((id, token)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    match desk.hook(id, &token, &body).await {
        Some(answer) => Json(answer).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Served on its own loopback listener; the per-session token in the URL is
/// the credential.
pub(crate) fn hook_router(desk: Desk) -> Router {
    Router::new()
        .route("/hook/{id}/{token}", post(hook))
        .with_state(desk)
}

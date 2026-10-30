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

use crate::crew::By;
use crate::model::{Decision, NewSession, SessionPatch};
use crate::providers::{self, AccountError, Accounts, Catalog, ConnectBody, Entry};
use crate::pty::Frame;
use crate::realtime::{Engine, Realtime, RealtimeError};
use crate::rooms::{Bound, NewMessage, NewRoom, Page, RoomPatch};
use crate::service::{Desk, DeskError, Import, Layout, NewTerminal, TerminalPatch};
use crate::speech::{Speak, SpeakError, Speech};

/// Decides whether a request may use the desk, and says who it is from: the
/// person its token names, none for one that names no one. Implemented by
/// the hub.
#[async_trait]
pub trait Authorize: Send + Sync {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<Option<String>, StatusCode>;
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
        .map(drop)
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
    s.desk
        .create(new, Some(By::you()))
        .await
        .map(Json)
        .into_response()
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
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn stop(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.stop(id, Some(By::you())).map(Json).into_response()
}

async fn interrupt(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .interrupt(id, Some(By::you()))
        .map(Json)
        .into_response()
}

async fn allow_asked(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .allow_asked(id, Some(By::you()))
        .map(Json)
        .into_response()
}

async fn dismiss_asked(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .dismiss_asked(id, Some(By::you()))
        .map(Json)
        .into_response()
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
    s.desk
        .resume(id, prompt, Some(By::you()))
        .await
        .map(Json)
        .into_response()
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
        .decide(id, approval, b.decision, Some(By::you()))
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct RestoreBody {
    ids: Vec<Uuid>,
}

async fn restore(State(s): S, h: HeaderMap, Query(q): Q, Json(b): Json<RestoreBody>) -> Response {
    guarded!(s, h, q);
    Json(s.desk.restore(&b.ids, Some(By::you())).await).into_response()
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
    Claim,
}

/// A frame as the viewer `me` is sent it: output and snapshots as binary,
/// the terminal's size and this viewer's role as text.
fn pty_message(me: u64, frame: Frame) -> Option<Message> {
    match frame {
        Frame::Bytes(bytes) => Some(Message::Binary(bytes)),
        Frame::Size { cols, rows } => Some(Message::text(
            json!({ "type": "size", "cols": cols, "rows": rows }).to_string(),
        )),
        Frame::Active(active) => Some(Message::text(
            json!({ "type": "role", "active": active == Some(me), "free": active.is_none() })
                .to_string(),
        )),
        Frame::Exit => None,
    }
}

/// Binary frames carry terminal bytes both ways; text frames carry control
/// messages: `resize` and `claim` from the viewer, `size` and `role` to it.
/// Only the active viewer's keys and size reach the terminal; a viewer that
/// falls behind is sent the screen again rather than losing bytes.
async fn stream_pty(socket: WebSocket, pty: Arc<crate::pty::Pty>) {
    let (mut tx, mut rx) = socket.split();
    let (viewer, frames, mut output) = pty.attach();
    let me = viewer.id();
    for frame in frames {
        if let Some(m) = pty_message(me, frame) {
            if tx.send(m).await.is_err() {
                return;
            }
        }
    }
    loop {
        tokio::select! {
            out = output.recv() => match out {
                Ok(Frame::Exit) | Err(RecvError::Closed) => {
                    let _ = tx.send(Message::Close(None)).await;
                    return;
                }
                Ok(frame) => {
                    if let Some(m) = pty_message(me, frame) {
                        if tx.send(m).await.is_err() { return }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let (frames, fresh) = viewer.resync();
                    output = fresh;
                    for frame in frames {
                        if let Some(m) = pty_message(me, frame) {
                            if tx.send(m).await.is_err() { return }
                        }
                    }
                }
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Err(e) = viewer.input(&bytes) {
                        tracing::debug!(error = %e, "terminal input dropped");
                        return;
                    }
                }
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Control>(&text) {
                    Ok(Control::Resize { cols, rows }) => {
                        if let Err(e) = viewer.resize(cols, rows) {
                            tracing::debug!(error = %e, "terminal resize failed");
                        }
                    }
                    Ok(Control::Claim) => viewer.claim(),
                    Err(e) => tracing::debug!(error = %e, "unknown terminal control message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn terminals(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.terminals()).into_response()
}

async fn create_terminal(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(new): Json<NewTerminal>,
) -> Response {
    guarded!(s, h, q);
    s.desk.create_terminal(new).await.map(Json).into_response()
}

async fn update_terminal(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<TerminalPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update_terminal(id, p).map(Json).into_response()
}

async fn close_terminal(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .close_terminal(id)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn reopen_terminal(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.reopen_terminal(id).await.map(Json).into_response()
}

async fn terminal_programs(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
) -> Response {
    guarded!(s, h, q);
    s.desk.terminal_programs(id).await.map(Json).into_response()
}

async fn terminal_pty(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Response {
    guarded!(s, h, q);
    match s.desk.attach_terminal(id) {
        Ok(pty) => ws.on_upgrade(move |socket| stream_pty(socket, pty)),
        Err(e) => e.into_response(),
    }
}

async fn layout(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.layout()).into_response()
}

async fn replace_layout(State(s): S, h: HeaderMap, Query(q): Q, Json(l): Json<Layout>) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|_| Ok(l))
        .await
        .map(Json)
        .into_response()
}

async fn import_layout(State(s): S, h: HeaderMap, Query(q): Q, Json(i): Json<Import>) -> Response {
    guarded!(s, h, q);
    s.desk.import_layout(i).await.map(Json).into_response()
}

#[derive(Deserialize)]
struct NewGroup {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    before: Option<String>,
}

async fn create_group(State(s): S, h: HeaderMap, Query(q): Q, Json(g): Json<NewGroup>) -> Response {
    guarded!(s, h, q);
    let id = g.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    s.desk
        .change_layout(|l| l.create(&id, &g.name, &g.items, g.before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct GroupPatch {
    name: Option<String>,
    collapsed: Option<bool>,
}

async fn update_group(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    Json(p): Json<GroupPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.edit(&id, p.name.as_deref(), p.collapsed))
        .await
        .map(Json)
        .into_response()
}

async fn delete_group(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<String>) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.delete(&id))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize, Default)]
struct Before {
    #[serde(default)]
    before: Option<String>,
}

async fn move_group(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    body: Option<Json<Before>>,
) -> Response {
    guarded!(s, h, q);
    let before = body.map(|Json(b)| b.before).unwrap_or_default();
    s.desk
        .change_layout(|l| l.move_group(&id, before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

async fn put_member(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, item)): Path<(String, String)>,
    body: Option<Json<Before>>,
) -> Response {
    guarded!(s, h, q);
    let before = body.map(|Json(b)| b.before).unwrap_or_default();
    s.desk
        .change_layout(|l| l.put(&item, &id, before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

async fn remove_member(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| match l.group_of(&item) {
            Some(g) if g.id == id => l.remove(&item),
            _ => Err(DeskError::NotFound),
        })
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct Order {
    items: Vec<String>,
}

/// Ungrouped's order as Desk shows it (`Layout::order_ungrouped`): the
/// route for putting an item in Ungrouped or taking it out of a group,
/// since only Desk knows what shows there unlisted.
async fn order_ungrouped(State(s): S, h: HeaderMap, Query(q): Q, Json(o): Json<Order>) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.order_ungrouped(&o.items))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct NameBody {
    name: Option<String>,
}

async fn name_session(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<NameBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk.name_session(id, b.name).map(Json).into_response()
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
    before: Option<Bound>,
    after: Option<Bound>,
    around: Option<Uuid>,
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
    let page = match Page::of(t.thread, t.before, t.after, t.around, t.limit) {
        Ok(page) => page,
        Err(why) => return DeskError::Invalid(why).into_response(),
    };
    s.desk
        .room_messages(id, page)
        .await
        .map(Json)
        .into_response()
}

async fn room_message(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, message)): Path<(Uuid, Uuid)>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .room_message(id, message)
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
    s.desk
        .post(id, m)
        .await
        .map(|posted| {
            let status = match posted.created {
                true => StatusCode::CREATED,
                false => StatusCode::OK,
            };
            (status, Json(posted.message))
        })
        .into_response()
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

/// The sessions' and rooms' routes and the events socket alone.
pub fn sessions_router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/sessions", get(list).post(create))
        .route("/api/desk/sessions/{id}", patch(update).delete(forget))
        .route("/api/desk/sessions/{id}/stop", post(stop))
        .route("/api/desk/sessions/{id}/interrupt", post(interrupt))
        .route("/api/desk/sessions/{id}/asked/allow", post(allow_asked))
        .route("/api/desk/sessions/{id}/asked/dismiss", post(dismiss_asked))
        .route("/api/desk/sessions/{id}/resume", post(resume))
        .route("/api/desk/sessions/{id}/approvals/{approval}", post(decide))
        .route("/api/desk/sessions/{id}/pty", get(pty))
        .route(
            "/api/desk/sessions/{id}/name",
            axum::routing::put(name_session),
        )
        .route("/api/desk/layout", get(layout).put(replace_layout))
        .route("/api/desk/layout/import", post(import_layout))
        .route("/api/desk/groups", get(layout).post(create_group))
        .route(
            "/api/desk/groups/{id}",
            patch(update_group).delete(delete_group),
        )
        .route("/api/desk/groups/{id}/move", post(move_group))
        .route(
            "/api/desk/groups/ungrouped/order",
            axum::routing::put(order_ungrouped),
        )
        .route(
            "/api/desk/groups/{id}/items/{*item}",
            axum::routing::put(put_member).delete(remove_member),
        )
        .route("/api/desk/terminals", get(terminals).post(create_terminal))
        .route(
            "/api/desk/terminals/{id}",
            patch(update_terminal).delete(close_terminal),
        )
        .route("/api/desk/terminals/{id}/reopen", post(reopen_terminal))
        .route("/api/desk/terminals/{id}/programs", get(terminal_programs))
        .route("/api/desk/terminals/{id}/pty", get(terminal_pty))
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
        .route("/api/desk/rooms/{id}/messages/{message}", get(room_message))
        .with_state(Arc::new(ApiState { desk, auth }))
}

pub fn router(
    desk: Desk,
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
    accounts: Arc<dyn Accounts>,
    crew: Arc<dyn crate::crew::Crew>,
) -> Router {
    desk.read_replies_with(&crew);
    sessions_router(desk.clone(), Arc::clone(&auth))
        .merge(crate::state::router(desk.clone(), Arc::clone(&auth)))
        .merge(crate::crew_api::crew_router(desk, Arc::clone(&auth), crew))
        .merge(voice_router(Arc::clone(&auth), realtime, speech))
        .merge(providers_router(auth, accounts))
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

async fn terminal_hook(
    State(desk): State<Desk>,
    Path((terminal, token)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    match desk.terminal_hook(terminal, &token, &body).await {
        Some(answer) => Json(answer).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Served on its own loopback listener; the per-session token in the URL is
/// the credential.
pub(crate) fn hook_router(desk: Desk) -> Router {
    Router::new()
        .route("/hook/{id}/{token}", post(hook))
        .route("/hook/terminal/{terminal}/{token}", post(terminal_hook))
        .with_state(desk)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message as Ws;
    use workforce_postgres::test_support::with_test_pool;

    use crate::service::NewTerminal;
    use crate::store::{build_pool, DeskStore};
    use crate::testing::{allow_all, options};

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// The next text frame of `kind`, skipping output.
    async fn next_text(ws: &mut Client, kind: &str) -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("a frame in time")
                .unwrap()
                .unwrap();
            if let Ws::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        }
    }

    /// Output until `needle` shows up.
    async fn output_until(ws: &mut Client, needle: &str) -> String {
        let mut seen = String::new();
        while !seen.contains(needle) {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("output in time")
                .unwrap()
                .unwrap();
            if let Ws::Binary(b) = m {
                seen.push_str(&String::from_utf8_lossy(&b));
            }
        }
        seen
    }

    #[tokio::test]
    async fn two_viewers_one_types_the_other_mirrors_until_it_claims() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
            opts.env = BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SHELL".into(), "/bin/sh".into()),
            ]);
            DeskStore::migrate(&pool).await.unwrap();
            let desk = crate::Desk::start(
                DeskStore::from_pool(pool),
                opts,
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let t = desk
                .create_terminal(NewTerminal {
                    id: None,
                    name: None,
                    cwd: dir.path().to_string_lossy().into_owned(),
                    env: BTreeMap::new(),
                })
                .await
                .unwrap();
            let app = super::sessions_router(desk.clone(), allow_all());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("ws://{addr}/api/desk/terminals/{}/pty", t.id);

            let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let size = next_text(&mut first, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(120), Some(40))
            );
            assert_eq!(next_text(&mut first, "role").await["active"], true);
            let (mut second, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let role = next_text(&mut second, "role").await;
            assert_eq!(
                (role["active"].as_bool(), role["free"].as_bool()),
                (Some(false), Some(false))
            );

            // The mirror's keys and size go nowhere.
            second
                .send(Ws::Binary("echo from-mirror\n".into()))
                .await
                .unwrap();
            second
                .send(Ws::text(
                    json!({ "type": "resize", "cols": 50, "rows": 10 }).to_string(),
                ))
                .await
                .unwrap();
            first
                .send(Ws::text(
                    json!({ "type": "resize", "cols": 100, "rows": 30 }).to_string(),
                ))
                .await
                .unwrap();
            let size = next_text(&mut second, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(100), Some(30))
            );
            first
                .send(Ws::Binary("echo from-first\n".into()))
                .await
                .unwrap();
            let seen = output_until(&mut second, "from-first\r\n").await;
            assert!(!seen.contains("from-mirror"));

            // Claiming makes it the one that types, at the size it asked for.
            second.send(Ws::text(r#"{"type":"claim"}"#)).await.unwrap();
            assert_eq!(next_text(&mut second, "role").await["active"], true);
            assert_eq!(next_text(&mut first, "role").await["active"], false);
            let size = next_text(&mut first, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(50), Some(10))
            );
            second
                .send(Ws::Binary("echo now-second\n".into()))
                .await
                .unwrap();
            output_until(&mut first, "now-second\r\n").await;
            desk.close_terminal(t.id).unwrap();
        })
        .await;
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(axum::body::Body::empty, |b| {
                axum::body::Body::from(b.to_string())
            }))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn names_and_groups_through_the_routes() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = crate::store::tests::record_fixture("monorepo-fd");
            store.put(&r).await.unwrap();
            let desk = crate::Desk::start(store, options(dir.path()), Default::default(), tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let s = format!("session:{}", r.id);

            let (status, v) = call(&app, "PUT", &format!("/api/desk/sessions/{}/name", r.id), Some(json!({ "name": "Ledger migration" }))).await;
            assert_eq!((status, &v["displayName"]), (200, &json!("Ledger migration")));
            let (_, v) = call(&app, "PATCH", &format!("/api/desk/sessions/{}", r.id), Some(json!({ "displayName": null }))).await;
            assert_eq!(v["displayName"], Value::Null);
            assert_eq!(v["name"], "monorepo-fd");

            let (status, l) = call(&app, "POST", "/api/desk/groups", Some(json!({ "id": "g1", "name": "Atlas" }))).await;
            assert_eq!((status, &l["groups"][0]["name"]), (200, &json!("Atlas")));
            call(&app, "POST", "/api/desk/groups", Some(json!({ "id": "g2", "name": "Infra", "items": ["agent:dw/allan"] }))).await;
            let (status, l) = call(&app, "PUT", &format!("/api/desk/groups/g1/items/{s}"), Some(json!({ "before": null }))).await;
            assert_eq!((status, &l["groups"][0]["items"]), (200, &json!([s])));
            let (_, v) = call(&app, "GET", "/api/desk/sessions", None).await;
            assert_eq!(v[0]["group"], json!({ "id": "g1", "name": "Atlas" }));
            let (_, l) = call(&app, "POST", "/api/desk/groups/g2/move", Some(json!({ "before": "g1" }))).await;
            assert_eq!(l["groups"][0]["id"], "g2");
            let (_, l) = call(&app, "PATCH", "/api/desk/groups/g1", Some(json!({ "name": "Atlas work", "collapsed": true }))).await;
            assert_eq!((&l["groups"][1]["name"], &l["groups"][1]["collapsed"]), (&json!("Atlas work"), &json!(true)));
            let (_, l) = call(&app, "PATCH", "/api/desk/groups/ungrouped", Some(json!({ "collapsed": true }))).await;
            assert_eq!(l["ungrouped"]["collapsed"], true);
            let (status, _) = call(&app, "PATCH", "/api/desk/groups/ungrouped", Some(json!({ "name": "x" }))).await;
            assert_eq!(status, 400);
            let (status, l) = call(&app, "DELETE", &format!("/api/desk/groups/g1/items/{s}"), None).await;
            assert_eq!((status, &l["ungrouped"]["items"]), (200, &json!([s])));
            let (status, _) = call(&app, "DELETE", "/api/desk/groups/g1/items/agent:dw/allan", None).await;
            assert_eq!(status, 404, "not in that group");
            let (status, l) = call(&app, "DELETE", "/api/desk/groups/g2", None).await;
            assert_eq!(status, 200);
            assert_eq!(l["ungrouped"]["items"], json!([s, "agent:dw/allan"]));
            let (status, _) = call(&app, "POST", "/api/desk/groups", Some(json!({ "name": "  " }))).await;
            assert_eq!(status, 400);
            let (_, got) = call(&app, "GET", "/api/desk/layout", None).await;
            assert_eq!(got["groups"].as_array().unwrap().len(), 1);
            let (status, l) = call(&app, "PUT", "/api/desk/layout", Some(json!({ "groups": [], "ungrouped": { "collapsed": false, "items": [] } }))).await;
            assert_eq!((status, l["groups"].as_array().unwrap().len()), (200, 0));
            let (status, i) = call(&app, "POST", "/api/desk/layout/import", Some(json!({
                "layout": { "groups": [{ "id": "m", "name": "Atlas", "collapsed": false, "items": [s] }], "ungrouped": { "collapsed": false, "items": [] } },
                "sessionNames": { r.id.to_string(): "Ledger migration" }
            }))).await;
            assert_eq!((status, &i["imported"]), (200, &json!(true)));
            assert_eq!(i["sessionNames"][r.id.to_string()], "Ledger migration");

            // Ungrouped's order as Desk settled it: taken out of its group.
            let (status, l) = call(&app, "PUT", "/api/desk/groups/ungrouped/order", Some(json!({ "items": ["agent:dw/allan", s, "agent:local/scout"] }))).await;
            assert_eq!((status, &l["ungrouped"]["items"]), (200, &json!(["agent:dw/allan", s, "agent:local/scout"])));
            assert_eq!(l["groups"][0]["items"], json!([]));
            let (status, _) = call(&app, "PUT", "/api/desk/groups/ungrouped/order", Some(json!({ "items": ["nonsense"] }))).await;
            assert_eq!(status, 400);
            // Still a group's own route: a group may not be called "ungrouped".
            let (status, l) = call(&app, "PUT", &format!("/api/desk/groups/m/items/{s}"), Some(json!({}))).await;
            assert_eq!((status, &l["groups"][0]["items"]), (200, &json!([s])));

            // Forgotten, the session leaves the layout.
            let (status, _) = call(&app, "DELETE", &format!("/api/desk/sessions/{}", r.id), None).await;
            assert_eq!(status, 204);
            let (_, l) = call(&app, "GET", "/api/desk/layout", None).await;
            assert_eq!((&l["groups"][0]["items"], &l["ungrouped"]["items"]), (&json!([]), &json!(["agent:dw/allan", "agent:local/scout"])));
        })
        .await;
    }

    #[tokio::test]
    async fn a_sourced_room_post_is_created_once_and_repeats_answer_with_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(store, options(dir.path()), Default::default(), tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (status, room) = call(&app, "POST", "/api/desk/rooms", Some(json!({ "name": "Release", "members": [{ "hub": "local", "agent": "scout" }] }))).await;
            assert_eq!(status, 200);
            let channel = format!("desk:{}", room["id"].as_str().unwrap());
            assert_eq!(room["channel"], json!(channel));
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let reply = json!({ "author": "local/scout", "content": "may I?", "source": "local/scout/m-1", "delivery": "approval", "requestId": "req-1" });
            let (status, first) = call(&app, "POST", &uri, Some(reply.clone())).await;
            assert_eq!(
                (status, &first["source"], &first["delivery"], &first["requestId"]),
                (201, &json!("local/scout/m-1"), &json!("approval"), &json!("req-1"))
            );
            let (status, again) = call(&app, "POST", &uri, Some(reply)).await;
            assert_eq!((status, &again["id"]), (200, &first["id"]));
            let (status, said) = call(&app, "POST", &uri, Some(json!({ "author": "you", "content": "thanks" }))).await;
            assert_eq!((status, said.get("source"), said.get("delivery")), (201, None, None));
        })
        .await;
    }

    #[tokio::test]
    async fn room_pages_by_before_after_and_around_and_one_message_reads_alone() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (_, room) = call(
                &app,
                "POST",
                "/api/desk/rooms",
                Some(json!({ "name": "Atlas launch" })),
            )
            .await;
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let mut ids = Vec::new();
            for i in 0..5 {
                let (_, m) = call(
                    &app,
                    "POST",
                    &uri,
                    Some(json!({ "author": "you", "content": format!("line {i}") })),
                )
                .await;
                ids.push(m["id"].as_str().unwrap().to_string());
            }
            let ids_of = |v: &Value| {
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            };
            let (status, v) = call(
                &app,
                "GET",
                &format!("{uri}?before={}&limit=2", ids[3]),
                None,
            )
            .await;
            assert_eq!((status, ids_of(&v)), (200, ids[1..3].to_vec()));
            let (_, v) = call(&app, "GET", &format!("{uri}?after={}", ids[3]), None).await;
            assert_eq!(ids_of(&v), ids[4..].to_vec());
            let (_, v) = call(
                &app,
                "GET",
                &format!("{uri}?around={}&limit=3", ids[2]),
                None,
            )
            .await;
            assert_eq!(ids_of(&v), ids[1..4].to_vec());
            let (_, v) = call(
                &app,
                "GET",
                &format!("{uri}?before=2000-01-01T00:00:00Z"),
                None,
            )
            .await;
            assert_eq!(
                ids_of(&v),
                Vec::<String>::new(),
                "a plain time still reads as one"
            );
            let (status, _) = call(
                &app,
                "GET",
                &format!("{uri}?before={}", uuid::Uuid::new_v4()),
                None,
            )
            .await;
            assert_eq!(status, 404, "a message that isn't the room's");

            let (status, one) = call(&app, "GET", &format!("{uri}/{}", ids[2]), None).await;
            assert_eq!(
                (status, &one["content"], &one["forYou"]),
                (200, &json!("line 2"), &json!(false))
            );
            let (status, _) = call(
                &app,
                "GET",
                &format!("{uri}/{}", uuid::Uuid::new_v4()),
                None,
            )
            .await;
            assert_eq!(status, 404);
        })
        .await;
    }

    #[tokio::test]
    async fn two_spans_at_once_are_refused() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (_, room) = call(
                &app,
                "POST",
                "/api/desk/rooms",
                Some(json!({ "name": "Atlas launch" })),
            )
            .await;
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let (_, m) = call(
                &app,
                "POST",
                &uri,
                Some(json!({ "author": "you", "content": "hi" })),
            )
            .await;
            let id = m["id"].as_str().unwrap();
            let (status, v) =
                call(&app, "GET", &format!("{uri}?before={id}&around={id}"), None).await;
            assert_eq!(
                (status, &v["error"]),
                (400, &json!("ask for one of before, after or around"))
            );
        })
        .await;
    }
}

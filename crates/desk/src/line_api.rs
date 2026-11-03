//! Desk's lines over HTTP. On the hook listener, where a session's own
//! per-launch token is the credential: the routes Desk's Claude Code
//! plugin uses (`/line/…`) and the MCP endpoint other engines use
//! (`/mcp/…`). On the API, for the person: a session's thread, the
//! composer, editing what hasn't gone out, and answering.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::Authorize;
use crate::line::{When, TOOLS};
use crate::service::{Desk, LineError, Sender};

/// How long one plugin request may wait: a mod's fetch is cut off around
/// 30 s, so a longer wait is asked again (`more`).
const STEP: Duration = Duration::from_secs(25);
/// How long the plugin's poll waits for the next message.
const NEXT_WAIT: Duration = Duration::from_secs(20);

impl IntoResponse for LineError {
    fn into_response(self) -> Response {
        match self {
            LineError::Refused(r) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": r.to_string() })),
            )
                .into_response(),
            LineError::Desk(e) => e.into_response(),
        }
    }
}

/// The session a hook-listener path speaks for: `{id}/{token}`, or a
/// terminal's `terminal/{terminal}/{token}`.
fn caller(desk: &Desk, p: &HashMap<String, String>) -> Option<Uuid> {
    let token = p.get("token")?;
    match (p.get("terminal"), p.get("id")) {
        (Some(t), _) => desk.terminal_line_caller(t.parse().ok()?, token),
        (None, Some(id)) => desk.line_caller(id.parse().ok()?, token),
        (None, None) => None,
    }
}

macro_rules! caller {
    ($desk:expr, $p:expr) => {
        match caller(&$desk, &$p) {
            Some(id) => id,
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    };
}

type P = Path<HashMap<String, String>>;

async fn tools(State(desk): State<Desk>, Path(p): P) -> Response {
    let id = caller!(desk, p);
    desk.plugin_heard_from(id);
    Json(json!({ "tools": TOOLS.iter().map(|t| t.definition()).collect::<Vec<_>>() }))
        .into_response()
}

#[derive(Deserialize)]
struct CallBody {
    name: String,
    #[serde(default)]
    input: Value,
}

/// A tool call from Desk's plugin: `{ result }`, or `{ error }` for the
/// session to read.
async fn call(State(desk): State<Desk>, Path(p): P, Json(b): Json<CallBody>) -> Response {
    let id = caller!(desk, p);
    let name = b.name.strip_prefix("mcp__desk__").unwrap_or(&b.name);
    match desk.line_call(id, name, &b.input, Some(STEP)).await {
        Ok(result) => Json(json!({ "result": result })),
        Err(error) => Json(json!({ "error": error })),
    }
    .into_response()
}

#[derive(Deserialize)]
struct WaitQuery {
    wait: Option<u64>,
}

async fn await_answer(
    State(desk): State<Desk>,
    Path(p): P,
    Query(q): Query<WaitQuery>,
) -> Response {
    let id = caller!(desk, p);
    let Some(ask) = p.get("ask").and_then(|a| a.parse().ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let step = q.wait.map_or(STEP, Duration::from_secs).min(STEP);
    match desk.line_await(id, ask, step).await {
        Ok(result) => Json(json!({ "result": result })),
        Err(error) => Json(json!({ "error": error })),
    }
    .into_response()
}

/// The plugin's long poll: the next message to submit, handed to it, or
/// `204` when none came in time.
async fn next(State(desk): State<Desk>, Path(p): P, Query(q): Query<WaitQuery>) -> Response {
    let id = caller!(desk, p);
    let wait = q.wait.map_or(NEXT_WAIT, Duration::from_secs).min(NEXT_WAIT);
    match desk.line_next(id, wait).await {
        Some(next) => Json(next).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
struct AckBody {
    id: Uuid,
    ok: bool,
}

async fn ack(State(desk): State<Desk>, Path(p): P, Json(b): Json<AckBody>) -> Response {
    let id = caller!(desk, p);
    desk.line_ack(id, b.id, b.ok)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

/// MCP on a session's line, one JSON-RPC request per POST; a notification
/// answers 202 with no body.
async fn mcp(State(desk): State<Desk>, Path(p): P, body: String) -> Response {
    let id = caller!(desk, p);
    match desk.line_mcp(id, &body).await {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(None) => StatusCode::ACCEPTED.into_response(),
        Some(Some(answer)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            answer,
        )
            .into_response(),
    }
}

/// The line's routes on the hook listener.
pub(crate) fn hook_routes() -> Router<Desk> {
    let mut r = Router::new();
    for at in ["/line/{id}/{token}", "/line/terminal/{terminal}/{token}"] {
        r = r
            .route(&format!("{at}/tools"), get(tools))
            .route(&format!("{at}/call"), post(call))
            .route(&format!("{at}/await/{{ask}}"), get(await_answer))
            .route(&format!("{at}/next"), get(next))
            .route(&format!("{at}/ack"), post(ack));
    }
    r.route("/mcp/{id}/{token}", post(mcp))
        .route("/mcp/terminal/{terminal}/{token}", post(mcp))
}

struct Api {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<Api>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn allowed(s: &Api, h: &HeaderMap, token: Option<&str>) -> Result<(), Response> {
    s.auth
        .authorize(h, token)
        .await
        .map(drop)
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

macro_rules! guarded {
    ($s:expr, $h:expr, $t:expr) => {
        if let Err(r) = allowed(&$s, &$h, $t).await {
            return r;
        }
    };
}

#[derive(Deserialize)]
struct ThreadQuery {
    token: Option<String>,
    before: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

async fn thread(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<ThreadQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    guarded!(s, h, q.token.as_deref());
    s.desk
        .line_thread(id, q.before, q.limit)
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct PostBody {
    text: String,
    #[serde(default = "queue")]
    when: When,
}

fn queue() -> When {
    When::Queue
}

async fn post_line(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(b): Json<PostBody>,
) -> Response {
    guarded!(s, h, q.token.as_deref());
    s.desk
        .line_post(id, b.text, b.when)
        .await
        .map(|m| (StatusCode::CREATED, Json(m)))
        .into_response()
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

async fn edit(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(b): Json<TextBody>,
) -> Response {
    guarded!(s, h, q.token.as_deref());
    s.desk
        .line_edit(id, Some(b.text))
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn take_back(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    guarded!(s, h, q.token.as_deref());
    s.desk
        .line_edit(id, None)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn answer(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(b): Json<TextBody>,
) -> Response {
    guarded!(s, h, q.token.as_deref());
    s.desk
        .line_answer(id, Sender::Person, b.text)
        .await
        .map(|m| (StatusCode::CREATED, Json(m)))
        .into_response()
}

/// The person's line routes.
pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/sessions/{id}/line", get(thread).post(post_line))
        .route("/api/desk/line/{id}", patch(edit).delete(take_back))
        .route("/api/desk/line/{id}/answer", post(answer))
        .with_state(Arc::new(Api { desk, auth }))
}

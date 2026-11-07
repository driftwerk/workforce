//! Plans over HTTP, for the person: the list, one in full, their edit,
//! approving, pausing, resuming and dropping. A boss proposes and changes
//! plans with its crew tools, never here.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::Authorize;
use crate::crew::By;
use crate::engine::Engine;
use crate::plan::{Paused, PlanState};
use crate::service::{Desk, DeskError, OnDrop, PlanEdit};

struct Api {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<Api>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

type Q = Query<TokenQuery>;

macro_rules! guarded {
    ($s:expr, $h:expr, $q:expr) => {
        if let Err(status) = $s.auth.authorize(&$h, $q.token.as_deref()).await {
            return (status, Json(json!({ "error": "not allowed" }))).into_response();
        }
    };
}

fn bad(why: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// A group's id, or `""` for the ungrouped.
    #[serde(default)]
    group: Option<String>,
}

/// Plans with where their tickets stand.
async fn list(State(s): S, h: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return (status, Json(json!({ "error": "not allowed" }))).into_response();
    }
    let state = match q.state.as_deref().filter(|s| !s.is_empty()) {
        Some(st) => match PlanState::parse(st) {
            Ok(st) => Some(st),
            Err(_) => {
                return bad("state is proposed, running, paused, done or dropped");
            }
        },
        None => None,
    };
    s.desk
        .plans(state, q.group.as_deref())
        .await
        .map(Json)
        .into_response()
}

/// A plan, its tickets, its waves and its revisions.
async fn read(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.plan(id).await.map(Json).into_response()
}

/// The person's edit, made from `revision`: refused with 409 when the
/// plan has changed since.
async fn edit(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(e): Json<PlanEdit>,
) -> Response {
    guarded!(s, h, q);
    if e.revision.is_none() {
        return bad("say the revision the edit was made from");
    }
    // A ticket the person adds with no engine runs on the plan's first's.
    let engine = match s.desk.plan(id).await {
        Ok(p) => p
            .tickets
            .iter()
            .find_map(|t| t.engine)
            .unwrap_or(Engine::Claude),
        Err(e) => return e.into_response(),
    };
    s.desk
        .change_plan(id, e, engine, false, &By::you())
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct Approve {
    revision: i32,
    /// A cap on the plan's own spend, which comes with plan budgets.
    #[serde(default)]
    budget: Option<Value>,
}

/// **Approve and run**, as of the revision the person read.
async fn approve(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<Approve>,
) -> Response {
    guarded!(s, h, q);
    if b.budget.is_some_and(|v| !v.is_null()) {
        return DeskError::Invalid(
            "A plan can't have a budget of its own yet; set its group's budget instead.".into(),
        )
        .into_response();
    }
    s.desk
        .approve_plan(id, b.revision, &By::you())
        .await
        .map(Json)
        .into_response()
}

async fn pause(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .pause_plan(id, Paused::Person, &By::you())
        .await
        .map(Json)
        .into_response()
}

/// Resumes a plan the person paused, or one waiting on them since its
/// group lost its boss.
async fn resume(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .resume_plan(id, &[Paused::Person, Paused::NoBoss], &By::you())
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct DropBody {
    /// What becomes of the sessions working on it: `stop` or `leave`.
    sessions: OnDrop,
}

async fn drop_plan(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    body: Option<Json<Value>>,
) -> Response {
    guarded!(s, h, q);
    let Some(Ok(b)) = body.map(|Json(v)| serde_json::from_value::<DropBody>(v)) else {
        return bad("say what becomes of its sessions: { \"sessions\": \"stop\" | \"leave\" }");
    };
    s.desk
        .drop_plan(id, b.sessions, &By::you())
        .await
        .map(Json)
        .into_response()
}

/// The person's plan routes.
pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/plans", get(list))
        .route("/api/desk/plans/{id}", get(read).patch(edit))
        .route("/api/desk/plans/{id}/approve", post(approve))
        .route("/api/desk/plans/{id}/pause", post(pause))
        .route("/api/desk/plans/{id}/resume", post(resume))
        .route("/api/desk/plans/{id}/drop", post(drop_plan))
        .with_state(Arc::new(Api { desk, auth }))
}

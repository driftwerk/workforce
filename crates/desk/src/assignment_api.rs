//! A boss's assignments over HTTP, for the Floor: who holds which job
//! (`GET /api/desk/assignments?boss=`), and the person closing one
//! (`POST /api/desk/assignments/{id}/close`), and letting a held one go
//! once they checked its old holder didn't start it
//! (`POST /api/desk/assignments/{id}/release`). A boss reads, closes and
//! lets go its own with its crew tools, never here.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use workforce_surface::routing::{get, post};
use workforce_surface::Routes;

use crate::api::Authorize;
use crate::assignment::{self, AssignmentState};
use crate::crew::{Event, Kind, Party};
use crate::service::Desk;

struct Api {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<Api>>;

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    boss: Option<String>,
    /// Those of the last day that closed too.
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct CloseBody {
    /// `done` or `cancelled`.
    state: String,
    #[serde(default)]
    note: Option<String>,
}

fn failed(e: eyre::Report) -> Response {
    tracing::warn!(error = %format!("{e:#}"), "the assignments couldn't be read or changed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Desk couldn't reach its assignments just now." })),
    )
        .into_response()
}

/// `boss`'s assignments, newest first: the live ones, or with `all` those
/// of the last day too.
async fn list(State(s): S, h: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return (status, Json(json!({ "error": "not allowed" }))).into_response();
    }
    let Some(boss) = q.boss.as_deref().map(str::trim).filter(|b| !b.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "say whose: ?boss=<agent id>" })),
        )
            .into_response();
    };
    match s.desk.store().assignments_of(boss, !q.all).await {
        Ok(a) => Json(json!({ "assignments": a })).into_response(),
        Err(e) => failed(e),
    }
}

/// The person closes an assignment: done, or cancelled.
async fn close(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(b): Json<CloseBody>,
) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return (status, Json(json!({ "error": "not allowed" }))).into_response();
    }
    let state = match b.state.as_str() {
        "done" => AssignmentState::Done,
        "cancelled" => AssignmentState::Cancelled,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "state is done or cancelled" })),
            )
                .into_response()
        }
    };
    let why = b
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("You closed it.");
    match s
        .desk
        .store()
        .assignment_close(id, state, "person", why)
        .await
    {
        Ok(Some(a)) => {
            s.desk.note(Event::new(
                Kind::AssignmentClosed,
                Party::you(),
                None,
                format!(
                    "closed {} ({}) as {}",
                    assignment::short(a.id),
                    a.about(),
                    state.as_str()
                ),
            ));
            Json(json!({ "assignment": a })).into_response()
        }
        Ok(None) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "That assignment is closed already." })),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct ReleaseBody {
    #[serde(default)]
    note: Option<String>,
}

/// The person lets a held assignment go: they checked its old holder
/// didn't start it. The boss's loop sends it, checked as any send.
async fn release(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(b): Json<ReleaseBody>,
) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return (status, Json(json!({ "error": "not allowed" }))).into_response();
    }
    let note = b
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("The person checked it and let it go.");
    match s.desk.store().assignment_let_go(id, note).await {
        Ok(true) => {
            s.desk.note(Event::new(
                Kind::AssignmentClosed,
                Party::you(),
                None,
                format!("let {} go: {note}", assignment::short(id)),
            ));
            Json(json!({ "released": true })).into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "That assignment isn't held." })),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

/// The assignments' routes.
pub(crate) fn api_routes(ground: crate::api::Ground) -> Routes {
    Routes::new()
        .admin("/api/desk/assignments", get(list))
        .admin("/api/desk/assignments/{id}/close", post(close))
        .admin("/api/desk/assignments/{id}/release", post(release))
        .bind(ground.map(|(desk, auth)| Arc::new(Api { desk, auth })))
}

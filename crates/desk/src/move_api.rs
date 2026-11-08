//! Moves over HTTP, for the person (boss-gaps W8): preview what moving an
//! item to another group does, read its handover note as its old boss
//! drafts it, and make the move as previewed.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::api::Authorize;
use crate::service::{Desk, MoveAsk, MoveCommit};

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

/// What moving an item would do, with its draft handover note.
async fn preview(State(s): S, h: HeaderMap, Query(q): Q, Json(ask): Json<MoveAsk>) -> Response {
    guarded!(s, h, q);
    s.desk.preview_move(ask).await.map(Json).into_response()
}

/// Makes a previewed move: 409 when it changed since or ran out.
async fn commit(State(s): S, h: HeaderMap, Query(q): Q, Json(c): Json<MoveCommit>) -> Response {
    guarded!(s, h, q);
    s.desk.commit_move(c).await.map(Json).into_response()
}

/// A handover note as it stands, while its old boss drafts it.
async fn handover(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.handover_view(id).await.map(Json).into_response()
}

pub fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/moves/preview", post(preview))
        .route("/api/desk/moves", post(commit))
        .route("/api/desk/handovers/{id}", get(handover))
        .with_state(Arc::new(Api { desk, auth }))
}

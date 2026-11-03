//! Charges over HTTP, for the person: which boss runs which session,
//! handing one to the boss and taking it back, and the wheel.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::api::Authorize;
use crate::charge::How;
use crate::crew::{By, Party};
use crate::service::{Desk, DeskError};

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

/// Every live charge, with its boss's name and who has the wheel.
async fn list(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.charges()).into_response()
}

#[derive(Deserialize, Default)]
struct HandBody {
    /// The boss to hand it to; the one there is when not said.
    #[serde(default)]
    boss: Option<String>,
}

/// The person hands a session to its boss.
async fn hand(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    body: Option<Json<HandBody>>,
) -> Response {
    guarded!(s, h, q);
    let Json(b) = body.unwrap_or_default();
    let Some(boss) = s.desk.boss() else {
        return DeskError::Conflict("No agent oversees the crew to hand it to.".into())
            .into_response();
    };
    if boss.level == "watch" {
        return DeskError::Conflict(format!(
            "{} only watches; set them to Act to run work.",
            boss.name
        ))
        .into_response();
    }
    if b.boss.as_deref().is_some_and(|wanted| wanted != boss.id) {
        return DeskError::Conflict(format!(
            "{} runs the sessions on this Desk; hand it to them.",
            boss.name
        ))
        .into_response();
    }
    let party = Party::manager(&boss.id, &boss.name);
    s.desk
        .take_on(id, &party, How::Handed, &HashSet::new(), &By::you())
        .await
        .map(Json)
        .into_response()
}

/// The person takes a session back from its boss.
async fn release(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .release(id, &By::you())
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct WheelBody {
    wheel: String,
}

/// **I'll drive** (`person_pinned`) or **Hand back** (`boss`).
async fn wheel(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<WheelBody>,
) -> Response {
    guarded!(s, h, q);
    match b.wheel.as_str() {
        "person_pinned" => s.desk.drive(id).await.map(Json).into_response(),
        "boss" => s.desk.hand_back(id).await.map(Json).into_response(),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "wheel is person_pinned or boss" })),
        )
            .into_response(),
    }
}

/// The person's charge routes.
pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/charges", get(list))
        .route(
            "/api/desk/sessions/{id}/charge",
            post(hand).delete(release).patch(wheel),
        )
        .with_state(Arc::new(Api { desk, auth }))
}

//! The board over HTTP, for the person: boards, their tickets, moves,
//! dependencies, assignment, pin and snooze, search and history.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::api::Authorize;
use crate::crew::By;
use crate::service::{
    AssignTo, BoardPatch, Desk, DeskError, Move, NewBoard, NewTicket, TicketPatch, TicketQuery,
};

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

/// The answer to a board request; what isn't there is a ticket or a board.
fn reply<T: Serialize>(r: Result<T, DeskError>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(DeskError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "There's no such ticket or board." })),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

fn done(r: Result<(), DeskError>) -> Response {
    match r {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => reply::<()>(Err(e)),
    }
}

async fn boards(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    reply(s.desk.boards().await)
}

async fn create_board(State(s): S, h: HeaderMap, Query(q): Q, Json(b): Json<NewBoard>) -> Response {
    guarded!(s, h, q);
    reply(s.desk.create_board(b).await)
}

async fn update_board(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<BoardPatch>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.update_board(id, b).await)
}

async fn delete_board(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    done(s.desk.delete_board(id, &By::you()).await)
}

/// The board's tickets, with `ready` and `blockers` computed.
async fn tickets(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Query(t): Query<TicketQuery>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.tickets(id, &t).await)
}

async fn create_ticket(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(t): Json<NewTicket>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.create_ticket(t, &By::you()).await)
}

async fn ticket(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    reply(s.desk.ticket(id).await)
}

async fn edit_ticket(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<TicketPatch>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.edit_ticket(id, p, &By::you()).await)
}

async fn delete_ticket(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    done(s.desk.delete_ticket(id, &By::you()).await)
}

/// A drop anywhere in a column.
async fn move_ticket(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(m): Json<Move>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.move_ticket(id, m, &By::you()).await)
}

async fn add_need(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, other)): Path<(Uuid, Uuid)>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.need_ticket(id, other, true, &By::you()).await)
}

async fn remove_need(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, other)): Path<(Uuid, Uuid)>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.need_ticket(id, other, false, &By::you()).await)
}

#[derive(Deserialize)]
struct AssignBody {
    /// Whom to; `null` takes it off whoever has it.
    to: Option<AssignTo>,
}

async fn assign(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<AssignBody>,
) -> Response {
    guarded!(s, h, q);
    reply(s.desk.assign_ticket(id, b.to, &By::you()).await)
}

#[derive(Deserialize)]
struct Page {
    before: Option<String>,
    #[serde(default = "page")]
    limit: i64,
}

fn page() -> i64 {
    50
}

async fn ticket_history(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Query(p): Query<Page>,
) -> Response {
    guarded!(s, h, q);
    reply(
        s.desk
            .ticket_history(id, p.before.as_deref(), p.limit)
            .await,
    )
}

#[derive(Deserialize)]
struct Subject {
    subject: String,
}

/// What a session or an agent did, and had done to it.
async fn history(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Query(of): Query<Subject>,
    Query(p): Query<Page>,
) -> Response {
    guarded!(s, h, q);
    reply(
        s.desk
            .history_of(&of.subject, p.before.as_deref(), p.limit)
            .await,
    )
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
}

/// Tickets for the palette.
async fn search(State(s): S, h: HeaderMap, Query(q): Q, Query(f): Query<Search>) -> Response {
    guarded!(s, h, q);
    reply(s.desk.search_tickets(&f.q).await)
}

/// The person's board routes.
pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/boards", get(boards).post(create_board))
        .route(
            "/api/desk/boards/{id}",
            axum::routing::patch(update_board).delete(delete_board),
        )
        .route("/api/desk/boards/{id}/tickets", get(tickets))
        .route("/api/desk/tickets", post(create_ticket))
        .route("/api/desk/tickets/search", get(search))
        .route(
            "/api/desk/tickets/{id}",
            get(ticket).patch(edit_ticket).delete(delete_ticket),
        )
        .route("/api/desk/tickets/{id}/move", post(move_ticket))
        .route(
            "/api/desk/tickets/{id}/needs/{other}",
            put(add_need).delete(remove_need),
        )
        .route("/api/desk/tickets/{id}/assign", post(assign))
        .route("/api/desk/tickets/{id}/history", get(ticket_history))
        .route("/api/desk/history", get(history))
        .with_state(Arc::new(Api { desk, auth }))
}

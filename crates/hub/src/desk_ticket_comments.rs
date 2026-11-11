//! A ticket's comments over HTTP, for the person whose Desk this is
//! (`workforce_desk::ticket_comments`).
//!
//! - `GET /api/desk/tickets/{id}/comments`: its comments, oldest first. A
//!   synced ticket's are read from its issue first when they are stale,
//!   through the route its board syncs by, as the person who linked it;
//!   when that read fails, what was kept is answered with the `problem`.
//! - `POST /api/desk/tickets/{id}/comments` `{body}`: a comment, as the
//!   person. On a synced ticket it goes to its issue with the sync's own
//!   comment tool (`jira.add_comment`, `linear.comment_add`), through the
//!   same route, as them, with no prefix: it is their words. A call the hub
//!   holds for approval answers 202. A ticket of their own keeps it here.
//!
//! Only the person who linked a board reads or writes its issues' comments;
//! a device's token, or the hub's own, is no person.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use workforce_desk::board_sync::RemoteError;
use workforce_desk::crew::By;
use workforce_desk::{CommentPost, PostVia};

use crate::desk_board_sync::{paused_words, person_of, refusal, BoardSync};
use crate::desk_ticket_files::sync_of;

struct Routes {
    sync: Arc<BoardSync>,
}

type S = State<Arc<Routes>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

macro_rules! person {
    ($r:expr, $h:expr, $q:expr) => {
        match person_of(&$r.sync, &$h, $q.token.as_deref()).await {
            Ok(person) => person,
            Err(refused) => return refused,
        }
    };
}

/// The ticket comments' routes, hardened like the board sync's.
pub fn install(sync: Arc<BoardSync>) -> Router {
    crate::gateway::hardened(
        Router::new()
            .route("/api/desk/tickets/{id}/comments", get(list).post(post))
            .with_state(Arc::new(Routes { sync })),
    )
}

async fn list(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let stale = match desk.comments_stale(id).await {
        Ok(stale) => stale,
        Err(e) => return e.into_response(),
    };
    let mut problem = None;
    if stale {
        let t = match desk.ticket(id).await {
            Ok(v) => v.ticket,
            Err(e) => return e.into_response(),
        };
        let s = match sync_of(&r.sync, &t, &person).await {
            Ok(s) => s,
            Err(refused) => return refused,
        };
        let hub = r.sync.hub_name(&s.via).await;
        match r
            .sync
            .remote(s.system, &s.via, &person, s.agent.as_deref())
            .await
        {
            Ok(remote) => {
                if let Err(e) = desk.read_ticket_comments(id, &*remote).await {
                    tracing::info!(ticket = %id, error = %e, "a ticket's comments weren't read");
                    problem = Some(match e {
                        RemoteError::Waiting { .. } => {
                            format!("Waiting for your approval on {hub} to read its comments.")
                        }
                        RemoteError::Missing(_) | RemoteError::NotAllowed(_) => {
                            format!("{hub} can't send {}'s comments yet.", s.system.title())
                        }
                        _ => format!("{}'s comments couldn't be read just now.", s.system.title()),
                    });
                }
            }
            Err(p) => problem = Some(paused_words(&hub, s.system, &p)),
        }
    }
    match desk.ticket_comments(id).await {
        Ok(comments) => Json(json!({ "comments": comments, "problem": problem })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn post(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(new): Json<NewComment>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let t = match desk.ticket(id).await {
        Ok(v) => v.ticket,
        Err(e) => return e.into_response(),
    };
    let by = By::you();
    let posted = match t.external.as_ref().filter(|e| e.synced) {
        None => desk.post_ticket_comment(id, &new.body, &by, None).await,
        Some(_) => {
            let s = match sync_of(&r.sync, &t, &person).await {
                Ok(s) => s,
                Err(refused) => return refused,
            };
            let hub = r.sync.hub_name(&s.via).await;
            let remote = match r
                .sync
                .remote(s.system, &s.via, &person, s.agent.as_deref())
                .await
            {
                Ok(remote) => remote,
                Err(p) => return refusal(StatusCode::CONFLICT, &paused_words(&hub, s.system, &p)),
            };
            match desk
                .post_ticket_comment(
                    id,
                    &new.body,
                    &by,
                    Some(PostVia {
                        remote: &*remote,
                        link: s.link,
                    }),
                )
                .await
            {
                Err(CommentPost::Waiting) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "pending": true,
                            "message": format!("Waiting for your approval on {hub} to post it."),
                        })),
                    )
                        .into_response()
                }
                Err(CommentPost::Remote(e)) => {
                    tracing::info!(ticket = %id, error = %e, "a comment wasn't posted");
                    return refusal(
                        StatusCode::BAD_GATEWAY,
                        &format!("{} didn't take the comment: {e}", s.system.title()),
                    );
                }
                other => other,
            }
        }
    };
    match posted {
        Ok(c) => (StatusCode::CREATED, Json(c)).into_response(),
        Err(CommentPost::Desk(e)) => e.into_response(),
        Err(e) => refusal(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

#[cfg(test)]
#[path = "desk_ticket_comments_tests.rs"]
mod tests;

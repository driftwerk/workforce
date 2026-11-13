//! A ticket's files over HTTP, for the person whose Desk this is
//! (`workforce_desk::ticket_files`).
//!
//! - `GET /api/desk/tickets/{id}/files/{file}[?thumb=1]`: the file's bytes.
//!   An upload's are here. A synced ticket's are read through the route its
//!   board syncs by (`BoardSync::remote`), as the person who linked it, and
//!   kept a while; only that person reads them. The file is found by its id
//!   on that ticket: no address, tool or id the issue calls it by is ever
//!   taken from the request, and none goes back in an answer.
//! - `POST /api/desk/tickets/{id}/files?name=…`: drops a file (the body's
//!   bytes) on a ticket of the person's own; at most 20 MB.
//! - `DELETE /api/desk/tickets/{id}/files/{file}`: takes one off again.
//! - `POST /api/desk/tickets/{id}/read[?force=1]`: reads a synced ticket's
//!   issue whole for its files, unless it was read in the last few minutes.
//!
//! Bytes are served as what they are by their first bytes, never by what
//! anyone said they are: a PNG, JPEG, GIF, WebP or AVIF picture, or a PDF,
//! may be shown in place; anything else (SVG and HTML among it) is
//! `application/octet-stream`, a download. Every answer is `nosniff` and
//! sandboxed (`Content-Security-Policy: sandbox; default-src 'none'`).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;
use workforce_desk::board::Ticket;
use workforce_desk::board_sync::RemoteError;
use workforce_desk::crew::By;
use workforce_desk::ticket_files::{self as files, FileRead, FileSource, TicketFile, FILE_MAX};
use workforce_desk::DeskError;

use crate::desk_board_sync::{paused_words, person_of, refusal, BoardSync};

/// What a file answer's `Content-Security-Policy` says: nothing in it runs.
const SANDBOX: &str = "sandbox; default-src 'none'";

struct Routes {
    sync: Arc<BoardSync>,
}

type S = State<Arc<Routes>>;

#[derive(Deserialize)]
struct FileQuery {
    token: Option<String>,
    #[serde(default)]
    thumb: Option<String>,
}

#[derive(Deserialize)]
struct UploadQuery {
    token: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ReadQuery {
    token: Option<String>,
    #[serde(default)]
    force: Option<String>,
}

fn yes(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes"))
}

macro_rules! person {
    ($r:expr, $h:expr, $q:expr) => {
        match person_of(&$r.sync, &$h, $q.token.as_deref()).await {
            Ok(person) => person,
            Err(refused) => return refused,
        }
    };
}

/// The ticket files' routes.
pub fn router(sync: Arc<BoardSync>) -> Router {
    let state = Arc::new(Routes { sync });
    let uploads = Router::new()
        .route("/api/desk/tickets/{id}/files", post(upload))
        // A file and a little more: the 1 MB every other route keeps to is
        // too little for a screenshot.
        .layer(DefaultBodyLimit::max(FILE_MAX as usize + 64 * 1024))
        .with_state(Arc::clone(&state));
    Router::new()
        .route(
            "/api/desk/tickets/{id}/files/{file}",
            get(file_bytes).delete(remove),
        )
        .route("/api/desk/tickets/{id}/read", post(read_issue))
        .with_state(state)
        .merge(uploads)
}

/// The routes, hardened like the board sync's, then sandboxed: the
/// sandbox goes on last, so the gateway's own policy doesn't replace it.
pub fn install(sync: Arc<BoardSync>) -> Router {
    crate::gateway::hardened(router(sync)).layer(axum::middleware::from_fn(sandboxed))
}

async fn sandboxed(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX),
    );
    response
}

/// `bytes` served as what they are.
fn served(f: &TicketFile, bytes: Vec<u8>) -> Response {
    let (media_type, inline) = files::served_as(&bytes);
    let name = files::safe_filename(&f.name);
    let disposition = format!(
        "{}; filename=\"{name}\"",
        if inline { "inline" } else { "attachment" }
    );
    let mut r = Response::new(Body::from(bytes));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    r
}

/// Why a remote file wasn't read, as the page shows it: the hub's words,
/// never a credential, an address or a tool's raw answer.
fn unread(hub: &str, system: workforce_desk::board::System, e: FileRead) -> Response {
    let title = system.title();
    match e {
        FileRead::Remote(RemoteError::Waiting { .. }) => refusal(
            StatusCode::CONFLICT,
            &format!("Waiting for your approval on {hub} to read it."),
        ),
        FileRead::Remote(RemoteError::Missing(_) | RemoteError::NotAllowed(_)) => refusal(
            StatusCode::CONFLICT,
            &format!("{hub} can't send {title}'s files yet: open it in {title}."),
        ),
        FileRead::Remote(RemoteError::RateLimited { .. }) => refusal(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("{title} asked to slow down: try again in a minute."),
        ),
        FileRead::Remote(
            e @ (RemoteError::Offline(_) | RemoteError::NoToken | RemoteError::Refused(_)),
        ) => refusal(
            StatusCode::BAD_GATEWAY,
            &paused_words(hub, system, &crate::desk_board_sync::paused_by(&e)),
        ),
        FileRead::Remote(_) => {
            refusal(StatusCode::BAD_GATEWAY, &format!("{title} didn't send it."))
        }
        FileRead::TooBig => refusal(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
        FileRead::Link => refusal(StatusCode::BAD_REQUEST, &e.to_string()),
        FileRead::Garbled(_) => refusal(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// The board sync of `t`, when the person asking is the one who linked
/// it; why not, as an answer.
pub(crate) async fn sync_of(
    sync: &BoardSync,
    t: &Ticket,
    person: &str,
) -> Result<workforce_desk::board::Sync, Response> {
    let s = match sync.desk.board_sync(t.board).await {
        Ok(Some((_, s))) => s,
        Ok(None) => {
            return Err(refusal(
                StatusCode::CONFLICT,
                "That ticket's board isn't synced any more.",
            ))
        }
        Err(e) => return Err(e.into_response()),
    };
    if s.person != person {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the person who linked this board reads and writes its issues.",
        ));
    }
    Ok(s)
}

async fn file_bytes(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<FileQuery>,
    Path((id, file)): Path<(Uuid, Uuid)>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let (t, f) = match desk.ticket_file(id, file).await {
        Ok(found) => found,
        Err(e) => return e.into_response(),
    };
    let thumb = yes(&q.thumb);
    if f.source == FileSource::Upload {
        return match desk.kept_file_bytes(&f, false).await {
            Ok(Some(bytes)) => served(&f, bytes),
            Ok(None) => DeskError::NotFound.into_response(),
            Err(e) => e.into_response(),
        };
    }
    if !f.source.has_bytes() {
        return refusal(
            StatusCode::BAD_REQUEST,
            "That's a link: open it where it is.",
        );
    }
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
    match desk.remote_file_bytes(&t, f, thumb, &*remote).await {
        Ok(got) => served(&got.file, got.bytes),
        Err(e) => {
            tracing::info!(ticket = %id, file = %file, error = %e, "a ticket's file wasn't read");
            unread(&hub, s.system, e)
        }
    }
}

async fn upload(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<UploadQuery>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Response {
    let _person = person!(r, h, q);
    let name = q.name.as_deref().unwrap_or("file");
    match r
        .sync
        .desk
        .add_ticket_upload(id, name, &body, &By::you())
        .await
    {
        Ok(f) => (StatusCode::CREATED, Json(f)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn remove(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<FileQuery>,
    Path((id, file)): Path<(Uuid, Uuid)>,
) -> Response {
    let _person = person!(r, h, q);
    match r.sync.desk.remove_ticket_upload(id, file, &By::you()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn read_issue(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<ReadQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let t = match desk.ticket(id).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    if t.ticket.external.as_ref().is_none_or(|e| !e.synced) {
        return Json(t).into_response();
    }
    let s = match sync_of(&r.sync, &t.ticket, &person).await {
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
    match desk.read_ticket_issue(id, &*remote, yes(&q.force)).await {
        Ok(v) => Json(v).into_response(),
        // The detail is logged; the page gets plain words, never the
        // remote's own text.
        Err(e) => {
            tracing::warn!(ticket = %id, error = %e, "a ticket's issue wasn't read");
            unread(&hub, s.system, FileRead::Remote(e))
        }
    }
}

#[cfg(test)]
#[path = "desk_ticket_files_tests.rs"]
mod tests;

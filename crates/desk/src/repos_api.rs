//! `/api/desk/repos` and `/api/desk/repos/checkout`: a repository's
//! checkouts on this computer, and one found or cloned for a plan's
//! ticket ([`crate::repos`]), for the Desk that drives it over SSH.
//!
//! Native-only, as grants are: the hub's own token alone, from the
//! `Authorization` header alone. Desk's page can't ask them (they are on
//! none of its broker's lists); Desk's native side carries a boss's
//! `crew.machine_folders` and its plan runner's `desk.checkout` here, after
//! the Mac decided them.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use workforce_surface::routing::{get, post};
use workforce_surface::Routes;

use crate::api::Authorize;
use crate::repos::{repo_of, Repo};
use crate::service::{Desk, DeskError};

struct ReposApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<ReposApi>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Find {
    remote: String,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Want {
    remote: String,
    #[serde(default)]
    folder: Option<String>,
}

/// Why a request isn't the native side's, when it isn't.
fn not_native(s: &ReposApi, h: &HeaderMap, token: Option<&String>) -> Option<Response> {
    if token.is_some() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "repositories take the hub's token in the Authorization header only"})),
            )
                .into_response(),
        );
    }
    (!s.auth.is_hub_token(h)).then(|| {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "only Desk itself asks for repositories, with the hub's own token"})),
        )
            .into_response()
    })
}

fn repo_said(said: &str) -> Result<Repo, DeskError> {
    repo_of(said).ok_or_else(|| {
        DeskError::Invalid(
            "a repository is host/owner/name, like github.com/driftwerk/vellev, or owner/name for GitHub"
                .into(),
        )
    })
}

/// Every checkout of `remote` under the home: main ones first.
async fn find(State(s): S, h: HeaderMap, Query(q): Query<Find>) -> Response {
    if let Some(r) = not_native(&s, &h, q.token.as_ref()) {
        return r;
    }
    let repo = match repo_said(&q.remote) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let d = s.desk.clone();
    let key = repo.key();
    match tokio::task::spawn_blocking(move || d.repo_checkouts(&repo)).await {
        Ok(found) => Json(json!({ "repo": key, "checkouts": found })).into_response(),
        Err(_) => DeskError::Invalid("the home folder couldn't be read".into()).into_response(),
    }
}

/// The checkout of `remote` a plan's ticket works in: its `folder` when it
/// is one, one found, or a clone begun ([`crate::repos::Got`]).
async fn checkout(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Result<Json<Want>, JsonRejection>,
) -> Response {
    if let Some(r) = not_native(&s, &h, q.token.as_ref()) {
        return r;
    }
    let Want { remote, folder } = match body {
        Ok(Json(b)) => b,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    let repo = match repo_said(&remote) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let d = s.desk.clone();
    match tokio::task::spawn_blocking(move || d.repo_checkout(&repo, folder.as_deref())).await {
        Ok(got) => Json(got).into_response(),
        Err(_) => DeskError::Invalid("the home folder couldn't be read".into()).into_response(),
    }
}

pub(crate) fn api_routes(ground: crate::api::Ground) -> Routes {
    Routes::new()
        .admin("/api/desk/repos", get(find))
        .admin("/api/desk/repos/checkout", post(checkout))
        .bind(ground.map(|(desk, auth)| Arc::new(ReposApi { desk, auth })))
}

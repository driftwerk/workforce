//! `/api/desk/grants/*` and `/api/desk/bosses/*` (remote bosses, RB1): the
//! home's grants for this engine, and the engine's stop and pause.
//!
//! Native-only, as an account's hand-over is: the hub's own token alone,
//! from the `Authorization` header alone (never a token in the address,
//! which logs keep), never a signed-in person's or a paired device's. Desk's
//! page can't ask them: they are on none of its broker's lists. So only
//! Desk's native side writes a grant (to a machine, over its SSH forward),
//! and `desk-helper bosses stop|status` on the box itself; no agent, page
//! or session does.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::Authorize;
use crate::grant::StoppedFrom;
use crate::service::{Desk, DeskError, Native};

struct GrantApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<GrantApi>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

type Q = Query<TokenQuery>;

/// Why a request isn't the native side's, when it isn't.
fn not_native(s: &GrantApi, h: &HeaderMap, q: &TokenQuery) -> Option<Response> {
    if q.token.is_some() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "grants take the hub's token in the Authorization header only"})),
            )
                .into_response(),
        );
    }
    if !s.auth.is_hub_token(h) {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "only Desk itself writes grants, with the hub's own token"})),
            )
                .into_response(),
        );
    }
    None
}

/// The proof a request is the native side's, or its refusal answered.
macro_rules! native {
    ($s:expr, $h:expr, $q:expr) => {
        match not_native(&$s, &$h, &$q) {
            None => Native::routes(),
            Some(r) => return r,
        }
    };
}

async fn list(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    let n = native!(s, h, q);
    Json(s.desk.grants_view_spent(&n).await).into_response()
}

async fn one(State(s): S, h: HeaderMap, Query(q): Q, Path(group): Path<String>) -> Response {
    let n = native!(s, h, q);
    match s.desk.grant_view_spent(&n, &group).await {
        Some(g) => Json(g).into_response(),
        None => DeskError::NotFound.into_response(),
    }
}

async fn put(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(group): Path<String>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let n = native!(s, h, q);
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    s.desk
        .put_grant(n, &group, body)
        .await
        .map(Json)
        .into_response()
}

async fn remove(State(s): S, h: HeaderMap, Query(q): Q, Path(group): Path<String>) -> Response {
    let n = native!(s, h, q);
    s.desk
        .delete_grant(n, &group)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Stop {
    /// One group's boss; none, every boss here, and the engine paused.
    #[serde(default)]
    group: Option<String>,
    /// `desk` (Desk on the Mac) or `machine` (`desk-helper` on the box,
    /// the default).
    #[serde(default)]
    from: Option<String>,
}

/// `desk-helper bosses stop [--group <id>]`, and Desk's Stop: in force at
/// once, whatever the home sends after.
async fn stop(State(s): S, h: HeaderMap, Query(q): Q, body: Bytes) -> Response {
    let n = native!(s, h, q);
    let Stop { group, from } = match body.is_empty() {
        true => Stop::default(),
        false => match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return DeskError::Invalid(format!("not a stop: {e}")).into_response(),
        },
    };
    let from = match from.as_deref() {
        None => StoppedFrom::Machine,
        Some(f) => match StoppedFrom::parse(f) {
            Some(f) => f,
            None => {
                return DeskError::Invalid("from is desk or machine".into()).into_response();
            }
        },
    };
    s.desk
        .stop_bosses(n, group.as_deref(), from)
        .await
        .map(Json)
        .into_response()
}

/// `desk-helper bosses status`: the grants here, and the pause, in words.
async fn status(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    let n = native!(s, h, q);
    let view = s.desk.grants_view(&n);
    let lines: Vec<String> = view
        .grants
        .iter()
        .map(|g| match &g.why {
            None => format!(
                "{}: {} bosses it ({}), until {}.",
                g.grant.name,
                g.grant.boss.as_ref().map_or_else(String::new, |b| b.key()),
                g.grant.level.as_str(),
                g.grant.expires_at.format("%Y-%m-%d %H:%M UTC")
            ),
            Some(why) => format!("{}: {why}", g.grant.name),
        })
        .collect();
    Json(json!({
        "paused": view.paused,
        "stampsFromGrants": view.stamps_from_grants,
        "said": lines,
        "grants": view.grants,
    }))
    .into_response()
}

/// Lets the bosses here go on again: Desk on the Mac alone asks it.
async fn clear_pause(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    let n = native!(s, h, q);
    s.desk.clear_bosses_pause(n).await.map(Json).into_response()
}

pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/grants", get(list))
        .route("/api/desk/grants/{group}", get(one).put(put).delete(remove))
        .route("/api/desk/bosses", get(status))
        .route("/api/desk/bosses/stop", post(stop))
        .route("/api/desk/bosses/paused", delete(clear_pause))
        .with_state(Arc::new(GrantApi { desk, auth }))
}

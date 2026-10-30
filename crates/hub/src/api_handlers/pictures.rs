//! Agents' and people's pictures:
//!
//! - `GET /api/agents/{id}/picture`: the picture an agent is shown with, to
//!   anyone the roster shows the agent to.
//! - `PUT` / `DELETE /api/agents/{id}/picture`: choose a picture for it (the
//!   image as the body), or stop choosing one (its Slack picture, if any,
//!   shows again); for whoever may change the agent.
//! - `GET` / `PUT` / `DELETE /api/me/picture`: the same for the caller's own
//!   account.
//!
//! An image is answered with a strong `ETag` (its version, as lists give
//! it) and `Cache-Control: private, no-cache`, and `If-None-Match` with 304.
//! Tokens are only taken from the `Authorization` header, never the query.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tracing::warn;

use crate::gateway::{
    check_action_rate_limit, require_admin_action, resolve_agent, resolve_caller_context,
    too_many_requests_err, AgentViewer, AuthFailure, CallerContext, GatewayState,
};
use crate::pictures::{self, Image, Owner, PictureStore, Refusal, Source};

type Refused = (StatusCode, Json<Value>);

fn refused(status: StatusCode, error: &str) -> Refused {
    (status, Json(json!({ "error": error })))
}

fn store(state: &GatewayState) -> Result<&Arc<dyn PictureStore>, Refused> {
    state.pictures.as_ref().ok_or_else(|| {
        refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "Pictures need the hub's database, which isn't connected.",
        )
    })
}

fn failed(owner: &Owner, e: eyre::Report) -> Refused {
    warn!(target: "pictures", %owner, error = %format!("{e:#}"), "picture store failed");
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The hub couldn't reach its pictures.",
    )
}

/// The caller, refused when the credential it presented doesn't hold.
async fn caller(state: &GatewayState, headers: &HeaderMap) -> Result<CallerContext, Refused> {
    let caller = resolve_caller_context(state, headers, None).await;
    match caller.auth_failure {
        Some(AuthFailure::StoreError) => Err(refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "The hub couldn't check who you are.",
        )),
        Some(AuthFailure::InvalidCredentials) => {
            Err(refused(StatusCode::UNAUTHORIZED, "unauthorized"))
        }
        None if !caller.is_authenticated() => {
            Err(refused(StatusCode::UNAUTHORIZED, "unauthorized"))
        }
        None => Ok(caller),
    }
}

/// The caller's own account, for `/api/me/picture`.
async fn me(state: &GatewayState, headers: &HeaderMap) -> Result<(Owner, CallerContext), Refused> {
    let caller = caller(state, headers).await?;
    let id = caller.identity_id.clone().ok_or_else(|| {
        refused(
            StatusCode::UNAUTHORIZED,
            "Only someone signed in as themselves has a picture.",
        )
    })?;
    Ok((Owner::identity(id), caller))
}

fn quoted(etag: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{etag}\"")).expect("an etag is hex")
}

/// Whether `If-None-Match` names `etag` (or is `*`).
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == "*" || tag.trim_matches('"') == etag)
}

async fn serve(
    store: &dyn PictureStore,
    owner: &Owner,
    headers: &HeaderMap,
) -> Result<Response, Refused> {
    let picture = store
        .shown(owner)
        .await
        .map_err(|e| failed(owner, e))?
        .ok_or_else(|| refused(StatusCode::NOT_FOUND, "No picture."))?;
    let etag = quoted(&picture.image.etag);
    let cache = [
        (header::ETAG, etag),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
    ];
    if not_modified(headers, &picture.image.etag) {
        return Ok((StatusCode::NOT_MODIFIED, cache).into_response());
    }
    let media_type = HeaderValue::from_str(&picture.image.media_type)
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    Ok((
        cache,
        [(header::CONTENT_TYPE, media_type)],
        picture.image.bytes,
    )
        .into_response())
}

/// The picture `owner` is shown with now, as the API answers it.
async fn shown(store: &dyn PictureStore, owner: &Owner) -> Result<Json<Value>, Refused> {
    let face = pictures::face(store, owner)
        .await
        .map_err(|e| failed(owner, e))?;
    Ok(Json(json!({ "picture": face.map(|f| f.info()) })))
}

async fn choose(
    store: &dyn PictureStore,
    owner: &Owner,
    body: Bytes,
) -> Result<Json<Value>, Refused> {
    let image = Image::accept(body.to_vec()).map_err(|refusal| {
        let status = match refusal {
            Refusal::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Refusal::NotAnImage => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        };
        refused(status, &refusal.to_string())
    })?;
    store
        .choose(owner, &image)
        .await
        .map_err(|e| failed(owner, e))?;
    shown(store, owner).await
}

async fn unchoose(store: &dyn PictureStore, owner: &Owner) -> Result<Json<Value>, Refused> {
    store
        .remove(owner, Source::Chosen)
        .await
        .map_err(|e| failed(owner, e))?;
    shown(store, owner).await
}

/// An agent for whoever may change it.
async fn managed(state: &GatewayState, headers: &HeaderMap, id: &str) -> Result<Owner, Refused> {
    require_admin_action(state, headers, None).await?;
    resolve_agent(state, id)?;
    Ok(Owner::agent(id))
}

pub async fn agent_picture_get(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Response, Refused> {
    let caller = caller(&state, &headers).await?;
    if !AgentViewer::new(&state, caller).can_see(&id) {
        return Err(crate::gateway::not_found_agent(&id));
    }
    serve(store(&state)?.as_ref(), &Owner::agent(id), &headers).await
}

pub async fn agent_picture_put(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> Result<Json<Value>, Refused> {
    let owner = managed(&state, &headers, &id).await?;
    choose(store(&state)?.as_ref(), &owner, body).await
}

pub async fn agent_picture_delete(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, Refused> {
    let owner = managed(&state, &headers, &id).await?;
    unchoose(store(&state)?.as_ref(), &owner).await
}

pub async fn my_picture_get(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Response, Refused> {
    let (owner, _) = me(&state, &headers).await?;
    serve(store(&state)?.as_ref(), &owner, &headers).await
}

pub async fn my_picture_put(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> Result<Json<Value>, Refused> {
    let (owner, caller) = me(&state, &headers).await?;
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(too_many_requests_err)?;
    choose(store(&state)?.as_ref(), &owner, body).await
}

pub async fn my_picture_delete(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, Refused> {
    let (owner, caller) = me(&state, &headers).await?;
    check_action_rate_limit(state.action_limiter.as_ref(), &caller)
        .map_err(too_many_requests_err)?;
    unchoose(store(&state)?.as_ref(), &owner).await
}

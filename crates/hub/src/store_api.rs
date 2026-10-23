//! `/api/store/{scope}/{collection}[/{doc_id}]` — the document store over
//! the gateway, for the UI, scripts and anything outside an agent turn.
//!
//! `scope` is spelled as the tools spell it: `user` (the caller's own),
//! `user:<id>`, `agent:<id>`, `global`. A bare `agent` has no meaning here
//! — a request carries no calling agent — so it is refused. Policy is
//! [`crate::store::Caller::authorise`]: a member reaches their own user
//! scope and reads global; an admin reaches every agent scope and writes
//! global; nobody reads another user's scope.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::authz::Capability;
use crate::gateway::{
    require_capability_action, require_capability_read, CallerContext, GatewayState,
};
use crate::hub_core::pipelines::{Access, Caller, Scope, StoreError};
use crate::store::{content, Descriptor, DocumentStore, Query as StoreQuery, WriteOp, MAX_BATCH};

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Debug, Deserialize)]
pub(crate) struct StoreParams {
    token: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    direction: Option<String>,
    order_by: Option<String>,
    /// `set` / `update` / `delete` / `claim` — write only if the document is
    /// at this version (`0` = only if absent).
    if_version: Option<i64>,
    /// `set` — expire the document after this many seconds.
    ttl_secs: Option<i64>,
    /// `claim` / `release` — who holds the claim. Defaults to the caller's
    /// identity.
    owner: Option<String>,
    /// `PUT …/content/{name}` — where this write starts; must be where the
    /// last one ended.
    offset: Option<i64>,
    /// `PUT …/content/{name}` — commit the stream after this write. Default
    /// true, so a single-request upload needs nothing extra; pass false on
    /// every piece but the last when streaming one in.
    commit: Option<bool>,
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    (status, Json(json!({"error": message.into(), "code": code})))
}

/// Map a store error onto the status a client would expect.
pub(crate) fn store_error(e: StoreError) -> ApiError {
    let status = match &e {
        StoreError::NotFound => StatusCode::NOT_FOUND,
        StoreError::VersionConflict { .. } | StoreError::Claimed { .. } => StatusCode::CONFLICT,
        StoreError::Denied(_) => StatusCode::FORBIDDEN,
        StoreError::NoIdentity => StatusCode::UNAUTHORIZED,
        StoreError::Invalid(_) => StatusCode::BAD_REQUEST,
        StoreError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        StoreError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut body = json!({"error": e.to_string(), "code": e.code()});
    match &e {
        StoreError::VersionConflict { expected, actual } => {
            body["expected"] = json!(expected);
            body["actual"] = json!(actual);
        }
        StoreError::Claimed { owner, until } => {
            body["owner"] = json!(owner);
            body["until"] = json!(until);
        }
        _ => {}
    }
    (status, Json(body))
}

fn backend(e: eyre::Report) -> ApiError {
    tracing::error!(error = %e, "document store request failed");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "backend",
        "document store request failed",
    )
}

/// The gateway caller as the store sees it. A request has no calling agent,
/// so `agent:<id>` scopes are an admin's to reach.
pub(crate) fn caller_from_request(caller: &CallerContext) -> Caller {
    Caller {
        identity: caller.identity_id.clone(),
        agent_id: None,
        admin: caller.is_admin,
        system: false,
    }
}

fn store() -> Result<Arc<dyn DocumentStore>, ApiError> {
    crate::store::global_document_store().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "document store is not configured on this hub",
        )
    })
}

/// Authenticate, rate-limit writes, resolve the scope and check policy.
async fn admit(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    token: Option<&str>,
    scope_spec: &str,
    access: Access,
) -> Result<(Caller, Scope, Arc<dyn DocumentStore>), ApiError> {
    let ctx = match access {
        Access::Read => {
            require_capability_read(state, headers, token, Capability::OwnDataRw).await?
        }
        Access::Write => {
            require_capability_action(state, headers, token, Capability::OwnDataRw).await?
        }
    };
    let caller = caller_from_request(&ctx);
    if scope_spec == "agent" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "a request has no calling agent — name one: agent:<id>",
        ));
    }
    let scope = Scope::parse(scope_spec, &caller).map_err(store_error)?;
    caller.authorise(&scope, access).map_err(store_error)?;
    Ok((caller, scope, store()?))
}

fn query_from_params(p: &StoreParams) -> Result<StoreQuery, ApiError> {
    let mut v = json!({});
    if let Some(l) = p.limit {
        v["limit"] = json!(l);
    }
    if let Some(c) = &p.cursor {
        v["cursor"] = json!(c);
    }
    if let Some(d) = &p.direction {
        v["direction"] = json!(d);
    }
    if let Some(o) = &p.order_by {
        v["order_by"] = json!(o);
    }
    StoreQuery::from_json(&v).map_err(store_error)
}

fn actor(caller: &Caller) -> Option<String> {
    caller.identity.clone()
}

fn owner_of(p: &StoreParams, caller: &Caller) -> Result<String, ApiError> {
    p.owner
        .clone()
        .or_else(|| caller.identity.clone())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "invalid", "`owner` is required"))
}

pub(crate) async fn list(
    Path((scope, collection)): Path<(String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    let query = query_from_params(&p)?;
    let page = store
        .list(&scope, &collection, &query)
        .await
        .map_err(backend)?;
    Ok(Json(json!({
        "scope": scope.to_string(),
        "collection": collection,
        "count": page.documents.len(),
        "documents": page.documents,
        "next_cursor": page.next_cursor,
    })))
}

pub(crate) async fn query(
    Path((scope, collection)): Path<(String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    let query = StoreQuery::from_json(&body).map_err(store_error)?;
    let page = store
        .query(&scope, &collection, &query)
        .await
        .map_err(backend)?;
    Ok(Json(json!({
        "scope": scope.to_string(),
        "collection": collection,
        "count": page.documents.len(),
        "documents": page.documents,
        "next_cursor": page.next_cursor,
    })))
}

pub(crate) async fn collections(
    Path(scope): Path<String>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    let collections = store.collections(&scope).await.map_err(store_error)?;
    Ok(Json(
        json!({"scope": scope.to_string(), "collections": collections}),
    ))
}

pub(crate) async fn described(
    Path((scope, collection)): Path<(String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    match store
        .described(&scope, &collection)
        .await
        .map_err(store_error)?
    {
        Some(info) => Ok(Json(serde_json::to_value(info).unwrap_or(Value::Null))),
        None => Err(store_error(StoreError::NotFound)),
    }
}

pub(crate) async fn describe(
    Path((scope, collection)): Path<(String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let descriptor = Descriptor::from_json(&body).map_err(store_error)?;
    let info = store
        .describe(&scope, &collection, &descriptor)
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::to_value(info).unwrap_or(Value::Null)))
}

/// `GET …/content/{name}` — the stream's bytes, honouring `Range` so a client
/// can pull a large one in pieces, or a browser can seek in it, without the
/// hub ever holding more than one range in memory.
pub(crate) async fn content_get(
    Path((scope, collection, doc_id, name)): Path<(String, String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<axum::response::Response, ApiError> {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    let info = store
        .content_stat(&scope, &collection, &doc_id, &name)
        .await
        .map_err(store_error)?
        .ok_or_else(|| store_error(StoreError::NotFound))?;

    let (start, end) = requested_range(&headers, info.size_bytes);
    let slice = store
        .content_read(&scope, &collection, &doc_id, &name, start, end - start)
        .await
        .map_err(store_error)?
        .ok_or_else(|| store_error(StoreError::NotFound))?;

    let partial = headers.contains_key(axum::http::header::RANGE);
    let last = start + slice.bytes.len() as i64 - 1;
    let mut response = axum::response::Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(
            axum::http::header::CONTENT_TYPE,
            info.content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_LENGTH, slice.bytes.len());
    if partial {
        response = response.header(
            axum::http::header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", last.max(start), info.size_bytes),
        );
    }
    if let Some(digest) = &info.digest {
        response = response.header(axum::http::header::ETAG, format!("\"{digest}\""));
    }
    response
        .body(axum::body::Body::from(slice.bytes))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "backend", e.to_string()))
}

/// The byte range a request asks for, clamped to what exists. Only the single
/// `bytes=start-end` form is honoured; anything else reads as the whole
/// stream, which is a legal answer to a Range a server chooses not to satisfy.
fn requested_range(headers: &axum::http::HeaderMap, size: i64) -> (i64, i64) {
    let whole = (0, size.min(content::MAX_READ_BYTES));
    let Some(spec) = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
    else {
        return whole;
    };
    let Some((from, to)) = spec.split_once('-') else {
        return whole;
    };
    let start = from.trim().parse::<i64>().unwrap_or(0).clamp(0, size);
    let end = to
        .trim()
        .parse::<i64>()
        .map(|last| (last + 1).min(size))
        .unwrap_or(size);
    if end <= start {
        return whole;
    }
    (start, end.min(start + content::MAX_READ_BYTES))
}

/// `PUT …/content/{name}` — append the body at `offset`, then commit when the
/// request says this is the last piece.
pub(crate) async fn content_put(
    Path((scope, collection, doc_id, name)): Path<(String, String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    bytes: axum::body::Bytes,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let write = store
        .content_write(
            &scope,
            &collection,
            &doc_id,
            &name,
            p.offset.unwrap_or(0),
            &bytes,
            content_type,
        )
        .await
        .map_err(store_error)?;
    if p.commit.unwrap_or(true) {
        let info = store
            .content_commit(&scope, &collection, &doc_id, &name)
            .await
            .map_err(store_error)?;
        return Ok(Json(json!({"committed": true, "content": info})));
    }
    Ok(Json(json!({
        "committed": false,
        "version": write.version,
        "next_offset": write.next_offset,
    })))
}

pub(crate) async fn content_delete(
    Path((scope, collection, doc_id, name)): Path<(String, String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let deleted = store
        .content_delete(&scope, &collection, &doc_id, &name)
        .await
        .map_err(store_error)?;
    Ok(Json(json!({"deleted": deleted})))
}

pub(crate) async fn content_list(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    let content = store
        .content_list(&scope, &collection, &doc_id)
        .await
        .map_err(store_error)?;
    Ok(Json(json!({"content": content})))
}

pub(crate) async fn get_doc(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Read).await?;
    match store
        .get(&scope, &collection, &doc_id)
        .await
        .map_err(backend)?
    {
        Some(doc) => Ok(Json(serde_json::to_value(doc).unwrap_or(Value::Null))),
        None => Err(store_error(StoreError::NotFound)),
    }
}

pub(crate) async fn set_doc(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let doc = store
        .set(
            &scope,
            &collection,
            &doc_id,
            body,
            p.if_version,
            p.ttl_secs,
            actor(&caller).as_deref(),
        )
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::to_value(doc).unwrap_or(Value::Null)))
}

pub(crate) async fn update_doc(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let doc = store
        .update(
            &scope,
            &collection,
            &doc_id,
            body,
            p.if_version,
            actor(&caller).as_deref(),
        )
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::to_value(doc).unwrap_or(Value::Null)))
}

pub(crate) async fn delete_doc(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let existed = store
        .delete(
            &scope,
            &collection,
            &doc_id,
            p.if_version,
            actor(&caller).as_deref(),
        )
        .await
        .map_err(store_error)?;
    Ok(Json(json!({"deleted": true, "existed": existed})))
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchBody {
    writes: Vec<WriteOp>,
}

pub(crate) async fn batch(
    Path(scope): Path<String>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<BatchBody>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    if body.writes.len() > MAX_BATCH {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!(
                "batch has {} writes; the limit is {MAX_BATCH}",
                body.writes.len()
            ),
        ));
    }
    let outcomes = store
        .batch(&scope, &body.writes, actor(&caller).as_deref())
        .await
        .map_err(store_error)?;
    Ok(Json(
        json!({"applied": outcomes.len(), "outcomes": outcomes}),
    ))
}

pub(crate) async fn claim(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let owner = owner_of(&p, &caller)?;
    let doc = store
        .claim(
            &scope,
            &collection,
            &doc_id,
            &owner,
            p.ttl_secs.unwrap_or(300),
        )
        .await
        .map_err(store_error)?;
    Ok(Json(json!({
        "claimed": true,
        "owner": owner,
        "until": doc.lease_expires_at,
        "document": doc,
    })))
}

pub(crate) async fn release(
    Path((scope, collection, doc_id)): Path<(String, String, String)>,
    Query(p): Query<StoreParams>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (caller, scope, store) =
        admit(&state, &headers, p.token.as_deref(), &scope, Access::Write).await?;
    let owner = owner_of(&p, &caller)?;
    let released = store
        .release(&scope, &collection, &doc_id, &owner)
        .await
        .map_err(store_error)?;
    Ok(Json(json!({"released": released, "owner": owner})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_errors_map_to_the_status_a_client_expects() {
        let cases = [
            (StoreError::NotFound, StatusCode::NOT_FOUND, "not_found"),
            (
                StoreError::VersionConflict {
                    expected: 1,
                    actual: 2,
                },
                StatusCode::CONFLICT,
                "version_conflict",
            ),
            (
                StoreError::Claimed {
                    owner: "w1".into(),
                    until: chrono::Utc::now(),
                },
                StatusCode::CONFLICT,
                "claimed",
            ),
            (
                StoreError::Denied("no".into()),
                StatusCode::FORBIDDEN,
                "denied",
            ),
            (
                StoreError::NoIdentity,
                StatusCode::UNAUTHORIZED,
                "no_identity",
            ),
            (
                StoreError::Invalid("bad".into()),
                StatusCode::BAD_REQUEST,
                "invalid",
            ),
            (
                StoreError::Backend("db".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "backend",
            ),
        ];
        for (e, status, code) in cases {
            let (s, Json(body)) = store_error(e);
            assert_eq!(s, status);
            assert_eq!(body["code"], code);
        }
        let (_, Json(body)) = store_error(StoreError::VersionConflict {
            expected: 3,
            actual: 4,
        });
        assert_eq!(body["expected"], 3);
        assert_eq!(body["actual"], 4);
    }

    #[test]
    fn a_request_caller_has_no_agent_and_carries_admin() {
        let ctx = CallerContext {
            identity_id: Some("alice".into()),
            device_id: None,
            is_admin: true,
            scopes: vec![],
            auth_failure: None,
        };
        let c = caller_from_request(&ctx);
        assert_eq!(c.identity.as_deref(), Some("alice"));
        assert!(c.agent_id.is_none());
        assert!(c.admin);
        assert!(!c.system);
        assert!(c
            .authorise(&Scope::Agent("atlas".into()), Access::Write)
            .is_ok());
        assert!(c
            .authorise(&Scope::User("bob".into()), Access::Read)
            .is_err());
    }

    #[test]
    fn a_member_request_caller_reaches_no_agent_scope() {
        let ctx = CallerContext {
            identity_id: Some("alice".into()),
            ..Default::default()
        };
        let c = caller_from_request(&ctx);
        for access in [Access::Read, Access::Write] {
            assert!(c.authorise(&Scope::Agent("atlas".into()), access).is_err());
        }
    }
}

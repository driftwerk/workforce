//! Desk's own state kept by the hub: window layouts, grid positions,
//! selections, as JSON values by key. A write is committed before it is
//! answered, and every Desk window watching the events socket hears it.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::api::Authorize;
use crate::model::DeskEvent;
use crate::service::{Desk, DeskError};
use crate::store::DeskStore;

/// The largest value a key may hold, as its JSON text.
pub const MAX_VALUE_BYTES: usize = 256 * 1024;
/// The longest key.
pub const MAX_KEY_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub key: String,
    pub value: Value,
    pub updated_at: DateTime<Utc>,
}

/// A key in a listing; its value only when asked for.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub updated_at: DateTime<Utc>,
}

impl DeskStore {
    pub async fn state(&self, key: &str) -> Result<Option<Entry>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT key, value, updated_at FROM state WHERE key = $1",
                &[&key],
            )
            .await?;
        Ok(row.map(|r| Entry {
            key: r.get(0),
            value: r.get(1),
            updated_at: r.get(2),
        }))
    }

    pub async fn put_state(&self, key: &str, value: &Value) -> Result<Entry> {
        let client = self.pool().get().await?;
        let now = Utc::now();
        client
            .execute(
                "INSERT INTO state (key, value, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = $3",
                &[&key, value, &now],
            )
            .await
            .context("saving desk state")?;
        Ok(Entry {
            key: key.to_string(),
            value: value.clone(),
            updated_at: now,
        })
    }

    /// Whether the key was there.
    pub async fn delete_state(&self, key: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute("DELETE FROM state WHERE key = $1", &[&key])
            .await?
            > 0)
    }

    /// Keys starting with `prefix` (taken literally), in key order.
    pub async fn list_state(&self, prefix: &str, values: bool) -> Result<Vec<Listed>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT key, CASE WHEN $2 THEN value END, updated_at FROM state \
                 WHERE starts_with(key, $1) ORDER BY key",
                &[&prefix, &values],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| Listed {
                key: r.get(0),
                value: r.get(1),
                updated_at: r.get(2),
            })
            .collect())
    }
}

fn check_key(key: &str) -> Result<(), DeskError> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS || key.chars().any(char::is_control) {
        return Err(DeskError::Invalid(format!(
            "a key is 1 to {MAX_KEY_CHARS} characters, none of them control characters"
        )));
    }
    Ok(())
}

impl Desk {
    pub async fn state(&self, key: &str) -> Result<Entry, DeskError> {
        check_key(key)?;
        self.store().state(key).await?.ok_or(DeskError::NotFound)
    }

    /// Saves `value` under `key` and tells every watching Desk once it is
    /// committed.
    pub async fn put_state(&self, key: &str, value: Value) -> Result<Entry, DeskError> {
        check_key(key)?;
        let entry = self.store().put_state(key, &value).await?;
        self.announce(DeskEvent::State {
            key: entry.key.clone(),
            value: entry.value.clone(),
        });
        Ok(entry)
    }

    /// Removes `key`; watchers hear it as the key's value turning `null`.
    pub async fn delete_state(&self, key: &str) -> Result<(), DeskError> {
        check_key(key)?;
        if !self.store().delete_state(key).await? {
            return Err(DeskError::NotFound);
        }
        self.announce(DeskEvent::State {
            key: key.to_string(),
            value: Value::Null,
        });
        Ok(())
    }

    pub async fn list_state(&self, prefix: &str, values: bool) -> Result<Vec<Listed>, DeskError> {
        Ok(self.store().list_state(prefix, values).await?)
    }
}

struct StateApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<StateApi>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    values: bool,
}

#[derive(Deserialize)]
struct PutBody {
    value: Value,
}

async fn guard(s: &StateApi, headers: &HeaderMap, token: Option<&str>) -> Result<(), Response> {
    s.auth
        .authorize(headers, token)
        .await
        .map(drop)
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

fn too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({ "error": format!("a value is at most {MAX_VALUE_BYTES} bytes of JSON") })),
    )
        .into_response()
}

async fn get_one(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(key): Path<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    s.desk.state(&key).await.map(Json).into_response()
}

async fn put_one(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(key): Path<String>,
    body: Result<Json<PutBody>, JsonRejection>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let value = match body {
        Ok(Json(b)) => b.value,
        Err(JsonRejection::BytesRejection(_)) => return too_large(),
        Err(_) => {
            return DeskError::Invalid("the body is {\"value\": <any JSON>}".into()).into_response()
        }
    };
    if serde_json::to_vec(&value).map_or(true, |v| v.len() > MAX_VALUE_BYTES) {
        return too_large();
    }
    s.desk
        .put_state(&key, value)
        .await
        .map(Json)
        .into_response()
}

async fn delete_one(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(key): Path<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    s.desk
        .delete_state(&key)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn list(State(s): S, h: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    s.desk
        .list_state(&q.prefix, q.values)
        .await
        .map(Json)
        .into_response()
}

pub(crate) fn router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/state", get(list))
        .route(
            "/api/desk/state/{*key}",
            get(get_one).put(put_one).delete(delete_one),
        )
        // Room for the envelope and escaping around a value at the limit;
        // the value itself is measured above.
        .layer(DefaultBodyLimit::max(MAX_VALUE_BYTES * 2))
        .with_state(Arc::new(StateApi { desk, auth }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use crate::testing::{allow_all, desk_on};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use workforce_postgres::test_support::with_test_pool;

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn values_are_kept_listed_by_prefix_removed_and_announced() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let desk = desk_on(pool, dir.path()).await;
            let (_, mut events) = desk.subscribe();
            let app = router(desk.clone(), allow_all());

            let (status, _) = call(&app, "GET", "/api/desk/state/layout/main", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let layout = json!({ "panes": [{ "id": "a", "x": 0.0, "w": 0.5 }], "focus": "a" });
            let (status, put) = call(
                &app,
                "PUT",
                "/api/desk/state/layout/main",
                Some(json!({ "value": layout })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(put["key"], "layout/main");
            call(
                &app,
                "PUT",
                "/api/desk/state/layout/grid-1",
                Some(json!({ "value": [1, 2] })),
            )
            .await;
            call(
                &app,
                "PUT",
                "/api/desk/state/layout_x",
                Some(json!({ "value": true })),
            )
            .await;
            call(
                &app,
                "PUT",
                "/api/desk/state/selection",
                Some(json!({ "value": "s1" })),
            )
            .await;

            // Committed: a store on the same database reads it back.
            let fresh = DeskStore::from_pool(desk.store().pool().clone());
            assert_eq!(
                fresh.state("layout/main").await.unwrap().unwrap().value,
                layout
            );

            let (status, got) = call(&app, "GET", "/api/desk/state/layout/main", None).await;
            assert_eq!((status, &got["value"]), (StatusCode::OK, &layout));
            assert!(got["updatedAt"].is_string());

            let (_, listed) = call(&app, "GET", "/api/desk/state?prefix=layout/", None).await;
            let keys: Vec<_> = listed
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["key"].clone())
                .collect();
            assert_eq!(keys, vec![json!("layout/grid-1"), json!("layout/main")]);
            assert!(listed[0].get("value").is_none());
            // `_` is not a wildcard.
            let (_, literal) = call(
                &app,
                "GET",
                "/api/desk/state?prefix=layout_&values=true",
                None,
            )
            .await;
            assert_eq!(
                literal,
                json!([{ "key": "layout_x", "value": true, "updatedAt": literal[0]["updatedAt"] }])
            );

            let (status, _) = call(&app, "DELETE", "/api/desk/state/selection", None).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let (status, _) = call(&app, "DELETE", "/api/desk/state/selection", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let mut heard = Vec::new();
            while let Ok(e) = events.try_recv() {
                if let DeskEvent::State { key, value } = e {
                    heard.push((key, value));
                }
            }
            assert_eq!(heard.first(), Some(&("layout/main".to_string(), layout)));
            assert_eq!(heard.last(), Some(&("selection".to_string(), Value::Null)));
            let wire = serde_json::to_value(DeskEvent::State {
                key: "k".into(),
                value: json!(1),
            })
            .unwrap();
            assert_eq!(wire, json!({ "type": "state", "key": "k", "value": 1 }));
        })
        .await;
    }

    #[tokio::test]
    async fn oversized_values_and_bad_bodies_are_refused() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let app = router(desk_on(pool, dir.path()).await, allow_all());
            let big = "x".repeat(MAX_VALUE_BYTES);
            let (status, _) = call(
                &app,
                "PUT",
                "/api/desk/state/big",
                Some(json!({ "value": big })),
            )
            .await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
            let huge = "x".repeat(MAX_VALUE_BYTES * 3);
            let (status, _) = call(
                &app,
                "PUT",
                "/api/desk/state/big",
                Some(json!({ "value": huge })),
            )
            .await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
            let fits = "x".repeat(MAX_VALUE_BYTES - 2);
            let (status, _) = call(
                &app,
                "PUT",
                "/api/desk/state/big",
                Some(json!({ "value": fits })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) =
                call(&app, "PUT", "/api/desk/state/k", Some(json!({ "nope": 1 }))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        })
        .await;
    }

    #[tokio::test]
    async fn state_routes_are_guarded() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let app = router(desk_on(pool, dir.path()).await, crate::testing::deny_all());
            let (status, _) = call(&app, "GET", "/api/desk/state", None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            let (status, _) = call(
                &app,
                "PUT",
                "/api/desk/state/k",
                Some(json!({ "value": 1 })),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        })
        .await;
    }
}

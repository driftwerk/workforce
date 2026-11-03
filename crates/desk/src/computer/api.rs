//! Desk's routes for what agents may touch on this computer, under the
//! desk's own [`Authorize`].
//!
//! - `GET /api/desk/computer/disk-access`: whether macOS lets the hub read
//!   other apps' files (Full Disk Access), as the hub itself finds it.
//! - `GET|PUT /api/desk/computer/access`: the kill switch, `{"stopped"}`.
//! - `GET /api/desk/computer/grants[?agent=]`: what is always allowed, and
//!   for which session; `DELETE /api/desk/computer/grants/{id}` revokes one.
//! - `POST /api/desk/computer/sessions/{session_id}/forget`: drops a
//!   session's allowances.
//! - `GET /api/desk/computer/activity[?agent=&kind=&before=]`: a page of
//!   what agents did here, newest first, and where the next starts.
//!
//! An agent's level is its `[computer]` table, set through the agent's
//! config like any other setting.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::store::{ActivityFilter, KeptGrant};

use crate::api::Authorize;
use crate::service::Desk;

/// Whether the hub may read the files macOS keeps from apps without Full
/// Disk Access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskAccess {
    Granted,
    Missing,
    /// It couldn't be told: not macOS, or the probe failed some other way.
    Unknown,
}

/// The file only Full Disk Access opens: the privacy database itself.
fn tcc_db(home: &Path) -> PathBuf {
    home.join("Library/Application Support/com.apple.TCC/TCC.db")
}

/// [`DiskAccess`] as opening `path` read-only finds it.
pub fn probe(path: &Path) -> DiskAccess {
    match std::fs::File::open(path) {
        Ok(_) => DiskAccess::Granted,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DiskAccess::Missing,
        Err(_) => DiskAccess::Unknown,
    }
}

/// [`probe`] of this computer.
pub fn disk_access() -> DiskAccess {
    if !cfg!(target_os = "macos") {
        return DiskAccess::Unknown;
    }
    match std::env::var_os("HOME") {
        Some(home) => probe(&tcc_db(Path::new(&home))),
        None => DiskAccess::Unknown,
    }
}

struct ComputerApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

type S = State<Arc<ComputerApi>>;

async fn guard(s: &ComputerApi, h: &HeaderMap, q: &TokenQuery) -> Result<(), Response> {
    s.auth
        .authorize(h, q.token.as_deref())
        .await
        .map(drop)
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

async fn get_disk_access(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    let state = tokio::task::spawn_blocking(disk_access)
        .await
        .unwrap_or(DiskAccess::Unknown);
    (StatusCode::OK, Json(json!({ "state": state }))).into_response()
}

fn failed(e: eyre::Report) -> Response {
    tracing::error!(error = format!("{e:#}"), "desk computer request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Desk couldn't read what agents may do here." })),
    )
        .into_response()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// A grant as Settings lists it: its folder as the person reads it.
#[derive(Serialize)]
struct GrantView {
    id: Uuid,
    agent: String,
    ability: workforce_core::computer::Ability,
    folder: Option<String>,
    connector: Option<String>,
    session_id: Option<String>,
    run_name: Option<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl GrantView {
    fn of(kept: KeptGrant, home: &Path) -> Self {
        Self {
            id: kept.grant.id,
            agent: kept.grant.agent,
            ability: kept.grant.ability,
            folder: kept.grant.folder.map(|f| super::policy::shown(&f, home)),
            connector: kept.grant.connector,
            session_id: kept.grant.session_id,
            run_name: kept.run_name,
            expires_at: kept.expires_at,
            created_at: kept.created_at,
        }
    }
}

async fn get_access(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    match s.desk.store().access_stopped().await {
        Ok(stopped) => Json(json!({ "stopped": stopped })).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct AccessBody {
    stopped: bool,
}

async fn put_access(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Option<Json<AccessBody>>,
) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    let Some(Json(body)) = body else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the body is {\"stopped\": true|false}" })),
        )
            .into_response();
    };
    match s.desk.store().set_access_stopped(body.stopped).await {
        Ok(()) => {
            tracing::info!(stopped = body.stopped, "access to this computer set");
            Json(json!({ "stopped": body.stopped })).into_response()
        }
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct GrantsQuery {
    token: Option<String>,
    agent: Option<String>,
}

async fn list_grants(State(s): S, h: HeaderMap, Query(q): Query<GrantsQuery>) -> Response {
    let t = TokenQuery {
        token: q.token.clone(),
    };
    if let Err(r) = guard(&s, &h, &t).await {
        return r;
    }
    let home = home();
    match s.desk.store().grants(q.agent.as_deref()).await {
        Ok(grants) => Json(
            grants
                .into_iter()
                .map(|g| GrantView::of(g, &home))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => failed(e),
    }
}

async fn revoke_grant(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    UrlPath(id): UrlPath<Uuid>,
) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    match s.desk.store().revoke_grant(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such grant" })),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

async fn forget_session(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    UrlPath(session): UrlPath<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    match s.desk.store().forget_session_grants(&session).await {
        Ok(n) => Json(json!({ "forgotten": n })).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct ActivityQuery {
    token: Option<String>,
    agent: Option<String>,
    kind: Option<String>,
    /// A page's `next`: `<micros>~<id>`.
    before: Option<String>,
}

fn decode_before(s: &str) -> Option<(chrono::DateTime<chrono::Utc>, Uuid)> {
    let (at, id) = s.split_once('~')?;
    Some((
        chrono::DateTime::from_timestamp_micros(at.parse().ok()?)?,
        id.parse().ok()?,
    ))
}

async fn list_activity(State(s): S, h: HeaderMap, Query(q): Query<ActivityQuery>) -> Response {
    let t = TokenQuery {
        token: q.token.clone(),
    };
    if let Err(r) = guard(&s, &h, &t).await {
        return r;
    }
    let before = match q.before.as_deref() {
        None => None,
        Some(b) => match decode_before(b) {
            Some(b) => Some(b),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "before is a page's next" })),
                )
                    .into_response()
            }
        },
    };
    let filter = ActivityFilter {
        agent: q.agent,
        kind: q.kind,
        before,
    };
    match s.desk.store().activity(&filter).await {
        Ok(rows) => {
            let next = (rows.len() as i64 == super::store::PAGE)
                .then(|| {
                    rows.last()
                        .map(|r| format!("{}~{}", r.at.timestamp_micros(), r.id))
                })
                .flatten();
            Json(json!({ "rows": rows, "next": next })).into_response()
        }
        Err(e) => failed(e),
    }
}

pub(crate) fn computer_router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/computer/disk-access", get(get_disk_access))
        .route("/api/desk/computer/access", get(get_access).put(put_access))
        .route("/api/desk/computer/grants", get(list_grants))
        .route("/api/desk/computer/grants/{id}", delete(revoke_grant))
        .route(
            "/api/desk/computer/sessions/{session_id}/forget",
            post(forget_session),
        )
        .route("/api/desk/computer/activity", get(list_activity))
        .with_state(Arc::new(ComputerApi { desk, auth }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::store::{Activity, NewGrant};
    use crate::store::build_pool;
    use crate::testing::{allow_all, deny_all};
    use serde_json::Value;
    use workforce_postgres::test_support::with_test_pool;

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(axum::body::Body::empty, |b| {
                axum::body::Body::from(b.to_string())
            }))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn the_switch_grants_and_activity_are_read_and_changed_by_desk_alone() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let desk = crate::testing::desk_on(pool, dir.path()).await;
            let closed = computer_router(desk.clone(), deny_all());
            for (method, uri) in [
                ("GET", "/api/desk/computer/access"),
                ("GET", "/api/desk/computer/grants"),
                ("GET", "/api/desk/computer/activity"),
                ("GET", "/api/desk/computer/disk-access"),
            ] {
                assert_eq!(call(&closed, method, uri, None).await.0, 401, "{uri}");
            }
            let app = computer_router(desk.clone(), allow_all());
            assert_eq!(
                call(&app, "GET", "/api/desk/computer/access", None).await,
                (200, json!({"stopped": false}))
            );
            assert_eq!(
                call(
                    &app,
                    "PUT",
                    "/api/desk/computer/access",
                    Some(json!({"stopped": true}))
                )
                .await,
                (200, json!({"stopped": true}))
            );
            assert!(desk.store().access_stopped().await.unwrap());
            assert_eq!(
                call(&app, "PUT", "/api/desk/computer/access", None).await.0,
                400
            );

            let home = home();
            let grant = |session: Option<&str>| NewGrant {
                hub: "local".into(),
                agent: "allan".into(),
                person: "sarah".into(),
                ability: workforce_core::computer::Ability::Run,
                folder: Some(home.join("dev/acme-web")),
                connector: None,
                session_id: session.map(str::to_string),
                expires_at: None,
                run: None,
                run_name: None,
            };
            let kept = desk.store().add_grant(&grant(None)).await.unwrap();
            desk.store().add_grant(&grant(Some("s1"))).await.unwrap();
            let (status, listed) =
                call(&app, "GET", "/api/desk/computer/grants?agent=allan", None).await;
            assert_eq!(status, 200);
            assert_eq!(listed.as_array().unwrap().len(), 2);
            assert!(listed
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["folder"] == "~/dev/acme-web"));
            assert_eq!(
                call(&app, "GET", "/api/desk/computer/grants?agent=ben", None)
                    .await
                    .1,
                json!([])
            );
            assert_eq!(
                call(&app, "POST", "/api/desk/computer/sessions/s1/forget", None).await,
                (200, json!({"forgotten": 1}))
            );
            let uri = format!("/api/desk/computer/grants/{}", kept.id);
            assert_eq!(call(&app, "DELETE", &uri, None).await.0, 204);
            assert_eq!(call(&app, "DELETE", &uri, None).await.0, 404);

            for kind in ["exec", "write"] {
                desk.store()
                    .record_activity(&Activity {
                        hub: "local".into(),
                        agent: "allan".into(),
                        person: Some("sarah".into()),
                        session_id: None,
                        request_id: None,
                        kind: kind.into(),
                        doing: "npm test".into(),
                        folder: None,
                        connector: None,
                        tool: "exec.run".into(),
                        vault_source: None,
                        decided_by: "person".into(),
                        outcome: json!({"outcome": "allowed"}),
                        reason: None,
                        exit_code: None,
                        duration_ms: None,
                        arguments: json!({}),
                        device: None,
                    })
                    .await
                    .unwrap();
            }
            let (status, page) = call(
                &app,
                "GET",
                "/api/desk/computer/activity?agent=allan&kind=exec",
                None,
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(page["rows"].as_array().unwrap().len(), 1);
            assert_eq!(page["next"], Value::Null);
            assert_eq!(
                call(&app, "GET", "/api/desk/computer/activity?before=junk", None)
                    .await
                    .0,
                400
            );
            let (status, probe) = call(&app, "GET", "/api/desk/computer/disk-access", None).await;
            assert_eq!(status, 200);
            assert!(["granted", "missing", "unknown"].contains(&probe["state"].as_str().unwrap()));
        })
        .await;
    }

    #[test]
    fn the_probe_reads_granted_missing_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("TCC.db");
        std::fs::write(&db, b"x").unwrap();
        assert_eq!(probe(&db), DiskAccess::Granted);
        assert_eq!(probe(&dir.path().join("absent.db")), DiskAccess::Unknown);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway; everyone else is refused, as without FDA.
            if std::fs::File::open(&db).is_err() {
                assert_eq!(probe(&db), DiskAccess::Missing);
            }
        }
        assert_eq!(
            tcc_db(Path::new("/Users/sarah")),
            PathBuf::from("/Users/sarah/Library/Application Support/com.apple.TCC/TCC.db")
        );
        assert_eq!(
            serde_json::to_value(DiskAccess::Missing).unwrap(),
            "missing"
        );
    }
}

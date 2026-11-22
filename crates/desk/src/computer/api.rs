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
//! - `GET /api/desk/computer/activity[?agent=&kind=&decided_by=&before=]`:
//!   a page of what agents did here, newest first, and where the next
//!   starts.
//! - `GET /api/desk/computer/on-its-own?agent=`: what a local agent does on
//!   its own; `PUT` sets one level or override, `{agent, kind, scope,
//!   level}` (`level` null asks each time again); `POST
//!   /api/desk/computer/on-its-own/ask-again` `{agent}` forgets all of it
//!   and every grant the agent holds.
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

use super::store::{level_refusal, ActivityFilter, KeptGrant, OnItsOwnRow};
use workforce_core::computer::{ActionKind, Level, Scope};

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
    decided_by: Option<String>,
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
        decided_by: q.decided_by,
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

/// The hub a local agent's levels are kept for.
const LOCAL: &str = "local";

fn bad(why: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": why.into() })),
    )
        .into_response()
}

/// A level or override as Settings lists it: a folder as the person reads it.
#[derive(Serialize)]
struct OnItsOwnView {
    kind: ActionKind,
    scope: Scope,
    level: Level,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl OnItsOwnView {
    fn of(row: OnItsOwnRow, home: &Path) -> Self {
        let scope = match row.scope {
            Scope::Folder(f) => Scope::Folder(PathBuf::from(super::policy::shown(&f, home))),
            other => other,
        };
        Self {
            kind: row.kind,
            scope,
            level: row.level,
            created_at: row.created_at,
        }
    }
}

#[derive(Deserialize)]
struct AgentQuery {
    token: Option<String>,
    agent: String,
}

async fn get_on_its_own(State(s): S, h: HeaderMap, Query(q): Query<AgentQuery>) -> Response {
    let t = TokenQuery {
        token: q.token.clone(),
    };
    if let Err(r) = guard(&s, &h, &t).await {
        return r;
    }
    let home = home();
    match s.desk.store().on_its_own_rows(LOCAL, &q.agent).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|r| OnItsOwnView::of(r, &home))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct OnItsOwnBody {
    agent: String,
    kind: ActionKind,
    scope: Scope,
    level: Option<Level>,
}

/// The longest host name.
const MAX_HOST: usize = 253;

/// `scope` as it is kept, or why it can't be: a host as a command names
/// it, less any `user@`; a site's bare origin; a folder, `~` read as home,
/// that an agent may work in, as the disk spells it with its links
/// followed.
fn kept_scope(scope: Scope, home: &Path) -> Result<Scope, String> {
    match spelled(scope, home) {
        Scope::Whole => Ok(Scope::Whole),
        Scope::Host(host) => {
            let plain = !host.is_empty()
                && host.len() <= MAX_HOST
                && (host.parse::<std::net::Ipv6Addr>().is_ok()
                    || host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')));
            match plain {
                true => Ok(Scope::Host(host)),
                false => Err(format!("{host} isn't a host name or address.")),
            }
        }
        Scope::Site(site) => {
            let bare = site
                .split_once("://")
                .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
                .is_some_and(|(_, host)| {
                    !host.is_empty()
                        && host
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
                });
            match bare {
                true => Ok(Scope::Site(site)),
                false => Err(format!(
                    "{site} isn't a site's address: write it as https://example.com."
                )),
            }
        }
        Scope::Folder(folder) => {
            let plain = folder.is_absolute()
                && folder.components().all(|c| {
                    matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                });
            if !plain {
                return Err(format!("{} isn't a full folder path.", folder.display()));
            }
            if let Some(why) =
                workforce_common::computer_access::works_in_refusal(&folder, home, &[])
            {
                return Err(why);
            }
            super::boss::resolve_real(&folder)
                .map(Scope::Folder)
                .ok_or_else(|| format!("{} leads nowhere.", folder.display()))
        }
    }
}

/// `scope` as it is kept, whatever is set there: a host lower-cased with
/// any `user@` and IPv6 brackets dropped, an IPv6 address in its standard
/// form, a site lower-cased with no
/// trailing `/`, a folder with `~` expanded and its links followed where
/// it still leads somewhere.
fn spelled(scope: Scope, home: &Path) -> Scope {
    match scope {
        Scope::Host(host) => {
            let host = host.trim();
            let host = host.rsplit('@').next().unwrap_or(host);
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            Scope::Host(super::asks::host_spelled(host))
        }
        Scope::Site(site) => Scope::Site(site.trim().trim_end_matches('/').to_ascii_lowercase()),
        Scope::Folder(folder) => {
            let folder = match folder.strip_prefix("~") {
                Ok(rest) => home.join(rest),
                Err(_) => folder,
            };
            Scope::Folder(super::boss::resolve_real(&folder).unwrap_or(folder))
        }
        Scope::Whole => Scope::Whole,
    }
}

async fn put_on_its_own(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Option<Json<OnItsOwnBody>>,
) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    let Some(Json(body)) = body else {
        return bad("the body is {\"agent\", \"kind\", \"scope\", \"level\"}");
    };
    // Asking again where it was set is never refused: what is kept is
    // removed as it was kept, however it would be judged now.
    let scope = match body.level {
        None => spelled(body.scope, &home()),
        Some(_) => match kept_scope(body.scope, &home()) {
            Ok(scope) => scope,
            Err(why) => return bad(why),
        },
    };
    if let Some(why) = body
        .level
        .and_then(|_| level_refusal(body.kind, &scope, body.level))
    {
        return bad(why);
    }
    match s
        .desk
        .store()
        .set_on_its_own(LOCAL, &body.agent, body.kind, &scope, body.level)
        .await
    {
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "It already asks each time there." })),
        )
            .into_response(),
        Ok(true) => {
            tracing::info!(agent = %body.agent, kind = body.kind.as_str(), scope = %scope.text(), level = ?body.level, "what an agent does on its own set");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct AskAgainBody {
    agent: String,
}

async fn ask_again(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Option<Json<AskAgainBody>>,
) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    let Some(Json(body)) = body else {
        return bad("the body is {\"agent\"}");
    };
    match s.desk.store().ask_again(LOCAL, &body.agent).await {
        Ok(cleared) => {
            tracing::info!(agent = %body.agent, cleared, "an agent asks about everything again");
            Json(json!({ "cleared": cleared })).into_response()
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
        .route(
            "/api/desk/computer/on-its-own",
            get(get_on_its_own).put(put_on_its_own),
        )
        .route("/api/desk/computer/on-its-own/ask-again", post(ask_again))
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

    #[tokio::test]
    async fn what_an_agent_does_on_its_own_is_set_listed_and_forgotten_by_desk_alone() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let desk = crate::testing::desk_on(pool, dir.path()).await;
            let closed = computer_router(desk.clone(), deny_all());
            let devbox = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "dev@DevBox"}, "level": "anywhere"});
            for (method, uri, body) in [
                ("GET", "/api/desk/computer/on-its-own?agent=kat", None),
                ("PUT", "/api/desk/computer/on-its-own", Some(devbox.clone())),
                (
                    "POST",
                    "/api/desk/computer/on-its-own/ask-again",
                    Some(json!({"agent": "kat"})),
                ),
                ("GET", "/api/desk/computer/activity?decided_by=on_its_own", None),
            ] {
                assert_eq!(call(&closed, method, uri, body).await.0, 401, "{uri}");
            }
            assert!(desk
                .store()
                .on_its_own_rows("local", "kat")
                .await
                .unwrap()
                .is_empty());

            let app = computer_router(desk.clone(), allow_all());
            let put = |body: Value| {
                let app = app.clone();
                async move { call(&app, "PUT", "/api/desk/computer/on-its-own", Some(body)).await }
            };
            assert_eq!(put(devbox.clone()).await.0, 204);
            assert_eq!(
                put(json!({"agent": "kat", "kind": "files", "scope": {"kind": "folder", "value": "~/dev/acme-web"}, "level": "anywhere"}))
                    .await
                    .0,
                204
            );
            assert_eq!(
                put(json!({"agent": "kat", "kind": "commands", "scope": {"kind": "whole"}, "level": "in_folders"}))
                    .await
                    .0,
                204
            );
            let (status, listed) =
                call(&app, "GET", "/api/desk/computer/on-its-own?agent=kat", None).await;
            assert_eq!(status, 200);
            let rows: Vec<(&str, &Value, &str)> = listed
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["kind"].as_str().unwrap(), &r["scope"], r["level"].as_str().unwrap()))
                .collect();
            assert_eq!(
                rows,
                vec![
                    ("commands", &json!({"kind": "whole"}), "in_folders"),
                    ("files", &json!({"kind": "folder", "value": "~/dev/acme-web"}), "anywhere"),
                    ("network", &json!({"kind": "host", "value": "devbox"}), "anywhere"),
                ]
            );

            for bad in [
                json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "devbox:22"}, "level": "anywhere"}),
                json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "dev box"}, "level": "anywhere"}),
                json!({"agent": "kat", "kind": "network", "scope": {"kind": "whole"}, "level": "in_folders"}),
                json!({"agent": "kat", "kind": "commands", "scope": {"kind": "host", "value": "devbox"}, "level": "anywhere"}),
                json!({"agent": "kat", "kind": "files", "scope": {"kind": "folder", "value": "~/.ssh"}, "level": "anywhere"}),
                json!({"agent": "kat", "kind": "browser", "scope": {"kind": "site", "value": "github.com"}, "level": "anywhere"}),
            ] {
                assert_eq!(put(bad.clone()).await.0, 400, "{bad}");
            }
            assert_eq!(
                call(&app, "PUT", "/api/desk/computer/on-its-own", None).await.0,
                400
            );
            assert_eq!(
                call(&app, "POST", "/api/desk/computer/on-its-own/ask-again", None)
                    .await
                    .0,
                400
            );

            let removed = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "devbox"}, "level": null});
            assert_eq!(put(removed.clone()).await.0, 204);
            let (status, missing) = put(removed).await;
            assert_eq!(status, 404);
            assert_eq!(missing["error"], "It already asks each time there.");
            // Asking again is never refused for how the scope is written:
            // one no level could be set for is simply not there.
            for odd in [
                json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "dev box"}, "level": null}),
                json!({"agent": "kat", "kind": "commands", "scope": {"kind": "host", "value": "devbox"}, "level": null}),
                json!({"agent": "kat", "kind": "files", "scope": {"kind": "folder", "value": "~/.ssh"}, "level": null}),
            ] {
                assert_eq!(put(odd.clone()).await.0, 404, "{odd}");
            }
            // An IPv6 address, brackets dropped, as the card reads it.
            let v6 = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "[FE80::1]"}, "level": "anywhere"});
            assert_eq!(put(v6).await.0, 204);
            let gone = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "fe80::1"}, "level": null});
            assert_eq!(put(gone).await.0, 204);
            // Kept in its one standard form, as a command naming it is read.
            let long = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "[FE80:0:0:0::0001]"}, "level": "anywhere"});
            assert_eq!(put(long).await.0, 204);
            let short = json!({"agent": "kat", "kind": "network", "scope": {"kind": "host", "value": "fe80::1"}, "level": null});
            assert_eq!(put(short).await.0, 204);

            for decided_by in ["on_its_own", "person"] {
                desk.store()
                    .record_activity(&Activity {
                        hub: "local".into(),
                        agent: "kat".into(),
                        person: Some("sarah".into()),
                        session_id: None,
                        request_id: None,
                        kind: "exec".into(),
                        doing: "git push --force".into(),
                        folder: None,
                        connector: None,
                        tool: "exec.run".into(),
                        vault_source: None,
                        decided_by: decided_by.into(),
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
                "/api/desk/computer/activity?agent=kat&decided_by=on_its_own",
                None,
            )
            .await;
            assert_eq!(status, 200);
            let rows = page["rows"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["decided_by"], "on_its_own");

            assert_eq!(
                call(
                    &app,
                    "POST",
                    "/api/desk/computer/on-its-own/ask-again",
                    Some(json!({"agent": "kat"}))
                )
                .await,
                (200, json!({"cleared": 2}))
            );
            assert_eq!(
                call(&app, "GET", "/api/desk/computer/on-its-own?agent=kat", None)
                    .await
                    .1,
                json!([])
            );
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

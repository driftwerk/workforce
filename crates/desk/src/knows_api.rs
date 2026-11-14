//! `/api/desk/knows`: what each agent knows of the work in Desk ("What they
//! see"), as the person sets it. The rules are read by whoever may use the
//! desk; they are changed, and what an agent holds is deleted or kept, by
//! the person who holds this hub alone: never an agent or a session, so no
//! agent can widen what it, or another, is told.
//!
//! - `GET /api/desk/knows`: every rule that has been set.
//! - `GET /api/desk/knows/{hub}/{agent}`: one agent's rule (its own work
//!   alone until one is set), what it holds outside it, and what was shared
//!   with it today, by kind.
//! - `PUT /api/desk/knows/{hub}/{agent}` `{places, transcripts, digest}`:
//!   keeps the rule, and answers what the agent holds that it no longer
//!   covers. Nothing more of that is shared meanwhile.
//! - `POST /api/desk/knows/{hub}/{agent}/held` `{choice: "delete"|"keep"}`:
//!   the person's choice for it, logged.
//! - `GET /api/desk/knows/{hub}/{agent}/log`: the rule's changes and the
//!   choices, newest first: counts and places, never content.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::api::Authorize;
use crate::knows::{agent_key_ok, Digest, Rule};
use crate::service::{Desk, DeskError};

struct KnowsApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

type S = State<Arc<KnowsApi>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn failed(e: DeskError) -> Response {
    match e {
        DeskError::Invalid(why) => error(StatusCode::BAD_REQUEST, why),
        e => {
            tracing::error!(error = %e, "desk knows request failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Desk couldn't read what its agents see.",
            )
        }
    }
}

/// The agent a path names, as rules are kept by.
fn agent_of(hub: &str, agent: &str) -> Option<String> {
    let key = format!("{hub}/{agent}");
    agent_key_ok(&key).then_some(key)
}

fn not_an_agent() -> Response {
    error(StatusCode::BAD_REQUEST, "That isn't an agent.")
}

async fn rules(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return error(status, "not allowed");
    }
    match s.desk.knows_rules().await {
        Ok(rules) => Json(json!({ "rules": rules })).into_response(),
        Err(e) => failed(e),
    }
}

async fn rule(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((hub, agent)): Path<(String, String)>,
) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return error(status, "not allowed");
    }
    let Some(agent) = agent_of(&hub, &agent) else {
        return not_an_agent();
    };
    let view = match s.desk.knows_rule(&agent).await {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let kept = match s.desk.store().knows_rule(&agent).await {
        Ok(k) => k.map(|k| k.rule).unwrap_or_default(),
        Err(e) => return failed(DeskError::Internal(e)),
    };
    let outside = match s.desk.knows_outside(&agent, &kept).await {
        Ok((_, count)) => count,
        Err(e) => return failed(e),
    };
    let today = match s
        .desk
        .store()
        .knows_shared_since(&agent, Utc::now() - Duration::hours(24))
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(kind, items, bytes)| json!({ "kind": kind, "items": items, "bytes": bytes }))
            .collect::<Vec<_>>(),
        Err(e) => return failed(DeskError::Internal(e)),
    };
    Json(json!({ "rule": view, "heldOutside": outside, "sharedToday": today })).into_response()
}

#[derive(Deserialize)]
struct RuleBody {
    places: Vec<String>,
    /// Left out: as it was.
    #[serde(default)]
    transcripts: Option<bool>,
    /// Left out: as it was.
    #[serde(default)]
    digest: Option<String>,
}

async fn put_rule(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((hub, agent)): Path<(String, String)>,
    Json(body): Json<RuleBody>,
) -> Response {
    if let Err(status) = s.auth.authorize_holder(&h, q.token.as_deref()).await {
        return error(status, "Only you can change what an agent sees.");
    }
    let Some(agent) = agent_of(&hub, &agent) else {
        return not_an_agent();
    };
    let before = match s.desk.store().knows_rule(&agent).await {
        Ok(k) => k.map(|k| k.rule).unwrap_or_default(),
        Err(e) => return failed(DeskError::Internal(e)),
    };
    let digest = match body.digest.as_deref().map(str::trim) {
        None | Some("") => before.digest,
        Some(d) => match Digest::parse(d) {
            Ok(d) => d,
            Err(_) => return error(StatusCode::BAD_REQUEST, "digest is off, live or daily."),
        },
    };
    let transcripts = body.transcripts.unwrap_or(before.transcripts);
    let rule = match Rule::new(&body.places, transcripts, digest) {
        Ok(r) => r,
        Err(why) => return error(StatusCode::BAD_REQUEST, why),
    };
    match s.desk.knows_rule_set(&agent, &rule).await {
        Ok(changed) => Json(changed).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct Choice {
    choice: String,
}

async fn held(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((hub, agent)): Path<(String, String)>,
    Json(body): Json<Choice>,
) -> Response {
    if let Err(status) = s.auth.authorize_holder(&h, q.token.as_deref()).await {
        return error(status, "Only you can choose what an agent keeps.");
    }
    let Some(agent) = agent_of(&hub, &agent) else {
        return not_an_agent();
    };
    let delete = match body.choice.as_str() {
        "delete" => true,
        "keep" => false,
        _ => return error(StatusCode::BAD_REQUEST, "choice is delete or keep."),
    };
    match s.desk.knows_choose(&agent, delete).await {
        Ok(count) => Json(json!({ "choice": body.choice, "covered": count })).into_response(),
        Err(e) => failed(e),
    }
}

async fn log(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((hub, agent)): Path<(String, String)>,
) -> Response {
    if let Err(status) = s.auth.authorize(&h, q.token.as_deref()).await {
        return error(status, "not allowed");
    }
    let Some(agent) = agent_of(&hub, &agent) else {
        return not_an_agent();
    };
    match s.desk.store().knows_log(&agent).await {
        Ok(entries) => Json(json!({ "log": entries })).into_response(),
        Err(e) => failed(DeskError::Internal(e)),
    }
}

pub(crate) fn knows_router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/knows", get(rules))
        .route("/api/desk/knows/{hub}/{agent}", get(rule).put(put_rule))
        .route("/api/desk/knows/{hub}/{agent}/held", post(held))
        .route("/api/desk/knows/{hub}/{agent}/log", get(log))
        .with_state(Arc::new(KnowsApi { desk, auth }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::knows::TRANSCRIPT;
    use crate::knows_store::Sent;
    use crate::store::{build_pool, DeskStore};
    use crate::testing::Bearer;

    async fn desk(pool: workforce_postgres::PostgresPool, dir: &std::path::Path) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        Desk::start(
            DeskStore::from_pool(pool),
            crate::service::Options {
                programs: crate::testing::programs("/bin/false".into(), "/bin/false".into()),
                env: BTreeMap::new(),
                run_dir: dir.join("run"),
                scrollback_bytes: 1024,
                approval_timeout: std::time::Duration::from_secs(30),
                restore: crate::service::RestorePolicy::Off,
                adopt_every: None,
                holder: None,
                hub_internal: Default::default(),
                account_keys: None,
                line_reach: crate::line::Reach::Group,
                memory: Default::default(),
                bosses: Default::default(),
            },
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer hub-token");
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let response = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_rule_is_set_read_narrowed_and_what_is_held_outside_is_deleted_or_kept() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let app = knows_router(d.clone(), Arc::new(Bearer));
            let uri = "/api/desk/knows/driftwerk/allan";
            // Nothing set: its own work alone.
            let (status, body) = send(&app, "GET", uri, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["rule"]["places"], json!([]));
            assert_eq!(body["rule"]["transcripts"], false);
            // Everything, with transcripts.
            let (status, body) = send(
                &app,
                "PUT",
                uri,
                Some(json!({ "places": ["all"], "transcripts": true, "digest": "live" })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["rule"]["places"], json!(["all"]));
            assert_eq!(body["heldOutside"]["items"], 0);
            // It was sent a transcript and an account.
            let s = uuid::Uuid::from_u128(1);
            let held = |kind: &str, r: &str| Sent {
                held: crate::knows::Held {
                    kind: kind.into(),
                    reference: r.into(),
                    session: Some(s),
                    group: None,
                    board: None,
                    boss: None,
                },
                bytes: 10,
                hash: None,
            };
            d.store()
                .knows_sent_put(
                    "driftwerk/allan",
                    &[held(TRANSCRIPT, "x1"), held("account", "a1")],
                    Utc::now(),
                )
                .await
                .unwrap();
            // Transcripts off: the transcript is held outside.
            let (_, body) = send(
                &app,
                "PUT",
                uri,
                Some(json!({ "places": ["all"], "transcripts": false })),
            )
            .await;
            assert_eq!(body["heldOutside"]["items"], 1);
            assert_eq!(body["heldOutside"]["transcripts"], 1);
            assert_eq!(body["rule"]["revision"], 2);
            let (status, body) = send(
                &app,
                "POST",
                &format!("{uri}/held"),
                Some(json!({ "choice": "keep" })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["covered"]["items"], 1);
            // Kept: settled, so nothing waits on the person, and a delete
            // after it covers nothing.
            let (_, body) = send(&app, "GET", uri, None).await;
            assert_eq!(body["heldOutside"]["items"], 0);
            let (_, body) = send(
                &app,
                "POST",
                &format!("{uri}/held"),
                Some(json!({ "choice": "delete" })),
            )
            .await;
            assert_eq!(body["covered"]["items"], 0);
            assert!(d.store().knows_pending_deletes().await.unwrap().is_empty());
            // Narrowed to a group the session isn't in: the account is
            // outside now, and deleting it waits on the agent's hub.
            let (_, body) = send(
                &app,
                "PUT",
                uri,
                Some(json!({ "places": ["group:patchwork"] })),
            )
            .await;
            assert_eq!(body["heldOutside"]["items"], 1);
            assert_eq!(body["rule"]["transcripts"], false, "left out: as it was");
            assert_eq!(body["rule"]["digest"], "live", "left out: as it was");
            let (_, body) = send(
                &app,
                "POST",
                &format!("{uri}/held"),
                Some(json!({ "choice": "delete" })),
            )
            .await;
            assert_eq!(body["covered"]["items"], 1);
            let pending = d.store().knows_pending_deletes().await.unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(
                d.store()
                    .knows_deleting("driftwerk/allan", pending[0].1.id)
                    .await
                    .unwrap(),
                [("account".to_string(), "a1".to_string())]
            );
            let (_, body) = send(&app, "GET", &format!("{uri}/log"), None).await;
            let whats: Vec<&str> = body["log"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["what"].as_str().unwrap())
                .collect();
            assert_eq!(whats, ["delete", "rule", "keep", "rule", "rule"]);
            let (_, body) = send(&app, "GET", "/api/desk/knows", None).await;
            assert_eq!(body["rules"].as_array().unwrap().len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn the_world_reads_each_grid_by_its_key_and_skips_the_index() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = uuid::Uuid::from_u128(5);
            let pane =
                json!({ "id": "a", "content": { "kind": "session", "sessionId": s.to_string() } });
            d.put_state(
                "grid.g1",
                json!({ "id": "other", "name": "One", "panes": [pane] }),
            )
            .await
            .unwrap();
            d.put_state("grid.index", json!(["g1"])).await.unwrap();
            let w = d.knows_world().await.unwrap();
            assert_eq!(w.grids.len(), 1);
            assert_eq!(
                (w.grids[0].id.as_str(), w.grids[0].sessions.as_slice()),
                ("g1", &[s][..])
            );
            let rule = Rule::new(&["grid:other".into()], false, Digest::Off).unwrap();
            assert!(!crate::knows::resolve("local/mia", &rule, &w)
                .sessions
                .contains(&s));
        })
        .await;
    }

    /// Lets anyone use the desk, and no one hold it.
    struct NotTheHolder;

    #[async_trait::async_trait]
    impl Authorize for NotTheHolder {
        async fn authorize(
            &self,
            _: &HeaderMap,
            _: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Ok(None)
        }
        async fn authorize_holder(
            &self,
            _: &HeaderMap,
            _: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Err(StatusCode::FORBIDDEN)
        }
        fn is_hub_token(&self, _: &HeaderMap) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn only_the_holder_changes_a_rule_or_chooses_what_is_kept() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let app = knows_router(desk(pool, dir.path()).await, Arc::new(NotTheHolder));
            let uri = "/api/desk/knows/driftwerk/allan";
            let (status, _) = send(&app, "PUT", uri, Some(json!({ "places": ["all"] }))).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let (status, _) = send(
                &app,
                "POST",
                &format!("{uri}/held"),
                Some(json!({ "choice": "delete" })),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(send(&app, "GET", uri, None).await.0, StatusCode::OK);
            assert_eq!(
                send(&app, "GET", "/api/desk/knows", None).await.0,
                StatusCode::OK
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_bad_rule_or_agent_is_refused_plainly_and_a_stranger_reaches_nothing() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let app = knows_router(desk(pool, dir.path()).await, Arc::new(Bearer));
            let (status, body) = send(
                &app,
                "PUT",
                "/api/desk/knows/driftwerk/allan",
                Some(json!({ "places": ["repo:a/b"] })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].as_str().unwrap().contains("kind of place"));
            let (status, _) = send(
                &app,
                "PUT",
                "/api/desk/knows/driftwerk/allan",
                Some(json!({ "places": [], "digest": "hourly" })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _) = send(&app, "GET", "/api/desk/knows/drift%20werk/allan", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _) = send(
                &app,
                "POST",
                "/api/desk/knows/driftwerk/allan/held",
                Some(json!({ "choice": "maybe" })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/desk/knows")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        })
        .await;
    }
}

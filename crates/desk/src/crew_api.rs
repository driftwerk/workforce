//! `/api/desk/activity`, `/overview`, `/manager` and `/suggestions`: the
//! crew as Floor and the overseer's DM read it, and the person's answers to
//! the overseer's suggestions.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::Authorize;
use crate::crew::{By, Crew, Filter, Kind, ManagerView, SuggestionState};
use crate::service::Desk;

struct CrewApi {
    desk: Desk,
    auth: Arc<dyn Authorize>,
    crew: Arc<dyn Crew>,
}

type S = State<Arc<CrewApi>>;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn failed(e: eyre::Report) -> Response {
    tracing::error!(error = %e, "desk crew request failed");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the hub could not read the crew",
    )
}

async fn guard(s: &CrewApi, h: &HeaderMap, token: Option<&str>) -> Result<(), Response> {
    s.auth
        .authorize(h, token)
        .await
        .map_err(|status| error(status, "not allowed"))
}

#[derive(Deserialize)]
struct ActivityQuery {
    token: Option<String>,
    limit: Option<i64>,
    before: Option<String>,
    #[serde(default)]
    filter: Filter,
}

async fn activity(State(s): S, h: HeaderMap, Query(q): Query<ActivityQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let before = q.before.as_deref().filter(|b| !b.trim().is_empty());
    let page = match q.filter {
        Filter::NeedsYou => s.desk.needs_you(&*s.crew, before, limit).await,
        filter => s.desk.activity(filter, before, limit).await,
    };
    match page {
        Ok(page) => Json(page).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn overview(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    match s.desk.overview(&*s.crew).await {
        Ok(o) => Json(o).into_response(),
        Err(e) => failed(e),
    }
}

async fn manager(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let o = s.crew.overseer();
    Json(ManagerView {
        agent_id: o.as_ref().map(|o| o.id.clone()),
        level: o.map(|o| o.level),
    })
    .into_response()
}

async fn suggestions(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    match s.desk.store().suggestions(200).await {
        Ok(list) => Json(json!({ "suggestions": list })).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Accept,
    Dismiss,
}

async fn answer(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path((id, what)): Path<(String, Answer)>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let mut suggestion = match s.desk.store().suggestion(&id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "That suggestion isn’t there any more.",
            )
        }
        Err(e) => return failed(e),
    };
    match suggestion.state {
        SuggestionState::Open => {}
        SuggestionState::Done => {
            return error(StatusCode::CONFLICT, "That suggestion was already done.")
        }
        SuggestionState::Dismissed => {
            return error(StatusCode::CONFLICT, "That suggestion was dismissed.")
        }
    }
    match what {
        Answer::Accept => {
            if let Err(problem) = s.crew.accept(&suggestion).await {
                return error(StatusCode::BAD_REQUEST, problem);
            }
            suggestion.state = SuggestionState::Done;
        }
        Answer::Dismiss => {
            suggestion.state = SuggestionState::Dismissed;
            let about = suggestion
                .subject
                .as_ref()
                .map_or("a suggestion".to_string(), |p| {
                    format!("the suggestion about {}", p.name)
                });
            let mut by = By::you();
            by.suggestion_id = Some(suggestion.id.clone());
            s.desk.note(by.event(
                Kind::Dismissed,
                suggestion.subject.clone(),
                format!("dismissed {about}"),
            ));
        }
    }
    if let Err(e) = s.desk.store().put_suggestion(&suggestion).await {
        return failed(e);
    }
    Json(suggestion).into_response()
}

pub(crate) fn crew_router(desk: Desk, auth: Arc<dyn Authorize>, crew: Arc<dyn Crew>) -> Router {
    Router::new()
        .route("/api/desk/activity", get(activity))
        .route("/api/desk/overview", get(overview))
        .route("/api/desk/manager", get(manager))
        .route("/api/desk/suggestions", get(suggestions))
        .route("/api/desk/suggestions/{id}/{answer}", post(answer))
        .with_state(Arc::new(CrewApi { desk, auth, crew }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use chrono::{DateTime, Utc};
    use parking_lot::Mutex;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::crew::{CrewAgent, Event, Overseer, Party, Suggestion};
    use crate::store::{build_pool, DeskStore};
    use crate::testing::Bearer;

    /// Oversees as Desk, at `suggest`; accepting remembers what it did.
    #[derive(Default)]
    struct Hub(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Crew for Hub {
        fn overseer(&self) -> Option<Overseer> {
            Some(Overseer {
                id: "desk".into(),
                name: "Desk".into(),
                level: "suggest".into(),
                quiet_minutes: 30,
            })
        }
        fn agents(&self) -> Vec<CrewAgent> {
            vec![CrewAgent {
                id: "scout".into(),
                name: "Scout".into(),
                live: None,
            }]
        }
        fn midnight(&self) -> DateTime<Utc> {
            Utc::now() - chrono::Duration::hours(1)
        }
        async fn accept(&self, s: &Suggestion) -> Result<(), String> {
            if s.text == "impossible" {
                return Err("That session isn’t running any more.".into());
            }
            self.0.lock().push(s.id.clone());
            Ok(())
        }
    }

    async fn desk(pool: workforce_postgres::PostgresPool, dir: &std::path::Path) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        Desk::start(
            DeskStore::from_pool(pool),
            crate::service::Options {
                claude: "/bin/false".into(),
                codex: "/bin/false".into(),
                env: BTreeMap::new(),
                run_dir: dir.join("run"),
                scrollback_bytes: 1024,
                approval_timeout: std::time::Duration::from_secs(30),
                restore: crate::service::RestorePolicy::Off,
                adopt_every: None,
                holder: None,
            },
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", "Bearer hub-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn settle(d: &Desk, n: usize) {
        for _ in 0..100 {
            if d.activity(Filter::All, None, 500)
                .await
                .unwrap()
                .events
                .len()
                >= n
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn activity_overview_manager_and_suggestions_are_served() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let hub = Arc::new(Hub::default());
            let app = crew_router(d.clone(), Arc::new(Bearer), hub.clone());
            let scout = Party::agent("scout", "Scout");
            d.note(
                Event::new(
                    Kind::Question,
                    scout.clone(),
                    Some(scout.clone()),
                    "asked “main or dev?”",
                )
                .detail(json!({"question": {"text": "main or dev?", "options": ["main", "dev"]}})),
            );
            d.note(Event::new(
                Kind::Nudged,
                Party::manager("desk", "Desk"),
                Some(scout.clone()),
                "asked Scout how it’s going",
            ));
            settle(&d, 2).await;

            let (status, page) = call(&app, "GET", "/api/desk/activity?limit=1&filter=all").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["events"][0]["kind"], "nudged");
            assert_eq!(page["events"][0]["actor"]["kind"], "manager");
            assert_eq!(page["more"], true);
            let older = page["events"][0]["id"].as_str().unwrap();
            let (_, page) = call(&app, "GET", &format!("/api/desk/activity?before={older}")).await;
            assert_eq!(page["events"][0]["kind"], "question");
            assert_eq!(page["more"], false);
            let (_, page) = call(&app, "GET", "/api/desk/activity?filter=needs_you").await;
            assert_eq!(page["events"].as_array().unwrap().len(), 1);
            let (_, page) = call(&app, "GET", "/api/desk/activity?filter=manager").await;
            assert_eq!(page["events"][0]["text"], "asked Scout how it’s going");

            let (_, o) = call(&app, "GET", "/api/desk/overview").await;
            assert_eq!(
                o["counts"],
                json!({"working": 0, "needsYou": 1, "doneToday": 0})
            );
            assert_eq!(o["items"][0]["kind"], "agent");
            assert_eq!(o["items"][0]["state"], "needs_you");
            assert_eq!(o["items"][0]["question"]["options"], json!(["main", "dev"]));
            assert_eq!(o["brief"], Value::Null);

            let (_, m) = call(&app, "GET", "/api/desk/manager").await;
            assert_eq!(m, json!({"agentId": "desk", "level": "suggest"}));

            let made = |text: &str| Suggestion {
                id: crate::crew::new_id(),
                at: crate::crew::now(),
                text: text.into(),
                action: json!({"tool": "crew.nudge", "input": {"agent": "scout"}}),
                state: SuggestionState::Open,
                subject: Some(scout.clone()),
                agent_id: "desk".into(),
            };
            let (keep, drop, refuse) = (made("Ask Scout"), made("Leave it"), made("impossible"));
            for s in [&keep, &drop, &refuse] {
                d.store().put_suggestion(s).await.unwrap();
            }
            let (_, list) = call(&app, "GET", "/api/desk/suggestions").await;
            assert_eq!(list["suggestions"].as_array().unwrap().len(), 3);
            assert_eq!(list["suggestions"][0]["state"], "open");
            assert_eq!(list["suggestions"][0]["subject"]["id"], "scout");

            let (status, done) = call(
                &app,
                "POST",
                &format!("/api/desk/suggestions/{}/accept", keep.id),
            )
            .await;
            assert_eq!(
                (status, done["state"].as_str()),
                (StatusCode::OK, Some("done"))
            );
            assert_eq!(*hub.0.lock(), vec![keep.id.clone()]);
            let (status, _) = call(
                &app,
                "POST",
                &format!("/api/desk/suggestions/{}/accept", keep.id),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            let (status, body) = call(
                &app,
                "POST",
                &format!("/api/desk/suggestions/{}/accept", refuse.id),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"], "That session isn’t running any more.");

            let (status, _) = call(
                &app,
                "POST",
                &format!("/api/desk/suggestions/{}/dismiss", drop.id),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            settle(&d, 3).await;
            let (_, page) = call(&app, "GET", "/api/desk/activity").await;
            let dismissed = &page["events"][0];
            assert_eq!(
                (
                    dismissed["kind"].as_str(),
                    dismissed["actor"]["kind"].as_str()
                ),
                (Some("dismissed"), Some("you"))
            );
            assert_eq!(dismissed["suggestionId"], drop.id.as_str());
            assert_eq!(dismissed["text"], "dismissed the suggestion about Scout");
            let (status, _) = call(&app, "POST", "/api/desk/suggestions/nope/dismiss").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }
}

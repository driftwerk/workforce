//! `/api/desk/activity`, `/overview`, `/manager` and `/suggestions`: the
//! crew as Floor and the overseer's DM read it, and the person's answers to
//! the overseer's suggestions; `/decisions/{id}/undo`, the person undoing
//! one of its decisions; `/items/{id}/answer` and `/drop`, the person
//! settling what the boss asked them; `/dm/{agent}/typing` and
//! `/presence`, the person typing to a boss and whether its DM is on
//! screen. `/unread` and `/reads`: what is unread on this
//! Desk, and how far the person has read each place.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::Authorize;
use crate::crew::{
    By, Crew, Filter, GroupBoss, ItemRefused, Kind, ManagerView, SuggestionState, UndoRefused,
};
use crate::reads::{AllScope, Read, ReadPut};
use crate::rooms::YOU;
use crate::service::{Desk, DeskError};

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
        "Desk could not read the crew",
    )
}

/// Who a request is from, when it may read the crew.
async fn guard(
    s: &CrewApi,
    h: &HeaderMap,
    token: Option<&str>,
) -> Result<Option<String>, Response> {
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
    let person = match guard(&s, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let before = q.before.as_deref().filter(|b| !b.trim().is_empty());
    let page = match q.filter {
        Filter::NeedsYou => {
            s.desk
                .needs_you(&*s.crew, person.as_deref(), before, limit)
                .await
        }
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
    let person = match guard(&s, &h, q.token.as_deref()).await {
        Ok(person) => person,
        Err(r) => return r,
    };
    match s.desk.overview(&*s.crew, person.as_deref()).await {
        Ok(o) => Json(o).into_response(),
        Err(e) => failed(e),
    }
}

async fn manager(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let o = s.crew.overseer();
    let bosses = s
        .desk
        .bosses()
        .into_iter()
        .filter_map(|b| {
            Some(GroupBoss {
                group: b.scope.group()?.to_string(),
                agent_id: b.agent,
                name: b.name,
                level: b.level,
            })
        })
        .collect();
    Json(ManagerView {
        agent_id: o.as_ref().map(|o| o.id.clone()),
        level: o.map(|o| o.level),
        bosses,
        sealed: s.desk.sealed(),
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
    // An overseer's own limits are the holder's to change, as plans and
    // breakers are: nobody else's accept (or dismiss) of that card counts.
    if crate::crew::holder_only(&suggestion) {
        if let Err(status) = s.auth.authorize_holder(&h, q.token.as_deref()).await {
            return error(status, "not allowed");
        }
    }
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

/// Undoes one of the boss's decisions, as the person.
async fn undo(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let Ok(id) = id.parse::<uuid::Uuid>() else {
        return error(StatusCode::NOT_FOUND, "That decision isn’t there.");
    };
    match s.crew.undo_decision(id).await {
        Ok(done) => Json(json!({ "done": done })).into_response(),
        Err(UndoRefused::NotFound) => error(StatusCode::NOT_FOUND, "That decision isn’t there."),
        Err(UndoRefused::Already) => {
            error(StatusCode::CONFLICT, "That decision was already undone.")
        }
        Err(UndoRefused::Cant(why)) => error(StatusCode::BAD_REQUEST, why),
    }
}

#[derive(Deserialize)]
struct ItemAnswer {
    answer: String,
}

fn item_result(r: Result<String, ItemRefused>) -> Response {
    match r {
        Ok(done) => Json(json!({ "done": done })).into_response(),
        Err(ItemRefused::NotFound) => error(StatusCode::NOT_FOUND, "That isn’t open any more."),
        Err(ItemRefused::Closed) => error(StatusCode::CONFLICT, "That was already settled."),
        Err(ItemRefused::Cant(why)) => error(StatusCode::BAD_REQUEST, why),
    }
}

/// The person answers one of the boss's open items from its buttons: it is
/// closed in this request, with no model involved.
async fn item_answer(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<String>,
    Json(body): Json<ItemAnswer>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let Ok(id) = id.parse::<uuid::Uuid>() else {
        return error(StatusCode::NOT_FOUND, "That isn’t open any more.");
    };
    if body.answer.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "Say your answer.");
    }
    item_result(s.crew.answer_item(id, body.answer.trim()).await)
}

/// The person drops one of the boss's open items unanswered.
async fn item_drop(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let Ok(id) = id.parse::<uuid::Uuid>() else {
        return error(StatusCode::NOT_FOUND, "That isn’t open any more.");
    };
    item_result(s.crew.drop_item(id).await)
}

/// The person is typing in the boss `agent`'s DM: Desk sends it every few
/// seconds while the composer has keystrokes, and the boss's posts wait
/// for a pause.
async fn dm_typing(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(agent): Path<String>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    match s.crew.dm_typing(&agent) {
        true => StatusCode::NO_CONTENT.into_response(),
        false => error(StatusCode::NOT_FOUND, "There is no boss by that name here."),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceBody {
    attending: bool,
    #[serde(default)]
    dm_on_screen: Option<String>,
}

/// Whether Desk's window is attended and which boss's DM is on screen:
/// sent on focus and view changes, and each minute.
async fn presence(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(b): Json<PresenceBody>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    let dm = b
        .dm_on_screen
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    s.crew.presence(b.attending, dm);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct Puts {
    puts: Vec<ReadPut>,
}

#[derive(Deserialize)]
struct All {
    scope: AllScope,
}

fn reads(result: Result<Vec<Read>, DeskError>) -> Response {
    match result {
        Ok(reads) => Json(json!({ "reads": reads })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn unread(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    s.desk.unread(&*s.crew, YOU).await.map(Json).into_response()
}

async fn read_points(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    reads(s.desk.reads(YOU).await)
}

async fn put_reads(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<Puts>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    reads(s.desk.put_reads(YOU, &body.puts).await)
}

async fn all_read(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<All>,
) -> Response {
    if let Err(r) = guard(&s, &h, q.token.as_deref()).await {
        return r;
    }
    reads(s.desk.mark_all(&*s.crew, YOU, body.scope).await)
}

pub(crate) fn crew_router(desk: Desk, auth: Arc<dyn Authorize>, crew: Arc<dyn Crew>) -> Router {
    Router::new()
        .route("/api/desk/activity", get(activity))
        .route("/api/desk/overview", get(overview))
        .route("/api/desk/manager", get(manager))
        .route("/api/desk/suggestions", get(suggestions))
        .route("/api/desk/suggestions/{id}/{answer}", post(answer))
        .route("/api/desk/decisions/{id}/undo", post(undo))
        .route("/api/desk/items/{id}/answer", post(item_answer))
        .route("/api/desk/items/{id}/drop", post(item_drop))
        .route("/api/desk/dm/{agent}/typing", post(dm_typing))
        .route("/api/desk/presence", post(presence))
        .route("/api/desk/unread", get(unread))
        .route("/api/desk/reads", get(read_points).put(put_reads))
        .route("/api/desk/reads/all", post(all_read))
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
        async fn undo_decision(&self, id: uuid::Uuid) -> Result<String, UndoRefused> {
            match id.as_u128() {
                1 => Ok("let acme-web go".into()),
                2 => Err(UndoRefused::Already),
                3 => Err(UndoRefused::Cant("What ran is fixed forward.".into())),
                _ => Err(UndoRefused::NotFound),
            }
        }
        async fn answer_item(&self, id: uuid::Uuid, answer: &str) -> Result<String, ItemRefused> {
            match id.as_u128() {
                1 => Ok(format!("Sent: {answer}")),
                2 => Err(ItemRefused::Closed),
                3 => Err(ItemRefused::Cant("acme-web went ahead without you.".into())),
                _ => Err(ItemRefused::NotFound),
            }
        }
        async fn drop_item(&self, id: uuid::Uuid) -> Result<String, ItemRefused> {
            match id.as_u128() {
                1 => Ok("Dropped.".into()),
                _ => Err(ItemRefused::NotFound),
            }
        }
        fn dm_typing(&self, agent: &str) -> bool {
            self.0.lock().push(format!("typing {agent}"));
            agent == "desk"
        }
        fn presence(&self, attending: bool, dm: Option<&str>) {
            self.0.lock().push(format!("presence {attending} {dm:?}"));
        }
    }

    #[tokio::test]
    async fn the_undo_route_says_each_outcome_by_its_status() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let app = crew_router(d, Arc::new(Bearer), Arc::new(Hub::default()));
            let uri = |n: u128| format!("/api/desk/decisions/{}/undo", uuid::Uuid::from_u128(n));
            let (status, body) = call(&app, "POST", &uri(1)).await;
            assert_eq!(
                (status, body["done"].as_str()),
                (StatusCode::OK, Some("let acme-web go"))
            );
            assert_eq!(call(&app, "POST", &uri(2)).await.0, StatusCode::CONFLICT);
            let (status, body) = call(&app, "POST", &uri(3)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"], "What ran is fixed forward.");
            assert_eq!(call(&app, "POST", &uri(4)).await.0, StatusCode::NOT_FOUND);
            assert_eq!(
                call(&app, "POST", "/api/desk/decisions/nope/undo").await.0,
                StatusCode::NOT_FOUND
            );
        })
        .await;
    }

    #[tokio::test]
    async fn the_item_routes_say_each_outcome_by_its_status() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let app = crew_router(d, Arc::new(Bearer), Arc::new(Hub::default()));
            let answer = |n: u128| format!("/api/desk/items/{}/answer", uuid::Uuid::from_u128(n));
            let body = json!({ "answer": " Wait for green " });
            let (status, said) = call_json(&app, &answer(1), &body).await;
            assert_eq!(
                (status, said["done"].as_str()),
                (StatusCode::OK, Some("Sent: Wait for green"))
            );
            assert_eq!(
                call_json(&app, &answer(2), &body).await.0,
                StatusCode::CONFLICT
            );
            let (status, said) = call_json(&app, &answer(3), &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(said["error"], "acme-web went ahead without you.");
            assert_eq!(
                call_json(&app, &answer(4), &body).await.0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                call_json(&app, &answer(1), &json!({ "answer": "  " }))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
            let drop = |n: u128| format!("/api/desk/items/{}/drop", uuid::Uuid::from_u128(n));
            assert_eq!(call(&app, "POST", &drop(1)).await.0, StatusCode::OK);
            assert_eq!(call(&app, "POST", &drop(9)).await.0, StatusCode::NOT_FOUND);
            assert_eq!(
                call(&app, "POST", "/api/desk/items/nope/drop").await.0,
                StatusCode::NOT_FOUND
            );
        })
        .await;
    }

    #[tokio::test]
    async fn typing_and_presence_reach_the_boss() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let hub = Arc::new(Hub::default());
            let app = crew_router(d, Arc::new(Bearer), hub.clone());
            assert_eq!(
                call(&app, "POST", "/api/desk/dm/desk/typing").await.0,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                call(&app, "POST", "/api/desk/dm/nobody/typing").await.0,
                StatusCode::NOT_FOUND
            );
            let on = json!({ "attending": true, "dmOnScreen": "desk" });
            assert_eq!(
                call_json(&app, "/api/desk/presence", &on).await.0,
                StatusCode::NO_CONTENT
            );
            let off = json!({ "attending": false, "dmOnScreen": " " });
            assert_eq!(
                call_json(&app, "/api/desk/presence", &off).await.0,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                *hub.0.lock(),
                vec![
                    "typing desk".to_string(),
                    "typing nobody".to_string(),
                    "presence true Some(\"desk\")".to_string(),
                    "presence false None".to_string(),
                ]
            );
        })
        .await;
    }

    async fn call_json(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("authorization", "Bearer hub-token")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
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
            assert_eq!(
                m,
                json!({"agentId": "desk", "level": "suggest", "bosses": [], "sealed": []})
            );

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

    /// Signed in, but not the person who holds the hub.
    struct NotHolder;

    #[async_trait::async_trait]
    impl Authorize for NotHolder {
        async fn authorize(
            &self,
            h: &HeaderMap,
            q: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Bearer
                .authorize(h, q)
                .await
                .map(|_| Some("someone-else".into()))
        }
        async fn authorize_holder(
            &self,
            _: &HeaderMap,
            _: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Err(StatusCode::FORBIDDEN)
        }
    }

    /// An overseer's own limits are the holder's alone to change: anyone
    /// else's accept or dismiss of that card is refused and changes
    /// nothing; their answers to other suggestions stand as before.
    #[tokio::test]
    async fn only_the_holder_answers_a_card_that_changes_an_overseers_own_limits() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let hub = Arc::new(Hub::default());
            let app = crew_router(d.clone(), Arc::new(NotHolder), hub.clone());
            let card = |tool: &str| Suggestion {
                id: crate::crew::new_id(),
                at: crate::crew::now(),
                text: "Set Act fully's daily budget to $5,000.00".into(),
                action: json!({"tool": tool, "input": {"agent_id": "desk", "budget_usd_per_day": 5000}}),
                state: SuggestionState::Open,
                subject: None,
                agent_id: "desk".into(),
            };
            let own = card(crate::crew::OWN_OVERSIGHT_TOOL);
            let other = card("crew.nudge");
            for s in [&own, &other] {
                d.store().put_suggestion(s).await.unwrap();
            }
            for answer in ["accept", "dismiss"] {
                let (status, _) = call(
                    &app,
                    "POST",
                    &format!("/api/desk/suggestions/{}/{answer}", own.id),
                )
                .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
            }
            assert!(hub.0.lock().is_empty(), "nothing was done");
            let kept = d.store().suggestion(&own.id).await.unwrap().unwrap();
            assert_eq!(kept.state, SuggestionState::Open);
            let (status, _) = call(
                &app,
                "POST",
                &format!("/api/desk/suggestions/{}/accept", other.id),
            )
            .await;
            assert_eq!(status, StatusCode::OK);

            // The holder accepts it.
            let holder = crew_router(d.clone(), Arc::new(Bearer), hub.clone());
            let (status, done) = call(
                &holder,
                "POST",
                &format!("/api/desk/suggestions/{}/accept", own.id),
            )
            .await;
            assert_eq!((status, done["state"].as_str()), (StatusCode::OK, Some("done")));
        })
        .await;
    }

    #[tokio::test]
    async fn the_manager_route_says_each_groups_boss_and_the_main_overseer_apart() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let app = crew_router(d.clone(), Arc::new(Bearer), Arc::new(Hub::default()));
            d.set_bosses(vec![
                crate::Boss {
                    agent: "desk".into(),
                    name: "Desk".into(),
                    scope: crate::scope::Scope::Ungrouped,
                    level: "suggest".into(),
                },
                crate::Boss {
                    agent: "sarah".into(),
                    name: "Sarah".into(),
                    scope: crate::scope::Scope::Group("g-acme".into()),
                    level: "act".into(),
                },
            ]);
            // Globex is named for a boss that oversees nothing now: sealed.
            d.change_layout(|l| {
                l.create("g-acme", "Acme", &[], None)?
                    .create("g-globex", "Globex", &[], None)
            })
            .await
            .unwrap();
            d.bound_groups(["g-acme".to_string(), "g-globex".to_string()])
                .await;
            let (status, m) = call(&app, "GET", "/api/desk/manager").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(m["agentId"], "desk");
            assert_eq!(m["sealed"], json!(["g-globex"]));
            assert_eq!(
                m["bosses"],
                json!([{"agentId": "sarah", "name": "Sarah", "group": "g-acme", "level": "act"}])
            );
        })
        .await;
    }

    async fn send(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", "Bearer hub-token")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
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

    #[tokio::test]
    async fn the_summary_route_answers_each_room_and_reads_move_through_their_routes() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let hub = Arc::new(Hub::default());
            let app = crew_router(d.clone(), Arc::new(Bearer), hub.clone());
            let room = d
                .create_room(crate::rooms::NewRoom {
                    name: "Atlas launch".into(),
                    members: vec![],
                    default_member: None,
                    ..Default::default()
                })
                .await
                .unwrap();
            let (status, summary) = call(&app, "GET", "/api/desk/unread").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(summary["rooms"][0]["roomId"], json!(room.id));
            assert_eq!(summary["rooms"][0]["main"]["count"], json!(0));
            assert_eq!(summary["activity"], json!({ "count": 0, "lastAt": null }));
            assert!(summary["floor"].is_string());

            let place = format!("room:{}", room.id);
            let put = json!({ "puts": [{ "place": place, "point": { "at": "2026-10-30T10:00:00Z", "message": null }, "mark": "advance" }] });
            let (status, changed) = send(&app, "PUT", "/api/desk/reads", put.clone()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(changed, json!({ "reads": [{ "place": place, "at": "2026-10-30T10:00:00Z", "message": null }] }));
            let (_, again) = send(&app, "PUT", "/api/desk/reads", put).await;
            assert_eq!(again, json!({ "reads": [] }), "no point moved");
            let (_, reads) = call(&app, "GET", "/api/desk/reads").await;
            assert!(reads["reads"].as_array().unwrap().iter().any(|r| r["place"] == json!(place)));
            let (status, _) = send(&app, "PUT", "/api/desk/reads", json!({ "puts": [{ "place": "floor", "point": { "at": "2026-10-30T10:00:00Z" }, "mark": "set" }] })).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "the floor is Desk's own");
            let (status, all) = send(&app, "POST", "/api/desk/reads/all", json!({ "scope": { "room": room.id } })).await;
            assert_eq!((status, all), (StatusCode::OK, json!({ "reads": [] })));
            let (status, _) = send(&app, "POST", "/api/desk/reads/all", json!({ "scope": "everything" })).await;
            assert_eq!(status, StatusCode::OK);
        })
        .await;
    }
}

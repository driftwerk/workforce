//! `/api/desk/*` for the Desk app (REST + two websockets), and the loopback
//! endpoint Claude Code's hooks post to.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::crew::By;
use crate::model::Engine as CliEngine;
use crate::model::{Decision, NewSession, SessionPatch};
use crate::providers::{self, AccountError, Accounts, Catalog, ConnectBody, Entry};
use crate::pty::Frame;
use crate::realtime::{Engine, Realtime, RealtimeError};
use crate::rooms::{Bound, NewMessage, NewRoom, Page, RoomPatch, SearchFilters};
use crate::service::{
    Clearing, Desk, DeskError, Import, Layout, NewTerminal, Removal, TerminalPatch,
};
use crate::speech::{Speak, SpeakError, Speech};

/// Decides whether a request may use the desk, and says who it is from: the
/// person its token names, none for one that names no one. Implemented by
/// the hub.
#[async_trait]
pub trait Authorize: Send + Sync {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<Option<String>, StatusCode>;

    /// Whether the request's bearer is the hub's own gateway token itself,
    /// compared in constant time: what an account's sign-in is handed to,
    /// and nothing else (not a system administrator's sign-in).
    fn is_hub_token(&self, headers: &HeaderMap) -> bool {
        let _ = headers;
        false
    }
}

struct ApiState {
    desk: Desk,
    auth: Arc<dyn Authorize>,
    /// When each account's sign-in was last asked for by a hand-over.
    handed: parking_lot::Mutex<std::collections::HashMap<Uuid, std::time::Instant>>,
}

/// One hand-over of an account's sign-in per this long.
const HAND_OVER_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

impl IntoResponse for DeskError {
    fn into_response(self) -> Response {
        let status = match &self {
            DeskError::NotFound => StatusCode::NOT_FOUND,
            DeskError::Conflict(_) => StatusCode::CONFLICT,
            DeskError::Invalid(_) => StatusCode::BAD_REQUEST,
            DeskError::Internal(e) => {
                tracing::error!(error = format!("{e:#}"), "desk request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

async fn guard(auth: &dyn Authorize, headers: &HeaderMap, q: &TokenQuery) -> Result<(), Response> {
    auth.authorize(headers, q.token.as_deref())
        .await
        .map(drop)
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

macro_rules! guarded {
    ($s:expr, $h:expr, $q:expr) => {
        if let Err(r) = guard(&*$s.auth, &$h, &$q).await {
            return r;
        }
    };
}

type S = State<Arc<ApiState>>;
type Q = Query<TokenQuery>;

async fn list(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.list()).into_response()
}

/// Which spend a route asks for: a period (`day`, `week`, `month`,
/// `five_hours`, in the person's timezone) or `from`/`to`, grouped `by`.
#[derive(Debug, Deserialize)]
struct SpendParams {
    period: Option<String>,
    from: Option<chrono::DateTime<chrono::Utc>>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    by: Option<String>,
    limit: Option<usize>,
    session: Option<Uuid>,
    engine: Option<String>,
    account: Option<String>,
    /// The ledger's page to go on from: a page's `next`.
    before: Option<String>,
}

/// Most rows a spend view returns.
const MAX_SPEND_ROWS: usize = 500;

impl SpendParams {
    fn window(
        &self,
        desk: &Desk,
    ) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), DeskError> {
        let now = chrono::Utc::now();
        let to = self.to.unwrap_or(now);
        let from = match (&self.period, self.from) {
            (Some(p), _) => {
                let period = crate::ledger::Period::parse(p).map_err(|_| {
                    DeskError::Invalid("period is day, week, month or five_hours".into())
                })?;
                desk.period_start(period, now)
            }
            (None, Some(from)) => from,
            (None, None) => desk.period_start(crate::ledger::Period::Day, now),
        };
        if from > to {
            return Err(DeskError::Invalid("from is after to".into()));
        }
        Ok((from, to))
    }

    fn filter(&self) -> Result<crate::ledger_store::Filter, DeskError> {
        Ok(crate::ledger_store::Filter {
            engine: self
                .engine
                .as_deref()
                .map(crate::engine::Engine::parse)
                .transpose()
                .map_err(|_| DeskError::Invalid("that isn't an engine".into()))?,
            account: self.account.clone(),
            session: self.session,
            seen: None,
        })
    }
}

fn spend_params(p: Result<Query<SpendParams>, QueryRejection>) -> Result<SpendParams, DeskError> {
    p.map(|Query(p)| p)
        .map_err(|e| DeskError::Invalid(e.body_text()))
}

/// The budgets, where each stands, the breakers and their rules.
async fn budgets(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    s.desk.budgets_view().await.map(Json).into_response()
}

async fn put_budget(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<crate::service::NewBudget>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .put_budget(b, By::you())
        .await
        .map(Json)
        .into_response()
}

async fn patch_budget(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<crate::service::BudgetPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .patch_budget(id, p, By::you())
        .await
        .map(Json)
        .into_response()
}

async fn delete_budget(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .delete_budget(id, By::you())
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn breaker_rules(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(r): Json<crate::service::Rules>,
) -> Response {
    guarded!(s, h, q);
    s.desk.set_breaker_rules(r).await.map(Json).into_response()
}

#[derive(Deserialize)]
struct BreakerBody {
    subject: String,
}

/// The person lets what a breaker holds go on.
async fn breaker_continue(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<BreakerBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .breaker_continue(&b.subject, By::you())
        .map(Json)
        .into_response()
}

/// The person leaves what a breaker holds paused.
async fn breaker_keep(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<BreakerBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk.breaker_keep(&b.subject).map(Json).into_response()
}

/// What sessions spent, summed and grouped.
async fn spend(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    p: Result<Query<SpendParams>, QueryRejection>,
) -> Response {
    guarded!(s, h, q);
    let run = async {
        let p = spend_params(p)?;
        let (from, to) = p.window(&s.desk)?;
        let by =
            crate::ledger::SpendBy::parse(p.by.as_deref().unwrap_or("session")).map_err(|_| {
                DeskError::Invalid("by is session, project, day, engine, account or group".into())
            })?;
        s.desk
            .spend(&crate::service::SpendQuery {
                by,
                from,
                to,
                limit: p.limit.unwrap_or(50).clamp(1, MAX_SPEND_ROWS),
                filter: p.filter()?,
            })
            .await
    };
    run.await.map(Json).into_response()
}

/// The ledger's calls themselves, newest first, a page at a time.
async fn ledger(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    p: Result<Query<SpendParams>, QueryRejection>,
) -> Response {
    guarded!(s, h, q);
    let run = async {
        let p = spend_params(p)?;
        let (from, to) = match (p.period.is_some(), p.from) {
            (false, None) => (
                chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
                p.to.unwrap_or_else(chrono::Utc::now),
            ),
            _ => p.window(&s.desk)?,
        };
        s.desk
            .ledger_page(from, to, &p.filter()?, p.before.as_deref())
            .await
    };
    run.await.map(Json).into_response()
}

/// Every engine: its program, version and sign-in, and where Desk runs it.
async fn engines(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.engines()).into_response()
}

#[derive(Deserialize)]
struct EngineBody {
    engine: crate::engine::Engine,
}

/// Signs an engine's own CLI in where it has no browser (Codex's device
/// sign-in): the page and the code to type there, once it shows them.
async fn start_engine_sign_in(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<EngineBody>,
) -> Response {
    guarded!(s, h, q);
    match s.desk.start_engine_sign_in(b.engine) {
        Ok(view) => (StatusCode::ACCEPTED, Json(view)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn engine_sign_in(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.engine_sign_in(id).map(Json).into_response()
}

async fn cancel_engine_sign_in(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .cancel_engine_sign_in(id)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

/// The person's engine accounts, the store their sign-ins are kept in
/// (none: this Desk keeps none), and any sign-in under way.
async fn accounts(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(json!({
        "accounts": s.desk.accounts(),
        "store": s.desk.accounts_store(),
        "signIns": s.desk.account_sign_ins(),
        "needs": s.desk.account_needs(),
        "autoSwitch": s.desk.auto_switch(),
        "problems": s.desk.sign_in_problems(),
        "ownPlanKey": s.desk.own_plan_key(),
    }))
    .into_response()
}

/// Adds an account: with a pasted token, kept once Claude Code signs in by
/// it; without, signing in through `claude setup-token` begins.
async fn add_account(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(new): Json<crate::engine_accounts::NewAccount>,
) -> Response {
    guarded!(s, h, q);
    match s.desk.add_account(new).await {
        Ok(added @ crate::service::Added::Account { .. }) => {
            (StatusCode::CREATED, Json(added)).into_response()
        }
        Ok(added @ crate::service::Added::SignIn { .. }) => {
            (StatusCode::ACCEPTED, Json(added)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn update_account(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<crate::engine_accounts::AccountPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update_account(id, p).await.map(Json).into_response()
}

async fn remove_account(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .remove_account(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

/// An account's sign-in, for Desk's native side to copy to one of the
/// person's machines over its SSH connection: the hub's own (gateway)
/// token only, never a signed-in person's, an administrator's included;
/// from the `Authorization` header alone (never a token in the address,
/// which logs keep); once a minute per account; and never kept by anything
/// between (`no-store`). Desk's page can't ask it: it isn't on the broker's
/// list.
async fn hand_over_account(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
) -> Response {
    if q.token.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "a sign-in is handed over only to a token in the Authorization header"})),
        )
            .into_response();
    }
    if !s.auth.is_hub_token(&h) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "a sign-in is handed over only to the hub's own token"})),
        )
            .into_response();
    }
    {
        let now = std::time::Instant::now();
        let mut handed = s.handed.lock();
        handed.retain(|_, at| now.duration_since(*at) < HAND_OVER_EVERY);
        if handed.contains_key(&id) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "that sign-in was handed over a moment ago: try again in a minute"})),
            )
                .into_response();
        }
        handed.insert(id, now);
    }
    match s.desk.hand_over_account(id).await {
        Ok(handed) => (
            [
                (axum::http::header::CACHE_CONTROL, "no-store"),
                (axum::http::header::PRAGMA, "no-cache"),
            ],
            Json(handed),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// "OK" on a sign-in problem the person has dealt with.
async fn dismiss_problem(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.dismiss_sign_in_problem(id);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchAllBody {
    #[serde(default = "claude")]
    engine: crate::engine::Engine,
    /// The account they're on; `null`, the CLI's own sign-in.
    from: Option<Uuid>,
    to: Option<Uuid>,
}

fn claude() -> crate::engine::Engine {
    crate::engine::Engine::Claude
}

/// Moves every session on one account to another: "Switch all".
async fn switch_all(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<SwitchAllBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .switch_all(b.engine, b.from, b.to)
        .await
        .map(Json)
        .into_response()
}

/// "Not now" on an account need.
async fn put_off_need(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<String>) -> Response {
    guarded!(s, h, q);
    s.desk.put_off(&id);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StandingBody {
    accounts: Vec<crate::engine_accounts::StandingElsewhere>,
    /// The sender's clock as it sent this: what its times are shifted by.
    #[serde(default)]
    sent_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// What the person's other computers know of their accounts' plans, passed
/// on by Desk.
async fn learn_standing(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<StandingBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk.learn_standing(b.accounts, b.sent_at).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct OwnSignInBody {
    /// The account this computer's Claude Code is signed in to; none: none
    /// of the person's accounts.
    account: Option<Uuid>,
}

/// "This computer's sign-in is …": which of the person's accounts it is.
async fn own_sign_in(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<OwnSignInBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .link_own_sign_in(b.account)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
struct OnBody {
    on: bool,
}

/// "Switch automatically when an account hits its limit".
async fn auto_switch(State(s): S, h: HeaderMap, Query(q): Q, Json(b): Json<OnBody>) -> Response {
    guarded!(s, h, q);
    s.desk
        .set_auto_switch(b.on)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

/// The code the sign-in page showed, typed into the sign-in waiting for it.
async fn sign_in_code(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<CodeBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk.sign_in_code(id, &b.code).map(Json).into_response()
}

/// The page that comes back by itself for a sign-in made from another
/// computer, for the Desk that forwards its port.
async fn sign_in_direct(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.sign_in_direct(id).map(Json).into_response()
}

async fn cancel_sign_in(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .cancel_sign_in(id)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
struct SwitchBody {
    /// The account; `null`, the CLI's own sign-in.
    account: Option<Uuid>,
}

/// Moves a session to another account: now when it is idle, once its turn
/// ends when it works.
async fn switch_account(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<SwitchBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .switch_account(id, b.account, Some(By::you()))
        .await
        .map(Json)
        .into_response()
}

async fn create(State(s): S, h: HeaderMap, Query(q): Q, Json(new): Json<NewSession>) -> Response {
    guarded!(s, h, q);
    s.desk.create_chosen(new).await.map(Json).into_response()
}

async fn update(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<SessionPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update(id, p).map(Json).into_response()
}

/// Removes a session from Desk, its engine's transcript kept.
async fn forget(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .remove(id, Removal::default())
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .into_response()
}

/// Removes a session from Desk, and its engine's transcript when asked.
async fn remove(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    body: Result<Json<Removal>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let how = match body {
        Ok(Json(how)) => how,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    s.desk.remove(id, how).await.map(Json).into_response()
}

async fn end(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .end(id, Some(By::you()))
        .await
        .map(Json)
        .into_response()
}

async fn closed(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    s.desk.closed_sessions().await.map(Json).into_response()
}

async fn hidden(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    s.desk.hidden_sessions().await.map(Json).into_response()
}

/// Starts tracking a removed CLI session again.
async fn unhide(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((engine, cli)): Path<(String, String)>,
) -> Response {
    guarded!(s, h, q);
    let engine = match CliEngine::parse(&engine) {
        Ok(e) => e,
        Err(e) => return DeskError::Invalid(e.to_string()).into_response(),
    };
    s.desk.unhide(engine, &cli);
    StatusCode::NO_CONTENT.into_response()
}

async fn clear(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    body: Result<Json<Clearing>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let c = match body {
        Ok(Json(c)) => c,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    s.desk.clear_finished(c).await.map(Json).into_response()
}

async fn stop(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.stop(id, Some(By::you())).map(Json).into_response()
}

async fn interrupt(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .interrupt(id, Some(By::you()))
        .map(Json)
        .into_response()
}

async fn allow_asked(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .allow_asked(id, Some(By::you()))
        .map(Json)
        .into_response()
}

async fn dismiss_asked(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .dismiss_asked(id, Some(By::you()))
        .map(Json)
        .into_response()
}

#[derive(Deserialize, Default)]
struct ResumeBody {
    prompt: Option<String>,
}

async fn resume(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    body: Option<Json<ResumeBody>>,
) -> Response {
    guarded!(s, h, q);
    let prompt = body
        .and_then(|Json(b)| b.prompt)
        .filter(|p| !p.trim().is_empty());
    s.desk
        .resume(id, prompt, Some(By::you()))
        .await
        .map(Json)
        .into_response()
}

/// Stops it and resumes its conversation: what a CLI that read the
/// terminal's colours only at its start needs to take new ones.
async fn restart(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .restart(id, None, By::you())
        .await
        .map(Json)
        .into_response()
}

/// The colours Desk draws its terminals in ([`crate::vt::Colors`]).
async fn terminal_colors(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(colors): Json<crate::vt::Colors>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .set_terminal_colors(colors)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
struct DecideBody {
    decision: Decision,
}

async fn decide(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, approval)): Path<(Uuid, Uuid)>,
    Json(b): Json<DecideBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .decide(id, approval, b.decision, Some(By::you()))
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct RestoreBody {
    ids: Vec<Uuid>,
}

async fn restore(State(s): S, h: HeaderMap, Query(q): Q, Json(b): Json<RestoreBody>) -> Response {
    guarded!(s, h, q);
    Json(s.desk.restore(&b.ids, Some(By::you())).await).into_response()
}

async fn dismiss_restore(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    s.desk.dismiss_restore();
    StatusCode::NO_CONTENT.into_response()
}

async fn events(State(s): S, h: HeaderMap, Query(q): Q, ws: WebSocketUpgrade) -> Response {
    guarded!(s, h, q);
    let desk = s.desk.clone();
    ws.on_upgrade(move |socket| stream_events(socket, desk))
}

async fn stream_events(socket: WebSocket, desk: Desk) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut changes) = desk.subscribe();
    let send = |e: &crate::model::DeskEvent| {
        Message::text(serde_json::to_string(e).unwrap_or_else(|_| "{}".into()))
    };
    if tx.send(send(&snapshot)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            change = changes.recv() => match change {
                Ok(e) => if tx.send(send(&e)).await.is_err() { return },
                // Fell behind: start again from a fresh snapshot.
                Err(RecvError::Lagged(_)) => {
                    let (snapshot, fresh) = desk.subscribe();
                    changes = fresh;
                    if tx.send(send(&snapshot)).await.is_err() { return }
                }
                Err(RecvError::Closed) => return,
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn pty(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Response {
    guarded!(s, h, q);
    match s.desk.attach(id) {
        Ok(pty) => ws.on_upgrade(move |socket| stream_pty(socket, pty, "session", id)),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
    Claim,
}

/// A frame as the viewer `me` is sent it: output and snapshots as binary,
/// the terminal's size and this viewer's role as text.
fn pty_message(me: u64, frame: Frame) -> Option<Message> {
    match frame {
        Frame::Bytes(bytes) => Some(Message::Binary(bytes)),
        Frame::Size { cols, rows } => Some(Message::text(
            json!({ "type": "size", "cols": cols, "rows": rows }).to_string(),
        )),
        Frame::Active(active) => Some(Message::text(
            json!({ "type": "role", "active": active == Some(me), "free": active.is_none() })
                .to_string(),
        )),
        Frame::Exit => None,
    }
}

/// Logs, when dropped, that a viewer left.
struct Detached {
    kind: &'static str,
    id: Uuid,
    me: u64,
}

impl Drop for Detached {
    fn drop(&mut self) {
        tracing::info!(kind = self.kind, id = %self.id, viewer = self.me, "terminal viewer detached");
    }
}

/// Binary frames carry terminal bytes both ways; text frames carry control
/// messages: `resize` and `claim` from the viewer, `size` and `role` to it.
/// Only the active viewer's keys and size reach the terminal; a viewer that
/// falls behind is sent the screen again rather than losing bytes.
async fn stream_pty(socket: WebSocket, pty: Arc<crate::pty::Pty>, kind: &'static str, id: Uuid) {
    let (mut tx, mut rx) = socket.split();
    let (viewer, frames, mut output) = pty.attach();
    let me = viewer.id();
    tracing::info!(kind, id = %id, viewer = me, "terminal viewer attached");
    let _detached = Detached { kind, id, me };
    for frame in frames {
        if let Some(m) = pty_message(me, frame) {
            if tx.send(m).await.is_err() {
                return;
            }
        }
    }
    loop {
        tokio::select! {
            out = output.recv() => match out {
                Ok(Frame::Exit) | Err(RecvError::Closed) => {
                    let _ = tx.send(Message::Close(None)).await;
                    return;
                }
                Ok(frame) => {
                    if let Some(m) = pty_message(me, frame) {
                        if tx.send(m).await.is_err() { return }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let (frames, fresh) = viewer.resync();
                    output = fresh;
                    for frame in frames {
                        if let Some(m) = pty_message(me, frame) {
                            if tx.send(m).await.is_err() { return }
                        }
                    }
                }
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Err(e) = viewer.input(&bytes) {
                        tracing::debug!(error = %e, "terminal input dropped");
                        return;
                    }
                }
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Control>(&text) {
                    Ok(Control::Resize { cols, rows }) => {
                        if let Err(e) = viewer.resize(cols, rows) {
                            tracing::debug!(error = %e, "terminal resize failed");
                        }
                    }
                    Ok(Control::Claim) => {
                        tracing::info!(kind, id = %id, viewer = me, "terminal viewer claimed");
                        viewer.claim();
                    }
                    Err(e) => tracing::debug!(error = %e, "unknown terminal control message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[derive(Deserialize)]
struct FolderParams {
    token: Option<String>,
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

impl IntoResponse for crate::folders::FolderError {
    fn into_response(self) -> Response {
        use crate::folders::FolderError as E;
        let status = match self {
            E::Outside => StatusCode::FORBIDDEN,
            E::NotFound => StatusCode::NOT_FOUND,
            E::NotAFolder => StatusCode::BAD_REQUEST,
            E::NoHome => StatusCode::CONFLICT,
            E::Unreadable => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// The folders under the hub user's home or a live session's folder, for
/// choosing where a session runs.
async fn folders(State(s): S, h: HeaderMap, Query(q): Query<FolderParams>) -> Response {
    guarded!(s, h, q_token(&q));
    let (home, live) = (s.desk.home_folder(), s.desk.live_folders());
    let listing = tokio::task::spawn_blocking(move || {
        crate::folders::list(q.path.as_deref(), q.hidden, home.as_deref(), &live)
    })
    .await;
    match listing {
        Ok(Ok(l)) => Json(l).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => crate::folders::FolderError::Unreadable.into_response(),
    }
}

fn q_token(q: &FolderParams) -> TokenQuery {
    TokenQuery {
        token: q.token.clone(),
    }
}

async fn terminals(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.terminals()).into_response()
}

async fn create_terminal(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(new): Json<NewTerminal>,
) -> Response {
    guarded!(s, h, q);
    s.desk.create_terminal(new).await.map(Json).into_response()
}

async fn update_terminal(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<TerminalPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk.update_terminal(id, p).map(Json).into_response()
}

async fn close_terminal(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .close_terminal(id)
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

async fn reopen_terminal(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.reopen_terminal(id).await.map(Json).into_response()
}

async fn terminal_programs(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
) -> Response {
    guarded!(s, h, q);
    s.desk.terminal_programs(id).await.map(Json).into_response()
}

async fn terminal_pty(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Response {
    guarded!(s, h, q);
    match s.desk.attach_terminal(id) {
        Ok(pty) => ws.on_upgrade(move |socket| stream_pty(socket, pty, "terminal", id)),
        Err(e) => e.into_response(),
    }
}

async fn layout(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.layout()).into_response()
}

/// `PUT /api/desk/layout`: the whole layout, and, optionally, the layout it
/// was made from. When that's given and the hub's has changed since (an
/// agent grouped something, a session was placed or forgotten), nothing is
/// written and it answers 409, so a write made meanwhile is never reverted:
/// the caller reads the layout again and makes its change to that.
#[derive(Deserialize)]
struct ReplaceLayout {
    #[serde(flatten)]
    layout: Layout,
    #[serde(default)]
    expected: Option<Layout>,
}

async fn replace_layout(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Json(r): Json<ReplaceLayout>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|before| match &r.expected {
            Some(expected) if expected != before => Err(DeskError::Conflict(
                "The layout changed since it was read: read it again.".into(),
            )),
            _ => Ok(r.layout),
        })
        .await
        .map(Json)
        .into_response()
}

async fn import_layout(State(s): S, h: HeaderMap, Query(q): Q, Json(i): Json<Import>) -> Response {
    guarded!(s, h, q);
    s.desk.import_layout(i).await.map(Json).into_response()
}

#[derive(Deserialize)]
struct NewGroup {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    before: Option<String>,
}

async fn create_group(State(s): S, h: HeaderMap, Query(q): Q, Json(g): Json<NewGroup>) -> Response {
    guarded!(s, h, q);
    let id = g.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    s.desk
        .change_layout(|l| l.create(&id, &g.name, &g.items, g.before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct GroupPatch {
    name: Option<String>,
    collapsed: Option<bool>,
}

async fn update_group(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    Json(p): Json<GroupPatch>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.edit(&id, p.name.as_deref(), p.collapsed))
        .await
        .map(Json)
        .into_response()
}

async fn delete_group(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<String>) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.delete(&id))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize, Default)]
struct Before {
    #[serde(default)]
    before: Option<String>,
}

async fn move_group(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    body: Option<Json<Before>>,
) -> Response {
    guarded!(s, h, q);
    let before = body.map(|Json(b)| b.before).unwrap_or_default();
    s.desk
        .change_layout(|l| l.move_group(&id, before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

async fn put_member(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, item)): Path<(String, String)>,
    body: Option<Json<Before>>,
) -> Response {
    guarded!(s, h, q);
    let before = body.map(|Json(b)| b.before).unwrap_or_default();
    s.desk
        .change_layout(|l| l.put(&item, &id, before.as_deref()))
        .await
        .map(Json)
        .into_response()
}

async fn remove_member(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| match l.group_of(&item) {
            Some(g) if g.id == id => l.remove(&item),
            _ => Err(DeskError::NotFound),
        })
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct Order {
    items: Vec<String>,
}

/// Ungrouped's order as Desk shows it (`Layout::order_ungrouped`): the
/// route for putting an item in Ungrouped or taking it out of a group,
/// since only Desk knows what shows there unlisted.
async fn order_ungrouped(State(s): S, h: HeaderMap, Query(q): Q, Json(o): Json<Order>) -> Response {
    guarded!(s, h, q);
    s.desk
        .change_layout(|l| l.order_ungrouped(&o.items))
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
struct NameBody {
    name: Option<String>,
}

async fn name_session(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(b): Json<NameBody>,
) -> Response {
    guarded!(s, h, q);
    s.desk.name_session(id, b.name).map(Json).into_response()
}

async fn rooms(State(s): S, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(s.desk.rooms()).into_response()
}

async fn create_room(State(s): S, h: HeaderMap, Query(q): Q, Json(new): Json<NewRoom>) -> Response {
    guarded!(s, h, q);
    let asks = s.desk.room_asks(
        None,
        Some(new.group.as_deref()),
        &new.members,
        &new.share_with,
    );
    room_saved(s.desk.create_room(new).await, asks)
}

/// A room saved, or why not: a room of a group with a boss whose
/// conversation would go to a hub the person hasn't agreed to is a `409`
/// naming those hubs, for Desk to ask about once.
fn room_saved(saved: Result<crate::rooms::Room, DeskError>, asks: Vec<String>) -> Response {
    match saved {
        Err(DeskError::Conflict(why)) if !asks.is_empty() => (
            StatusCode::CONFLICT,
            Json(json!({ "error": why, "hubs": asks })),
        )
            .into_response(),
        other => other.map(Json).into_response(),
    }
}

async fn update_room(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(p): Json<RoomPatch>,
) -> Response {
    guarded!(s, h, q);
    let asks = match &p.members {
        Some(members) => s.desk.room_asks(
            Some(id),
            p.group.as_ref().map(|g| g.as_deref()),
            members,
            &p.share_with,
        ),
        None => Vec::new(),
    };
    room_saved(s.desk.update_room(id, p).await, asks)
}

async fn delete_room(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk
        .delete_room(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineQuery {
    token: Option<String>,
    thread: Option<Uuid>,
    before: Option<Bound>,
    after: Option<Bound>,
    around: Option<Uuid>,
    limit: Option<i64>,
}

async fn room_messages(
    State(s): S,
    h: HeaderMap,
    Query(t): Query<TimelineQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let q = TokenQuery { token: t.token };
    guarded!(s, h, q);
    let page = match Page::of(t.thread, t.before, t.after, t.around, t.limit) {
        Ok(page) => page,
        Err(why) => return DeskError::Invalid(why).into_response(),
    };
    s.desk
        .room_messages(id, page)
        .await
        .map(Json)
        .into_response()
}

async fn room_message(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, message)): Path<(Uuid, Uuid)>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .room_message(id, message)
        .await
        .map(Json)
        .into_response()
}

async fn copy_message(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path((id, message)): Path<(Uuid, Uuid)>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .copy_to_room(id, message)
        .await
        .map(|posted| {
            let status = match posted.created {
                true => StatusCode::CREATED,
                false => StatusCode::OK,
            };
            (status, Json(posted.message))
        })
        .into_response()
}

async fn post_message(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<Uuid>,
    Json(m): Json<NewMessage>,
) -> Response {
    guarded!(s, h, q);
    s.desk
        .post(id, m)
        .await
        .map(|posted| {
            let status = match posted.created {
                true => StatusCode::CREATED,
                false => StatusCode::OK,
            };
            (status, Json(posted.message))
        })
        .into_response()
}

async fn search(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    filters: Result<Query<SearchFilters>, QueryRejection>,
) -> Response {
    guarded!(s, h, q);
    let Query(filters) = match filters {
        Ok(filters) => filters,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    s.desk.search(&filters).await.map(Json).into_response()
}

/// A search of what Desk remembers (`?q=&group=&session=&kind=`).
#[derive(Debug, serde::Deserialize)]
struct MemoryParams {
    q: String,
    /// A group's id, or `ungrouped`.
    group: Option<String>,
    session: Option<Uuid>,
    /// `exchanges` or `all`; decisions, todos and facts are noted once Desk
    /// writes them, and none are kept yet.
    kind: Option<String>,
    limit: Option<i64>,
}

/// The person's keyword search of what Desk remembers of its sessions.
async fn memory_search(
    State(s): S,
    h: HeaderMap,
    Query(q): Q,
    p: Result<Query<MemoryParams>, QueryRejection>,
) -> Response {
    guarded!(s, h, q);
    let Query(p) = match p {
        Ok(p) => p,
        Err(e) => return DeskError::Invalid(e.body_text()).into_response(),
    };
    let run = async {
        let exchanges = match p.kind.as_deref().unwrap_or("all") {
            "all" | "exchanges" => true,
            "decisions" | "todos" | "facts" => false,
            _ => {
                return Err(DeskError::Invalid(
                    "kind is decisions, todos, facts, exchanges or all".into(),
                ))
            }
        };
        let mut found = s
            .desk
            .memory_search(&crate::memory_store::MemoryQuery {
                q: p.q,
                group: p
                    .group
                    .map(|g| Some(g).filter(|g| g != crate::service::UNGROUPED)),
                session: p.session,
                limit: p.limit.unwrap_or(20),
            })
            .await?;
        if !exchanges {
            found.hits.clear();
        }
        Ok(found)
    };
    run.await.map(Json).into_response()
}

/// What Desk remembers of one session.
async fn session_memory(State(s): S, h: HeaderMap, Query(q): Q, Path(id): Path<Uuid>) -> Response {
    guarded!(s, h, q);
    s.desk.session_memory(id).await.map(Json).into_response()
}

/// The sessions' and rooms' routes and the events socket alone.
pub fn sessions_router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/sessions", get(list).post(create))
        .route("/api/desk/engines", get(engines))
        .route("/api/desk/engines/sign-ins", post(start_engine_sign_in))
        .route(
            "/api/desk/engines/sign-ins/{id}",
            get(engine_sign_in).delete(cancel_engine_sign_in),
        )
        .route("/api/desk/accounts", get(accounts).post(add_account))
        .route(
            "/api/desk/accounts/{id}",
            patch(update_account).delete(remove_account),
        )
        .route("/api/desk/accounts/{id}/hand-over", post(hand_over_account))
        .route("/api/desk/accounts/sign-ins/{id}/code", post(sign_in_code))
        .route(
            "/api/desk/accounts/sign-ins/{id}/direct",
            get(sign_in_direct),
        )
        .route("/api/desk/accounts/switch-all", post(switch_all))
        .route(
            "/api/desk/accounts/own-sign-in",
            axum::routing::put(own_sign_in),
        )
        .route(
            "/api/desk/accounts/standing",
            axum::routing::put(learn_standing),
        )
        .route("/api/desk/accounts/needs/{id}/put-off", post(put_off_need))
        .route(
            "/api/desk/accounts/auto-switch",
            axum::routing::put(auto_switch),
        )
        .route(
            "/api/desk/accounts/problems/{id}/dismiss",
            post(dismiss_problem),
        )
        .route(
            "/api/desk/accounts/sign-ins/{id}",
            axum::routing::delete(cancel_sign_in),
        )
        .route("/api/desk/sessions/{id}/account", post(switch_account))
        .route("/api/desk/spend", get(spend))
        .route("/api/desk/ledger", get(ledger))
        .route("/api/desk/memory/search", get(memory_search))
        .route("/api/desk/sessions/{id}/memory", get(session_memory))
        .route("/api/desk/budgets", get(budgets).post(put_budget))
        .route(
            "/api/desk/budgets/{id}",
            patch(patch_budget).delete(delete_budget),
        )
        .route("/api/desk/breaker/rules", axum::routing::put(breaker_rules))
        .route("/api/desk/breakers/continue", post(breaker_continue))
        .route("/api/desk/breakers/keep", post(breaker_keep))
        .route("/api/desk/sessions/{id}", patch(update).delete(forget))
        .route("/api/desk/sessions/closed", get(closed))
        .route("/api/desk/sessions/hidden", get(hidden))
        .route(
            "/api/desk/sessions/hidden/{engine}/{cli}",
            axum::routing::delete(unhide),
        )
        .route("/api/desk/sessions/clear", post(clear))
        .route("/api/desk/sessions/{id}/remove", post(remove))
        .route("/api/desk/sessions/{id}/end", post(end))
        .route("/api/desk/sessions/{id}/stop", post(stop))
        .route("/api/desk/sessions/{id}/interrupt", post(interrupt))
        .route("/api/desk/sessions/{id}/asked/allow", post(allow_asked))
        .route("/api/desk/sessions/{id}/asked/dismiss", post(dismiss_asked))
        .route("/api/desk/sessions/{id}/resume", post(resume))
        .route("/api/desk/sessions/{id}/restart", post(restart))
        .route(
            "/api/desk/terminal/colors",
            // 20 colours: a few hundred bytes.
            axum::routing::put(terminal_colors).layer(axum::extract::DefaultBodyLimit::max(4096)),
        )
        .route("/api/desk/sessions/{id}/approvals/{approval}", post(decide))
        .route("/api/desk/sessions/{id}/pty", get(pty))
        .route(
            "/api/desk/sessions/{id}/name",
            axum::routing::put(name_session),
        )
        .route("/api/desk/folders", get(folders))
        .route("/api/desk/layout", get(layout).put(replace_layout))
        .route("/api/desk/layout/import", post(import_layout))
        .route("/api/desk/groups", get(layout).post(create_group))
        .route(
            "/api/desk/groups/{id}",
            patch(update_group).delete(delete_group),
        )
        .route("/api/desk/groups/{id}/move", post(move_group))
        .route(
            "/api/desk/groups/ungrouped/order",
            axum::routing::put(order_ungrouped),
        )
        .route(
            "/api/desk/groups/{id}/items/{*item}",
            axum::routing::put(put_member).delete(remove_member),
        )
        .route("/api/desk/terminals", get(terminals).post(create_terminal))
        .route(
            "/api/desk/terminals/{id}",
            patch(update_terminal).delete(close_terminal),
        )
        .route("/api/desk/terminals/{id}/reopen", post(reopen_terminal))
        .route("/api/desk/terminals/{id}/programs", get(terminal_programs))
        .route("/api/desk/terminals/{id}/pty", get(terminal_pty))
        .route("/api/desk/restore", post(restore))
        .route("/api/desk/restore/dismiss", post(dismiss_restore))
        .route("/api/desk/events", get(events))
        .route("/api/desk/rooms", get(rooms).post(create_room))
        .route("/api/desk/rooms/search", get(search))
        .route(
            "/api/desk/rooms/{id}",
            patch(update_room).delete(delete_room),
        )
        .route(
            "/api/desk/rooms/{id}/messages",
            get(room_messages).post(post_message),
        )
        .route("/api/desk/rooms/{id}/messages/{message}", get(room_message))
        .route(
            "/api/desk/rooms/{id}/messages/{message}/copy",
            post(copy_message),
        )
        .with_state(Arc::new(ApiState {
            desk: desk.clone(),
            auth: Arc::clone(&auth),
            handed: Default::default(),
        }))
        .merge(crate::line_api::api_routes(desk.clone(), Arc::clone(&auth)))
        .merge(crate::charge_api::api_routes(
            desk.clone(),
            Arc::clone(&auth),
        ))
        .merge(crate::board_api::api_routes(desk, auth))
}

pub fn router(
    desk: Desk,
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
    accounts: Arc<dyn Accounts>,
    crew: Arc<dyn crate::crew::Crew>,
) -> Router {
    desk.read_replies_with(&crew);
    sessions_router(desk.clone(), Arc::clone(&auth))
        .merge(crate::state::router(desk.clone(), Arc::clone(&auth)))
        .merge(crate::computer::api::computer_router(
            desk.clone(),
            Arc::clone(&auth),
        ))
        .merge(crate::crew_api::crew_router(desk, Arc::clone(&auth), crew))
        .merge(voice_router(Arc::clone(&auth), realtime, speech))
        .merge(providers_router(auth, accounts))
}

struct ProvidersApi {
    auth: Arc<dyn Authorize>,
    accounts: Arc<dyn Accounts>,
}

type P = State<Arc<ProvidersApi>>;

impl IntoResponse for AccountError {
    fn into_response(self) -> Response {
        let status = match &self {
            AccountError::Invalid(_) => StatusCode::BAD_REQUEST,
            AccountError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AccountError::Internal(e) => {
                tracing::error!(error = %e, "desk provider credentials failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

fn unknown_provider(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("no provider named {id:?}") })),
    )
        .into_response()
}

async fn list_providers(State(s): P, h: HeaderMap, Query(q): Q) -> Response {
    guarded!(s, h, q);
    Json(Catalog::of(&*s.accounts).await).into_response()
}

/// Saves the credentials a provider's fields hold where the hub reads them,
/// and answers the provider as it now stands. What was typed is never logged
/// or echoed.
async fn connect_provider(
    State(s): P,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
    body: Result<Json<ConnectBody>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let Some(spec) = providers::spec(&id) else {
        return unknown_provider(&id);
    };
    let fields = body
        .map_err(|_| AccountError::Invalid("the body is {\"fields\": {name: value}}".into()))
        .and_then(|Json(b)| providers::checked(spec, b.fields));
    let fields = match fields {
        Ok(fields) => fields,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = s.accounts.connect(spec, &fields).await {
        return e.into_response();
    }
    tracing::info!(provider = spec.id, "desk provider connected");
    Json(Entry::of(spec, &*s.accounts).await).into_response()
}

/// Removes a provider's credentials (a plan's login included).
async fn disconnect_provider(
    State(s): P,
    h: HeaderMap,
    Query(q): Q,
    Path(id): Path<String>,
) -> Response {
    guarded!(s, h, q);
    let Some(spec) = providers::spec(&id) else {
        return unknown_provider(&id);
    };
    if let Err(e) = s.accounts.disconnect(spec).await {
        return e.into_response();
    }
    tracing::info!(provider = spec.id, "desk provider disconnected");
    Json(Entry::of(spec, &*s.accounts).await).into_response()
}

pub(crate) fn providers_router(auth: Arc<dyn Authorize>, accounts: Arc<dyn Accounts>) -> Router {
    Router::new()
        .route("/api/desk/providers", get(list_providers))
        .route(
            "/api/desk/providers/{id}",
            axum::routing::delete(disconnect_provider),
        )
        .route("/api/desk/providers/{id}/connect", post(connect_provider))
        .with_state(Arc::new(ProvidersApi { auth, accounts }))
}

struct VoiceApi {
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
}

#[derive(Deserialize)]
struct SessionBody {
    engine: Engine,
}

/// A short-lived way into a cloud voice session for the Desk app; the
/// provider key never leaves the vault.
async fn voice_session(
    State(s): State<Arc<VoiceApi>>,
    h: HeaderMap,
    Query(q): Q,
    Json(b): Json<SessionBody>,
) -> Response {
    guarded!(s, h, q);
    let host = h
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    match s.realtime.grant(b.engine, host).await {
        Ok(grant) => Json(grant).into_response(),
        Err(e) => {
            let status = match e {
                RealtimeError::NoKey(_) => StatusCode::SERVICE_UNAVAILABLE,
                RealtimeError::Upstream(_) => StatusCode::BAD_GATEWAY,
            };
            (status, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct RelayQuery {
    ticket: String,
}

/// The GPT-Live relay; the single-use ticket from `voice_session` is the
/// credential.
async fn voice_relay(
    State(s): State<Arc<VoiceApi>>,
    Query(r): Query<RelayQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !s.realtime.redeem(&r.ticket) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let realtime = Arc::clone(&s.realtime);
    ws.on_upgrade(move |socket| async move { realtime.relay(socket).await })
}

impl IntoResponse for SpeakError {
    fn into_response(self) -> Response {
        let status = match &self {
            SpeakError::Invalid(_) => StatusCode::BAD_REQUEST,
            SpeakError::NoKey(_) => StatusCode::SERVICE_UNAVAILABLE,
            SpeakError::Upstream(e) => {
                tracing::warn!(error = %e, "desk speech synthesis failed");
                StatusCode::BAD_GATEWAY
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Speaks `text` with a vault key; answers the audio itself.
async fn voice_speak(
    State(s): State<Arc<VoiceApi>>,
    h: HeaderMap,
    Query(q): Q,
    body: Result<Json<Speak>, JsonRejection>,
) -> Response {
    guarded!(s, h, q);
    let speak = body
        .map_err(|e| SpeakError::Invalid(e.body_text()))
        .and_then(|Json(speak)| speak.validated());
    let speak = match speak {
        Ok(speak) => speak,
        Err(e) => return e.into_response(),
    };
    match s.speech.speak(&speak).await {
        Ok(audio) => ([(header::CONTENT_TYPE, audio.media_type)], audio.bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(crate) fn voice_router(
    auth: Arc<dyn Authorize>,
    realtime: Arc<Realtime>,
    speech: Arc<dyn Speech>,
) -> Router {
    Router::new()
        .route("/api/desk/voice/session", post(voice_session))
        .route("/api/desk/voice/relay", get(voice_relay))
        .route("/api/desk/voice/speak", post(voice_speak))
        .with_state(Arc::new(VoiceApi {
            auth,
            realtime,
            speech,
        }))
}

async fn hook(
    State(desk): State<Desk>,
    Path((id, token)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    match desk.hook(id, &token, &body).await {
        Some(answer) => Json(answer).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn terminal_hook(
    State(desk): State<Desk>,
    Path((terminal, token)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    match desk.terminal_hook(terminal, &token, &body).await {
        Some(answer) => Json(answer).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Served on its own loopback listener; the per-session token in the URL is
/// the credential.
pub(crate) fn hook_router(desk: Desk) -> Router {
    Router::new()
        .route("/hook/{id}/{token}", post(hook))
        .route("/hook/terminal/{terminal}/{token}", post(terminal_hook))
        .merge(crate::line_api::hook_routes())
        .with_state(desk)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message as Ws;
    use workforce_postgres::test_support::with_test_pool;

    use crate::service::NewTerminal;
    use crate::store::{build_pool, DeskStore};
    use crate::testing::{allow_all, options};

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// The next text frame of `kind`, skipping output.
    async fn next_text(ws: &mut Client, kind: &str) -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("a frame in time")
                .unwrap()
                .unwrap();
            if let Ws::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        }
    }

    /// Output until `needle` shows up.
    async fn output_until(ws: &mut Client, needle: &str) -> String {
        let mut seen = String::new();
        while !seen.contains(needle) {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("output in time")
                .unwrap()
                .unwrap();
            if let Ws::Binary(b) = m {
                seen.push_str(&String::from_utf8_lossy(&b));
            }
        }
        seen
    }

    #[tokio::test]
    async fn two_viewers_one_types_the_other_mirrors_until_it_claims() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
            opts.env = BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SHELL".into(), "/bin/sh".into()),
            ]);
            DeskStore::migrate(&pool).await.unwrap();
            let desk = crate::Desk::start(
                DeskStore::from_pool(pool),
                opts,
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let t = desk
                .create_terminal(NewTerminal {
                    id: None,
                    name: None,
                    cwd: dir.path().to_string_lossy().into_owned(),
                    env: BTreeMap::new(),
                })
                .await
                .unwrap();
            let app = super::sessions_router(desk.clone(), allow_all());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("ws://{addr}/api/desk/terminals/{}/pty", t.id);

            let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let size = next_text(&mut first, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(120), Some(40))
            );
            assert_eq!(next_text(&mut first, "role").await["active"], true);
            let (mut second, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let role = next_text(&mut second, "role").await;
            assert_eq!(
                (role["active"].as_bool(), role["free"].as_bool()),
                (Some(false), Some(false))
            );

            // The mirror's keys and size go nowhere.
            second
                .send(Ws::Binary("echo from-mirror\n".into()))
                .await
                .unwrap();
            second
                .send(Ws::text(
                    json!({ "type": "resize", "cols": 50, "rows": 10 }).to_string(),
                ))
                .await
                .unwrap();
            first
                .send(Ws::text(
                    json!({ "type": "resize", "cols": 100, "rows": 30 }).to_string(),
                ))
                .await
                .unwrap();
            let size = next_text(&mut second, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(100), Some(30))
            );
            first
                .send(Ws::Binary("echo from-first\n".into()))
                .await
                .unwrap();
            let seen = output_until(&mut second, "from-first\r\n").await;
            assert!(!seen.contains("from-mirror"));

            // Claiming makes it the one that types, at the size it asked for.
            second.send(Ws::text(r#"{"type":"claim"}"#)).await.unwrap();
            assert_eq!(next_text(&mut second, "role").await["active"], true);
            assert_eq!(next_text(&mut first, "role").await["active"], false);
            let size = next_text(&mut first, "size").await;
            assert_eq!(
                (size["cols"].as_u64(), size["rows"].as_u64()),
                (Some(50), Some(10))
            );
            second
                .send(Ws::Binary("echo now-second\n".into()))
                .await
                .unwrap();
            output_until(&mut first, "now-second\r\n").await;
            desk.close_terminal(t.id).unwrap();
        })
        .await;
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
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
    async fn an_accounts_sign_in_is_handed_over_to_the_admin_alone_and_never_kept() {
        use std::os::unix::fs::PermissionsExt;
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            // A Claude Code that signs in by a "good" token.
            let cli = dir.path().join("claude");
            std::fs::write(
                &cli,
                "#!/bin/sh\ncase \"$1 $2\" in\n'--version ') echo '2.1.300 (Claude Code)' ;;\n\
                 'auth status') t=$(cat <&3 2>/dev/null); case \"$t\" in sk-ant-oat01-good*) echo '{\"loggedIn\":true,\"authMethod\":\"oauth_token\"}' ;; *) echo '{\"loggedIn\":false}' ;; esac ;;\nesac\n",
            )
            .unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let mut opts = options(dir.path());
            opts.programs.insert(crate::engine::Engine::Claude, cli);
            opts.account_keys = Some(std::sync::Arc::new(
                crate::engine_accounts::MemoryKeys::default(),
            ));
            let desk = crate::Desk::start(
                DeskStore::from_pool(pool),
                opts,
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let token = "sk-ant-oat01-good-0123456789abcdefghijklmnopqrstuvwxyz";
            let id = match desk
                .add_account(crate::engine_accounts::NewAccount {
                    engine: crate::engine::Engine::Claude,
                    name: "Work".into(),
                    token: Some(token.into()),
                    make_default: true,
                    remote: false,
                })
                .await
                .unwrap()
            {
                crate::service::Added::Account { account } => account.id,
                crate::service::Added::SignIn { .. } => unreachable!(),
            };
            let path = format!("/api/desk/accounts/{id}/hand-over");

            // Not the admin: refused.
            let denied = super::sessions_router(desk.clone(), crate::testing::deny_all());
            let (status, body) = call(&denied, "POST", &path, None).await;
            assert!(status == 401 || status == 403, "{status}");
            assert!(!body.to_string().contains("sk-ant"), "{body}");

            let app = super::sessions_router(desk.clone(), allow_all());
            // A token in the address (which logs keep) is never answered.
            let (status, body) = call(&app, "POST", &format!("{path}?token=x"), None).await;
            assert_eq!(status, 400, "{body}");
            assert!(!body.to_string().contains("sk-ant"), "{body}");
            // Only POST.
            let (status, _) = call(&app, "GET", &path, None).await;
            assert_eq!(status, 405);

            // The admin: the whole token, never kept on the way.
            use tower::ServiceExt;
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(&path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status().as_u16(), 200);
            assert_eq!(res.headers()["cache-control"], "no-store");
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body, json!({"engine": "claude", "name": "Work", "isDefault": true, "token": token}));
            // The list still never says it.
            let (_, listed) = call(&app, "GET", "/api/desk/accounts", None).await;
            assert!(!listed.to_string().contains("sk-ant"), "{listed}");
        })
        .await;
    }

    #[tokio::test]
    async fn folders_are_listed_from_home_and_a_path_out_of_it_is_refused() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join("app/.git")).unwrap();
            std::fs::write(home.join("a-file"), "x").unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let mut opts = options(dir.path());
            opts.env = BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]);
            let desk = crate::Desk::start(
                DeskStore::from_pool(pool),
                opts,
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (status, l) = call(&app, "GET", "/api/desk/folders", None).await;
            assert_eq!(status, 200, "{l}");
            assert_eq!(l["folders"].as_array().unwrap().len(), 1);
            assert_eq!(l["folders"][0]["git"], true);
            for path in ["..", "/etc"] {
                let (status, _) =
                    call(&app, "GET", &format!("/api/desk/folders?path={path}"), None).await;
                assert_eq!(status, 403, "{path}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn every_engine_is_listed_and_one_desk_cant_start_is_refused_in_plain_words() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let desk = crate::testing::desk_on(pool, dir.path()).await;
            let app = super::sessions_router(desk, allow_all());
            let (status, engines) = call(&app, "GET", "/api/desk/engines", None).await;
            assert_eq!(status, 200);
            let names: Vec<&str> = engines
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["engine"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                ["claude", "codex", "gemini", "copilot", "opencode", "cursor", "devin", "custom"]
            );
            assert_eq!(engines[0]["views"], json!(["terminal"]));
            assert_eq!(engines[0]["caps"]["terminal"]["state"], "hooks");
            assert_eq!(engines[2]["views"], json!([]));
            assert_eq!(engines[7]["program"], Value::Null);
            let cwd = dir.path().to_string_lossy().into_owned();
            let (status, refused) = call(
                &app,
                "POST",
                "/api/desk/sessions",
                Some(json!({ "engine": "gemini", "name": "atlas", "cwd": cwd })),
            )
            .await;
            assert_eq!(status, 400);
            assert!(
                refused
                    .to_string()
                    .contains("Desk can't start Gemini CLI sessions yet"),
                "{refused}"
            );
            let (status, refused) = call(
                &app,
                "POST",
                "/api/desk/sessions",
                Some(json!({ "engine": "claude", "view": "desk", "name": "atlas", "cwd": cwd })),
            )
            .await;
            assert_eq!(status, 400);
            assert!(
                refused.to_string().contains("in its desk view"),
                "{refused}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn spend_and_the_ledger_are_read_through_their_routes() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join("t.jsonl");
            std::fs::write(&transcript, include_str!("../fixtures/ledger/claude.jsonl")).unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut r = crate::store::tests::record_fixture("acme-web");
            r.transcript = Some(transcript.to_string_lossy().into_owned());
            store.put(&r).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let uri =
                "/api/desk/spend?from=2026-10-31T00:00:00Z&to=2026-11-01T00:00:00Z&by=session";
            let mut view = Value::Null;
            for _ in 0..200 {
                view = call(&app, "GET", uri, None).await.1;
                if view["total"]["calls"] == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(view["total"]["calls"], 2, "{view}");
            assert_eq!(view["rows"][0]["label"], "acme-web");
            assert_eq!(view["rows"][0]["session"], r.id.to_string());
            assert_eq!(view["total"]["tokens"]["cacheRead"], 20000);
            assert_eq!(
                view["total"]["unpricedModels"],
                json!(["claude-opus-5-5"]),
                "no pricer here"
            );
            let (status, page) = call(
                &app,
                "GET",
                &format!("/api/desk/ledger?session={}", r.id),
                None,
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(page["rows"].as_array().unwrap().len(), 2);
            assert_eq!(page["rows"][0]["key"], "msg_02");
            assert_eq!(page["next"], Value::Null);
            let (status, _) = call(&app, "GET", "/api/desk/spend?period=fortnight", None).await;
            assert_eq!(status, 400);
            let (status, _) = call(&app, "GET", "/api/desk/spend?by=planet", None).await;
            assert_eq!(status, 400);
            let (status, _) = call(&app, "GET", "/api/desk/ledger?before=nonsense", None).await;
            assert_eq!(status, 400);
        })
        .await;
    }

    #[tokio::test]
    async fn names_and_groups_through_the_routes() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = crate::store::tests::record_fixture("monorepo-fd");
            store.put(&r).await.unwrap();
            let desk = crate::Desk::start(store, options(dir.path()), Default::default(), tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let s = format!("session:{}", r.id);

            let (status, v) = call(&app, "PUT", &format!("/api/desk/sessions/{}/name", r.id), Some(json!({ "name": "Ledger migration" }))).await;
            assert_eq!((status, &v["displayName"]), (200, &json!("Ledger migration")));
            let (_, v) = call(&app, "PATCH", &format!("/api/desk/sessions/{}", r.id), Some(json!({ "displayName": null }))).await;
            assert_eq!(v["displayName"], Value::Null);
            assert_eq!(v["name"], "monorepo-fd");

            let (status, l) = call(&app, "POST", "/api/desk/groups", Some(json!({ "id": "g1", "name": "Atlas" }))).await;
            assert_eq!((status, &l["groups"][0]["name"]), (200, &json!("Atlas")));
            call(&app, "POST", "/api/desk/groups", Some(json!({ "id": "g2", "name": "Infra", "items": ["agent:dw/allan"] }))).await;
            let (status, l) = call(&app, "PUT", &format!("/api/desk/groups/g1/items/{s}"), Some(json!({ "before": null }))).await;
            assert_eq!((status, &l["groups"][0]["items"]), (200, &json!([s])));
            let (_, v) = call(&app, "GET", "/api/desk/sessions", None).await;
            assert_eq!(v[0]["group"], json!({ "id": "g1", "name": "Atlas" }));
            let (_, l) = call(&app, "POST", "/api/desk/groups/g2/move", Some(json!({ "before": "g1" }))).await;
            assert_eq!(l["groups"][0]["id"], "g2");
            let (_, l) = call(&app, "PATCH", "/api/desk/groups/g1", Some(json!({ "name": "Atlas work", "collapsed": true }))).await;
            assert_eq!((&l["groups"][1]["name"], &l["groups"][1]["collapsed"]), (&json!("Atlas work"), &json!(true)));
            let (_, l) = call(&app, "PATCH", "/api/desk/groups/ungrouped", Some(json!({ "collapsed": true }))).await;
            assert_eq!(l["ungrouped"]["collapsed"], true);
            let (status, _) = call(&app, "PATCH", "/api/desk/groups/ungrouped", Some(json!({ "name": "x" }))).await;
            assert_eq!(status, 400);
            let (status, l) = call(&app, "DELETE", &format!("/api/desk/groups/g1/items/{s}"), None).await;
            assert_eq!((status, &l["ungrouped"]["items"]), (200, &json!([s])));
            let (status, _) = call(&app, "DELETE", "/api/desk/groups/g1/items/agent:dw/allan", None).await;
            assert_eq!(status, 404, "not in that group");
            let (status, l) = call(&app, "DELETE", "/api/desk/groups/g2", None).await;
            assert_eq!(status, 200);
            assert_eq!(l["ungrouped"]["items"], json!([s, "agent:dw/allan"]));
            let (status, _) = call(&app, "POST", "/api/desk/groups", Some(json!({ "name": "  " }))).await;
            assert_eq!(status, 400);
            let (_, got) = call(&app, "GET", "/api/desk/layout", None).await;
            assert_eq!(got["groups"].as_array().unwrap().len(), 1);
            // Made from a layout that has changed since: refused, nothing written.
            let stale = json!({ "groups": [], "ungrouped": { "collapsed": false, "items": [] } });
            let (status, _) = call(&app, "PUT", "/api/desk/layout", Some(json!({ "groups": [], "ungrouped": { "collapsed": false, "items": [] }, "expected": stale }))).await;
            assert_eq!(status, 409);
            let (_, still) = call(&app, "GET", "/api/desk/layout", None).await;
            assert_eq!(still, got);
            // Made from the one it has: written.
            let (status, l) = call(&app, "PUT", "/api/desk/layout", Some(json!({ "groups": [], "ungrouped": { "collapsed": false, "items": [] }, "expected": got }))).await;
            assert_eq!((status, l["groups"].as_array().unwrap().len()), (200, 0));
            // A machine's session has a place through the wildcard route, by Desk's key for it.
            call(&app, "POST", "/api/desk/groups", Some(json!({ "id": "dev", "name": "Devbox" }))).await;
            let devbox = format!("session:devbox/{}", uuid::Uuid::from_u128(77));
            let (status, l) = call(&app, "PUT", &format!("/api/desk/groups/dev/items/{devbox}"), Some(json!({ "before": null }))).await;
            assert_eq!((status, &l["groups"][0]["items"]), (200, &json!([devbox])));
            let (status, _) = call(&app, "PUT", "/api/desk/groups/dev/items/session:devbox/nonsense", Some(json!({ "before": null }))).await;
            assert_eq!(status, 400);
            let (status, l) = call(&app, "PUT", "/api/desk/layout", Some(json!({ "groups": [], "ungrouped": { "collapsed": false, "items": [] } }))).await;
            assert_eq!((status, l["groups"].as_array().unwrap().len()), (200, 0));
            let (status, i) = call(&app, "POST", "/api/desk/layout/import", Some(json!({
                "layout": { "groups": [{ "id": "m", "name": "Atlas", "collapsed": false, "items": [s] }], "ungrouped": { "collapsed": false, "items": [] } },
                "sessionNames": { r.id.to_string(): "Ledger migration" }
            }))).await;
            assert_eq!((status, &i["imported"]), (200, &json!(true)));
            assert_eq!(i["sessionNames"][r.id.to_string()], "Ledger migration");

            // Ungrouped's order as Desk settled it: taken out of its group.
            let (status, l) = call(&app, "PUT", "/api/desk/groups/ungrouped/order", Some(json!({ "items": ["agent:dw/allan", s, "agent:local/scout"] }))).await;
            assert_eq!((status, &l["ungrouped"]["items"]), (200, &json!(["agent:dw/allan", s, "agent:local/scout"])));
            assert_eq!(l["groups"][0]["items"], json!([]));
            let (status, _) = call(&app, "PUT", "/api/desk/groups/ungrouped/order", Some(json!({ "items": ["nonsense"] }))).await;
            assert_eq!(status, 400);
            // Still a group's own route: a group may not be called "ungrouped".
            let (status, l) = call(&app, "PUT", &format!("/api/desk/groups/m/items/{s}"), Some(json!({}))).await;
            assert_eq!((status, &l["groups"][0]["items"]), (200, &json!([s])));

            // Forgotten, the session leaves the layout.
            let (status, _) = call(&app, "DELETE", &format!("/api/desk/sessions/{}", r.id), None).await;
            assert_eq!(status, 204);
            let (_, l) = call(&app, "GET", "/api/desk/layout", None).await;
            assert_eq!((&l["groups"][0]["items"], &l["ungrouped"]["items"]), (&json!([]), &json!(["agent:dw/allan", "agent:local/scout"])));
        })
        .await;
    }

    #[tokio::test]
    async fn sessions_end_clear_and_are_removed_and_tracked_again_through_the_routes() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let old = chrono::Utc::now() - chrono::Duration::days(9);
            let stopped = crate::model::SessionRecord {
                state: crate::model::SessionState::Stopped,
                cli_session_id: Some("cc-old".into()),
                terminal_id: None,
                updated_at: old,
                ..crate::store::tests::record_fixture("old-run")
            };
            let closed = crate::model::SessionRecord {
                state: crate::model::SessionState::Closed,
                cli_session_id: Some("cc-closed".into()),
                terminal_id: None,
                ..crate::store::tests::record_fixture("closed-run")
            };
            store.put(&stopped).await.unwrap();
            store.put(&closed).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());

            let (status, c) = call(&app, "GET", "/api/desk/sessions/closed", None).await;
            assert_eq!(
                (status, c[0]["id"].as_str()),
                (200, Some(closed.id.to_string().as_str()))
            );
            let (status, n) = call(
                &app,
                "POST",
                "/api/desk/sessions/clear",
                Some(json!({ "olderThanDays": 7, "dryRun": true })),
            )
            .await;
            assert_eq!((status, &n["removed"]), (200, &json!(1)));
            let (status, _) = call(
                &app,
                "POST",
                "/api/desk/sessions/clear",
                Some(json!({ "olderThan": 7 })),
            )
            .await;
            assert_eq!(status, 400);
            let (status, n) = call(
                &app,
                "POST",
                &format!("/api/desk/sessions/{}/remove", closed.id),
                Some(json!({ "transcript": false })),
            )
            .await;
            assert_eq!((status, &n["removed"]), (200, &json!(1)));
            let (status, _) = call(
                &app,
                "POST",
                &format!("/api/desk/sessions/{}/end", stopped.id),
                None,
            )
            .await;
            assert_eq!(status, 200);
            let mut hidden = json!([]);
            for _ in 0..200 {
                hidden = call(&app, "GET", "/api/desk/sessions/hidden", None).await.1;
                if hidden.as_array().is_some_and(|h| !h.is_empty()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(hidden[0]["cliSessionId"], "cc-closed");
            assert_eq!(hidden[0]["name"], "closed-run");
            let (status, _) = call(
                &app,
                "DELETE",
                "/api/desk/sessions/hidden/claude/cc-closed",
                None,
            )
            .await;
            assert_eq!(status, 204);
            let (status, _) = call(
                &app,
                "DELETE",
                "/api/desk/sessions/hidden/nonsense/cc-closed",
                None,
            )
            .await;
            assert_eq!(status, 400);
        })
        .await;
    }

    #[tokio::test]
    async fn no_one_unauthorised_ends_removes_clears_or_lists_sessions() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let old = chrono::Utc::now() - chrono::Duration::days(9);
            let kept = crate::model::SessionRecord {
                state: crate::model::SessionState::Stopped,
                cli_session_id: Some("cc-kept".into()),
                terminal_id: None,
                updated_at: old,
                ..crate::store::tests::record_fixture("kept-run")
            };
            store.put(&kept).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk.clone(), crate::testing::deny_all());
            let id = kept.id;
            for (method, uri, body) in [
                ("GET", "/api/desk/sessions/closed".to_string(), None),
                ("GET", "/api/desk/sessions/hidden".to_string(), None),
                (
                    "DELETE",
                    "/api/desk/sessions/hidden/claude/cc-kept".to_string(),
                    None,
                ),
                (
                    "POST",
                    "/api/desk/sessions/clear".to_string(),
                    Some(json!({ "olderThanDays": 0 })),
                ),
                (
                    "POST",
                    format!("/api/desk/sessions/{id}/remove"),
                    Some(json!({ "transcript": true })),
                ),
                ("POST", format!("/api/desk/sessions/{id}/end"), None),
                ("DELETE", format!("/api/desk/sessions/{id}"), None),
            ] {
                let (status, _) = call(&app, method, &uri, body).await;
                assert_eq!(status, 401, "{method} {uri}");
            }
            assert_eq!(
                desk.list().iter().map(|s| s.id).collect::<Vec<_>>(),
                [id],
                "nothing changed"
            );
            assert_eq!(desk.list()[0].state, crate::model::SessionState::Stopped);
        })
        .await;
    }

    #[tokio::test]
    async fn a_sourced_room_post_is_created_once_and_repeats_answer_with_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(store, options(dir.path()), Default::default(), tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (status, room) = call(&app, "POST", "/api/desk/rooms", Some(json!({ "name": "Release", "members": [{ "hub": "local", "agent": "scout" }] }))).await;
            assert_eq!(status, 200);
            let channel = format!("desk:{}", room["id"].as_str().unwrap());
            assert_eq!(room["channel"], json!(channel));
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let reply = json!({ "author": "local/scout", "content": "may I?", "source": "local/scout/m-1", "delivery": "approval", "requestId": "req-1" });
            let (status, first) = call(&app, "POST", &uri, Some(reply.clone())).await;
            assert_eq!(
                (status, &first["source"], &first["delivery"], &first["requestId"]),
                (201, &json!("local/scout/m-1"), &json!("approval"), &json!("req-1"))
            );
            let (status, again) = call(&app, "POST", &uri, Some(reply)).await;
            assert_eq!((status, &again["id"]), (200, &first["id"]));
            let (status, said) = call(&app, "POST", &uri, Some(json!({ "author": "you", "content": "thanks" }))).await;
            assert_eq!((status, said.get("source"), said.get("delivery")), (201, None, None));
        })
        .await;
    }

    #[tokio::test]
    async fn room_pages_by_before_after_and_around_and_one_message_reads_alone() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (_, room) = call(
                &app,
                "POST",
                "/api/desk/rooms",
                Some(json!({ "name": "Atlas launch" })),
            )
            .await;
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let mut ids = Vec::new();
            for i in 0..5 {
                let (_, m) = call(
                    &app,
                    "POST",
                    &uri,
                    Some(json!({ "author": "you", "content": format!("line {i}") })),
                )
                .await;
                ids.push(m["id"].as_str().unwrap().to_string());
            }
            let ids_of = |v: &Value| {
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            };
            let (status, v) = call(
                &app,
                "GET",
                &format!("{uri}?before={}&limit=2", ids[3]),
                None,
            )
            .await;
            assert_eq!((status, ids_of(&v)), (200, ids[1..3].to_vec()));
            let (_, v) = call(&app, "GET", &format!("{uri}?after={}", ids[3]), None).await;
            assert_eq!(ids_of(&v), ids[4..].to_vec());
            let (_, v) = call(
                &app,
                "GET",
                &format!("{uri}?around={}&limit=3", ids[2]),
                None,
            )
            .await;
            assert_eq!(ids_of(&v), ids[1..4].to_vec());
            let (_, v) = call(
                &app,
                "GET",
                &format!("{uri}?before=2000-01-01T00:00:00Z"),
                None,
            )
            .await;
            assert_eq!(
                ids_of(&v),
                Vec::<String>::new(),
                "a plain time still reads as one"
            );
            let (status, _) = call(
                &app,
                "GET",
                &format!("{uri}?before={}", uuid::Uuid::new_v4()),
                None,
            )
            .await;
            assert_eq!(status, 404, "a message that isn't the room's");

            let (status, one) = call(&app, "GET", &format!("{uri}/{}", ids[2]), None).await;
            assert_eq!(
                (status, &one["content"], &one["forYou"]),
                (200, &json!("line 2"), &json!(false))
            );
            let (status, _) = call(
                &app,
                "GET",
                &format!("{uri}/{}", uuid::Uuid::new_v4()),
                None,
            )
            .await;
            assert_eq!(status, 404);
        })
        .await;
    }

    #[tokio::test]
    async fn two_spans_at_once_are_refused() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(
                store,
                options(dir.path()),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (_, room) = call(
                &app,
                "POST",
                "/api/desk/rooms",
                Some(json!({ "name": "Atlas launch" })),
            )
            .await;
            let uri = format!("/api/desk/rooms/{}/messages", room["id"].as_str().unwrap());
            let (_, m) = call(
                &app,
                "POST",
                &uri,
                Some(json!({ "author": "you", "content": "hi" })),
            )
            .await;
            let id = m["id"].as_str().unwrap();
            let (status, v) =
                call(&app, "GET", &format!("{uri}?before={id}&around={id}"), None).await;
            assert_eq!(
                (status, &v["error"]),
                (400, &json!("ask for one of before, after or around"))
            );
        })
        .await;
    }

    #[tokio::test]
    async fn search_takes_its_filters_from_the_query_and_a_thread_message_is_copied_once() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::with_pool(pool).await.unwrap();
            let desk = crate::Desk::start(store, options(dir.path()), Default::default(), tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            let app = super::sessions_router(desk, allow_all());
            let (_, room) = call(&app, "POST", "/api/desk/rooms", Some(json!({ "name": "Atlas launch" }))).await;
            let id = room["id"].as_str().unwrap();
            let uri = format!("/api/desk/rooms/{id}/messages");
            let (_, ask) = call(&app, "POST", &uri, Some(json!({ "author": "you", "content": "is the launch page ready?" }))).await;
            let (_, reply) = call(&app, "POST", &uri, Some(json!({ "author": "you", "content": "the launch page is ready", "threadId": ask["id"] }))).await;
            let (status, hits) = call(&app, "GET", &format!("/api/desk/rooms/search?q=launch&room={id}&from=you&inThread=true"), None).await;
            assert_eq!((status, hits.as_array().unwrap().len()), (200, 1));
            assert_eq!(hits[0]["message"]["id"], reply["id"]);
            assert!(hits[0]["snippet"].as_str().unwrap().contains('\u{E000}'));
            let (_, none) = call(&app, "GET", "/api/desk/rooms/search?q=", None).await;
            assert_eq!(none, json!([]));
            let (status, _) = call(&app, "GET", "/api/desk/rooms/search?has=nothing", None).await;
            assert_eq!(status, 400);

            let copy = format!("{uri}/{}/copy", reply["id"].as_str().unwrap());
            let (status, first) = call(&app, "POST", &copy, None).await;
            assert_eq!((status, &first["copyOf"]), (201, &reply["id"]));
            let (status, again) = call(&app, "POST", &copy, None).await;
            assert_eq!((status, &again["id"]), (200, &first["id"]));
            let (status, _) = call(&app, "POST", &format!("{uri}/{}/copy", ask["id"].as_str().unwrap()), None).await;
            assert_eq!(status, 400, "it is in the room already");
        })
        .await;
    }
}

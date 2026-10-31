//! The floor as the hub composes it for a person: their turns from the
//! log, live or not by who still runs them, named as they are now; the
//! needs they make; their plans' readings; and the routes that read and
//! answer them (`GET /api/desk/floor`, `POST /api/desk/needs/decide`,
//! `GET /api/desk/usage`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use workforce_agent::runner::approval_detail;
use workforce_common::channels::desk::{DeskPlace, DeskSpeaker, DeskTurnContext};
use workforce_common::channels::{is_platform, DESK_CHANNEL};
use workforce_common::subscription::{is_automated_source, split_per_user_key, SubscriptionKind};
use workforce_common::usage::{LoginRef, PlanKind, Reading, Window};
use workforce_core::policy::PermissionChoice;
use workforce_desk::crew::{self, By, PartyKind};
use workforce_desk::engine::{Engine, SignIns};
use workforce_desk::floor::{
    self, Answer, DecideRef, Need, NeedKind, Place, Scope, Subject, Turn, Who,
};
use workforce_desk::model::Decision;
use workforce_desk::{Desk, DeskError};
use workforce_identity::store::IdentityStore;

use crate::approvals::resume::APPROVAL_DIRECTIVE;
use crate::approvals::PendingApproval;
use crate::desk_crew::CHECK_IN;
use crate::floor::Installed;
use crate::gateway::{
    owns_session, resolve_caller_context, AuthFailure, CallerContext, GatewayState,
};
use crate::slack_manager::RESUME_PROMPT;

/// What an agent's turn came in as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Origin<'a> {
    pub channel: &'a str,
    pub user: &'a str,
    pub run_as: Option<&'a str>,
    pub message: &'a str,
    pub channel_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    /// The Desk turn it is, when Desk asked it.
    pub desk: Option<&'a DeskTurnContext>,
    /// Where on Desk a run made there answers (a job, a workflow step).
    pub answers_to: Option<&'a DeskPlace>,
}

impl Origin<'_> {
    /// Whose work it is: who it runs as, else who sent it.
    pub(crate) fn owner(&self) -> &str {
        self.run_as.unwrap_or(self.user)
    }
}

/// Where in Desk something said at `place` is found. A room an older Desk
/// knows only by its slug has no id to open, and its answers go to the DM.
pub(crate) fn place_in_desk(place: &DeskPlace) -> Place {
    match place {
        DeskPlace::Dm | DeskPlace::LegacyRoom { .. } => Place::Dm,
        DeskPlace::Room { room, thread, .. } => Place::Room {
            room: *room,
            thread: *thread,
        },
        DeskPlace::Call => Place::Call,
    }
}

/// Where a turn, and anything it asks, can be found.
pub(crate) fn place_of(o: &Origin<'_>) -> Place {
    if let Some(place) = o.desk.map(|d| &d.turn.place).or(o.answers_to) {
        return place_in_desk(place);
    }
    if o.channel == DESK_CHANNEL {
        return Place::Dm;
    }
    if is_platform(o.channel, "slack") {
        let mut key = o.channel.split(':').skip(1);
        let team = key.next().filter(|t| !t.is_empty()).map(str::to_string);
        if let Some(channel) = o.channel_id.or_else(|| key.next()) {
            return Place::Slack {
                team,
                channel: channel.to_string(),
                thread: o.thread_id.map(str::to_string),
            };
        }
    }
    Place::Elsewhere
}

/// Whether the person started the turn themselves, which settles what the
/// agent asked them: not a scheduled job, a workflow step, another agent's
/// hand-off, Desk's own check-in, a turn resumed after a restart, or one
/// resumed once an approval it asked for was decided.
pub(crate) fn by_person_of(o: &Origin<'_>) -> bool {
    o.desk.is_none_or(|d| d.turn.speaker == DeskSpeaker::Person)
        && !is_automated_source(o.user)
        && !o.user.starts_with("agent:")
        && !o.user.starts_with("system")
        && !o.message.starts_with(RESUME_PROMPT)
        && !o.message.starts_with(APPROVAL_DIRECTIVE)
        && !o.message.starts_with(CHECK_IN)
}

/// Whether `identity` holds this hub: on a hub running Desk, its sessions
/// are the person's who administers it, however they connect (the Desk app
/// reads its own hub with its device).
pub(crate) fn holds_the_hub(
    identities: Option<&Arc<dyn IdentityStore>>,
    has_desk: bool,
    identity: Option<&str>,
) -> bool {
    has_desk
        && identity
            .and_then(|id| Uuid::parse_str(id).ok())
            .zip(identities)
            .and_then(|(id, store)| store.get_by_id(id).ok().flatten())
            .is_some_and(|i| crate::authz::is_effective_admin(i.role, i.status))
}

/// Who the floor is read for: a person (their identity, else their paired
/// device), and whether this hub is theirs, with its Desk sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorCaller {
    pub id: String,
    pub personal: bool,
}

/// Why a request names no one the floor can be read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    Unauthorized,
    Unavailable,
}

impl FloorCaller {
    /// `holds`: whether the caller's identity holds the hub
    /// ([`holds_the_hub`]).
    fn of(caller: &CallerContext, holds: bool) -> Result<Self, Refused> {
        match caller.auth_failure {
            Some(AuthFailure::InvalidCredentials) => return Err(Refused::Unauthorized),
            Some(AuthFailure::StoreError) => return Err(Refused::Unavailable),
            None => {}
        }
        let id = caller
            .identity_id
            .clone()
            .or_else(|| caller.device_id.clone())
            .ok_or(Refused::Unauthorized)?;
        Ok(Self {
            id,
            personal: holds,
        })
    }

    fn scope(&self) -> Scope {
        Scope {
            owner: self.id.clone(),
            personal: self.personal,
        }
    }
}

/// What the floor reads from: the hub's store, and its Desk when it runs one.
#[derive(Clone)]
pub struct Sources {
    floor: Installed,
    desk: Arc<dyn Fn() -> Option<Desk> + Send + Sync>,
}

impl Sources {
    pub fn new(floor: Installed, desk: Arc<dyn Fn() -> Option<Desk> + Send + Sync>) -> Self {
        Self { floor, desk }
    }

    /// The hub's own, once its store is open.
    pub fn installed() -> Option<Self> {
        crate::floor::installed().map(|floor| {
            Self::new(
                floor.clone(),
                Arc::new(|| crate::desk_driver::desk().cloned()),
            )
        })
    }
}

/// What says which open turns still run: the sessions' live turn tokens,
/// the instances still running, and the Desk turns open now.
#[derive(Debug, Default)]
struct Running {
    tokens: HashMap<String, String>,
    alive: HashSet<String>,
    desk: HashSet<Uuid>,
}

/// Whether an open turn still runs. An agent's turn with a session record
/// runs while that record's token is its own on a live instance; one
/// without runs while its instance does. A Desk session's runs while the
/// desk has it open.
fn runs(t: &Turn, r: &Running) -> bool {
    match &t.subject {
        Subject::Session { .. } => r.desk.contains(&t.id),
        Subject::Agent { session_id, .. } => match &t.token {
            Some(token) => r.tokens.get(session_id) == Some(token),
            None => t.instance.as_ref().is_some_and(|i| r.alive.contains(i)),
        },
    }
}

async fn running(state: &GatewayState, sources: &Sources, turns: &[Turn]) -> eyre::Result<Running> {
    let open: Vec<&Turn> = turns.iter().filter(|t| t.ended_at.is_none()).collect();
    let mut by_agent: HashMap<&str, Vec<String>> = HashMap::new();
    open.iter().for_each(|t| {
        if let (
            Subject::Agent {
                agent_id,
                session_id,
            },
            Some(_),
        ) = (&t.subject, &t.token)
        {
            by_agent
                .entry(agent_id.as_str())
                .or_default()
                .push(session_id.clone());
        }
    });
    let mut tokens = HashMap::new();
    for (agent, sessions) in by_agent {
        let Some(service) = state.registry.get(agent) else {
            continue;
        };
        let store = Arc::clone(&service.sessions);
        let live = tokio::task::spawn_blocking(move || store.live_turns(&sessions)).await??;
        tokens.extend(live);
    }
    let instances: Vec<String> = open
        .iter()
        .filter(|t| t.token.is_none())
        .filter_map(|t| t.instance.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut alive = sources.floor.store.alive(&instances).await?;
    alive.insert(workforce_common::instance::process_instance_id().to_string());
    Ok(Running {
        tokens,
        alive,
        desk: (sources.desk)().map(|d| d.open_turns()).unwrap_or_default(),
    })
}

/// `turns` named as their agents and sessions are called now. What an
/// agent or session that is gone asked is no one's to answer, and is left
/// out; the rest of its work keeps the name it had.
fn named(turns: Vec<Turn>, name: impl Fn(&Subject) -> Option<String>) -> Vec<Turn> {
    turns
        .into_iter()
        .filter_map(|t| match name(&t.subject) {
            Some(name) => Some(Turn { name, ..t }),
            None => (!t.asking()).then_some(t),
        })
        .collect()
}

/// The caller's turns since `since`, every open one and every question
/// still asked, each open one settled by whether it still runs.
async fn turns(
    state: &GatewayState,
    sources: &Sources,
    caller: &FloorCaller,
    since: DateTime<Utc>,
) -> eyre::Result<Vec<Turn>> {
    let logged = sources.floor.log.relevant(&caller.scope(), since).await?;
    let running = running(state, sources, &logged).await?;
    let live: HashSet<Uuid> = logged
        .iter()
        .filter(|t| t.ended_at.is_none() && runs(t, &running))
        .map(|t| t.id)
        .collect();
    let desk = (sources.desk)();
    Ok(named(
        floor::settle(logged, &live),
        |subject| match subject {
            Subject::Agent { agent_id, .. } => state
                .registry
                .get(agent_id)
                .map(|s| crate::desk_crew::name_of(&s)),
            Subject::Session { id, .. } => desk
                .as_ref()
                .and_then(|d| d.view(*id).ok())
                .map(|v| v.label().to_string()),
        },
    ))
}

/// A routed approval waiting on its approver, as a need: the call it asks
/// about (`detail`, as the approver's card shows it), where the run that
/// asked was made, and the agent that asked (`name`).
fn routed_need(row: &PendingApproval, detail: &str, place: Place, name: String) -> Need {
    Need {
        id: format!("routed:{}", row.request_id),
        kind: NeedKind::Approval,
        who: Who {
            kind: PartyKind::Agent,
            id: row.agent_id.clone(),
            name,
            engine: None,
        },
        ask: crew::approval_summary(&row.tool_name, detail),
        options: None,
        since: row.created_at,
        place,
        answer: Answer::Decide {
            at: DecideRef::Routed {
                request_id: row.request_id.clone(),
            },
        },
    }
}

/// The routed approvals waiting on `caller` now, each where its run was
/// made (the DM, when that is not known).
fn routed_needs(
    state: &GatewayState,
    caller: &FloorCaller,
    now: DateTime<Utc>,
) -> eyre::Result<Vec<Need>> {
    let Some(store) = state
        .registry
        .resolve(None)
        .and_then(|svc| svc.pending_approval_store.read().clone())
    else {
        return Ok(Vec::new());
    };
    Ok(store
        .pending_for(&caller.id, now)?
        .iter()
        .map(|row| {
            let service = crate::approvals::resume::resume_service(&state.registry, row);
            let detail = service
                .as_ref()
                .map(|s| approval_detail(&s.tools, &row.tool_name, &row.tool_input))
                .unwrap_or_default();
            let place = service
                .as_ref()
                .and_then(|s| s.sessions.find(&row.session_key, &row.requester_id).ok())
                .flatten()
                .and_then(|session| session.desk_origin())
                .map_or(Place::Dm, |target| place_in_desk(&target.place));
            let name = service
                .as_ref()
                .filter(|s| s.agent_id.as_str() == row.agent_id)
                .map_or_else(|| row.agent_id.clone(), |s| crate::desk_crew::name_of(s));
            routed_need(row, &detail, place, name)
        })
        .collect())
}

/// What still needs the person: what their turns wait on and ask, and the
/// routed approvals waiting on them. The one source every count is a count
/// of.
fn needs_of(
    state: &GatewayState,
    caller: &FloorCaller,
    turns: &[Turn],
    now: DateTime<Utc>,
) -> eyre::Result<Vec<Need>> {
    Ok(floor::longest_waiting(
        floor::open_needs(turns)
            .into_iter()
            .chain(routed_needs(state, caller, now)?)
            .collect(),
    ))
}

/// How far back the floor looks for the last day's finished work.
const RECENT: Duration = Duration::hours(24);

/// What still needs `caller` now.
pub(crate) async fn open_needs(
    state: &GatewayState,
    sources: &Sources,
    caller: &FloorCaller,
) -> eyre::Result<Vec<Need>> {
    let now = Utc::now();
    needs_of(
        state,
        caller,
        &turns(state, sources, caller, now - RECENT).await?,
        now,
    )
}

struct Routes {
    state: Arc<GatewayState>,
    sources: Option<Sources>,
}

type S = State<Arc<Routes>>;

/// The floor's routes, answering 404 where the hub keeps no floor.
pub fn router(state: Arc<GatewayState>, sources: Option<Sources>) -> Router {
    Router::new()
        .route("/api/desk/floor", get(floor_view))
        .route("/api/desk/needs/decide", post(decide))
        .route("/api/desk/usage", get(usage))
        .with_state(Arc::new(Routes { state, sources }))
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn failed(e: eyre::Report) -> Response {
    tracing::warn!(target: "floor", error = %format!("{e:#}"), "reading the floor failed");
    refusal(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
}

/// The caller and what the floor reads from, or why there are none.
async fn reader(
    routes: &Routes,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<(FloorCaller, Sources), Response> {
    let caller = resolve_caller_context(&routes.state, headers, token).await;
    let sources = routes.sources.clone();
    let has_desk = sources.as_ref().is_some_and(|s| (s.desk)().is_some());
    let holds = holds_the_hub(
        routes.state.identity_store.as_ref(),
        has_desk,
        caller.identity_id.as_deref(),
    );
    let caller = FloorCaller::of(&caller, holds).map_err(|refused| match refused {
        Refused::Unauthorized => refusal(StatusCode::UNAUTHORIZED, "unauthorized"),
        Refused::Unavailable => refusal(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    })?;
    let sources = sources.ok_or_else(|| refusal(StatusCode::NOT_FOUND, "no_floor"))?;
    Ok((caller, sources))
}

#[derive(Deserialize)]
struct FloorQuery {
    day: Option<String>,
    token: Option<String>,
}

/// How far before now the person's day may start: a midnight in any
/// timezone, an hour longer on a day DST ends.
const DAY_REACH: Duration = Duration::hours(25);

/// How far the clock of the Desk that names the day may be from the hub's.
const CLOCK_SKEW: Duration = Duration::minutes(5);

/// The person's local midnight, read from `day`.
fn day_of(day: Option<&str>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let day = DateTime::parse_from_rfc3339(day?).ok()?.to_utc();
    (day <= now + CLOCK_SKEW && day >= now - DAY_REACH - CLOCK_SKEW).then_some(day)
}

async fn floor_view(State(r): S, headers: HeaderMap, Query(q): Query<FloorQuery>) -> Response {
    let (caller, sources) = match reader(&r, &headers, q.token.as_deref()).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let now = Utc::now();
    let Some(day) = day_of(q.day.as_deref(), now) else {
        return refusal(StatusCode::BAD_REQUEST, "bad_day");
    };
    let read = async {
        let turns = turns(&r.state, &sources, &caller, day.min(now - RECENT)).await?;
        let needs = needs_of(&r.state, &caller, &turns, now)?;
        let scope = caller.scope();
        let gists = sources.floor.store.gists(&scope).await.unwrap_or_else(|e| {
            tracing::warn!(target: "floor", error = %format!("{e:#}"), "reading the gists failed");
            floor::Gists::new()
        });
        let mut view = floor::view(&turns, needs, &gists, day, now);
        let happening = floor::happening(&view, now);
        view.summary = sources.floor.gists.summary(&scope, happening, now).await;
        eyre::Ok(view)
    };
    match read.await {
        Ok(view) => Json(view).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Choice {
    Allow,
    Deny,
}

impl Choice {
    /// The answer to a prompt this is: for this call alone, as a quick
    /// action never writes a standing rule.
    fn permission(self) -> PermissionChoice {
        match self {
            Self::Allow => PermissionChoice::AllowOnce,
            Self::Deny => PermissionChoice::DenyOnce,
        }
    }
}

#[derive(Deserialize)]
struct DecideBody {
    at: DecideRef,
    choice: Choice,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Why a decision was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// Someone answered it first, or it went away.
    AlreadyAnswered,
    NotYours,
    Unknown,
}

/// Answers the need `at` points to with `choice`, as `caller`, through the
/// function that answers it everywhere else.
pub(crate) async fn answer(
    state: &GatewayState,
    sources: &Sources,
    caller: &FloorCaller,
    at: &DecideRef,
    choice: Choice,
) -> eyre::Result<Result<(), Unanswered>> {
    match at {
        DecideRef::Prompt {
            agent_id,
            session_id,
            request_id,
        } => {
            let Some(service) = state.registry.get(agent_id) else {
                return Ok(Err(Unanswered::Unknown));
            };
            if !owns_session(service.sessions.as_ref(), session_id, &caller.id) {
                return Ok(Err(Unanswered::NotYours));
            }
            // A live turn takes any answer it is sent, so whether the prompt
            // still waits is the floor's to say.
            let waiting = open_needs(state, sources, caller)
                .await?
                .iter()
                .any(|need| matches!(&need.answer, Answer::Decide { at: open } if open == at));
            if !waiting {
                return Ok(Err(Unanswered::AlreadyAnswered));
            }
            let answered = crate::prompt_answer::answer(
                state.nats.as_ref(),
                &service,
                &caller.id,
                session_id,
                request_id,
                choice.permission(),
            )
            .await;
            Ok(answered.map_err(|why| match why {
                crate::prompt_answer::Unanswered::NotYours => Unanswered::NotYours,
                crate::prompt_answer::Unanswered::NotLive => Unanswered::AlreadyAnswered,
            }))
        }
        DecideRef::Routed { request_id } => {
            use crate::approvals::decide::{decide, DecideError, Decided, Decider};
            match decide(
                &state.registry,
                request_id,
                Decider::Person(&caller.id),
                choice.permission(),
            )
            .await
            {
                Ok(Decided::Resolved(_)) => Ok(Ok(())),
                Ok(Decided::AlreadyResolved(_)) => Ok(Err(Unanswered::AlreadyAnswered)),
                Err(DecideError::NotAnApprover) => Ok(Err(Unanswered::NotYours)),
                Err(DecideError::Unknown) => Ok(Err(Unanswered::Unknown)),
                Err(DecideError::Failed(e)) => Err(e),
            }
        }
        DecideRef::Session {
            session_id,
            approval_id,
        } => {
            if !caller.personal {
                return Ok(Err(Unanswered::NotYours));
            }
            let Some(desk) = (sources.desk)() else {
                return Ok(Err(Unanswered::Unknown));
            };
            let decision = match choice {
                Choice::Allow => Decision::Allow,
                Choice::Deny => Decision::Deny,
            };
            match desk.decide(*session_id, *approval_id, decision, Some(By::you())) {
                Ok(_) => Ok(Ok(())),
                Err(DeskError::NotFound) => Ok(Err(Unanswered::Unknown)),
                Err(DeskError::Conflict(_)) => Ok(Err(Unanswered::AlreadyAnswered)),
                Err(e) => Err(e.into()),
            }
        }
    }
}

async fn decide(
    State(r): S,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<DecideBody>,
) -> Response {
    let (caller, sources) = match reader(&r, &headers, q.token.as_deref()).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match answer(&r.state, &sources, &caller, &body.at, body.choice).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(Unanswered::AlreadyAnswered)) => refusal(StatusCode::CONFLICT, "already_answered"),
        Ok(Err(Unanswered::NotYours)) => refusal(StatusCode::FORBIDDEN, "not_yours"),
        Ok(Err(Unanswered::Unknown)) => refusal(StatusCode::NOT_FOUND, "unknown"),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Where {
    Mac,
    Hub,
}

/// Whether the person is signed in to a kind of plan, here or on this Mac.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginView {
    kind: PlanKind,
    #[serde(rename = "where")]
    at: Where,
    signed_in: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadingView {
    kind: PlanKind,
    #[serde(rename = "where")]
    at: Where,
    account: Option<String>,
    plan: Option<String>,
    windows: Vec<Window>,
    observed_at: DateTime<Utc>,
}

/// `GET /api/desk/usage`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageView {
    as_of: DateTime<Utc>,
    logins: Vec<LoginView>,
    readings: Vec<ReadingView>,
}

const PLAN_KINDS: [PlanKind; 2] = [PlanKind::Claude, PlanKind::Chatgpt];

/// Whether a reading is of `caller`'s plan: on their own hub every login it
/// holds and this Mac's are; elsewhere only their own per-user logins, as
/// one an agent or the hub shares is not their plan.
fn plan_scope(login: &LoginRef, caller: &FloorCaller) -> bool {
    caller.personal || matches!(login, LoginRef::Hub { owner: Some(o), .. } if *o == caller.id)
}

/// The kinds of plan `caller` holds a login to among `keys`, a vault's.
fn signed_in(keys: impl IntoIterator<Item = String>, caller: &FloorCaller) -> HashSet<PlanKind> {
    keys.into_iter()
        .filter(|key| {
            caller.personal || split_per_user_key(key).is_some_and(|(uid, _)| uid == caller.id)
        })
        .filter_map(|key| {
            SubscriptionKind::of_vault_key(&key).map(|kind| match kind {
                SubscriptionKind::Claude => PlanKind::Claude,
                SubscriptionKind::ChatGpt => PlanKind::Chatgpt,
            })
        })
        .collect()
}

/// What `caller` is shown of their plans: their logins here, this Mac's
/// sign-ins (`mac`, on their own hub), and each reading of theirs.
fn usage_of(
    readings: Vec<Reading>,
    held: &HashSet<PlanKind>,
    mac: Option<&SignIns>,
    caller: &FloorCaller,
    now: DateTime<Utc>,
) -> UsageView {
    let hub = PLAN_KINDS.iter().map(|&kind| LoginView {
        kind,
        at: Where::Hub,
        signed_in: held.contains(&kind),
    });
    let mac = mac.into_iter().flat_map(|m| {
        [
            LoginView {
                kind: PlanKind::Claude,
                at: Where::Mac,
                signed_in: m.of(Engine::Claude).on_plan(),
            },
            LoginView {
                kind: PlanKind::Chatgpt,
                at: Where::Mac,
                signed_in: m.of(Engine::Codex).on_plan(),
            },
        ]
    });
    UsageView {
        as_of: now,
        logins: hub.chain(mac).collect(),
        readings: readings
            .into_iter()
            .filter(|r| plan_scope(&r.login, caller))
            .map(|r| ReadingView {
                kind: r.kind,
                at: match r.login {
                    LoginRef::Mac => Where::Mac,
                    LoginRef::Hub { .. } => Where::Hub,
                },
                account: r.account,
                plan: r.plan,
                windows: r.windows,
                observed_at: r.observed_at,
            })
            .collect(),
    }
}

async fn usage(State(r): S, headers: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    let (caller, sources) = match reader(&r, &headers, q.token.as_deref()).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    let readings = match sources.floor.store.readings().await {
        Ok(readings) => readings,
        Err(e) => return failed(e),
    };
    let keys = crate::gateway::login_vaults(&r.state)
        .iter()
        .flat_map(|v| v.list(None))
        .collect::<Vec<_>>();
    let mac = caller
        .personal
        .then(|| (sources.desk)())
        .flatten()
        .map(|d| d.sign_ins());
    Json(usage_of(
        readings,
        &signed_in(keys, &caller),
        mac.as_ref(),
        &caller,
        Utc::now(),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin<'a>(channel: &'a str, user: &'a str, message: &'a str) -> Origin<'a> {
        Origin {
            channel,
            user,
            run_as: None,
            message,
            channel_id: None,
            thread_id: None,
            desk: None,
            answers_to: None,
        }
    }

    #[test]
    fn a_turn_is_found_where_it_came_in() {
        assert_eq!(place_of(&origin("desk", "u1", "hi")), Place::Dm);
        let slack = Origin {
            channel_id: Some("C1"),
            thread_id: Some("1700.1"),
            ..origin("slack:T1:C1:1700.1", "u1", "hi")
        };
        assert_eq!(
            place_of(&slack),
            Place::Slack {
                team: Some("T1".into()),
                channel: "C1".into(),
                thread: Some("1700.1".into())
            }
        );
        assert_eq!(
            place_of(&origin("scheduler", "scheduler", "run")),
            Place::Elsewhere
        );
        assert_eq!(place_of(&origin("ios", "u1", "hi")), Place::Elsewhere);
    }

    fn asked_from(place: DeskPlace, speaker: DeskSpeaker, channel: &str) -> DeskTurnContext {
        DeskTurnContext::new(
            workforce_common::channels::desk::DeskTurn {
                place,
                speaker,
                hop: 0,
                context: vec![],
                message: None,
                spoken: false,
                voice: None,
            },
            channel,
            None,
            false,
            None,
            "reply-1".into(),
        )
    }

    #[test]
    fn a_desk_turn_is_found_in_its_room_thread_or_call() {
        let (room, thread) = (Uuid::new_v4(), Uuid::new_v4());
        let channel = format!("desk:{room}");
        let in_thread = asked_from(
            DeskPlace::Room {
                room,
                name: "Release".into(),
                thread: Some(thread),
            },
            DeskSpeaker::Person,
            &channel,
        );
        let at = |desk: &DeskTurnContext, channel: &str| {
            place_of(&Origin {
                desk: Some(desk),
                ..origin(channel, "u1", "hi")
            })
        };
        assert_eq!(
            at(&in_thread, &channel),
            Place::Room {
                room,
                thread: Some(thread)
            }
        );
        let call = asked_from(DeskPlace::Call, DeskSpeaker::Person, "desk:tuple");
        assert_eq!(at(&call, "desk:tuple"), Place::Call);
        let legacy = asked_from(
            DeskPlace::LegacyRoom {
                slug: "release".into(),
            },
            DeskSpeaker::Person,
            "desk:release",
        );
        assert_eq!(
            at(&legacy, "desk:release"),
            Place::Dm,
            "a room known only by its slug is answered in the DM"
        );
        let job = DeskPlace::Room {
            room,
            name: "Release".into(),
            thread: None,
        };
        assert_eq!(
            place_of(&Origin {
                answers_to: Some(&job),
                ..origin("scheduler:j1", "scheduler", "report")
            }),
            Place::Room { room, thread: None },
            "a job made in a room is found there"
        );
    }

    #[test]
    fn another_agents_hand_off_in_a_room_is_not_the_persons() {
        let room = Uuid::new_v4();
        let channel = format!("desk:{room}");
        let place = || DeskPlace::Room {
            room,
            name: "Release".into(),
            thread: None,
        };
        let handed = asked_from(
            place(),
            DeskSpeaker::Agent {
                name: "Scout".into(),
            },
            &channel,
        );
        let asked = asked_from(place(), DeskSpeaker::Person, &channel);
        let by = |desk: &DeskTurnContext| {
            by_person_of(&Origin {
                desk: Some(desk),
                ..origin(&channel, "u1", "over to you")
            })
        };
        assert!(!by(&handed));
        assert!(by(&asked));
    }

    #[test]
    fn only_the_persons_own_message_is_theirs() {
        assert!(by_person_of(&origin("desk", "u1", "deploy it")));
        assert!(
            by_person_of(&origin("ios", "u1", "deploy it")),
            "a plain /ws message"
        );
        assert!(by_person_of(&origin("slack:T1:C1:1", "u1", "deploy it")));
        assert!(!by_person_of(&origin(
            "scheduler",
            "scheduler",
            "daily report"
        )));
        assert!(!by_person_of(&origin(
            "workflow",
            "workflow-engine",
            "step 2"
        )));
        assert!(!by_person_of(&origin("desk", "agent:scout", "over to you")));
        assert!(!by_person_of(&origin("desk", "u1", RESUME_PROMPT)));
        assert!(
            !by_person_of(&origin(
                "desk",
                "u1",
                &format!("{APPROVAL_DIRECTIVE}Ada approved your request")
            )),
            "a turn resumed on an approval is not the person speaking"
        );
        assert!(!by_person_of(&origin(
            "desk",
            "u1",
            &format!("{CHECK_IN} anything stuck?")
        )));
    }

    #[test]
    fn the_owner_is_who_it_runs_as() {
        let job = Origin {
            run_as: Some("u1"),
            ..origin("scheduler", "scheduler", "run")
        };
        assert_eq!(job.owner(), "u1");
        assert_eq!(origin("desk", "u2", "hi").owner(), "u2");
    }

    fn open_turn(subject: Subject, token: Option<&str>, instance: Option<&str>) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            owner: Some("u1".into()),
            subject,
            name: "Scout".into(),
            place: Place::Dm,
            task: None,
            doing: None,
            started_at: Utc::now(),
            heard_at: Utc::now(),
            ended_at: None,
            outcome: None,
            answer: None,
            question: None,
            settled_at: None,
            waits: vec![],
            idle: vec![],
            token: token.map(str::to_string),
            instance: instance.map(str::to_string),
        }
    }

    #[test]
    fn a_turn_runs_while_what_runs_it_does() {
        let agent = || Subject::Agent {
            agent_id: "scout".into(),
            session_id: "s1".into(),
        };
        let running = Running {
            tokens: HashMap::from([("s1".to_string(), "tok-2".to_string())]),
            alive: HashSet::from(["hub-1".to_string()]),
            desk: HashSet::new(),
        };
        assert!(runs(
            &open_turn(agent(), Some("tok-2"), Some("hub-1")),
            &running
        ));
        assert!(
            !runs(&open_turn(agent(), Some("tok-1"), Some("hub-1")), &running),
            "the session has moved on to another turn"
        );
        assert!(runs(&open_turn(agent(), None, Some("hub-1")), &running));
        assert!(
            !runs(&open_turn(agent(), None, Some("hub-dead")), &running),
            "its instance crashed"
        );
        assert!(
            !runs(&open_turn(agent(), None, None), &running),
            "nothing runs it"
        );
        let session = open_turn(
            Subject::Session {
                id: Uuid::new_v4(),
                engine: workforce_desk::model::Engine::Claude,
            },
            None,
            None,
        );
        assert!(!runs(&session, &running));
        let desk = Running {
            desk: HashSet::from([session.id]),
            ..Running::default()
        };
        assert!(runs(&session, &desk));
    }

    #[test]
    fn a_gone_agent_s_question_is_no_one_s_and_its_work_keeps_its_name() {
        let agent = |id: &str| Subject::Agent {
            agent_id: id.into(),
            session_id: "s1".into(),
        };
        let asked = |subject| Turn {
            ended_at: Some(Utc::now()),
            outcome: Some(workforce_desk::floor::Outcome::Asked),
            ..open_turn(subject, None, None)
        };
        let done = Turn {
            ended_at: Some(Utc::now()),
            outcome: Some(workforce_desk::floor::Outcome::Finished),
            ..open_turn(agent("gone"), None, None)
        };
        let kept = named(
            vec![asked(agent("scout")), asked(agent("gone")), done.clone()],
            |subject| match subject {
                Subject::Agent { agent_id, .. } if agent_id == "scout" => Some("Scout 2".into()),
                _ => None,
            },
        );
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].name, "Scout 2");
        assert_eq!(kept[1], done);
    }

    #[test]
    fn the_floor_is_read_for_a_person_and_personal_on_their_own_hub() {
        let caller = |identity: Option<&str>, device: Option<&str>, admin: bool| CallerContext {
            identity_id: identity.map(str::to_string),
            device_id: device.map(str::to_string),
            is_admin: admin,
            scopes: vec![],
            auth_failure: None,
        };
        assert_eq!(
            FloorCaller::of(&caller(Some("u1"), Some("d1"), false), true),
            Ok(FloorCaller {
                id: "u1".into(),
                personal: true
            }),
            "the device of the person who holds the hub"
        );
        assert_eq!(
            FloorCaller::of(&caller(None, Some("d1"), false), false),
            Ok(FloorCaller {
                id: "d1".into(),
                personal: false
            }),
            "a device-only pairing"
        );
        assert_eq!(
            FloorCaller::of(&caller(None, None, true), true),
            Err(Refused::Unauthorized),
            "the admin token is no one's"
        );
        let failed = CallerContext {
            auth_failure: Some(AuthFailure::StoreError),
            ..caller(Some("u1"), None, false)
        };
        assert_eq!(FloorCaller::of(&failed, true), Err(Refused::Unavailable));
    }

    #[test]
    fn a_desk_hub_is_held_by_whoever_administers_it_however_they_connect() {
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};
        let store = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        let member = Identity::new("Member", IdentityRole::Member);
        let suspended = Identity::new("Former", IdentityRole::SystemAdmin)
            .with_status(IdentityStatus::Suspended);
        for i in [&admin, &member, &suspended] {
            store.create(i).unwrap();
        }
        let store: Arc<dyn IdentityStore> = Arc::new(store);
        let holds = |desk, who: Option<&Identity>| {
            holds_the_hub(Some(&store), desk, who.map(|i| i.id.to_string()).as_deref())
        };
        assert!(holds(true, Some(&admin)));
        assert!(
            !holds(false, Some(&admin)),
            "a hub without Desk has no sessions to hold"
        );
        assert!(!holds(true, Some(&member)));
        assert!(!holds(true, Some(&suspended)));
        assert!(!holds(true, None), "a device paired to nobody");
        assert!(!holds_the_hub(None, true, Some(&admin.id.to_string())));
    }

    #[test]
    fn the_day_is_a_recent_midnight() {
        let now = DateTime::parse_from_rfc3339("2026-10-29T16:42:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            day_of(Some("2026-10-29T00:00:00+01:00"), now),
            Some(
                DateTime::parse_from_rfc3339("2026-10-28T23:00:00Z")
                    .unwrap()
                    .to_utc()
            )
        );
        assert_eq!(day_of(None, now), None);
        assert_eq!(day_of(Some("yesterday"), now), None);
        assert_eq!(day_of(Some("2026-10-30T00:00:00Z"), now), None, "tomorrow");
        assert_eq!(day_of(Some("2026-10-26T00:00:00Z"), now), None, "days ago");
        assert_eq!(
            day_of(Some("2026-10-28T00:00:00Z"), now),
            None,
            "yesterday's midnight"
        );
        assert!(
            day_of(Some("2026-10-29T16:44:00Z"), now).is_some(),
            "a Desk whose clock runs a little ahead"
        );
    }

    fn reading(login: LoginRef) -> Reading {
        Reading {
            kind: PlanKind::Claude,
            login,
            account: None,
            plan: None,
            windows: vec![],
            observed_at: Utc::now(),
        }
    }

    #[test]
    fn a_plan_is_the_persons_on_their_hub_and_their_own_logins_elsewhere() {
        let mine = |personal| FloorCaller {
            id: "u1".into(),
            personal,
        };
        let hub = |owner: Option<&str>| LoginRef::Hub {
            login: "k".into(),
            owner: owner.map(str::to_string),
        };
        for (login, personal, shown) in [
            (LoginRef::Mac, true, true),
            (hub(None), true, true),
            (hub(Some("u2")), true, true),
            (LoginRef::Mac, false, false),
            (hub(None), false, false),
            (hub(Some("u2")), false, false),
            (hub(Some("u1")), false, true),
        ] {
            assert_eq!(
                plan_scope(&login, &mine(personal)),
                shown,
                "{login:?} {personal}"
            );
        }
        let keys = || {
            [
                "claude_code/oauth_token",
                "users/u2/codex/auth_json",
                "users/u1/claude_code/oauth_token",
                "github/token",
            ]
            .map(str::to_string)
        };
        assert_eq!(
            signed_in(keys(), &mine(false)),
            HashSet::from([PlanKind::Claude])
        );
        assert_eq!(
            signed_in(keys(), &mine(true)),
            HashSet::from([PlanKind::Claude, PlanKind::Chatgpt])
        );
    }

    #[test]
    fn usage_has_the_wire_shape_desk_reads() {
        let at = DateTime::parse_from_rfc3339("2026-10-29T16:42:00Z")
            .unwrap()
            .to_utc();
        let mac = Reading {
            account: Some("3f1c".into()),
            plan: Some("max".into()),
            windows: vec![Window {
                used_percent: 62.0,
                resets_at: Some(at),
                minutes: 300,
            }],
            observed_at: at,
            ..reading(LoginRef::Mac)
        };
        let view = usage_of(
            vec![mac],
            &HashSet::from([PlanKind::Claude]),
            Some(&SignIns {
                by_engine: [(
                    Engine::Claude,
                    workforce_desk::engine::claude_sign_in(
                        r#"{"loggedIn":true,"authMethod":"oauth_token"}"#,
                    ),
                )]
                .into(),
            }),
            &FloorCaller {
                id: "u1".into(),
                personal: true,
            },
            at,
        );
        assert_eq!(
            serde_json::to_value(view).unwrap(),
            json!({
                "asOf": "2026-10-29T16:42:00Z",
                "logins": [
                    {"kind": "claude", "where": "hub", "signedIn": true},
                    {"kind": "chatgpt", "where": "hub", "signedIn": false},
                    {"kind": "claude", "where": "mac", "signedIn": true},
                    {"kind": "chatgpt", "where": "mac", "signedIn": false}
                ],
                "readings": [{
                    "kind": "claude", "where": "mac", "account": "3f1c", "plan": "max",
                    "windows": [{"usedPercent": 62.0, "resetsAt": "2026-10-29T16:42:00Z", "minutes": 300}],
                    "observedAt": "2026-10-29T16:42:00Z"
                }]
            })
        );
    }
}

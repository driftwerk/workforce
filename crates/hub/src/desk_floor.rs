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
use workforce_common::channels::{is_platform, DESK_CHANNEL};
use workforce_common::subscription::{is_automated_source, split_per_user_key, SubscriptionKind};
use workforce_common::usage::{LoginRef, PlanKind, Reading, Window};
use workforce_desk::crew::By;
use workforce_desk::floor::{self, DecideRef, Need, Place, Scope, Subject, Turn};
use workforce_desk::model::Decision;
use workforce_desk::usage::MacSignIns;
use workforce_desk::{Desk, DeskError};

use crate::desk_crew::CHECK_IN;
use crate::floor::Installed;
use crate::gateway::{resolve_caller_context, AuthFailure, CallerContext, GatewayState};
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
}

impl Origin<'_> {
    /// Whose work it is: who it runs as, else who sent it.
    pub(crate) fn owner(&self) -> &str {
        self.run_as.unwrap_or(self.user)
    }
}

/// Where a turn, and anything it asks, can be found.
pub(crate) fn place_of(o: &Origin<'_>) -> Place {
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
/// hand-off, Desk's own check-in, or a turn resumed after a restart.
pub(crate) fn by_person_of(o: &Origin<'_>) -> bool {
    !is_automated_source(o.user)
        && !o.user.starts_with("agent:")
        && !o.user.starts_with("system")
        && !o.message.starts_with(RESUME_PROMPT)
        && !o.message.starts_with(CHECK_IN)
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
    fn of(caller: &CallerContext, has_desk: bool) -> Result<Self, Refused> {
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
            personal: has_desk && caller.is_admin,
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
/// the instances known to have crashed, and the Desk turns open now.
#[derive(Debug, Default)]
struct Running {
    tokens: HashMap<String, String>,
    stale: HashSet<String>,
    desk: HashSet<Uuid>,
}

/// Whether an open turn still runs. An agent's turn with a session record
/// runs while that record's token is its own on a live instance; one
/// without runs while its instance has not crashed. A Desk session's runs
/// while the desk has it open.
fn runs(t: &Turn, r: &Running) -> bool {
    match &t.subject {
        Subject::Session { .. } => r.desk.contains(&t.id),
        Subject::Agent { session_id, .. } => match &t.token {
            Some(token) => r.tokens.get(session_id) == Some(token),
            None => t.instance.as_ref().is_none_or(|i| !r.stale.contains(i)),
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
    Ok(Running {
        tokens,
        stale: sources.floor.store.stale(&instances).await?,
        desk: (sources.desk)().map(|d| d.open_turns()).unwrap_or_default(),
    })
}

/// `turns` named as their agents and sessions are called now.
fn named(turns: Vec<Turn>, name: impl Fn(&Subject) -> Option<String>) -> Vec<Turn> {
    turns
        .into_iter()
        .map(|t| Turn {
            name: name(&t.subject).unwrap_or(t.name.clone()),
            ..t
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

/// What still needs the person: the one source every count is a count of.
fn needs_of(turns: &[Turn]) -> Vec<Need> {
    floor::open_needs(turns)
}

/// How far back the floor looks for the last day's finished work.
const RECENT: Duration = Duration::hours(24);

/// What still needs `caller` now.
pub(crate) async fn open_needs(
    state: &GatewayState,
    sources: &Sources,
    caller: &FloorCaller,
) -> eyre::Result<Vec<Need>> {
    let since = Utc::now() - RECENT;
    Ok(needs_of(&turns(state, sources, caller, since).await?))
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
    let caller = FloorCaller::of(&caller, has_desk).map_err(|refused| match refused {
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
/// timezone, a day longer for DST.
const DAY_REACH: Duration = Duration::hours(48);

/// The person's local midnight, read from `day`.
fn day_of(day: Option<&str>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let day = DateTime::parse_from_rfc3339(day?).ok()?.to_utc();
    (day <= now && day >= now - DAY_REACH).then_some(day)
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
    match turns(&r.state, &sources, &caller, day.min(now - RECENT)).await {
        Ok(turns) => Json(floor::view(&turns, needs_of(&turns), day, now)).into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Choice {
    Allow,
    Deny,
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
pub(crate) fn answer(
    sources: &Sources,
    caller: &FloorCaller,
    at: &DecideRef,
    choice: Choice,
) -> eyre::Result<Result<(), Unanswered>> {
    let decision = match choice {
        Choice::Allow => Decision::Allow,
        Choice::Deny => Decision::Deny,
    };
    match at {
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
    match answer(&sources, &caller, &body.at, body.choice) {
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
    mac: Option<&MacSignIns>,
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
                signed_in: m.claude.is_some(),
            },
            LoginView {
                kind: PlanKind::Chatgpt,
                at: Where::Mac,
                signed_in: m.codex,
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
            stale: HashSet::from(["hub-dead".to_string()]),
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
    fn the_floor_is_read_for_a_person_and_personal_on_their_own_hub() {
        let caller = |identity: Option<&str>, device: Option<&str>, admin: bool| CallerContext {
            identity_id: identity.map(str::to_string),
            device_id: device.map(str::to_string),
            is_admin: admin,
            scopes: vec![],
            auth_failure: None,
        };
        assert_eq!(
            FloorCaller::of(&caller(Some("u1"), Some("d1"), true), true),
            Ok(FloorCaller {
                id: "u1".into(),
                personal: true
            })
        );
        assert_eq!(
            FloorCaller::of(&caller(None, Some("d1"), true), false),
            Ok(FloorCaller {
                id: "d1".into(),
                personal: false
            }),
            "a device-only pairing, on a hub without Desk"
        );
        assert_eq!(
            FloorCaller::of(&caller(Some("u1"), None, false), true).map(|c| c.personal),
            Ok(false)
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
            Some(&MacSignIns {
                claude: Some(Default::default()),
                codex: false,
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

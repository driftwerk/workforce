//! The relay (remote bosses, RB3): a local overseer on the person's Mac
//! reaches its group's sessions on one of their machines through Desk's
//! native side, which holds the machine's SSH forward. Both ends are here:
//!
//! - **On the Mac**, [`CrewHub::relay`] queues the call and waits for its
//!   answer. Desk's native side takes queued calls from
//!   `GET /api/desk/crew/relay` (a long poll), sends each to its machine,
//!   and posts the machine's answer to `POST /api/desk/crew/relay/{id}`,
//!   once.
//! - **On the machine**, `POST /api/desk/crew/served` hands the call to
//!   [`CrewHub::serve_crew`], which decides it under the machine's grant
//!   for the group, as any served boss's call is.
//!
//! Every route here is the hub's own token's alone, from the
//! `Authorization` header alone (`?token=` is refused), as grants are:
//! Desk's native side holds it, its page can't ask these routes (they are
//! on none of the broker's lists), and nothing is relayed through the
//! page's own socket, `/ws/desk`, so a page can never make, change, drop or
//! answer a boss's call. A call relayed carries the boss of the overseer
//! that made it, never one its input names; a link (`home-relay`) and an
//! id of its own, so a call sent again is never acted on twice; and only a
//! boss on the home's own hub (`local:<agent>`) comes this way: one on
//! another hub comes only on its own uplink.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::grant::BossRef;

use super::served::{ServedCall, ServedStatus};
use super::CrewHub;
use crate::gateway::GatewayState;
use workforce_desk::sees;

/// The link every relayed call comes on.
pub const RELAY_LINK: &str = "home-relay";
/// The most calls waiting to be taken at once.
const MOST_QUEUED: usize = 64;
/// The most calls waiting for their answers at once, taken or not.
const MOST_WAITING: usize = 128;
/// The most calls one poll takes.
const MOST_TAKEN: usize = 16;
/// The largest answer a machine's call may post.
const MOST_ANSWER: usize = 256 * 1024;
/// How long a poll waits for a call when none is queued.
const POLL_FOR: Duration = Duration::from_secs(25);
/// The largest input a call is queued with, as JSON: a poll's batch then
/// always fits what Desk's native side reads of it.
const MOST_INPUT: usize = 256 * 1024;

/// One call queued for a machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayCall {
    /// Its id, and the id the machine records it by.
    pub id: Uuid,
    /// The machine it is for, as the sidebar names it.
    pub machine: String,
    pub group: String,
    pub tool: String,
    pub input: Value,
    /// `local:<agent>`: the overseer that made it.
    pub boss: String,
    pub link_id: &'static str,
    /// The overseer's own level and switches here: the machine decides at
    /// no more than these and its grant's.
    pub level: workforce_common::oversight::Level,
    pub act: workforce_common::oversight::Act,
    /// When this hub stops waiting for its answer: Desk's native side sends
    /// nothing it can't carry by then, so a call the boss was told may
    /// not have been done is never done later. Not sent to the machine.
    pub deadline: chrono::DateTime<chrono::Utc>,
}

/// What a machine answered.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayAnswer {
    pub status: ServedStatus,
    #[serde(default)]
    pub body: Value,
}

/// The Mac's relay: what waits to be taken, and who waits for each answer.
#[derive(Default)]
pub(crate) struct Relays {
    queued: VecDeque<(RelayCall, Instant)>,
    waiting: HashMap<Uuid, tokio::sync::oneshot::Sender<RelayAnswer>>,
    /// Calls Desk's native side took: they may have been done.
    taken: std::collections::HashSet<Uuid>,
}

/// Whether `name` can name a machine: no `/`, `:` or space in it.
pub fn machine_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `id`, a session id as a tool was given it, split when it names a
/// machine's session: `devbox/9f3c…`.
pub fn machine_session(id: &str) -> Option<(&str, Uuid)> {
    let (machine, rest) = id.split_once('/')?;
    Some((machine, rest.parse().ok()?)).filter(|(m, _)| machine_ok(m))
}

impl CrewHub {
    /// Sends `tool` with `input` for the overseer `agent` to `machine`'s
    /// sessions of `group`, through Desk's native side, and waits for the
    /// machine's answer; when none comes in time, says so.
    pub(crate) async fn relay(
        &self,
        machine: &str,
        group: &str,
        tool: &str,
        input: Value,
        agent: &str,
        ceiling: &super::served::Ceiling,
    ) -> Result<RelayAnswer, String> {
        let unreached = || {
            format!(
                "{machine} can't be reached: Desk isn't open on this Mac, or {machine} isn't connected."
            )
        };
        // No one takes calls: nothing is queued to wait on.
        let polled = self
            .relay_polled
            .lock()
            .is_some_and(|at| at.elapsed() < POLL_FOR * 2);
        if !polled {
            return Err(unreached());
        }
        // Too large to carry: never queued, so nothing of it is done.
        if input.to_string().len() > MOST_INPUT {
            return Err(format!(
                "That call is too large to send to {machine}; send less in it."
            ));
        }
        let id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let wait = *self.relay_wait.lock();
        let deadline = chrono::Utc::now()
            + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::seconds(30));
        {
            let mut r = self.relays.lock();
            r.queued.retain(|(_, at)| at.elapsed() < wait);
            if r.queued.len() >= MOST_QUEUED || r.waiting.len() >= MOST_WAITING {
                return Err(unreached());
            }
            r.queued.push_back((
                RelayCall {
                    id,
                    machine: machine.to_string(),
                    group: group.to_string(),
                    tool: tool.to_string(),
                    input,
                    boss: BossRef::Local {
                        agent: agent.to_string(),
                    }
                    .key(),
                    link_id: RELAY_LINK,
                    level: ceiling.level,
                    act: ceiling.act,
                    deadline,
                },
                Instant::now(),
            ));
            r.waiting.insert(id, tx);
        }
        self.relay_queued.notify_waiters();
        let wait = *self.relay_wait.lock();
        let got = tokio::time::timeout(wait, rx).await;
        // Whatever came of it, nothing waits for it any more, and a call
        // no one took goes.
        let taken = {
            let mut r = self.relays.lock();
            r.waiting.remove(&id);
            r.queued.retain(|(c, _)| c.id != id);
            r.taken.remove(&id)
        };
        match got {
            Ok(Ok(answer)) => Ok(answer),
            // Desk took it: it may have been done there. Not to be sent
            // again as new.
            _ if taken => Err(format!(
                "Sent to {machine}, but its outcome isn't known yet: check {machine}'s session (crew.history) before you try it again."
            )),
            _ => Err(unreached()),
        }
    }

    /// The calls queued for machines, up to [`MOST_TAKEN`]: each taken
    /// once. Waits up to `wait` for one when none is queued.
    pub(crate) async fn relay_take(&self, wait: Duration) -> Vec<RelayCall> {
        *self.relay_polled.lock() = Some(Instant::now());
        let take = |crew: &Self| {
            let mut r = crew.relays.lock();
            let mut out = Vec::new();
            while out.len() < MOST_TAKEN {
                match r.queued.pop_front() {
                    Some((c, _)) if r.waiting.contains_key(&c.id) => {
                        r.taken.insert(c.id);
                        out.push(c);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            out
        };
        let notified = self.relay_queued.notified();
        let now = take(self);
        if !now.is_empty() {
            return now;
        }
        let _ = tokio::time::timeout(wait, notified).await;
        *self.relay_polled.lock() = Some(Instant::now());
        take(self)
    }

    /// The machine's answer to call `id`: taken once, by the call that
    /// waits for it; one for a call no one waits for (late, or answered
    /// already) is dropped.
    pub(crate) fn relay_answer(&self, id: Uuid, answer: RelayAnswer) -> bool {
        match self.relays.lock().waiting.remove(&id) {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

/// The machine's parts of an overview, beside this Mac's: each machine's
/// sessions as it said them, or why it said nothing. Nothing of a part is
/// changed or raised; each says which machine it is.
pub fn merge_overview(
    mut local: Value,
    parts: Vec<(String, Result<RelayAnswer, String>)>,
) -> Value {
    let machines: Vec<Value> = parts
        .into_iter()
        .map(|(machine, part)| match part {
            Ok(a) if a.status == ServedStatus::Ok => json!({
                "machine": machine,
                "sessions": super::watcher::reported(
                    &sees::capped(
                        &a.body.get("sessions").map_or_else(String::new, Value::to_string),
                        sees::MAX_CHARS,
                    ),
                    &format!("{machine}'s sessions"),
                ),
                "ids": format!("Name them as {machine}/<id>."),
            }),
            Ok(a) => json!({
                "machine": machine,
                "not_read": super::watcher::reported(
                    &sees::capped(
                        &a.body.get("error").map_or_else(String::new, Value::to_string),
                        2000,
                    ),
                    &format!("{machine}'s answer"),
                ),
            }),
            Err(why) => json!({ "machine": machine, "not_read": why }),
        })
        .collect();
    if !machines.is_empty() {
        local["machines"] = Value::Array(machines);
    }
    local
}

// ── Routes ──────────────────────────────────────────────────────────────

struct RelayApi {
    state: Arc<GatewayState>,
    crew: Arc<CrewHub>,
}

type S = State<Arc<RelayApi>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Why a request isn't Desk's native side's, when it isn't.
fn not_native(s: &RelayApi, h: &HeaderMap, q: &TokenQuery) -> Option<Response> {
    if q.token.is_some() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "the relay takes the hub's token in the Authorization header only"})),
            )
                .into_response(),
        );
    }
    let ours = h
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| {
            !s.state.auth_token.is_empty()
                && crate::auth::constant_time_token_eq(given, &s.state.auth_token)
        });
    // 401: refused before anything is done, so Desk's native side may read
    // the hub's token again and send it once more.
    (!ours).then(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "only Desk itself relays a boss's calls, with the hub's own token"})),
        )
            .into_response()
    })
}

macro_rules! native {
    ($s:expr, $h:expr, $q:expr) => {
        if let Some(r) = not_native(&$s, &$h, &$q) {
            return r;
        }
    };
}

/// On the Mac: the calls queued for machines.
async fn take(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    native!(s, h, q);
    Json(s.crew.relay_take(POLL_FOR).await).into_response()
}

/// On the Mac: a machine's answer to one call, once.
async fn answer(
    State(s): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Response {
    native!(s, h, q);
    if body.len() > MOST_ANSWER {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "that answer is too large"})),
        )
            .into_response();
    }
    let Ok(a) = serde_json::from_slice::<RelayAnswer>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "that isn't a relayed answer"})),
        )
            .into_response();
    };
    match s.crew.relay_answer(id, a) {
        true => StatusCode::NO_CONTENT.into_response(),
        false => (
            StatusCode::GONE,
            Json(json!({"error": "no call waits for that answer"})),
        )
            .into_response(),
    }
}

/// What Desk's native side sends a machine.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Relayed {
    group: String,
    tool: String,
    #[serde(default)]
    input: Value,
    boss: String,
    link_id: String,
    id: String,
    /// The boss's own level and switches at its home.
    level: String,
    #[serde(default)]
    act: Value,
}

/// On the machine: a boss's call relayed from the home, decided under the
/// machine's grant for its group.
async fn served(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>, body: Bytes) -> Response {
    native!(s, h, q);
    let bad = |why: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": why}))).into_response();
    let Ok(r) = serde_json::from_slice::<Relayed>(&body) else {
        return bad("that isn't a relayed call");
    };
    // Only the home's own boss comes this way.
    let boss = match BossRef::parse(&r.boss) {
        Ok(b @ BossRef::Local { .. }) => b,
        _ => return bad("only a boss on the home's own hub is relayed"),
    };
    // On the home's relay link, with its id, always: one sent again is
    // never done twice.
    if r.link_id != RELAY_LINK || r.id.trim().is_empty() || r.id.len() > 100 {
        return bad("a relayed call needs the home's link and its id");
    }
    use workforce_common::oversight::Level;
    let level = match r.level.as_str() {
        "watch" => Some(Level::Watch),
        "suggest" => Some(Level::Suggest),
        "act" => Some(Level::Act),
        "full" => Some(Level::Full),
        _ => None,
    };
    let Some(level) = level else {
        return bad("a relayed call says its boss's level");
    };
    if !workforce_desk::grant::group_id_ok(&r.group) {
        return bad("that can't be a group");
    }
    let out = s
        .crew
        .serve_crew(
            ServedCall {
                tool: r.tool,
                input: r.input,
                boss,
                link_id: Some(RELAY_LINK.to_string()),
                call_id: Some(r.id),
                ceiling: Some(super::served::Ceiling {
                    level,
                    act: super::served::act_of(&r.act),
                }),
            },
            &r.group,
        )
        .await;
    Json(json!({ "status": out.status, "body": out.body })).into_response()
}

pub(crate) fn routes(state: Arc<GatewayState>, crew: Arc<CrewHub>) -> Router {
    Router::new()
        .route("/api/desk/crew/relay", get(take))
        .route("/api/desk/crew/relay/{id}", post(answer))
        .route("/api/desk/crew/served", post(served))
        .with_state(Arc::new(RelayApi { state, crew }))
}

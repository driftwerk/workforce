//! The relay (remote bosses, RB3): a local overseer on the person's Mac
//! reaches its group's sessions on one of their machines, or the main
//! overseer its plans' ticket sessions there, through Desk's native side,
//! which holds the machine's SSH forward. Both ends are here:
//!
//! - **On the Mac**, [`CrewHub::relay`] queues the call and waits for its
//!   answer. Desk's native side takes queued calls from
//!   `GET /api/desk/crew/relay` (a long poll), sends each to its machine,
//!   and posts the machine's answer to `POST /api/desk/crew/relay/{id}`,
//!   once.
//! - **On the machine**, `POST /api/desk/crew/served` hands the call to
//!   [`CrewHub::serve_crew`], which decides it under the machine's grant
//!   for the group (or the plan), as any served boss's call is.
//! - **That grant** is what the Mac says at `GET /api/desk/machine-grants`
//!   ([`super::machine_grants`]), which Desk's native side carries to each
//!   machine.
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

use std::sync::Arc;
use std::time::Duration;

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
use crate::desk_pickup::{Limits, Pickup, Ticketed, Unsent};
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
/// The largest call queued, as JSON, its input and all: what Desk's native
/// side carries of one (its `MOST_BODY`), so a poll's batch of
/// [`MOST_TAKEN`] also fits what it reads of a poll (`MOST_POLL`).
const MOST_CALL: usize = 256 * 1024;

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
    /// Its switches, and whether it takes on every session
    /// ([`super::served::Ceiling::relayed_act`]).
    pub act: Value,
    /// How long, in milliseconds as it is taken, this hub still waits for
    /// its answer: Desk's native side sends nothing it can't carry in that
    /// time, so a call the boss was told may not have been done is never
    /// done later. Desk leaves it out of what it sends the machine, whose
    /// `served` route refuses a field it doesn't know.
    pub left_ms: u64,
    /// What the boss's turn making it had read of outside content, when it
    /// had: what it says to a session there carries that in. Said only
    /// then, so a call of a turn that read none is as a machine older than
    /// this takes it, and one that had read some is refused there rather
    /// than taken as carrying nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside: Option<String>,
}

impl RelayCall {
    /// `agent`'s call of `tool` to `machine`'s sessions of `group`, at
    /// `ceiling`.
    fn of(
        machine: &str,
        group: &str,
        tool: &str,
        input: Value,
        agent: &str,
        ceiling: &super::served::Ceiling,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
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
            act: ceiling.relayed_act(),
            left_ms: 0,
            outside: None,
        }
    }

    /// Whether it is small enough to carry ([`MOST_CALL`]), however long is
    /// left to wait for it when it is taken ([`Ticketed::left`]).
    fn carried(&self) -> bool {
        let most_left = u64::MAX.to_string().len() - self.left_ms.to_string().len();
        serde_json::to_vec(self).is_ok_and(|b| b.len() + most_left <= MOST_CALL)
    }
}

/// Whether `input` is small enough to carry in `agent`'s call of `tool` to
/// `machine`'s sessions of `group`: [`CrewHub::relay_as`] refuses one that
/// isn't, before it is queued.
pub(super) fn carries(
    machine: &str,
    group: &str,
    tool: &str,
    input: &Value,
    agent: &str,
    ceiling: &super::served::Ceiling,
) -> bool {
    RelayCall::of(machine, group, tool, input.clone(), agent, ceiling).carried()
}

/// What a machine answered.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayAnswer {
    pub status: ServedStatus,
    #[serde(default)]
    pub body: Value,
}

impl Ticketed for RelayCall {
    fn ticket(&self) -> Uuid {
        self.id
    }

    fn left(&mut self, left: Duration) {
        self.left_ms = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    }
}

/// The Mac's relay: what waits to be taken, and who waits for each answer.
pub(crate) type Relays = Pickup<RelayCall, RelayAnswer>;

/// An empty relay.
pub(crate) fn relays() -> Relays {
    Pickup::new(Limits {
        most_queued: MOST_QUEUED,
        most_waiting: MOST_WAITING,
        most_taken: MOST_TAKEN,
        poll_for: POLL_FOR,
    })
}

/// When Desk's native side last reached this hub by any of its routes.
#[derive(Default)]
pub(crate) struct NativeSeen(parking_lot::Mutex<Option<std::time::Instant>>);

impl NativeSeen {
    pub(crate) fn mark(&self) {
        *self.0.lock() = Some(std::time::Instant::now());
    }

    /// Whether it did within `within`.
    pub(crate) fn within(&self, within: Duration) -> bool {
        self.0.lock().is_some_and(|at| at.elapsed() < within)
    }
}

/// `router`, noting in `seen` each `/api/desk/` request made with the hub's
/// own token `token`, as only Desk's native side makes them, whatever its
/// route. Desk Helper's bootstrap (`/api/identities`, `/api/me`,
/// `/api/pair/*`) carries the same token and is no sign of Desk's native
/// side, so it isn't noted.
pub(crate) fn noting_native(router: Router, token: String, seen: Arc<NativeSeen>) -> Router {
    router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            if request.uri().path().starts_with("/api/desk/") && hubs_own(&token, request.headers())
            {
                seen.mark();
            }
            next.run(request)
        },
    ))
}

/// Whether `h` carries the hub's own token `token` in `Authorization`.
fn hubs_own(token: &str, h: &HeaderMap) -> bool {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| !token.is_empty() && crate::auth::constant_time_token_eq(given, token))
}

/// What only Desk's own plan runner asks a machine through the relay, never
/// a boss's tool: its sessions, a new session started there, and a plan
/// ticket's repository's checkout there (found, or cloned). Desk's native
/// side sends each to the machine's own route for it.
pub const DESK_CALLS: [&str; 3] = ["desk.sessions", "desk.session_start", "desk.checkout"];

/// Whether `name` can name a machine: no `/`, `:` or space in it.
pub fn machine_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `tool` is one Desk carries to a machine's own route, never a
/// group's: [`DESK_CALLS`] and [`super::machines::MACHINE_CALLS`].
pub fn is_machine_call(tool: &str) -> bool {
    DESK_CALLS
        .iter()
        .chain(super::machines::MACHINE_CALLS.iter())
        .any(|t| *t == tool)
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
        self.relay_as(machine, group, tool, input, agent, ceiling)
            .await
            .map_err(|e| e.to_string())
    }

    /// [`Self::relay`], saying whether what didn't come back may have
    /// reached the machine.
    pub(crate) async fn relay_as(
        &self,
        machine: &str,
        group: &str,
        tool: &str,
        input: Value,
        agent: &str,
        ceiling: &super::served::Ceiling,
    ) -> Result<RelayAnswer, workforce_desk::crew::MachineRefused> {
        use workforce_desk::crew::MachineRefused;
        let unreached = || {
            MachineRefused::Unreached(format!(
                "{machine} can't be reached: Desk isn't open on this Mac, or {machine} isn't connected."
            ))
        };
        // No one takes calls: nothing is queued to wait on.
        if !self.relay.polling() {
            return Err(unreached());
        }
        let wait = *self.relay_wait.lock();
        let call = RelayCall {
            outside: self.desk().turn_read_outside(),
            ..RelayCall::of(machine, group, tool, input, agent, ceiling)
        };
        // Too large to carry: never queued, so nothing of it is done.
        if !call.carried() {
            return Err(MachineRefused::Refused(format!(
                "That call is too large to send to {machine}; send less in it."
            )));
        }
        match self.relay.send(call, wait).await {
            Ok(answer) => {
                self.machine_reached
                    .lock()
                    .insert(machine.to_string(), std::time::Instant::now());
                Ok(answer)
            }
            // Desk took it: it may have been done there. Not to be sent
            // again as new.
            Err(Unsent::Taken) if tool == "desk.run" => Err(MachineRefused::Unknown(format!(
                "Sent to {machine}, but no answer came back: the check may have run there. Look at what it would have changed before you run it again."
            ))),
            Err(Unsent::Taken) => Err(MachineRefused::Unknown(format!(
                "Sent to {machine}, but its outcome isn't known yet: check {machine}'s session (crew.history) before you try it again."
            ))),
            Err(Unsent::NotPolling | Unsent::Full | Unsent::NotTaken) => Err(unreached()),
        }
    }

    /// When `machine` last answered a call, of any kind.
    pub(crate) fn machine_reached(&self, machine: &str) -> Option<std::time::Instant> {
        self.machine_reached.lock().get(machine).copied()
    }

    /// Whether Desk's native side relays now: it asked for calls lately.
    pub(crate) fn relaying(&self) -> bool {
        self.relay.polling()
    }

    /// The calls queued for machines, up to [`MOST_TAKEN`]: each taken
    /// once. Waits up to `wait` for one when none is queued.
    pub(crate) async fn relay_take(&self, wait: Duration) -> Vec<RelayCall> {
        self.relay.take(wait).await
    }

    /// The machine's answer to call `id`: taken once, by the call that
    /// waits for it; one for a call no one waits for (late, or answered
    /// already) is dropped.
    pub(crate) fn relay_answer(&self, id: Uuid, answer: RelayAnswer) -> bool {
        self.relay.answer(id, answer)
    }
}

/// What a machine's refusal of `tool` means when it is one an older Desk
/// there doesn't serve: that Desk there needs updating.
pub(crate) fn older(machine: &str, tool: &str, answer: &RelayAnswer) -> Option<String> {
    (answer.status == ServedStatus::Refused
        && answer.body["error"].as_str() == Some(&super::served::not_served(tool)))
    .then(|| {
        format!("Desk on {machine} is older and can't do {tool} yet: tell the person to update Desk there.")
    })
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

/// A native route's query: a `token` in it is refused.
#[derive(Deserialize)]
pub(crate) struct TokenQuery {
    pub(crate) token: Option<String>,
}

/// What Desk asks the machines' grants with: `plans=1` from one that
/// carries plans' grants, `ungrouped=1` from one that carries machines'
/// Ungrouped grants.
#[derive(Deserialize)]
struct GrantsQuery {
    token: Option<String>,
    plans: Option<String>,
    ungrouped: Option<String>,
}

/// Why a request isn't Desk's native side's, when it isn't: only the hub's
/// own token, in the `Authorization` header alone, is.
pub(crate) fn not_native(
    state: &GatewayState,
    h: &HeaderMap,
    token: Option<&str>,
) -> Option<Response> {
    if token.is_some() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "the relay takes the hub's token in the Authorization header only"})),
            )
                .into_response(),
        );
    }
    let ours = hubs_own(&state.auth_token, h);
    // 401: refused before anything is done, so Desk's native side may read
    // the hub's token again and send it once more.
    (!ours).then(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "only Desk itself relays these calls, with the hub's own token"})),
        )
            .into_response()
    })
}

/// Returns the refusal when the request isn't Desk's native side's.
macro_rules! native {
    ($s:expr, $h:expr, $token:expr) => {
        if let Some(r) = $crate::desk_crew::relay::not_native(&$s.state, &$h, $token) {
            return r;
        }
    };
}
pub(crate) use native;

/// On the Mac: the calls queued for machines.
async fn take(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    native!(s, h, q.token.as_deref());
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
    native!(s, h, q.token.as_deref());
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
    /// What the boss's turn had read of outside content; absent, none.
    #[serde(default)]
    outside: Option<String>,
}

/// On the machine: a boss's call relayed from the home, decided under the
/// machine's grant for its group.
async fn served(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>, body: Bytes) -> Response {
    native!(s, h, q.token.as_deref());
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
    // A machine call is never a group's: an older Desk sent it here rather
    // than to its own route.
    if is_machine_call(&r.tool) {
        return bad(&format!(
            "{} goes to its own route on this machine, not to a group's: update Desk where the boss runs.",
            r.tool
        ));
    }
    if !workforce_desk::grant::group_id_ok(&r.group) {
        return bad("that can't be a group");
    }
    let ceiling = super::served::Ceiling::of(level, &r.act);
    // The home taking its boss's decisions at Act fully, and answering
    // them ([`super::relayed_decide`]): the Mac hub's own calls, never a
    // crew tool's.
    use super::relayed_decide::{DECIDED, TAKE};
    use super::relayed_work::WORK_TAKE;
    let (status, body) = match r.tool.as_str() {
        WORK_TAKE => s.crew.work_take(&boss, &r.group, &r.input).await,
        TAKE => {
            s.crew
                .take_relayed(&boss, &r.group, &r.input, &ceiling)
                .await
        }
        DECIDED => s.crew.relayed_decided(&boss, &r.group, &r.input).await,
        _ => {
            let out = s
                .crew
                .serve_crew(
                    ServedCall {
                        tool: r.tool,
                        input: r.input,
                        boss,
                        link_id: Some(RELAY_LINK.to_string()),
                        call_id: Some(r.id),
                        ceiling: Some(ceiling),
                        outside: r.outside.map(|read| super::served::relayed_read(&read)),
                    },
                    &r.group,
                )
                .await;
            (out.status, out.body)
        }
    };
    Json(json!({ "status": status, "body": body })).into_response()
}

/// On the Mac: the grants each of the person's machines should hold for
/// this hub's own group bosses ([`super::machine_grants`]), for Desk's
/// native side to carry to them.
async fn machine_grants(State(s): S, h: HeaderMap, Query(q): Query<GrantsQuery>) -> Response {
    native!(s, h, q.token.as_deref());
    let said = |f: &Option<String>| f.as_deref() == Some("1");
    match s.crew.machine_grants(said(&q.plans), said(&q.ungrouped)) {
        Some(w) => Json(w).into_response(),
        // Not known yet: Desk writes nothing, and deletes nothing, until it is.
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "this hub hasn't read its bosses yet"})),
        )
            .into_response(),
    }
}

pub(crate) fn routes(state: Arc<GatewayState>, crew: Arc<CrewHub>) -> Router {
    Router::new()
        .route("/api/desk/machine-grants", get(machine_grants))
        .route("/api/desk/crew/relay", get(take))
        .route("/api/desk/crew/relay/{id}", post(answer))
        .route("/api/desk/crew/served", post(served))
        .with_state(Arc::new(RelayApi { state, crew }))
}

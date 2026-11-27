//! Sessions' Linear and Jira (`workforce_desk::issues`), wherever they run.
//!
//! - **On the person's own computer** ([`HomeIssues`]) a call goes through
//!   the board link of its project or team, exactly as that board syncs:
//!   [`BoardSync::remote`], as the person who linked it. No key or token
//!   leaves this hub.
//! - **On one of their machines** ([`MachineIssues`]) nothing holds a link
//!   or a key, and its hub can't reach the Mac. The call is queued; Desk's
//!   native side on the Mac, which holds the machine's forward, takes it
//!   (`GET /api/desk/issues/relay`, a long poll), hands it to this
//!   computer's hub (`POST /api/desk/issues/served`), and posts the answer
//!   back (`POST /api/desk/issues/relay/{id}`), once.
//!
//! Every route here is Desk's native side's alone, with the hub's own token
//! from the `Authorization` header alone, as the crew relay's are
//! ([`crate::desk_crew::relay::not_native`]). The Mac checks every call
//! again as the session asked it; the machine it came from is the one
//! Desk's native side names, and a call it served is never served twice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::surface::routing::{get, post};
use crate::surface::{Bound, Routes};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use workforce_desk::board::{Sync, System};
use workforce_desk::board_sync::RemoteError;
use workforce_desk::issues::{self, Asked, Asker, By, Issues};

use crate::desk_board_sync::{paused_words, refusal, BoardSync};
use crate::desk_crew::relay::{machine_ok, native, NativeSeen, TokenQuery};
use crate::desk_pickup::{Limits, Pickup, Ticketed, Unsent};
use crate::gateway::GatewayState;

/// How long a served call's id is remembered, and how many at most: a
/// machine's call waits [`issues::WAIT`], so ten minutes outlasts any.
const SERVED_FOR: Duration = Duration::from_secs(10 * 60);
const MOST_SERVED: usize = 4096;
/// How long since Desk's native side reached a machine by its other routes
/// it may be said to be an older Desk, when it never asked for calls.
const NATIVE_LATELY: Duration = Duration::from_secs(5 * 60);
/// The largest call or answer carried.
const MOST_BODY: usize = 256 * 1024;
/// What a machine keeps back of a session's wait, for the answer to reach
/// it.
const MARGIN: Duration = Duration::from_secs(2);
/// How much a machine's queue holds.
const LIMITS: Limits = Limits {
    most_queued: 32,
    most_waiting: 64,
    most_taken: 8,
    poll_for: Duration::from_secs(25),
};

/// An answer, as it is carried: `{ "ok": … }` or `{ "error": "…" }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Answer {
    Ok(Value),
    Error(String),
}

impl From<Result<Value, String>> for Answer {
    fn from(r: Result<Value, String>) -> Self {
        r.map_or_else(Self::Error, Self::Ok)
    }
}

// ── The person's own computer ────────────────────────────────────────────

pub(crate) struct HomeIssues {
    sync: Arc<BoardSync>,
    served: Mutex<HashMap<Uuid, Instant>>,
}

impl HomeIssues {
    pub(crate) fn new(sync: Arc<BoardSync>) -> Arc<Self> {
        Arc::new(Self {
            sync,
            served: Mutex::default(),
        })
    }

    /// What the person who linked `link` is called, for what Desk signs.
    fn person(&self, link: &Sync) -> String {
        let named = || {
            let id = Uuid::parse_str(&link.person).ok()?;
            let store = self.sync.state.identity_store.as_ref()?;
            store.get_by_id(id).ok().flatten().map(|i| i.display_name)
        };
        link.me_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(named)
            .unwrap_or_else(|| "the person".to_string())
    }

    /// `asked`, made from the session named `session` with id `id` (on
    /// `machine`, when it runs on one), in [`issues::RUN_FOR`] at most.
    pub(crate) async fn serve(
        &self,
        session: &str,
        id: Uuid,
        machine: Option<&str>,
        asked: &Asked,
    ) -> Result<Value, String> {
        tokio::time::timeout(issues::RUN_FOR, self.run(session, id, machine, asked))
            .await
            .unwrap_or_else(|_| Err(issues::no_answer(asked)))
    }

    /// The links the session with id `id` (on `machine`) reaches, or why
    /// it reaches none.
    async fn links(&self, id: Uuid, machine: Option<&str>) -> Result<Vec<Sync>, String> {
        let key = machine.map_or_else(|| id.to_string(), |m| format!("{m}/{id}"));
        self.sync.desk.issue_links(&key).await.map_err(|e| match e {
            workforce_desk::DeskError::Invalid(why) => why,
            e => {
                tracing::warn!(error = %e, "the boards' links couldn't be read for a session");
                "Desk couldn't read its boards just now; try again.".to_string()
            }
        })
    }

    async fn run(
        &self,
        session: &str,
        id: Uuid,
        machine: Option<&str>,
        asked: &Asked,
    ) -> Result<Value, String> {
        let call = issues::call_of(asked.system, &asked.input)?;
        let title = asked.system.title();
        let links: Vec<Sync> = self
            .links(id, machine)
            .await?
            .into_iter()
            .filter(|s| s.system == asked.system)
            .collect();
        let picked = issues::pick(&links, &call)?;
        let link = picked.link;
        let hub = self.sync.hub_name(&link.via).await;
        let remote = self
            .sync
            .remote(
                link.system,
                &link.via,
                &link.person,
                link.agent.as_deref(),
                link.workspace.as_deref(),
            )
            .await
            .map_err(|p| {
                format!(
                    "Desk can't reach {title} through the {} board's link. {}",
                    link.target,
                    paused_words(&hub, link.system, &p)
                )
            })?;
        let by = By {
            person: self.person(link),
            session: session.to_string(),
            machine: machine.map(str::to_string),
        };
        match issues::run(&*remote, &picked, &call, &by).await {
            Ok(answer) => Ok(answer),
            Err(RemoteError::Waiting { .. }) => Err(format!(
                "That waits for the person's approval on {hub}, which holds the {title} key; it goes through once they approve it there."
            )),
            Err(e) => Err(format!("{title} through {hub}: {e}")),
        }
    }

    /// Whether call `id` may be served: each once, and none while every
    /// id kept is still within its window, so none is forgotten early.
    fn first_time(&self, id: Uuid) -> Fresh {
        let mut served = self.served.lock();
        served.retain(|_, at| at.elapsed() < SERVED_FOR);
        if served.contains_key(&id) {
            return Fresh::Served;
        }
        if served.len() >= MOST_SERVED {
            return Fresh::Full;
        }
        served.insert(id, Instant::now());
        Fresh::Yes
    }
}

/// Whether a call may be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fresh {
    Yes,
    Served,
    Full,
}

#[async_trait]
impl Issues for HomeIssues {
    async fn issue(&self, asker: &Asker, asked: &Asked) -> Result<Value, String> {
        self.serve(&asker.name, asker.session, None, asked).await
    }

    async fn offered(&self, asker: &Asker) -> Vec<System> {
        let links = self.links(asker.session, None).await.unwrap_or_default();
        [System::Linear, System::Jira]
            .into_iter()
            .filter(|s| links.iter().any(|l| l.system == *s))
            .collect()
    }
}

// ── One of the person's machines ─────────────────────────────────────────

/// A machine's session's call, queued for Desk's native side.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelayCall {
    pub id: Uuid,
    pub system: System,
    /// The session it was made from, by its name.
    pub session: String,
    /// That session's id on this machine.
    pub session_id: Uuid,
    pub input: Value,
    /// How long, in milliseconds as it is taken, the machine still waits
    /// for its answer: nothing is carried past it.
    pub left_ms: u64,
}

impl Ticketed for RelayCall {
    fn ticket(&self) -> Uuid {
        self.id
    }

    fn left(&mut self, left: Duration) {
        self.left_ms = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    }
}

pub(crate) struct MachineIssues {
    pickup: Pickup<RelayCall, Answer>,
    native: Arc<NativeSeen>,
}

impl MachineIssues {
    /// Its calls, for a Desk whose last visits by any route `native` notes.
    pub(crate) fn new(native: Arc<NativeSeen>) -> Arc<Self> {
        Arc::new(Self {
            pickup: Pickup::new(LIMITS),
            native,
        })
    }
}

/// Why a machine's session has no Linear or Jira now.
fn not_connected(system: System) -> String {
    format!(
        "{} reaches this machine's sessions through Desk on the person's Mac, which isn't connected to this machine right now. It connects by itself when Desk is open there; try again then.",
        system.title()
    )
}

/// What a session hears when Desk took its call and no answer came back.
fn carried_no_answer(asked: &Asked) -> String {
    match asked.action().is_some_and(issues::Action::writes) {
        true => "Sent to Desk on your Mac, but no answer came back in time: it may have gone through. Read the issue (get) before you try again.".into(),
        false => "Sent to Desk on your Mac, but no answer came back in time; try again.".into(),
    }
}

#[async_trait]
impl Issues for MachineIssues {
    async fn issue(&self, asker: &Asker, asked: &Asked) -> Result<Value, String> {
        let wait = issues::WAIT.saturating_sub(MARGIN);
        let call = RelayCall {
            id: Uuid::new_v4(),
            system: asked.system,
            session: asker.name.clone(),
            session_id: asker.session,
            input: asked.input.clone(),
            left_ms: 0,
        };
        if serde_json::to_vec(&call).map_or(usize::MAX, |b| b.len()) > MOST_BODY {
            return Err("That call is too large to carry; send less in it.".into());
        }
        match self.pickup.send(call, wait).await {
            Ok(Answer::Ok(v)) => Ok(v),
            Ok(Answer::Error(e)) => Err(e),
            Err(Unsent::Taken) => Err(carried_no_answer(asked)),
            Err(Unsent::Full) => {
                Err("Too many calls wait for Desk on your Mac; try again in a moment.".into())
            }
            Err(Unsent::NotPolling)
                if !self.pickup.polled_ever() && self.native.within(NATIVE_LATELY) =>
            {
                Err("Desk on the Mac needs an update to give machines Linear and Jira.".into())
            }
            Err(Unsent::NotPolling | Unsent::NotTaken) => Err(not_connected(asked.system)),
        }
    }
}

// ── Routes ───────────────────────────────────────────────────────────────

struct Api<T> {
    state: Arc<GatewayState>,
    issues: Arc<T>,
}

/// On a machine: its sessions' queued calls.
async fn take(
    State(s): State<Arc<Api<MachineIssues>>>,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    native!(s, h, q.token.as_deref());
    Json(s.issues.pickup.take(LIMITS.poll_for).await).into_response()
}

/// On a machine: the Mac's answer to one call, once.
async fn answer(
    State(s): State<Arc<Api<MachineIssues>>>,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Response {
    native!(s, h, q.token.as_deref());
    if body.len() > MOST_BODY {
        return refusal(StatusCode::PAYLOAD_TOO_LARGE, "that answer is too large");
    }
    let Ok(a) = serde_json::from_slice::<Answer>(&body) else {
        return refusal(StatusCode::BAD_REQUEST, "that isn't an answer");
    };
    match s.issues.pickup.answer(id, a) {
        true => StatusCode::NO_CONTENT.into_response(),
        false => refusal(StatusCode::GONE, "no call waits for that answer"),
    }
}

/// What Desk's native side hands this computer's hub of a machine's call.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Served {
    id: Uuid,
    /// The machine, as Desk names it.
    machine: String,
    system: System,
    session: String,
    session_id: Uuid,
    #[serde(default)]
    input: Value,
}

/// On the person's own computer: a machine's session's call, run here.
async fn served(
    State(s): State<Arc<Api<HomeIssues>>>,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Bytes,
) -> Response {
    native!(s, h, q.token.as_deref());
    if body.len() > MOST_BODY {
        return refusal(StatusCode::PAYLOAD_TOO_LARGE, "that call is too large");
    }
    let Ok(c) = serde_json::from_slice::<Served>(&body) else {
        return refusal(StatusCode::BAD_REQUEST, "that isn't a machine's call");
    };
    if !machine_ok(&c.machine) || c.session.trim().is_empty() || c.session.len() > 200 {
        return refusal(
            StatusCode::BAD_REQUEST,
            "that call doesn't say its machine and session",
        );
    }
    match s.issues.first_time(c.id) {
        Fresh::Yes => {}
        Fresh::Served => return refusal(StatusCode::CONFLICT, "that call was served already"),
        Fresh::Full => {
            return refusal(
                StatusCode::TOO_MANY_REQUESTS,
                "too many machines' calls were served lately; try again in a moment",
            )
        }
    }
    let asked = Asked {
        system: c.system,
        input: c.input,
    };
    let out = s
        .issues
        .serve(&c.session, c.session_id, Some(&c.machine), &asked)
        .await;
    tracing::info!(
        id = %c.id,
        machine = %c.machine,
        system = c.system.as_str(),
        action = asked.action().map_or("", issues::Action::as_str),
        ok = out.is_ok(),
        "a machine's session's issue call was served"
    );
    Json(Answer::from(out)).into_response()
}

/// Where a computer's sessions' Linear and Jira calls go.
#[derive(Clone)]
pub(crate) enum Carrier {
    /// A machine's: through Desk on the person's Mac.
    Machine(Arc<MachineIssues>),
    /// The person's own computer's: through its board links.
    Home(Arc<HomeIssues>),
}

/// The issue routes of a computer whose calls go by `issues`: a machine's
/// for Desk on the Mac to carry, or the person's own computer's for its
/// machines' sessions' calls. Declared alone, both.
pub(crate) fn routes(state: Arc<GatewayState>, issues: Bound<Carrier>) -> Routes {
    match issues {
        Bound::To(Carrier::Machine(issues)) => {
            machine_routes(Bound::To(Arc::new(Api { state, issues })))
        }
        Bound::To(Carrier::Home(issues)) => home_routes(Bound::To(Arc::new(Api { state, issues }))),
        Bound::Declared => machine_routes(Bound::Declared).merge(home_routes(Bound::Declared)),
    }
}

fn machine_routes(api: Bound<Arc<Api<MachineIssues>>>) -> Routes {
    Routes::new()
        .admin("/api/desk/issues/relay", get(take))
        .admin("/api/desk/issues/relay/{id}", post(answer))
        .bind(api)
}

fn home_routes(api: Bound<Arc<Api<HomeIssues>>>) -> Routes {
    Routes::new()
        .admin("/api/desk/issues/served", post(served))
        .bind(api)
}

#[cfg(test)]
#[path = "desk_issues_tests.rs"]
mod tests;

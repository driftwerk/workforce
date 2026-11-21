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

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use workforce_desk::board::{Sync, System};
use workforce_desk::board_sync::RemoteError;
use workforce_desk::issues::{self, Asked, Asker, By, Issues};

use crate::desk_board_sync::{paused_words, refusal, BoardSync};
use crate::desk_crew::relay::{machine_ok, not_native};
use crate::desk_pickup::{Limits, Pickup, Ticketed, Unsent};
use crate::gateway::GatewayState;

/// How long a served call's id is remembered, and how many at most.
const SERVED_FOR: Duration = Duration::from_secs(10 * 60);
const MOST_SERVED: usize = 1024;
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
    /// `machine`, when it runs on one).
    pub(crate) async fn serve(
        &self,
        session: &str,
        id: Uuid,
        machine: Option<&str>,
        asked: &Asked,
    ) -> Result<Value, String> {
        let call = issues::call_of(asked.system, &asked.input)?;
        let title = asked.system.title();
        let links = self
            .sync
            .desk
            .issue_links(
                asked.system,
                &machine.map_or_else(|| id.to_string(), |m| format!("{m}/{id}")),
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "the boards' links couldn't be read for a session");
                "Desk couldn't read its boards just now; try again.".to_string()
            })?;
        let Some(link) = issues::pick(&links, &call) else {
            return Err(format!(
                "No board this session reaches in Desk is linked to {title}. Link one (Board, then Link to {title}) and the sessions that reach it get {title} through it."
            ));
        };
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
        match tokio::time::timeout(issues::RUN_FOR, issues::run(&*remote, link, &call, &by)).await
        {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(RemoteError::Waiting { .. })) => Err(format!(
                "That waits for the person's approval on {hub}, which holds the {title} key; it goes through once they approve it there."
            )),
            Ok(Err(e)) => Err(format!("{title} through {hub}: {e}")),
            Err(_) => Err(issues::no_answer(asked)),
        }
    }

    /// Whether call `id` comes for the first time: each is served once.
    fn first_time(&self, id: Uuid) -> bool {
        let mut served = self.served.lock();
        served.retain(|_, at| at.elapsed() < SERVED_FOR);
        if served.contains_key(&id) {
            return false;
        }
        if served.len() >= MOST_SERVED {
            if let Some(oldest) = served.iter().min_by_key(|(_, at)| **at).map(|(i, _)| *i) {
                served.remove(&oldest);
            }
        }
        served.insert(id, Instant::now());
        true
    }
}

#[async_trait]
impl Issues for HomeIssues {
    async fn issue(&self, asker: &Asker, asked: &Asked) -> Result<Value, String> {
        self.serve(&asker.name, asker.session, None, asked).await
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
    /// When the machine stops waiting for its answer: nothing is carried
    /// past it.
    pub deadline: DateTime<Utc>,
}

impl Ticketed for RelayCall {
    fn ticket(&self) -> Uuid {
        self.id
    }
}

pub(crate) struct MachineIssues {
    pickup: Pickup<RelayCall, Answer>,
}

impl MachineIssues {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            pickup: Pickup::new(LIMITS),
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
            deadline: Utc::now()
                + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::zero()),
        };
        if serde_json::to_vec(&call).map_or(usize::MAX, |b| b.len()) > MOST_BODY {
            return Err("That call is too large to carry; send less in it.".into());
        }
        match self.pickup.send(call, wait).await {
            Ok(Answer::Ok(v)) => Ok(v),
            Ok(Answer::Error(e)) => Err(e),
            Err(Unsent::Taken) => Err(issues::no_answer(asked)),
            Err(Unsent::Unqueued | Unsent::NotTaken) => Err(not_connected(asked.system)),
        }
    }
}

// ── Routes ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

struct Api<T> {
    state: Arc<GatewayState>,
    issues: Arc<T>,
}

macro_rules! native {
    ($s:expr, $h:expr, $q:expr) => {
        if let Some(r) = not_native(&$s.state, &$h, $q.token.as_deref()) {
            return r;
        }
    };
}

/// On a machine: its sessions' queued calls.
async fn take(
    State(s): State<Arc<Api<MachineIssues>>>,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    native!(s, h, q);
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
    native!(s, h, q);
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
    native!(s, h, q);
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
    if !s.issues.first_time(c.id) {
        return refusal(StatusCode::CONFLICT, "that call was served already");
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

/// A machine's routes: its sessions' calls, for Desk on the Mac to carry.
pub(crate) fn machine_routes(state: Arc<GatewayState>, issues: Arc<MachineIssues>) -> Router {
    Router::new()
        .route("/api/desk/issues/relay", get(take))
        .route("/api/desk/issues/relay/{id}", post(answer))
        .with_state(Arc::new(Api { state, issues }))
}

/// The person's own computer's route: its machines' sessions' calls.
pub(crate) fn home_routes(state: Arc<GatewayState>, issues: Arc<HomeIssues>) -> Router {
    Router::new()
        .route("/api/desk/issues/served", post(served))
        .with_state(Arc::new(Api { state, issues }))
}

#[cfg(test)]
#[path = "desk_issues_tests.rs"]
mod tests;

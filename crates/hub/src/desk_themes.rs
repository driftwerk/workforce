//! `theme.*`: an agent makes the person a Desk theme when they ask for one.
//!
//! Themes are Desk's, on the person's computer: their files are in Desk's
//! themes folder, and Desk works every colour out from a few (its
//! `derive`), then checks how they read. So the hub does none of that
//! itself. It carries each call to the person's Desk and hands the agent
//! Desk's answer:
//!
//! 1. A call is queued for the person whose conversation it is in
//!    ([`Jobs::ask`]), and waits [`ANSWER_WITHIN`] at most.
//! 2. The person's Desk, signed in here as them, asks for their calls
//!    (`GET /api/me/desk-theme-jobs`, held open until one comes) and
//!    answers each (`POST /api/me/desk-theme-jobs/{id}`).
//! 3. What Desk answered is the tool's result. Not taken in time, or the
//!    turn stopped: the call is dropped, never done later, and the agent
//!    is told Desk didn't answer. Taken and not answered: it waits
//!    [`TAKEN_GRACE`] more, then the agent is told Desk may have saved it.
//!
//! The calls wait in this hub's memory: a hub run as several instances
//! hands a call only to a Desk asking the instance the turn is on. A call
//! handed to a Desk whose ask has just broken off is lost, and the agent is
//! told as above.
//!
//! - `theme.current` reads the theme the person has on, every colour of
//!   it, and the themes they have.
//! - `theme.propose` and `theme.revise` save a proposal beside the person's
//!   themes. Nothing is applied: the person keeps it, discards it, or asks
//!   for changes, on its card in the conversation or in Settings.
//!
//! **Who may do what.** A call is the person's of the turn
//! (`CURRENT_USER_ID`), and a proposal is made in their conversation only.
//! Only that person's Desk is handed it or may answer it. The hub reads
//! nothing of what is asked beyond its size: Desk checks every colour and
//! name, and writes only theme proposals, only in its themes folder. A turn
//! run as the person unattended (a schedule, a workflow) may propose too:
//! nothing is applied, so the person still decides.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};
use uuid::Uuid;
use workforce_agent::tools::{Tool, ToolResult};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// The tools, the one that changes nothing first.
pub const NAMES: [&str; 3] = ["theme.current", "theme.propose", "theme.revise"];

/// How long a call waits for the person's Desk to take it and answer.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);
/// How much longer a call Desk has taken waits for its answer: Desk may be
/// saving it.
pub const TAKEN_GRACE: Duration = Duration::from_secs(10);
/// The longest Desk's ask for calls is held open.
const HOLD_MOST: Duration = Duration::from_secs(25);
/// The most calls waiting for one person's Desk at once.
const MOST_QUEUED: usize = 8;
/// The largest call carried: a theme is a few dozen colours.
const MOST_INPUT: usize = 16 * 1024;
/// The largest answer taken from Desk, and handed to the model: a theme's
/// every colour, and the names of the person's themes.
pub const MOST_ANSWER: usize = 64 * 1024;

pub const JOBS_PATH: &str = "/api/me/desk-theme-jobs";
pub const ANSWER_PATH: &str = "/api/me/desk-theme-jobs/{id}";

/// A call, as the person's Desk is handed it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: Uuid,
    /// `current`, `propose` or `revise`.
    pub kind: &'static str,
    /// The agent that called it.
    pub agent: String,
    /// The conversation it was called in.
    pub session: Option<String>,
    /// What the agent gave, as it gave it.
    pub input: Value,
    pub at: DateTime<Utc>,
}

/// Desk's answer: what the tool returns, or why it couldn't.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Answer {
    pub ok: bool,
    #[serde(default)]
    pub body: Value,
}

struct Waiting {
    person: String,
    tx: oneshot::Sender<Answer>,
    /// Handed to the person's Desk.
    taken: bool,
}

#[derive(Default)]
struct Inner {
    /// Calls not yet handed to the person's Desk, by person.
    queued: HashMap<String, VecDeque<Job>>,
    /// Calls waiting for an answer, handed or not, by id.
    waiting: HashMap<Uuid, Waiting>,
}

/// The calls waiting on people's Desks.
#[derive(Default)]
pub struct Jobs {
    inner: Mutex<Inner>,
    /// Woken whenever a call is queued.
    queued: Notify,
}

static JOBS: OnceLock<Arc<Jobs>> = OnceLock::new();

/// The hub's one queue.
pub fn jobs() -> Arc<Jobs> {
    Arc::clone(JOBS.get_or_init(Default::default))
}

impl Jobs {
    /// Hands `job` to `person`'s Desk and waits `within` for its answer.
    pub async fn ask(&self, person: &str, job: Job, within: Duration) -> Result<Answer, String> {
        let id = job.id;
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock();
            let queue = inner.queued.entry(person.to_string()).or_default();
            if queue.len() >= MOST_QUEUED {
                return Err(
                    "Desk has too many theme requests waiting. Try again in a moment.".into(),
                );
            }
            queue.push_back(job);
            inner.waiting.insert(
                id,
                Waiting {
                    person: person.to_string(),
                    tx,
                    taken: false,
                },
            );
        }
        // However this ends (answered, timed out, or the turn stopped and
        // this was dropped), the call goes: never done later.
        let _forget = Forget {
            jobs: self,
            person,
            id,
        };
        self.queued.notify_waiters();
        tokio::pin!(rx);
        let answer = match tokio::time::timeout(within, &mut rx).await {
            Ok(a) => Some(a),
            // Desk took it and may be saving it: a little longer.
            Err(_) if self.taken(id) => tokio::time::timeout(TAKEN_GRACE, &mut rx).await.ok(),
            Err(_) => None,
        };
        match answer {
            Some(Ok(answer)) => Ok(answer),
            _ if self.taken(id) => Err(
                "Desk took this but didn't answer in time; it may have saved it. Call theme.current to see \
                 the person's proposals before trying again."
                    .into(),
            ),
            _ => Err(
                "Desk didn't answer. The person's Desk has to be open and signed in to this hub: \
                 ask them to open it, then try again."
                    .into(),
            ),
        }
    }

    fn taken(&self, id: Uuid) -> bool {
        self.inner.lock().waiting.get(&id).is_some_and(|w| w.taken)
    }

    fn forget(&self, person: &str, id: Uuid) {
        let mut inner = self.inner.lock();
        inner.waiting.remove(&id);
        if let Some(queue) = inner.queued.get_mut(person) {
            queue.retain(|j| j.id != id);
            if queue.is_empty() {
                inner.queued.remove(person);
            }
        }
    }

    /// `person`'s calls not yet handed to their Desk, handed now.
    pub fn take(&self, person: &str) -> Vec<Job> {
        let mut inner = self.inner.lock();
        let jobs: Vec<Job> = inner
            .queued
            .remove(person)
            .map(Vec::from)
            .unwrap_or_default();
        for j in &jobs {
            if let Some(w) = inner.waiting.get_mut(&j.id) {
                w.taken = true;
            }
        }
        jobs
    }

    /// `person`'s calls, waiting `hold` at most for one to come.
    pub async fn next(&self, person: &str, hold: Duration) -> Vec<Job> {
        let until = tokio::time::Instant::now() + hold;
        loop {
            let queued = self.queued.notified();
            tokio::pin!(queued);
            // Seen before looking, so a call queued between the look and
            // the wait still wakes it.
            queued.as_mut().enable();
            let jobs = self.take(person);
            if !jobs.is_empty() {
                return jobs;
            }
            if tokio::time::timeout_at(until, queued).await.is_err() {
                return vec![];
            }
        }
    }

    /// Answers call `id`, when it is `person`'s and still waits.
    pub fn answer(&self, person: &str, id: Uuid, answer: Answer) -> bool {
        let mut inner = self.inner.lock();
        match inner.waiting.get(&id) {
            Some(w) if w.person == person => {}
            _ => return false,
        }
        let Some(w) = inner.waiting.remove(&id) else {
            return false;
        };
        w.tx.send(answer).is_ok()
    }
}

/// Forgets a call when its ask ends, however it ends.
struct Forget<'a> {
    jobs: &'a Jobs,
    person: &'a str,
    id: Uuid,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.jobs.forget(self.person, self.id);
    }
}

/// One of the `theme.*` tools, for one agent.
pub struct ThemeTool {
    name: &'static str,
    agent: String,
}

impl ThemeTool {
    pub fn all(agent: &str) -> Vec<Self> {
        NAMES
            .into_iter()
            .map(|name| Self {
                name,
                agent: agent.to_string(),
            })
            .collect()
    }

    fn kind(&self) -> &'static str {
        match self.name {
            "theme.current" => "current",
            "theme.propose" => "propose",
            _ => "revise",
        }
    }
}

/// Registers `theme.*` for `service`, on every hub: a hub agent makes the
/// person's theme as a local one does.
pub fn register(service: &crate::serve::AgentService) {
    for tool in ThemeTool::all(service.agent_id.as_str()) {
        if !service.tools.has(tool.name) {
            service.tools.register(Arc::new(tool));
        }
    }
}

const PROPOSE: &str =
    "Make the person a Desk theme, when they ask for one. Give its name, whether \
it is dark or light, and its base colours: the background, the signal (the accent: buttons, the \
cursor, what is chosen) and, if you like, the text. Desk works out every other colour from them, \
the terminal's 16 colours, cursor and selection included, then checks every pair it draws one on \
the other and moves any that read too faintly, and says which it moved. Set any other colour \
yourself under `colors` (theme.current lists them all, by name). If the person shared an image (a \
screenshot, a brand palette, a photo), pick the colours from it. It is saved as a proposal: \
nothing changes until the person keeps it, on the card Desk shows them in this conversation. \
Optionally set `density` and `corners`, else the person's own are kept, and `reason`: what they \
asked for, in their words, shown on the card. Tell them in a sentence what you made, and what Desk \
moved, if anything.";

const REVISE: &str = "Change a theme proposal, or one of the person's own themes, from what they \
said about it: give its id (from theme.propose, or theme.current's list) and only what changes. A \
colour under `colors` set to null goes back to what Desk works out. Desk checks how it reads \
again. Revising a proposal replaces it; revising one of their themes saves the change as a \
proposal beside it, and their theme is never changed until they keep it. Desk's built-in themes \
can't be revised (that fails): propose a new one from their colours (theme.current) instead.";

const CURRENT: &str = "The theme the person has on in Desk: its id, name, whether it is dark, its \
base colours, every colour Desk draws with by name (the terminal's included), and the themes and \
proposals they have. It takes nothing. Start a theme from it, or change one, by name.";

fn hex(what: &str) -> Value {
    json!({ "type": "string", "pattern": "^#[0-9A-Fa-f]{6}$", "description": what })
}

const BACKGROUND: &str = "The background, like \"#13201A\"";
const SIGNAL: &str = "The accent: buttons, the cursor, what is chosen";
const TEXT: &str = "The text; left out, Desk picks one that reads on the background";

fn colors(nullable: bool) -> Value {
    json!({
        "type": "object",
        "description": if nullable {
            "Colours to set by Desk's names (theme.current lists them), like \"#1E2A35\"; the shadows and rings may carry alpha, \"#00000059\". null: back to what Desk works out."
        } else {
            "Colours to set by Desk's names (theme.current lists them), like \"#1E2A35\"; the shadows and rings may carry alpha, \"#00000059\". Leave out what Desk should work out."
        },
        "additionalProperties": if nullable { json!({ "type": ["string", "null"] }) } else { json!({ "type": "string" }) }
    })
}

#[async_trait]
impl Tool for ThemeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        match self.name {
            "theme.current" => CURRENT,
            "theme.propose" => PROPOSE,
            _ => REVISE,
        }
    }

    fn input_schema(&self) -> Value {
        let shape = json!({
            "density": { "type": "string", "enum": ["compact", "cozy"] },
            "corners": { "type": "string", "enum": ["sharp", "soft", "round"] },
        });
        let reason = json!({ "type": "string", "description": "What the person asked for, in their words, for the card" });
        match self.name {
            "theme.current" => json!({ "type": "object", "properties": {} }),
            "theme.propose" => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "maxLength": 48 },
                    "dark": { "type": "boolean", "description": "true for a dark theme, false for a light one" },
                    "background": hex(BACKGROUND),
                    "signal": hex(SIGNAL),
                    "text": hex(TEXT),
                    "colors": colors(false),
                    "density": shape["density"],
                    "corners": shape["corners"],
                    "reason": reason,
                },
                "required": ["name", "dark", "background", "signal"]
            }),
            _ => json!({
                "type": "object",
                "properties": {
                    "theme": { "type": "string", "description": "The proposal's or theme's id" },
                    "name": { "type": "string", "maxLength": 48 },
                    "dark": { "type": "boolean" },
                    "background": hex(BACKGROUND),
                    "signal": hex(SIGNAL),
                    "text": { "type": ["string", "null"], "description": "The text colour, or null for one Desk picks" },
                    "colors": colors(true),
                    "density": shape["density"],
                    "corners": shape["corners"],
                    "reason": reason,
                },
                "required": ["theme"]
            }),
        }
    }

    /// Nothing on this computer changes: Desk saves a proposal, and only
    /// the person keeps one.
    fn effect(&self, _input: &Value) -> workforce_core::computer::Effect {
        workforce_core::computer::Effect::None
    }

    fn soft_deadline(&self) -> Option<Duration> {
        Some(ANSWER_WITHIN + TAKEN_GRACE + Duration::from_secs(10))
    }

    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_USER_ID};
        let started = Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        // The person's identity, whose own Desk signs in as them: a turn of
        // someone known only by a platform's id has no Desk to ask.
        let Some(person) = CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| Uuid::parse_str(u).is_ok())
        else {
            return Ok(ToolResult::failure(
                "Themes are made for a person, in their conversation with you.",
                ms(),
            ));
        };
        let session = CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
            .filter(|s| !s.is_empty());
        if session.is_none() && self.name != "theme.current" {
            return Ok(ToolResult::failure(
                "A theme is proposed in the person's conversation with you, where its card shows.",
                ms(),
            ));
        }
        if serde_json::to_vec(&input).map_or(usize::MAX, |b| b.len()) > MOST_INPUT {
            return Ok(ToolResult::failure("That is too much for a theme.", ms()));
        }
        let job = Job {
            id: Uuid::new_v4(),
            kind: self.kind(),
            agent: self.agent.clone(),
            session,
            input,
            at: Utc::now(),
        };
        Ok(match jobs().ask(&person, job, ANSWER_WITHIN).await {
            Ok(Answer { ok: true, body }) => ToolResult::success(body, ms()),
            Ok(Answer { body, .. }) => ToolResult::failure(
                body.get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| "Desk couldn't do that.".into()),
                ms(),
            ),
            Err(e) => ToolResult::failure(e, ms()),
        })
    }
}

/// The routes the person's Desk asks for its calls and answers them on.
pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route(JOBS_PATH, get(jobs_route))
        .route(
            ANSWER_PATH,
            post(answer_route).layer(axum::extract::DefaultBodyLimit::max(MOST_ANSWER)),
        )
        .with_state(state)
}

#[derive(Deserialize)]
struct JobsQuery {
    /// How long to hold the ask open for a call, in seconds.
    wait: Option<u64>,
}

fn refused(status: StatusCode, e: &str) -> Response {
    (status, Json(json!({ "error": e }))).into_response()
}

/// The person asking, by their own Desk: their own sign-in, or a desktop
/// device paired with this hub and linked to them; never a phone's or any
/// other device's token, nor the hub's own. They are active here.
async fn person(state: &GatewayState, headers: &HeaderMap) -> Result<String, Response> {
    // The header only: a token in the address would be in access logs.
    let caller = resolve_caller_context(state, headers, None).await;
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => {
            return Err(refused(StatusCode::UNAUTHORIZED, "not allowed"))
        }
        Some(AuthFailure::StoreError) => {
            return Err(refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not check that token",
            ))
        }
        None => {}
    }
    let Some(person) = caller.identity_id else {
        return Err(refused(
            StatusCode::UNAUTHORIZED,
            "A theme call is a person's: ask as yourself, not as the hub",
        ));
    };
    let store = state.identity_store.as_ref();
    if let Some(device) = caller.device_id.as_deref() {
        let at_desk = store.is_some_and(|store| {
            crate::device_link::is_desktop_device(store.as_ref(), device)
                && crate::device_link::identity_for_device(store.as_ref(), device).as_deref()
                    == Some(person.as_str())
        });
        if !at_desk {
            return Err(refused(
                StatusCode::FORBIDDEN,
                "Only your Desk on a computer takes theme calls: not a phone or other device",
            ));
        }
    }
    let active = Uuid::parse_str(&person).ok().is_some_and(|id| {
        store
            .and_then(|s| s.get_by_id(id).ok().flatten())
            .is_some_and(|i| i.status == workforce_identity::types::IdentityStatus::Active)
    });
    if !active {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "You aren't active on this hub",
        ));
    }
    Ok(person)
}

async fn jobs_route(
    State(state): State<Arc<GatewayState>>,
    h: HeaderMap,
    Query(q): Query<JobsQuery>,
) -> Response {
    let person = match person(&state, &h).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let hold = Duration::from_secs(q.wait.unwrap_or(0)).min(HOLD_MOST);
    Json(json!({ "jobs": jobs().next(&person, hold).await })).into_response()
}

async fn answer_route(
    State(state): State<Arc<GatewayState>>,
    h: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    // Who is asking first: nothing of the body is read for anyone else.
    let person = match person(&state, &h).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return refused(StatusCode::NOT_FOUND, "There is no such request.");
    };
    let Ok(answer) = serde_json::from_slice::<Answer>(&body) else {
        return refused(StatusCode::BAD_REQUEST, "An answer is {ok, body}.");
    };
    if jobs().answer(&person, id, answer) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        refused(
            StatusCode::NOT_FOUND,
            "That request has ended: the agent stopped waiting for it.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: &'static str) -> Job {
        Job {
            id: Uuid::new_v4(),
            kind,
            agent: "atlas".into(),
            session: Some("s1".into()),
            input: json!({ "name": "Moss" }),
            at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_call_is_handed_to_its_persons_desk_and_takes_its_answer() {
        let q = Arc::new(Jobs::default());
        let j = job("propose");
        let id = j.id;
        let asking = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.ask("kim", j, Duration::from_secs(5)).await }
        });
        let handed = q.next("kim", Duration::from_secs(5)).await;
        assert_eq!(handed.iter().map(|j| j.id).collect::<Vec<_>>(), vec![id]);
        // Another person's Desk neither sees it nor answers it.
        assert!(q.take("lee").is_empty());
        assert!(!q.answer(
            "lee",
            id,
            Answer {
                ok: true,
                body: json!({})
            }
        ));
        assert!(q.answer(
            "kim",
            id,
            Answer {
                ok: true,
                body: json!({ "id": "moss" })
            }
        ));
        let answer = asking.await.unwrap().unwrap();
        assert_eq!(answer.body["id"], "moss");
        // Answered once only.
        assert!(!q.answer(
            "kim",
            id,
            Answer {
                ok: true,
                body: json!({})
            }
        ));
    }

    #[tokio::test]
    async fn a_call_not_answered_in_time_is_dropped_never_done_later() {
        let q = Jobs::default();
        let j = job("current");
        let id = j.id;
        let e = q
            .ask("kim", j, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(e.contains("Desk didn't answer"), "{e}");
        assert!(q.take("kim").is_empty());
        assert!(!q.answer(
            "kim",
            id,
            Answer {
                ok: true,
                body: json!({})
            }
        ));
    }

    #[tokio::test]
    async fn a_call_whose_turn_stopped_is_dropped_never_done_later() {
        let q = Arc::new(Jobs::default());
        let asking = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.ask("kim", job("propose"), Duration::from_secs(60)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        asking.abort();
        let _ = asking.await;
        assert!(q.take("kim").is_empty());
        assert!(q.inner.lock().waiting.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_desk_took_waits_a_little_longer_and_says_it_may_be_saved() {
        let q = Arc::new(Jobs::default());
        let asking = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.ask("kim", job("propose"), Duration::from_secs(20)).await }
        });
        tokio::task::yield_now().await;
        let handed = q.take("kim");
        assert_eq!(handed.len(), 1);
        // Past the first wait, inside the grace: still answered.
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(q.answer(
            "kim",
            handed[0].id,
            Answer {
                ok: true,
                body: json!({ "id": "moss" })
            }
        ));
        assert_eq!(asking.await.unwrap().unwrap().body["id"], "moss");
        // Taken and never answered: it may have been saved.
        let asking = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.ask("kim", job("propose"), Duration::from_secs(20)).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(q.take("kim").len(), 1);
        let e = asking.await.unwrap().unwrap_err();
        assert!(e.contains("may have saved it"), "{e}");
    }

    #[tokio::test]
    async fn a_desk_waiting_with_nothing_queued_is_answered_empty() {
        let q = Jobs::default();
        assert!(q.next("kim", Duration::from_millis(10)).await.is_empty());
    }

    #[tokio::test]
    async fn the_queue_for_one_person_is_bounded() {
        let q = Arc::new(Jobs::default());
        for _ in 0..MOST_QUEUED {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.ask("kim", job("current"), Duration::from_secs(5)).await });
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let e = q
            .ask("kim", job("current"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(e.contains("too many"), "{e}");
        assert_eq!(q.take("kim").len(), MOST_QUEUED);
    }

    #[tokio::test]
    async fn a_call_outside_a_persons_turn_is_refused() {
        let tool = ThemeTool::all("atlas")
            .into_iter()
            .find(|t| t.name == "theme.propose")
            .unwrap();
        let r = tool.execute(json!({ "name": "Moss" })).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn a_turn_of_someone_without_an_identity_is_refused_at_once() {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_USER_ID};
        let tool = ThemeTool::all("atlas")
            .into_iter()
            .find(|t| t.name == "theme.current")
            .unwrap();
        let r = CURRENT_USER_ID
            .scope(
                "slack-U123".into(),
                CURRENT_SESSION_ID.scope("s1".into(), tool.execute(json!({}))),
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(jobs().take("slack-U123").is_empty());
    }

    #[tokio::test]
    async fn a_proposal_outside_a_conversation_is_refused() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let tool = ThemeTool::all("atlas")
            .into_iter()
            .find(|t| t.name == "theme.propose")
            .unwrap();
        let r = CURRENT_USER_ID
            .scope(
                "6f1c2a3b-4d5e-4f60-8a1b-2c3d4e5f6a7b".into(),
                tool.execute(json!({ "name": "Moss" })),
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(jobs()
            .take("6f1c2a3b-4d5e-4f60-8a1b-2c3d4e5f6a7b")
            .is_empty());
    }

    #[test]
    fn the_tools_change_nothing_on_this_computer() {
        for t in ThemeTool::all("atlas") {
            assert_eq!(t.effect(&json!({})), workforce_core::computer::Effect::None);
        }
    }

    mod routes {
        use super::*;
        use crate::remote_mcp::testing::{
            fixture_with_devices, issue, pair_device, pair_device_as, TokenShape,
        };
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

        struct Hub {
            app: Router,
            state: Arc<GatewayState>,
            jwt: crate::jwt::JwtConfig,
            ann: Identity,
            ann_desk: String,
            ann_phone: String,
            ann_mcp: String,
            bob_desk: String,
        }

        fn hub() -> Hub {
            let (fx, devices) = fixture_with_devices(false);
            let mut ann = Identity::new("Ann", IdentityRole::Member);
            let mut bob = Identity::new("Bob", IdentityRole::Member);
            fx.identities.create(&ann).unwrap();
            fx.identities.create(&bob).unwrap();
            let (_, ann_desk) = pair_device(&fx, &devices, &mut ann, "Ann's Mac");
            let (_, ann_phone) = pair_device_as(&fx, &devices, &mut ann, "Ann's phone", "ios");
            let (_, bob_desk) = pair_device(&fx, &devices, &mut bob, "Bob's Mac");
            let ann_mcp = issue(&fx, &ann, TokenShape::default());
            let jwt =
                crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600)
                    .unwrap();
            let state = Arc::new(GatewayState {
                jwt_config: Some(jwt.clone()),
                ..Arc::try_unwrap(fx.state)
                    .ok()
                    .expect("the fixture's only handle")
            });
            Hub {
                app: router(Arc::clone(&state)),
                state,
                jwt,
                ann,
                ann_desk,
                ann_phone,
                ann_mcp,
                bob_desk,
            }
        }

        impl Hub {
            async fn ask(
                &self,
                method: &str,
                uri: &str,
                bearer: Option<&str>,
                body: &str,
            ) -> StatusCode {
                let mut req = Request::builder().method(method).uri(uri);
                if let Some(b) = bearer {
                    req = req.header("authorization", format!("Bearer {b}"));
                }
                let req = req
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                self.app.clone().oneshot(req).await.unwrap().status()
            }

            fn sign_in(&self, who: &Identity) -> String {
                crate::jwt::generate_token_with_identity(
                    &self.jwt,
                    &who.display_name,
                    who.role,
                    Some(who.id.to_string()),
                )
                .unwrap()
            }
        }

        const ANSWER: &str = r#"{"ok":true,"body":{}}"#;

        #[tokio::test]
        async fn only_the_persons_own_desk_or_sign_in_takes_and_answers_calls() {
            let hub = hub();
            let take = format!("{JOBS_PATH}?wait=0");
            let answer = format!("{JOBS_PATH}/{}", Uuid::new_v4());
            let sign_in = hub.sign_in(&hub.ann);
            for (who, bearer) in [
                ("Ann's Desk", hub.ann_desk.as_str()),
                ("Ann's sign-in", sign_in.as_str()),
            ] {
                assert_eq!(
                    hub.ask("GET", &take, Some(bearer), "").await,
                    StatusCode::OK,
                    "{who}"
                );
                assert_eq!(
                    hub.ask("POST", &answer, Some(bearer), ANSWER).await,
                    StatusCode::NOT_FOUND,
                    "{who}"
                );
            }
            let refused = [
                ("no one", None),
                ("the hub's own token", Some(hub.state.auth_token.as_str())),
                ("Ann's phone", Some(hub.ann_phone.as_str())),
                ("Ann's remote MCP token", Some(hub.ann_mcp.as_str())),
                ("an unknown token", Some("wfp_not-a-device")),
            ];
            for (who, bearer) in refused {
                let s = hub.ask("GET", &take, bearer, "").await;
                assert!(
                    matches!(s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
                    "{who} takes: {s}"
                );
                // Refused before the body is read: junk is no 400.
                let s = hub.ask("POST", &answer, bearer, "junk").await;
                assert!(
                    matches!(s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
                    "{who} answers: {s}"
                );
            }
        }

        #[tokio::test]
        async fn another_persons_call_is_never_theirs_to_take_or_answer() {
            let hub = hub();
            let ann = hub.ann.id.to_string();
            let q = jobs();
            let asking = tokio::spawn(async move {
                jobs()
                    .ask(&ann, job("propose"), Duration::from_secs(5))
                    .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            let id = q.inner.lock().queued.get(&hub.ann.id.to_string()).unwrap()[0].id;
            let bob = Some(hub.bob_desk.as_str());
            assert_eq!(
                hub.ask("POST", &format!("{JOBS_PATH}/{id}"), bob, ANSWER)
                    .await,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                hub.ask(
                    "POST",
                    &format!("{JOBS_PATH}/{id}"),
                    Some(hub.ann_desk.as_str()),
                    ANSWER
                )
                .await,
                StatusCode::NO_CONTENT
            );
            assert!(asking.await.unwrap().unwrap().ok);
        }

        #[tokio::test]
        async fn someone_suspended_takes_nothing() {
            let hub = hub();
            let mut ann = hub.ann.clone();
            ann.status = IdentityStatus::Suspended;
            hub.state
                .identity_store
                .as_ref()
                .unwrap()
                .update(&ann)
                .unwrap();
            let s = hub
                .ask(
                    "GET",
                    &format!("{JOBS_PATH}?wait=0"),
                    Some(hub.ann_desk.as_str()),
                    "",
                )
                .await;
            assert!(
                matches!(s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
                "{s}"
            );
        }
    }
}

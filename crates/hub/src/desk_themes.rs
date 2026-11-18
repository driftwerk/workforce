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
//! 3. What Desk answered is the tool's result. No answer in time: the call
//!    is dropped, never done later, and the agent is told Desk didn't
//!    answer.
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
//! name, and writes only theme proposals, only in its themes folder.

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

use crate::gateway::GatewayState;

/// The tools, the one that changes nothing first.
pub const NAMES: [&str; 3] = ["theme.current", "theme.propose", "theme.revise"];

/// How long a call waits for the person's Desk to answer.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);
/// The longest Desk's ask for calls is held open.
const HOLD_MOST: Duration = Duration::from_secs(25);
/// The most calls waiting for one person's Desk at once.
const MOST_QUEUED: usize = 8;
/// The largest call carried: a theme is a few dozen colours.
const MOST_INPUT: usize = 16 * 1024;
/// The largest answer taken from Desk: a theme's every colour, and the
/// names of the person's themes.
pub const MOST_ANSWER: usize = 256 * 1024;

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
                },
            );
        }
        self.queued.notify_waiters();
        let answer = tokio::time::timeout(within, rx).await;
        // Never done later: a call not answered in time goes.
        self.forget(person, id);
        match answer {
            Ok(Ok(answer)) => Ok(answer),
            _ => Err(
                "Desk didn't answer. The person's Desk has to be open and signed in to this hub: \
                 ask them to open it, then try again."
                    .into(),
            ),
        }
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
        self.inner
            .lock()
            .queued
            .remove(person)
            .map(Vec::from)
            .unwrap_or_default()
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
Tell them in a sentence what you made, and what Desk moved, if anything.";

const REVISE: &str = "Change a theme proposal, or one of the person's own themes, from what they \
said about it: give its id (from theme.propose, or theme.current's list) and only what changes. A \
colour under `colors` set to null goes back to what Desk works out. Desk checks how it reads \
again, and saves the change as a proposal beside the theme: a theme of theirs is never changed \
until they keep it. Desk's built-in themes can't be changed: propose a new one from their \
colours (theme.current) instead.";

const CURRENT: &str = "The theme the person has on in Desk: its id, name, whether it is dark, its \
base colours, every colour Desk draws with by name (the terminal's included), and the themes and \
proposals they have. Start a theme from it, or change one, by name.";

fn hex() -> Value {
    json!({ "type": "string", "pattern": "^#[0-9A-Fa-f]{6}$" })
}

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
                    "background": hex(),
                    "signal": hex(),
                    "text": hex(),
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
                    "background": hex(),
                    "signal": hex(),
                    "text": { "type": ["string", "null"], "description": "A colour, or null for Desk's own" },
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
        Some(ANSWER_WITHIN + Duration::from_secs(10))
    }

    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_USER_ID};
        let started = Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        let Some(person) = CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty())
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
    token: Option<String>,
    /// How long to hold the ask open for a call, in seconds.
    wait: Option<u64>,
}

fn refused(status: StatusCode, e: &str) -> Response {
    (status, Json(json!({ "error": e }))).into_response()
}

/// The person asking, by their own sign-in or a device paired with them:
/// never the hub's own token, which is no one's.
async fn person(
    state: &GatewayState,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = crate::gateway::resolve_caller_context(state, headers, token).await;
    if caller.auth_failure.is_some() {
        return Err(refused(StatusCode::UNAUTHORIZED, "not allowed"));
    }
    caller
        .identity_id
        .filter(|p| !p.is_empty())
        .ok_or_else(|| refused(StatusCode::UNAUTHORIZED, "Sign in as yourself"))
}

async fn jobs_route(
    State(state): State<Arc<GatewayState>>,
    h: HeaderMap,
    Query(q): Query<JobsQuery>,
) -> Response {
    let person = match person(&state, &h, q.token.as_deref()).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let hold = Duration::from_secs(q.wait.unwrap_or(0)).min(HOLD_MOST);
    Json(json!({ "jobs": jobs().next(&person, hold).await })).into_response()
}

#[derive(Deserialize)]
struct AnswerQuery {
    token: Option<String>,
}

async fn answer_route(
    State(state): State<Arc<GatewayState>>,
    h: HeaderMap,
    Query(q): Query<AnswerQuery>,
    Path(id): Path<Uuid>,
    Json(answer): Json<Answer>,
) -> Response {
    let person = match person(&state, &h, q.token.as_deref()).await {
        Ok(p) => p,
        Err(r) => return r,
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
    async fn a_proposal_outside_a_conversation_is_refused() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let tool = ThemeTool::all("atlas")
            .into_iter()
            .find(|t| t.name == "theme.propose")
            .unwrap();
        let r = CURRENT_USER_ID
            .scope("kim".into(), tool.execute(json!({ "name": "Moss" })))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(jobs().take("kim").is_empty());
    }

    #[test]
    fn the_tools_change_nothing_on_this_computer() {
        for t in ThemeTool::all("atlas") {
            assert_eq!(t.effect(&json!({})), workforce_core::computer::Effect::None);
        }
    }
}

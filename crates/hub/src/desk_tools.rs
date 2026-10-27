//! `desk.*` — agents start, prompt, read and wait on the person's Claude Code
//! and Codex sessions, so one agent can fan work out to several sessions.
//! Host-exec class: gated as `ShellExec` (authz.rs).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_desk::model::{Decision, Engine, NewSession, SessionState};
use workforce_desk::{Desk, DeskError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    List,
    Start,
    Prompt,
    Read,
    Wait,
    Approve,
    Stop,
}

const VERBS: [Verb; 7] = [
    Verb::List,
    Verb::Start,
    Verb::Prompt,
    Verb::Read,
    Verb::Wait,
    Verb::Approve,
    Verb::Stop,
];

pub const NAMES: [&str; 7] = [
    "desk.sessions",
    "desk.start",
    "desk.prompt",
    "desk.read",
    "desk.wait",
    "desk.approve",
    "desk.stop",
];

const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
const TERMINAL_CHARS: usize = 4000;

pub struct DeskTool {
    verb: Verb,
    desk: Desk,
}

impl DeskTool {
    pub fn all(desk: &Desk) -> Vec<Self> {
        VERBS
            .into_iter()
            .map(|verb| Self {
                verb,
                desk: desk.clone(),
            })
            .collect()
    }
}

fn id_schema() -> Value {
    json!({ "type": "string", "description": "Session id from desk.sessions or desk.start" })
}

#[derive(Deserialize)]
struct IdInput {
    session_id: Uuid,
}

#[derive(Deserialize)]
struct StartInput {
    engine: Option<Engine>,
    name: String,
    cwd: String,
    goal: Option<String>,
    worktree: Option<String>,
    prompt: Option<String>,
}

#[derive(Deserialize)]
struct PromptInput {
    session_id: Uuid,
    prompt: String,
}

#[derive(Deserialize)]
struct ReadInput {
    session_id: Uuid,
    #[serde(default)]
    terminal: bool,
}

#[derive(Deserialize)]
struct WaitInput {
    session_id: Uuid,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ApproveInput {
    session_id: Uuid,
    approval_id: Uuid,
    allow: bool,
}

fn out<T: serde::Serialize>(v: T) -> Result<Value, DeskError> {
    serde_json::to_value(v).map_err(|e| DeskError::Internal(e.into()))
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, DeskError> {
    serde_json::from_value(input).map_err(|e| DeskError::Invalid(e.to_string()))
}

impl DeskTool {
    async fn run(&self, input: Value) -> Result<Value, DeskError> {
        let d = &self.desk;
        match self.verb {
            Verb::List => out(d.list()),
            Verb::Start => {
                let i: StartInput = parse(input)?;
                out(d
                    .create(NewSession {
                        engine: i.engine.unwrap_or(Engine::Claude),
                        name: i.name,
                        cwd: i.cwd,
                        goal: i.goal.unwrap_or_default(),
                        worktree: i.worktree,
                        prompt: i.prompt,
                        approvals: None,
                    })
                    .await?)
            }
            Verb::Prompt => {
                let i: PromptInput = parse(input)?;
                out(d.prompt(i.session_id, &i.prompt)?)
            }
            Verb::Read => {
                let i: ReadInput = parse(input)?;
                let mut v = out(d.view(i.session_id)?)?;
                if i.terminal {
                    v["terminal"] = Value::String(d.terminal_text(i.session_id, TERMINAL_CHARS)?);
                }
                Ok(v)
            }
            Verb::Wait => {
                let i: WaitInput = parse(input)?;
                let timeout = Duration::from_secs(i.timeout_secs.unwrap_or(600)).min(MAX_WAIT);
                out(d
                    .wait_for(
                        i.session_id,
                        &[
                            SessionState::Done,
                            SessionState::Blocked,
                            SessionState::Idle,
                        ],
                        timeout,
                    )
                    .await?)
            }
            Verb::Approve => {
                let i: ApproveInput = parse(input)?;
                let decision = if i.allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                };
                out(d.decide(i.session_id, i.approval_id, decision)?)
            }
            Verb::Stop => {
                let i: IdInput = parse(input)?;
                out(d.stop(i.session_id)?)
            }
        }
    }
}

#[async_trait]
impl Tool for DeskTool {
    fn name(&self) -> &str {
        NAMES[VERBS.iter().position(|v| *v == self.verb).unwrap_or(0)]
    }

    fn description(&self) -> &str {
        match self.verb {
            Verb::List => "List the person's Claude Code and Codex sessions on this machine: name, goal, state (working, blocked = needs an answer, done, idle, interrupted, stopped), last prompt and reply.",
            Verb::Start => "Start a Claude Code (default) or Codex session in a folder on this machine, optionally on a new git worktree branch and with a first prompt. It runs in a real terminal the person can watch in Desk.",
            Verb::Prompt => "Type a prompt into a running session and submit it, as the person would.",
            Verb::Read => "Read one session's state, last prompt and last reply; with terminal=true also the end of its terminal as plain text.",
            Verb::Wait => "Wait until a session finishes its turn, needs an answer, or stops (default 10 minutes, at most 30). Returns its state either way.",
            Verb::Approve => "Answer a session's pending approval (from desk.read's pending.id) with allow or deny. Only for approvals the person asked you to handle.",
            Verb::Stop => "Stop a running session. Its conversation is kept and can be resumed in Desk.",
        }
    }

    fn input_schema(&self) -> Value {
        match self.verb {
            Verb::List => json!({ "type": "object", "properties": {} }),
            Verb::Start => json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "enum": ["claude", "codex"] },
                    "name": { "type": "string", "description": "Short name, e.g. auth-fix" },
                    "cwd": { "type": "string", "description": "Absolute path of the repo or folder" },
                    "goal": { "type": "string", "description": "What the session is for, shown on its card" },
                    "worktree": { "type": "string", "description": "Branch for a new git worktree" },
                    "prompt": { "type": "string", "description": "First prompt" }
                },
                "required": ["name", "cwd"]
            }),
            Verb::Prompt => json!({
                "type": "object",
                "properties": { "session_id": id_schema(), "prompt": { "type": "string" } },
                "required": ["session_id", "prompt"]
            }),
            Verb::Read => json!({
                "type": "object",
                "properties": { "session_id": id_schema(), "terminal": { "type": "boolean" } },
                "required": ["session_id"]
            }),
            Verb::Wait => json!({
                "type": "object",
                "properties": { "session_id": id_schema(), "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 1800 } },
                "required": ["session_id"]
            }),
            Verb::Approve => json!({
                "type": "object",
                "properties": {
                    "session_id": id_schema(),
                    "approval_id": { "type": "string" },
                    "allow": { "type": "boolean" }
                },
                "required": ["session_id", "approval_id", "allow"]
            }),
            Verb::Stop => json!({
                "type": "object",
                "properties": { "session_id": id_schema() },
                "required": ["session_id"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        Ok(match self.run(input).await {
            Ok(v) => ToolResult::success(v, ms()),
            Err(e) => ToolResult::failure(e.to_string(), ms()),
        })
    }
}

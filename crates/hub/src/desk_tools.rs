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
use workforce_core::budget::{BudgetScope, OnExceed, Period, Unit};
use workforce_desk::crew::{By, Party};
use workforce_desk::model::{Decision, Engine, NewSession, SessionState};
use workforce_desk::{Desk, DeskError, NewBudget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    List,
    Start,
    Prompt,
    Read,
    Wait,
    Approve,
    Stop,
    Interrupt,
    Budget,
}

const VERBS: [Verb; 9] = [
    Verb::List,
    Verb::Start,
    Verb::Prompt,
    Verb::Read,
    Verb::Wait,
    Verb::Approve,
    Verb::Stop,
    Verb::Interrupt,
    Verb::Budget,
];

pub const NAMES: [&str; 9] = [
    "desk.sessions",
    "desk.start",
    "desk.prompt",
    "desk.read",
    "desk.wait",
    "desk.approve",
    "desk.stop",
    "desk.interrupt",
    "desk.budget",
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
    /// The person's account to run on, by name; none: the default.
    account: Option<String>,
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
#[serde(rename_all = "snake_case")]
enum BudgetAction {
    List,
    Set,
    Remove,
}

#[derive(Deserialize)]
struct BudgetInput {
    action: BudgetAction,
    scope: Option<String>,
    group: Option<String>,
    period: Option<Period>,
    unit: Option<Unit>,
    amount: Option<f64>,
    warn_at: Option<f64>,
    on_exceed: Option<OnExceed>,
    budget_id: Option<Uuid>,
}

impl BudgetInput {
    fn new_budget(self) -> Result<NewBudget, DeskError> {
        let need = |what: &str| DeskError::Invalid(format!("set needs {what}"));
        Ok(NewBudget {
            scope: BudgetScope::parse(self.scope.as_deref().ok_or_else(|| need("scope"))?)
                .map_err(|e| DeskError::Invalid(e.to_string()))?,
            group: self.group,
            period: self.period.ok_or_else(|| need("period"))?,
            unit: self.unit.unwrap_or(Unit::Usd),
            amount: self.amount.ok_or_else(|| need("amount"))?,
            warn_at: self.warn_at,
            on_exceed: self.on_exceed,
        })
    }
}

#[derive(Deserialize)]
struct ApproveInput {
    session_id: Uuid,
    approval_id: Uuid,
    allow: bool,
}

/// The agent calling the tool, as the actor of what it does.
fn caller() -> Option<By> {
    let id = workforce_agent::runner::CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())?;
    Some(By {
        party: Party::agent(&id, &id),
        reason: None,
        suggestion_id: None,
        hop: 0,
    })
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
            Verb::List => {
                let query = input["query"].as_str().unwrap_or_default();
                out(d.find_sessions(query))
            }
            Verb::Start => {
                let i: StartInput = parse(input)?;
                let engine = i.engine.unwrap_or(Engine::Claude);
                let account = match i.account.as_deref().map(str::trim) {
                    Some(name) if !name.is_empty() => Some(Some(d.account_named(engine, name)?)),
                    _ => None,
                };
                out(d
                    .create(
                        NewSession {
                            engine,
                            view: None,
                            name: i.name,
                            cwd: i.cwd,
                            goal: i.goal.unwrap_or_default(),
                            worktree: i.worktree,
                            prompt: i.prompt,
                            approvals: None,
                            approve_for_me: false,
                            account,
                            env: Default::default(),
                            group: None,
                        },
                        caller(),
                    )
                    .await?)
            }
            Verb::Prompt => {
                let i: PromptInput = parse(input)?;
                out(d.prompt(i.session_id, &i.prompt, caller())?)
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
                // What Codex asks is to leave its sandbox: the person's alone.
                let codex = d
                    .view(i.session_id)
                    .ok()
                    .and_then(|v| v.pending)
                    .is_some_and(|p| p.id == i.approval_id && !p.choices.is_empty());
                if codex {
                    return Err(DeskError::Invalid(
                        "Codex's requests are the person's to answer: tell them in Desk".into(),
                    ));
                }
                let decision = if i.allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                };
                out(d.decide(i.session_id, i.approval_id, decision, caller())?)
            }
            Verb::Stop => {
                let i: IdInput = parse(input)?;
                out(d.stop(i.session_id, caller())?)
            }
            Verb::Interrupt => {
                let i: IdInput = parse(input)?;
                out(d.interrupt(i.session_id, caller())?)
            }
            Verb::Budget => {
                let i: BudgetInput = parse(input)?;
                // What an agent changes is the person's, done as it: never
                // a way to let anything the breaker holds go on.
                let by = caller().unwrap_or_else(By::you);
                match i.action {
                    BudgetAction::List => out(d.budgets_view().await?),
                    BudgetAction::Set => out(d.put_budget(i.new_budget()?, by).await?),
                    BudgetAction::Remove => {
                        let id = i.budget_id.ok_or_else(|| {
                            DeskError::Invalid("remove needs budget_id (from list)".into())
                        })?;
                        d.delete_budget(id, by).await?;
                        Ok(json!({ "removed": id }))
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Tool for DeskTool {
    fn name(&self) -> &str {
        NAMES[VERBS.iter().position(|v| *v == self.verb).unwrap_or(0)]
    }

    /// Starting a session is a session in its folder. Typing into one,
    /// answering its approvals, stopping or interrupting it act on a session
    /// the person can watch, under that session's own approvals, and reading
    /// Desk touches no file: none of them is decided here.
    fn effect(&self, input: &Value) -> workforce_core::computer::Effect {
        match self.verb {
            Verb::Start => workforce_core::computer::Effect::Session {
                cwd: input["cwd"]
                    .as_str()
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| workforce_agent::tools::paths::resolve_real(std::path::Path::new(c))),
            },
            Verb::List
            | Verb::Prompt
            | Verb::Read
            | Verb::Wait
            | Verb::Approve
            | Verb::Stop
            | Verb::Interrupt
            | Verb::Budget => workforce_core::computer::Effect::None,
        }
    }

    fn description(&self) -> &str {
        match self.verb {
            Verb::List => "List the person's Claude Code and Codex sessions on this machine: displayName (what the person calls it — use it first), name (the hub's), cwd, group (the sidebar group it is in), goal, state (working, blocked = needs an answer, asking = its turn ended asking the person something, done, idle, interrupted, stopped), last prompt and reply. With query, only the sessions it names: matched on the person's names, the hub's, the folder and the group, ignoring case and on parts of words (\"ledger\", \"the Atlas sessions\").",
            Verb::Start => "Start a Claude Code (default) or Codex session in a folder on this machine, optionally on a new git worktree branch and with a first prompt. It runs in a real terminal the person can watch in Desk.",
            Verb::Prompt => "Type a prompt into a running session and submit it, as the person would.",
            Verb::Read => "Read one session's state, last prompt and last reply; with terminal=true also the end of its terminal as plain text.",
            Verb::Wait => "Wait until a session finishes its turn, needs an answer, or stops (default 10 minutes, at most 30). Returns its state either way.",
            Verb::Approve => "Answer a session's pending approval (from desk.read's pending.id) with allow or deny. Only for approvals the person asked you to handle.",
            Verb::Stop => "Stop a running session. Its conversation is kept and can be resumed in Desk. A session the person started in a Desk terminal can't be stopped (the shell is theirs): interrupt it instead.",
            Verb::Budget => "The person's spend budgets on Desk, set when they ask. action=list shows each budget with where it stands, and what Desk's breaker holds. action=set sets one (one for the same scope, group, period and unit is replaced): scope is desk (everything together), each_session (each session without its own), session:<id>, each_agent (each agent without its own) or agent:<agent id>; group limits it to a sidebar group's; period five_hours, day, week or month; unit usd (default) or tokens; warn_at between 0 and 1 (default 0.8); on_exceed pause (default: the session finishes its turn, then waits for the person; an agent's next turns wait; agents can't start new sessions while a total is held) or warn. action=remove removes budget_id. Only the person can let something held go on, on Desk's Floor.",
            Verb::Interrupt => "End the turn a working session is on, as the person pressing Esc in its terminal would. The session and its terminal keep running, and a session the person started in a Desk terminal can be interrupted too; the shell stays theirs. Does nothing to a session that isn't working.",
        }
    }

    fn input_schema(&self) -> Value {
        match self.verb {
            Verb::List => json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words that name the sessions: a name the person gave, the hub's name, the folder, or the group" }
                }
            }),
            Verb::Start => json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "enum": workforce_desk::engine::startable() },
                    "name": { "type": "string", "description": "Short name, e.g. auth-fix" },
                    "cwd": { "type": "string", "description": "Absolute path of the repo or folder" },
                    "goal": { "type": "string", "description": "What the session is for, shown on its card" },
                    "worktree": { "type": "string", "description": "Branch for a new git worktree" },
                    "prompt": { "type": "string", "description": "First prompt" },
                    "account": { "type": "string", "description": "The person's account to run on, by name (Work, Personal); leave out for their default" }
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
            Verb::Budget => json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "set", "remove"] },
                    "scope": { "type": "string", "description": "desk, each_session, session:<id>, each_agent, or agent:<agent id>" },
                    "group": { "type": "string", "description": "A sidebar group's id, for that group's budget" },
                    "period": { "type": "string", "enum": ["five_hours", "day", "week", "month"] },
                    "unit": { "type": "string", "enum": ["usd", "tokens"] },
                    "amount": { "type": "number", "exclusiveMinimum": 0 },
                    "warn_at": { "type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1 },
                    "on_exceed": { "type": "string", "enum": ["pause", "warn"] },
                    "budget_id": { "type": "string", "description": "For remove: the id from list" }
                },
                "required": ["action"]
            }),
            Verb::Stop | Verb::Interrupt => json!({
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

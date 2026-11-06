//! What the overseer can do to the crew, whether its level lets it, and
//! doing it — for the overseer's tools, and for the person accepting one of
//! its suggestions.

use std::borrow::Cow;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Act, Level, Oversight, Scope, TakeOn};
use workforce_desk::board::TicketState;
use workforce_desk::charge::How;
use workforce_desk::crew::{self, By, Kind, Party};
use workforce_desk::engine::{self, Deliver, View};
use workforce_desk::model::{Engine, NewSession, Origin};
use workforce_desk::scope::Seen;
use workforce_desk::{AssignTo, ChargedTo, Move, NewSessionFor, Speaker};

use super::read_only::read_only_in;
use super::CrewHub;

/// Who a nudge goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Session(Uuid),
    Agent(String),
}

/// Whom a ticket is handed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Handout {
    Session(Uuid),
    Agent(String),
    /// A session started for it, which the overseer runs: `engine` None is
    /// `[oversight.work] engine`.
    NewSession {
        folder: String,
        engine: Option<Engine>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// A message to a session (on its line, waiting for its turn to end)
    /// or an agent; with `reply_to`, the answer to that session's question.
    Nudge {
        to: Target,
        message: String,
        reply_to: Option<Uuid>,
    },
    /// Give a session's question to the person: why, the answers to offer,
    /// and a draft of one.
    Escalate {
        message: Uuid,
        why: String,
        options: Vec<String>,
        draft: Option<String>,
    },
    Approve {
        session: Uuid,
        approval: Uuid,
    },
    Deny {
        session: Uuid,
        approval: Uuid,
    },
    Stop {
        session: Uuid,
    },
    /// End the turn it is on (Esc in its terminal); it runs on.
    Interrupt {
        session: Uuid,
    },
    Resume {
        session: Uuid,
        prompt: Option<String>,
    },
    Restart {
        session: Uuid,
        prompt: Option<String>,
    },
    /// A visible session the overseer runs from the start.
    StartSession {
        folder: String,
        goal: String,
        name: Option<String>,
        /// None: `[oversight.work] engine`.
        engine: Option<Engine>,
        view: Option<View>,
        /// A new branch to work on, in a worktree of its own.
        worktree: Option<String>,
    },
    Assign {
        agent: String,
        task: String,
    },
    /// Hand a ticket on the board (by its key) to a session, an agent or a
    /// session started for it.
    AssignTicket {
        ticket: String,
        /// The ticket's id, once it is resolved ([`CrewHub::resolve_ticket`]):
        /// a suggestion keeps it, so that it is that ticket that is acted
        /// on, whatever the key means by then.
        id: Option<Uuid>,
        /// Its title and body as resolved, which the hand-out says to
        /// whoever gets it.
        text: String,
        to: Handout,
    },
    /// Move a ticket on the board (by its key) to a column.
    MoveTicket {
        ticket: String,
        id: Option<Uuid>,
        state: TicketState,
        /// The column it is in as resolved.
        was: Option<TicketState>,
        /// Whether the move takes it off a session started in another
        /// terminal, which is then told.
        off_external: bool,
    },
    /// Run a session for the person: it wakes the overseer at each turn's
    /// end, and waits for it while the person isn't driving.
    TakeOn {
        session: Uuid,
    },
    /// Let a session it runs go: the person runs it on their own.
    Release {
        session: Uuid,
    },
}

#[derive(Deserialize)]
struct Input {
    session_id: Option<String>,
    agent_id: Option<String>,
    approval_id: Option<String>,
    message: Option<String>,
    prompt: Option<String>,
    /// `path` before it was `folder`: suggestions kept from then say it.
    path: Option<String>,
    folder: Option<String>,
    goal: Option<String>,
    name: Option<String>,
    engine: Option<Engine>,
    view: Option<View>,
    worktree: Option<String>,
    task: Option<String>,
    ticket: Option<String>,
    ticket_id: Option<String>,
    to: Option<Value>,
    state: Option<TicketState>,
    reply_to: Option<String>,
    message_id: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    draft: Option<String>,
}

fn id(raw: Option<&str>, what: &str) -> Result<Uuid, String> {
    raw.ok_or_else(|| format!("Say which {what}."))?
        .trim()
        .parse()
        .map_err(|_| format!("That isn’t a {what} id from crew.overview."))
}

fn text(raw: Option<String>, what: &str) -> Result<String, String> {
    raw.map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("Say {what}."))
}

/// The key of a ticket as typed: `acm-12` is `ACM-12`.
fn ticket_key(raw: Option<String>) -> Result<String, String> {
    let key = text(raw, "which ticket, by its key (like ACM-12)")?;
    workforce_desk::board::parse_key(&key)
        .map(|(prefix, n)| format!("{prefix}-{n}"))
        .ok_or_else(|| format!("{key:?} isn’t a ticket’s key, like ACM-12."))
}

/// The id a resolved ticket is kept by in a suggestion.
fn ticket_id(raw: Option<String>) -> Result<Option<Uuid>, String> {
    raw.map(|r| {
        r.trim()
            .parse()
            .map_err(|_| "That isn’t a ticket id.".to_string())
    })
    .transpose()
}

/// Who `to` hands a ticket to: `{ "session": id }`, `{ "agent": id }` or
/// `{ "newSession": { "folder", "engine" } }`.
fn handout(to: Option<Value>) -> Result<Handout, String> {
    let say = || {
        "Say who gets it: { \"session\": id from crew.overview }, { \"agent\": id } or { \"newSession\": { \"folder\": path, \"engine\": optional } }.".to_string()
    };
    let to = to.ok_or_else(say)?;
    fn word(v: &Value) -> Option<&str> {
        v.as_str().map(str::trim).filter(|v| !v.is_empty())
    }
    if let Some(s) = to.get("session") {
        return Ok(Handout::Session(id(word(s), "session")?));
    }
    if let Some(a) = to.get("agent") {
        return Ok(Handout::Agent(
            word(a)
                .ok_or_else(|| "Say which agent.".to_string())?
                .into(),
        ));
    }
    if let Some(n) = to.get("newSession").or_else(|| to.get("new_session")) {
        let folder = n
            .get("folder")
            .or_else(|| n.get("path"))
            .and_then(word)
            .ok_or_else(|| "Say the project’s folder for the new session.".to_string())?;
        let engine = match n.get("engine").and_then(word) {
            Some(e) => {
                Some(Engine::parse(e).map_err(|_| format!("{e} isn’t an engine Desk knows."))?)
            }
            None => None,
        };
        return Ok(Handout::NewSession {
            folder: folder.into(),
            engine,
        });
    }
    Err(say())
}

fn blank_none(raw: Option<String>) -> Option<String> {
    raw.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

impl Action {
    /// The action a `crew.*` tool's input asks for.
    pub(crate) fn parse(tool: &str, input: &Value) -> Result<Self, String> {
        let i: Input = serde_json::from_value(input.clone()).map_err(|e| e.to_string())?;
        let session = || id(i.session_id.as_deref(), "session");
        Ok(match tool {
            "crew.nudge" => {
                let to = match (&i.session_id, &i.agent_id) {
                    (Some(_), _) => Target::Session(session()?),
                    (None, Some(a)) => Target::Agent(a.trim().to_string()),
                    (None, None) => return Err("Say which session or agent to nudge.".into()),
                };
                let reply_to = i
                    .reply_to
                    .as_deref()
                    .filter(|r| !r.trim().is_empty())
                    .map(|r| id(Some(r), "question (a message id from the line)"))
                    .transpose()?;
                if reply_to.is_some() && !matches!(to, Target::Session(_)) {
                    return Err(
                        "Only a session's question can be answered: say its session_id.".into(),
                    );
                }
                Self::Nudge {
                    to,
                    message: text(i.message, "what to ask")?,
                    reply_to,
                }
            }
            "crew.escalate" => Self::Escalate {
                message: id(
                    i.message_id.as_deref(),
                    "question (a message id from the line)",
                )?,
                why: text(i.reason, "why it is the person's to answer")?,
                options: i
                    .options
                    .into_iter()
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .take(6)
                    .collect(),
                draft: blank_none(i.draft),
            },
            "crew.approve" | "crew.deny" => {
                let session = session()?;
                let approval = id(i.approval_id.as_deref(), "approval")?;
                if tool == "crew.approve" {
                    Self::Approve { session, approval }
                } else {
                    Self::Deny { session, approval }
                }
            }
            "crew.stop" => Self::Stop {
                session: session()?,
            },
            "crew.interrupt" => Self::Interrupt {
                session: session()?,
            },
            "crew.resume" => Self::Resume {
                session: session()?,
                prompt: blank_none(i.prompt),
            },
            "crew.restart" => Self::Restart {
                session: session()?,
                prompt: blank_none(i.prompt),
            },
            "crew.start_session" => Self::StartSession {
                folder: text(blank_none(i.folder).or(i.path), "the project's folder")?,
                goal: text(i.goal, "the goal")?,
                name: blank_none(i.name),
                engine: i.engine,
                view: i.view,
                worktree: blank_none(i.worktree),
            },
            "crew.assign" => Self::Assign {
                agent: text(i.agent_id, "which agent")?,
                task: text(i.task, "the task")?,
            },
            "crew.assign_ticket" => Self::AssignTicket {
                ticket: ticket_key(i.ticket)?,
                id: ticket_id(i.ticket_id)?,
                text: String::new(),
                to: handout(i.to)?,
            },
            "crew.move_ticket" => Self::MoveTicket {
                ticket: ticket_key(i.ticket)?,
                id: ticket_id(i.ticket_id)?,
                state: i
                    .state
                    .ok_or("Say which column: backlog, todo, doing, review, done or dropped.")?,
                was: None,
                off_external: false,
            },
            "crew.take_on" => Self::TakeOn {
                session: session()?,
            },
            "crew.release" => Self::Release {
                session: session()?,
            },
            other => return Err(format!("{other} is not something the overseer can do")),
        })
    }

    /// The key of the ticket it is about.
    pub(crate) fn ticket_key(&self) -> Option<&str> {
        match self {
            Self::AssignTicket { ticket, .. } | Self::MoveTicket { ticket, .. } => Some(ticket),
            _ => None,
        }
    }

    /// The id of the ticket it is about, once resolved.
    pub(crate) fn ticket_id(&self) -> Option<Uuid> {
        match self {
            Self::AssignTicket { id, .. } | Self::MoveTicket { id, .. } => *id,
            _ => None,
        }
    }

    /// Whether it starts a session.
    pub(crate) fn starts_session(&self) -> bool {
        matches!(
            self,
            Self::StartSession { .. }
                | Self::AssignTicket {
                    to: Handout::NewSession { .. },
                    ..
                }
        )
    }

    /// The action a suggestion holds.
    pub(crate) fn from_suggestion(action: &Value) -> Result<Self, String> {
        let tool = action["tool"]
            .as_str()
            .ok_or("That suggestion doesn’t say what to do.")?;
        Self::parse(tool, &action["input"])
    }

    /// Whether what it says reads as pushing code, deleting something, or
    /// reaching outside Desk. A hint only, which can make an action the
    /// person's call but never lets one run: what may run is decided by
    /// [`CrewHub::contained`].
    fn outward(&self) -> bool {
        let said = match self {
            Self::Nudge { message, .. } => message.as_str(),
            Self::Resume { prompt, .. } | Self::Restart { prompt, .. } => {
                prompt.as_deref().unwrap_or_default()
            }
            Self::StartSession { goal, .. } => goal.as_str(),
            Self::Assign { task, .. } => task.as_str(),
            Self::AssignTicket { .. }
            | Self::MoveTicket { .. }
            | Self::Approve { .. }
            | Self::Deny { .. }
            | Self::Stop { .. }
            | Self::Interrupt { .. }
            | Self::Escalate { .. }
            | Self::TakeOn { .. }
            | Self::Release { .. } => "",
        };
        outward(said)
    }
}

/// Words that name pushing, deploying, deleting or sending: each word of a
/// text is matched whole against `OUTWARD_WORDS`, and by its start against
/// `OUTWARD_STEMS` (so pushing, deployment, publishing, merged, released
/// and deleting are caught with push, deploy and the rest).
const OUTWARD_STEMS: &[&str] = &[
    "push",
    "deploy",
    "ship",
    "publish",
    "releas",
    "merg",
    "delet",
    "remov",
    "destroy",
    "wip",
    "purg",
    "send",
    "upload",
    "submit",
    "notif",
    "announc",
    "invit",
    "purchas",
    "email",
    "mail",
    "messag",
    "tweet",
    "slack",
    "migrat",
    "commit",
    "production",
    "rollout",
    "rebase",
    "drop",
    "force",
];
const OUTWARD_WORDS: &[&str] = &[
    "sent", "post", "posts", "posted", "posting", "pr", "prs", "rm", "dm", "share", "shares",
    "shared", "sharing", "reply", "replies", "replied", "pay", "pays", "paid", "buy", "order",
    "prod", "apply", "applies", "applied", "applying", "reset",
];

/// Whether `text` names something that pushes, deploys, deletes or sends.
/// Words can't say all that a message leads to ("may I go on?" with the
/// push only in what came before), so this only ever narrows what runs.
pub(crate) fn outward(text: &str) -> bool {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| OUTWARD_WORDS.contains(&w) || OUTWARD_STEMS.iter().any(|s| w.starts_with(s)))
}

/// A charge refused in the words the person reads, as they are.
fn charge_plain(e: workforce_desk::DeskError) -> String {
    match e {
        workforce_desk::DeskError::Conflict(why) | workforce_desk::DeskError::Invalid(why) => why,
        workforce_desk::DeskError::NotFound => "That session isn’t there any more.".into(),
        workforce_desk::DeskError::Internal(e) => {
            tracing::warn!(error = %e, "a charge failed");
            "Something went wrong on this Mac.".into()
        }
    }
}

/// A board change refused, in the words the person reads, as they are.
fn board_plain(e: workforce_desk::DeskError) -> String {
    match e {
        workforce_desk::DeskError::Conflict(why) | workforce_desk::DeskError::Invalid(why) => why,
        workforce_desk::DeskError::NotFound => "That ticket isn’t there any more.".into(),
        workforce_desk::DeskError::Internal(e) => {
            tracing::warn!(error = %e, "a board change failed");
            "Something went wrong on this Mac.".into()
        }
    }
}

/// Whether the overseer may do it now, or should suggest it instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gate {
    Run,
    /// Suggest it; why, for the overseer.
    Suggest(Cow<'static, str>),
    /// It may not even suggest (level `watch`).
    Refuse,
}

impl CrewHub {
    /// What `level` and `[oversight.act]` let the overseer do with `action`.
    pub(crate) fn gate(&self, action: &Action, o: &Oversight) -> Gate {
        // Asking the person is always allowed, and so is letting a session
        // go: the person runs it then, as before it was taken on.
        if let Action::Escalate { .. } | Action::Release { .. } = action {
            return match o.level {
                Level::Watch => Gate::Refuse,
                Level::Suggest | Level::Act => Gate::Run,
            };
        }
        match o.level {
            Level::Watch => return Gate::Refuse,
            Level::Suggest => return Gate::Suggest("you suggest; the person decides".into()),
            Level::Act => {}
        }
        // Taking on a session the person runs is theirs to say, unless
        // they let it take on every one.
        if let Action::TakeOn { session } = action {
            if self.taken_back(*session) {
                return Gate::Suggest("the person took this session back from you; offer to take it on again and they accept".into());
            }
            return match o.work.take_on {
                TakeOn::All => Gate::Run,
                TakeOn::Handed => Gate::Suggest("the person hands you the sessions you run; offer to take it on and they accept".into()),
            };
        }
        if let Some(gate) = self.gate_charged(action, o) {
            return gate;
        }
        if let Some(why) = self.uncontained(action, &o.act) {
            return Gate::Suggest(why.into());
        }
        let asked_outward = match action {
            Action::Nudge {
                reply_to: Some(ask),
                ..
            } => self.questions.lock().get(ask).is_none_or(|q| outward(q)),
            _ => false,
        };
        if action.outward() || asked_outward || self.ticket_outward(action) {
            return Gate::Suggest(
                "anything that pushes, deletes or reaches outside Desk is the person's call".into(),
            );
        }
        // Letting go of a ticket for good is the person's, whatever else
        // they let it hand out; so is bringing back one they dropped.
        if let Action::MoveTicket { state, was, .. } = action {
            if *state == TicketState::Dropped {
                return Gate::Suggest("dropping a ticket is the person's call".into());
            }
            if *was == Some(TicketState::Dropped) {
                return Gate::Suggest(
                    "the person dropped this ticket; bringing it back is their call".into(),
                );
            }
        }
        if let Action::MoveTicket {
            off_external: true, ..
        } = action
        {
            if !o.act.message_terminal_sessions {
                return Gate::Suggest(
                    "moving it tells a session in another terminal, which the person hasn't let you message without asking".into(),
                );
            }
        }
        let allowed = match action {
            Action::Nudge {
                reply_to: Some(_), ..
            } => o.act.answer_sessions,
            Action::Nudge { .. } => o.act.nudge,
            Action::Escalate { .. } => true,
            // Switched on by `message_agents`, above.
            Action::Assign { .. } => true,
            Action::Resume { .. } | Action::Restart { .. } => o.act.resume_restart,
            Action::StartSession { .. } => o.act.hand_out_work,
            Action::AssignTicket { .. } => o.act.hand_out_work,
            Action::MoveTicket { .. } => o.act.hand_out_work,
            Action::Approve { session, approval } => {
                // Codex's requests are to leave its sandbox, and what it says
                // of them isn't what runs: the person's alone.
                let codex = self.desk.view(*session).ok().is_some_and(|v| {
                    v.pending
                        .as_ref()
                        .is_some_and(|p| p.id == *approval && !p.choices.is_empty())
                });
                if codex {
                    return Gate::Suggest("Codex's requests are the person's to answer".into());
                }
                if !o.act.read_only_approvals {
                    false
                } else {
                    let view = self.desk.view(*session).ok();
                    let ok = view.as_ref().is_some_and(|v| {
                        v.pending
                            .as_ref()
                            .is_some_and(|p| p.id == *approval && read_only_in(&v.cwd, p))
                    });
                    if !ok {
                        return Gate::Suggest("only requests that just read inside the session's folder are allowed without the person".into());
                    }
                    true
                }
            }
            Action::Interrupt { .. } => o.act.interrupt,
            Action::Deny { .. } | Action::Stop { .. } => false,
            // Decided above.
            Action::TakeOn { .. } | Action::Release { .. } => true,
        };
        if allowed {
            Gate::Run
        } else {
            Gate::Suggest("the person hasn't let you do that without asking".into())
        }
    }

    /// Whether the ticket `action` hands out says something reaching
    /// outside Desk, as `crew.assign` and `crew.start_session` are judged by
    /// what they say: to an agent, to a session started for it, and to a
    /// session in another terminal (one Desk started waits on its
    /// approvals).
    fn ticket_outward(&self, action: &Action) -> bool {
        let Action::AssignTicket { to, text, .. } = action else {
            return false;
        };
        let says = match to {
            Handout::Agent(_) | Handout::NewSession { .. } => true,
            Handout::Session(s) => self
                .desk
                .view(*s)
                .is_ok_and(|v| v.origin == Origin::External),
        };
        says && outward(text)
    }

    /// The session `action` is done to, when it is one.
    fn session_of(action: &Action) -> Option<Uuid> {
        match action {
            Action::Nudge {
                to: Target::Session(s),
                ..
            }
            | Action::Approve { session: s, .. }
            | Action::Deny { session: s, .. }
            | Action::Stop { session: s }
            | Action::Interrupt { session: s }
            | Action::Resume { session: s, .. }
            | Action::Restart { session: s, .. } => Some(*s),
            _ => None,
        }
    }

    /// Whether the overseer of `scope` runs `session` now: it is in its
    /// charge.
    fn overseer_runs(&self, session: Uuid, scope: &Scope) -> bool {
        self.overseer_for(scope)
            .is_some_and(|(service, _)| self.runs(session, service.agent_id.as_str()))
    }

    /// What the overseer may do, at `act`, to a session it runs: answer
    /// for it what Desk reads in full and judges safe (Desk checks again as
    /// it answers), deny, and interrupt it whatever `[oversight.act]` says
    /// of sessions it doesn't run; resuming and restarting it stay the
    /// person's; only suggest while the person drives it. `None`: the usual rules decide (a plain
    /// message waits on its line while the person drives).
    fn gate_charged(&self, action: &Action, o: &Oversight) -> Option<Gate> {
        let session = Self::session_of(action).filter(|s| self.overseer_runs(*s, &o.scope))?;
        // What it leads to must wait on the person as for any session.
        if let Some(why) = self.uncontained(action, &o.act) {
            return Some(Gate::Suggest(why.into()));
        }
        let name = self.session_name(session);
        if self.desk.wheel_of(session).is_some_and(|w| w.person()) {
            return match action {
                Action::Nudge { reply_to: None, .. } => None,
                _ => Some(Gate::Suggest(
                    format!(
                        "the person is driving {name}, so I've held this for them as a suggestion"
                    )
                    .into(),
                )),
            };
        }
        let outward_said = || {
            Some(Gate::Suggest(
                "anything that pushes, deletes or reaches outside Desk is the person's call".into(),
            ))
        };
        match action {
            Action::Approve { approval, .. } => {
                Some(match self.desk.boss_answerable(session, *approval) {
                    Ok(()) => Gate::Run,
                    Err(why) => Gate::Suggest(
                        format!("it {why}, so it's the person's to allow; tell them why").into(),
                    ),
                })
            }
            Action::Deny { .. } => Some(Gate::Run),
            Action::Nudge {
                reply_to: Some(ask),
                ..
            } => {
                let asked_outward = self.questions.lock().get(ask).is_none_or(|q| outward(q));
                if action.outward() || asked_outward {
                    return outward_said();
                }
                Some(match o.act.answer_sessions {
                    true => Gate::Run,
                    false => Gate::Suggest(
                        "the person hasn't let you answer sessions without asking".into(),
                    ),
                })
            }
            Action::Interrupt { .. } => Some(Gate::Run),
            // A session started again reads its settings again: changes to
            // them it made by itself are the person's to let take effect.
            Action::Resume { .. } | Action::Restart { .. } => Some(Gate::Suggest(
                "starting it again loads whatever settings it changed; that's the person's call"
                    .into(),
            )),
            _ => None,
        }
    }

    /// Why what `action` leads to might not wait on the person, unless
    /// `act` lets it: `None` when it may be done without them. An
    /// allowlist, matched in full, of what is known to be held:
    ///
    /// - What a session Desk started does next waits on its approvals, which
    ///   reach the person in Desk or in its terminal, and which the overseer
    ///   may answer only for reads inside the session's folder.
    /// - Approving is judged by what it allows ([`read_only_in`]); denying,
    ///   stopping and interrupting reach nothing.
    ///
    /// An agent's turn runs under its own permissions, which can let it act
    /// outside Desk (open a pull request, file an issue, call an API)
    /// without asking, and a session started in another terminal may have
    /// been started not to ask: messages to either wait on the person unless
    /// they switched them on (`message_agents`, `message_terminal_sessions`).
    /// Resuming or restarting a session from another terminal always does.
    pub(crate) fn uncontained(&self, action: &Action, act: &Act) -> Option<&'static str> {
        let started_here = |session: &Uuid| {
            self.desk
                .view(*session)
                .is_ok_and(|v| v.origin == Origin::Desk)
        };
        let held = match action {
            Action::Nudge {
                to: Target::Session(s),
                ..
            } => match self.desk.view(*s).map(|v| v.origin) {
                Ok(Origin::Desk) => true,
                Ok(Origin::External) => act.message_terminal_sessions,
                Err(_) => false,
            },
            Action::Resume { session: s, .. } | Action::Restart { session: s, .. } => {
                started_here(s)
            }
            Action::StartSession { .. } => true,
            // What a ticket's session or agent is told is a message to it.
            Action::AssignTicket {
                to: Handout::Session(s),
                ..
            } => match self.desk.view(*s).map(|v| v.origin) {
                Ok(Origin::External) => act.message_terminal_sessions,
                _ => true,
            },
            Action::AssignTicket {
                to: Handout::Agent(_),
                ..
            } => {
                return (!act.message_agents).then_some(
                    "the person hasn't let you message agents without asking: an agent acts on a message under its own permissions",
                );
            }
            Action::AssignTicket {
                to: Handout::NewSession { .. },
                ..
            }
            | Action::MoveTicket { .. } => true,
            Action::Approve { .. }
            | Action::Deny { .. }
            | Action::Stop { .. }
            | Action::Interrupt { .. }
            | Action::Escalate { .. }
            | Action::TakeOn { .. }
            | Action::Release { .. } => true,
            Action::Nudge {
                to: Target::Agent(_),
                ..
            }
            | Action::Assign { .. } => {
                return (!act.message_agents).then_some(
                    "the person hasn't let you message agents without asking: an agent acts on a message under its own permissions",
                );
            }
        };
        (!held).then_some(
            "what it leads to wouldn't wait on the person: a session Desk didn't start may not ask before it acts",
        )
    }

    /// Whether a boss ran `session` and it was let go: only the person
    /// hands it back.
    fn taken_back(&self, session: Uuid) -> bool {
        self.desk.charge_of(session).is_some_and(|c| !c.live())
    }

    fn session_name(&self, id: Uuid) -> String {
        self.desk
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "that session".into())
    }

    fn agent_name(&self, id: &str) -> String {
        self.party_of_agent(id)
            .map(|p| p.name)
            .unwrap_or_else(|| crew::shown_name(id, None, false))
    }

    /// What the action is about.
    pub(crate) fn subject_of(&self, action: &Action) -> Option<Party> {
        match action {
            Action::Nudge {
                to: Target::Agent(a),
                ..
            }
            | Action::Assign { agent: a, .. } => self.party_of_agent(a),
            Action::AssignTicket {
                to: Handout::Agent(a),
                ..
            } => self.party_of_agent(a),
            Action::AssignTicket {
                to: Handout::Session(s),
                ..
            } => self.desk.view(*s).ok().map(|v| Party::session(&v)),
            Action::StartSession { .. }
            | Action::AssignTicket {
                to: Handout::NewSession { .. },
                ..
            }
            | Action::MoveTicket { .. }
            | Action::Escalate { .. } => None,
            Action::Nudge {
                to: Target::Session(s),
                ..
            }
            | Action::Approve { session: s, .. }
            | Action::Deny { session: s, .. }
            | Action::Stop { session: s }
            | Action::Interrupt { session: s }
            | Action::Resume { session: s, .. }
            | Action::Restart { session: s, .. }
            | Action::TakeOn { session: s }
            | Action::Release { session: s } => self.desk.view(*s).ok().map(|v| Party::session(&v)),
        }
    }

    /// The action in plain words, naming who it is for: a suggestion's text.
    pub(crate) fn describe(&self, action: &Action) -> String {
        let request = |s: &Uuid| {
            self.desk
                .view(*s)
                .ok()
                .and_then(|v| v.pending)
                .map(|p| {
                    let what = crew::approval_summary(&p.tool, &p.detail);
                    what.trim_end_matches('?').to_string()
                })
                .unwrap_or_else(|| "go ahead".into())
        };
        match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to: None,
            } => format!(
                "Ask {}: “{}”",
                self.session_name(*s),
                crew::clip(message, 200)
            ),
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to: Some(_),
            } => format!(
                "Answer {}: “{}”",
                self.session_name(*s),
                crew::clip(message, 200)
            ),
            Action::Nudge {
                to: Target::Agent(a),
                message,
                ..
            } => format!("Ask {}: “{}”", self.agent_name(a), crew::clip(message, 200)),
            Action::Escalate { why, .. } => {
                format!(
                    "Give the person a session's question: {}",
                    crew::clip(why, 200)
                )
            }
            Action::Approve { session, .. } => format!(
                "Allow {}: {}",
                self.session_name(*session),
                request(session)
            ),
            Action::Deny { session, .. } => {
                format!("Deny {}: {}", self.session_name(*session), request(session))
            }
            Action::Stop { session } => format!("Stop {}", self.session_name(*session)),
            Action::Interrupt { session } => {
                format!("Interrupt {}", self.session_name(*session))
            }
            Action::Resume { session, prompt } => match prompt {
                Some(p) => format!(
                    "Resume {} with “{}”",
                    self.session_name(*session),
                    crew::clip(p, 200)
                ),
                None => format!("Resume {}", self.session_name(*session)),
            },
            Action::Restart { session, prompt } => match prompt {
                Some(p) => format!(
                    "Restart {} with “{}”",
                    self.session_name(*session),
                    crew::clip(p, 200)
                ),
                None => format!("Restart {}", self.session_name(*session)),
            },
            Action::StartSession { folder, goal, .. } => {
                format!("Start a session in {folder} to {}", crew::clip(goal, 200))
            }
            Action::Assign { agent, task } => {
                format!(
                    "Give {} this: “{}”",
                    self.agent_name(agent),
                    crew::clip(task, 200)
                )
            }
            Action::AssignTicket { ticket, to, .. } => match to {
                Handout::Session(s) => format!("Give {ticket} to {}", self.session_name(*s)),
                Handout::Agent(a) => format!("Give {ticket} to {}", self.agent_name(a)),
                Handout::NewSession { folder, .. } => {
                    format!("Start a session in {folder} for {ticket}")
                }
            },
            Action::MoveTicket { ticket, state, .. } => {
                format!("Move {ticket} to {}", state.title())
            }
            Action::TakeOn { session } => {
                format!("Run {} for you", self.session_name(*session))
            }
            Action::Release { session } => {
                format!("Let {} go: you run it", self.session_name(*session))
            }
        }
    }

    /// Does `action` as `by`. What happened, or what went wrong in plain
    /// words.
    pub(crate) async fn perform(&self, action: &Action, by: By) -> Result<String, String> {
        let plain = |e: workforce_desk::DeskError| match e {
            workforce_desk::DeskError::NotFound => "That session isn’t there any more.".to_string(),
            workforce_desk::DeskError::Internal(e) => {
                tracing::warn!(error = %e, "a crew action failed");
                "Something went wrong on this Mac.".to_string()
            }
            other => {
                let s = other.to_string();
                let mut c = s.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect::<String>() + ".")
                    .unwrap_or(s)
            }
        };
        let d = &self.desk;
        match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to,
            } => {
                let name = self.session_name(*s);
                let sender = match by.party.kind {
                    crew::PartyKind::Manager => workforce_desk::LineSender::Overseer(by),
                    // The person, saying yes to a suggestion.
                    crew::PartyKind::You => workforce_desk::LineSender::Person,
                    crew::PartyKind::Agent | crew::PartyKind::Session | crew::PartyKind::Desk => {
                        return Err(
                            "Only the overseer or the person writes on a session's line.".into(),
                        )
                    }
                };
                let line = |e: workforce_desk::LineError| match e {
                    workforce_desk::LineError::Refused(r) => r.to_string(),
                    workforce_desk::LineError::Desk(e) => plain(e),
                };
                match reply_to {
                    Some(ask) => {
                        let answer = d
                            .line_answer(*ask, sender, message.clone())
                            .await
                            .map_err(line)?;
                        self.answered_notice(&answer).await;
                        Ok(format!("answered {name}"))
                    }
                    // On its line: it gets it once its turn ends, never typed
                    // in over what it is doing (Interrupt is for that).
                    None => {
                        let told = d
                            .line_send(
                                sender,
                                workforce_desk::line::Send::Tell {
                                    to: workforce_desk::line::Target::Session(*s),
                                    text: message.clone(),
                                    when: workforce_desk::line::When::Queue,
                                },
                            )
                            .await
                            .map_err(line)?;
                        if d.wheel_of(*s).is_some_and(|w| w.person()) {
                            return Ok(format!(
                                "told {name}; the person is driving it, so your message waits until they hand it back"
                            ));
                        }
                        Ok(match told.delivery {
                            workforce_desk::line::Delivery::Held => {
                                format!("told {name}; it is stopped and gets it when it runs again")
                            }
                            workforce_desk::line::Delivery::Delivered => format!("told {name}"),
                            _ => format!("told {name}; it gets it once its turn ends"),
                        })
                    }
                }
            }
            Action::Escalate {
                message,
                why,
                options,
                draft,
            } => {
                self.escalate(*message, &by, why, options.clone(), draft.clone())
                    .await?;
                Ok("gave the question to the person".into())
            }
            Action::Nudge {
                to: Target::Agent(a),
                message,
                ..
            } => self.message_agent(a, message, by, Kind::Nudged).await,
            Action::Assign { agent, task } => {
                self.message_agent(agent, task, by, Kind::Assigned).await
            }
            Action::Approve { session, approval } => {
                let v = d
                    .decide(
                        *session,
                        *approval,
                        workforce_desk::model::Decision::Allow,
                        Some(by),
                    )
                    .map_err(plain)?;
                Ok(format!("allowed {}", v.label()))
            }
            Action::Deny { session, approval } => {
                use workforce_desk::model::Decision;
                // Codex may offer only to cancel its turn.
                let offered = d
                    .view(*session)
                    .ok()
                    .and_then(|v| v.pending)
                    .filter(|p| p.id == *approval)
                    .map(|p| p.choices)
                    .unwrap_or_default();
                let decision = match offered.is_empty() || offered.contains(&Decision::Deny) {
                    true => Decision::Deny,
                    false => Decision::Cancel,
                };
                let v = d
                    .decide(*session, *approval, decision, Some(by))
                    .map_err(plain)?;
                Ok(format!("denied {}", v.label()))
            }
            Action::Stop { session } => {
                // The shell a tracked session runs in is the person's.
                if d.view(*session).is_ok_and(|v| v.terminal_id.is_some()) {
                    return Err("That session runs in the person's own Desk terminal, so it can't be stopped; crew.interrupt ends its turn and leaves the shell theirs.".into());
                }
                let v = d.stop(*session, Some(by)).map_err(plain)?;
                Ok(format!("stopped {}", v.label()))
            }
            Action::Interrupt { session } => {
                let v = d.interrupt(*session, Some(by)).map_err(plain)?;
                Ok(format!("interrupted {}", v.label()))
            }
            Action::Resume { session, prompt } => {
                let v = d
                    .resume(*session, prompt.clone(), Some(by))
                    .await
                    .map_err(plain)?;
                Ok(format!("resumed {}", v.label()))
            }
            Action::Restart { session, prompt } => {
                let v = d
                    .restart(*session, prompt.clone(), by)
                    .await
                    .map_err(plain)?;
                Ok(format!("restarted {}", v.label()))
            }
            Action::StartSession { .. } => self.start_session(action, by).await,
            Action::AssignTicket { .. } => self.assign_ticket(action, by).await,
            Action::MoveTicket {
                ticket, id, state, ..
            } => {
                let t = self.ticket_named(ticket, *id, None).await?;
                let mv = Move {
                    state: *state,
                    before: None,
                    after: None,
                };
                d.move_ticket(t.ticket.id, mv, &by)
                    .await
                    .map_err(board_plain)?;
                Ok(format!("moved {ticket} to {}", state.title()))
            }
            Action::TakeOn { session } => {
                let (service, o) = self
                    .boss_for(&by)
                    .await
                    .ok_or("No one oversees the crew to run it.")?;
                let boss = Party::manager(service.agent_id.as_str(), &super::name_of(&service));
                // The overseer taking it on by itself, or the person saying
                // yes to its offer.
                let how = match by.party.kind {
                    crew::PartyKind::You => How::Handed,
                    _ if self.taken_back(*session) => {
                        return Err(format!(
                            "The person took {} back; only they can hand it to you again.",
                            self.session_name(*session)
                        ))
                    }
                    _ => How::Taken,
                };
                let c = d
                    .take_on(*session, &boss, how, &o.scope, &by)
                    .await
                    .map_err(charge_plain)?;
                Ok(format!("took on {}", self.session_name(c.session)))
            }
            Action::Release { session } => {
                let c = d.release(*session, &by).await.map_err(charge_plain)?;
                Ok(format!("let {} go", self.session_name(c.session)))
            }
        }
    }

    /// Starts the session `action` asks for, as `by`: a visible one the
    /// overseer runs from the start, in its group. The overseer starts no
    /// more than `[oversight.work] parallel` at once (counted with the
    /// start, so two at once can't both take the last slot); the person
    /// saying yes to its suggestion is theirs to decide, and starts it.
    async fn start_session(&self, action: &Action, by: By) -> Result<String, String> {
        let Action::StartSession {
            folder,
            goal,
            name,
            engine,
            view,
            worktree,
        } = action
        else {
            return Err("That isn't a session to start.".into());
        };
        let (service, o) = self
            .boss_for(&by)
            .await
            .ok_or("No one oversees the crew to start sessions for.")?;
        let boss = Party::manager(service.agent_id.as_str(), &super::name_of(&service));
        let engine = self.engine_or_default(*engine, &o)?;
        let view = self.startable(engine, *view)?;
        let _one = self.starting.lock().await;
        self.slot_free(&boss, &o, &by)?;
        let label = name.clone().unwrap_or_else(|| {
            Path::new(folder)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".into())
        });
        let v = self
            .desk
            .create_for(
                NewSession {
                    engine,
                    view: Some(view),
                    name: label,
                    cwd: folder.clone(),
                    goal: goal.clone(),
                    worktree: worktree.clone(),
                    prompt: Some(goal.clone()),
                    approvals: None,
                    approve_for_me: false,
                    account: None,
                    env: Default::default(),
                    group: self.boss_group(&service, &o),
                },
                &boss,
                by,
            )
            .await
            .map_err(|e| match e {
                workforce_desk::DeskError::Internal(e) => {
                    tracing::warn!(error = %e, "a session didn't start");
                    "Something went wrong on this Mac.".to_string()
                }
                other => {
                    let s = other.to_string();
                    let mut c = s.chars();
                    c.next()
                        .map(|f| f.to_uppercase().chain(c).collect::<String>() + ".")
                        .unwrap_or(s)
                }
            })?;
        Ok(format!(
            "started {} ({}); you run it, and the person can watch it, type into it and take it over",
            v.label(),
            v.id
        ))
    }

    /// `engine`, or the overseer's default one.
    fn engine_or_default(&self, engine: Option<Engine>, o: &Oversight) -> Result<Engine, String> {
        match engine {
            Some(e) => Ok(e),
            None => Engine::parse(o.work.engine.trim()).map_err(|_| {
                format!(
                    "Your default engine, {}, isn't one Desk can start; say engine ({}).",
                    o.work.engine,
                    self.engines_to_start().join(" or ")
                )
            }),
        }
    }

    /// Whether `boss` has a slot free for one more session it starts, when
    /// `by` is the overseer itself: the person saying yes to a start is
    /// theirs to decide. Call it holding `starting`.
    fn slot_free(&self, boss: &Party, o: &Oversight, by: &By) -> Result<(), String> {
        if by.party.kind != crew::PartyKind::Manager {
            return Ok(());
        }
        let busy = self.desk.charges_busy(&boss.id);
        let parallel = o.work.parallel as usize;
        if busy >= parallel {
            return Err(format!(
                "You already run {busy} sessions that are starting or working, the most you may run at once ({parallel}). Wait for one to finish, or ask the person to let you run more."
            ));
        }
        Ok(())
    }

    /// The ticket by its id when it has one (a suggestion's), else by its
    /// key; with `seen`, only one on a board its scope sees: one on another
    /// scope's board is refused in the words one that isn't there is, so
    /// a key never says what another scope holds.
    async fn ticket_named(
        &self,
        key: &str,
        id: Option<Uuid>,
        seen: Option<&Seen>,
    ) -> Result<workforce_desk::TicketView, String> {
        let t = match id {
            Some(id) => self.desk.ticket(id).await,
            None => self.desk.ticket_by_key(key).await,
        };
        if let Some(seen) = seen {
            let group = match &t {
                Ok(t) => self.desk.group_of_board(t.ticket.board).await.ok(),
                Err(_) => None,
            };
            if !group.is_some_and(|g| seen.sees(g.as_deref())) {
                return Err(format!("There's no {key}."));
            }
        }
        let t = t.map_err(board_plain)?;
        // A suggestion's id is only good for the ticket it was made about.
        if id.is_some() && t.key.as_deref() != Some(key) {
            return Err(format!(
                "That was about {key}, which isn’t that ticket any more; look at the board again."
            ));
        }
        Ok(t)
    }

    /// Fills in the ticket `action` is about, before it is gated: its id
    /// (the suggestion keeps that, not the key), its words, and where it
    /// stands. With `seen`, a ticket on a board outside that scope is
    /// refused. Other actions are left as they are.
    pub(crate) async fn resolve_ticket(
        &self,
        action: &mut Action,
        seen: Option<&Seen>,
    ) -> Result<(), String> {
        match action {
            Action::AssignTicket {
                ticket, id, text, ..
            } => {
                let t = self.ticket_named(ticket, *id, seen).await?;
                *id = Some(t.ticket.id);
                if let Some(key) = &t.key {
                    ticket.clone_from(key);
                }
                *text = format!("{}\n{}", t.ticket.title, t.ticket.body);
            }
            Action::MoveTicket {
                ticket,
                id,
                state,
                was,
                off_external,
            } => {
                let t = self.ticket_named(ticket, *id, seen).await?;
                *id = Some(t.ticket.id);
                if let Some(key) = &t.key {
                    ticket.clone_from(key);
                }
                *was = Some(t.ticket.state);
                *off_external = workforce_desk::board::unassigns(*state)
                    && t.ticket
                        .session()
                        .and_then(|s| self.desk.view(s).ok())
                        .is_some_and(|v| v.origin == Origin::External);
            }
            _ => {}
        }
        Ok(())
    }

    /// Hands the ticket `action` names to a session, an agent, or a session
    /// started for it, as `by`. One that is started is the overseer's to
    /// run from the start: it counts against `[oversight.work] parallel`
    /// as any start does, and is in the overseer's charge and group.
    async fn assign_ticket(&self, action: &Action, by: By) -> Result<String, String> {
        let Action::AssignTicket {
            ticket: key,
            id,
            to,
            ..
        } = action
        else {
            return Err("That isn't a ticket to hand out.".into());
        };
        let d = &self.desk;
        // One hand-out at a time, and the ticket read after waiting for
        // its turn: a hand-out made meanwhile, or a suggestion accepted
        // after one, finds it taken.
        let _one = self.starting.lock().await;
        let t = self.ticket_named(key, *id, None).await?;
        if matches!(t.ticket.state, TicketState::Done | TicketState::Dropped) {
            return Err(format!(
                "{key} is {} already; there is nothing to hand out.",
                t.ticket.state.title().to_lowercase()
            ));
        }
        if let Some(has) = &t.ticket.assignee {
            return Err(format!(
                "{key} is already with {}; it can't be handed out again.",
                has.name
            ));
        }
        if t.ticket.state != TicketState::Todo {
            return Err(format!(
                "{key} is in {}, not To do; only a ready ticket can be handed out.",
                t.ticket.state.title()
            ));
        }
        if !t.blockers.is_empty() {
            let mut waits = Vec::new();
            for b in &t.blockers {
                if let Ok(n) = d.ticket(*b).await {
                    waits.push(n.key.unwrap_or_else(|| "another ticket".into()));
                }
            }
            return Err(format!(
                "{key} waits on {}, which isn't done; hand it out once it is.",
                waits.join(", ")
            ));
        }
        let id = t.ticket.id;
        let given = match to {
            Handout::Session(s) => {
                if d.view(*s).is_err() {
                    return Err("That session isn’t there any more.".into());
                }
                d.assign_ticket(id, Some(AssignTo::Session(*s)), &by)
                    .await
                    .map_err(board_plain)?
            }
            Handout::Agent(a) => d
                .assign_ticket(id, Some(AssignTo::Agent(a.clone())), &by)
                .await
                .map_err(board_plain)?,
            Handout::NewSession { folder, engine } => {
                let (service, o) = self
                    .boss_for(&by)
                    .await
                    .ok_or("No one oversees the crew to start sessions for.")?;
                let boss = Party::manager(service.agent_id.as_str(), &super::name_of(&service));
                let engine = self.engine_or_default(*engine, &o)?;
                self.startable(engine, None)?;
                self.slot_free(&boss, &o, &by)?;
                let charged = ChargedTo {
                    group: self.boss_group(&service, &o),
                    boss,
                };
                let new = AssignTo::NewSession(NewSessionFor {
                    path: folder.clone(),
                    engine,
                });
                d.assign_ticket_for(id, Some(new), &by, Some(&charged))
                    .await
                    .map_err(board_plain)?
            }
        };
        let who = given.ticket.assignee;
        let name = who.as_ref().map_or("no one", |a| a.name.as_str());
        Ok(match (to, who.as_ref()) {
            (Handout::NewSession { .. }, Some(a)) => format!(
                "started {name} ({}) for {key}; you run it, and the person can watch it, type into it and take it over",
                a.id
            ),
            _ => format!("gave {key} to {name}"),
        })
    }

    /// Whether the overseer (`by`) may suggest one more start: the sessions
    /// it runs that are starting or working, and the starts it suggested
    /// that the person hasn't answered (a ticket handed to a new session is a start), are fewer than its limit.
    pub(crate) async fn start_suggestable(&self, by: &By, o: &Oversight) -> Result<(), String> {
        let busy = self.desk.charges_busy(&by.party.id);
        let open = self
            .desk
            .store()
            .open_start_suggestions(&by.party.id)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "couldn't count the starts the overseer suggested");
                "Something went wrong on this Mac.".to_string()
            })?;
        let held = busy + usize::try_from(open).unwrap_or(usize::MAX);
        let parallel = o.work.parallel as usize;
        if held >= parallel {
            return Err(format!(
                "You already run or have suggested {held} sessions, the most you may run at once ({parallel}). Wait for one to finish, or for the person to answer the starts you suggested."
            ));
        }
        Ok(())
    }

    /// The sidebar group the sessions the overseer `service` starts go into:
    /// its scope's group for a group's boss; for the main overseer, the
    /// group its agent sits in when that group has no boss of its own
    /// (which is still its scope), else none.
    fn boss_group(&self, service: &crate::serve::AgentService, o: &Oversight) -> Option<String> {
        if let Some(g) = o.scope.group() {
            return Some(g.to_string());
        }
        let g = self
            .desk
            .group_of_member(&workforce_desk::local_agent_item(service.agent_id.as_str()))?;
        (self.desk.scope_of_group(Some(&g)) == o.scope).then_some(g)
    }

    /// The overseer `by` acts for: itself, or for the person saying yes to
    /// a suggestion, the overseer that made it, while it still oversees.
    pub(crate) async fn boss_for(
        &self,
        by: &By,
    ) -> Option<(std::sync::Arc<crate::serve::AgentService>, Oversight)> {
        match by.party.kind {
            crew::PartyKind::Manager => self.overseer_named(&by.party.id),
            // A suggestion Desk no longer has: the main overseer, as
            // before groups had bosses; what it may run is still its scope's.
            _ => {
                let made = match by.suggestion_id.as_deref() {
                    Some(id) => self.desk.store().suggestion(id).await.ok().flatten(),
                    None => None,
                };
                match made {
                    Some(s) => self.overseer_named(&s.agent_id),
                    None => self.overseer_for(&Scope::Ungrouped),
                }
            }
        }
    }

    /// `engine` in `view` (its default when none), when the overseer may
    /// start it: Desk runs it there, it has a line to run it on, and it is
    /// installed on this computer.
    fn startable(&self, engine: Engine, view: Option<View>) -> Result<View, String> {
        let title = engine.title();
        let view = view
            .or_else(|| engine::views(engine).first().copied())
            .filter(|&v| engine::runs(engine, v))
            .ok_or_else(|| match view {
                Some(v) => format!(
                    "Desk can't start {title} sessions in its {} view.",
                    v.as_str()
                ),
                None => format!("Desk can't start {title} sessions yet."),
            })?;
        if engine::caps(engine, view).line == Deliver::None {
            return Err(format!(
                "{title} sessions have no line for you to run them on."
            ));
        }
        let installed = self
            .desk
            .engines()
            .into_iter()
            .find(|e| e.engine == engine)
            .is_some_and(|e| e.program.is_some() && e.installed != Some(false));
        if !installed {
            return Err(format!(
                "{title} isn't installed on this computer; start {} instead.",
                self.engines_to_start().join(" or ")
            ));
        }
        Ok(view)
    }

    /// The engines the overseer can start sessions of now, by name.
    pub(crate) fn engines_to_start(&self) -> Vec<&'static str> {
        self.desk
            .engines()
            .into_iter()
            .filter(|e| e.program.is_some() && e.installed != Some(false))
            .filter(|e| {
                e.views
                    .first()
                    .is_some_and(|&v| engine::caps(e.engine, v).line != Deliver::None)
            })
            .map(|e| e.engine.as_str())
            .collect()
    }

    /// Sends `text` into `agent`'s Desk DM with the person, as the overseer
    /// (or you), held to the loop guard; the turn runs on its own.
    async fn message_agent(
        &self,
        agent: &str,
        text: &str,
        by: By,
        kind: Kind,
    ) -> Result<String, String> {
        let service = self
            .state
            .registry
            .get(agent)
            .ok_or_else(|| format!("There’s no agent called {agent} on this Mac."))?;
        let name = super::name_of(&service);
        let from_overseer = by.party.kind == crew::PartyKind::Manager;
        if from_overseer {
            if by.hop > super::MAX_HOPS {
                return Err(format!(
                    "Loop guard: agents have handed this on {} times already; tell the person instead.",
                    super::MAX_HOPS
                ));
            }
            let mut sent = self.sent.lock();
            if sent.len() >= super::TURN_BUDGET {
                return Err(format!(
                    "Loop guard: you have messaged the crew {} times since the person last spoke; tell the person instead.",
                    super::TURN_BUDGET
                ));
            }
            sent.push(chrono::Utc::now());
        }
        let owner = self
            .person(Some(&service))
            .ok_or_else(|| format!("No one has talked with {name} in Desk yet, so there’s no conversation to send this to."))?;
        let party = Party::agent(agent, &name);
        let (verb, prefix) = match kind {
            Kind::Assigned => ("gave", "has a task for you"),
            _ => ("asked", "asks"),
        };
        let event = by.event(
            kind,
            Some(party),
            format!("{verb} {name} “{}”", crew::one_line(text, 120)),
        );
        let hop = by.hop;
        // Worded for the agent reading it: the overseer by its name, the
        // person (saying yes to a suggestion) by theirs.
        let (author, says) = if from_overseer {
            (
                Speaker::overseer(&by.party.id, &by.party.name),
                by.party.name.clone(),
            )
        } else {
            (Speaker::you(), self.person_name(&owner))
        };
        let message = format!("{says} {prefix}: {text}");
        self.desk.note(event);
        let crew = self.me();
        tokio::spawn(async move {
            if let Err(e) = crew
                .run_dm_turn(&service, &owner, &message, hop, None, Some(author))
                .await
            {
                tracing::warn!(agent_id = %service.agent_id, error = %e, "a message to the crew did not run");
            }
        });
        Ok(format!("{verb} {name}"))
    }

    /// What the agents call the person `owner`: their name, when the hub
    /// knows them by one.
    pub(super) fn person_name(&self, owner: &str) -> String {
        crate::desk_surface::person_name(self.state.identity_store.as_ref(), owner)
            .unwrap_or_else(|| "The person you work for".into())
    }

    /// The suggestion `action` becomes, recorded and announced.
    pub(crate) async fn suggest(
        &self,
        tool: &str,
        input: &Value,
        action: &Action,
        by: &By,
    ) -> Result<crew::Suggestion, String> {
        let mut input = input.clone();
        if let Some(o) = input.as_object_mut() {
            o.remove("reason");
        }
        let suggestion = crew::Suggestion {
            id: crew::new_id(),
            at: crew::now(),
            text: self.describe(action),
            action: json!({ "tool": tool, "input": input }),
            state: crew::SuggestionState::Open,
            subject: self.subject_of(action),
            agent_id: by.party.id.clone(),
        };
        self.desk
            .store()
            .put_suggestion(&suggestion)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "saving a suggestion failed");
                "The suggestion could not be saved.".to_string()
            })?;
        let mut b = by.clone();
        b.suggestion_id = Some(suggestion.id.clone());
        self.desk.note(b.event(
            Kind::Suggested,
            suggestion.subject.clone(),
            format!("suggested: {}", suggestion.text),
        ));
        Ok(suggestion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushing_deleting_and_sending_are_outward() {
        assert!(outward("Push the branch when green"));
        assert!(outward("rm the build folder"));
        assert!(outward("email the team"));
        for said in [
            "Share it in #general",
            "Upload the report",
            "Open a PR for the date picker",
            "Notify the client it is fixed",
            "Submit it",
            "Reply to the thread when done",
        ] {
            assert!(outward(said), "{said}");
        }
        for said in [
            "Pushing to main now",
            "Deploying to staging",
            "The deployment is ready",
            "Publishing the crate",
            "Merging the PR",
            "Releasing 4.2",
            "Deleting the old branch",
            "Ship it to prod?",
            "Roll it out to production",
            "Run the migration",
            "Migrate the database",
            "Apply the terraform plan",
            "Commit and push",
            "May I commit this?",
        ] {
            assert!(outward(said), "{said}");
        }
        for said in [
            "Which port does the test database use?",
            "Is postgres on 5433?",
            "What does the product page show?",
            "Run the tests without the network ones",
        ] {
            assert!(!outward(said), "{said}");
        }
        assert!(!outward("Run the tests without the network ones"));
        assert!(!outward("Still on the provider tests, or stuck?"));
    }

    #[test]
    fn actions_parse_from_tool_input_and_back_from_a_suggestion() {
        let s = Uuid::new_v4();
        let a = Action::parse(
            "crew.restart",
            &json!({"session_id": s.to_string(), "prompt": "Skip network tests.", "reason": "stuck"}),
        )
        .unwrap();
        assert_eq!(
            a,
            Action::Restart {
                session: s,
                prompt: Some("Skip network tests.".into())
            }
        );
        let back = Action::from_suggestion(
            &json!({"tool": "crew.restart", "input": {"session_id": s.to_string(), "prompt": "Skip network tests."}}),
        )
        .unwrap();
        assert_eq!(back, a);
        assert!(Action::parse("crew.nudge", &json!({"message": "hi"})).is_err());
        assert_eq!(
            Action::parse("crew.nudge", &json!({"agent_id": "scout", "message": "hi"})).unwrap(),
            Action::Nudge {
                to: Target::Agent("scout".into()),
                message: "hi".into(),
                reply_to: None,
            }
        );
        let s = Uuid::new_v4();
        let q = Uuid::new_v4();
        assert_eq!(
            Action::parse(
                "crew.nudge",
                &json!({"session_id": s.to_string(), "reply_to": q.to_string(), "message": "5433"})
            )
            .unwrap(),
            Action::Nudge {
                to: Target::Session(s),
                message: "5433".into(),
                reply_to: Some(q),
            }
        );
        assert!(
            Action::parse(
                "crew.nudge",
                &json!({"agent_id": "scout", "reply_to": q.to_string(), "message": "x"})
            )
            .is_err(),
            "only a session's question is answered"
        );
        assert_eq!(
            Action::parse(
                "crew.escalate",
                &json!({"message_id": q.to_string(), "reason": "Pushing is yours.", "options": ["Yes", " ", "No"], "draft": "Yes"})
            )
            .unwrap(),
            Action::Escalate {
                message: q,
                why: "Pushing is yours.".into(),
                options: vec!["Yes".into(), "No".into()],
                draft: Some("Yes".into()),
            }
        );
    }
}

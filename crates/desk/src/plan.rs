//! Plans: a boss's goal broken into tickets on a board. The person approves
//! a plan before any of it runs, and the approval is what lets the hub's
//! runner start sessions for its tickets, up to the boss's limit. Order and
//! parallelism come from the tickets' needs, so a plan's waves are
//! computed here, never stored. The boss keeps a running plan current
//! within what was approved; adding or dropping a ticket, or changing the
//! goal, is a change of scope, which the person approves again.
//!
//! This module decides; the service stores, announces and runs. Desk's
//! `crew/plan.ts` agrees with [`waves`] and [`progress`] on the fixtures in
//! `fixtures/plan/`.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::budget::Status;

use crate::board::{TicketState, BODY_MAX, TITLE_MAX};
use crate::crew::Party;
use crate::engine::{self, Deliver, Engine, EngineView, SignIn, View};
use crate::model::text_enum;

/// The most tickets one plan has.
pub const MAX_TICKETS: usize = 30;
/// The most a goal says, in characters.
pub const GOAL_MAX: usize = 1_000;
/// What a ticket names to run as a sub-agent job rather than a session.
pub const SUBAGENT: &str = "subagent";

text_enum!(PlanState {
    Proposed => "proposed",
    Running => "running",
    Paused => "paused",
    Done => "done",
    Dropped => "dropped",
});

text_enum!(Change {
    // Within what was approved: order, needs, titles and bodies, and where
    // a ticket not yet started runs.
    Minor => "minor",
    // A ticket added or dropped, or the goal changed: the person approves it.
    Scope => "scope",
});

/// Why a plan that would run starts nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Paused {
    /// The person paused it.
    Person,
    /// Its group's or its own budget is at its warning or over.
    Budget,
    /// Its boss is over its own budget, or held by the breaker.
    BossHeld,
    /// The computer restarted, and its sessions are being resumed.
    Restoring,
    /// Its group has no boss now; the main boss takes it on once the person
    /// has read the handover.
    NoBoss,
}

impl Paused {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Budget => "budget",
            Self::BossHeld => "boss_held",
            Self::Restoring => "restoring",
            Self::NoBoss => "no_boss",
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        match s {
            "person" => Ok(Self::Person),
            "budget" => Ok(Self::Budget),
            "boss_held" => Ok(Self::BossHeld),
            "restoring" => Ok(Self::Restoring),
            "no_boss" => Ok(Self::NoBoss),
            other => Err(eyre::eyre!("unknown Paused {other:?}")),
        }
    }

    /// Why, as the person reads it after "Paused: ".
    pub fn why(self) -> &'static str {
        match self {
            Self::Person => "you paused it",
            Self::Budget => "its budget is nearly spent",
            Self::BossHeld => "its boss is held",
            Self::Restoring => "Desk is resuming its sessions",
            Self::NoBoss => "its group has no boss",
        }
    }
}

/// A plan, as stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: Uuid,
    pub board: Uuid,
    /// The board's group when it was made; none is ungrouped.
    pub group: Option<String>,
    /// The boss's agent id.
    pub boss: String,
    pub goal: String,
    pub state: PlanState,
    pub paused: Option<Paused>,
    /// This plan's limit of sessions at once, when lower than its boss's.
    pub parallel: Option<i16>,
    /// Its newest revision.
    pub revision: i32,
    /// The revision the runner follows; none before the person approves.
    pub approved_revision: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Plan {
    /// Whether a revision waits on the person: the first one, or a change of
    /// scope since.
    pub fn awaiting(&self) -> bool {
        match self.state {
            PlanState::Done | PlanState::Dropped => false,
            PlanState::Proposed | PlanState::Running | PlanState::Paused => {
                self.approved_revision != Some(self.revision)
            }
        }
    }

    /// Whether it may start sessions now, as far as the plan itself says.
    pub fn runs(&self) -> bool {
        self.state == PlanState::Running
            && self.paused.is_none()
            && self.approved_revision.is_some()
    }

    /// How many of its boss's sessions may start or work at once for it,
    /// the boss's own limit being `boss`.
    pub fn limit(&self, boss: usize) -> usize {
        match self.parallel {
            Some(p) => boss.min(usize::try_from(p).unwrap_or(0)),
            None => boss,
        }
    }
}

/// One ticket of a revision, as the plan has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Planned {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub engine: Engine,
    pub folder: String,
    #[serde(default)]
    pub view: Option<View>,
    /// The tickets of the plan it needs done first.
    #[serde(default)]
    pub needs: Vec<Uuid>,
}

/// A plan's goal and tickets as of one revision, in its order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub goal: String,
    pub tickets: Vec<Planned>,
    /// The plan's own limit of sessions at once, when lower than its
    /// boss's; none is the boss's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<i16>,
}

impl Snapshot {
    pub fn ticket(&self, id: Uuid) -> Option<&Planned> {
        self.tickets.iter().find(|t| t.id == id)
    }

    /// Its tickets not in `before`.
    pub fn added<'a>(&'a self, before: &Snapshot) -> Vec<&'a Planned> {
        self.tickets
            .iter()
            .filter(|t| before.ticket(t.id).is_none())
            .collect()
    }

    /// The tickets of `before` it no longer has.
    pub fn dropped(&self, before: &Snapshot) -> Vec<Uuid> {
        before
            .tickets
            .iter()
            .filter(|t| self.ticket(t.id).is_none())
            .map(|t| t.id)
            .collect()
    }
}

/// One revision of a plan, as kept.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub plan: Uuid,
    pub revision: i32,
    pub at: DateTime<Utc>,
    pub by: Party,
    pub change: Change,
    pub summary: String,
    pub snapshot: Snapshot,
}

/// A plan as the boss or the person writes it: each ticket by its id when
/// it is one the plan has, and what it needs by its place in this list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub goal: String,
    pub tickets: Vec<DraftTicket>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftTicket {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// None: the boss's default engine.
    #[serde(default)]
    pub engine: Option<String>,
    pub folder: String,
    #[serde(default)]
    pub view: Option<String>,
    /// The places in the list (from 0) of the tickets it needs done first.
    #[serde(default)]
    pub needs: Vec<usize>,
}

/// What is wrong with a plan, in the words the person and the boss read.
/// `ticket` is its place in the list, from 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("A plan needs a goal.")]
    NoGoal,
    #[error("The goal is {0} characters; keep it to {GOAL_MAX}.")]
    LongGoal(usize),
    #[error("A plan needs at least one ticket.")]
    NoTickets,
    #[error("A plan has at most {MAX_TICKETS} tickets; this one has {0}. Split it into plans one after another.")]
    TooMany(usize),
    #[error("Ticket {ticket} needs a title.")]
    NoTitle { ticket: usize },
    #[error("Ticket {ticket}'s title is {len} characters; keep it to {TITLE_MAX}.")]
    LongTitle { ticket: usize, len: usize },
    #[error("Ticket {ticket}'s description is {len} characters; keep it to {BODY_MAX}.")]
    LongBody { ticket: usize, len: usize },
    #[error("Ticket {ticket} names {engine:?}, which isn't an engine Desk knows.")]
    UnknownEngine { ticket: usize, engine: String },
    #[error("Ticket {ticket} asks for a sub-agent; Desk is set to prefer real sessions, so give it an engine.")]
    Subagent { ticket: usize },
    #[error("Ticket {ticket} names the view {view:?}; Desk starts sessions in its terminal view.")]
    UnknownView { ticket: usize, view: String },
    #[error("Ticket {ticket}: {engine} isn't installed on this computer.")]
    NotInstalled { ticket: usize, engine: &'static str },
    #[error("Ticket {ticket}: {engine} isn't signed in on this computer.")]
    SignedOut { ticket: usize, engine: &'static str },
    #[error("Ticket {ticket}: Desk can't start {engine} sessions there.")]
    CantStart { ticket: usize, engine: &'static str },
    #[error("Ticket {ticket}: {engine} sessions have no line for a boss to run them on.")]
    NoLine { ticket: usize, engine: &'static str },
    #[error("Ticket {ticket}'s folder {folder:?} isn't a folder on this computer.")]
    NoFolder { ticket: usize, folder: String },
    #[error("Ticket {ticket} isn't one this plan has; leave out its id to add it.")]
    UnknownTicket { ticket: usize },
    #[error("Ticket {ticket} is in the list twice.")]
    Twice { ticket: usize },
    #[error("Ticket {ticket} needs ticket {need}, which isn't in the list.")]
    UnknownNeed { ticket: usize, need: usize },
    #[error("Ticket {ticket} can't need itself.")]
    NeedsItself { ticket: usize },
    #[error("Ticket {ticket} needs a ticket that isn't in this plan.")]
    NeedsOutside { ticket: usize },
    #[error("Tickets {0} need each other in a loop, so none of them could start.")]
    Cycle(String),
    #[error(
        "{title:?} has started, so where it runs can't change; drop it and add another instead."
    )]
    Started { title: String },
}

/// The problems as one message.
pub fn plain(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `draft` as a revision's snapshot: its tickets by id (a new one gets one
/// from `new_id`), its needs by those ids, an engine for each (the boss's
/// `default_engine` where none is said). `before` is the plan's snapshot
/// the draft changes, when it changes one: a ticket's id must be one of it.
pub fn snapshot_of(
    draft: &Draft,
    before: Option<&Snapshot>,
    default_engine: Engine,
    new_id: &mut dyn FnMut() -> Uuid,
) -> Result<Snapshot, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    let mut ids = Vec::with_capacity(draft.tickets.len());
    for (i, t) in draft.tickets.iter().enumerate() {
        let ticket = i + 1;
        let id = match t.id {
            Some(id) if before.is_none_or(|b| b.ticket(id).is_none()) => {
                problems.push(Problem::UnknownTicket { ticket });
                id
            }
            Some(id) => id,
            None => new_id(),
        };
        if !seen.insert(id) {
            problems.push(Problem::Twice { ticket });
        }
        ids.push(id);
    }
    let mut tickets = Vec::with_capacity(draft.tickets.len());
    for (i, t) in draft.tickets.iter().enumerate() {
        let ticket = i + 1;
        let engine = match t.engine.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            None => Some(default_engine),
            Some(e) if e.eq_ignore_ascii_case(SUBAGENT) => {
                problems.push(Problem::Subagent { ticket });
                None
            }
            Some(e) => match Engine::parse(&e.to_lowercase()) {
                Ok(e) => Some(e),
                Err(_) => {
                    problems.push(Problem::UnknownEngine {
                        ticket,
                        engine: e.to_string(),
                    });
                    None
                }
            },
        };
        let view = match t.view.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => match View::parse(&v.to_lowercase()) {
                Ok(v) => Some(v),
                Err(_) => {
                    problems.push(Problem::UnknownView {
                        ticket,
                        view: v.to_string(),
                    });
                    None
                }
            },
        };
        let mut needs = Vec::new();
        for &n in &t.needs {
            match ids.get(n) {
                _ if n == i => problems.push(Problem::NeedsItself { ticket }),
                Some(id) if !needs.contains(id) => needs.push(*id),
                Some(_) => {}
                None => problems.push(Problem::UnknownNeed {
                    ticket,
                    need: n + 1,
                }),
            }
        }
        if let Some(engine) = engine {
            tickets.push(Planned {
                id: ids[i],
                title: t.title.trim().to_string(),
                body: t.body.trim().to_string(),
                engine,
                folder: t.folder.trim().to_string(),
                view,
                needs,
            });
        }
    }
    match problems.is_empty() {
        true => Ok(Snapshot {
            goal: draft.goal.trim().to_string(),
            tickets,
            parallel: before.and_then(|b| b.parallel),
        }),
        false => Err(problems),
    }
}

/// Whether `s` can run on this computer: a goal, between one and
/// [`MAX_TICKETS`] tickets, each with a title, an engine that is installed,
/// signed in and has a line for its boss (`engines`, as Desk reads them),
/// a folder that `is_dir`, and needs that never loop.
pub fn validate(
    s: &Snapshot,
    engines: &[EngineView],
    is_dir: &dyn Fn(&str) -> bool,
) -> Result<(), Vec<Problem>> {
    let mut problems = Vec::new();
    let goal = s.goal.trim();
    if goal.is_empty() {
        problems.push(Problem::NoGoal);
    }
    let len = goal.chars().count();
    if len > GOAL_MAX {
        problems.push(Problem::LongGoal(len));
    }
    match s.tickets.len() {
        0 => problems.push(Problem::NoTickets),
        n if n > MAX_TICKETS => problems.push(Problem::TooMany(n)),
        _ => {}
    }
    let place: HashMap<Uuid, usize> = s
        .tickets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i + 1))
        .collect();
    for (i, t) in s.tickets.iter().enumerate() {
        let ticket = i + 1;
        let title = t.title.trim();
        if title.is_empty() {
            problems.push(Problem::NoTitle { ticket });
        }
        let len = title.chars().count();
        if len > TITLE_MAX {
            problems.push(Problem::LongTitle { ticket, len });
        }
        let len = t.body.chars().count();
        if len > BODY_MAX {
            problems.push(Problem::LongBody { ticket, len });
        }
        if let Some(p) = engine_problem(ticket, t.engine, t.view, engines) {
            problems.push(p);
        }
        let folder = t.folder.trim();
        if !std::path::Path::new(folder).is_absolute() || !is_dir(folder) {
            problems.push(Problem::NoFolder {
                ticket,
                folder: t.folder.clone(),
            });
        }
        for n in &t.needs {
            if *n == t.id {
                problems.push(Problem::NeedsItself { ticket });
            } else if !place.contains_key(n) {
                problems.push(Problem::NeedsOutside { ticket });
            }
        }
    }
    let (_, looped) = layers(s.tickets.iter().map(|t| (t.id, t.needs.as_slice())));
    if !looped.is_empty() {
        let mut at: Vec<usize> = looped
            .iter()
            .filter_map(|id| place.get(id))
            .copied()
            .collect();
        at.sort_unstable();
        let words: Vec<String> = at.iter().map(ToString::to_string).collect();
        problems.push(Problem::Cycle(words.join(", ")));
    }
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems),
    }
}

/// What keeps `engine` in `view` from running ticket `ticket` here, if
/// anything.
fn engine_problem(
    ticket: usize,
    engine: Engine,
    view: Option<View>,
    engines: &[EngineView],
) -> Option<Problem> {
    let title = engine.title();
    let Some(e) = engines
        .iter()
        .find(|e| e.engine == engine)
        .filter(|e| e.program.is_some() && e.installed != Some(false))
    else {
        return Some(Problem::NotInstalled {
            ticket,
            engine: title,
        });
    };
    if e.sign_in == SignIn::Out {
        return Some(Problem::SignedOut {
            ticket,
            engine: title,
        });
    }
    let Some(view) = view
        .or_else(|| engine::views(engine).first().copied())
        .filter(|&v| engine::runs(engine, v))
    else {
        return Some(Problem::CantStart {
            ticket,
            engine: title,
        });
    };
    (engine::caps(engine, view).line == Deliver::None).then_some(Problem::NoLine {
        ticket,
        engine: title,
    })
}

/// The layers of the needs graph of `tickets` (`(ticket, needs)`), each in
/// the order given: a ticket is in the first layer after every one it needs
/// (a need outside `tickets` holds nothing back). The tickets that need
/// each other in a loop, or need one that does, are left over.
fn layers<'a>(
    tickets: impl IntoIterator<Item = (Uuid, &'a [Uuid])>,
) -> (Vec<Vec<Uuid>>, Vec<Uuid>) {
    let tickets: Vec<(Uuid, &[Uuid])> = tickets.into_iter().collect();
    let ours: HashSet<Uuid> = tickets.iter().map(|(id, _)| *id).collect();
    let mut placed: HashSet<Uuid> = HashSet::new();
    let mut out = Vec::new();
    loop {
        let wave: Vec<Uuid> = tickets
            .iter()
            .filter(|(id, _)| !placed.contains(id))
            .filter(|(_, needs)| {
                needs
                    .iter()
                    .all(|n| !ours.contains(n) || placed.contains(n))
            })
            .map(|(id, _)| *id)
            .collect();
        if wave.is_empty() {
            break;
        }
        placed.extend(wave.iter().copied());
        out.push(wave);
    }
    let left = tickets
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !placed.contains(id))
        .collect();
    (out, left)
}

/// A plan's ticket as the runner and the card read it: where it stands on
/// the board, and whether anyone has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTicket {
    pub id: Uuid,
    #[serde(default)]
    pub key: Option<String>,
    pub state: TicketState,
    pub rank: f64,
    #[serde(default)]
    pub needs: Vec<Uuid>,
    #[serde(default)]
    pub engine: Option<Engine>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub view: Option<View>,
    /// A session or an agent has it.
    #[serde(default)]
    pub assigned: bool,
    /// The runner is starting a session for it.
    #[serde(default)]
    pub starting: bool,
}

impl PlanTicket {
    /// Its key, or its id where its board has no keys.
    pub fn name(&self) -> String {
        self.key.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// The waves of a plan's tickets, for showing: the first holds what needs
/// nothing, each next one what needs only those before. Tickets that need
/// each other in a loop come last, together, so none is left out.
pub fn waves(tickets: &[PlanTicket]) -> Vec<Vec<Uuid>> {
    let mut sorted: Vec<&PlanTicket> = tickets.iter().collect();
    sorted.sort_by(|a, b| a.rank.total_cmp(&b.rank));
    let (mut out, left) = layers(sorted.iter().map(|t| (t.id, t.needs.as_slice())));
    if !left.is_empty() {
        out.push(left);
    }
    out
}

/// Whether `t` may start now: in To do, every need Done (a need not among
/// `states` holds it back), and no one has it or is starting it.
fn startable(t: &PlanTicket, states: &HashMap<Uuid, TicketState>) -> bool {
    t.state == TicketState::Todo
        && !t.assigned
        && !t.starting
        && t.needs
            .iter()
            .all(|n| states.get(n) == Some(&TicketState::Done))
}

/// A session the runner starts for a ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Start {
    pub ticket: Uuid,
    pub engine: Engine,
    pub folder: String,
    pub view: Option<View>,
    /// Another of the plan's tickets works in the same folder: this one
    /// gets a worktree of its own.
    pub worktree: bool,
}

/// What the runner starts now for `plan`: its ready tickets in rank order,
/// while the boss's sessions starting or working (`busy`, counting the
/// starts under way) are fewer than the plan's limit (the boss's own being
/// `limit`). None while the plan doesn't run (not approved, paused, done or
/// dropped), or while `standing`, its group's spend, is at its warning or
/// over.
pub fn next_starts(
    plan: &Plan,
    tickets: &[PlanTicket],
    busy: usize,
    limit: usize,
    standing: Option<Status>,
) -> Vec<Start> {
    if !plan.runs() {
        return Vec::new();
    }
    match standing {
        Some(Status::Warn | Status::Over) => return Vec::new(),
        Some(Status::Under) | None => {}
    }
    let cap = plan.limit(limit);
    let states: HashMap<Uuid, TicketState> = tickets.iter().map(|t| (t.id, t.state)).collect();
    let mut ready: Vec<&PlanTicket> = tickets.iter().filter(|t| startable(t, &states)).collect();
    ready.sort_by(|a, b| a.rank.total_cmp(&b.rank));
    let mut in_use: HashSet<&str> = tickets
        .iter()
        .filter(|t| t.state == TicketState::Doing || t.starting)
        .filter_map(|t| t.folder.as_deref())
        .collect();
    let mut busy = busy;
    let mut out = Vec::new();
    for t in ready {
        if busy >= cap {
            break;
        }
        let (Some(engine), Some(folder)) = (t.engine, t.folder.as_deref()) else {
            continue;
        };
        out.push(Start {
            ticket: t.id,
            engine,
            folder: folder.to_string(),
            view: t.view,
            worktree: !in_use.insert(folder),
        });
        busy += 1;
    }
    out
}

/// Where a plan's tickets stand, for its card and the Floor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub done: usize,
    pub doing: usize,
    pub review: usize,
    /// In To do with every need done, and no one on it yet.
    pub ready: usize,
    /// Held back by a need that was dropped: someone must decide.
    pub blocked: usize,
    /// Waiting on needs not done yet, or not handed out yet.
    pub waiting: usize,
    /// Every ticket but the dropped.
    pub total: usize,
    /// The tickets that start next as slots free, by key, in order.
    pub next: Vec<String>,
}

/// Where `tickets`, a plan's, stand.
pub fn progress(tickets: &[PlanTicket]) -> Progress {
    let states: HashMap<Uuid, TicketState> = tickets.iter().map(|t| (t.id, t.state)).collect();
    let mut p = Progress::default();
    let mut next: Vec<&PlanTicket> = Vec::new();
    for t in tickets {
        match t.state {
            TicketState::Dropped => continue,
            TicketState::Done => p.done += 1,
            TicketState::Review => p.review += 1,
            TicketState::Doing => p.doing += 1,
            TicketState::Todo if t.starting || t.assigned => p.doing += 1,
            TicketState::Todo | TicketState::Backlog => {
                let dropped_need = t
                    .needs
                    .iter()
                    .any(|n| states.get(n) == Some(&TicketState::Dropped));
                if dropped_need {
                    p.blocked += 1;
                } else if startable(t, &states) {
                    p.ready += 1;
                    next.push(t);
                } else {
                    p.waiting += 1;
                }
            }
        }
        p.total += 1;
    }
    next.sort_by(|a, b| a.rank.total_cmp(&b.rank));
    p.next = next.iter().map(|t| t.name()).collect();
    p
}

/// Whether every one of a plan's tickets is Done or Dropped: its boss
/// writes the closing summary, and it is done.
pub fn settled(tickets: &[PlanTicket]) -> bool {
    !tickets.is_empty()
        && tickets
            .iter()
            .all(|t| matches!(t.state, TicketState::Done | TicketState::Dropped))
}

/// What changing the plan from `applied`, the revision the runner follows,
/// to `next` is: nothing (`None`), a minor change, or a change of scope.
///
/// Minor, within what was approved: the order, needs and titles, a ticket
/// not started moved to an engine and a folder the approved revision
/// already uses, and a lower limit. Scope, which the person approves: the
/// goal, a ticket added or dropped, what a ticket says (its session's first
/// prompt), an engine or a folder the approved revision doesn't use, and a
/// higher limit. Where a ticket that has `started` runs can't change.
pub fn plan_change(
    applied: &Snapshot,
    next: &Snapshot,
    started: &HashSet<Uuid>,
) -> Result<Option<Change>, Problem> {
    for t in &next.tickets {
        let Some(was) = applied.ticket(t.id) else {
            continue;
        };
        let moved = was.engine != t.engine || was.folder != t.folder || was.view != t.view;
        if moved && started.contains(&t.id) {
            return Err(Problem::Started {
                title: was.title.clone(),
            });
        }
    }
    if applied == next {
        return Ok(None);
    }
    let engines: HashSet<Engine> = applied.tickets.iter().map(|t| t.engine).collect();
    let folders: HashSet<&str> = applied.tickets.iter().map(|t| t.folder.as_str()).collect();
    let elsewhere = next.tickets.iter().any(|t| {
        applied.ticket(t.id).is_some()
            && (!engines.contains(&t.engine) || !folders.contains(t.folder.as_str()))
    });
    let said = next.tickets.iter().any(|t| {
        applied
            .ticket(t.id)
            .is_some_and(|was| was.body.trim() != t.body.trim())
    });
    let scope = applied.goal.trim() != next.goal.trim()
        || !next.added(applied).is_empty()
        || !next.dropped(applied).is_empty()
        || elsewhere
        || said
        || raises(applied.parallel, next.parallel);
    Ok(Some(match scope {
        true => Change::Scope,
        false => Change::Minor,
    }))
}

/// Whether a plan's limit going from `was` to `now` lets it run more at
/// once: none is the boss's own limit, the most it can be.
fn raises(was: Option<i16>, now: Option<i16>) -> bool {
    match (was, now) {
        (Some(_), None) => true,
        (Some(a), Some(b)) => b > a,
        (None, _) => false,
    }
}

/// What changed from `before` to `after`, in a line the person reads:
/// "Added “Fix the flaky payment test”; moved “Upgrade the date picker”
/// after “Add the login form”".
pub fn describe(before: &Snapshot, after: &Snapshot) -> String {
    let q = |t: &Planned| format!("“{}”", crate::crew::one_line(&t.title, 80));
    let mut said = Vec::new();
    if before.goal.trim() != after.goal.trim() {
        said.push(format!(
            "changed the goal to “{}”",
            crate::crew::one_line(&after.goal, 120)
        ));
    }
    for t in after.added(before) {
        said.push(format!("added {}", q(t)));
    }
    for id in after.dropped(before) {
        if let Some(t) = before.ticket(id) {
            said.push(format!("dropped {}", q(t)));
        }
    }
    let kept: HashSet<Uuid> = before
        .tickets
        .iter()
        .filter(|t| after.ticket(t.id).is_some())
        .map(|t| t.id)
        .collect();
    let order = |s: &Snapshot| -> Vec<Uuid> {
        s.tickets
            .iter()
            .filter(|t| kept.contains(&t.id))
            .map(|t| t.id)
            .collect()
    };
    let (was, now) = (order(before), order(after));
    if was != now {
        // The first that isn't where it was.
        if let Some(i) = (0..now.len()).find(|&i| was.get(i) != now.get(i)) {
            let at = |s: &Snapshot, id: Uuid| s.ticket(id).map(q).unwrap_or_default();
            let moved = now[i];
            said.push(match i.checked_sub(1).map(|j| now[j]) {
                Some(prev) => format!("moved {} after {}", at(after, moved), at(after, prev)),
                None => format!("moved {} first", at(after, moved)),
            });
        }
    }
    for t in &after.tickets {
        let Some(was) = before.ticket(t.id) else {
            continue;
        };
        if was.title != t.title {
            said.push(format!("renamed {} {}", q(was), q(t)));
        }
        if was.body != t.body {
            said.push(format!("edited what {} says", q(t)));
        }
        let needs = |p: &Planned| p.needs.iter().copied().collect::<HashSet<Uuid>>();
        if needs(was) != needs(t) {
            let names: Vec<String> = t
                .needs
                .iter()
                .filter_map(|n| after.ticket(*n))
                .map(q)
                .collect();
            said.push(match names.is_empty() {
                true => format!("made {} need nothing first", q(t)),
                false => format!("made {} need {}", q(t), names.join(", ")),
            });
        }
        if was.engine != t.engine {
            said.push(format!("moved {} to {}", q(t), t.engine.title()));
        }
        if was.folder != t.folder || was.view != t.view {
            said.push(format!("moved {} to {}", q(t), t.folder));
        }
    }
    if before.parallel != after.parallel {
        said.push(match after.parallel {
            Some(n) => format!("runs at most {n} at once"),
            None => "runs as many at once as its boss may".into(),
        });
    }
    match said.is_empty() {
        true => "No change".into(),
        false => {
            let mut s = said.join("; ");
            if let Some(first) = s.get(..1) {
                s = first.to_uppercase() + &s[1..];
            }
            s
        }
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;

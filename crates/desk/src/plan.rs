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
use workforce_core::budget::{self, OnExceed, Period, Status, Unit, WARN_AT};

use crate::board::{TicketState, BODY_MAX, TITLE_MAX};
use crate::crew::Party;
use crate::engine::{self, Deliver, Engine, EngineView, SignIn, View};
use crate::goal::{GoalResult, Met};
use crate::model::text_enum;

/// The most tickets one plan has.
pub const MAX_TICKETS: usize = 30;
/// The most a goal says, in characters.
pub const GOAL_MAX: usize = 1_000;
/// What a ticket names to run as a sub-agent job rather than a session.
pub const SUBAGENT: &str = "subagent";

/// What every item asking the person about a plan-limit pause is keyed by.
pub const LIMIT_KEY: &str = "plan-limit:";

/// What the items asking the person about plan `plan`'s plan-limit pauses
/// are keyed by.
pub fn limit_prefix(plan: Uuid) -> String {
    format!("{LIMIT_KEY}{plan}:")
}

/// The plan an item keyed `key` asks about the plan-limit pause of.
pub fn limit_plan(key: &str) -> Option<Uuid> {
    key.strip_prefix(LIMIT_KEY)?.split(':').next()?.parse().ok()
}

/// The item asking the person about plan `plan`'s plan-limit pause until
/// `until`: one each time the limit holds it, per window.
pub fn limit_key(plan: Uuid, until: DateTime<Utc>) -> String {
    format!("{}{}", limit_prefix(plan), until.timestamp())
}

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
    /// One of its tickets failed to start again and again: the person
    /// looks at why before it is tried again.
    StartFailing,
    /// Its boss's own turns are held by the person's plan limit.
    PlanLimit,
}

impl Paused {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Budget => "budget",
            Self::BossHeld => "boss_held",
            Self::Restoring => "restoring",
            Self::NoBoss => "no_boss",
            Self::StartFailing => "start_failing",
            Self::PlanLimit => "plan_limit",
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        match s {
            "person" => Ok(Self::Person),
            "budget" => Ok(Self::Budget),
            "boss_held" => Ok(Self::BossHeld),
            "restoring" => Ok(Self::Restoring),
            "no_boss" => Ok(Self::NoBoss),
            "start_failing" => Ok(Self::StartFailing),
            "plan_limit" => Ok(Self::PlanLimit),
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
            Self::StartFailing => "a ticket of it keeps failing to start",
            Self::PlanLimit => "the plan limit holds its boss",
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
    /// Why it paused, when Desk paused it for a ticket that keeps failing
    /// to start. Sent only as its summary's `pausedWhy`.
    #[serde(skip_serializing)]
    pub paused_why: Option<String>,
    /// Until when it is paused, when that is known: the plan limit's window.
    #[serde(skip_serializing)]
    pub paused_until: Option<DateTime<Utc>>,
    /// This plan's limit of sessions at once, when lower than its boss's.
    pub parallel: Option<i16>,
    /// Its newest revision.
    pub revision: i32,
    /// The revision the runner follows; none before the person approves.
    pub approved_revision: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How done is measured, when it is a goal the person agreed: then only
    /// a met reading or the person's own words close it.
    pub result: Option<GoalResult>,
    /// The person's own words that agreed it, as checked in their DM.
    pub agreed: Option<String>,
    pub agreed_at: Option<DateTime<Utc>>,
    /// When Desk said the goal back to the person.
    pub read_back_at: Option<DateTime<Utc>>,
    /// The ticket that stands for the goal: it goes to Done only when the
    /// goal is met.
    pub goal_ticket: Option<Uuid>,
    pub met: Option<Met>,
}

impl Plan {
    /// Whether it is a goal still to be met.
    pub fn goal_open(&self) -> bool {
        self.result.is_some() && self.met.is_none()
    }

    /// Whether what it set out to do is done: it has no result to meet, or
    /// its result was met.
    pub fn goal_done(&self) -> bool {
        self.result.is_none() || self.met.is_some()
    }

    /// Whether the person's agreement covers what the boss adds toward its
    /// result: an agreed goal with a result.
    pub fn covered(&self) -> bool {
        self.agreed.is_some() && self.result.is_some()
    }

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

    /// Paused for `why`, said as `said`, until `until` when that is known.
    pub fn pause(&mut self, why: Paused, said: Option<String>, until: Option<DateTime<Utc>>) {
        self.state = PlanState::Paused;
        self.paused = Some(why);
        self.paused_why = said;
        self.paused_until = until;
    }

    /// No longer paused, for any reason.
    pub fn unpause(&mut self) {
        self.paused = None;
        self.paused_why = None;
        self.paused_until = None;
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
    /// The person's machine it runs on, as the sidebar names it
    /// (`devbox`); none is this computer. Its folder is that machine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The model its session runs, as its engine names it; none is the
    /// engine's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The repository it works in, as [`crate::repos::Repo::key`] says one
    /// (`github.com/driftwerk/vellev`): its folder on the computer it runs
    /// on is that repository's checkout there, found or cloned as it
    /// starts. With one, `folder` may be empty until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A ticket taken on from a board (one of the person's own, or a synced
    /// Linear or Jira issue) rather than made by the plan: its id is that
    /// ticket's, its title is kept here only to name it, and the plan never
    /// writes its words, deletes, drops or re-files it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopted: bool,
    /// The running session it stands for, taken on with it: its ticket is
    /// that session's, and nothing starts for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Uuid>,
}

/// What a draft says of this computer as a ticket's machine.
pub const THIS_COMPUTER: &str = "local";
/// The longest model name a ticket says.
pub const MODEL_MAX: usize = 100;

/// Whether `name` can name one of the person's machines, as the sidebar
/// does (`session:<machine>/<id>`): letters, digits, `-`, `_` and `.`, not
/// starting with a dot, and not this computer's own word.
pub fn machine_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 100
        && !name.eq_ignore_ascii_case(THIS_COMPUTER)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `model` reads as a model's name on an engine's command line,
/// never as an option: `claude-opus-4-1`, `gpt-5.5`, `sonnet[1m]`,
/// `us.anthropic.claude-sonnet-4-5:0`.
pub fn model_ok(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MODEL_MAX
        && model
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && model.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '[' | ']' | '@')
        })
}

/// What a plan may spend, as the person approves it: counted over every
/// session working on its tickets. Over its whole run unless a period is
/// said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBudget {
    pub amount: f64,
    #[serde(default = "usd")]
    pub unit: Unit,
    #[serde(default = "total")]
    pub period: Period,
    /// The share of `amount` at which it warns, between 0 and 1.
    #[serde(default = "warn_at")]
    pub warn_at: f64,
    #[serde(default = "pause")]
    pub on_exceed: OnExceed,
}

fn usd() -> Unit {
    Unit::Usd
}

fn total() -> Period {
    Period::Total
}

fn warn_at() -> f64 {
    WARN_AT
}

fn pause() -> OnExceed {
    OnExceed::Pause
}

impl PlanBudget {
    /// A cap of `amount` dollars over the plan's whole run.
    pub fn usd(amount: f64) -> Self {
        Self {
            amount,
            unit: Unit::Usd,
            period: Period::Total,
            warn_at: WARN_AT,
            on_exceed: OnExceed::Pause,
        }
    }

    /// "a total budget of $40", "a day budget of 2M tokens".
    pub fn words(&self) -> String {
        let amount = match self.unit {
            Unit::Usd => budget::usd(self.amount),
            Unit::Tokens => format!("{} tokens", budget::tokens(self.amount)),
        };
        format!("a {} budget of {amount}", self.period.words())
    }

    /// Why it can't be kept, in plain words.
    pub fn problem(&self) -> Option<String> {
        if !(self.amount.is_finite() && self.amount > 0.0) {
            return Some("A plan's budget is more than nothing.".into());
        }
        if !(self.warn_at > 0.0 && self.warn_at < 1.0) {
            return Some("A plan's budget warns somewhere between 1% and 99% of it.".into());
        }
        if self.unit == Unit::Tokens && self.amount.fract() != 0.0 {
            return Some("A token budget is a whole number of tokens.".into());
        }
        None
    }

    /// Whether going from `self` to `now` lets the plan spend more: no
    /// budget, a higher amount, a later warning, one that only warns, or
    /// one counted another way (which can't be compared, so the person
    /// says).
    pub fn loosened_by(&self, now: Option<&PlanBudget>) -> bool {
        let Some(now) = now else {
            return true;
        };
        now.unit != self.unit
            || now.period != self.period
            || now.amount > self.amount
            || now.warn_at > self.warn_at
            || (self.on_exceed == OnExceed::Pause && now.on_exceed == OnExceed::Warn)
    }
}

/// A plan's goal and tickets as of one revision, in its order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub goal: String,
    pub tickets: Vec<Planned>,
    /// The plan's own limit of sessions at once, when lower than its
    /// boss's; none is the boss's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<i16>,
    /// What the plan may spend; none is no budget of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<PlanBudget>,
    /// How its goal is measured, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<GoalResult>,
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
    /// A ticket already on a board of the group the plan takes on instead
    /// of making one: its title and description are that ticket's, so
    /// `title` and `body` aren't said.
    #[serde(default)]
    pub adopt: Option<Uuid>,
    /// The running session already on the work: its ticket is taken on
    /// (made first, assigned to it in Doing, when it has none), and nothing
    /// starts for it.
    #[serde(default)]
    pub session: Option<Uuid>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// None: the boss's default engine.
    #[serde(default)]
    pub engine: Option<String>,
    /// Its folder on the computer it runs on; may be empty when it names
    /// its `repo`.
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub view: Option<String>,
    /// The places in the list (from 0) of the tickets it needs done first.
    #[serde(default)]
    pub needs: Vec<usize>,
    /// The person's machine it runs on (`devbox`), or [`THIS_COMPUTER`];
    /// none keeps a ticket the plan has where it runs, and starts a new
    /// one on this computer.
    #[serde(default)]
    pub machine: Option<String>,
    /// The model its session runs; `""` is the engine's default, none
    /// keeps a ticket the plan has on its model.
    #[serde(default)]
    pub model: Option<String>,
    /// Its repository (`github.com/owner/name`, or `owner/name` for
    /// GitHub); `""` none, and none keeps a ticket the plan has on its own.
    #[serde(default)]
    pub repo: Option<String>,
}

/// One ticket the plan has, changed in place: only what it says changes,
/// and its title, description, needs and history are kept. Each field reads
/// as [`DraftTicket`]'s does: `machine` [`THIS_COMPUTER`] (or `""`) is this
/// computer, `model` `""` its engine's own, and a model is kept only while
/// its engine is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketEdit {
    pub id: Uuid,
    /// How it was named (its key, like VEL-2), for the words read back.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
}

impl TicketEdit {
    /// What it is called in the words read back: its key, else its id.
    pub fn name(&self) -> String {
        self.key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map_or_else(|| self.id.to_string(), str::to_string)
    }

    /// Whether it says where, or how, the ticket's session runs.
    pub fn moves(&self) -> bool {
        self.engine.is_some()
            || self.folder.is_some()
            || self.view.is_some()
            || self.machine.is_some()
            || self.model.is_some()
            || self.repo.is_some()
    }

    /// Whether it changes where, or how, `was` runs: a field it gives that
    /// says what `was` says already changes nothing.
    pub fn moves_from(&self, was: &Planned) -> bool {
        let differs = |said: &Option<String>, now: Option<&str>| {
            said.as_deref()
                .map(str::trim)
                .is_some_and(|s| s != now.unwrap_or_default())
        };
        let machine = self.machine.as_deref().map(str::trim).is_some_and(|m| {
            let m = (!m.is_empty() && !m.eq_ignore_ascii_case(THIS_COMPUTER)).then_some(m);
            match (m, was.machine.as_deref()) {
                (None, None) => false,
                (Some(a), Some(b)) => !a.eq_ignore_ascii_case(b),
                _ => true,
            }
        });
        // An engine left empty is the default one, Claude.
        let engine = self.engine.as_deref().map(str::trim).is_some_and(|e| {
            let e = if e.is_empty() {
                Engine::Claude.as_str()
            } else {
                e
            };
            !e.eq_ignore_ascii_case(was.engine.as_str())
        });
        let view = self.view.as_deref().map(str::trim).is_some_and(|v| {
            !v.eq_ignore_ascii_case(was.view.map(|w| w.as_str()).unwrap_or_default())
        });
        machine
            || engine
            || differs(&self.folder, Some(&was.folder))
            || view
            || differs(&self.model, was.model.as_deref())
            || self.repo.as_deref().is_some_and(|r| {
                let r = crate::repos::repo_of(r).map(|r| r.key());
                r.as_deref() != was.repo.as_deref()
            })
    }
}

/// `before` with `edits` made, each to the ticket of it its id names; the
/// rest as they are. What an edit says is read as a whole list's ticket is
/// ([`snapshot_with`]), so the same problems refuse it, each named by its
/// key: a ticket it doesn't have, one edited twice, words given to one
/// taken on from its board, and a move to another machine that doesn't say
/// its folder there.
pub fn edited(before: &Snapshot, edits: &[TicketEdit]) -> Result<Snapshot, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    let place: HashMap<Uuid, usize> = before
        .tickets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i))
        .collect();
    let mut draft = Draft {
        goal: before.goal.clone(),
        tickets: before
            .tickets
            .iter()
            .map(|t| DraftTicket {
                id: Some(t.id),
                adopt: None,
                session: None,
                title: t.title.clone(),
                body: t.body.clone(),
                engine: Some(t.engine.as_str().to_string()),
                folder: t.folder.clone(),
                view: t.view.map(|v| v.as_str().to_string()),
                needs: t
                    .needs
                    .iter()
                    .filter_map(|n| place.get(n).copied())
                    .collect(),
                machine: Some(
                    t.machine
                        .clone()
                        .unwrap_or_else(|| THIS_COMPUTER.to_string()),
                ),
                model: None,
                repo: None,
            })
            .collect(),
    };
    for e in edits {
        let key = e.name();
        let Some(&i) = place.get(&e.id) else {
            problems.push(Problem::NotInPlan { key });
            continue;
        };
        if !seen.insert(e.id) {
            problems.push(Problem::EditedTwice { key });
            continue;
        }
        let was = &before.tickets[i];
        let t = &mut draft.tickets[i];
        if was.adopted && (e.title.is_some() || e.body.is_some()) {
            problems.push(Problem::AdoptedWords { key: key.clone() });
        }
        if let Some(title) = &e.title {
            t.title.clone_from(title);
        }
        if let Some(body) = &e.body {
            t.body.clone_from(body);
        }
        if let Some(engine) = &e.engine {
            t.engine = Some(engine.clone());
        }
        if let Some(view) = &e.view {
            t.view = Some(view.clone());
        }
        if let Some(model) = &e.model {
            t.model = Some(model.clone());
        }
        if let Some(folder) = &e.folder {
            t.folder = folder.trim().to_string();
        }
        if let Some(repo) = &e.repo {
            t.repo = Some(repo.clone());
        }
        if let Some(machine) = &e.machine {
            let to = machine.trim();
            let now = match to.is_empty() || to.eq_ignore_ascii_case(THIS_COMPUTER) {
                true => None,
                false => Some(to),
            };
            // The machine it runs on already, however it is spelled.
            let same = match (now, was.machine.as_deref()) {
                (None, None) => true,
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            };
            if same {
                t.machine = Some(
                    was.machine
                        .clone()
                        .unwrap_or_else(|| THIS_COMPUTER.to_string()),
                );
                continue;
            }
            // Its repository says its folder there, when it has one.
            let repo = e
                .repo
                .as_deref()
                .map_or(was.repo.is_some(), |r| !r.trim().is_empty());
            if e.folder.is_none() {
                match repo {
                    // The folder it had is on the other computer: its
                    // repository's checkout there is found as it starts.
                    true => t.folder.clear(),
                    false => problems.push(Problem::MovedWithoutFolder {
                        key: key.clone(),
                        machine: now.unwrap_or("this computer").to_string(),
                    }),
                }
            }
            t.machine = Some(machine.clone());
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    // Each model the plan has is kept, as a whole list keeps it, unless an
    // edit says another: `None` keeps it only while its engine is the same.
    let mut next = snapshot_with(
        &draft,
        Some(before),
        Engine::Claude,
        &HashMap::new(),
        &mut Uuid::new_v4,
    )?;
    next.parallel = before.parallel;
    next.budget.clone_from(&before.budget);
    Ok(next)
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
    #[error("Ticket {ticket} asks for a sub-agent; a plan's tickets run in real sessions, so give it an engine.")]
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
    #[error("Ticket {ticket}'s folder can't hold its repository: {why}.")]
    NoCheckout { ticket: usize, why: String },
    #[error("Ticket {ticket}'s folder {folder:?} isn't a full path on {machine}: give it as /home/… or /Users/… there.")]
    NoMachineFolder {
        ticket: usize,
        folder: String,
        machine: String,
    },
    #[error("Ticket {ticket} names the machine {machine:?}, which can't be one of the person's machines; name one as the sidebar does (like devbox), or local for this computer.")]
    BadMachine { ticket: usize, machine: String },
    #[error("Ticket {ticket} names the repository {repo:?}: say it as host/owner/name, like github.com/driftwerk/vellev, or owner/name for GitHub.")]
    BadRepo { ticket: usize, repo: String },
    #[error("Ticket {ticket} says neither its folder nor its repo: say one.")]
    NoFolderOrRepo { ticket: usize },
    #[error("Ticket {ticket} names the model {model:?}, which can't be a model's name.")]
    BadModel { ticket: usize, model: String },
    #[error("Ticket {ticket} isn't one this plan has; leave out its id to add it.")]
    UnknownTicket { ticket: usize },
    #[error("Ticket {ticket} takes on a ticket that isn't on a board of the plan's group.")]
    UnknownAdopt { ticket: usize },
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
    #[error("{0}")]
    Budget(String),
    #[error("{key} isn't one of the plan's tickets.")]
    NotInPlan { key: String },
    #[error("{key} is edited twice; say each ticket once.")]
    EditedTwice { key: String },
    #[error("{key} is taken on from its board: its title and description are its board's, so leave title and body out.")]
    AdoptedWords { key: String },
    #[error(
        "{key} moves to {machine}: say its folder there too, as an absolute path on {machine}, or its repo (like github.com/owner/name)."
    )]
    MovedWithoutFolder { key: String, machine: String },
    #[error("{key} has started (it is in {column}), so where it runs can't change: leave it out, or once it is done add a ticket for what is left.")]
    StartedEdit { key: String, column: &'static str },
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
    snapshot_with(draft, before, default_engine, &HashMap::new(), new_id)
}

/// [`snapshot_of`], with the tickets the draft may take on (`adoptable`:
/// each by its id, with its title): one a ticket of the draft adopts that
/// `before` doesn't have must be among them.
pub fn snapshot_with(
    draft: &Draft,
    before: Option<&Snapshot>,
    default_engine: Engine,
    adoptable: &HashMap<Uuid, String>,
    new_id: &mut dyn FnMut() -> Uuid,
) -> Result<Snapshot, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    let mut ids = Vec::with_capacity(draft.tickets.len());
    // Each ticket that takes one on, with that ticket's title.
    let mut adopted: HashMap<usize, String> = HashMap::new();
    for (i, t) in draft.tickets.iter().enumerate() {
        let ticket = i + 1;
        let had = |id: Uuid| before.and_then(|b| b.ticket(id));
        let id = match (t.adopt, t.id) {
            // One the plan has already, by either name.
            (Some(id), _) | (None, Some(id)) if had(id).is_some_and(|w| w.adopted) => {
                adopted.insert(i, had(id).map(|w| w.title.clone()).unwrap_or_default());
                id
            }
            (Some(id), _) => {
                match (had(id), adoptable.get(&id)) {
                    (None, Some(title)) => {
                        adopted.insert(i, title.clone());
                    }
                    // A ticket of the plan's own isn't taken on.
                    (Some(_), _) => problems.push(Problem::UnknownAdopt { ticket }),
                    (None, None) => problems.push(Problem::UnknownAdopt { ticket }),
                }
                id
            }
            (None, Some(id)) if had(id).is_none() => {
                problems.push(Problem::UnknownTicket { ticket });
                id
            }
            (None, Some(id)) => id,
            (None, None) => new_id(),
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
        let was = before.and_then(|b| b.ticket(ids[i]));
        let machine = match t.machine.as_deref().map(str::trim) {
            None => was.and_then(|w| w.machine.clone()),
            Some(m) if m.is_empty() || m.eq_ignore_ascii_case(THIS_COMPUTER) => None,
            Some(m) if machine_ok(m) => Some(m.to_string()),
            Some(m) => {
                problems.push(Problem::BadMachine {
                    ticket,
                    machine: m.to_string(),
                });
                None
            }
        };
        // A model is its engine's: one kept from before stays only while the
        // ticket keeps that engine, and one given must be a name its engine
        // takes ([`crate::engine::model_ok`]).
        let model = match t.model.as_deref().map(str::trim) {
            None => was
                .filter(|w| Some(w.engine) == engine)
                .and_then(|w| w.model.clone()),
            Some("") => None,
            Some(m) if engine.map_or(model_ok(m), |e| crate::engine::model_ok(e, m)) => {
                Some(m.to_string())
            }
            Some(m) => {
                problems.push(Problem::BadModel {
                    ticket,
                    model: m.chars().filter(|c| !c.is_control()).take(120).collect(),
                });
                None
            }
        };
        let repo = match t.repo.as_deref().map(str::trim) {
            None => was.and_then(|w| w.repo.clone()),
            Some("") => None,
            Some(r) => match crate::repos::repo_of(r) {
                Some(r) => Some(r.key()),
                None => {
                    problems.push(Problem::BadRepo {
                        ticket,
                        repo: r.chars().filter(|c| !c.is_control()).take(120).collect(),
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
            // A ticket taken on is named by its own title; its words are its
            // board's, never the plan's.
            let (title, body) = match adopted.get(&i) {
                Some(title) => (title.trim().to_string(), String::new()),
                None => (t.title.trim().to_string(), t.body.trim().to_string()),
            };
            tickets.push(Planned {
                id: ids[i],
                title,
                body,
                engine,
                folder: t.folder.trim().to_string(),
                view,
                needs,
                machine,
                model,
                repo,
                adopted: adopted.contains_key(&i),
                session: adopted
                    .contains_key(&i)
                    .then(|| t.session.or_else(|| was.and_then(|w| w.session)))
                    .flatten(),
            });
        }
    }
    match problems.is_empty() {
        true => Ok(Snapshot {
            goal: draft.goal.trim().to_string(),
            tickets,
            parallel: before.and_then(|b| b.parallel),
            budget: before.and_then(|b| b.budget.clone()),
            result: before.and_then(|b| b.result.clone()),
        }),
        false => Err(problems),
    }
}

/// Whether `s` can run on this computer: a goal, between one and
/// [`MAX_TICKETS`] tickets, each with a title, an engine that is installed,
/// signed in and has a line for its boss (`engines`, as Desk reads them),
/// a folder that `fits` (its repository's, when it has one: why not when
/// it doesn't), and needs that never loop.
pub fn validate(
    s: &Snapshot,
    engines: &[EngineView],
    fits: &dyn Fn(&str, Option<&str>) -> Result<(), String>,
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
    if let Some(why) = s.budget.as_ref().and_then(PlanBudget::problem) {
        problems.push(Problem::Budget(why));
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
        // A ticket taken on says what its board says.
        if title.is_empty() && !t.adopted {
            problems.push(Problem::NoTitle { ticket });
        }
        let len = title.chars().count();
        if len > TITLE_MAX && !t.adopted {
            problems.push(Problem::LongTitle { ticket, len });
        }
        let len = t.body.chars().count();
        if len > BODY_MAX && !t.adopted {
            problems.push(Problem::LongBody { ticket, len });
        }
        let folder = t.folder.trim();
        match &t.machine {
            // On a machine, what is there is the machine's to say when the
            // session starts: here only that it can start there at all.
            Some(machine) => {
                if let Some(p) = startable_problem(ticket, t.engine, t.view) {
                    problems.push(p);
                }
                if folder.is_empty() && t.repo.is_none() {
                    problems.push(Problem::NoFolderOrRepo { ticket });
                } else if !folder.is_empty() && !folder.starts_with('/') {
                    problems.push(Problem::NoMachineFolder {
                        ticket,
                        folder: t.folder.clone(),
                        machine: machine.clone(),
                    });
                }
            }
            None => {
                if let Some(p) = engine_problem(ticket, t.engine, t.view, engines) {
                    problems.push(p);
                }
                if folder.is_empty() && t.repo.is_none() {
                    problems.push(Problem::NoFolderOrRepo { ticket });
                } else if folder.is_empty() {
                    // Its repository's checkout, found or cloned as it starts.
                } else if !std::path::Path::new(folder).is_absolute() {
                    problems.push(Problem::NoFolder {
                        ticket,
                        folder: t.folder.clone(),
                    });
                } else if let Err(why) = fits(folder, t.repo.as_deref()) {
                    problems.push(match &t.repo {
                        None => Problem::NoFolder {
                            ticket,
                            folder: t.folder.clone(),
                        },
                        Some(_) => Problem::NoCheckout { ticket, why },
                    });
                }
            }
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
    startable_problem(ticket, engine, view)
}

/// What keeps `engine` in `view` from running a boss's ticket anywhere:
/// Desk can't start it there, or it has no line for its boss.
fn startable_problem(ticket: usize, engine: Engine, view: Option<View>) -> Option<Problem> {
    let title = engine.title();
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
    /// The person's machine it runs on; none is this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The model its session runs; none is its engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The repository it works in ([`Planned::repo`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A session or an agent has it.
    #[serde(default)]
    pub assigned: bool,
    /// The session that has it, by its assignee's id (`machine/id` for one
    /// on a machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The runner is starting a session for it.
    #[serde(default)]
    pub starting: bool,
    /// Why it waits in To do for someone to start it: the runner doesn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_back: Option<String>,
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

/// Whether `t` may start now: in To do, not held back, every need Done (a
/// need not among `states` holds it back), and no one has it or is
/// starting it.
fn startable(t: &PlanTicket, states: &HashMap<Uuid, TicketState>) -> bool {
    t.state == TicketState::Todo
        && t.held_back.is_none()
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
    /// Its repository, whose checkout its folder is (found or cloned as it
    /// starts, when its folder is empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// The tickets of the sessions a plan holds, one each in order: every
/// session its tickets in Doing are on until it is closed or removed, every
/// one its tickets in Review are on while it is `busy` (starting, working
/// or blocked: an idle or done one waits on the merge, not on a slot), and
/// every start of one under way or whose outcome isn't known yet.
pub fn holding(tickets: &[PlanTicket], busy: impl Fn(&str) -> bool) -> Vec<&PlanTicket> {
    let mut seen = HashSet::new();
    tickets
        .iter()
        .filter(|t| {
            let on = t.session.as_deref().filter(|s| {
                t.starting
                    || match t.state {
                        TicketState::Doing => true,
                        TicketState::Review => busy(s),
                        _ => false,
                    }
            });
            match on {
                Some(s) => seen.insert(s.to_string()),
                None => t.starting && seen.insert(t.id.to_string()),
            }
        })
        .collect()
}

/// How many sessions a plan holds ([`holding`]).
pub fn held(tickets: &[PlanTicket], busy: impl Fn(&str) -> bool) -> usize {
    holding(tickets, busy).len()
}

/// How many more sessions `plan` may start now: fewer than its own limit
/// counts what it `held`, and fewer than its boss's `limit` counts the
/// boss's sessions starting or working (`busy`).
pub fn room(plan: &Plan, held: usize, busy: usize, limit: usize) -> usize {
    plan.limit(limit)
        .saturating_sub(held)
        .min(limit.saturating_sub(busy))
}

/// What the runner starts now for `plan`: its ready tickets in rank order,
/// at most `room` of them ([`room`]). None while the plan doesn't run (not
/// approved, paused, done or dropped), or while `standing`, its group's
/// spend, is at its warning or over.
pub fn next_starts(
    plan: &Plan,
    tickets: &[PlanTicket],
    room: usize,
    standing: Option<Status>,
) -> Vec<Start> {
    if !plan.runs() {
        return Vec::new();
    }
    match standing {
        Some(Status::Warn | Status::Over) => return Vec::new(),
        Some(Status::Under) | None => {}
    }
    let states: HashMap<Uuid, TicketState> = tickets.iter().map(|t| (t.id, t.state)).collect();
    let mut ready: Vec<&PlanTicket> = tickets.iter().filter(|t| startable(t, &states)).collect();
    ready.sort_by(|a, b| a.rank.total_cmp(&b.rank));
    // A folder is in use on its own computer: the same path on a machine is
    // another folder. A repository is one checkout on each computer, so two
    // tickets of it there share a folder; so do a ticket of its folder and
    // one of its repository whose found checkout it is.
    let places = |t: &PlanTicket| -> Vec<String> {
        let mut at = Vec::new();
        if let Some(r) = t.repo.as_deref() {
            at.push(format!("repo:{r}"));
        }
        if let Some(f) = t.folder.as_deref().filter(|f| !f.is_empty()) {
            at.push(f.trim_end_matches('/').to_string());
        }
        at
    };
    let mut in_use: HashSet<(Option<&str>, String)> = tickets
        .iter()
        .filter(|t| t.state == TicketState::Doing || t.starting)
        .flat_map(|t| places(t).into_iter().map(|f| (t.machine.as_deref(), f)))
        .collect();
    let mut out = Vec::new();
    for t in ready {
        if out.len() >= room {
            break;
        }
        let at = places(t);
        let Some(engine) = t.engine.filter(|_| !at.is_empty()) else {
            continue;
        };
        let mut shared = false;
        for f in at {
            shared |= !in_use.insert((t.machine.as_deref(), f));
        }
        out.push(Start {
            ticket: t.id,
            engine,
            folder: t.folder.clone().unwrap_or_default(),
            view: t.view,
            worktree: shared,
            repo: t.repo.clone(),
        });
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
    /// Held back by a need that was dropped, or by its session ending as
    /// it started: someone must decide.
    pub blocked: usize,
    /// Waiting on needs not done yet, or not handed out yet.
    pub waiting: usize,
    /// Every ticket but the dropped.
    pub total: usize,
    /// The tickets that start next as slots free, by key, in order.
    pub next: Vec<String>,
    /// The tickets of the sessions it holds, one each ([`holding`]), by key.
    #[serde(default)]
    pub holding: Vec<String>,
}

/// Where `tickets`, a plan's, stand, their sessions `busy` as [`holding`]
/// reads them.
pub fn progress(tickets: &[PlanTicket], busy: impl Fn(&str) -> bool) -> Progress {
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
                if dropped_need || t.held_back.is_some() {
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
    p.holding = holding(tickets, busy).iter().map(|t| t.name()).collect();
    p
}

/// "holds 4 of 4 sessions: DES-1, DES-2, DES-3, DES-4", while `plan` holds
/// as many sessions as it may run at once (`cap`): it starts no more until
/// one of them is closed or its ticket moves on.
pub fn full(p: &Progress, cap: usize) -> Option<String> {
    let n = p.holding.len();
    (cap > 0 && n >= cap).then(|| {
        let sessions = if cap == 1 { "session" } else { "sessions" };
        format!("holds {n} of {cap} {sessions}: {}", p.holding.join(", "))
    })
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
/// already uses, a lower limit, and a budget set or lowered. Scope, which
/// the person approves: the goal, a ticket added or dropped, what a ticket
/// says (its session's first prompt), an engine or a folder the approved
/// revision doesn't use, a higher limit, and a budget raised or taken off.
/// Where a ticket that has `started` runs can't change.
///
/// A goal the person agreed `covered` the work toward its result: then a
/// ticket added or dropped, or what one says, is minor, all but the goal's
/// own ticket. Its goal or result changing is never covered; where a
/// result reads from, its measures and bounds the same, is minor.
pub fn plan_change(
    applied: &Snapshot,
    next: &Snapshot,
    started: &HashSet<Uuid>,
    covered: Option<&Covered>,
) -> Result<Option<Change>, Problem> {
    for t in &next.tickets {
        let Some(was) = applied.ticket(t.id) else {
            continue;
        };
        let moved = was.engine != t.engine
            || was.folder != t.folder
            || was.view != t.view
            || was.machine != t.machine
            || was.model != t.model
            || was.repo != t.repo;
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
    let machines: HashSet<Option<&str>> = applied
        .tickets
        .iter()
        .map(|t| t.machine.as_deref())
        .collect();
    let repos: HashSet<Option<&str>> = applied.tickets.iter().map(|t| t.repo.as_deref()).collect();
    let elsewhere = next.tickets.iter().any(|t| {
        applied.ticket(t.id).is_some()
            && (!engines.contains(&t.engine)
                || !folders.contains(t.folder.as_str())
                || !machines.contains(&t.machine.as_deref())
                || !repos.contains(&t.repo.as_deref()))
    });
    let said: Vec<Uuid> = next
        .tickets
        .iter()
        .filter(|t| {
            applied
                .ticket(t.id)
                .is_some_and(|was| was.body.trim() != t.body.trim())
        })
        .map(|t| t.id)
        .collect();
    let dropped = next.dropped(applied);
    let work = match covered {
        Some(c) => c
            .goal_ticket
            .is_some_and(|g| dropped.contains(&g) || said.contains(&g)),
        None => !next.added(applied).is_empty() || !dropped.is_empty() || !said.is_empty(),
    };
    let measured_otherwise = match (&applied.result, &next.result) {
        (Some(a), Some(b)) => !a.same_but_address(b),
        (a, b) => a != b,
    };
    let scope = applied.goal.trim() != next.goal.trim()
        || measured_otherwise
        || work
        || elsewhere
        || raises(applied.parallel, next.parallel)
        || applied
            .budget
            .as_ref()
            .is_some_and(|b| b.loosened_by(next.budget.as_ref()));
    Ok(Some(match scope {
        true => Change::Scope,
        false => Change::Minor,
    }))
}

/// What the person's agreement of a goal covers of a change to its plan:
/// every ticket toward its result but `goal_ticket`, the one standing for
/// the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Covered {
    pub goal_ticket: Option<Uuid>,
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
    if before.result != after.result {
        said.push(match (&before.result, &after.result) {
            (Some(was), Some(now)) if was.same_but_address(now) => {
                "changed where its goal is read from".into()
            }
            (_, Some(r)) => format!(
                "changed how it's measured to “{}”",
                crate::crew::one_line(&r.words, 120)
            ),
            (_, None) => "stopped measuring it".into(),
        });
    }
    for t in after.added(before) {
        said.push(match t.adopted {
            true => format!("took on {}", q(t)),
            false => format!("added {}", q(t)),
        });
    }
    for id in after.dropped(before) {
        if let Some(t) = before.ticket(id) {
            said.push(match t.adopted {
                true => format!("let go of {}", q(t)),
                false => format!("dropped {}", q(t)),
            });
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
        if was.title != t.title && !t.adopted {
            said.push(format!("renamed {} {}", q(was), q(t)));
        }
        if was.body != t.body && !t.adopted {
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
        if was.folder.trim() != t.folder.trim() {
            said.push(match t.folder.trim() {
                "" => format!("left the folder of {} for Desk to find", q(t)),
                f => format!("moved {} to {f}", q(t)),
            });
        }
        if was.view != t.view {
            said.push(match t.view {
                Some(v) => format!("set {} to run in the {} view", q(t), v.as_str()),
                None => format!("set {} to run in its engine's own view", q(t)),
            });
        }
        if was.repo != t.repo {
            said.push(match &t.repo {
                Some(r) => format!("set {} to work in {r}", q(t)),
                None => format!("set {} to work in its folder alone", q(t)),
            });
        }
        if was.machine != t.machine {
            said.push(format!(
                "moved {} to {}",
                q(t),
                t.machine.as_deref().unwrap_or("this computer")
            ));
        }
        if was.model != t.model {
            said.push(match &t.model {
                Some(m) => format!("set {} to run {m}", q(t)),
                None => format!("set {} to its engine's own model", q(t)),
            });
        }
    }
    if before.budget != after.budget {
        said.push(match &after.budget {
            Some(b) => format!("capped it at {}", b.words()),
            None => "took its budget off".into(),
        });
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

// ── Reviews ─────────────────────────────────────────────────────────────

/// A pull request on GitHub: `owner/repo#number`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: i64,
}

impl PrRef {
    /// `owner/repo#number`, its names in lowercase as GitHub matches them,
    /// so one pull request is one `PrRef` however it was spelled.
    pub fn new(owner: &str, repo: &str, number: i64) -> Self {
        Self {
            owner: owner.to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
            number,
        }
    }
}

/// What a review session is started to review: a pull request, at its
/// head as it starts, which its verdict names ([`REVIEW_VERDICT`]), and
/// the session whose work it is, when its boss runs one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOf {
    #[serde(flatten)]
    pub pr: PrRef,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_session: Option<Uuid>,
}

/// A review session as Desk linked it: the pull request it reviews, at
/// the head it was started at, for the session whose work that is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLink {
    pub session: Uuid,
    pub boss: String,
    pub pr: PrRef,
    pub head_sha: Option<String>,
    pub for_session: Option<Uuid>,
    /// Desk is done with it: its verdict recorded, or nothing left to
    /// record ([`crate::store::DeskStore::pr_review_settle`]).
    pub settled: bool,
}

/// The worktree a review of `pr` at `head` reads it in, under its
/// repository's `.worktrees/`: `review-<number>-<short head>`.
pub fn review_worktree(pr: &PrRef, head: &str) -> String {
    format!("review-{}-{}", pr.number, &head[..head.len().min(7)])
}

/// Why a session's own merge of a pull request is refused while a boss
/// acting fully has it.
pub const OWN_MERGE: &str = "At Act fully a pull request is merged only by your boss, with \
crew.github_merge, once a clean review is recorded at its head.";

/// What a session does instead of merging its own pull request.
pub const OWN_MERGE_INSTEAD: &str = "Don't merge it yourself: tell your boss on your Desk line \
that it is ready to merge; your boss merges it once it has a clean review at its head.";

/// [`OWN_MERGE`] and [`OWN_MERGE_INSTEAD`], as a refused call says them.
pub fn own_merge_refused() -> String {
    format!("{OWN_MERGE} {OWN_MERGE_INSTEAD}")
}

/// Why a session's push straight to a default branch that pull requests
/// merge into is refused while a boss acting fully has it.
pub const DEFAULT_PUSH: &str = "At Act fully the default branch changes only by a pull request \
your boss merges once a clean review is recorded at its head.";

/// What a session does instead of pushing its default branch.
pub const DEFAULT_PUSH_INSTEAD: &str = "Push your own branch and open a pull request for it; Desk \
has it reviewed, and your boss merges it.";

/// [`DEFAULT_PUSH`] and [`DEFAULT_PUSH_INSTEAD`], as a refused call says
/// them.
pub fn default_push_refused() -> String {
    format!("{DEFAULT_PUSH} {DEFAULT_PUSH_INSTEAD}")
}

impl ReviewOf {
    /// Why it can't be reviewed as said, when it can't.
    pub fn check(&self) -> Result<(), String> {
        let named = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !named(&self.pr.owner) || !named(&self.pr.repo) || self.pr.number < 1 {
            return Err("a review names its pull request as owner, repo and number".into());
        }
        match is_sha(&self.head_sha) {
            true => Ok(()),
            false => Err("a review names its pull request's head as a full commit SHA".into()),
        }
    }
}

/// What a review session said of a pull request, at the head it reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrReview {
    /// The boss whose review session it was.
    pub boss: String,
    #[serde(flatten)]
    pub pr: PrRef,
    pub head_sha: String,
    pub clean: bool,
    pub by_session: Uuid,
    pub at: DateTime<Utc>,
}

/// What Desk adds to a review session's prompt, `{head}` the pull
/// request's head as it starts: the verdict line its final reply ends with
/// ([`review_verdict`]).
pub const REVIEW_VERDICT: &str = "When your review is done, end your final reply with exactly \
one line of its own naming the commit you reviewed (the pull request's head is {head}): \
REVIEW: clean {head} when it is ready to merge as it is, or REVIEW: changes {head} when it \
needs changes.";

/// [`REVIEW_VERDICT`], for a pull request at `head`.
pub fn review_instruction(head: &str) -> String {
    REVIEW_VERDICT.replace("{head}", head)
}

/// The verdict a review's reply ends with ([`REVIEW_VERDICT`]): its last
/// line reading `REVIEW: clean <sha>` or `REVIEW: changes <sha>`, with the
/// full SHA of the commit it reviewed, as a model writes it: spaced, in
/// code or bold, or ending its sentence. `true`: clean.
pub fn review_verdict(text: &str) -> Option<(bool, String)> {
    text.lines().rev().find_map(|line| {
        let bare = line
            .trim()
            .trim_matches(|c| matches!(c, '`' | '*'))
            .trim_end_matches(['.', '\r'])
            .trim_matches(|c| matches!(c, '`' | '*'));
        let (word, sha) = bare.strip_prefix("REVIEW: ")?.split_once(' ')?;
        let clean = match word {
            "clean" => true,
            "changes" => false,
            _ => return None,
        };
        is_sha(sha).then(|| (clean, sha.to_string()))
    })
}

/// Whether `s` is a full commit SHA, as git spells it: 40 lowercase hex.
pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;

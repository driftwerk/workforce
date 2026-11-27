//! Plans ([`crate::plan`]) as the desk keeps them: a boss proposes one, its
//! tickets go on the board in Backlog, and nothing runs until the person
//! approves it. Each change is a revision. A change within what was
//! approved applies at once; a change of scope the boss makes waits for the
//! person, while the plan runs on as approved. Each change is stored,
//! recorded in Activity and announced as `{type: "plan"}`.
//!
//! Starting a ticket's session is the hub runner's to decide; doing it is
//! here, in two steps a restart can't split: [`Desk::plan_start`] moves
//! the ticket to Doing with a `starting` link naming the session, then
//! starts that session; [`Desk::plan_restart_start`] finishes a start a
//! stop cut off, starting the same session, never a second one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError};
use super::{Move, NewBoard};
use crate::board::{self, Board, Link, Ticket, TicketState, RANK_STEP};
use crate::crew::{self, By, Kind, Party, PartyKind};
use crate::engine::{Engine, View};
use crate::floor::{Answer, DecideRef, Need, NeedKind, Place, Who};
use crate::goal::{self, GoalResult, Met};
use crate::model::{DeskEvent, NewSession, SessionState, SessionView};
use crate::plan::{
    self, Change, Draft, DraftTicket, Paused, Plan, PlanBudget, PlanState, PlanTicket, Planned,
    Problem, Progress, Revision, Snapshot, Start, TicketEdit,
};
use crate::plan_store::{Adopt, BeginStart, Claim, Runs, Starting};
use crate::scope;
use workforce_core::budget::{BudgetScope, Standing};

/// What a ticket's session is told when a session worked on it before
/// and let it go (its session ended or went away while it was in Doing).
pub(crate) const AGAIN_NOTE: &str = "\n\nThis ticket was started before and its session ended. Check what is already done first (its folder, branch, commits and pull requests) and carry on from there; don't redo it.";

/// What the desk keeps of plans in memory: the ones waiting on the person,
/// for the Floor, which is read without waiting on Postgres.
#[derive(Default)]
pub(crate) struct Plans {
    awaiting: HashMap<Uuid, Need>,
    /// What each boss that made a plan is called, by agent id: until the
    /// hub's roster says.
    names: HashMap<String, String>,
}

/// A plan with where its tickets stand: the list, and `{type: "plan"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    #[serde(flatten)]
    pub plan: Plan,
    pub boss_name: String,
    /// What the person calls its group; none in Ungrouped.
    pub group_name: Option<String>,
    /// Why it is paused, in the person's words, while it is.
    pub paused_why: Option<String>,
    pub progress: Progress,
    /// What it may spend, as approved (or as proposed, before it is).
    pub budget: Option<PlanBudget>,
    /// What holds its new starts, in the Floor's words, while Desk's
    /// breaker does: its own budget, its group's or Desk's. The person
    /// says Continue on the Floor.
    pub held: Option<String>,
    /// How many sessions it runs at once: its own limit within its boss's,
    /// as its runner counts them; none while no boss of it oversees.
    pub limit: Option<usize>,
}

/// A plan's ticket as its card shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTicketView {
    #[serde(flatten)]
    pub ticket: super::TicketView,
    pub engine: Option<Engine>,
    pub folder: Option<String>,
    pub view: Option<View>,
    /// The person's machine it runs on; none is this computer.
    pub machine: Option<String>,
    /// The model its session runs; none is its engine's default.
    pub model: Option<String>,
    /// The repository it works in, by where it comes from
    /// (`github.com/owner/name`).
    pub repo: Option<String>,
    /// Taken on from its board, not made by the plan: its key and link are
    /// its board's (a Linear or Jira issue's, when synced).
    pub adopted: bool,
    /// Its session is being started.
    pub starting: bool,
    /// What holds its session, while Desk's breaker does: the ticket is
    /// blocked on the person, and nothing is started in its place.
    pub held: Option<String>,
    /// Why it waits in To do for someone to start it ([`Runs::held_back`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_back: Option<String>,
    /// The session it is assigned to was closed or removed, so it holds no
    /// slot of its plan ([`Runs::session_closed`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_closed: bool,
}

/// A plan moved to another group, and why its running sessions weren't
/// moved with it, when they weren't.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Regrouped {
    #[serde(flatten)]
    pub plan: PlanDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmoved: Option<String>,
}

/// A plan in full: its tickets, its waves, and every revision.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanDetail {
    #[serde(flatten)]
    pub summary: PlanSummary,
    pub tickets: Vec<PlanTicketView>,
    pub waves: Vec<Vec<Uuid>>,
    pub revisions: Vec<Revision>,
    /// Where its budget stands now, when it has one and it can be counted.
    pub spend: Option<Standing>,
    /// How its goal stands, part by part, when it has a result.
    #[serde(rename = "goalProgress", skip_serializing_if = "Option::is_none")]
    pub goal: Option<goal::Progress>,
    /// What its boss reported and showed toward its goal, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<goal::Fact>,
}

/// A plan a boss proposes.
#[derive(Debug, Clone)]
pub struct NewPlan {
    pub draft: Draft,
    /// Its board; the group's own (made when it has none) when not said.
    pub board: Option<Uuid>,
    pub parallel: Option<i16>,
    /// The engine of a ticket that names none: the boss's default.
    pub engine: Engine,
    /// The group its work belongs to; none is Ungrouped.
    pub group: Option<String>,
    /// What it may spend, as the boss proposes it.
    pub budget: Option<PlanBudget>,
    /// How its goal is measured, when it is one.
    pub result: Option<GoalResult>,
    /// The ticket that stands for the goal; the first it takes on when not
    /// said.
    pub goal_ticket: Option<Uuid>,
}

/// A change to a plan, by the boss or the person.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEdit {
    /// The revision it was made from: an edit from an older one is refused.
    #[serde(default)]
    pub revision: Option<i32>,
    #[serde(default)]
    pub goal: Option<String>,
    /// `null` takes the boss's limit again.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub parallel: Option<Option<i16>>,
    /// Every ticket the plan is to have, in order; none leaves them.
    #[serde(default)]
    pub tickets: Option<Vec<DraftTicket>>,
    /// Tickets the plan has, each changed in place by its id: only what an
    /// edit says changes ([`plan::edited`]). Not with `tickets`.
    #[serde(default)]
    pub ticket_edits: Option<Vec<TicketEdit>>,
    /// What it may spend; `null` takes its budget off.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub budget: Option<Option<PlanBudget>>,
    /// How its goal is measured now.
    #[serde(default)]
    pub result: Option<GoalResult>,
    /// Why, in the boss's words.
    #[serde(default)]
    pub why: Option<String>,
}

/// What a change came to.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Changed {
    pub plan: PlanDetail,
    /// None: nothing changed.
    pub change: Option<Change>,
    /// Whether it applies now; a change of scope the boss made waits for
    /// the person.
    pub applied: bool,
    pub summary: String,
}

/// Why a plan's ticket didn't start.
#[derive(Debug, thiserror::Error)]
pub enum StartRefused {
    /// Nothing is wrong with the ticket: it moved, its plan was paused or
    /// ended, it is starting already, or Desk's breaker holds new starts.
    /// It is read again as it is.
    #[error("{0}")]
    Skipped(String),
    /// Its session couldn't start: a folder or an account gone, or
    /// anything else that keeps it from starting until someone acts.
    #[error("{0}")]
    Failed(DeskError),
    /// It waits on something under way that it needs, its repository being
    /// cloned: it starts once that is done, and nothing failed.
    #[error("{0}")]
    Waits(String),
}

impl From<DeskError> for StartRefused {
    fn from(e: DeskError) -> Self {
        Self::Failed(e)
    }
}

impl From<StartRefused> for DeskError {
    fn from(e: StartRefused) -> Self {
        match e {
            StartRefused::Failed(e) => e,
            StartRefused::Skipped(why) | StartRefused::Waits(why) => Self::Conflict(why),
        }
    }
}

/// A plan's ticket's session, started: on this computer (`machine` none),
/// or on one of the person's machines, by its id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub id: Uuid,
    pub machine: Option<String>,
    pub name: String,
    /// What its boss is told of how it started, when it isn't as asked: a
    /// machine's session started as the person's, not its boss's.
    pub note: Option<String>,
}

impl Started {
    /// Its sidebar item: `session:<id>`, or `session:<machine>/<id>`.
    pub fn item(&self) -> String {
        match &self.machine {
            Some(m) => format!("session:{m}/{}", self.id),
            None => super::session_item(self.id),
        }
    }
}

/// What a machine ticket's session is told besides its ticket: its board
/// is on the person's own computer, which its machine doesn't hold.
pub fn machine_note(key: &str, machine: &str) -> String {
    format!(
        "{key}'s board is on the person's own computer, not on {machine}: the ticket tool here \
         can't see it, so don't use it for {key}. Say plainly in your replies what you finished \
         and what is left; your boss moves the ticket."
    )
}

/// What dropping a plan does with the sessions working on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDrop {
    /// Stops them.
    Stop,
    /// Leaves them running, the person's own.
    Leave,
}

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

fn stale_plan(p: &Plan, boss: &str) -> DeskError {
    DeskError::Conflict(format!(
        "{boss} changed the plan; look at revision {}.",
        p.revision
    ))
}

fn problems(p: Vec<Problem>) -> DeskError {
    DeskError::Invalid(plan::plain(&p))
}

/// The tickets of `rows` that have started: past To do, or someone has
/// them, or a session is being started for them.
fn started_of(rows: &[PlanTicket]) -> HashSet<Uuid> {
    rows.iter()
        .filter(|t| {
            t.assigned || t.starting || !matches!(t.state, TicketState::Backlog | TicketState::Todo)
        })
        .map(|t| t.id)
        .collect()
}

/// The branch a ticket's own worktree is on.
fn branch_for(key: &str) -> String {
    crate::worktree::slug(key)
}

impl Desk {
    // ── Reading ─────────────────────────────────────────────────────────

    async fn plan_of(&self, id: Uuid) -> Result<Plan, DeskError> {
        self.0
            .store
            .plan(id)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)
    }

    /// What its boss is called, as the hub says now.
    fn plan_boss_name(&self, p: &Plan) -> String {
        self.plan_boss_named(&p.boss)
    }

    /// What the boss `boss` of a plan is called, as the hub says now.
    pub(super) fn plan_boss_named(&self, boss: &str) -> String {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(n) = crew
            .and_then(|c| c.agent_name(boss))
            .filter(|n| !n.trim().is_empty() && *n != boss)
        {
            return n;
        }
        let plans = self.0.boss_plans.lock();
        crew::shown_name(boss, plans.names.get(boss).map(String::as_str), true)
    }

    /// The engine `boss` starts a ticket on that names none: its default,
    /// as the hub says, else Claude Code.
    pub fn boss_engine(&self, boss: &str) -> Engine {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        crew.and_then(|c| c.default_engine(boss))
            .and_then(|e| Engine::parse(e.trim()).ok())
            .unwrap_or(Engine::Claude)
    }

    /// Why `p` is paused, in the person's words: what keeps a ticket of it
    /// from starting, as the runner said, or why it was paused.
    fn paused_why(&self, p: &Plan) -> Option<String> {
        let why = p.paused?;
        let said = match why {
            Paused::StartFailing => p.paused_why.clone(),
            Paused::Budget => self
                .plan_starts_held(p.group.as_deref(), p.id)
                .map(|(_, ask)| ask)
                .filter(|a| !a.is_empty()),
            Paused::BossHeld => self
                .agent_held(&p.boss)
                .filter(|a| !a.is_empty())
                .map(|a| format!("{} is held: {a}", self.plan_boss_name(p))),
            _ => None,
        };
        Some(said.unwrap_or_else(|| {
            let w = why.why();
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        }))
    }

    /// `plan`'s tickets as the runner and the card read them.
    pub async fn plan_tickets(&self, plan: Uuid) -> Result<Vec<PlanTicket>, DeskError> {
        let (tickets, runs, starting) = self.plan_rows(plan).await?;
        let ours: HashSet<Uuid> = tickets.iter().map(|t| t.id).collect();
        let views = self.views(tickets).await?;
        Ok(views
            .into_iter()
            .map(|v| {
                plan_ticket(
                    &v,
                    runs.get(&v.ticket.id),
                    starting.contains(&v.ticket.id),
                    &ours,
                )
            })
            .collect())
    }

    async fn plan_rows(
        &self,
        plan: Uuid,
    ) -> Result<(Vec<Ticket>, HashMap<Uuid, Runs>, HashSet<Uuid>), DeskError> {
        let tickets = self.0.store.plan_tickets(plan).await.map_err(internal)?;
        let runs: HashMap<Uuid, Runs> = self
            .0
            .store
            .plan_runs(plan)
            .await
            .map_err(internal)?
            .into_iter()
            .collect();
        let ids: HashSet<Uuid> = tickets.iter().map(|t| t.id).collect();
        let starting = self
            .0
            .store
            .starting()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|s| s.ticket)
            .filter(|t| ids.contains(t))
            .collect();
        Ok((tickets, runs, starting))
    }

    /// Whether a plan ticket's `session` is at work ([`SessionState::busy`]),
    /// as [`plan::holding`] reads it: one here by its state now, one on a
    /// machine (`machine/id`, whose state is read there) as at work.
    pub fn session_busy(&self, session: &str) -> bool {
        match Uuid::parse_str(session) {
            Ok(id) => self.view(id).is_ok_and(|v| v.state.busy()),
            Err(_) => true,
        }
    }

    async fn plan_summary(&self, p: Plan) -> Result<PlanSummary, DeskError> {
        let tickets = self.plan_tickets(p.id).await?;
        let budget = self.applied(&p).await?.budget;
        Ok(PlanSummary {
            boss_name: self.plan_boss_name(&p),
            group_name: p.group.as_deref().and_then(|g| self.group_name(g)),
            paused_why: self.paused_why(&p),
            progress: plan::progress(&tickets, |s| self.session_busy(s)),
            budget,
            held: self.plan_held(&p),
            limit: self.plan_limit(&p),
            plan: p,
        })
    }

    /// How many sessions `p` runs at once ([`PlanSummary::limit`]).
    fn plan_limit(&self, p: &Plan) -> Option<usize> {
        self.crew()?.boss_limit(p).map(|l| p.limit(l))
    }

    /// What holds `p`'s new starts now, while it is open.
    fn plan_held(&self, p: &Plan) -> Option<String> {
        if matches!(p.state, PlanState::Done | PlanState::Dropped) {
            return None;
        }
        self.plan_starts_held(p.group.as_deref(), p.id)
            .map(|(_, ask)| ask)
    }

    /// Every plan, newest first: those in `state`, and in `group` (`""`
    /// for the ungrouped), when said.
    pub async fn plans(
        &self,
        state: Option<PlanState>,
        group: Option<&str>,
    ) -> Result<Vec<PlanSummary>, DeskError> {
        let mut out = Vec::new();
        for p in self.0.store.plans(state).await.map_err(internal)? {
            let ours = match group {
                None => true,
                Some("") => p.group.is_none(),
                Some(g) => p.group.as_deref() == Some(g),
            };
            if ours {
                out.push(self.plan_summary(p).await?);
            }
        }
        Ok(out)
    }

    /// A plan in full.
    pub async fn plan(&self, id: Uuid) -> Result<PlanDetail, DeskError> {
        let p = self.plan_of(id).await?;
        self.plan_detail(p).await
    }

    async fn plan_detail(&self, p: Plan) -> Result<PlanDetail, DeskError> {
        let (tickets, runs, starting) = self.plan_rows(p.id).await?;
        let ours: HashSet<Uuid> = tickets.iter().map(|t| t.id).collect();
        let views = self.views(tickets).await?;
        let mut shown: Vec<PlanTicketView> = views
            .into_iter()
            .map(|v| {
                let r = runs.get(&v.ticket.id).cloned().unwrap_or_default();
                let held = v.ticket.session().and_then(|s| self.session_held(s));
                PlanTicketView {
                    held,
                    starting: starting.contains(&v.ticket.id),
                    engine: r.engine,
                    folder: r.folder,
                    view: r.view,
                    machine: r.machine,
                    model: r.model,
                    repo: r.repo,
                    adopted: r.adopted,
                    held_back: r.held_back,
                    session_closed: r.session_closed,
                    ticket: v,
                }
            })
            .collect();
        // In the order the plan lists them.
        let applied = self.applied(&p).await?;
        let order: HashMap<Uuid, usize> = applied
            .tickets
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i))
            .collect();
        shown.sort_by_key(|t| {
            order
                .get(&t.ticket.ticket.id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        let rows: Vec<PlanTicket> = shown
            .iter()
            .map(|t| plan_ticket(&t.ticket, runs.get(&t.ticket.ticket.id), t.starting, &ours))
            .collect();
        let revisions = self.0.store.revisions(p.id).await.map_err(internal)?;
        // Its budget as kept, once approved, counted now.
        let kept = self
            .budget_list()
            .into_iter()
            .filter(|b| b.scope == BudgetScope::Plan(p.id))
            .min_by_key(|b| b.updated_at);
        let spend = match kept {
            Some(b) => self.plan_standing(p.id, &b).await,
            None => None,
        };
        let (goal, facts) = match &p.result {
            Some(r) => {
                let facts = self.0.store.facts(&[p.id]).await.map_err(internal)?;
                let readings = self.0.store.readings(p.id).await.map_err(internal)?;
                (Some(goal::progress(r, &readings, &facts)), facts)
            }
            None => (None, Vec::new()),
        };
        Ok(PlanDetail {
            goal,
            facts,
            waves: plan::waves(&rows),
            summary: PlanSummary {
                boss_name: self.plan_boss_name(&p),
                group_name: p.group.as_deref().and_then(|g| self.group_name(g)),
                paused_why: self.paused_why(&p),
                progress: plan::progress(&rows, |s| self.session_busy(s)),
                budget: applied.budget.clone(),
                held: self.plan_held(&p),
                limit: self.plan_limit(&p),
                plan: p,
            },
            tickets: shown,
            revisions,
            spend,
        })
    }

    /// Whether a ticket on this computer may run in a folder: one there, or
    /// for a ticket of a repository, its checkout there or a folder Desk
    /// would clone it into as it starts.
    fn folder_fits(&self) -> impl Fn(&str, Option<&str>) -> Result<(), String> {
        let home = self.home_folder();
        move |f, repo| match repo.and_then(crate::repos::repo_of) {
            Some(r) => crate::repos::fits(&r, f, home.as_deref()),
            None => Path::new(f)
                .is_dir()
                .then_some(())
                .ok_or_else(|| format!("{f} isn't a folder")),
        }
    }

    /// The snapshot the plan's rows follow: its approved revision's, or
    /// its newest while none is.
    async fn applied(&self, p: &Plan) -> Result<Snapshot, DeskError> {
        let n = p.approved_revision.unwrap_or(p.revision);
        let mut s = self
            .0
            .store
            .revision(p.id, n)
            .await
            .map_err(internal)?
            .map(|r| r.snapshot)
            .unwrap_or_default();
        // What runs is the plan's own limit.
        s.parallel = p.parallel;
        Ok(s)
    }

    async fn newest(&self, p: &Plan) -> Result<Snapshot, DeskError> {
        Ok(self
            .0
            .store
            .revision(p.id, p.revision)
            .await
            .map_err(internal)?
            .map(|r| r.snapshot)
            .unwrap_or_default())
    }

    /// The plans still open (proposed, running or paused), newest first,
    /// as the Floor shows them in their groups' sections.
    pub async fn floor_plans(&self) -> Result<Vec<crate::floor::PlanProgress>, DeskError> {
        Ok(self
            .plans(None, None)
            .await?
            .into_iter()
            .filter(|s| !matches!(s.plan.state, PlanState::Done | PlanState::Dropped))
            .map(|s| crate::floor::PlanProgress {
                id: s.plan.id,
                goal: s.plan.goal,
                group: s.plan.group,
                boss: s.plan.boss,
                boss_name: s.boss_name,
                state: s.plan.state,
                paused_why: s.paused_why,
                progress: s.progress,
            })
            .collect())
    }

    /// The plans waiting on the person, as Floor needs: a plan to approve,
    /// or a change of scope to one.
    pub fn plan_needs(&self) -> Vec<Need> {
        let mut needs: Vec<Need> = self
            .0
            .boss_plans
            .lock()
            .awaiting
            .values()
            .cloned()
            .collect();
        needs.sort_by_key(|n| n.since);
        needs
    }

    /// Reads the plans waiting on the person: to approve, or to go on
    /// after their tickets kept failing to start, saying why again.
    pub(super) async fn plans_load(&self) {
        match self.0.store.plans(None).await {
            Ok(all) => {
                for p in all
                    .into_iter()
                    .filter(|p| p.awaiting() || p.paused == Some(Paused::StartFailing))
                {
                    self.plan_awaits(&p).await;
                }
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "couldn't read plans"),
        }
    }

    /// Keeps the Floor's need for `p` as it is now.
    async fn plan_awaits(&self, p: &Plan) {
        let failing = p.paused == Some(Paused::StartFailing);
        if !p.awaiting() && !failing {
            self.0.boss_plans.lock().awaiting.remove(&p.id);
            return;
        }
        let summary = self.0.store.revision(p.id, p.revision).await.ok().flatten();
        let boss = self.plan_boss_name(p);
        let (ask, why) = match p.approved_revision {
            _ if failing && !p.awaiting() => (
                format!(
                    "{boss}'s plan is paused: {}",
                    p.paused_why
                        .as_deref()
                        .unwrap_or("a ticket keeps failing to start")
                ),
                Some("Continue tries it again; or edit or drop the ticket first.".into()),
            ),
            None => (
                format!("Approve {boss}'s plan: {}", crew::one_line(&p.goal, 160)),
                summary.as_ref().map(|r| plan_why(&r.snapshot, p.parallel)),
            ),
            Some(_) => (
                format!(
                    "Plan change: {}",
                    summary
                        .as_ref()
                        .map_or_else(|| p.goal.clone(), |r| crew::one_line(&r.summary, 160))
                ),
                Some(format!(
                    "{boss}'s plan runs as you approved it until you say."
                )),
            ),
        };
        let need = Need {
            id: format!("plan:{}", p.id),
            kind: NeedKind::Approval,
            who: Who {
                kind: PartyKind::Manager,
                id: p.boss.clone(),
                name: boss,
                engine: None,
            },
            ask,
            why,
            options: None,
            chose: None,
            since: summary.map_or(p.updated_at, |r| r.at),
            place: Place::Dm,
            answer: Answer::Decide {
                at: DecideRef::Plan {
                    plan: p.id,
                    revision: p.revision,
                },
            },
            // Its plan's group, whoever its boss is now.
            group: p.group.clone(),
            boss: None,
        };
        self.0.boss_plans.lock().awaiting.insert(p.id, need);
    }

    /// Writes `p`, keeps its need, and announces it.
    async fn plan_saved(&self, p: &Plan) -> Result<PlanDetail, DeskError> {
        self.0.store.put_plan(p).await.map_err(internal)?;
        self.plan_heard(p).await
    }

    /// Keeps `p`'s need, as it is stored, and announces it.
    async fn plan_heard(&self, p: &Plan) -> Result<PlanDetail, DeskError> {
        self.plan_awaits(p).await;
        let detail = self.plan_detail(p.clone()).await?;
        self.announce(DeskEvent::Plan {
            plan: Box::new(detail.summary.clone()),
        });
        Ok(detail)
    }

    /// Announces `plan` as it is now: one of its tickets moved.
    pub async fn plan_touched(&self, plan: Uuid) {
        match self.plan_of(plan).await {
            Ok(p) => match self.plan_summary(p).await {
                Ok(s) => self.announce(DeskEvent::Plan { plan: Box::new(s) }),
                Err(e) => tracing::info!(%plan, error = %e, "a plan wasn't announced"),
            },
            Err(e) => tracing::info!(%plan, error = %e, "a plan wasn't read"),
        }
    }

    fn plan_note(&self, p: &Plan, by: &By, kind: Kind, text: String) {
        self.plan_note_why(p, by, kind, text, None);
    }

    /// [`Self::plan_note`], saying why it paused or went on, when that is
    /// what it is.
    fn plan_note_why(&self, p: &Plan, by: &By, kind: Kind, text: String, why: Option<Paused>) {
        if by.party.kind == PartyKind::Manager && by.party.id == p.boss {
            self.0
                .boss_plans
                .lock()
                .names
                .insert(p.boss.clone(), by.party.name.clone());
        }
        // Which plan, for whoever watches Desk's events live.
        let mut detail = serde_json::json!({ "plan": p.id.to_string() });
        if let Some(why) = why {
            detail["paused"] = why.as_str().into();
        }
        let mut e = by.event(kind, None, text).detail(detail);
        e.group = p.group.clone();
        self.note(e);
    }

    // ── Proposing ───────────────────────────────────────────────────────

    /// `boss` proposes `new`, as `by`: checked against what this computer
    /// can run, its tickets put on the board in Backlog, and the person
    /// asked to approve it. Nothing of it runs before they do.
    pub async fn propose_plan(
        &self,
        new: NewPlan,
        boss: &Party,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        self.named_group(new.group.as_deref())?;
        let mut draft = new.draft;
        let goal = draft.goal.clone();
        self.take_sessions_on(
            &mut draft.tickets,
            None,
            new.group.as_deref(),
            None,
            &goal,
            new.engine,
            by,
        )
        .await?;
        let taking = self
            .adoptable(&draft.tickets, None, new.group.as_deref(), None)
            .await?;
        let mut s = plan::snapshot_with(
            &draft,
            None,
            new.engine,
            &titles_of(&taking),
            &mut Uuid::new_v4,
        )
        .map_err(problems)?;
        keep_needs(&mut s, &taking, &HashMap::new());
        s.parallel = new.parallel;
        s.budget = new.budget;
        s.result = new.result;
        let goal_ticket = match new.goal_ticket {
            Some(id) if s.ticket(id).is_none() => {
                return Err(DeskError::Invalid(
                    "The goal's ticket is one of the plan's tickets.".into(),
                ))
            }
            Some(id) => Some(id),
            None => s
                .result
                .as_ref()
                .and_then(|_| s.tickets.iter().find(|t| t.adopted))
                .map(|t| t.id),
        };
        plan::validate(&s, &self.engines(), &self.folder_fits()).map_err(problems)?;
        if new.parallel.is_some_and(|p| !(1..=12).contains(&p)) {
            return Err(DeskError::Invalid(
                "A plan runs from 1 to 12 sessions at once.".into(),
            ));
        }
        // It shows on the board said, else on the board of the tickets it
        // takes on, else on the group's own; what it makes goes on a board
        // of Desk's own, never a synced one.
        let first_taken = s
            .tickets
            .iter()
            .find(|t| t.adopted)
            .and_then(|t| taking.get(&t.id))
            .map(|t| t.board);
        let b = match new.board.or(first_taken) {
            Some(id) => {
                let b = self.board_of(id).await?;
                if !self.in_scope(b.group.as_deref(), new.group.as_deref()) {
                    return Err(DeskError::Invalid(
                        "That board isn't in a group the plan's boss runs.".into(),
                    ));
                }
                b
            }
            None => self.plan_board(new.group.as_deref()).await?,
        };
        let makes = s.tickets.iter().any(|t| !t.adopted);
        let made_on = match makes {
            true => self.new_tickets_board(&b).await?,
            false => b.clone(),
        };
        let at = crew::now();
        let p = Plan {
            id: Uuid::new_v4(),
            board: b.id,
            group: new.group.clone(),
            boss: boss.id.clone(),
            goal: s.goal.clone(),
            state: PlanState::Proposed,
            paused: None,
            paused_why: None,
            parallel: new.parallel,
            revision: 1,
            approved_revision: None,
            created_at: at,
            updated_at: at,
            result: s.result.clone(),
            agreed: None,
            agreed_at: None,
            read_back_at: None,
            goal_ticket,
            met: None,
        };
        let taken = s.tickets.iter().filter(|t| t.adopted).count();
        let summary = match taken {
            0 => format!(
                "Proposed {} tickets for “{}”",
                s.tickets.len(),
                crew::one_line(&s.goal, 120)
            ),
            n => format!(
                "Proposed {} tickets ({n} taken on from the board) for “{}”",
                s.tickets.len(),
                crew::one_line(&s.goal, 120)
            ),
        };
        // The plan, its tickets and its first revision, in one write.
        let mut rank = self
            .end_rank(made_on.id, TicketState::Backlog, Uuid::nil())
            .await?;
        let mut rows: Vec<(Ticket, Runs)> = Vec::with_capacity(s.tickets.len());
        let mut adopt: Vec<Claim> = Vec::new();
        for t in &s.tickets {
            let runs = Runs {
                plan: Some(p.id),
                engine: Some(t.engine),
                folder: Some(t.folder.clone()),
                view: t.view,
                machine: t.machine.clone(),
                model: t.model.clone(),
                repo: t.repo.clone(),
                adopted: t.adopted,
                held_back: None,
                session_closed: false,
            };
            if t.adopted {
                // Only the needs it doesn't have already.
                let had = taking.get(&t.id).map(|r| r.needs.as_slice()).unwrap_or(&[]);
                let more = t
                    .needs
                    .iter()
                    .filter(|n| !had.contains(n))
                    .copied()
                    .collect();
                adopt.push(Claim {
                    ticket: t.id,
                    runs,
                    needs: more,
                    session: t.session,
                });
                continue;
            }
            rows.push((
                Ticket {
                    id: t.id,
                    board: made_on.id,
                    number: 0,
                    title: t.title.clone(),
                    body: t.body.clone(),
                    state: TicketState::Backlog,
                    rank,
                    assignee: None,
                    needs: t.needs.clone(),
                    links: vec![],
                    external: None,
                    created_by: by.party.clone(),
                    pinned: false,
                    snoozed_until: None,
                    upstream_rank: None,
                    snoozed_at: None,
                    state_at: at,
                    created_at: at,
                    updated_at: at,
                    version: 0,
                },
                runs,
            ));
            rank += RANK_STEP;
        }
        let r = Revision {
            plan: p.id,
            revision: 1,
            at,
            by: by.party.clone(),
            change: Change::Scope,
            summary,
            snapshot: s.clone(),
        };
        if let Err(e) = self.0.store.add_plan(&p, &mut rows, &adopt, &r).await {
            return Err(self.adopt_failed(e, None).await);
        }
        for (t, _) in rows {
            if let Ok(v) = self.view_ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        for c in &adopt {
            if let Ok(v) = self.ticket(c.ticket).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        self.plan_note(
            &p,
            by,
            Kind::Planned,
            format!("proposed a plan: {}", crew::one_line(&p.goal, 120)),
        );
        self.plan_saved(&p).await
    }

    // ── Taking tickets on ───────────────────────────────────────────────

    /// The tickets of `draft` that take one on and `before` doesn't have,
    /// each as it is now: on a board of `group`, in Backlog or To do, with
    /// no one on it and in no other open plan than `plan`. One that isn't
    /// is refused in words the boss reads, naming it by its key.
    async fn adoptable(
        &self,
        draft: &[DraftTicket],
        before: Option<&Snapshot>,
        group: Option<&str>,
        plan: Option<Uuid>,
    ) -> Result<HashMap<Uuid, Ticket>, DeskError> {
        let mut out = HashMap::new();
        for (i, d) in draft.iter().enumerate() {
            let Some(id) = d.adopt else {
                continue;
            };
            if before.is_some_and(|b| b.ticket(id).is_some()) || out.contains_key(&id) {
                continue;
            }
            let outside = || {
                DeskError::Invalid(format!(
                    "Ticket {} takes on a ticket that isn't on a board of a group the plan's boss runs.",
                    i + 1
                ))
            };
            let t = match self.ticket_of(id).await {
                Ok(t) => t,
                Err(DeskError::NotFound) => return Err(outside()),
                Err(e) => return Err(e),
            };
            if !self.in_scope(self.board_of(t.board).await?.group.as_deref(), group) {
                return Err(outside());
            }
            let why = match d.session {
                Some(session) => self.held_from_session(&t, session, plan).await?,
                None => self.held_why(&t, plan).await?,
            };
            if let Some(why) = why {
                return Err(DeskError::Conflict(why));
            }
            out.insert(id, t);
        }
        Ok(out)
    }

    /// Makes each ticket of `draft` that names a running session take that
    /// session's ticket on: the one it has that isn't settled, else one
    /// made for it now on the board of `group`, named by its goal and
    /// assigned to it in Doing. Its folder and engine are the session's
    /// when it says neither. One `before` has already is left as it is.
    /// A ticket is made for a session only once the plan, `goal` with
    /// these tickets and `engine` their default, checks out: a plan
    /// refused leaves nothing behind on the board.
    #[allow(clippy::too_many_arguments)]
    async fn take_sessions_on(
        &self,
        draft: &mut [DraftTicket],
        before: Option<&Snapshot>,
        group: Option<&str>,
        plan: Option<Uuid>,
        goal: &str,
        engine: Engine,
        by: &By,
    ) -> Result<(), DeskError> {
        let mut makes = Vec::new();
        for (i, d) in draft.iter_mut().enumerate() {
            let Some(session) = d.session else {
                continue;
            };
            if d.adopt.is_some() || d.id.is_some() {
                return Err(DeskError::Invalid(format!(
                    "Ticket {} names a session: give the session alone.",
                    i + 1
                )));
            }
            let view = self.view(session).map_err(|_| {
                DeskError::Invalid(format!("Ticket {} names a session there isn't.", i + 1))
            })?;
            if view.state == SessionState::Closed {
                return Err(DeskError::Conflict(format!(
                    "{} has ended: a plan takes on only a session that runs.",
                    view.label()
                )));
            }
            let had = before.and_then(|b| b.tickets.iter().find(|t| t.session == Some(session)));
            match had {
                Some(t) => d.adopt = Some(t.id),
                None => match self.open_session_ticket(view.id).await? {
                    Some(t) => d.adopt = Some(t),
                    None => makes.push((i, view.clone())),
                },
            }
            if d.folder.trim().is_empty() && d.repo.is_none() {
                d.folder.clone_from(&view.cwd);
            }
            if d.engine.is_none() {
                d.engine = Some(view.engine.as_str().to_string());
            }
        }
        if makes.is_empty() {
            return Ok(());
        }
        // Checked first as if each were a new ticket of the plan's.
        let mut dry = draft.to_vec();
        for (i, view) in &makes {
            dry[*i].session = None;
            dry[*i].title = session_title(view);
        }
        let taking = self.adoptable(&dry, before, group, plan).await?;
        let s = plan::snapshot_with(
            &Draft {
                goal: goal.to_string(),
                tickets: dry,
            },
            before,
            engine,
            &titles_of(&taking),
            &mut Uuid::new_v4,
        )
        .map_err(problems)?;
        plan::validate(&s, &self.engines(), &self.folder_fits()).map_err(problems)?;
        for (i, view) in makes {
            draft[i].adopt = Some(self.session_ticket(&view, group, by).await?);
        }
        Ok(())
    }

    /// The ticket session `session` works on, not yet settled, if it has
    /// one.
    async fn open_session_ticket(&self, session: Uuid) -> Result<Option<Uuid>, DeskError> {
        Ok(self
            .0
            .store
            .session_tickets(session)
            .await
            .map_err(internal)?
            .into_iter()
            .find(|t| !matches!(t.state, TicketState::Done | TicketState::Dropped))
            .map(|t| t.id))
    }

    /// The ticket session `view` works on, not yet settled: the one given
    /// to it, else one made for it now on `group`'s board, named by its
    /// goal and given to it in Doing.
    async fn session_ticket(
        &self,
        view: &SessionView,
        group: Option<&str>,
        by: &By,
    ) -> Result<Uuid, DeskError> {
        if let Some(t) = self.open_session_ticket(view.id).await? {
            return Ok(t);
        }
        let shown = self.plan_board(group).await?;
        let b = self.new_tickets_board(&shown).await?;
        let made = self
            .create_ticket(
                super::board::NewTicket {
                    board: b.id,
                    title: session_title(view),
                    body: String::new(),
                    state: Some(TicketState::Todo),
                    needs: Vec::new(),
                },
                by,
            )
            .await?;
        let key = made.ticket.key(&b).unwrap_or_else(|| "a ticket".into());
        let t = self.ticket_of(made.ticket.id).await?;
        self.assign_session(t, &key, view.clone(), by, false)
            .await
            .map(|v| v.ticket.id)
    }

    /// Why `t` can't be taken on for the running `session` it stands for:
    /// it isn't that session's, or another open plan than `plan` has it.
    async fn held_from_session(
        &self,
        t: &Ticket,
        session: Uuid,
        plan: Option<Uuid>,
    ) -> Result<Option<String>, DeskError> {
        let key = self.key_of(t).await?;
        let its = t
            .assignee
            .as_ref()
            .is_some_and(|a| a.kind == PartyKind::Session && a.id == session.to_string());
        if !its {
            return Ok(Some(format!(
                "{key} isn't that session's: take the session on by itself, or {key} with adopt."
            )));
        }
        self.other_plan_why(t, &key, plan).await
    }

    /// Why `t` can't be taken on by a plan (`plan`, when one has it
    /// already): someone has it, it started, or another open plan has it.
    async fn held_why(&self, t: &Ticket, plan: Option<Uuid>) -> Result<Option<String>, DeskError> {
        let key = self.key_of(t).await?;
        if let Some(a) = &t.assignee {
            let who = match a.kind {
                PartyKind::You => "the person's".to_string(),
                _ => format!("{}'s", a.name),
            };
            return Ok(Some(format!(
                "{key} is {who} already: a plan never takes a ticket someone has. Leave it out, or ask the person to let it go."
            )));
        }
        if !matches!(t.state, TicketState::Backlog | TicketState::Todo) {
            return Ok(Some(format!(
                "{key} is in {} already: a plan takes on only tickets that haven't started (Backlog or To do).",
                t.state.title()
            )));
        }
        if let Some(why) = self.other_plan_why(t, &key, plan).await? {
            return Ok(Some(why));
        }
        if self
            .0
            .store
            .starting_of(t.id)
            .await
            .map_err(internal)?
            .is_some()
        {
            return Ok(Some(format!("{key} is starting a session now.")));
        }
        Ok(None)
    }

    /// Why `t` (`key`) can't be taken on because an open plan other than
    /// `plan` has it.
    async fn other_plan_why(
        &self,
        t: &Ticket,
        key: &str,
        plan: Option<Uuid>,
    ) -> Result<Option<String>, DeskError> {
        let Some(other) = self
            .0
            .store
            .plan_of_ticket(t.id)
            .await
            .map_err(internal)?
            .filter(|o| Some(*o) != plan)
        else {
            return Ok(None);
        };
        Ok(self
            .0
            .store
            .plan(other)
            .await
            .map_err(internal)?
            .filter(|o| !matches!(o.state, PlanState::Done | PlanState::Dropped))
            .map(|o| {
                format!(
                    "{key} is in another plan already (“{}”): a ticket is in one plan at a time.",
                    crew::one_line(&o.goal, 80)
                )
            }))
    }

    /// A claim that failed, in the boss's words: the ticket taken meanwhile
    /// is read again to say why.
    async fn adopt_failed(&self, e: Adopt, plan: Option<Uuid>) -> DeskError {
        match e {
            Adopt::Held(id) => match self.ticket_of(id).await {
                Ok(t) => match self.held_why(&t, plan).await {
                    Ok(Some(why)) => DeskError::Conflict(why),
                    Ok(None) => DeskError::Conflict(format!(
                        "{} changed as the plan took it on; read it again.",
                        self.key_of(&t).await.unwrap_or_else(|_| "A ticket".into())
                    )),
                    Err(e) => e,
                },
                Err(DeskError::NotFound) => {
                    DeskError::Conflict("A ticket it takes on is gone from its board.".into())
                }
                Err(e) => e,
            },
            Adopt::Other(e) => internal(e),
        }
    }

    /// The board a plan on `b` makes its new tickets on: `b`, or, when `b`
    /// is synced, its group's own board of Desk's (a plan's words never
    /// become a client's issues).
    async fn new_tickets_board(&self, b: &Board) -> Result<Board, DeskError> {
        match b.sync {
            None => Ok(b.clone()),
            Some(_) => self.plan_board(b.group.as_deref()).await,
        }
    }

    /// Lets `t`, a ticket plan `p` took on, go where it is: its sessions
    /// stopped or left running as the person's own (`on`), or, dropped from
    /// a plan that goes on (`None`), taken off whoever the plan gave it to;
    /// it is the plan's no longer, and the plan's own needs between it and
    /// the plan's other tickets go. It is never moved, so its issue says
    /// what it said.
    async fn plan_let_go(
        &self,
        p: &Plan,
        t: Ticket,
        on: Option<OnDrop>,
        by: &By,
    ) -> Result<(), DeskError> {
        // One that isn't the plan's now (let go, or someone else's) is
        // left alone: its sessions are someone else's to stop.
        if self.0.store.plan_of_ticket(t.id).await.map_err(internal)? != Some(p.id) {
            return Ok(());
        }
        let sessions: Vec<Uuid> = t
            .links
            .iter()
            .filter_map(|l| match l {
                Link::Session { session } => Some(*session),
                _ => None,
            })
            .filter(|s| self.view(*s).is_ok_and(|v| v.state != SessionState::Closed))
            .collect();
        let held_by_plan = t
            .assignee
            .as_ref()
            .and_then(crate::line::session_of)
            .is_some_and(|s| sessions.contains(&s));
        let id = t.id;
        match on {
            Some(OnDrop::Leave) => {
                for s in sessions {
                    if self.charge_of(s).is_some_and(|c| c.live()) {
                        if let Err(e) = self.release(s, by).await {
                            tracing::info!(session = %s, error = %e, "a dropped plan's session wasn't let go");
                        }
                    }
                }
            }
            Some(OnDrop::Stop) | None => {
                if on.is_some() {
                    for s in &sessions {
                        if let Err(e) = self.stop(*s, Some(by.clone())) {
                            tracing::info!(session = %s, error = %e, "a dropped plan's session wasn't stopped");
                        }
                    }
                }
                if held_by_plan {
                    let key = self.key_of(&t).await?;
                    if on.is_none() {
                        if let Some(session) = t.assignee.as_ref().and_then(crate::line::session_of)
                        {
                            let text = format!(
                                "{key} left the plan; it is no longer yours. Stop working on it unless you are asked again."
                            );
                            self.tell_session(session, text, by).await;
                        }
                    }
                    self.unassign(t, &key, by).await?;
                }
            }
        }
        // Only the needs the plan added go; one the person or the tracker
        // had stays, as does one its issue has there now too.
        let mut unneeded = Vec::new();
        for (a, b) in self
            .0
            .store
            .plan_added_needs(p.id, id)
            .await
            .map_err(internal)?
        {
            let upstream = match (self.ticket_of(a).await, self.ticket_of(b).await) {
                (Ok(from), Ok(to)) => match (&from.external, &to.external) {
                    (Some(f), Some(t)) => f.seen.needs.contains(&t.key),
                    _ => false,
                },
                _ => false,
            };
            if !upstream {
                unneeded.push((a, b));
            }
        }
        self.0
            .store
            .release_ticket(p.id, id, &unneeded)
            .await
            .map_err(internal)?;
        let touched = unneeded.iter().map(|(a, _)| *a).chain([id]);
        for t in touched.collect::<HashSet<_>>() {
            if let Ok(v) = self.ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        Ok(())
    }

    /// The board a plan in `group` goes on: the group's first, or one made
    /// for it.
    pub(super) async fn plan_board(&self, group: Option<&str>) -> Result<Board, DeskError> {
        let boards = self.boards().await?;
        // Never a synced board: a plan's words don't become a client's issues.
        if let Some(b) = boards
            .into_iter()
            .find(|b| b.group.as_deref() == group && b.sync.is_none())
        {
            return Ok(b);
        }
        let name = group
            .and_then(|g| {
                self.layout()
                    .groups
                    .into_iter()
                    .find(|x| x.id == g)
                    .map(|x| x.name)
            })
            .unwrap_or_else(|| "Desk".into());
        self.create_board(NewBoard {
            name,
            group: group.map(str::to_string),
            prefix: None,
        })
        .await
    }

    /// A board of Desk's own in `group` for a boss's work: the one with
    /// `prefix` when it is there already (in `group`, not synced), else
    /// one made, named `name`. The prefix comes from the name when none is
    /// said. A prefix another board has, elsewhere, is refused in plain
    /// words; nothing of that board is said.
    pub async fn board_in_group(
        &self,
        group: Option<&str>,
        name: &str,
        prefix: Option<&str>,
    ) -> Result<(Board, bool), DeskError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DeskError::Invalid("A board needs a name.".into()));
        }
        let prefix = match prefix.map(|p| p.trim().to_ascii_uppercase()) {
            Some(p) if !p.is_empty() => {
                board::check_prefix(&p).map_err(DeskError::Invalid)?;
                p
            }
            _ => board::prefix_of(name),
        };
        // Its own with that prefix, as it is; another's is taken.
        let taken = || {
            DeskError::Conflict(format!(
                "The prefix {prefix} is another board's; pass a different prefix in new_board."
            ))
        };
        let had = async || -> Result<Option<Board>, DeskError> {
            match self
                .0
                .store
                .board_by_prefix(&prefix)
                .await
                .map_err(internal)?
            {
                Some(b) if b.group.as_deref() == group && b.sync.is_none() => Ok(Some(b)),
                Some(_) => Err(taken()),
                None => Ok(None),
            }
        };
        if let Some(b) = had().await? {
            return Ok((b, false));
        }
        match self
            .create_board(NewBoard {
                name: name.to_string(),
                group: group.map(str::to_string),
                prefix: Some(prefix.clone()),
            })
            .await
        {
            Ok(b) => Ok((b, true)),
            // Made meanwhile by another call: its own, or taken.
            Err(DeskError::Conflict(_)) => had().await?.map(|b| (b, false)).ok_or_else(taken),
            Err(e) => Err(e),
        }
    }

    /// Re-files plan `id` on board `board`, as `by`: every ticket of it
    /// moves there under a new key, still in the plan, with what it needs,
    /// its links, history and whoever has it. The board is one of Desk's
    /// own, in a group the plan's boss runs; nothing moves while a ticket
    /// of it is starting.
    pub async fn move_plan(&self, id: Uuid, board: Uuid, by: &By) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        if p.board == board {
            return self.plan_detail(p).await;
        }
        let to = self.board_of(board).await?;
        if !self.in_scope(to.group.as_deref(), p.group.as_deref()) {
            return Err(DeskError::Invalid(format!(
                "{} isn't in a group the plan's boss runs: name a board of one, or pass new_board to make one there.",
                to.name
            )));
        }
        if let Some(sync) = &to.sync {
            return Err(DeskError::Invalid(format!(
                "{} is synced with {} {}: a plan's tickets go on a board of Desk's own; name another, or pass new_board.",
                to.name,
                sync.system.title(),
                sync.target
            )));
        }
        let _one = self.0.plan_starting.lock().await;
        let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
        // One it took on stays on its own board, and isn't moved.
        let taken: HashSet<Uuid> = self
            .0
            .store
            .plan_runs(p.id)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|(_, r)| r.adopted)
            .map(|(id, _)| id)
            .collect();
        let rows: Vec<Ticket> = rows
            .into_iter()
            .filter(|t| !taken.contains(&t.id))
            .collect();
        // One filed in another tracker keeps its place there: moving it
        // would cut its link.
        if let Some(t) = rows.iter().find(|t| t.external.is_some()) {
            return Err(DeskError::Invalid(format!(
                "“{}” of the plan is filed in another tracker, so the plan stays where it is; \
                 move that ticket off the plan first, or leave the plan on its board.",
                crew::one_line(&t.title, 80)
            )));
        }
        let mine: HashSet<Uuid> = rows.iter().map(|t| t.id).collect();
        let starting = self.0.store.starting().await.map_err(internal)?;
        if starting.iter().any(|s| mine.contains(&s.ticket)) {
            return Err(DeskError::Conflict(
                "A ticket of the plan is starting its session now; try the move again in a moment."
                    .into(),
            ));
        }
        let from = self.board_of(p.board).await?;
        p.board = to.id;
        p.updated_at = crew::now();
        let moved = self
            .0
            .store
            .move_plan_board(&p, to.id)
            .await
            .map_err(internal)?;
        let n = moved.len();
        for t in moved {
            if let Ok(v) = self.view_ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        self.plan_note(
            &p,
            by,
            Kind::PlanChanged,
            format!(
                "moved a plan's {n} {} from {} to {}: {}",
                if n == 1 { "ticket" } else { "tickets" },
                from.name,
                to.name,
                crew::one_line(&p.goal, 120)
            ),
        );
        self.plan_saved(&p).await
    }

    /// Moves plan `id` to `group` (none: Ungrouped), as `by`: the group its
    /// work belongs to, where the sessions it starts from now on start. Its
    /// sessions still in its old group move there too; its tickets stay on
    /// their boards, under their keys. Only to a group its boss runs (as
    /// its group's scope, when its boss isn't one now).
    pub async fn regroup_plan(
        &self,
        id: Uuid,
        group: Option<String>,
        by: &By,
    ) -> Result<Regrouped, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let _one = self.0.plan_starting.lock().await;
        let mut p = self.plan_of(id).await?;
        let name = self.named_group(group.as_deref())?;
        if matches!(p.state, PlanState::Done | PlanState::Dropped) {
            return Err(DeskError::Invalid(
                "A finished plan stays where it was.".into(),
            ));
        }
        if p.group == group {
            return Ok(Regrouped {
                plan: self.plan_detail(p).await?,
                unmoved: None,
            });
        }
        let bossed = self.bossed();
        let theirs = scope::scope_of(group.as_deref(), &bossed);
        let ours = self
            .scope_of_boss(&p.boss)
            .unwrap_or_else(|| scope::scope_of(p.group.as_deref(), &bossed));
        if ours != theirs {
            return Err(DeskError::Conflict(
                match self.boss_of(&theirs) {
                    Some(b) => format!(
                        "{name} is {}'s to run, not this plan's boss's: a plan moves only between groups its own boss runs.",
                        b.name
                    ),
                    None => format!(
                        "No boss runs {name}: a plan moves only between groups its own boss runs."
                    ),
                },
            ));
        }
        let layout = self.layout();
        let moving: Vec<String> = self
            .0
            .store
            .plan_tickets(p.id)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|t| matches!(t.state, TicketState::Doing | TicketState::Review))
            .filter_map(|t| t.assignee.filter(|a| a.kind == PartyKind::Session))
            .map(|a| format!("session:{}", a.id))
            .filter(|item| scope::group_of(item, &layout) == p.group.as_deref())
            .collect();
        p.group = group;
        p.updated_at = crew::now();
        self.plan_note(
            &p,
            by,
            Kind::PlanChanged,
            format!("moved the plan to {name}: {}", crew::one_line(&p.goal, 120)),
        );
        let plan = self.plan_saved(&p).await?;
        let to = p.group.as_deref().unwrap_or(super::UNGROUPED);
        let unmoved = match moving.is_empty() {
            true => None,
            false => self
                .change_layout(|l| {
                    moving
                        .iter()
                        .try_fold(l.clone(), |l, item| l.put(item, to, None))
                })
                .await
                .err()
                .map(|e| {
                    tracing::warn!(plan = %p.id, error = %e, "a plan's sessions weren't moved to its group");
                    let said = format!("Its running sessions stayed where they were: {e}");
                    self.plan_note(
                        &p,
                        &By::desk("Moving its sessions failed."),
                        Kind::PlanChanged,
                        format!("{said} ({})", crew::one_line(&p.goal, 120)),
                    );
                    said
                }),
        };
        Ok(Regrouped { plan, unmoved })
    }

    /// What the person calls `group` (none: Ungrouped), when the sidebar
    /// has it.
    fn named_group(&self, group: Option<&str>) -> Result<String, DeskError> {
        match group {
            None => Ok(super::UNGROUPED_NAME.into()),
            Some(g) => self
                .group_name(g)
                .ok_or_else(|| DeskError::Invalid("The sidebar has no such group.".into())),
        }
    }

    /// Whether rows of groups `a` and `b` are the same boss's.
    fn in_scope(&self, a: Option<&str>, b: Option<&str>) -> bool {
        let bossed = self.bossed();
        scope::scope_of(a, &bossed) == scope::scope_of(b, &bossed)
    }

    /// The group plan `p`'s calls to the person's machines go under: its
    /// group, when that is a boundary; none in the main overseer's part,
    /// whose plans run in grants of their own there.
    pub fn relay_group<'a>(&self, p: &'a Plan) -> Option<&'a str> {
        self.live_group(p).filter(|g| self.bossed().contains(*g))
    }

    /// Plan `p`'s group while the sidebar has it; none (Ungrouped) once it
    /// is deleted.
    pub fn live_group<'a>(&self, p: &'a Plan) -> Option<&'a str> {
        p.group
            .as_deref()
            .filter(|g| self.layout().groups.iter().any(|x| x.id == *g))
    }

    /// The group `tickets`' work belongs to among the groups `sees` lets
    /// a boss use ([`crate::plan_group::pick`]), by this computer's open
    /// sessions.
    pub fn work_group(
        &self,
        tickets: &[DraftTicket],
        sees: impl Fn(Option<&str>) -> bool,
    ) -> Option<crate::plan_group::Picked> {
        use crate::plan_group::{pick, Choice, Work, Working};
        let layout = self.layout();
        let groups: Vec<Choice> = layout
            .groups
            .iter()
            .filter(|g| sees(Some(&g.id)))
            .map(|g| Choice {
                id: &g.id,
                name: &g.name,
            })
            .collect();
        let views = self.list();
        let working: Vec<Working> = views
            .iter()
            .filter(|v| v.state != crate::model::SessionState::Closed)
            .filter_map(|v| {
                v.group.as_ref().map(|g| Working {
                    cwd: &v.cwd,
                    group: &g.id,
                })
            })
            .collect();
        let work: Vec<Work> = tickets
            .iter()
            .map(|t| Work {
                folder: &t.folder,
                repo: t.repo.as_deref(),
                session: t
                    .session
                    .and_then(|s| views.iter().find(|v| v.id == s))
                    .and_then(|v| v.group.as_ref())
                    .map(|g| g.id.as_str()),
            })
            .collect();
        let home = std::env::var("HOME").unwrap_or_default();
        pick(&work, &working, &groups, &home)
    }

    /// Announces each of `plans` as it is stored now: their group was
    /// deleted, so they are ungrouped.
    pub(super) async fn plans_ungrouped(&self, plans: &[Uuid]) {
        for id in plans {
            let heard = match self.plan_of(*id).await {
                Ok(p) => self.plan_heard(&p).await.map(drop),
                Err(e) => Err(e),
            };
            if let Err(e) = heard {
                tracing::warn!(plan = %id, error = %e, "an ungrouped plan wasn't announced");
            }
        }
    }

    async fn add_revision(
        &self,
        p: &Plan,
        n: i32,
        by: &By,
        change: Change,
        summary: &str,
        s: &Snapshot,
    ) -> Result<(), DeskError> {
        let r = Revision {
            plan: p.id,
            revision: n,
            at: crew::now(),
            by: by.party.clone(),
            change,
            summary: summary.to_string(),
            snapshot: s.clone(),
        };
        match self.0.store.add_revision(&r).await.map_err(internal)? {
            true => Ok(()),
            false => Err(stale_plan(p, &self.plan_boss_name(p))),
        }
    }

    /// Makes `p`'s tickets what `after` says, from what `before` said: new
    /// ones made in `column`; dropped ones deleted while the plan was never
    /// approved, else dropped; the rest renamed, re-pointed and reordered.
    async fn plan_rows_follow(
        &self,
        p: &Plan,
        before: &Snapshot,
        after: &Snapshot,
        column: TicketState,
        by: &By,
    ) -> Result<(), DeskError> {
        let shown = self.board_of(p.board).await?;
        let makes = after.added(before).iter().any(|t| !t.adopted);
        let b = match makes {
            true => self.new_tickets_board(&shown).await?,
            false => shown,
        };
        // Every one taken on is claimed before anything is written: one
        // someone took meanwhile refuses the change whole.
        self.claim_taken_on(p, before, after).await?;
        let mut rank = self.end_rank(b.id, column, Uuid::nil()).await?;
        for t in after.added(before) {
            // One taken on is queued where it is once the plan runs: what
            // it says is its board's.
            if t.adopted {
                let row = self.ticket_of(t.id).await?;
                if column == TicketState::Todo && row.state == TicketState::Backlog {
                    let mv = Move {
                        state: TicketState::Todo,
                        before: None,
                        after: None,
                    };
                    self.move_read(row, mv, by).await?;
                }
                continue;
            }
            let at = crew::now();
            let mut row = Ticket {
                id: t.id,
                board: b.id,
                number: 0,
                title: t.title.clone(),
                body: t.body.clone(),
                state: column,
                rank,
                assignee: None,
                needs: vec![],
                links: vec![],
                external: None,
                created_by: by.party.clone(),
                pinned: false,
                snoozed_until: None,
                upstream_rank: None,
                snoozed_at: None,
                state_at: at,
                created_at: at,
                updated_at: at,
                version: 0,
            };
            rank += RANK_STEP;
            self.0.store.add_ticket(&mut row).await.map_err(internal)?;
            self.put_runs(p, t, None).await?;
        }
        for id in after.dropped(before) {
            let Ok(t) = self.ticket_of(id).await else {
                continue;
            };
            // One taken on goes back where it is, never deleted or dropped.
            if before.ticket(id).is_some_and(|w| w.adopted) {
                self.plan_let_go(p, t, None, by).await?;
                continue;
            }
            match p.approved_revision {
                None => {
                    self.0.store.delete_ticket(id).await.map_err(internal)?;
                    self.announce(DeskEvent::TicketRemoved { id });
                }
                Some(_) if matches!(t.state, TicketState::Done | TicketState::Dropped) => {}
                Some(_) => {
                    let mv = Move {
                        state: TicketState::Dropped,
                        before: None,
                        after: None,
                    };
                    self.move_read(t, mv, by).await?;
                }
            }
        }
        // What every kept ticket says, needs and runs in.
        let ours: HashSet<Uuid> = after.tickets.iter().map(|t| t.id).collect();
        for t in &after.tickets {
            // What one taken on says is its board's: never written here.
            let was = before.ticket(t.id).filter(|_| !t.adopted);
            let mut row = self.ticket_of(t.id).await?;
            let mut changed = false;
            if was.is_some_and(|w| w.title != t.title) {
                row.title.clone_from(&t.title);
                changed = true;
            }
            if was.is_some_and(|w| w.body != t.body) {
                row.body.clone_from(&t.body);
                changed = true;
            }
            if changed {
                row.updated_at = crew::now();
                self.save_ticket(&mut row).await?;
            }
            if let Some(w) = before.ticket(t.id).filter(|w| {
                (w.engine, &w.folder, w.view, &w.machine, &w.model, &w.repo)
                    != (t.engine, &t.folder, t.view, &t.machine, &t.model, &t.repo)
            }) {
                self.put_runs(p, t, Some(w)).await?;
            }
            let want: HashSet<Uuid> = t
                .needs
                .iter()
                .copied()
                .filter(|n| ours.contains(n))
                .collect();
            let had: HashSet<Uuid> = row
                .needs
                .iter()
                .copied()
                .filter(|n| ours.contains(n))
                .collect();
            for n in want.difference(&had) {
                self.0
                    .store
                    .put_plan_need(t.id, *n, p.id)
                    .await
                    .map_err(internal)?;
            }
            for n in had.difference(&want) {
                self.0.store.delete_need(t.id, *n).await.map_err(internal)?;
            }
        }
        self.plan_reorder(p, after).await?;
        for t in &after.tickets {
            if let Ok(v) = self.ticket(t.id).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        Ok(())
    }

    /// Claims for `p` every ticket `after` takes on that `before` didn't,
    /// all or none: when one can't be, those claimed here go again and it
    /// is refused, saying why. One already `p`'s is claimed again as it is.
    async fn claim_taken_on(
        &self,
        p: &Plan,
        before: &Snapshot,
        after: &Snapshot,
    ) -> Result<Vec<Uuid>, DeskError> {
        let mut claimed = Vec::new();
        for t in after.added(before).into_iter().filter(|t| t.adopted) {
            let runs = Runs {
                plan: Some(p.id),
                engine: Some(t.engine),
                folder: Some(t.folder.clone()),
                view: t.view,
                machine: t.machine.clone(),
                model: t.model.clone(),
                repo: t.repo.clone(),
                adopted: true,
                held_back: None,
                session_closed: false,
            };
            if let Err(e) = self.0.store.adopt_ticket(t.id, &runs, t.session).await {
                self.unclaim(p, &claimed).await;
                return Err(self.adopt_failed(e, Some(p.id)).await);
            }
            claimed.push(t.id);
        }
        Ok(claimed)
    }

    /// Lets the tickets `ids` that [`Self::claim_taken_on`] claimed for `p`
    /// go again, as they were: nothing else was written of them.
    async fn unclaim(&self, p: &Plan, ids: &[Uuid]) {
        for id in ids {
            if let Err(e) = self.0.store.release_ticket(p.id, *id, &[]).await {
                tracing::warn!(ticket = %id, error = %e, "a ticket claimed for a refused plan change wasn't let go");
            }
        }
    }

    /// Writes where `t` of `p` runs, `was` before: a folder said that the
    /// plan now leaves for its repository to say goes, so the next start
    /// finds or clones its checkout afresh.
    async fn put_runs(
        &self,
        p: &Plan,
        t: &Planned,
        was: Option<&Planned>,
    ) -> Result<(), DeskError> {
        self.0
            .store
            .put_runs(
                t.id,
                &Runs {
                    plan: Some(p.id),
                    engine: Some(t.engine),
                    folder: Some(t.folder.clone()),
                    view: t.view,
                    machine: t.machine.clone(),
                    model: t.model.clone(),
                    repo: t.repo.clone(),
                    adopted: t.adopted,
                    held_back: None,
                    session_closed: false,
                },
                was.is_none_or(|w| w.folder.trim().is_empty()),
            )
            .await
            .map_err(internal)
    }

    /// The tickets not started yet, in the order `s` lists them: what
    /// starts first.
    async fn plan_reorder(&self, p: &Plan, s: &Snapshot) -> Result<(), DeskError> {
        let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
        for column in [TicketState::Backlog, TicketState::Todo] {
            // One taken on keeps its place on its board: its order there is
            // the person's, or its tracker's.
            let mut queued: Vec<&Ticket> = rows
                .iter()
                .filter(|t| t.state == column)
                .filter(|t| !s.ticket(t.id).is_some_and(|x| x.adopted))
                .collect();
            if queued.len() < 2 {
                continue;
            }
            let place: HashMap<Uuid, usize> = s
                .tickets
                .iter()
                .enumerate()
                .map(|(i, t)| (t.id, i))
                .collect();
            let mut ranks: Vec<f64> = queued.iter().map(|t| t.rank).collect();
            ranks.sort_by(f64::total_cmp);
            ranks.dedup();
            if ranks.len() < queued.len() {
                let base = ranks.first().copied().unwrap_or(RANK_STEP);
                ranks = (0..queued.len())
                    .map(|i| base + i as f64 * RANK_STEP)
                    .collect();
            }
            queued.sort_by_key(|t| place.get(&t.id).copied().unwrap_or(usize::MAX));
            for (t, rank) in queued.into_iter().zip(ranks) {
                if t.rank != rank {
                    let mut row = t.clone();
                    row.rank = rank;
                    self.save_ticket(&mut row).await?;
                }
            }
        }
        Ok(())
    }

    // ── Changing ────────────────────────────────────────────────────────

    /// The tickets of `p` that have started ([`started_of`]).
    async fn started(&self, p: &Plan) -> Result<HashSet<Uuid>, DeskError> {
        Ok(started_of(&self.plan_tickets(p.id).await?))
    }

    /// Changes plan `id` as `by` asks. The person's change applies at once;
    /// the boss's applies at once within what was approved, while a change
    /// of scope it makes waits for the person (as every change it makes
    /// does at `suggest`, `asks`). Before the plan is approved every change
    /// applies to its tickets, which wait in Backlog for the approval.
    pub async fn change_plan(
        &self,
        id: Uuid,
        edit: PlanEdit,
        engine: Engine,
        asks: bool,
        by: &By,
    ) -> Result<Changed, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        if matches!(p.state, PlanState::Done | PlanState::Dropped) {
            return Err(DeskError::Conflict(format!(
                "That plan is {}.",
                p.state.as_str()
            )));
        }
        let boss = self.plan_boss_name(&p);
        if edit.revision.is_some_and(|r| r != p.revision) {
            return Err(stale_plan(&p, &boss));
        }
        // The person's edit never approves, by the way, a change the boss is
        // waiting on them for.
        if by.party.kind == PartyKind::You && p.approved_revision.is_some_and(|a| a != p.revision) {
            return Err(DeskError::Conflict(format!(
                "Approve or reject {boss}'s change first."
            )));
        }
        if let Some(Some(n)) = edit.parallel {
            if !(1..=12).contains(&n) {
                return Err(DeskError::Invalid(
                    "A plan runs from 1 to 12 sessions at once.".into(),
                ));
            }
        }
        let newest = self.newest(&p).await?;
        let applied = self.applied(&p).await?;
        let rows = self.plan_tickets(p.id).await?;
        let started = started_of(&rows);
        let mut next = match (&edit.tickets, &edit.ticket_edits) {
            (Some(_), Some(_)) => {
                return Err(DeskError::Invalid(
                    "Say every ticket the plan is to have, or the ones to change, not both.".into(),
                ))
            }
            (None, Some(edits)) => {
                // Where a started ticket runs is refused by its key, before
                // anything else is read of the edit.
                let moved: Vec<Problem> = edits
                    .iter()
                    .filter(|e| started.contains(&e.id))
                    .filter(|e| {
                        newest
                            .ticket(e.id)
                            .map_or(e.moves(), |was| e.moves_from(was))
                    })
                    .filter_map(|e| {
                        let t = rows.iter().find(|t| t.id == e.id)?;
                        let column = match (t.state, t.starting) {
                            (TicketState::Backlog | TicketState::Todo, true) => "Doing, starting",
                            (s, _) => s.title(),
                        };
                        Some(Problem::StartedEdit {
                            key: e
                                .key
                                .clone()
                                .or_else(|| t.key.clone())
                                .unwrap_or_else(|| e.name()),
                            column,
                        })
                    })
                    .collect();
                if !moved.is_empty() {
                    return Err(DeskError::Conflict(plan::plain(&moved)));
                }
                plan::edited(&newest, edits).map_err(problems)?
            }
            (Some(tickets), None) => {
                let mut tickets = tickets.clone();
                let goal = edit.goal.clone().unwrap_or_else(|| newest.goal.clone());
                self.take_sessions_on(
                    &mut tickets,
                    Some(&newest),
                    p.group.as_deref(),
                    Some(p.id),
                    &goal,
                    engine,
                    by,
                )
                .await?;
                let taking = self
                    .adoptable(&tickets, Some(&newest), p.group.as_deref(), Some(p.id))
                    .await?;
                let mut next = plan::snapshot_with(
                    &Draft {
                        goal: edit.goal.clone().unwrap_or_else(|| newest.goal.clone()),
                        tickets,
                    },
                    Some(&newest),
                    engine,
                    &titles_of(&taking),
                    &mut Uuid::new_v4,
                )
                .map_err(problems)?;
                if !taking.is_empty() {
                    let mut rows = HashMap::new();
                    for t in next.tickets.iter().filter(|t| t.adopted) {
                        if !taking.contains_key(&t.id) {
                            rows.insert(t.id, self.ticket_of(t.id).await?);
                        }
                    }
                    keep_needs(&mut next, &taking, &rows);
                }
                next
            }
            (None, None) => newest.clone(),
        };
        if let Some(goal) = &edit.goal {
            next.goal = goal.trim().to_string();
        }
        if let Some(par) = edit.parallel {
            next.parallel = par;
        }
        if let Some(b) = edit.budget {
            next.budget = b;
        }
        if let Some(r) = edit.result {
            next.result = Some(r);
        }
        plan::validate(&next, &self.engines(), &self.folder_fits()).map_err(problems)?;
        // The person's agreement covers the boss's work toward its result,
        // past a Suggest level, where every change asks.
        let covered = (p.covered() && !asks).then_some(plan::Covered {
            goal_ticket: p.goal_ticket,
        });
        let change = plan::plan_change(&applied, &next, &started, covered.as_ref())
            .map_err(|e| DeskError::Conflict(e.to_string()))?;
        if change.is_none() && next == newest {
            let detail = self.plan_detail(p).await?;
            return Ok(Changed {
                plan: detail,
                change: None,
                applied: true,
                summary: "No change".into(),
            });
        }
        let mut summary = plan::describe(&applied, &next);
        if let Some(why) = edit.why.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
            summary = format!("{summary} ({})", crew::one_line(why, 160));
        }
        let kind = change.unwrap_or(Change::Minor);
        let person = by.party.kind == PartyKind::You;
        // An unbound boss's change applies as the person's would.
        let unbound = by.party.kind == PartyKind::Manager
            && !crate::grant::is_served(&by.party.id)
            && self.full_of(&by.party.id).unbound;
        let waits =
            p.approved_revision.is_some() && !person && !unbound && (asks || kind == Change::Scope);
        let n = p.revision + 1;
        let from = match p.approved_revision {
            None => newest,
            Some(_) => applied,
        };
        // What it takes on is claimed before the revision is written, so a
        // ticket taken meanwhile refuses it with nothing written.
        let claimed = match waits {
            true => vec![],
            false => self.claim_taken_on(&p, &from, &next).await?,
        };
        if let Err(e) = self.add_revision(&p, n, by, kind, &summary, &next).await {
            self.unclaim(&p, &claimed).await;
            return Err(e);
        }
        p.revision = n;
        if !waits {
            let column = match p.approved_revision {
                None => TicketState::Backlog,
                Some(_) => TicketState::Todo,
            };
            if let Err(e) = self.plan_rows_follow(&p, &from, &next, column, by).await {
                self.unclaim(&p, &claimed).await;
                return Err(e);
            }
            if p.approved_revision.is_some() {
                p.approved_revision = Some(n);
                // What it may spend is what applies now.
                self.keep_plan_budget(&p, next.budget.as_ref(), by).await?;
            }
            p.parallel = next.parallel;
            follow_goal(&mut p, &next);
        }
        p.updated_at = crew::now();
        let text = match waits {
            true => format!("proposed a change to its plan: {summary}"),
            false => format!("changed the plan: {summary}"),
        };
        self.plan_note(&p, by, Kind::PlanChanged, text);
        let detail = self.plan_saved(&p).await?;
        Ok(Changed {
            plan: detail,
            change: Some(kind),
            applied: !waits,
            summary,
        })
    }

    /// The person approves plan `id` as of `revision`, its newest: its
    /// tickets go to To do and the runner starts them. A newer revision
    /// than the one they read is refused, so what they approve is what
    /// they saw.
    pub async fn approve_plan(
        &self,
        id: Uuid,
        revision: i32,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        self.approve_plan_with(id, revision, None, by).await
    }

    /// [`Self::approve_plan`], with what it may spend as the person says
    /// on approving (`budget`: `None` keeps what the revision says;
    /// `Some(None)` takes it off). A budget other than the revision's is a
    /// revision of the person's first, and that is what they approve.
    pub async fn approve_plan_with(
        &self,
        id: Uuid,
        revision: i32,
        budget: Option<Option<PlanBudget>>,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        self.approve(id, revision, budget, None, by).await
    }

    /// Plan `id`'s boss approves it as of `revision`, its newest, on the
    /// person's own words agreeing it (`said`, at `at`), which the hub
    /// found in their messages: they are kept as what agreed its goal,
    /// unless other words did first.
    pub async fn approve_agreed(
        &self,
        id: Uuid,
        revision: i32,
        said: &str,
        at: DateTime<Utc>,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        self.approve(id, revision, None, Some((said, at)), by).await
    }

    async fn approve(
        &self,
        id: Uuid,
        revision: i32,
        budget: Option<Option<PlanBudget>>,
        agreed: Option<(&str, DateTime<Utc>)>,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        // An unbound boss approves its own plans: the person let it. The
        // hub asks this for one only on the person's own words asking for
        // the work to start (`crew.plan`'s `start_now`); a plan it only
        // proposes stays a draft. Any boss approves on the person's words
        // agreeing it.
        let unbound = by.party.kind == PartyKind::Manager
            && !crate::grant::is_served(&by.party.id)
            && self.full_of(&by.party.id).unbound;
        if by.party.kind != PartyKind::You && !unbound && agreed.is_none() {
            return Err(DeskError::Invalid(
                "Only the person approves a plan.".into(),
            ));
        }
        let mut p = self.plan_of(id).await?;
        let agrees = p.agreed.is_none() && agreed.is_some();
        if let (true, Some((said, at))) = (agrees, agreed) {
            p.agreed = Some(crew::clip(said.trim(), 300));
            p.agreed_at = Some(at);
        }
        if matches!(p.state, PlanState::Done | PlanState::Dropped) {
            return Err(DeskError::Conflict(format!(
                "That plan is {}.",
                p.state.as_str()
            )));
        }
        if revision != p.revision {
            return Err(stale_plan(&p, &self.plan_boss_name(&p)));
        }
        if p.approved_revision == Some(revision) && budget.is_none() {
            return match agrees {
                true => self.plan_saved(&p).await,
                false => self.plan_detail(p).await,
            };
        }
        let mut next = self.newest(&p).await?;
        // A change it approves claims what it takes on before anything is
        // written: one taken meanwhile refuses it as it was.
        let claimed = match p.approved_revision {
            Some(_) => {
                let applied = self.applied(&p).await?;
                self.claim_taken_on(&p, &applied, &next).await?
            }
            None => vec![],
        };
        // Whatever stops it after, what it claimed goes again: the plan as
        // approved never had them.
        // True once there is something new to save; the save is outside, so
        // one that fails after it committed never lets them go.
        let done: Result<bool, DeskError> = async {
            let mut revision = revision;
            if let Some(b) = budget.filter(|b| *b != next.budget) {
                let before = next.clone();
                next.budget = b;
                if let Some(why) = next.budget.as_ref().and_then(PlanBudget::problem) {
                    return Err(DeskError::Invalid(why));
                }
                let summary = plan::describe(&before, &next);
                let applied = self.applied(&p).await?;
                let started = self.started(&p).await?;
                let kind = plan::plan_change(&applied, &next, &started, None)
                    .map_err(|e| DeskError::Conflict(e.to_string()))?
                    .unwrap_or(Change::Minor);
                revision = p.revision + 1;
                self.add_revision(&p, revision, by, kind, &summary, &next)
                    .await?;
                p.revision = revision;
            }
            if p.approved_revision == Some(revision) {
                return Ok(false);
            }
            match p.approved_revision {
                // Its tickets are what it says: they go to To do.
                None => {
                    let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
                    let place: HashMap<Uuid, usize> = next
                        .tickets
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (t.id, i))
                        .collect();
                    let mut queued: Vec<Ticket> = rows
                        .into_iter()
                        .filter(|t| t.state == TicketState::Backlog)
                        .collect();
                    queued.sort_by_key(|t| place.get(&t.id).copied().unwrap_or(usize::MAX));
                    for mut t in queued {
                        // One taken on moves as any move of the person's would,
                        // so its issue moves too.
                        if next.ticket(t.id).is_some_and(|x| x.adopted) {
                            let mv = Move {
                                state: TicketState::Todo,
                                before: None,
                                after: None,
                            };
                            self.move_read(t, mv, by).await?;
                            continue;
                        }
                        let at = crew::now();
                        self.to_column(&mut t, TicketState::Todo, at).await?;
                        self.save_ticket(&mut t).await?;
                        if let Ok(v) = self.view_ticket(t).await {
                            self.announce(DeskEvent::Ticket {
                                ticket: Box::new(v),
                            });
                        }
                    }
                    p.state = PlanState::Running;
                }
                // A change of scope: it applies now.
                Some(_) => {
                    let applied = self.applied(&p).await?;
                    let started = self.started(&p).await?;
                    plan::plan_change(&applied, &next, &started, None)
                        .map_err(|e| DeskError::Conflict(e.to_string()))?;
                    self.plan_rows_follow(&p, &applied, &next, TicketState::Todo, by)
                        .await?;
                    follow_goal(&mut p, &next);
                }
            }
            p.approved_revision = Some(revision);
            p.parallel = next.parallel;
            p.updated_at = crew::now();
            // What it may spend is what was approved.
            self.keep_plan_budget(&p, next.budget.as_ref(), by).await?;
            let text = match (revision, &next.budget) {
                (1, None) => format!("approved the plan: {}", crew::one_line(&p.goal, 120)),
                (1, Some(b)) => format!(
                    "approved the plan, capped at {}: {}",
                    b.words(),
                    crew::one_line(&p.goal, 120)
                ),
                (n, _) => format!("approved revision {n} of the plan"),
            };
            self.plan_note(&p, by, Kind::PlanApproved, text);
            Ok(true)
        }
        .await;
        match done {
            Err(e) => {
                self.unclaim(&p, &claimed).await;
                Err(e)
            }
            Ok(false) => self.plan_detail(p).await,
            Ok(true) => {
                let detail = self.plan_saved(&p).await?;
                if by.party.kind == PartyKind::You {
                    self.asks_approved(&detail.summary.plan).await;
                }
                Ok(detail)
            }
        }
    }

    /// The boss's questions about plan `p`'s goal that its revision the
    /// person just approved on its card holds: settled by that, so none
    /// stays open in their list or applies again.
    async fn asks_approved(&self, p: &Plan) {
        let asks = match self.0.store.goal_asks(p.id).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(plan = %p.id, error = %e, "a plan's goal questions weren't read on its approval");
                return;
            }
        };
        for a in asks
            .into_iter()
            .filter(|a| goal::approved_covers(&a.proposal, &p.goal, p.result.as_ref()))
        {
            if let Err(e) = self
                .0
                .store
                .ask_settled(a.item, "You approved it on the plan's card.")
                .await
            {
                tracing::warn!(plan = %p.id, error = %e, "a goal question wasn't settled on the plan's approval");
            }
        }
    }

    /// Pauses plan `id` for `why`: it starts nothing until it is resumed.
    /// What runs keeps running.
    pub async fn pause_plan(
        &self,
        id: Uuid,
        why: Paused,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        self.pause_plan_saying(id, why, None, by).await
    }

    /// [`Self::pause_plan`], `said` saying why in the person's words: a
    /// pause for the same reason saying something new keeps the newer.
    async fn pause_plan_saying(
        &self,
        id: Uuid,
        why: Paused,
        said: Option<String>,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        if !matches!(p.state, PlanState::Running | PlanState::Paused) {
            return Err(DeskError::Conflict(
                "Only a running plan can be paused.".into(),
            ));
        }
        // A pause of the person's stays theirs: Desk's own reasons never
        // replace it.
        if p.paused == Some(Paused::Person) && why != Paused::Person {
            return self.plan_detail(p).await;
        }
        // Paused so already: only a newer reason is kept, said once.
        if p.paused == Some(why) {
            return match p.paused_why == said {
                true => self.plan_detail(p).await,
                false => {
                    p.paused_why = said;
                    p.updated_at = crew::now();
                    self.plan_saved(&p).await
                }
            };
        }
        p.state = PlanState::Paused;
        p.paused = Some(why);
        p.paused_why = said;
        p.updated_at = crew::now();
        self.plan_note_why(
            &p,
            by,
            Kind::PlanPaused,
            format!("paused the plan: {}", why.why()),
            Some(why),
        );
        self.plan_saved(&p).await
    }

    /// Plan `id` passes to the boss `to`: its boss moved to another scope.
    /// A running plan pauses until the person has read its handover note,
    /// so nothing runs unsupervised meanwhile.
    pub async fn pass_plan(&self, id: Uuid, to: &Party, by: &By) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        p.boss = to.id.clone();
        if p.state == PlanState::Running {
            p.state = PlanState::Paused;
            p.paused = Some(Paused::NoBoss);
            p.paused_why = None;
        }
        p.updated_at = crew::now();
        self.plan_note(
            &p,
            by,
            Kind::HandedOver,
            format!("passed the plan to {}", to.name),
        );
        self.plan_saved(&p).await
    }

    /// Resumes plan `id`, paused for `why`: a pause for another reason
    /// stays.
    pub async fn resume_plan(
        &self,
        id: Uuid,
        why: &[Paused],
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let p = self.plan_of(id).await?;
        self.resume_read(p, why, by).await
    }

    /// Resumes plan `id` if Desk paused it because its boss was held or the
    /// breaker held its new starts, and that is over now: read again with
    /// the plan's writes held, so a hold come back meanwhile keeps it
    /// paused. `None` when it stays paused.
    pub async fn resume_if_lifted(
        &self,
        id: Uuid,
        by: &By,
    ) -> Result<Option<PlanDetail>, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let p = self.plan_of(id).await?;
        let lifted = match p.paused {
            Some(Paused::BossHeld) => self.agent_held(&p.boss).is_none(),
            Some(Paused::Budget) => self.plan_starts_held(p.group.as_deref(), p.id).is_none(),
            _ => false,
        };
        if !lifted {
            return Ok(None);
        }
        self.resume_read(p, &[Paused::BossHeld, Paused::Budget], by)
            .await
            .map(Some)
    }

    async fn resume_read(
        &self,
        mut p: Plan,
        why: &[Paused],
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let Some(was) = p.paused.filter(|w| why.contains(w)) else {
            return match p.paused {
                Some(other) => Err(DeskError::Conflict(format!(
                    "The plan is paused because {}.",
                    other.why()
                ))),
                None => self.plan_detail(p).await,
            };
        };
        p.state = PlanState::Running;
        p.paused = None;
        p.paused_why = None;
        p.updated_at = crew::now();
        self.plan_note_why(
            &p,
            by,
            Kind::Continued,
            format!("resumed the plan, paused because {}", was.why()),
            Some(was),
        );
        self.plan_saved(&p).await
    }

    /// Pauses plan `id` because one of its tickets keeps failing to start,
    /// `why` saying so in the person's words, as Desk with that reason;
    /// they continue it from the Floor.
    pub async fn pause_plan_failing(&self, id: Uuid, why: &str) -> Result<PlanDetail, DeskError> {
        let why = crew::one_line(why, 300);
        let by = By::desk(&why);
        self.pause_plan_saying(id, Paused::StartFailing, Some(why), &by)
            .await
    }

    /// The person keeps plan `id` as approved and turns down the boss's
    /// change waiting on them (`revision`): a new revision, as approved.
    pub async fn reject_plan(
        &self,
        id: Uuid,
        revision: i32,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        if by.party.kind != PartyKind::You {
            return Err(DeskError::Invalid(
                "Only the person turns down a plan's change.".into(),
            ));
        }
        let mut p = self.plan_of(id).await?;
        if revision != p.revision {
            return Err(stale_plan(&p, &self.plan_boss_name(&p)));
        }
        if p.approved_revision.is_none_or(|a| a == p.revision) {
            return Err(DeskError::Conflict(
                "No change waits on you; edit or drop the plan instead.".into(),
            ));
        }
        let applied = self.applied(&p).await?;
        let turned_down = self
            .0
            .store
            .revision(p.id, p.revision)
            .await
            .map_err(internal)?
            .map(|r| r.summary)
            .unwrap_or_default();
        let summary = format!(
            "Kept as approved, not: {}",
            crew::one_line(&turned_down, 200)
        );
        let n = p.revision + 1;
        self.add_revision(&p, n, by, Change::Minor, &summary, &applied)
            .await?;
        // A ticket the change took on that an approval left claimed goes
        // back as it was: the plan as approved never had it.
        let newest = self.newest(&p).await?;
        let gone: Vec<Uuid> = newest
            .added(&applied)
            .into_iter()
            .filter(|t| t.adopted)
            .map(|t| t.id)
            .collect();
        self.unclaim(&p, &gone).await;
        p.revision = n;
        p.approved_revision = Some(n);
        p.updated_at = crew::now();
        self.plan_note(
            &p,
            by,
            Kind::PlanChanged,
            format!(
                "turned down the change to the plan: {}",
                crew::one_line(&turned_down, 160)
            ),
        );
        self.plan_saved(&p).await
    }

    /// The person's answer to plan `id`'s Floor need as of `revision`:
    /// Allow approves what waits (or continues a plan paused for a ticket
    /// failing to start); Deny turns down a change of a running plan, and
    /// leaves one never approved as proposed, for them to open.
    pub async fn decide_plan(
        &self,
        id: Uuid,
        revision: i32,
        allow: bool,
        budget: Option<Option<PlanBudget>>,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let p = self.plan_of(id).await?;
        if revision != p.revision {
            return Err(stale_plan(&p, &self.plan_boss_name(&p)));
        }
        if budget.is_some() && !(allow && p.awaiting()) {
            return Err(DeskError::Invalid(
                "A budget goes with an approval; change a running plan's on the plan.".into(),
            ));
        }
        match (p.awaiting(), allow, p.approved_revision) {
            (true, true, _) => self.approve_plan_with(id, revision, budget, by).await,
            (true, false, Some(_)) => self.reject_plan(id, revision, by).await,
            (true, false, None) => self.plan_detail(p).await,
            (false, true, _) if p.paused == Some(Paused::StartFailing) => {
                self.resume_plan(id, &[Paused::StartFailing], by).await
            }
            (false, false, _) if p.paused == Some(Paused::StartFailing) => {
                self.plan_detail(p).await
            }
            (false, _, _) => Err(DeskError::Conflict("Already answered.".into())),
        }
    }

    /// Drops plan `id`: its tickets not started are dropped; the sessions
    /// working on the rest are stopped, or left running as the person's
    /// own (`on`).
    pub async fn drop_plan(&self, id: Uuid, on: OnDrop, by: &By) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        // No start is under way while it drops: one begun finished first.
        let _one = self.0.plan_starting.lock().await;
        let mut p = self.plan_of(id).await?;
        if p.state == PlanState::Dropped {
            return self.plan_detail(p).await;
        }
        let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
        // A start a stop cut off never starts now.
        let ours: HashSet<Uuid> = rows.iter().map(|t| t.id).collect();
        for s in self.0.store.starting().await.map_err(internal)? {
            if ours.contains(&s.ticket) {
                self.end_start_of(&s).await;
            }
        }
        let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
        let taken: HashSet<Uuid> = self
            .0
            .store
            .plan_runs(p.id)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|(_, r)| r.adopted)
            .map(|(id, _)| id)
            .collect();
        for t in rows {
            // One taken on goes back where it is, whatever its column: one
            // done stays the plan's record.
            if taken.contains(&t.id) {
                if t.state != TicketState::Done {
                    self.plan_let_go(&p, t, Some(on), by).await?;
                }
                continue;
            }
            let sessions: Vec<Uuid> = t
                .links
                .iter()
                .filter_map(|l| match l {
                    Link::Session { session } => Some(*session),
                    _ => None,
                })
                .filter(|s| self.view(*s).is_ok_and(|v| v.state != SessionState::Closed))
                .collect();
            let working = matches!(t.state, TicketState::Doing | TicketState::Review);
            match (on, working) {
                (_, false) if matches!(t.state, TicketState::Done | TicketState::Dropped) => {}
                (OnDrop::Leave, true) => {
                    for s in sessions {
                        if self.charge_of(s).is_some_and(|c| c.live()) {
                            if let Err(e) = self.release(s, by).await {
                                tracing::info!(session = %s, error = %e, "a dropped plan's session wasn't let go");
                            }
                        }
                    }
                }
                (OnDrop::Stop, true) | (_, false) => {
                    for s in sessions {
                        if let Err(e) = self.stop(s, Some(by.clone())) {
                            tracing::info!(session = %s, error = %e, "a dropped plan's session wasn't stopped");
                        }
                    }
                    let mv = Move {
                        state: TicketState::Dropped,
                        before: None,
                        after: None,
                    };
                    self.move_read(t, mv, by).await?;
                }
            }
        }
        p.state = PlanState::Dropped;
        p.paused = None;
        p.paused_why = None;
        p.updated_at = crew::now();
        self.plan_budget_ends(&p, by).await;
        self.plan_note(
            &p,
            by,
            Kind::PlanDropped,
            format!("dropped the plan: {}", crew::one_line(&p.goal, 120)),
        );
        self.plan_saved(&p).await
    }

    /// Plan `id` is done: every ticket of it is Done or Dropped.
    pub async fn finish_plan(&self, id: Uuid, by: &By) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        if p.state == PlanState::Done {
            return self.plan_detail(p).await;
        }
        let tickets = self.plan_tickets(id).await?;
        if !p.goal_done() {
            return Err(DeskError::Conflict(
                "Its goal isn't met yet: only a met reading or the person's own words close it."
                    .into(),
            ));
        }
        if p.met.is_none() && !plan::settled(&tickets) {
            return Err(DeskError::Conflict(
                "Some of the plan's tickets aren't done yet.".into(),
            ));
        }
        p.state = PlanState::Done;
        p.paused = None;
        p.paused_why = None;
        p.updated_at = crew::now();
        self.plan_budget_ends(&p, by).await;
        self.plan_note(
            &p,
            by,
            Kind::PlanDone,
            format!("finished the plan: {}", crew::one_line(&p.goal, 120)),
        );
        self.plan_saved(&p).await
    }

    /// Plan `id`'s goal is met as `met` says: kept once, and the ticket
    /// standing for it moved to Done by Desk and its plan's watches ended
    /// without a word, as well as it can be: a step that fails is logged
    /// and leaves the goal met. False when it was met
    /// already, and nothing else is done.
    pub async fn goal_met(&self, id: Uuid, met: &Met) -> Result<bool, DeskError> {
        if !self.0.store.mark_met(id, met).await.map_err(internal)? {
            return Ok(false);
        }
        if let Err(e) = self.goal_ticket_done(id, met).await {
            tracing::warn!(plan = %id, error = %e, "a met goal's ticket wasn't moved to Done");
        }
        // Nothing is waited on for it now: its closing turn says so.
        if let Err(e) = self.0.store.watches_end_plan(id, "its goal is met").await {
            tracing::warn!(plan = %id, error = %e, "a met goal's watches weren't ended");
        }
        self.plan_touched(id).await;
        Ok(true)
    }

    /// Moves met plan `id`'s goal ticket to Done, unless it is settled.
    async fn goal_ticket_done(&self, id: Uuid, met: &Met) -> Result<(), DeskError> {
        let p = self.plan_of(id).await?;
        let Some(ticket) = p.goal_ticket else {
            return Ok(());
        };
        let t = self.ticket_of(ticket).await?;
        if matches!(t.state, TicketState::Done | TicketState::Dropped) {
            return Ok(());
        }
        let by = By::desk(&format!(
            "The goal is met: {}",
            crew::one_line(&met.why, 200)
        ));
        let mv = Move {
            state: TicketState::Done,
            before: None,
            after: None,
        };
        self.move_read(t, mv, &by).await.map(|_| ())
    }

    // ── Starting ────────────────────────────────────────────────────────

    /// Starts `start`'s session for plan `p`'s ticket, run by `boss`, as
    /// `by`: the ticket moves to Doing with a `starting` link naming the
    /// session in one write, then that session starts, in the plan's group
    /// and in the boss's charge before it runs, and is linked to the
    /// ticket. It starts in what the ticket's row says then, whatever `p`
    /// and `start` were read as. A ticket the person moved since it was
    /// read isn't started, and nothing starts once the plan is paused,
    /// dropped or done, or while Desk's breaker holds new starts in its
    /// group.
    pub async fn plan_start(
        &self,
        p: &Plan,
        start: &Start,
        boss: &Party,
        by: &By,
    ) -> Result<Started, StartRefused> {
        if start.repo.is_some() {
            self.plan_prepare(p, start.ticket, boss).await?;
        }
        let _one = self.0.plan_starting.lock().await;
        let p = &self.plan_of(p.id).await?;
        if let Some((_, ask)) = self.plan_starts_held(p.group.as_deref(), p.id) {
            return Err(StartRefused::Skipped(format!(
                "Desk's breaker holds new sessions: {ask} The person can say Continue on Desk's Floor."
            )));
        }
        let mut t = self.ticket_of(start.ticket).await?;
        if t.state != TicketState::Todo || t.assignee.is_some() {
            return Err(StartRefused::Skipped(
                "That ticket isn't waiting to start any more.".into(),
            ));
        }
        if self
            .0
            .store
            .starting_of(t.id)
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(StartRefused::Skipped(
                "That ticket is starting already.".into(),
            ));
        }
        let session = Uuid::new_v4();
        let at = crew::now();
        let rank = self.end_rank(t.board, TicketState::Doing, t.id).await?;
        let runs = self
            .0
            .store
            .begin_start(&mut t, p.id, rank, session, at)
            .await
            .map_err(|e| match e {
                BeginStart::Stale => StartRefused::Skipped(
                    "That ticket changed as it was started; it is read again.".into(),
                ),
                BeginStart::NotRunning => StartRefused::Skipped(
                    "That plan was paused or ended as its ticket was started.".into(),
                ),
                BeginStart::Other(e) => StartRefused::Failed(DeskError::Internal(e)),
            })?;
        self.plan_launch(p, t, session, at, false, &runs, start.worktree, boss, by)
            .await
    }

    /// Readies `ticket`, a plan's ticket of a repository, to start: its
    /// folder where it runs is that repository's checkout, found or cloned
    /// first, and kept on it while the ticket is as it was read. Asked
    /// before a start's lock, so a slow machine holds no other start; a
    /// clone under way is [`StartRefused::Waits`], and starts nothing.
    pub async fn plan_prepare(
        &self,
        p: &Plan,
        ticket: Uuid,
        boss: &Party,
    ) -> Result<(), StartRefused> {
        let t = self.ticket_of(ticket).await?;
        if t.state != TicketState::Todo || t.assignee.is_some() {
            return Err(StartRefused::Skipped(
                "That ticket isn't waiting to start any more.".into(),
            ));
        }
        self.plan_checkout(p, &t, boss).await.map(drop)
    }

    /// The checkout of `t`'s repository where it runs, as its folder: on
    /// its machine through the relay (`desk.checkout`), on this computer
    /// here. Whether its folder changed (it is kept on it); a clone under
    /// way is [`StartRefused::Waits`].
    async fn plan_checkout(
        &self,
        p: &Plan,
        t: &Ticket,
        boss: &Party,
    ) -> Result<bool, StartRefused> {
        let rows = self.0.store.plan_runs(p.id).await.map_err(internal)?;
        let Some(runs) = rows.into_iter().find(|(id, _)| *id == t.id).map(|(_, r)| r) else {
            return Ok(false);
        };
        let Some(repo) = runs.repo.as_deref().and_then(crate::repos::repo_of) else {
            return Ok(false);
        };
        let key = self.key_of(t).await?;
        let folder = runs.folder.clone().filter(|f| !f.trim().is_empty());
        // A folder the plan says is the person's: used, or refused. One a
        // start found (the plan leaves it for its repository to say) is
        // found again each start, so one gone since is never kept.
        let chosen = self
            .applied(p)
            .await
            .ok()
            .and_then(|s| s.ticket(t.id).map(|w| !w.folder.trim().is_empty()))
            .unwrap_or(true);
        let found = folder.clone().filter(|_| !chosen);
        let folder = folder.filter(|_| chosen);
        let got = match runs.machine.as_deref() {
            None => {
                let d = self.clone();
                let (r, f) = (repo.clone(), folder.clone());
                tokio::task::spawn_blocking(move || d.repo_checkout(&r, f.as_deref()))
                    .await
                    .map_err(|e| internal(eyre::eyre!("looking for a checkout: {e}")))?
            }
            Some(machine) => {
                let crew = self.plan_crew().ok_or_else(|| {
                    StartRefused::Failed(DeskError::Invalid(format!(
                        "{machine} can't be reached from this hub."
                    )))
                })?;
                let body = serde_json::json!({ "remote": repo.key(), "folder": folder });
                let answer = crew
                    .machine_call(machine, self.relay_group(p), &boss.id, "desk.checkout", body)
                    .await
                    .map_err(|e| {
                        StartRefused::Failed(DeskError::Invalid(match e {
                            crew::MachineRefused::Refused(why) if why.contains("is older") || why.contains("(404)") => format!(
                                "{machine}'s Desk is older and can't find {repo} there yet: update Desk on {machine}, or give {key} its folder there"
                            ),
                            e => format!("{key}'s checkout of {repo} on {machine} couldn't be read: {e}"),
                        }))
                    })?;
                serde_json::from_value(answer).map_err(|_| {
                    StartRefused::Failed(DeskError::Invalid(format!(
                        "{machine}'s answer about {repo} couldn't be read."
                    )))
                })?
            }
        };
        let on = runs.machine.as_deref().unwrap_or("this computer");
        // What a machine says of its disk is a path and a reason in Desk's
        // own words; anything else in them isn't said on.
        let plain = |s: &str| -> String {
            crew::one_line(s, 300)
                .chars()
                .filter(|c| !c.is_control())
                .collect()
        };
        match got {
            crate::repos::Got::Ready { path }
                if Some(&path) == folder.as_ref() || Some(&path) == found.as_ref() =>
            {
                Ok(false)
            }
            crate::repos::Got::Ready { path } => {
                if !path.starts_with('/') || path.chars().any(char::is_control) {
                    return Err(StartRefused::Failed(DeskError::Invalid(format!(
                        "{on}'s answer about {repo} isn't a folder."
                    ))));
                }
                let saved = self
                    .0
                    .store
                    .found_folder(
                        t.id,
                        &path,
                        &repo.key(),
                        runs.machine.as_deref(),
                        runs.folder.as_deref(),
                        t.version,
                    )
                    .await
                    .map_err(internal)?;
                if !saved {
                    return Err(StartRefused::Skipped(
                        "That ticket changed as its checkout was found; it is read again.".into(),
                    ));
                }
                Ok(true)
            }
            crate::repos::Got::Cloning { path } => Err(StartRefused::Waits(format!(
                "{key} waits while Desk clones {repo} into {} on {on}; it starts once that is done.",
                plain(&path)
            ))),
            crate::repos::Got::Failed { why } => Err(StartRefused::Failed(DeskError::Invalid(
                format!("{key} has no checkout of {repo} on {on}: {}.", plain(&why)),
            ))),
        }
    }

    /// Puts ticket `ticket` of plan `plan`, in Doing on `session` (its
    /// assignee's id: `machine/id` for one on a machine; or the session
    /// it is still being started on, given it by no one yet), back in To
    /// do, held back with `why`: its plan doesn't start it again by itself
    /// until someone starts it, hands it out or moves it on, in the one
    /// write that moves it. Whether it did: not once it is elsewhere,
    /// someone else's, out of the plan, or changed since it was read.
    pub async fn hold_back_ticket(
        &self,
        plan: Uuid,
        ticket: Uuid,
        session: &str,
        why: &str,
    ) -> Result<bool, DeskError> {
        let t = self.ticket_of(ticket).await?;
        let on_it = t.state == TicketState::Doing
            && match &t.assignee {
                Some(a) => a.kind == PartyKind::Session && a.id == session,
                None => self
                    .0
                    .store
                    .starting_of(ticket)
                    .await
                    .map_err(internal)?
                    .is_some_and(|s| s.session.to_string() == session),
            }
            && self
                .0
                .store
                .plan_of_ticket(ticket)
                .await
                .map_err(internal)?
                == Some(plan);
        if !on_it {
            return Ok(false);
        }
        let mv = Move {
            state: TicketState::Todo,
            before: None,
            after: None,
        };
        match self.move_holding(t, mv, &By::desk(why), Some(why)).await {
            Ok(_) => {
                self.plan_touched(plan).await;
                Ok(true)
            }
            Err(DeskError::Conflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Finishes the start `s` that a stop cut off: its session is known (it
    /// is linked to the ticket; one the holder kept was adopted when Desk
    /// started), or it never started (it is started now, as the same
    /// session, run by `boss`; with no boss that may start it, the ticket
    /// waits in To do again). A ticket the person moved meanwhile is left
    /// as they put it. `None` when nothing was started.
    pub async fn plan_restart_start(
        &self,
        s: Starting,
        boss: Option<&Party>,
        by: &By,
    ) -> Result<Option<Started>, DeskError> {
        let _one = self.0.plan_starting.lock().await;
        let t = match self.ticket_of(s.ticket).await {
            Ok(t) => t,
            Err(DeskError::NotFound) => {
                self.end_start_of(&s).await;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if let Ok(view) = self.view(s.session) {
            self.plan_link(t, view.clone(), by).await;
            self.end_start_of(&s).await;
            return Ok(Some(Started {
                id: view.id,
                machine: None,
                name: view.label().to_string(),
                note: None,
            }));
        }
        let plan_id = self.0.store.plan_of_ticket(t.id).await.map_err(internal)?;
        let p = match plan_id {
            Some(id) => Some(self.plan_of(id).await?),
            None => None,
        };
        let still = t.state == TicketState::Doing && t.assignee.is_none();
        let held = p
            .as_ref()
            .and_then(|p| self.plan_starts_held(p.group.as_deref(), p.id))
            .is_some();
        // A start on a machine that may have made its session there keeps
        // its link while it can't be looked for (no boss, paused or held):
        // undone, it could be made twice.
        let on_machine = match &p {
            Some(p) => self
                .0
                .store
                .plan_runs(p.id)
                .await
                .map_err(internal)?
                .into_iter()
                .any(|(id, r)| id == t.id && r.machine.is_some()),
            None => false,
        };
        if on_machine
            && still
            && p.as_ref().is_some_and(|p| {
                p.state == PlanState::Running && (!p.runs() || held || boss.is_none())
            })
        {
            return Ok(None);
        }
        let (Some(p), Some(boss)) = (p.filter(|p| still && p.runs() && !held), boss) else {
            // Moved, dropped, paused or held meanwhile, or no boss may
            // start it: no session for it now. A ticket left in Doing with
            // no one on it goes back to To do.
            self.undo_start(&s).await;
            return Ok(None);
        };
        let runs = self
            .0
            .store
            .plan_runs(p.id)
            .await
            .map_err(internal)?
            .into_iter()
            .find(|(id, _)| *id == t.id)
            .map(|(_, r)| r)
            .unwrap_or_default();
        let others = self.plan_tickets(p.id).await?;
        let worktree = others.iter().any(|o| {
            o.id != t.id
                && (o.state == TicketState::Doing || o.starting)
                && o.folder.is_some()
                && o.folder == runs.folder
                && o.machine == runs.machine
        });
        self.plan_launch(&p, t, s.session, s.at, true, &runs, worktree, boss, by)
            .await
            .map(Some)
            .map_err(DeskError::from)
    }

    /// The starts on the person's machines whose outcome wasn't known, made
    /// over `older_than` ago: each is looked for there again, and started
    /// only if it isn't there ([`Desk::plan_restart_start`]).
    pub async fn machine_starts_unsure(&self, older_than: std::time::Duration) -> Vec<Starting> {
        let Ok(list) = self.0.store.starting().await else {
            return Vec::new();
        };
        let old = crew::now() - chrono::Duration::from_std(older_than).unwrap_or_default();
        let mut out = Vec::new();
        for s in list.into_iter().filter(|s| s.at < old) {
            let Ok(Some(plan)) = self.0.store.plan_of_ticket(s.ticket).await else {
                continue;
            };
            let on_machine = self
                .0
                .store
                .plan_runs(plan)
                .await
                .unwrap_or_default()
                .into_iter()
                .any(|(id, r)| id == s.ticket && r.machine.is_some());
            if on_machine {
                out.push(s);
            }
        }
        out
    }

    /// How many of `boss`'s plans' starts are under way with no session
    /// yet: each holds one of its slots.
    pub async fn starts_under_way(&self, boss: &str) -> usize {
        let Ok(list) = self.0.store.starting().await else {
            return 0;
        };
        let mut n = 0;
        for s in list.iter().filter(|s| self.view(s.session).is_err()) {
            let plan = match self.0.store.plan_of_ticket(s.ticket).await {
                Ok(Some(id)) => self.0.store.plan(id).await.ok().flatten(),
                _ => None,
            };
            if plan.is_some_and(|p| p.boss == boss) {
                n += 1;
            }
        }
        n
    }

    async fn end_start_of(&self, s: &Starting) {
        if let Err(e) = self.0.store.end_start(s.ticket, s.session).await {
            tracing::warn!(ticket = %s.ticket, error = %e, "a start's link stays");
        }
    }

    /// Undoes the start `s` that didn't happen: its ticket, still in Doing
    /// with no one on it, waits in To do again (one the person moved stays
    /// where they put it), and its link goes.
    async fn undo_start(&self, s: &Starting) {
        if let Ok(mut back) = self.ticket_of(s.ticket).await {
            if back.state == TicketState::Doing && back.assignee.is_none() {
                let at = crew::now();
                if self
                    .to_column(&mut back, TicketState::Todo, at)
                    .await
                    .is_ok()
                {
                    if let Err(e) = self.save_ticket(&mut back).await {
                        tracing::info!(ticket = %s.ticket, error = %e, "a ticket that didn't start stays in Doing");
                    }
                }
            }
        }
        self.end_start_of(s).await;
    }

    /// Starts `session` for `t`, which is in Doing with its `starting`
    /// link, and links them; whatever keeps it from starting puts the
    /// ticket back in To do, so the runner tries again later.
    #[allow(clippy::too_many_arguments)]
    async fn plan_launch(
        &self,
        p: &Plan,
        t: Ticket,
        session: Uuid,
        since: chrono::DateTime<chrono::Utc>,
        retry: bool,
        runs: &Runs,
        worktree: bool,
        boss: &Party,
        by: &By,
    ) -> Result<Started, StartRefused> {
        let s = Starting {
            ticket: t.id,
            session,
            at: since,
        };
        let launched = match runs.machine.clone() {
            None => self
                .plan_spawn(p, &t, session, runs, worktree, boss, by)
                .await
                .map(|v| {
                    let started = Started {
                        id: v.id,
                        machine: None,
                        name: v.label().to_string(),
                        note: None,
                    };
                    (started, Some(v))
                })
                .map_err(StartRefused::from),
            Some(m) => match self
                .plan_spawn_on(&m, p, &t, session, since, retry, runs, worktree, boss)
                .await
            {
                Ok(started) => Ok((started, None)),
                // It may have started there: its link stays, and Desk
                // looks for it there again ([`Desk::machine_starts_unsure`])
                // before any start of it.
                Err((e, true)) => {
                    self.plan_touched(p.id).await;
                    return Err(e);
                }
                Err((e, false)) => Err(e),
            },
        };
        let (started, view) = match launched {
            Ok(v) => v,
            Err(e) => {
                self.undo_start(&s).await;
                self.plan_touched(p.id).await;
                return Err(e);
            }
        };
        match view {
            Some(view) => self.plan_link(t, view, by).await,
            None => self.plan_link_on(p, t, &started, by).await,
        }
        self.end_start_of(&s).await;
        self.plan_touched(p.id).await;
        Ok(started)
    }

    /// The session for `t` on `machine`, through Desk on this computer: the
    /// one there already for it (named for its key, in its folder), when
    /// one is, so a start cut off or never answered is never made twice;
    /// else one started there now in the machine's own folder, its prompt
    /// saying its board is here. A plan of the main boss (no group) on an
    /// engine Desk checks every call of starts in its plan's grant there,
    /// run by its boss from launch ([`Desk::create_held`]), as `session`:
    /// until the machine holds that grant, the start waits.
    #[allow(clippy::too_many_arguments)]
    async fn plan_spawn_on(
        &self,
        machine: &str,
        p: &Plan,
        t: &Ticket,
        session: Uuid,
        since: chrono::DateTime<chrono::Utc>,
        retry: bool,
        runs: &Runs,
        worktree: bool,
        boss: &Party,
    ) -> Result<Started, (StartRefused, bool)> {
        let failed = |e: DeskError, unsure: bool| (StartRefused::Failed(e), unsure);
        let sure = |e: DeskError| failed(e, false);
        let key = self.key_of(t).await.map_err(sure)?;
        let needs = self.need_keys(t).await.map_err(sure)?;
        let (Some(engine), Some(folder)) = (runs.engine, runs.folder.clone()) else {
            return Err(sure(DeskError::Invalid(format!(
                "{key} doesn't say where its session runs."
            ))));
        };
        let crew = self.plan_crew().ok_or_else(|| {
            sure(DeskError::Invalid(format!(
                "{machine} can't be reached from this hub."
            )))
        })?;
        // A retry of a start that may have reached it stays unsure until
        // the machine is read.
        let read_refused = |e: crew::MachineRefused| {
            let e = match e {
                crew::MachineRefused::Refused(why) => {
                    DeskError::Invalid(format!("{machine}'s sessions couldn't be read: {why}"))
                }
                e => DeskError::Invalid(e.to_string()),
            };
            failed(e, retry)
        };
        // Only a start that may have reached the machine is unsure; one the
        // machine can't hold for its boss yet waits, as the machine says.
        let refused = |e: crew::MachineRefused| match e {
            crew::MachineRefused::NotHeld(why) => (
                StartRefused::Waits(format!("{key} waits on {machine}: {why}")),
                false,
            ),
            crew::MachineRefused::Refused(why) => sure(DeskError::Invalid(format!(
                "{machine} didn't start {key}: {why}"
            ))),
            crew::MachineRefused::WorktreeTaken(why, holder) => {
                sure(DeskError::WorktreeTaken { why, holder })
            }
            e @ crew::MachineRefused::Unknown(_) => failed(DeskError::Invalid(e.to_string()), true),
            e => sure(DeskError::Invalid(e.to_string())),
        };
        // Started in its plan's grant there, run by its boss from launch, for
        // a plan of the main boss on an engine Desk checks every call of;
        // Desk carries the start's grant and boss from the call itself.
        let group = self.relay_group(p);
        let held = (group.is_none() && crate::engine::boss_safe(engine))
            .then(|| crate::grant::plan_key(p.id));
        let read = || async {
            let listed = crew
                .machine_call(
                    machine,
                    group,
                    &boss.id,
                    "desk.sessions",
                    serde_json::json!({}),
                )
                .await
                .map_err(read_refused)?;
            serde_json::from_value::<Vec<crew::MachineSession>>(listed).map_err(|_| {
                failed(
                    DeskError::Invalid(format!("{machine}'s sessions couldn't be read.")),
                    retry,
                )
            })
        };
        let made_in = |cwd: &str| {
            cwd == folder
                || cwd.starts_with(&format!("{}/.worktrees/", folder.trim_end_matches('/')))
        };
        // The one made for this start: a held start's is the session id it
        // gave, whatever has come of it since (the person may have it). Else
        // (a start not held, or a machine that doesn't take its id), one of
        // its key made since: an older session of the same key (the ticket
        // was sent back and started again) is never taken up. Wide enough
        // for the two computers' clocks to differ. A held start takes up its
        // own session there by its id, whatever that machine's clock says;
        // by its name, only one its boss has there, never the person's own,
        // and never one resting there made before this start began: that is
        // an earlier start's, which `free_resting` replaces.
        let after = since - chrono::Duration::minutes(10);
        let earlier = |s: &crew::MachineSession, made: chrono::DateTime<chrono::Utc>| {
            made < since && matches!(s.state.as_str(), "idle" | "done")
        };
        // There, a boss of this hub goes by its agent's id
        // ([`crate::grant::BossRef::name`]).
        let made_for = |s: &crew::MachineSession| match held {
            Some(_) => {
                s.created_at.is_some_and(|c| c >= after && !earlier(s, c))
                    && s.boss_has.as_deref() == Some(boss.id.as_str())
            }
            None => s.created_at.is_none_or(|c| c >= after),
        };
        let there = |listed: &[crew::MachineSession]| {
            let by_name = || {
                listed
                    .iter()
                    .filter(|s| {
                        s.name == key
                            && made_in(&s.cwd)
                            && !matches!(s.state.as_str(), "closed" | "stopped")
                            && made_for(s)
                    })
                    .max_by_key(|s| s.created_at)
            };
            listed
                .iter()
                .find(|s| held.is_some() && s.id == session)
                .or_else(by_name)
                .map(|s| Started {
                    id: s.id,
                    machine: Some(machine.to_string()),
                    name: s.label().to_string(),
                    note: None,
                })
        };
        if let Some(started) = there(&read().await?) {
            return Ok(started);
        }
        let prompt = format!(
            "{}{}\n\n{}",
            board::handed_text(&key, t, &needs),
            self.again_note(t).await,
            machine_note(&key, machine)
        );
        let mut body = serde_json::json!({
            "engine": engine.as_str(),
            "name": key,
            "cwd": folder,
            "goal": board::goal_of(&key, t),
            "prompt": prompt,
        });
        if let Some(view) = runs.view {
            body["view"] = view.as_str().into();
        }
        // Its own worktree there, when another of the plan's tickets works
        // in the same folder: the machine makes it.
        if worktree {
            body["worktree"] = branch_for(&key).into();
        }
        // Held there for its boss, its boss's own earlier session of it
        // resting in that worktree gives way, as here; a Desk there older
        // than that ignores it, and the start is refused as taken.
        if worktree && held.is_some() {
            body["free_resting"] = true.into();
            // The session its ticket names there now (`<machine>/<id>`, as
            // [`Self::assign_machine_session`] names it), which never gives way.
            let assigned = t
                .assignee
                .as_ref()
                .and_then(|a| a.id.strip_prefix(machine)?.strip_prefix('/'))
                .and_then(|id| id.parse::<Uuid>().ok());
            if let Some(a) = assigned {
                body["assigned"] = a.to_string().into();
            }
        }
        body["deps"] = true.into();
        if let Some(model) = &runs.model {
            body["model"] = model.clone().into();
        }
        if held.is_some() {
            body["id"] = session.to_string().into();
        }
        let made = match crew
            .machine_call(
                machine,
                held.as_deref().or(group),
                &boss.id,
                "desk.session_start",
                body,
            )
            .await
        {
            // A retry's worktree may be held by the session its first try
            // made there: taken up when it is.
            Err(crew::MachineRefused::WorktreeTaken(why, holder)) if retry => {
                return read()
                    .await
                    .ok()
                    .and_then(|listed| there(&listed))
                    .ok_or_else(|| sure(DeskError::WorktreeTaken { why, holder }));
            }
            made => made.map_err(refused)?,
        };
        let made: crew::MachineSession = serde_json::from_value(made).map_err(|_| {
            // Started there, its answer unread: found there next time.
            failed(
                DeskError::Invalid(format!(
                    "{machine} started {key}, but its answer couldn't be read; Desk finds it there next time."
                )),
                true,
            )
        })?;
        // A Desk there, or here, older than held starts makes it the
        // person's, under an id of its own: said, so its boss knows why it
        // only suggests.
        let note = (held.is_some() && made.id != session).then(|| {
            format!(
                "{key} started on {machine} as the person's, not yours: Desk on {machine}, or on this computer, \
                 is older and doesn't start a plan's sessions for its boss. Until it is updated, what you send \
                 {key} is a suggestion the person hands over."
            )
        });
        Ok(Started {
            id: made.id,
            machine: Some(machine.to_string()),
            name: made.label().to_string(),
            note,
        })
    }

    fn plan_crew(&self) -> Option<std::sync::Arc<dyn crew::Crew>> {
        self.0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    /// Gives `t` to `started`, its session on one of the person's machines:
    /// that session in the plan's group in the sidebar, and the ticket its.
    async fn plan_link_on(&self, p: &Plan, t: Ticket, started: &Started, by: &By) {
        let item = started.item();
        if let Some(g) = self.live_group(p).map(str::to_string) {
            if self.group_of_member(&item).as_deref() != Some(g.as_str()) {
                if let Err(e) = self.change_layout(|l| l.put(&item, &g, None)).await {
                    tracing::info!(%item, error = %e, "a machine's plan session wasn't put in its group");
                }
            }
        }
        let id = t.id;
        let key = match self.key_of(&t).await {
            Ok(k) => k,
            Err(_) => "a ticket".into(),
        };
        let Ok(now) = self.ticket_of(id).await else {
            return;
        };
        if now.assignee.is_some() || now.state != TicketState::Doing {
            return;
        }
        if let Err(e) = self.assign_machine_session(now, &key, started, by).await {
            tracing::info!(ticket = %id, error = %e, "a started ticket wasn't given to its machine's session");
        }
    }

    /// What a ticket started again is told after its handing text: a
    /// session worked on it before, so what is already done is checked
    /// first. Empty for a first start.
    async fn again_note(&self, t: &Ticket) -> &'static str {
        match self.store().ticket_worked_before(t.id).await {
            Ok(true) => AGAIN_NOTE,
            Ok(false) => "",
            Err(e) => {
                tracing::info!(ticket = %t.id, error = %format!("{e:#}"), "whether a ticket was worked on before couldn't be read");
                ""
            }
        }
    }

    /// The session for `t`, started as `session`.
    #[allow(clippy::too_many_arguments)]
    async fn plan_spawn(
        &self,
        p: &Plan,
        t: &Ticket,
        session: Uuid,
        runs: &Runs,
        worktree: bool,
        boss: &Party,
        by: &By,
    ) -> Result<SessionView, DeskError> {
        let key = self.key_of(t).await?;
        let needs = self.need_keys(t).await?;
        let (Some(engine), Some(folder)) = (runs.engine, runs.folder.clone()) else {
            return Err(DeskError::Invalid(format!(
                "{key} doesn't say where its session runs."
            )));
        };
        // Its own worktree, when another of the plan's tickets works in the
        // same folder and it is a repository: made once, and started in
        // again by each later start of it.
        let branch = (worktree && crate::worktree::in_repo(Path::new(&folder)).await)
            .then(|| branch_for(&key));
        // Its session from an earlier start, resting there since it was
        // moved back to To do, gives way.
        if let Some(b) = &branch {
            if let Ok((_, tree)) = crate::worktree::located(Path::new(&folder), b).await {
                let assigned = t.assignee.as_ref().map(|a| a.id.as_str());
                self.free_resting(&tree, &key, assigned, boss).await?;
            }
        }
        let group = self.live_group(p).map(str::to_string);
        let new = NewSession {
            fresh_worktree: false,
            engine,
            view: runs.view,
            name: key.clone(),
            cwd: folder,
            goal: board::goal_of(&key, t),
            worktree: branch,
            prompt: Some(format!(
                "{}{}",
                board::handed_text(&key, t, &needs),
                self.again_note(t).await
            )),
            approvals: None,
            approve_for_me: false,
            // As its boss runs: an agent's `[llm] fast`.
            fast: self.fast_for(engine, boss),
            // As the person chose for the ticket; none, its engine's own.
            model: runs.model.clone(),
            account: None,
            env: BTreeMap::new(),
            group,
            review_of: None,
        };
        self.create_for_as(session, new, boss, by.clone()).await
    }

    /// Gives `t` to the session started for it, as it is now: the person's
    /// move meanwhile stands, and the session runs either way.
    async fn plan_link(&self, t: Ticket, view: SessionView, by: &By) {
        let id = t.id;
        let key = match self.key_of(&t).await {
            Ok(k) => k,
            Err(_) => "a ticket".into(),
        };
        let now = match self.ticket_of(id).await {
            Ok(now) => now,
            Err(e) => {
                tracing::info!(ticket = %id, error = %e, "a started ticket wasn't read again");
                return;
            }
        };
        if now.assignee.is_some() || now.state != TicketState::Doing {
            return;
        }
        if let Err(e) = self.assign_session(now, &key, view, by, false).await {
            tracing::info!(ticket = %id, error = %e, "a started ticket wasn't given to its session");
        }
    }
}

/// `v`, a plan's ticket, as the plan's rules read it. A need of it
/// outside the plan (`ours`, its tickets) holds it back only until that
/// ticket is done: one taken on from a synced board may be blocked there
/// by an issue the plan doesn't have.
fn plan_ticket(
    v: &super::TicketView,
    runs: Option<&Runs>,
    starting: bool,
    ours: &HashSet<Uuid>,
) -> PlanTicket {
    let r = runs.cloned().unwrap_or_default();
    PlanTicket {
        id: v.ticket.id,
        key: v.key.clone(),
        state: v.ticket.state,
        rank: v.ticket.rank,
        needs: v
            .ticket
            .needs
            .iter()
            .filter(|n| ours.contains(n) || v.blockers.contains(n))
            .copied()
            .collect(),
        engine: r.engine,
        folder: r.folder,
        view: r.view,
        machine: r.machine,
        model: r.model,
        repo: r.repo,
        assigned: v.ticket.assignee.is_some(),
        session: v
            .ticket
            .assignee
            .as_ref()
            .filter(|a| a.kind == PartyKind::Session && !r.session_closed)
            .map(|a| a.id.clone()),
        starting,
        held_back: r.held_back,
    }
}

/// The title of the ticket made for session `view`: its goal.
fn session_title(view: &SessionView) -> String {
    match view.goal.trim() {
        "" => view.label().to_string(),
        goal => crew::one_line(goal, board::TITLE_MAX),
    }
}

/// `p` as revision `next` says its goal: what it achieves, how that is
/// measured, and its goal's ticket while it still has it.
fn follow_goal(p: &mut Plan, next: &Snapshot) {
    p.goal.clone_from(&next.goal);
    p.result.clone_from(&next.result);
    p.goal_ticket = p.goal_ticket.filter(|g| next.ticket(*g).is_some());
}

/// The titles of the tickets a draft takes on, as
/// [`plan::snapshot_with`] reads them.
fn titles_of(taking: &HashMap<Uuid, Ticket>) -> HashMap<Uuid, String> {
    taking.iter().map(|(id, t)| (*id, t.said_title())).collect()
}

/// The needs the tickets `s` takes on now (`taking`) have already on
/// another of its tickets, or another of its tickets taken on has on them
/// (`rows`, every ticket taken on as it is), kept in `s`: a plan never
/// drops a need it found.
fn keep_needs(s: &mut Snapshot, taking: &HashMap<Uuid, Ticket>, rows: &HashMap<Uuid, Ticket>) {
    let ids: HashSet<Uuid> = s.tickets.iter().map(|t| t.id).collect();
    for t in s.tickets.iter_mut().filter(|t| t.adopted) {
        let Some(row) = taking.get(&t.id).or_else(|| rows.get(&t.id)) else {
            continue;
        };
        for n in &row.needs {
            let fresh = taking.contains_key(&t.id) || taking.contains_key(n);
            if fresh && ids.contains(n) && !t.needs.contains(n) && *n != t.id {
                t.needs.push(*n);
            }
        }
    }
}

/// What a plan to approve comes to, in a line: "4 tickets in 3 waves, up to
/// 2 at once".
fn plan_why(s: &Snapshot, parallel: Option<i16>) -> String {
    let rows: Vec<PlanTicket> = s
        .tickets
        .iter()
        .enumerate()
        .map(|(i, t)| PlanTicket {
            id: t.id,
            key: None,
            state: TicketState::Backlog,
            rank: i as f64,
            needs: t.needs.clone(),
            engine: Some(t.engine),
            folder: Some(t.folder.clone()),
            view: t.view,
            machine: t.machine.clone(),
            model: t.model.clone(),
            repo: t.repo.clone(),
            assigned: false,
            session: None,
            starting: false,
            held_back: None,
        })
        .collect();
    let waves = plan::waves(&rows).len();
    let n = s.tickets.len();
    let tickets = if n == 1 { "ticket" } else { "tickets" };
    let in_waves = if waves == 1 { "wave" } else { "waves" };
    match parallel {
        Some(k) => format!("{n} {tickets} in {waves} {in_waves}, up to {k} at once."),
        None => format!("{n} {tickets} in {waves} {in_waves}."),
    }
}

#[cfg(test)]
#[path = "plans_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plans_adopt_tests.rs"]
mod adopt_tests;

#[cfg(test)]
#[path = "plans_group_tests.rs"]
mod group_tests;

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

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError};
use super::{Move, NewBoard};
use crate::board::{self, Board, Link, Ticket, TicketState, RANK_STEP};
use crate::crew::{self, By, Kind, Party, PartyKind};
use crate::engine::{Engine, View};
use crate::floor::{Answer, DecideRef, Need, NeedKind, Place, Who};
use crate::model::{DeskEvent, NewSession, SessionState, SessionView};
use crate::plan::{
    self, Change, Draft, DraftTicket, Paused, Plan, PlanBudget, PlanState, PlanTicket, Planned,
    Problem, Progress, Revision, Snapshot, Start,
};
use crate::plan_store::{BeginStart, Runs, Starting};
use workforce_core::budget::{BudgetScope, Standing};

/// What the desk keeps of plans in memory: the ones waiting on the person,
/// for the Floor, which is read without waiting on Postgres.
#[derive(Default)]
pub(crate) struct Plans {
    awaiting: HashMap<Uuid, Need>,
    /// Why each plan paused for a ticket failing to start is, in the
    /// person's words.
    failing: HashMap<Uuid, String>,
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
    /// Why it is paused, in the person's words, while it is.
    pub paused_why: Option<String>,
    pub progress: Progress,
    /// What it may spend, as approved (or as proposed, before it is).
    pub budget: Option<PlanBudget>,
    /// What holds its new starts, in the Floor's words, while Desk's
    /// breaker does: its own budget, its group's or Desk's. The person
    /// says Continue on the Floor.
    pub held: Option<String>,
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
    /// Its session is being started.
    pub starting: bool,
    /// What holds its session, while Desk's breaker does: the ticket is
    /// blocked on the person, and nothing is started in its place.
    pub held: Option<String>,
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
    /// The boss's group; none is the main boss's.
    pub group: Option<String>,
    /// What it may spend, as the boss proposes it.
    pub budget: Option<PlanBudget>,
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
    /// What it may spend; `null` takes its budget off.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub budget: Option<Option<PlanBudget>>,
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
}

impl From<DeskError> for StartRefused {
    fn from(e: DeskError) -> Self {
        Self::Failed(e)
    }
}

/// A plan's ticket's session, started: on this computer (`machine` none),
/// or on one of the person's machines, by its id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub id: Uuid,
    pub machine: Option<String>,
    pub name: String,
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
            Paused::StartFailing => self.0.boss_plans.lock().failing.get(&p.id).cloned(),
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
        let views = self.views(tickets).await?;
        Ok(views
            .into_iter()
            .map(|v| plan_ticket(&v, runs.get(&v.ticket.id), starting.contains(&v.ticket.id)))
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

    async fn plan_summary(&self, p: Plan) -> Result<PlanSummary, DeskError> {
        let tickets = self.plan_tickets(p.id).await?;
        let budget = self.applied(&p).await?.budget;
        Ok(PlanSummary {
            boss_name: self.plan_boss_name(&p),
            paused_why: self.paused_why(&p),
            progress: plan::progress(&tickets),
            budget,
            held: self.plan_held(&p),
            plan: p,
        })
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
            .map(|t| plan_ticket(&t.ticket, runs.get(&t.ticket.ticket.id), t.starting))
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
        Ok(PlanDetail {
            waves: plan::waves(&rows),
            summary: PlanSummary {
                boss_name: self.plan_boss_name(&p),
                paused_why: self.paused_why(&p),
                progress: plan::progress(&rows),
                budget: applied.budget.clone(),
                held: self.plan_held(&p),
                plan: p,
            },
            tickets: shown,
            revisions,
            spend,
        })
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

    /// Reads the plans waiting on the person.
    pub(super) async fn plans_load(&self) {
        match self.0.store.plans(None).await {
            Ok(all) => {
                for p in all.into_iter().filter(Plan::awaiting) {
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
        let failed = self.0.boss_plans.lock().failing.get(&p.id).cloned();
        let (ask, why) = match p.approved_revision {
            _ if failing && !p.awaiting() => (
                format!(
                    "{boss}'s plan is paused: {}",
                    failed.unwrap_or_else(|| "a ticket keeps failing to start".into())
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
        let mut s =
            plan::snapshot_of(&new.draft, None, new.engine, &mut Uuid::new_v4).map_err(problems)?;
        s.parallel = new.parallel;
        s.budget = new.budget;
        plan::validate(&s, &self.engines(), &|f| Path::new(f).is_dir()).map_err(problems)?;
        if new.parallel.is_some_and(|p| !(1..=12).contains(&p)) {
            return Err(DeskError::Invalid(
                "A plan runs from 1 to 12 sessions at once.".into(),
            ));
        }
        let b = match new.board {
            Some(id) => {
                let b = self.board_of(id).await?;
                if b.group != new.group {
                    return Err(DeskError::Invalid(
                        "That board isn't in the group you run.".into(),
                    ));
                }
                if let Some(sync) = &b.sync {
                    return Err(DeskError::Invalid(format!(
                        "{} is synced with {} {}: a plan's tickets go on a board of Desk's own.",
                        b.name,
                        sync.system.title(),
                        sync.target
                    )));
                }
                b
            }
            None => self.plan_board(new.group.as_deref()).await?,
        };
        let at = crew::now();
        let p = Plan {
            id: Uuid::new_v4(),
            board: b.id,
            group: b.group.clone(),
            boss: boss.id.clone(),
            goal: s.goal.clone(),
            state: PlanState::Proposed,
            paused: None,
            parallel: new.parallel,
            revision: 1,
            approved_revision: None,
            created_at: at,
            updated_at: at,
        };
        let summary = format!(
            "Proposed {} tickets for “{}”",
            s.tickets.len(),
            crew::one_line(&s.goal, 120)
        );
        // The plan, its tickets and its first revision, in one write.
        let mut rank = self
            .end_rank(b.id, TicketState::Backlog, Uuid::nil())
            .await?;
        let mut rows: Vec<(Ticket, Runs)> = Vec::with_capacity(s.tickets.len());
        for t in &s.tickets {
            rows.push((
                Ticket {
                    id: t.id,
                    board: b.id,
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
                    snoozed_at: None,
                    state_at: at,
                    created_at: at,
                    updated_at: at,
                    version: 0,
                },
                Runs {
                    plan: Some(p.id),
                    engine: Some(t.engine),
                    folder: Some(t.folder.clone()),
                    view: t.view,
                    machine: t.machine.clone(),
                    model: t.model.clone(),
                },
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
        self.0
            .store
            .add_plan(&p, &mut rows, &r)
            .await
            .map_err(internal)?;
        for (t, _) in rows {
            if let Ok(v) = self.view_ticket(t).await {
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
    /// its links, history and whoever has it. The board is in the plan's
    /// group and one of Desk's own; nothing moves while a ticket of it is
    /// starting.
    pub async fn move_plan(&self, id: Uuid, board: Uuid, by: &By) -> Result<PlanDetail, DeskError> {
        let _write = self.0.plan_write.lock().await;
        let mut p = self.plan_of(id).await?;
        if p.board == board {
            return self.plan_detail(p).await;
        }
        let to = self.board_of(board).await?;
        if to.group != p.group {
            return Err(DeskError::Invalid(format!(
                "{} isn't in the plan's group: name a board of that group, or pass new_board to make one there.",
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
        let b = self.board_of(p.board).await?;
        if b.sync.is_some() && !after.added(before).is_empty() {
            return Err(DeskError::Invalid(format!(
                "{} is synced: a plan adds no tickets to it.",
                b.name
            )));
        }
        let mut rank = self.end_rank(b.id, column, Uuid::nil()).await?;
        for t in after.added(before) {
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
                snoozed_at: None,
                state_at: at,
                created_at: at,
                updated_at: at,
                version: 0,
            };
            rank += RANK_STEP;
            self.0.store.add_ticket(&mut row).await.map_err(internal)?;
            self.put_runs(p, t).await?;
        }
        for id in after.dropped(before) {
            let Ok(t) = self.ticket_of(id).await else {
                continue;
            };
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
            let was = before.ticket(t.id);
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
            if was.is_some_and(|w| {
                (w.engine, &w.folder, w.view, &w.machine, &w.model)
                    != (t.engine, &t.folder, t.view, &t.machine, &t.model)
            }) {
                self.put_runs(p, t).await?;
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
                self.0.store.put_need(t.id, *n).await.map_err(internal)?;
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

    async fn put_runs(&self, p: &Plan, t: &Planned) -> Result<(), DeskError> {
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
                },
            )
            .await
            .map_err(internal)
    }

    /// The tickets not started yet, in the order `s` lists them: what
    /// starts first.
    async fn plan_reorder(&self, p: &Plan, s: &Snapshot) -> Result<(), DeskError> {
        let rows = self.0.store.plan_tickets(p.id).await.map_err(internal)?;
        for column in [TicketState::Backlog, TicketState::Todo] {
            let mut queued: Vec<&Ticket> = rows.iter().filter(|t| t.state == column).collect();
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

    /// The tickets of `p` that have started: past To do, or someone has
    /// them, or a session is being started for them.
    async fn started(&self, p: &Plan) -> Result<HashSet<Uuid>, DeskError> {
        Ok(self
            .plan_tickets(p.id)
            .await?
            .into_iter()
            .filter(|t| {
                t.assigned
                    || t.starting
                    || !matches!(t.state, TicketState::Backlog | TicketState::Todo)
            })
            .map(|t| t.id)
            .collect())
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
        let mut next = match &edit.tickets {
            Some(tickets) => plan::snapshot_of(
                &Draft {
                    goal: edit.goal.clone().unwrap_or_else(|| newest.goal.clone()),
                    tickets: tickets.clone(),
                },
                Some(&newest),
                engine,
                &mut Uuid::new_v4,
            )
            .map_err(problems)?,
            None => newest.clone(),
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
        plan::validate(&next, &self.engines(), &|f| Path::new(f).is_dir()).map_err(problems)?;
        let started = self.started(&p).await?;
        let change = plan::plan_change(&applied, &next, &started)
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
        self.add_revision(&p, n, by, kind, &summary, &next).await?;
        p.revision = n;
        if !waits {
            let column = match p.approved_revision {
                None => TicketState::Backlog,
                Some(_) => TicketState::Todo,
            };
            let from = match p.approved_revision {
                None => newest,
                Some(_) => applied,
            };
            self.plan_rows_follow(&p, &from, &next, column, by).await?;
            if p.approved_revision.is_some() {
                p.approved_revision = Some(n);
                // What it may spend is what applies now.
                self.keep_plan_budget(&p, next.budget.as_ref(), by).await?;
            }
            p.parallel = next.parallel;
            p.goal.clone_from(&next.goal);
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
        let _write = self.0.plan_write.lock().await;
        // An unbound boss approves its own plans: the person let it. The
        // hub asks this for one only on the person's own words asking for
        // the work to start (`crew.plan`'s `start_now`); a plan it only
        // proposes stays a draft.
        let unbound = by.party.kind == PartyKind::Manager
            && !crate::grant::is_served(&by.party.id)
            && self.full_of(&by.party.id).unbound;
        if by.party.kind != PartyKind::You && !unbound {
            return Err(DeskError::Invalid(
                "Only the person approves a plan.".into(),
            ));
        }
        let mut p = self.plan_of(id).await?;
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
            return self.plan_detail(p).await;
        }
        let mut next = self.newest(&p).await?;
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
            let kind = plan::plan_change(&applied, &next, &started)
                .map_err(|e| DeskError::Conflict(e.to_string()))?
                .unwrap_or(Change::Minor);
            revision = p.revision + 1;
            self.add_revision(&p, revision, by, kind, &summary, &next)
                .await?;
            p.revision = revision;
        }
        if p.approved_revision == Some(revision) {
            return self.plan_detail(p).await;
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
                plan::plan_change(&applied, &next, &started)
                    .map_err(|e| DeskError::Conflict(e.to_string()))?;
                self.plan_rows_follow(&p, &applied, &next, TicketState::Todo, by)
                    .await?;
                p.goal.clone_from(&next.goal);
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
        self.plan_saved(&p).await
    }

    /// Pauses plan `id` for `why`: it starts nothing until it is resumed.
    /// What runs keeps running.
    pub async fn pause_plan(
        &self,
        id: Uuid,
        why: Paused,
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
        if p.paused == Some(why) || (p.paused == Some(Paused::Person) && why != Paused::Person) {
            return self.plan_detail(p).await;
        }
        p.state = PlanState::Paused;
        p.paused = Some(why);
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
        self.0.boss_plans.lock().failing.remove(&p.id);
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
    /// `why` saying so in the person's words; they continue it from the
    /// Floor.
    pub async fn pause_plan_failing(
        &self,
        id: Uuid,
        why: &str,
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        self.0
            .boss_plans
            .lock()
            .failing
            .insert(id, crew::one_line(why, 300));
        self.pause_plan(id, Paused::StartFailing, by).await
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
        for t in rows {
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
        if !plan::settled(&tickets) {
            return Err(DeskError::Conflict(
                "Some of the plan's tickets aren't done yet.".into(),
            ));
        }
        p.state = PlanState::Done;
        p.paused = None;
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
        let _one = self.0.plan_starting.lock().await;
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
        Ok(self
            .plan_launch(p, t, session, at, &runs, start.worktree, boss, by)
            .await?)
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
        self.plan_launch(&p, t, s.session, s.at, &runs, worktree, boss, by)
            .await
            .map(Some)
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
        runs: &Runs,
        worktree: bool,
        boss: &Party,
        by: &By,
    ) -> Result<Started, DeskError> {
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
                    };
                    (started, Some(v))
                }),
            Some(m) => match self
                .plan_spawn_on(&m, p, &t, since, runs, worktree, boss)
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
    /// saying its board is here.
    async fn plan_spawn_on(
        &self,
        machine: &str,
        p: &Plan,
        t: &Ticket,
        since: chrono::DateTime<chrono::Utc>,
        runs: &Runs,
        worktree: bool,
        boss: &Party,
    ) -> Result<Started, (DeskError, bool)> {
        let sure = |e: DeskError| (e, false);
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
        let read_refused = |e: crew::MachineRefused| match e {
            crew::MachineRefused::Refused(why) => sure(DeskError::Invalid(format!(
                "{machine}'s sessions couldn't be read: {why}"
            ))),
            e => sure(DeskError::Invalid(e.to_string())),
        };
        // Only a start that may have reached the machine is unsure.
        let refused = |e: crew::MachineRefused| match e {
            crew::MachineRefused::Refused(why) => sure(DeskError::Invalid(format!(
                "{machine} didn't start {key}: {why}"
            ))),
            e @ crew::MachineRefused::Unknown(_) => (DeskError::Invalid(e.to_string()), true),
            e => sure(DeskError::Invalid(e.to_string())),
        };
        let group = p.group.as_deref();
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
        let listed: Vec<crew::MachineSession> = serde_json::from_value(listed).map_err(|_| {
            sure(DeskError::Invalid(format!(
                "{machine}'s sessions couldn't be read."
            )))
        })?;
        let made_in = |cwd: &str| {
            cwd == folder
                || cwd.starts_with(&format!("{}/.worktrees/", folder.trim_end_matches('/')))
        };
        // Only one made for this start: an older session of the same key
        // (the ticket was sent back and started again) is never taken up.
        let after = since - chrono::Duration::minutes(2);
        if let Some(there) = listed
            .iter()
            .filter(|s| {
                s.name == key
                    && made_in(&s.cwd)
                    && !matches!(s.state.as_str(), "closed" | "stopped")
                    && s.created_at.is_none_or(|c| c >= after)
            })
            .max_by_key(|s| s.created_at)
        {
            return Ok(Started {
                id: there.id,
                machine: Some(machine.to_string()),
                name: there.label().to_string(),
            });
        }
        let prompt = format!(
            "{}\n\n{}",
            board::handed_text(&key, t, &needs),
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
        if let Some(model) = &runs.model {
            body["model"] = model.clone().into();
        }
        let made = crew
            .machine_call(machine, group, &boss.id, "desk.session_start", body)
            .await
            .map_err(refused)?;
        let made: crew::MachineSession = serde_json::from_value(made).map_err(|_| {
            // Started there, its answer unread: found there next time.
            (
                DeskError::Invalid(format!(
                    "{machine} started {key}, but its answer couldn't be read; Desk finds it there next time."
                )),
                true,
            )
        })?;
        Ok(Started {
            id: made.id,
            machine: Some(machine.to_string()),
            name: made.label().to_string(),
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
        if let Some(g) = p
            .group
            .clone()
            .filter(|g| self.layout().groups.iter().any(|x| &x.id == g))
        {
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
        // same folder and it is a repository: made once, and found again by
        // a start cut off after.
        let worktree = worktree && crate::worktree::in_repo(Path::new(&folder)).await;
        let (cwd, branch) = match worktree {
            false => (folder, None),
            true => {
                let branch = branch_for(&key);
                let made = crate::worktree::existing(Path::new(&folder), &branch).await;
                match made {
                    Some(path) => (path.to_string_lossy().into_owned(), None),
                    None => (folder, Some(branch)),
                }
            }
        };
        let group = p
            .group
            .clone()
            .filter(|g| self.layout().groups.iter().any(|x| &x.id == g));
        let new = NewSession {
            fresh_worktree: false,
            engine,
            view: runs.view,
            name: key.clone(),
            cwd,
            goal: board::goal_of(&key, t),
            worktree: branch,
            prompt: Some(board::handed_text(&key, t, &needs)),
            approvals: None,
            approve_for_me: false,
            // As its boss runs: an agent's `[llm] fast`.
            fast: self.fast_for(engine, boss),
            account: None,
            env: BTreeMap::new(),
            group,
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

/// `v`, a plan's ticket, as the plan's rules read it.
fn plan_ticket(v: &super::TicketView, runs: Option<&Runs>, starting: bool) -> PlanTicket {
    let r = runs.cloned().unwrap_or_default();
    PlanTicket {
        id: v.ticket.id,
        key: v.key.clone(),
        state: v.ticket.state,
        rank: v.ticket.rank,
        needs: v.ticket.needs.clone(),
        engine: r.engine,
        folder: r.folder,
        view: r.view,
        machine: r.machine,
        model: r.model,
        assigned: v.ticket.assignee.is_some(),
        starting,
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
            assigned: false,
            starting: false,
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

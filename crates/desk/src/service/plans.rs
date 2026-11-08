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
    self, Change, Draft, DraftTicket, Paused, Plan, PlanState, PlanTicket, Planned, Problem,
    Progress, Revision, Snapshot, Start,
};
use crate::plan_store::{BeginStart, Runs, Starting};

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
    /// Its session is being started.
    pub starting: bool,
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
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(n) = crew
            .and_then(|c| c.agent_name(&p.boss))
            .filter(|n| !n.trim().is_empty() && *n != p.boss)
        {
            return n;
        }
        let plans = self.0.boss_plans.lock();
        crew::shown_name(&p.boss, plans.names.get(&p.boss).map(String::as_str), true)
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
        Ok(PlanSummary {
            boss_name: self.plan_boss_name(&p),
            paused_why: self.paused_why(&p),
            progress: plan::progress(&tickets),
            plan: p,
        })
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
                PlanTicketView {
                    starting: starting.contains(&v.ticket.id),
                    engine: r.engine,
                    folder: r.folder,
                    view: r.view,
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
        Ok(PlanDetail {
            waves: plan::waves(&rows),
            summary: PlanSummary {
                boss_name: self.plan_boss_name(&p),
                paused_why: self.paused_why(&p),
                progress: plan::progress(&rows),
                plan: p,
            },
            tickets: shown,
            revisions,
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
        if by.party.kind == PartyKind::Manager && by.party.id == p.boss {
            self.0
                .boss_plans
                .lock()
                .names
                .insert(p.boss.clone(), by.party.name.clone());
        }
        // Which plan, for whoever watches Desk's events live.
        let mut e = by
            .event(kind, None, text)
            .detail(serde_json::json!({ "plan": p.id.to_string() }));
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
    async fn plan_board(&self, group: Option<&str>) -> Result<Board, DeskError> {
        let boards = self.boards().await?;
        if let Some(b) = boards.into_iter().find(|b| b.group.as_deref() == group) {
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
            if was.is_some_and(|w| (w.engine, &w.folder, w.view) != (t.engine, &t.folder, t.view)) {
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
        let waits = p.approved_revision.is_some() && !person && (asks || kind == Change::Scope);
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
        let _write = self.0.plan_write.lock().await;
        if by.party.kind != PartyKind::You {
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
        if p.approved_revision == Some(revision) {
            return self.plan_detail(p).await;
        }
        let next = self.newest(&p).await?;
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
        let text = match revision {
            1 => format!("approved the plan: {}", crew::one_line(&p.goal, 120)),
            n => format!("approved revision {n} of the plan"),
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
        self.plan_note(
            &p,
            by,
            Kind::PlanPaused,
            format!("paused the plan: {}", why.why()),
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
        let mut p = self.plan_of(id).await?;
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
        self.plan_note(
            &p,
            by,
            Kind::Continued,
            format!("resumed the plan, paused because {}", was.why()),
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
        by: &By,
    ) -> Result<PlanDetail, DeskError> {
        let p = self.plan_of(id).await?;
        if revision != p.revision {
            return Err(stale_plan(&p, &self.plan_boss_name(&p)));
        }
        match (p.awaiting(), allow, p.approved_revision) {
            (true, true, _) => self.approve_plan(id, revision, by).await,
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
    ) -> Result<SessionView, StartRefused> {
        let _one = self.0.plan_starting.lock().await;
        if let Some(ask) = self.starts_held(p.group.as_deref()) {
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
            .plan_launch(p, t, session, &runs, start.worktree, boss, by)
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
    ) -> Result<Option<SessionView>, DeskError> {
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
            return Ok(Some(view));
        }
        let plan_id = self.0.store.plan_of_ticket(t.id).await.map_err(internal)?;
        let p = match plan_id {
            Some(id) => Some(self.plan_of(id).await?),
            None => None,
        };
        let still = t.state == TicketState::Doing && t.assignee.is_none();
        let held = p
            .as_ref()
            .and_then(|p| self.starts_held(p.group.as_deref()))
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
        });
        self.plan_launch(&p, t, s.session, &runs, worktree, boss, by)
            .await
            .map(Some)
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
        runs: &Runs,
        worktree: bool,
        boss: &Party,
        by: &By,
    ) -> Result<SessionView, DeskError> {
        let s = Starting {
            ticket: t.id,
            session,
            at: crew::now(),
        };
        let started = self
            .plan_spawn(p, &t, session, runs, worktree, boss, by)
            .await;
        let view = match started {
            Ok(v) => v,
            Err(e) => {
                self.undo_start(&s).await;
                self.plan_touched(p.id).await;
                return Err(e);
            }
        };
        self.plan_link(t, view.clone(), by).await;
        self.end_start_of(&s).await;
        self.plan_touched(p.id).await;
        Ok(view)
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
            engine,
            view: runs.view,
            name: key.clone(),
            cwd,
            goal: board::goal_for(&key, &t.title),
            worktree: branch,
            prompt: Some(board::handed_text(&key, t, &needs)),
            approvals: None,
            approve_for_me: false,
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

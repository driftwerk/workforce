//! The hub checks every agreed goal on a schedule (DW-729,
//! designs/boss-keeps-goals.md §7): on 22–23 Nov each stall watch hung on
//! the boss arming it again, it armed its last at 00:03, and nothing
//! noticed the night go by.
//!
//! Each watchdog minute, off the watchdog's own loop and a few goals at a
//! time, every goal agreed and not yet met whose plan runs or is paused is
//! looked at, whatever its sessions are doing:
//!
//! - **Read when due.** Its measured parts are read as the person every
//!   [`CHECK_EVERY`] crew minutes, sooner when something moves: a minute
//!   after its last reading once a watch of its plan ends or a pull request
//!   it names changes, [`TURN_GAP_MINUTES`] after once one of its sessions
//!   ends a turn, and never within [`BACKOFF_MINUTES`] of a failed reading.
//!   A part GitHub is slow on is a failed reading. One that meets it closes
//!   it.
//! - **One wake a spell.** A part that hasn't come nearer its target for
//!   `stall_minutes` stalls; the stall furthest on wakes the boss's plan
//!   turn at one, two, four and eight times that, then every eight times
//!   more, each time with every part stalled then: what is stuck, its best,
//!   and what Desk already sees inside the plan's sessions. A boss with
//!   several goals stalling at once is woken in one turn. A part read
//!   failing [`FAILING_WAKE`] times in a row says why, once a run of
//!   failures. From the third spell the person's next roundup says it too.
//! - **A message that hasn't gone in.** The boss's message to one of its
//!   sessions still waiting after `stall_minutes` wakes it once: the session
//!   may not have its instructions.
//! - **Said once, a restart or not.** What its boss was told is kept with
//!   the plan ([`GoalSaid`]).
//! - **Only the boss.** A plan the person paused is read and wakes no one;
//!   a boss Desk's breaker holds is woken once the hold lifts, with the
//!   spell as it stands then. Nothing here makes a model call: a wake is
//!   the boss's plan turn.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::crew::{self, PartyKind};
use workforce_desk::goal::{self, GoalRefs, GoalSaid, Measure, PartProgress, Progress, Stall};
use workforce_desk::line::LineMessage;
use workforce_desk::plan::{Paused, Plan, PlanState};
use workforce_desk::sight::lasted;
use workforce_desk::PlanDetail;

use super::checkpoint::{readings_of, shows_what};
use super::github_seen::Pull;
use super::{AgentService, CrewHub};

/// How often, in crew minutes, a goal's measured parts are read.
pub(crate) const CHECK_EVERY: u32 = 10;
/// The fewest crew minutes between two readings, however much moves.
const READ_GAP_MINUTES: u32 = 1;
/// The fewest crew minutes between a reading and the one a session's ended
/// turn brings forward: turns end often, and a CI run takes longer.
const TURN_GAP_MINUTES: u32 = 5;
/// The crew minutes a reading waits after one that failed.
const BACKOFF_MINUTES: u32 = 5;
/// Failed reads in a row of a part before its boss is told why.
pub(crate) const FAILING_WAKE: u32 = 6;
/// The spell from which a stall is said in the person's roundup too.
const NOTE_SPELL: u32 = 2;
/// The most notes kept for one boss's roundup.
const NOTES_KEPT: usize = 20;
/// The most goals looked at once.
const GOALS_AT_ONCE: usize = 4;
/// The longest one look at every goal may take: one held up (a store that
/// doesn't answer) never keeps the next from starting.
const LOOK_FOR: std::time::Duration = std::time::Duration::from_secs(300);
/// What a stall's wake adds while its plan is paused at its budget.
const BUDGET_PAUSED: &str = "Its plan is paused at its budget: tell the person what is stuck and what it would take, rather than pushing its sessions on.";

/// What the goal checks keep between looks.
#[derive(Debug, Default)]
pub(crate) struct GoalChecks {
    /// The goals whose next reading something moved forward, with the
    /// fewest crew minutes after their last.
    pulled: HashMap<Uuid, u32>,
    /// Each goal's pull requests as GitHub last said, by plan, repository
    /// and number.
    pulls: HashMap<(Uuid, String, u64), Mark>,
    /// The sessions of the goals checked, as the last look found them;
    /// none before the first.
    sessions: Option<HashSet<String>>,
    /// What each boss's next roundup says of its stalled goals.
    notes: HashMap<String, Vec<GoalNote>>,
    /// Whether a look is under way: the next waits for it.
    looking: bool,
    /// The met plans whose closing turn was started.
    closing: HashSet<Uuid>,
}

/// What brought a goal's next reading forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pulled {
    /// Something it measures moved: a watch of its plan ended, or a pull
    /// request it names.
    Measured,
    /// One of its sessions ended a turn.
    TurnEnded,
}

impl Pulled {
    fn gap(self) -> u32 {
        match self {
            Self::Measured => READ_GAP_MINUTES,
            Self::TurnEnded => TURN_GAP_MINUTES,
        }
    }
}

/// What a goal's check notices of a pull request moving.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    open: bool,
    merged: bool,
    head: Option<String>,
    checks: Option<String>,
}

impl Mark {
    fn of(p: &Pull) -> Self {
        Self {
            open: p.open,
            merged: p.merged,
            head: p.head.clone(),
            checks: p.checks.clone(),
        }
    }
}

/// A stalled goal, as the person's roundup says it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalNote {
    plan: Uuid,
    at: DateTime<Utc>,
    text: String,
}

/// One goal looked at: whose, its words, its sessions, and what its boss
/// is woken with.
struct Looked {
    boss: String,
    plan: Uuid,
    goal: String,
    sessions: Vec<String>,
    lines: Vec<String>,
}

/// Whether a goal last read at `read_at` (never, none), `failed` whether
/// that reading failed, is read again at `now`: every [`CHECK_EVERY`] crew
/// `minute`s, or once `pulled` forward that many after the last reading,
/// [`BACKOFF_MINUTES`] at the least after a failed one.
pub(crate) fn due(
    read_at: Option<DateTime<Utc>>,
    failed: bool,
    pulled: Option<u32>,
    now: DateTime<Utc>,
    minute: Duration,
) -> bool {
    let Some(at) = read_at else {
        return true;
    };
    let after = |m: u32| now - at >= minute * m as i32;
    let backoff = match failed {
        true => BACKOFF_MINUTES,
        false => 0,
    };
    after(CHECK_EVERY) || pulled.is_some_and(|gap| after(gap.max(backoff)))
}

/// When `p`'s parts were last read, and whether any of those reads failed.
fn last_read(p: &Progress) -> (Option<DateTime<Utc>>, bool) {
    (
        p.parts
            .iter()
            .filter_map(|part| part.newest.as_ref().map(|r| r.at))
            .max(),
        p.parts.iter().any(|part| part.failing > 0),
    )
}

/// What the boss is told of part `part` of goal `goal` that hasn't come
/// nearer for `stalled`: its best, its target, what Desk sees inside the
/// plan's sessions (`seen`), and what to do, a checkpoint only as one the
/// goal would take (`shows`).
pub(crate) fn stall_line(
    goal: &str,
    part: &Measure,
    now: &PartProgress,
    stalled: Duration,
    tz: chrono_tz::Tz,
    seen: &[String],
    shows: &str,
) -> String {
    let hm = |at: DateTime<Utc>| at.with_timezone(&tz).format("%H:%M").to_string();
    let best = match (part.measured(), &now.best, now.improved_at) {
        (true, Some(b), _) => format!("best {} (at {})", b.words, hm(b.at)),
        (true, None, _) => "nothing read yet".to_string(),
        (false, _, Some(at)) => format!("last shown at {}", hm(at)),
        (false, _, None) => "nothing shown toward it yet".to_string(),
    };
    let seen = match seen.is_empty() {
        true => "none of its tickets has a session".to_string(),
        false => seen.join("; "),
    };
    let then = match part.measured() {
        true => "Get the work back onto the goal, or tell the person why it can't be.".to_string(),
        false => format!(
            "Judge it from the work (crew.inside): when it moves the goal on, record a checkpoint (crew.plan_update checkpoint) citing {shows}; else get the work back onto the goal, or tell the person why it can't be."
        ),
    };
    format!(
        "No progress toward “{}” on {} for {}: {best}, target {}. What Desk sees: {seen}. {then}",
        crew::one_line(goal, 160),
        part.label(),
        lasted(stalled),
        part.target(),
    )
}

/// What the boss is told of part `part` of plan `plan` that couldn't be
/// read `n` times in a row, the newest for `why`.
pub(crate) fn failing_line(plan: Uuid, part: &Measure, n: u32, why: &str) -> String {
    format!(
        "Desk can't read the goal's {} ({n} reads in a row): {}. Fix where it reads from (crew.plan_update plan_id {plan}, repair), or set up what it needs yourself (connections.setup).",
        part.label(),
        crew::one_line(why, 300)
    )
}

/// What the boss is told of its message to session `name` that hasn't
/// gone in for `waited`, for `why`.
pub(crate) fn undelivered_line(name: &str, waited: Duration, why: &str) -> String {
    format!(
        "Your message to {name} hasn't gone in for {} ({why}). It may not have its instructions: move what holds it, or tell the person.",
        lasted(waited)
    )
}

/// What the person's roundup says of goal `goal` with no progress for
/// `stalled`: when the boss last sent its sessions anything since, if it
/// did.
pub(crate) fn note_line(
    goal: &str,
    stalled: Duration,
    sent: Option<DateTime<Utc>>,
    tz: chrono_tz::Tz,
) -> String {
    let did = match sent {
        Some(at) => format!(
            "I last sent its sessions work at {}",
            at.with_timezone(&tz).format("%H:%M")
        ),
        None => "I haven't sent its sessions anything since".to_string(),
    };
    format!(
        "No progress on “{}” for {}; {did}.",
        crew::one_line(goal, 160),
        lasted(stalled)
    )
}

/// The lines of one boss's goals woken in one turn, `looked` in order: on
/// the first goal's plan, the others' said with their goal and plan.
fn one_turn(looked: Vec<Looked>) -> Option<(Uuid, Vec<String>)> {
    let mut woken = looked.into_iter().filter(|l| !l.lines.is_empty());
    let first = woken.next()?;
    let mut lines = first.lines;
    for other in woken {
        lines.extend(other.lines.into_iter().map(|l| {
            let goal = crew::one_line(&other.goal, 80);
            format!("On your goal “{goal}” (plan_id {}): {l}", other.plan)
        }));
    }
    Some((first.plan, lines))
}

/// The sessions of `p`'s tickets, by id (`machine/id` on a machine), with
/// what each is called: its ticket's key.
fn plan_sessions(p: &PlanDetail) -> Vec<(String, String)> {
    p.tickets
        .iter()
        .filter_map(|t| {
            let a = t.ticket.ticket.assignee.as_ref()?;
            (a.kind == PartyKind::Session).then(|| {
                let key = t.ticket.key.as_deref().unwrap_or("a ticket");
                (a.id.clone(), format!("{key}'s session"))
            })
        })
        .collect()
}

impl CrewHub {
    /// Looks at every agreed goal on its own task, once the look before
    /// has ended: GitHub slow never holds up the watchdog.
    pub(crate) fn spawn_goal_checks(&self) {
        let Some(crew) = self.me.upgrade() else {
            return;
        };
        if std::mem::replace(&mut self.goal_checks.lock().looking, true) {
            return;
        }
        /// Lets the next look start however this one ends.
        struct Looking(Arc<CrewHub>);
        impl Drop for Looking {
            fn drop(&mut self) {
                self.0.goal_checks.lock().looking = false;
            }
        }
        let looking = Looking(crew);
        tokio::spawn(async move {
            if tokio::time::timeout(LOOK_FOR, looking.0.check_goals())
                .await
                .is_err()
            {
                tracing::warn!("a look at the goals took too long; the next starts afresh");
            }
            drop(looking);
        });
    }

    /// The goals checked now, each with its boss and how it oversees:
    /// agreed by the person and approved, not met, running or paused, its
    /// boss watching its work.
    async fn goals_checked(&self) -> Option<Vec<(Plan, Arc<AgentService>, Oversight)>> {
        let goals = match self.desk.store().open_goals(None).await {
            Ok(g) => g,
            Err(e) => {
                tracing::info!(error = %e, "the open goals couldn't be read for their checks");
                return None;
            }
        };
        Some(
            goals
                .into_iter()
                .filter(|p| p.approved_revision.is_some() && p.agreed_at.is_some())
                .filter(|p| matches!(p.state, PlanState::Running | PlanState::Paused))
                .filter_map(|p| {
                    let (service, o) = self.overseer_named(&p.boss)?;
                    let watched = o.work.watch
                        && self.level_now(&p.boss, &o).0 != Level::Watch
                        && self.desk.seen_by(&o.scope).sees(p.group.as_deref());
                    watched.then_some((p, service, o))
                })
                .collect(),
        )
    }

    /// Every agreed goal not yet met, looked at once, a few at a time:
    /// read when due, met, or its boss woken in one turn for what stalls.
    async fn check_goals(&self) {
        let Some(goals) = self.goals_checked().await else {
            return;
        };
        let open: HashSet<Uuid> = goals.iter().map(|(p, ..)| p.id).collect();
        let mut waiting: HashMap<String, HashMap<String, Vec<LineMessage>>> = HashMap::new();
        for (p, ..) in &goals {
            if !waiting.contains_key(&p.boss) {
                let w = self.waiting_from(&p.boss).await;
                waiting.insert(p.boss.clone(), w);
            }
        }
        let order: Vec<Uuid> = goals.iter().map(|(p, ..)| p.id).collect();
        let waiting = &waiting;
        let mut looked: Vec<Looked> = futures::stream::iter(goals)
            .map(|(p, service, o)| async move {
                self.check_goal(&service, &o, p.id, &waiting[&p.boss]).await
            })
            .buffer_unordered(GOALS_AT_ONCE)
            .filter_map(std::future::ready)
            .collect()
            .await;
        looked.sort_by_key(|l| order.iter().position(|id| *id == l.plan));
        let sessions: HashSet<String> = looked
            .iter()
            .flat_map(|l| l.sessions.iter().cloned())
            .collect();
        {
            let mut c = self.goal_checks.lock();
            c.pulled.retain(|plan, _| open.contains(plan));
            c.pulls.retain(|(plan, ..), _| open.contains(plan));
            for notes in c.notes.values_mut() {
                notes.retain(|n| open.contains(&n.plan));
            }
            c.sessions = Some(sessions);
        }
        let mut by_boss: Vec<(String, Vec<Looked>)> = Vec::new();
        for l in looked {
            match by_boss.iter_mut().find(|(boss, _)| *boss == l.boss) {
                Some((_, all)) => all.push(l),
                None => by_boss.push((l.boss.clone(), vec![l])),
            }
        }
        for (_, all) in by_boss {
            if let Some((plan, lines)) = one_turn(all) {
                tracing::info!(plan = %plan, lines = lines.len(), "a goal's check woke its boss");
                self.wake_plan_with(plan, lines);
            }
        }
    }

    /// Goal plan `id` of `service`, the boss overseeing as `o`, looked at
    /// once, `waiting` its messages not yet gone into each session: none
    /// when it can't be read.
    async fn check_goal(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        id: Uuid,
        waiting: &HashMap<String, Vec<LineMessage>>,
    ) -> Option<Looked> {
        let detail = self.desk.plan(id).await.ok()?;
        let sessions = plan_sessions(&detail);
        let lines = self
            .goal_lines(service, o, detail.clone(), &sessions, waiting)
            .await;
        Some(Looked {
            boss: detail.summary.plan.boss,
            goal: detail.summary.plan.goal,
            plan: id,
            sessions: sessions.into_iter().map(|(id, _)| id).collect(),
            lines,
        })
    }

    /// What goal plan `detail` wakes its boss with now, its `sessions`
    /// with `waiting` messages: read first when due, met, or what stalls
    /// and isn't said yet, kept as said before it is.
    async fn goal_lines(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        detail: PlanDetail,
        sessions: &[(String, String)],
        waiting: &HashMap<String, Vec<LineMessage>>,
    ) -> Vec<String> {
        let Some(r) = detail.summary.plan.result.clone() else {
            return Vec::new();
        };
        let now = Utc::now();
        let minute = Duration::from_std(self.timing.minute).unwrap_or_default();
        let detail = self.read_when_due(service, detail, &r, now, minute).await;
        let id = detail.summary.plan.id;
        self.heads_kept(service, id, &GoalRefs::of(&r, &readings_of(&detail)))
            .await;
        let plan = &detail.summary.plan;
        if plan.met.is_some() || !matches!(plan.state, PlanState::Running | PlanState::Paused) {
            return Vec::new();
        }
        let progress = detail.goal.clone().unwrap_or_default();
        if goal::met(&progress) {
            let met = super::goal::met_by_reading(&r, &progress, now);
            if let Err(e) = self.goal_met(id, met).await {
                tracing::warn!(plan = %id, error = %e, "a goal read met wasn't closed");
            }
            return Vec::new();
        }
        // Read, and said to no one: the person paused it, or Desk's breaker
        // holds its boss, whose wake is said once the hold lifts.
        if plan.paused == Some(Paused::Person) || self.desk.agent_held(&plan.boss).is_some() {
            return Vec::new();
        }
        let said = match self.desk.store().goal_said(id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::info!(plan = %id, error = %e, "what a goal's boss was told couldn't be read");
                return Vec::new();
            }
        };
        let window = minute * o.work.stall_minutes as i32;
        let stalls = goal::stalls(
            &progress,
            plan.agreed_at.unwrap_or(plan.created_at),
            now,
            window,
        );
        let mut told = said.clone();
        let mut lines = self.stall_lines(o, &detail, &stalls, sessions, now, &mut told);
        lines.extend(self.undelivered_lines(plan, sessions, waiting, window, now, &mut told));
        let note = self
            .goal_note(service, &detail, &stalls, sessions, now, &mut told)
            .await;
        if told != said {
            if let Err(e) = self.desk.store().keep_goal_said(id, &told).await {
                tracing::warn!(plan = %id, error = %e, "what a goal's boss is told wasn't kept; it is said once it can be");
                return Vec::new();
            }
        }
        if let Some(note) = note {
            let mut c = self.goal_checks.lock();
            let notes = c.notes.entry(plan.boss.clone()).or_default();
            notes.push(note);
            let over = notes.len().saturating_sub(NOTES_KEPT);
            notes.drain(..over);
        }
        lines
    }

    /// Goal plan `detail`, measured by `r`, read now when it is due, as the
    /// plan is then.
    async fn read_when_due(
        &self,
        service: &AgentService,
        detail: PlanDetail,
        r: &goal::GoalResult,
        now: DateTime<Utc>,
        minute: Duration,
    ) -> PlanDetail {
        let id = detail.summary.plan.id;
        if self.goal_pulls_moved(id, &GoalRefs::of(r, &readings_of(&detail))) {
            self.pull_goal(id, Pulled::Measured);
        }
        let pulled = self.goal_checks.lock().pulled.get(&id).copied();
        let (read_at, failed) = last_read(detail.goal.as_ref().unwrap_or(&Progress::default()));
        if !r.parts.iter().any(Measure::measured) || !due(read_at, failed, pulled, now, minute) {
            return detail;
        }
        self.goal_checks.lock().pulled.remove(&id);
        self.read_kept(service, &detail).await.unwrap_or(detail)
    }

    /// What goal plan `detail`'s stalls (`stalls`) wake its boss with,
    /// noted in `told`: every stalled part once a spell of the leading
    /// stall, a part read failing [`FAILING_WAKE`] times in a row by why
    /// instead; and such a part once a run of failures besides.
    fn stall_lines(
        &self,
        o: &Oversight,
        detail: &PlanDetail,
        stalls: &[Stall],
        sessions: &[(String, String)],
        now: DateTime<Utc>,
        told: &mut GoalSaid,
    ) -> Vec<String> {
        let plan = &detail.summary.plan;
        let (Some(r), Some(progress)) = (plan.result.as_ref(), detail.goal.as_ref()) else {
            return Vec::new();
        };
        let failing: Vec<(u16, DateTime<Utc>, String)> = progress
            .parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.failing >= FAILING_WAKE)
            .filter_map(|(n, p)| {
                let why = p.newest.as_ref().and_then(|r| r.failed.as_deref());
                let since = p.failing_since.or(p.newest.as_ref().map(|r| r.at))?;
                let line = failing_line(
                    plan.id,
                    &r.parts[n],
                    p.failing,
                    why.unwrap_or("it couldn't be read"),
                );
                Some((u16::try_from(n).ok()?, since, line))
            })
            .collect();
        let mut out = Vec::new();
        if goal::wakes(stalls, told) {
            let tz = self.desk.timezone();
            let seen = self.seen_of(sessions, o, tz, now);
            let refs = GoalRefs::of(r, &readings_of(detail));
            let shows = shows_what(&refs, &self.work_repos(detail));
            for s in stalls {
                let n = usize::from(s.part);
                out.push(match failing.iter().find(|(part, ..)| *part == s.part) {
                    Some((_, since, line)) => {
                        told.failing.insert(s.part, *since);
                        line.clone()
                    }
                    None => stall_line(
                        &plan.goal,
                        &r.parts[n],
                        &progress.parts[n],
                        now - s.since,
                        tz,
                        &seen,
                        &shows,
                    ),
                });
            }
            told.tell(stalls);
            if plan.paused == Some(Paused::Budget) {
                out.push(BUDGET_PAUSED.to_string());
            }
        }
        for (part, since, line) in failing {
            if told.failing.get(&part) != Some(&since) {
                told.failing.insert(part, since);
                out.push(line);
            }
        }
        out
    }

    /// What Desk sees inside each of `sessions` from what it has read
    /// already: its glance in a line, or that it is gone.
    fn seen_of(
        &self,
        sessions: &[(String, String)],
        o: &Oversight,
        tz: chrono_tz::Tz,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        sessions
            .iter()
            .map(|(id, name)| match self.seen_inside(id, o) {
                Some((g, _)) if g.as_of.is_some() => format!("{name}: {}", g.line(tz, now)),
                Some(_) => format!("{name}: Desk hasn't looked inside it yet"),
                None => format!("{name}: gone"),
            })
            .collect()
    }

    /// What goal plan `plan`'s boss is told of its messages to the plan's
    /// `sessions` that have waited `window` and more without going in
    /// (`waiting`, by session, oldest first), each once, noted in `told`:
    /// whatever state the session is in, but never while the person is in
    /// it, took it back, or has questions waiting in it.
    fn undelivered_lines(
        &self,
        plan: &Plan,
        sessions: &[(String, String)],
        waiting: &HashMap<String, Vec<LineMessage>>,
        window: Duration,
        now: DateTime<Utc>,
        told: &mut GoalSaid,
    ) -> Vec<String> {
        sessions
            .iter()
            .filter_map(|(session, name)| {
                let held = waiting.get(session)?;
                let oldest = held.first()?;
                if now - oldest.at < window || told.undelivered.contains(&oldest.id) {
                    return None;
                }
                let why = match session.split_once('/') {
                    Some((machine, _)) => {
                        format!("it runs on {machine}, where Desk can't see why it waits")
                    }
                    None => {
                        let local: Uuid = session.parse().ok()?;
                        if self.taken_back(local) {
                            return None;
                        }
                        match self.desk.view(local).ok() {
                            Some(v) if self.person_has_questions(&v, &plan.boss) => return None,
                            Some(v) => self.why_waiting(&v, held)?.0,
                            None => "Desk can't reach it to put it in".to_string(),
                        }
                    }
                };
                told.undelivered_told(oldest.id);
                Some(undelivered_line(name, now - oldest.at, &why))
            })
            .collect()
    }

    /// The note the person's roundup gets once a spell of the leading
    /// stall from [`NOTE_SPELL`] on, for goal plan `detail` stalled as
    /// `stalls`, noted in `told`.
    async fn goal_note(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        stalls: &[Stall],
        sessions: &[(String, String)],
        now: DateTime<Utc>,
        told: &mut GoalSaid,
    ) -> Option<GoalNote> {
        let plan = &detail.summary.plan;
        let s = goal::leading(stalls).filter(|s| s.spell >= NOTE_SPELL)?;
        if told.noted == Some((s.since, s.spell)) {
            return None;
        }
        told.noted = Some((s.since, s.spell));
        let ids: Vec<Uuid> = sessions
            .iter()
            .filter_map(|(id, _)| id.parse().ok())
            .collect();
        let sent = self
            .desk
            .store()
            .line_sent_by(service.agent_id.as_str(), &ids, s.since)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(_, at)| at)
            .max();
        Some(GoalNote {
            plan: plan.id,
            at: now,
            text: note_line(&plan.goal, now - s.since, sent, self.desk.timezone()),
        })
    }

    /// Plan `id`, its goal met for `why`, closed by its boss's closing
    /// turn, run once on its own.
    pub(crate) fn close_met(&self, id: Uuid, why: &str) {
        if !self.closing_started(id) {
            return;
        }
        let Some(crew) = self.me.upgrade() else {
            return;
        };
        let line = format!("The goal is met: {}", crew::one_line(why, 200));
        tokio::spawn(async move { crew.plan_turn(id, vec![line]).await });
    }

    /// Notes plan `id`'s closing turn as started: true the first time, so
    /// that it is run once whichever way its goal was met.
    pub(crate) fn closing_started(&self, id: Uuid) -> bool {
        self.goal_checks.lock().closing.insert(id)
    }

    /// Whether a pull request goal plan `id` names (`refs`) moved since
    /// its check last looked, as GitHub was read for anything else.
    fn goal_pulls_moved(&self, id: Uuid, refs: &GoalRefs) -> bool {
        let marks: Vec<((Uuid, String, u64), Mark)> = {
            let seen = self.github_seen.lock();
            refs.pulls
                .iter()
                .filter_map(|(repo, n)| {
                    let p = seen.pull_kept(repo, *n)?;
                    Some(((id, repo.clone(), *n), Mark::of(&p)))
                })
                .collect()
        };
        let mut c = self.goal_checks.lock();
        marks.into_iter().fold(false, |moved, (key, mark)| {
            let was = c.pulls.insert(key, mark.clone());
            moved || was.is_some_and(|w| w != mark)
        })
    }

    /// Plan `id`'s goal is read at its next look, as soon after its last
    /// reading as `why` allows: something it measures may have moved.
    pub(crate) fn pull_goal(&self, id: Uuid, why: Pulled) {
        let mut c = self.goal_checks.lock();
        let gap = c.pulled.entry(id).or_insert(why.gap());
        *gap = (*gap).min(why.gap());
    }

    /// The sessions on agreed goals whose checks say their waiting
    /// messages, as the last look found them: looked up now before the
    /// first.
    pub(crate) async fn goal_checked_sessions(&self) -> HashSet<String> {
        if let Some(s) = self.goal_checks.lock().sessions.clone() {
            return s;
        }
        let mut sessions = HashSet::new();
        for (p, ..) in self.goals_checked().await.unwrap_or_default() {
            if let Ok(detail) = self.desk.plan(p.id).await {
                sessions.extend(plan_sessions(&detail).into_iter().map(|(id, _)| id));
            }
        }
        self.goal_checks
            .lock()
            .sessions
            .get_or_insert(sessions)
            .clone()
    }

    /// Whether `boss`, overseeing as `o`, keeps work its unbound sweep's
    /// net is for: an agreed goal open, a plan running, or a session
    /// running in its scope.
    pub(crate) async fn keeps_work(&self, boss: &str, o: &Oversight) -> bool {
        let seen = self.desk.seen_by(&o.scope);
        if self
            .desk
            .list()
            .iter()
            .any(|v| v.state.is_running() && seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
        {
            return true;
        }
        let goals = self
            .desk
            .store()
            .open_goals(Some(boss))
            .await
            .is_ok_and(|g| g.iter().any(|p| p.approved_revision.is_some()));
        goals
            || self
                .desk
                .plans(Some(PlanState::Running), None)
                .await
                .is_ok_and(|p| p.iter().any(|s| s.plan.boss == boss))
    }

    /// The notes on `boss`'s stalled goals its next roundup says, noted by
    /// `cutoff`.
    pub(crate) fn goal_notes(&self, boss: &str, cutoff: DateTime<Utc>) -> Vec<String> {
        self.goal_checks
            .lock()
            .notes
            .get(boss)
            .map(|n| {
                n.iter()
                    .filter(|note| note.at <= cutoff)
                    .map(|note| note.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `boss`'s roundup said its goal notes noted by `cutoff`: let go.
    pub(crate) fn goal_notes_told(&self, boss: &str, cutoff: DateTime<Utc>) {
        if let Some(n) = self.goal_checks.lock().notes.get_mut(boss) {
            n.retain(|note| note.at > cutoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use workforce_desk::goal::{GoalResult, Per, Reading, Want};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, 23, 0, 0).unwrap() + Duration::minutes(minute)
    }

    fn ci() -> Measure {
        Measure::Ci {
            repo: "acme/mantis".into(),
            workflow: None,
            branch: "main".into(),
            want: Want::Minutes {
                per: Per::Job,
                at_most: 5.0,
                runs: 3,
            },
        }
    }

    #[test]
    fn a_goal_is_read_every_ten_minutes_and_sooner_when_something_moved() {
        let m = Duration::minutes(1);
        let moved = Some(Pulled::Measured.gap());
        let turn = Some(Pulled::TurnEnded.gap());
        assert!(due(None, false, None, at(0), m), "never read");
        assert!(!due(Some(at(0)), false, None, at(9), m));
        assert!(due(Some(at(0)), false, None, at(10), m));
        assert!(
            !due(Some(at(0)), false, moved, at(0), m),
            "not within a minute"
        );
        assert!(due(Some(at(0)), false, moved, at(1), m));
        assert!(
            !due(Some(at(0)), false, turn, at(4), m),
            "a turn's end waits longer"
        );
        assert!(due(Some(at(0)), false, turn, at(5), m));
        assert!(
            !due(Some(at(0)), true, moved, at(4), m),
            "backing off a failed read"
        );
        assert!(due(Some(at(0)), true, moved, at(5), m));
        assert!(due(Some(at(0)), true, None, at(10), m));
    }

    #[test]
    fn the_last_read_is_the_newest_of_any_part_and_says_a_failure() {
        let read = |minute: i64, value: Option<f64>| PartProgress {
            newest: Some(match value {
                Some(v) => Reading {
                    value: Some(v),
                    ..Reading::failed(0, at(minute), "")
                },
                None => Reading::failed(0, at(minute), "slow"),
            }),
            failing: u32::from(value.is_none()),
            ..Default::default()
        };
        let p = Progress {
            parts: vec![read(3, Some(9.0)), read(7, None), PartProgress::default()],
        };
        assert_eq!(last_read(&p), (Some(at(7)), true));
        assert_eq!(last_read(&Progress::default()), (None, false));
    }

    #[test]
    fn a_stall_says_what_is_stuck_its_best_its_target_and_what_desk_sees() {
        let tz = chrono_tz::UTC;
        let now = PartProgress {
            best: Some(Reading {
                value: Some(22.0),
                words: "22 minutes, the longest job".into(),
                failed: None,
                ..Reading::failed(0, at(0), "")
            }),
            ..Default::default()
        };
        let refs = GoalRefs::of(
            &GoalResult {
                words: "CI fast".into(),
                parts: vec![ci()],
            },
            &[],
        );
        let line = stall_line(
            "Mantis CI tests only what a change touches",
            &ci(),
            &now,
            Duration::minutes(90),
            tz,
            &["MAN-768's session: nothing runs under it".into()],
            "",
        );
        assert_eq!(
            line,
            "No progress toward “Mantis CI tests only what a change touches” on CI on main of \
             acme/mantis for 1 h 30 min: best 22 minutes, the longest job (at 23:00), target at \
             most 5 minutes a job, 3 runs in a row. What Desk sees: MAN-768's session: nothing \
             runs under it. Get the work back onto the goal, or tell the person why it can't be."
        );
        let judged = Measure::Judged {
            evidence: "only the touched services run".into(),
        };
        let stalled = |refs: &GoalRefs| {
            let repos = std::collections::BTreeSet::from(["acme/mantis".to_string()]);
            stall_line(
                "g",
                &judged,
                &PartProgress::default(),
                Duration::minutes(30),
                tz,
                &[],
                &shows_what(refs, &repos),
            )
        };
        let line = stalled(&refs);
        assert!(line.contains("nothing shown toward it yet"), "{line}");
        assert!(line.contains("none of its tickets has a session"), "{line}");
        assert!(
            line.contains("crew.plan_update checkpoint) citing commits, runs or a pull request on the goal's work (main of acme/mantis)"),
            "{line}"
        );
        let line = stalled(&GoalRefs::default());
        assert!(
            line.contains("citing a pull request or run since the goal was agreed, in the plan's repositories (acme/mantis)"),
            "{line}"
        );
    }

    #[test]
    fn a_boss_s_goals_stalling_at_once_wake_it_in_one_turn() {
        let looked = |plan: u128, lines: &[&str]| Looked {
            boss: "desk".into(),
            plan: Uuid::from_u128(plan),
            goal: format!("Goal {plan}"),
            sessions: Vec::new(),
            lines: lines.iter().map(|l| l.to_string()).collect(),
        };
        assert_eq!(one_turn(vec![looked(1, &[])]), None);
        let (plan, lines) = one_turn(vec![
            looked(1, &[]),
            looked(2, &["CI is stuck."]),
            looked(3, &["Docs are stuck.", "A message waits."]),
        ])
        .unwrap();
        assert_eq!(plan, Uuid::from_u128(2));
        assert_eq!(
            lines,
            [
                "CI is stuck.".to_string(),
                format!(
                    "On your goal “Goal 3” (plan_id {}): Docs are stuck.",
                    Uuid::from_u128(3)
                ),
                format!(
                    "On your goal “Goal 3” (plan_id {}): A message waits.",
                    Uuid::from_u128(3)
                ),
            ]
        );
    }

    #[test]
    fn the_other_lines_say_why_and_what_to_do_in_one_go() {
        let plan = Uuid::nil();
        let line = failing_line(plan, &ci(), 6, "GitHub answered 502");
        assert!(
            line.contains("(6 reads in a row): GitHub answered 502"),
            "{line}"
        );
        assert!(line.contains(&format!("plan_id {plan}, repair")), "{line}");
        assert_eq!(
            undelivered_line(
                "MAN-768's session",
                Duration::minutes(45),
                "its turn hasn't ended"
            ),
            "Your message to MAN-768's session hasn't gone in for 45 min (its turn hasn't ended). \
             It may not have its instructions: move what holds it, or tell the person."
        );
        let tz = chrono_tz::UTC;
        assert_eq!(
            note_line("CI fast", Duration::minutes(120), Some(at(70)), tz),
            "No progress on “CI fast” for 2 h; I last sent its sessions work at 00:10."
        );
        assert_eq!(
            note_line("CI fast", Duration::minutes(120), None, tz),
            "No progress on “CI fast” for 2 h; I haven't sent its sessions anything since."
        );
    }
}

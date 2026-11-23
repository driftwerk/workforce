//! The hub checks every agreed goal on a schedule (DW-729,
//! designs/boss-keeps-goals.md §7): on 22–23 Nov each stall watch hung on
//! the boss arming it again, it armed its last at 00:03, and nothing
//! noticed the night go by.
//!
//! Each watchdog minute, every goal agreed and not yet met whose plan runs
//! or is paused is looked at, whatever its sessions are doing:
//!
//! - **Read when due.** Its measured parts are read as the person every
//!   [`CHECK_EVERY`] crew minutes, sooner when something moves (a watch of
//!   its plan ended, a pull request it names changed as GitHub was read for
//!   anything else, one of its sessions ended a turn), never within a
//!   minute of the last reading, nor within [`BACKOFF_MINUTES`] of a failed
//!   one. A reading that meets it closes it.
//! - **Each part on its own.** A part that hasn't come nearer its target
//!   for `stall_minutes` wakes the boss's plan turn, then again at twice,
//!   four and eight times that, and every eight after: what is stuck, its
//!   best, and what Desk sees inside its sessions, from what it has read
//!   already. A part read failing [`FAILING_WAKE`] times in a row says why
//!   instead. From the third spell the person's next roundup says it too.
//! - **A message that hasn't gone in.** The boss's message to one of its
//!   sessions still waiting after `stall_minutes` wakes it once: the
//!   session may not have its instructions.
//! - **Only the boss.** A plan the person paused is read and wakes no one;
//!   a boss Desk's breaker holds is woken once the hold lifts, with the
//!   spell as it stands then. Nothing here makes a model call: a wake is
//!   the boss's plan turn.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::crew::{self, PartyKind};
use workforce_desk::goal::{
    self, Evidence, FactKind, Found, GoalRefs, Measure, NotShown, PartProgress, Progress, Stall,
};
use workforce_desk::line::LineMessage;
use workforce_desk::plan::{Paused, Plan, PlanState};
use workforce_desk::sight::lasted;
use workforce_desk::PlanDetail;

use super::github::{calls_as_person, GithubAccess};
use super::github_seen::Pull;
use super::{AgentService, CrewHub};

/// How often, in crew minutes, a goal's measured parts are read.
pub(crate) const CHECK_EVERY: u32 = 10;
/// The fewest crew minutes between two readings, however much moves.
const READ_GAP_MINUTES: u32 = 1;
/// The crew minutes a reading waits after one that failed.
const BACKOFF_MINUTES: u32 = 5;
/// Failed reads in a row of a part before its boss is told why.
pub(crate) const FAILING_WAKE: u32 = 6;
/// The spell from which a stall is said in the person's roundup too.
const NOTE_SPELL: u32 = 2;
/// The most notes kept for one boss's roundup.
const NOTES_KEPT: usize = 20;

/// What the goal checks keep between looks.
#[derive(Debug, Default)]
pub(crate) struct GoalChecks {
    /// The goals whose next reading something moved forward.
    pulled: HashSet<Uuid>,
    /// Each goal's pull requests as GitHub last said, by plan, repository
    /// and number.
    pulls: HashMap<(Uuid, String, u64), Mark>,
    /// The sessions of the agreed goals, as the last look found them.
    sessions: HashSet<String>,
    /// What each boss's next roundup says of its stalled goals.
    notes: HashMap<String, Vec<GoalNote>>,
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
pub(crate) struct GoalNote {
    pub at: DateTime<Utc>,
    pub text: String,
}

/// Whether a goal last read at `read_at` (never, none), `failed` whether
/// that reading failed, is read again at `now`: every [`CHECK_EVERY`]
/// crew `minute`s, or once `pulled` forward a minute after the last
/// reading, [`BACKOFF_MINUTES`] after a failed one.
pub(crate) fn due(
    read_at: Option<DateTime<Utc>>,
    failed: bool,
    pulled: bool,
    now: DateTime<Utc>,
    minute: Duration,
) -> bool {
    let Some(at) = read_at else {
        return true;
    };
    let gap = match failed {
        true => BACKOFF_MINUTES,
        false => READ_GAP_MINUTES,
    };
    let since = now - at;
    since >= minute * CHECK_EVERY as i32 || pulled && since >= minute * gap as i32
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
/// plan's sessions (`seen`), and what to do.
pub(crate) fn stall_line(
    goal: &str,
    part: &Measure,
    now: &PartProgress,
    stalled: Duration,
    tz: chrono_tz::Tz,
    seen: &[String],
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
        true => "Get the work back onto the goal, or tell the person why it can't be.",
        false => "Judge it from the work (crew.inside): when it moves the goal on, record a checkpoint with the commits, runs or pull request that show it (crew.plan_update checkpoint); else get the work back onto the goal, or tell the person why it can't be.",
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

/// Whether `r` is a branch or commit as GitHub's compare takes it in its
/// path: git's own characters alone, nothing that steers the request
/// elsewhere.
pub(crate) fn plain_ref(r: &str) -> bool {
    !r.is_empty()
        && !r.starts_with(['/', '-', '.'])
        && !r.ends_with(['/', '.'])
        && !r.contains("..")
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Whether `sha` is a commit's id, whole or shortened.
fn is_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// `e` in words, as a refusal names it.
fn evidence_words(e: &Evidence) -> String {
    match e {
        Evidence::Run { repo, id } => format!("Run {id} of {repo}"),
        Evidence::Pull { repo, number } => format!("PR #{number} of {repo}"),
        Evidence::Commit { repo, sha } => format!("Commit {} of {repo}", &sha[..sha.len().min(7)]),
        Evidence::Ticket { key } => format!("Ticket {key}"),
    }
}

/// What the boss is told of a checkpoint that doesn't show its judged
/// part moving, `why`.
pub(crate) fn not_shown_line(why: &NotShown) -> String {
    match why {
        NotShown::Nothing => "A checkpoint shows what moved the goal on: name the commits, runs or pull request that show it (evidence).".into(),
        NotShown::Missing(e) => format!(
            "{} isn't on GitHub as it says, or couldn't be looked up: check it, and record the checkpoint again.",
            evidence_words(e)
        ),
        NotShown::OffGoal(e) => format!(
            "{} isn't the goal's own work: only commits on its branches or its pull request, that pull request, and runs on those branches show a judged part moving. Get the work back onto the goal.",
            evidence_words(e)
        ),
        NotShown::NothingNew => "All it shows was shown toward this part already: record a checkpoint once something new moves it on.".into(),
    }
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
    /// Every agreed goal not yet met, looked at once: read when due, met,
    /// or its boss woken for what stalls.
    pub(crate) async fn check_goals(&self) {
        let goals: Vec<Plan> = match self.desk.store().open_goals(None).await {
            Ok(g) => g
                .into_iter()
                .filter(|p| p.approved_revision.is_some())
                .filter(|p| matches!(p.state, PlanState::Running | PlanState::Paused))
                .collect(),
            Err(e) => {
                tracing::info!(error = %e, "the open goals couldn't be read for their checks");
                return;
            }
        };
        let open: HashSet<Uuid> = goals.iter().map(|p| p.id).collect();
        let mut sessions = HashSet::new();
        let mut waiting: HashMap<String, HashMap<String, Vec<LineMessage>>> = HashMap::new();
        for p in goals {
            let Some((service, o)) = self.overseer_named(&p.boss) else {
                continue;
            };
            if !o.work.watch
                || self.level_now(&p.boss, &o).0 == Level::Watch
                || !self.desk.seen_by(&o.scope).sees(p.group.as_deref())
            {
                continue;
            }
            let Ok(detail) = self.desk.plan(p.id).await else {
                continue;
            };
            sessions.extend(plan_sessions(&detail).into_iter().map(|(id, _)| id));
            if !waiting.contains_key(&p.boss) {
                let w = self.waiting_from(&p.boss).await;
                waiting.insert(p.boss.clone(), w);
            }
            self.check_goal(&service, &o, detail, &waiting[&p.boss])
                .await;
        }
        let mut c = self.goal_checks.lock();
        c.pulled.retain(|plan| open.contains(plan));
        c.pulls.retain(|(plan, ..), _| open.contains(plan));
        c.sessions = sessions;
    }

    /// Goal plan `detail` of `service`, the boss overseeing as `o`, looked
    /// at once, `waiting` its messages not yet gone into each session.
    async fn check_goal(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        detail: PlanDetail,
        waiting: &HashMap<String, Vec<LineMessage>>,
    ) {
        let id = detail.summary.plan.id;
        let Some(r) = detail.summary.plan.result.clone() else {
            return;
        };
        let now = Utc::now();
        let minute = Duration::from_std(self.timing.minute).unwrap_or_default();
        let progress = detail.goal.clone().unwrap_or_default();
        let read: Vec<goal::Reading> = progress
            .parts
            .iter()
            .filter_map(|p| p.newest.clone())
            .collect();
        let moved = self.goal_pulls_moved(id, &GoalRefs::of(&r, &read));
        let pulled = {
            let mut c = self.goal_checks.lock();
            if moved {
                c.pulled.insert(id);
            }
            c.pulled.contains(&id)
        };
        let (read_at, failed) = last_read(&progress);
        let detail = match r.parts.iter().any(Measure::measured)
            && due(read_at, failed, pulled, now, minute)
        {
            true => {
                self.goal_checks.lock().pulled.remove(&id);
                self.read_kept(service, &detail).await.unwrap_or(detail)
            }
            false => detail,
        };
        let plan = &detail.summary.plan;
        if plan.met.is_some() || !matches!(plan.state, PlanState::Running | PlanState::Paused) {
            return;
        }
        let progress = detail.goal.clone().unwrap_or_default();
        if goal::met(&progress) {
            let met = super::goal::met_by_reading(&r, &progress, now);
            if let Err(e) = self.goal_met(id, met).await {
                tracing::warn!(plan = %id, error = %e, "a goal read met wasn't closed");
            }
            return;
        }
        // Read, and said to no one: the person paused it, or Desk's breaker
        // holds its boss, whose wake is said once the hold lifts.
        if plan.paused == Some(Paused::Person) || self.desk.agent_held(&plan.boss).is_some() {
            return;
        }
        let window = minute * o.work.stall_minutes as i32;
        let stalls = goal::stalls(
            &progress,
            plan.agreed_at.unwrap_or(plan.created_at),
            now,
            window,
        );
        let sessions = plan_sessions(&detail);
        let mut lines = self.stall_lines(o, &detail, &r, &progress, &stalls, &sessions, now);
        lines.extend(self.undelivered_lines(plan, &sessions, waiting, window, now));
        if let Some(note) = self
            .goal_note(service, &detail, &stalls, &sessions, now)
            .await
        {
            let mut c = self.goal_checks.lock();
            let notes = c.notes.entry(plan.boss.clone()).or_default();
            notes.push(note);
            let over = notes.len().saturating_sub(NOTES_KEPT);
            notes.drain(..over);
        }
        if !lines.is_empty() {
            tracing::info!(plan = %id, lines = lines.len(), "a goal's check woke its boss");
            self.wake_plan_with(id, lines);
        }
    }

    /// What goal plan `detail`'s stalls (`stalls`, of `r` standing as
    /// `progress`) tell its boss, each once a spell: a part read failing
    /// [`FAILING_WAKE`] times in a row says why, once as it gets there
    /// and once each spell after.
    #[allow(clippy::too_many_arguments)]
    fn stall_lines(
        &self,
        o: &Oversight,
        detail: &PlanDetail,
        r: &goal::GoalResult,
        progress: &Progress,
        stalls: &[Stall],
        sessions: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Vec<String> {
        let id = detail.summary.plan.id;
        let tz = self.desk.timezone();
        let failing = |n: usize| {
            progress
                .parts
                .get(n)
                .filter(|p| p.failing >= FAILING_WAKE)
                .map(|p| {
                    let why = p
                        .newest
                        .as_ref()
                        .and_then(|r| r.failed.as_deref())
                        .unwrap_or("it couldn't be read");
                    (
                        p.failing_since,
                        failing_line(id, &r.parts[n], p.failing, why),
                    )
                })
        };
        let mut said = self.plan_watch_said();
        let mut out = Vec::new();
        for (n, part) in r.parts.iter().enumerate() {
            let stall = stalls.iter().find(|s| usize::from(s.part) == n);
            let line = match (stall, failing(n)) {
                (Some(s), Some((_, line))) => said(format!(
                    "goal-stall:{id}:{n}:{}:{}",
                    s.since.timestamp_millis(),
                    s.spell
                ))
                .then_some(line),
                (None, Some((since, line))) => said(format!(
                    "goal-failing:{id}:{n}:{}",
                    since.map(|s| s.timestamp_millis()).unwrap_or_default()
                ))
                .then_some(line),
                (Some(s), None) => said(format!(
                    "goal-stall:{id}:{n}:{}:{}",
                    s.since.timestamp_millis(),
                    s.spell
                ))
                .then(|| {
                    let seen = self.seen_of(sessions, o, tz, now);
                    stall_line(
                        &detail.summary.plan.goal,
                        part,
                        &progress.parts[n],
                        now - s.since,
                        tz,
                        &seen,
                    )
                }),
                (None, None) => None,
            };
            out.extend(line);
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
    /// (`waiting`, by session, oldest first): once each, whatever state the
    /// session is in, but never while the person is in it, took it back, or
    /// has questions waiting in it.
    fn undelivered_lines(
        &self,
        plan: &Plan,
        sessions: &[(String, String)],
        waiting: &HashMap<String, Vec<LineMessage>>,
        window: Duration,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        let mut said = self.plan_watch_said();
        sessions
            .iter()
            .filter_map(|(session, name)| {
                let oldest = waiting.get(session)?.first()?;
                if now - oldest.at < window {
                    return None;
                }
                let local = session.parse::<Uuid>().ok();
                if local.is_some_and(|s| self.taken_back(s)) {
                    return None;
                }
                let why = match local.and_then(|s| self.desk.view(s).ok()) {
                    Some(v) if self.person_has_questions(&v, &plan.boss) => return None,
                    Some(v) => self.why_waiting(&v, &waiting[session])?.0,
                    None => "Desk can't reach it to put it in".to_string(),
                };
                said(format!("goal-undelivered:{}:{}", plan.id, oldest.id))
                    .then(|| undelivered_line(name, now - oldest.at, &why))
            })
            .collect()
    }

    /// The note the person's roundup gets once a spell from
    /// [`NOTE_SPELL`] on, for goal plan `detail` stalled as `stalls`.
    async fn goal_note(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        stalls: &[Stall],
        sessions: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Option<GoalNote> {
        let plan = &detail.summary.plan;
        let s = stalls
            .iter()
            .filter(|s| s.spell >= NOTE_SPELL)
            .min_by_key(|s| s.since)?;
        let key = format!(
            "goal-note:{}:{}:{}",
            plan.id,
            s.since.timestamp_millis(),
            s.spell
        );
        if !self.plan_watch_said()(key) {
            return None;
        }
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
            at: now,
            text: note_line(&plan.goal, now - s.since, sent, self.desk.timezone()),
        })
    }

    /// Whether a checkpoint showing `evidence` toward judged part `part` of
    /// goal plan `detail`, its result `r`, moves it on (designs/boss-keeps-goals.md §7.3),
    /// looked up on GitHub as the person: refused in the boss's words when
    /// it doesn't.
    pub(super) async fn checkpoint_shown(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        r: &goal::GoalResult,
        part: u16,
        evidence: &[Evidence],
    ) -> Result<(), String> {
        let read: Vec<goal::Reading> = detail
            .goal
            .iter()
            .flat_map(|p| &p.parts)
            .flat_map(|p| p.newest.iter().chain(&p.best))
            .cloned()
            .collect();
        let refs = GoalRefs::of(r, &read);
        let earlier: Vec<&Evidence> = detail
            .facts
            .iter()
            .filter(|f| f.kind == FactKind::Checkpoint && f.checked && f.part == Some(part))
            .flat_map(|f| &f.evidence)
            .collect();
        let access = super::goal::access(service);
        let found = self
            .as_person(service, found_on_goal(&access, refs, evidence))
            .await;
        goal::checkpoint_shows(evidence, &found, &earlier).map_err(|why| not_shown_line(&why))
    }

    /// Notes `key` as said by the watchdog: true the first time.
    fn plan_watch_said(&self) -> impl FnMut(String) -> bool + '_ {
        move |key| self.plan_watch.lock().said(key)
    }

    /// Whether a pull request goal plan `id` names (`refs`) moved since
    /// its check last looked, as GitHub was read for anything else.
    fn goal_pulls_moved(&self, id: Uuid, refs: &GoalRefs) -> bool {
        let seen = self.github_seen.lock();
        let marks: Vec<((Uuid, String, u64), Mark)> = refs
            .pulls
            .iter()
            .filter_map(|(repo, n)| {
                let p = seen.pull_kept(repo, *n)?;
                Some(((id, repo.clone(), *n), Mark::of(&p)))
            })
            .collect();
        drop(seen);
        let mut c = self.goal_checks.lock();
        marks.into_iter().fold(false, |moved, (key, mark)| {
            let was = c.pulls.insert(key, mark.clone());
            moved || was.is_some_and(|w| w != mark)
        })
    }

    /// Plan `id`'s goal is read at its next look, a minute after its last
    /// reading at the soonest: something it measures may have moved.
    pub(crate) fn pull_goal(&self, id: Uuid) {
        self.goal_checks.lock().pulled.insert(id);
    }

    /// Whether session `id` (or `machine/id`) is on an agreed goal, as the
    /// last look found it: its goal's check says its messages that wait.
    pub(crate) fn on_goal(&self, id: &str) -> bool {
        self.goal_checks.lock().sessions.contains(id)
    }

    /// Whether `boss` has an agreed goal open or a plan running: the work
    /// its unbound sweep's net is for.
    pub(crate) async fn keeps_work(&self, boss: &str) -> bool {
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

/// What GitHub answers `call` with `args` as the person in scope: none
/// when it can't be read.
async fn github_one(access: &GithubAccess<'_>, call: &str, args: Value) -> Option<Value> {
    calls_as_person(access, vec![(call, args)])
        .await
        .ok()
        .and_then(|(_, mut got)| got.pop())
}

/// What is found of each of `evidence` on GitHub, as the person in scope:
/// whether it is there, and the goal's own work by `refs`, with the head
/// branches of the goal's pull requests counted as its branches.
async fn found_on_goal(
    access: &GithubAccess<'_>,
    mut refs: GoalRefs,
    evidence: &[Evidence],
) -> Vec<Found> {
    for (repo, n) in refs.pulls.clone() {
        let Some((owner, name)) = goal::github_repo(&repo) else {
            continue;
        };
        let head = github_one(
            access,
            "github.get_pull",
            json!({ "owner": owner, "repo": name, "number": n }),
        )
        .await
        .and_then(|p| p["head"].as_str().map(str::to_string));
        if let Some(head) = head {
            refs.branches.insert((repo.clone(), head));
        }
    }
    let mut out = Vec::with_capacity(evidence.len());
    for e in evidence {
        out.push(found_of(access, &refs, e).await);
    }
    out
}

/// What is found of `e` on GitHub against the goal's `refs`.
async fn found_of(access: &GithubAccess<'_>, refs: &GoalRefs, e: &Evidence) -> Found {
    let on = |yes: bool| match yes {
        true => Found::OnGoal,
        false => Found::OffGoal,
    };
    let Some(repo) = e.repo() else {
        return Found::OffGoal;
    };
    let Some((owner, name)) = goal::github_repo(repo) else {
        return Found::Missing;
    };
    match e {
        Evidence::Pull { number, .. } => github_one(
            access,
            "github.get_pull",
            json!({ "owner": owner, "repo": name, "number": number }),
        )
        .await
        .map_or(Found::Missing, |_| on(refs.names_pull(repo, *number))),
        Evidence::Run { id, .. } => github_one(
            access,
            "github.workflow_run_logs",
            json!({ "owner": owner, "repo": name, "run_id": id }),
        )
        .await
        .and_then(|got| got["run"]["head_branch"].as_str().map(str::to_string))
        .map_or(
            Found::Missing,
            |branch| on(refs.names_branch(repo, &branch)),
        ),
        Evidence::Commit { sha, .. } if !is_sha(sha) => Found::Missing,
        Evidence::Commit { sha, .. } => {
            let mut answered = false;
            for branch in refs.branches_of(repo).filter(|b| plain_ref(b)) {
                let compared = github_one(
                    access,
                    "github.compare",
                    json!({ "owner": owner, "repo": name, "base": sha, "head": branch }),
                )
                .await;
                if let Some(got) = compared {
                    answered = true;
                    if matches!(got["status"].as_str(), Some("ahead" | "identical")) {
                        return Found::OnGoal;
                    }
                }
            }
            match answered || refs.branches_of(repo).next().is_none() {
                true => Found::OffGoal,
                false => Found::Missing,
            }
        }
        Evidence::Ticket { .. } => Found::OffGoal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use workforce_desk::goal::{Per, Reading, Want};

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
        assert!(due(None, false, false, at(0), m), "never read");
        assert!(!due(Some(at(0)), false, false, at(9), m));
        assert!(due(Some(at(0)), false, false, at(10), m));
        assert!(
            !due(Some(at(0)), false, true, at(0), m),
            "not within a minute"
        );
        assert!(due(Some(at(0)), false, true, at(1), m));
        assert!(
            !due(Some(at(0)), true, true, at(4), m),
            "backing off a failed read"
        );
        assert!(due(Some(at(0)), true, true, at(5), m));
        assert!(due(Some(at(0)), true, false, at(10), m));
    }

    #[test]
    fn the_last_read_is_the_newest_of_any_part_and_says_a_failure() {
        let read = |minute: i64, value: Option<f64>| PartProgress {
            newest: Some(Reading {
                at: at(minute),
                part: 0,
                value,
                met: false,
                words: String::new(),
                evidence: Vec::new(),
                failed: value.is_none().then(|| "slow".into()),
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
                at: at(0),
                part: 0,
                value: Some(22.0),
                met: false,
                words: "22 minutes, the longest job".into(),
                evidence: Vec::new(),
                failed: None,
            }),
            ..Default::default()
        };
        let line = stall_line(
            "Mantis CI tests only what a change touches",
            &ci(),
            &now,
            Duration::minutes(90),
            tz,
            &["MAN-768's session: nothing runs under it".into()],
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
        let line = stall_line(
            "g",
            &judged,
            &PartProgress::default(),
            Duration::minutes(30),
            tz,
            &[],
        );
        assert!(line.contains("nothing shown toward it yet"), "{line}");
        assert!(line.contains("none of its tickets has a session"), "{line}");
        assert!(line.contains("crew.plan_update checkpoint"), "{line}");
    }

    #[test]
    fn only_a_plain_branch_or_commit_reaches_a_compare() {
        for ok in ["main", "fix/ci-cache", "release-1.2", "d05e8a4", "MAN_768"] {
            assert!(plain_ref(ok), "{ok}");
        }
        for not in [
            "",
            "../main",
            "main?per_page=1",
            "a#b",
            "-x",
            "/main",
            "main/",
            "a..b",
            "a b",
            "main.",
        ] {
            assert!(!plain_ref(not), "{not}");
        }
        assert!(is_sha("d05e8a4") && is_sha(&"a".repeat(40)));
        assert!(!is_sha("d05e8a") && !is_sha("main123") && !is_sha(&"a".repeat(41)));
    }

    #[test]
    fn a_checkpoint_refused_says_which_evidence_and_why() {
        let commit = Evidence::Commit {
            repo: "acme/mantis".into(),
            sha: "d05e8a4c0ffee".into(),
        };
        let line = not_shown_line(&NotShown::OffGoal(commit.clone()));
        assert!(
            line.starts_with("Commit d05e8a4 of acme/mantis isn't the goal's own work"),
            "{line}"
        );
        assert!(not_shown_line(&NotShown::Missing(commit)).contains("isn't on GitHub as it says"));
        assert!(not_shown_line(&NotShown::NothingNew).contains("shown toward this part already"));
        assert!(not_shown_line(&NotShown::Nothing).contains("(evidence)"));
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

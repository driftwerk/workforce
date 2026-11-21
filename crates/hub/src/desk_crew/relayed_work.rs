//! A boss relayed from the home: the way back for the work of the
//! sessions it runs on a machine (designs/boss-takes-on-machine-sessions.md).
//! The relay runs one way, the Mac asking ([`super::relay`]), so what those
//! sessions say and do comes back on its takes, as their decisions do
//! ([`super::relayed_decide`]).
//!
//! - **On the machine** a take ([`WORK_TAKE`]) hands back, for one boss and
//!   grant, the sessions it runs, what they did since the Mac's cursor,
//!   what they ask it on Desk's line and the reports that wait for it, and
//!   the requests it may answer. While its home takes within
//!   [`WORK_FRESH`], the line holds what they say to it for the home
//!   ([`CrewHub::home_reads`]); once it doesn't, their questions are the
//!   person's.
//! - **On the Mac** the work loop takes under each machine grant of each
//!   overseer, and what came for a session runs one `[Desk work]` turn of
//!   its boss, as a session's line turn here does. Whatever it does goes
//!   back by the relay, decided there under the grant.
//!
//! The take is the Mac hub's own: no crew tool sends it, and the machine
//! takes it only on its native relay route, from `local:<agent>`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Oversight;
use workforce_desk::charge::Wheel;
use workforce_desk::crew::{By, Event, Kind};
use workforce_desk::grant::{BossRef, Grant};
use workforce_desk::line::{Asked, Delivery, LineKind, LineMessage};
use workforce_desk::ChargeView;

use super::line::{about_session, settle, straight_to_person, Said, Settle};
use super::relayed_decide::{key_of, refused};
use super::served::{served_oversight, ServedStatus};
use super::{name_of, CrewHub};
use crate::serve::AgentService;

/// The take the Mac relays to a machine for its boss's work there.
pub const WORK_TAKE: &str = "crew.work_take";

/// How long a home's take keeps its boss reading its sessions' line on a
/// machine.
pub(crate) const WORK_FRESH: Duration = Duration::from_secs(45);
/// The most events one take hands back.
const MOST_EVENTS: i64 = 100;
/// How long a line message a turn was shown stays known for its tools.
const LINES_KEPT: Duration = Duration::from_secs(3_600);
/// When reports a turn didn't take are given to the next.
const NOT_RUN_AGAIN: Duration = Duration::from_secs(60);
/// How often the machine looks for questions whose home stopped reading.
const SWEEP_EVERY: Duration = Duration::from_secs(5);

const UNREAD: &str = "Its boss on the Mac can't be reached, so its question comes to you.";

/// A session the boss runs on a machine, as its take says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Run {
    pub id: Uuid,
    pub name: String,
    pub engine: String,
    pub state: String,
    pub wheel: Wheel,
    pub goal: String,
    pub folder: String,
    pub last_reply: Option<String>,
}

impl Run {
    /// Whether it holds one of its boss's slots: working, asking or
    /// blocked.
    fn busy(&self) -> bool {
        matches!(
            self.state.as_str(),
            "starting" | "working" | "blocked" | "asking"
        )
    }
}

/// A message on a session's line to the boss, as its take says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Line {
    pub session: Uuid,
    pub hop: i16,
    #[serde(flatten)]
    pub said: Said,
}

impl From<&LineMessage> for Line {
    fn from(m: &LineMessage) -> Self {
        Self {
            session: m.from_session().unwrap_or_default(),
            hop: m.hop,
            said: Said::from(m),
        }
    }
}

/// Something a session did or waits on, as its take says: `id` is the
/// event's, or the request's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Frame {
    pub id: String,
    pub subject: Uuid,
    pub line: String,
    #[serde(default)]
    pub hop: i16,
}

/// A machine's answer to a take.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Taken {
    pub now: i64,
    pub sessions: Vec<Run>,
    pub events: Vec<Frame>,
    pub asks: Vec<Line>,
    pub reports: Vec<Line>,
    pub requests: Vec<Frame>,
}

/// A boss and grant: the boss's party (`home/<agent>`) and the group's id.
type Key = (String, String);

/// What a machine keeps of its relayed bosses' takes.
pub(crate) struct Homes {
    /// When each boss and grant's home last took.
    took: HashMap<Key, Instant>,
    /// Where the next take of each reads from when its home doesn't say.
    cursor: HashMap<Key, DateTime<Utc>>,
    /// Until then, a boss no home took for yet may still be read: one
    /// restarted here is taken for again within [`WORK_FRESH`].
    grace: Instant,
    swept: Option<Instant>,
}

impl Default for Homes {
    fn default() -> Self {
        Self {
            took: HashMap::new(),
            cursor: HashMap::new(),
            grace: Instant::now() + WORK_FRESH,
            swept: None,
        }
    }
}

impl Homes {
    fn reads(&self, party: &str) -> bool {
        let mut took = self.took.iter().filter(|((p, _), _)| p == party).peekable();
        match took.peek() {
            None => Instant::now() < self.grace,
            Some(_) => took.any(|(_, at)| at.elapsed() < WORK_FRESH),
        }
    }
}

/// A line message a turn on the Mac was shown, for the tools that name it.
struct Known {
    machine: String,
    group: String,
    session: Uuid,
    at: Instant,
}

/// What the Mac keeps of its bosses' work on machines.
#[derive(Default)]
pub(crate) struct Away {
    /// Each machine session a boss runs, by `<machine>/<id>`: its grant's
    /// key, its boss and how it is.
    runs: HashMap<String, ((String, String), String, Run)>,
    lines: HashMap<Uuid, Known>,
}

/// Whether event `e` of session `run` is the boss's to hear: what
/// [`CrewHub::grant_event`] tells a served boss, a turn's end only while
/// it has the wheel, and the wheel coming back to it.
fn work_frame(base: Option<Value>, e: &Event, run: &ChargeView) -> Option<Frame> {
    let line = match e.kind {
        Kind::HandedBack => format!(
            "{} {}",
            e.subject.as_ref().map_or("", |s| s.name.as_str()),
            e.text
        ),
        Kind::Finished if run.wheel != Wheel::Boss => return None,
        _ => base?["line"].as_str()?.to_string(),
    };
    Some(Frame {
        id: e.id.clone(),
        subject: run.session,
        line: workforce_desk::sees::capped(&line, 2_000),
        hop: e.hop,
    })
}

// ── The machine's side ──────────────────────────────────────────────────

impl CrewHub {
    /// Whether the boss `party` relayed from the home reads what the
    /// sessions it runs here say on Desk's line: its home took lately.
    pub(crate) fn home_reads(&self, party: &str) -> bool {
        super::relayed_decide::relayed_party(party) && self.work_homes.lock().reads(party)
    }

    /// The boss relayed from the home that runs `session` here, while it
    /// reads its line.
    pub(super) fn reading_home(&self, session: Uuid) -> Option<String> {
        self.desk
            .charge_of(session)
            .filter(workforce_desk::charge::Charge::live)
            .map(|c| c.boss)
            .filter(|b| self.home_reads(b))
    }

    /// Keeps what `messages` report to `boss` waiting for its home's take;
    /// its questions stay open for it.
    pub(super) async fn hold_for_home(&self, boss: &str, messages: &[LineMessage]) {
        let heard: Vec<Uuid> = messages
            .iter()
            .filter(|m| {
                m.kind == LineKind::Tell && m.to.id == boss && m.delivery == Delivery::Delivered
            })
            .map(|m| m.id)
            .collect();
        if heard.is_empty() {
            return;
        }
        if let Err(e) = self.desk.line_hold_for_boss(&heard).await {
            tracing::warn!(error = %e, "reports couldn't be kept for a boss on the Mac");
        }
    }

    /// Gives the person each open question to a boss relayed from the home
    /// whose home no longer reads its line, at most every [`SWEEP_EVERY`].
    pub(super) async fn sweep_unread_homes(&self) {
        {
            let mut h = self.work_homes.lock();
            if h.swept.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
                return;
            }
            h.swept = Some(Instant::now());
        }
        let asks = match self.desk.store().line_open_asks(None).await {
            Ok(asks) => asks,
            Err(e) => {
                tracing::warn!(error = %e, "reading questions to bosses on the Mac failed");
                return;
            }
        };
        for ask in asks.iter().filter(|a| {
            a.asked == Some(Asked::Open)
                && super::relayed_decide::relayed_party(&a.to.id)
                && !self.home_reads(&a.to.id)
        }) {
            if let Err(e) = self
                .escalate(ask.id, &By::you(), UNREAD, Vec::new(), None)
                .await
            {
                tracing::info!(error = %e, "a question wasn't given to the person");
            }
        }
    }

    /// The home's take of `boss`'s work under `group`'s grant
    /// ([`WORK_TAKE`]): from `since` (else where its last take ended, else
    /// now), acknowledging the reports it says it `taken`. One that only
    /// `peek`s reads what is open now and leaves where the next take reads
    /// from.
    pub(crate) async fn work_take(
        &self,
        boss: &BossRef,
        group: &str,
        input: &Value,
    ) -> (ServedStatus, Value) {
        let Some(key) = key_of(boss, group) else {
            return refused("only the home's own boss takes its work");
        };
        let g = match self.desk.grant_live(group) {
            Ok(g) => g,
            Err(why) => return refused(&why),
        };
        if g.boss.as_ref() != Some(boss) || self.overseer_for(&served_oversight(&g).scope).is_some()
        {
            return refused(&workforce_desk::scope::not_ours(None));
        }
        let now = Utc::now();
        let peek = input["peek"].as_bool() == Some(true);
        let since = {
            let mut h = self.work_homes.lock();
            h.took.insert(key.clone(), Instant::now());
            match peek {
                true => now,
                false => {
                    let since = input["since"]
                        .as_i64()
                        .and_then(DateTime::from_timestamp_millis)
                        .filter(|s| *s <= now)
                        .or_else(|| h.cursor.get(&key).copied())
                        .unwrap_or(now);
                    h.cursor.insert(key.clone(), now);
                    since
                }
            }
        };
        match self.relayed_work_of(&g, &key.0, since, now, input).await {
            Ok(taken) => {
                let mut body = json!(taken);
                super::served::cleaned(&mut body);
                (ServedStatus::Ok, body)
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "a home's take of its work failed");
                refused("This machine couldn't read its sessions just now.")
            }
        }
    }

    /// What `party` is to hear of the sessions it runs under `g`.
    async fn relayed_work_of(
        &self,
        g: &Grant,
        party: &str,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
        input: &Value,
    ) -> eyre::Result<Taken> {
        let runs: Vec<ChargeView> = self
            .desk
            .charges()
            .into_iter()
            .filter(|c| c.boss == party && self.in_grant(g, c.session))
            .collect();
        let run_of = |s: Uuid| runs.iter().find(|c| c.session == s);
        let ours = |m: &LineMessage| {
            m.to.id == party && m.from_session().is_some_and(|s| run_of(s).is_some())
        };
        let taken: HashSet<Uuid> = input["taken"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str()?.parse().ok())
            .collect();
        let (acked, reports): (Vec<LineMessage>, Vec<LineMessage>) = self
            .desk
            .store()
            .line_waiting_for_bosses()
            .await?
            .into_iter()
            .filter(|m| ours(m))
            .partition(|m| taken.contains(&m.id));
        if !acked.is_empty() {
            let ids: Vec<Uuid> = acked.iter().map(|m| m.id).collect();
            self.desk.line_reports_taken(&ids).await?;
        }
        let asks: Vec<Line> = self
            .desk
            .store()
            .line_open_asks(Some(party))
            .await?
            .iter()
            .filter(|m| m.asked == Some(Asked::Open) && ours(m))
            .map(Line::from)
            .collect();
        let ids: Vec<String> = runs.iter().map(|c| c.session.to_string()).collect();
        let events = match ids.is_empty() {
            true => Vec::new(),
            false => {
                self.desk
                    .store()
                    .events_since(since, &ids, MOST_EVENTS, None)
                    .await?
            }
        };
        let events = events
            .iter()
            .filter_map(|e| {
                let run = run_of(e.subject.as_ref()?.id.parse().ok()?)?;
                work_frame(self.grant_event(g, e), e, run)
            })
            .collect();
        let replies = workforce_desk::sees::shows(g.sees, workforce_desk::sees::Part::Reply);
        let unbound = self.desk.boss_unbound(party);
        let (sessions, requests): (Vec<Run>, Vec<Option<Frame>>) = runs
            .iter()
            .filter_map(|c| {
                let v = self.desk.view(c.session).ok()?;
                let request = v
                    .pending
                    .as_ref()
                    .filter(|p| {
                        p.answerable && p.request.is_some() && c.wheel == Wheel::Boss && !unbound
                    })
                    .and_then(|p| {
                        let f = super::served::request_frame(g, &v, p);
                        Some(Frame {
                            id: p.id.to_string(),
                            subject: c.session,
                            line: f["line"].as_str()?.to_string(),
                            hop: 0,
                        })
                    });
                let run = Run {
                    id: c.session,
                    name: v.label().to_string(),
                    engine: v.engine.title().to_string(),
                    state: json!(v.state).as_str().unwrap_or_default().to_string(),
                    wheel: c.wheel,
                    goal: v.goal.clone(),
                    folder: v.cwd.clone(),
                    last_reply: v
                        .last_reply
                        .as_deref()
                        .filter(|_| replies)
                        .map(|r| workforce_desk::crew::clip(r, 1500)),
                };
                Some((run, request))
            })
            .unzip();
        Ok(Taken {
            now: now.timestamp_millis(),
            sessions,
            events,
            asks,
            reports: reports.iter().map(Line::from).collect(),
            requests: requests.into_iter().flatten().collect(),
        })
    }
}

#[cfg(test)]
impl CrewHub {
    /// Every home here as if it stopped taking [`WORK_FRESH`] ago.
    pub(crate) fn homes_gone(&self) {
        let mut h = self.work_homes.lock();
        let long_ago = Instant::now()
            .checked_sub(WORK_FRESH)
            .unwrap_or_else(Instant::now);
        h.grace = long_ago;
        h.swept = None;
        h.took.values_mut().for_each(|at| *at = long_ago);
    }
}

// ── The Mac's side ──────────────────────────────────────────────────────

/// What came for one machine session, waiting for its boss's turn.
#[derive(Debug, Default)]
struct Work {
    ended: Vec<String>,
    said: Vec<Line>,
}

impl Work {
    fn is_empty(&self) -> bool {
        self.ended.is_empty() && self.said.is_empty()
    }
}

/// One machine's grant, as the Mac's work loop keeps it.
struct Pump {
    next: Instant,
    busy: bool,
    /// Where the next take reads from: the last one's `now`.
    cursor: Option<i64>,
    /// The events the last take handed back: one at its cursor comes again.
    seen: HashSet<String>,
    /// The reports a turn took, for the next take to acknowledge.
    acked: Vec<Uuid>,
    /// What was given to a turn, while it is still open: none to be given
    /// again, or when to give it again (a turn that didn't run).
    offered: HashMap<String, Option<Instant>>,
    gathered: HashMap<Uuid, Work>,
    turning: HashSet<Uuid>,
}

impl Default for Pump {
    fn default() -> Self {
        Self {
            next: Instant::now(),
            busy: false,
            cursor: None,
            seen: HashSet::new(),
            acked: Vec::new(),
            offered: HashMap::new(),
            gathered: HashMap::new(),
            turning: HashSet::new(),
        }
    }
}

impl Pump {
    /// Takes in what `t` handed back: each new event, question, report and
    /// request, gathered by session for its turn.
    fn heard(&mut self, t: &Taken) {
        let run: HashSet<Uuid> = t.sessions.iter().map(|s| s.id).collect();
        let open: HashSet<String> = t
            .asks
            .iter()
            .chain(&t.reports)
            .map(|l| l.said.id.to_string())
            .chain(t.requests.iter().map(|r| r.id.clone()))
            .collect();
        self.offered.retain(|id, _| open.contains(id));
        let now = Instant::now();
        let fresh = |id: String, offered: &mut HashMap<String, Option<Instant>>| {
            let due = offered
                .get(&id)
                .is_none_or(|again| again.is_some_and(|at| at <= now));
            if due {
                offered.insert(id, None);
            }
            due
        };
        let events: Vec<&Frame> = t
            .events
            .iter()
            .filter(|e| !self.seen.contains(&e.id))
            .collect();
        for e in events.iter().copied().chain(
            t.requests
                .iter()
                .filter(|r| fresh(r.id.clone(), &mut self.offered)),
        ) {
            if run.contains(&e.subject) {
                self.gathered
                    .entry(e.subject)
                    .or_default()
                    .ended
                    .push(e.line.clone());
            }
        }
        for l in t
            .asks
            .iter()
            .chain(&t.reports)
            .filter(|l| fresh(l.said.id.to_string(), &mut self.offered))
        {
            if run.contains(&l.session) {
                self.gathered
                    .entry(l.session)
                    .or_default()
                    .said
                    .push(l.clone());
            }
        }
        self.seen = t.events.iter().map(|e| e.id.clone()).collect();
        self.gathered
            .retain(|s, w| run.contains(s) && !w.is_empty());
    }

    /// What is due for a turn now: each session's gathered work whose turn
    /// isn't running.
    fn due(&mut self) -> Vec<(Uuid, Work)> {
        let ready: Vec<Uuid> = self
            .gathered
            .keys()
            .filter(|s| !self.turning.contains(s))
            .copied()
            .collect();
        ready
            .into_iter()
            .filter_map(|s| {
                self.turning.insert(s);
                Some((s, self.gathered.remove(&s)?))
            })
            .collect()
    }

    /// A turn on `session` ended: its reports were taken if it `ran`, else
    /// given to a turn again after [`NOT_RUN_AGAIN`].
    fn turned(&mut self, session: Uuid, reports: &[Uuid], ran: bool) {
        self.turning.remove(&session);
        match ran {
            true => self.acked.extend_from_slice(reports),
            false => {
                let again = Instant::now() + NOT_RUN_AGAIN;
                for id in reports {
                    self.offered.insert(id.to_string(), Some(again));
                }
            }
        }
    }
}

type Pumps = Arc<Mutex<HashMap<(String, String), Pump>>>;

/// How often the Mac takes, by the crew's tick (30 s): every 3 s while its
/// boss runs sessions there, every 15 s while it runs none or the machine
/// can't be reached, and every minute once the machine says it doesn't
/// serve the take (older).
fn next_take(tick: Duration, runs: bool, older: bool) -> Duration {
    match (runs, older) {
        (true, _) => tick / 10,
        (false, true) => tick * 2,
        (false, false) => tick / 2,
    }
}

impl CrewHub {
    /// The machine and session a line message a turn here was shown is
    /// on, when it is one of a machine's: what its tools are relayed to.
    pub(crate) fn machine_line(&self, message: Uuid) -> Option<(String, Uuid)> {
        self.work_away
            .lock()
            .lines
            .get(&message)
            .map(|k| (k.machine.clone(), k.session))
    }

    /// How many of `boss`'s sessions on machines, outside its plans
    /// (`planned`), hold a slot.
    pub(crate) fn machine_runs_busy(&self, boss: &str, planned: impl Fn(&str) -> bool) -> usize {
        self.work_away
            .lock()
            .runs
            .iter()
            .filter(|(k, (_, b, run))| b == boss && run.busy() && !planned(k))
            .count()
    }

    /// What `t` says of `k`'s machine sessions run by `agent`, kept for the
    /// slots and the tools.
    fn away_heard(&self, k: &(String, String), agent: &str, t: &Taken) {
        let (machine, group) = k;
        let mut away = self.work_away.lock();
        away.runs.retain(|_, (grant, _, _)| grant != k);
        for s in &t.sessions {
            away.runs.insert(
                format!("{machine}/{}", s.id),
                (k.clone(), agent.to_string(), s.clone()),
            );
        }
        let open: HashSet<Uuid> = t.asks.iter().chain(&t.reports).map(|l| l.said.id).collect();
        away.lines.retain(|id, l| {
            open.contains(id)
                || l.at.elapsed() < LINES_KEPT
                || l.machine != *machine
                || l.group != *group
        });
        for l in t.asks.iter().chain(&t.reports) {
            away.lines.insert(
                l.said.id,
                Known {
                    machine: machine.clone(),
                    group: group.clone(),
                    session: l.session,
                    at: Instant::now(),
                },
            );
        }
    }

    fn away_forget(&self, keep: impl Fn(&(String, String)) -> bool) {
        let mut away = self.work_away.lock();
        away.runs.retain(|_, (k, _, _)| keep(k));
        away.lines
            .retain(|_, l| keep(&(l.machine.clone(), l.group.clone())));
    }

    /// What the boss reads for `work` of `run`, `<machine>/<id>` of `k`.
    fn machine_work_prompt(
        &self,
        k: &(String, String),
        run: &Run,
        work: &Work,
        o: &Oversight,
    ) -> String {
        let named = format!("{}/{}", k.0, run.id);
        let about = about_session(&json!({
            "name": run.name,
            "machine": k.0,
            "engine": run.engine,
            "folder": run.folder,
            "goal": run.goal,
            "state": run.state,
            "last_reply": run.last_reply,
        }));
        let said: Vec<Said> = work.said.iter().map(|l| l.said.clone()).collect();
        self.line_prompt_of(&named, &about, &said, &work.ended, o, false)
    }
}

/// The Mac's work loop: while Desk relays, each overseer takes under each
/// of its grants on the person's machines, as the module says.
pub(crate) async fn work_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let pumps: Pumps = Arc::default();
    let mut tick = tokio::time::interval((crew.timing.tick / 30).max(Duration::from_millis(20)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        if !crew.relaying() {
            pumps.lock().clear();
            crew.away_forget(|_| false);
            continue;
        }
        let mut seen = HashSet::new();
        for (service, o) in crew.overseers() {
            let agent = service.agent_id.as_str().to_string();
            for k in crew.machine_grants_of(&agent, &o) {
                seen.insert(k.clone());
                {
                    let mut p = pumps.lock();
                    let p = p.entry(k.clone()).or_default();
                    if p.busy || Instant::now() < p.next {
                        continue;
                    }
                    p.busy = true;
                }
                tokio::spawn(take(
                    Arc::clone(&crew),
                    Arc::clone(&service),
                    o.clone(),
                    k,
                    Arc::clone(&pumps),
                ));
            }
        }
        pumps
            .lock()
            .retain(|k, p| seen.contains(k) || p.busy || !p.turning.is_empty());
        crew.away_forget(|k| seen.contains(k));
    }
}

/// One take from `k`'s machine under its grant, and the turns what came
/// for its sessions runs.
async fn take(
    crew: Arc<CrewHub>,
    service: Arc<AgentService>,
    o: Oversight,
    k: (String, String),
    pumps: Pumps,
) {
    let (machine, group) = &k;
    let agent = service.agent_id.as_str().to_string();
    let started = Instant::now();
    let (cursor, sent) = {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        (p.cursor, p.acked.clone())
    };
    let input = json!({ "since": cursor, "taken": sent });
    let ceiling = crew.ceiling_now(&agent, &o);
    let got = crew
        .relay(machine, group, WORK_TAKE, input, &agent, &ceiling)
        .await;
    // A plan's sessions' turn ends go to its plan's turn (`super::plan`):
    // only what they say on the line and wait on comes here.
    let planned = workforce_desk::grant::plan_of(group).is_some();
    let taken = match &got {
        Ok(a) if a.status == ServedStatus::Ok => serde_json::from_value::<Taken>(a.body.clone())
            .ok()
            .map(|t| match planned {
                true => Taken {
                    events: Vec::new(),
                    ..t
                },
                false => t,
            }),
        _ => None,
    };
    let older = matches!(&got, Ok(a) if super::relay::older(machine, WORK_TAKE, a).is_some());
    let due = {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        p.busy = false;
        let runs = taken.as_ref().is_some_and(|t| !t.sessions.is_empty());
        p.next = started + next_take(crew.timing.tick, runs, older);
        match &taken {
            Some(t) => {
                p.cursor = Some(t.now);
                p.acked.retain(|id| !sent.contains(id));
                p.heard(t);
                p.due()
            }
            None => Vec::new(),
        }
    };
    let Some(t) = taken else {
        return;
    };
    crew.away_heard(&k, &agent, &t);
    for (session, work) in due {
        let Some(run) = t.sessions.iter().find(|s| s.id == session).cloned() else {
            pumps
                .lock()
                .entry(k.clone())
                .or_default()
                .turned(session, &[], true);
            continue;
        };
        tokio::spawn(work_turn(
            Arc::clone(&crew),
            Arc::clone(&service),
            o.clone(),
            k.clone(),
            run,
            work,
            Arc::clone(&pumps),
        ));
    }
}

/// One `[Desk work]` turn of the boss on `run` of `k`'s machine, for
/// `work`; and what it left of its questions, settled.
async fn work_turn(
    crew: Arc<CrewHub>,
    service: Arc<AgentService>,
    o: Oversight,
    k: (String, String),
    run: Run,
    mut work: Work,
    pumps: Pumps,
) {
    let agent = service.agent_id.as_str().to_string();
    let reports: Vec<Uuid> = work
        .said
        .iter()
        .filter(|l| l.said.kind != LineKind::Ask)
        .map(|l| l.said.id)
        .collect();
    let ran = crew
        .with_mind(
            &agent,
            "a machine session's line",
            crew.work_turn_now(&service, &o, &k, &run, &mut work),
        )
        .await;
    pumps
        .lock()
        .entry(k)
        .or_default()
        .turned(run.id, &reports, ran);
}

impl CrewHub {
    /// Relays `tool` with `input` under `k`'s grant for `agent`: what went
    /// wrong is only logged, as for a line turn's own settling here.
    async fn relay_settle(
        &self,
        agent: &str,
        o: &Oversight,
        k: &(String, String),
        tool: &str,
        input: Value,
    ) {
        let ceiling = self.ceiling_now(agent, o);
        match self.relay(&k.0, &k.1, tool, input, agent, &ceiling).await {
            Ok(a) if a.status == ServedStatus::Refused => {
                tracing::info!(machine = %k.0, %tool, answer = %a.body, "a machine refused a boss's settling");
            }
            Ok(_) => {}
            Err(why) => {
                tracing::info!(machine = %k.0, %tool, %why, "a boss's settling didn't reach its machine")
            }
        }
    }

    /// Gives each question in `asks` to the person on its machine, saying
    /// `why`.
    async fn machine_to_person(
        &self,
        agent: &str,
        o: &Oversight,
        k: &(String, String),
        asks: &[&Line],
        why: &str,
    ) {
        for a in asks {
            let input = json!({"message_id": a.said.id.to_string(), "reason": why});
            self.relay_settle(agent, o, k, "crew.escalate", input).await;
        }
    }

    /// [`work_turn`]'s own: whether its turn ran.
    async fn work_turn_now(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        k: &(String, String),
        run: &Run,
        work: &mut Work,
    ) -> bool {
        let agent = service.agent_id.as_str();
        let asks: Vec<Line> = work
            .said
            .iter()
            .filter(|l| l.said.kind == LineKind::Ask)
            .cloned()
            .collect();
        let asked: Vec<&Line> = asks.iter().collect();
        if let Some(held) = self.desk.agent_held_why(agent) {
            let why = format!("{} is {held}, so this came to you.", name_of(service));
            self.machine_to_person(agent, o, k, &asked, &why).await;
            return false;
        }
        let hop = work.said.iter().map(|l| l.hop).max().unwrap_or(0);
        if let Some(why) = straight_to_person(o, hop, self.unbound(o)) {
            self.machine_to_person(agent, o, k, &asked, why).await;
            work.said.retain(|l| l.said.kind != LineKind::Ask);
            if work.is_empty() {
                return true;
            }
        }
        let Some(owner) = self.person(Some(service)) else {
            self.machine_to_person(
                agent,
                o,
                k,
                &asked,
                "Desk has no conversation with you to work from yet.",
            )
            .await;
            return true;
        };
        let asks: Vec<Uuid> = work
            .said
            .iter()
            .filter(|l| l.said.kind == LineKind::Ask)
            .map(|l| l.said.id)
            .collect();
        let prompt = self.machine_work_prompt(k, run, work, o);
        let channel = format!("{DESK_CHANNEL}:line:{}/{}", k.0, run.id);
        let ran = self
            .run_turn_with(service, &owner, &channel, &prompt, hop, None, None, None)
            .await;
        if let Err(e) = &ran {
            tracing::warn!(machine = %k.0, session = %run.id, error = %format!("{e:#}"), "the overseer's turn on a machine session failed");
        }
        if asks.is_empty() {
            return ran.is_ok();
        }
        let ceiling = self.ceiling_now(agent, o);
        let open: Vec<Uuid> = match self
            .relay(
                &k.0,
                &k.1,
                WORK_TAKE,
                json!({"peek": true}),
                agent,
                &ceiling,
            )
            .await
        {
            Ok(a) if a.status == ServedStatus::Ok => serde_json::from_value::<Taken>(a.body)
                .map(|t| {
                    t.asks
                        .into_iter()
                        .map(|l| l.said.id)
                        .filter(|id| asks.contains(id))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let reply = ran.as_ref().map(|r| r.answer.as_str()).map_err(|_| ());
        for (ask, settled) in open.iter().zip(settle(reply, open.len())) {
            let (tool, input) = match settled {
                Settle::Answer(message) => {
                    let reason = match workforce_desk::crew::one_line(&message, 200) {
                        r if r.is_empty() => {
                            "Its reply answered the session's question.".to_string()
                        }
                        r => r,
                    };
                    (
                        "crew.nudge",
                        json!({"session_id": run.id.to_string(), "reply_to": ask.to_string(),
                               "message": message, "reason": reason}),
                    )
                }
                Settle::Escalate(why) => (
                    "crew.escalate",
                    json!({"message_id": ask.to_string(), "reason": why}),
                ),
            };
            self.relay_settle(agent, o, k, tool, input).await;
        }
        ran.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(session: Uuid, kind: LineKind) -> Line {
        Line {
            session,
            hop: 1,
            said: Said {
                id: Uuid::new_v4(),
                kind,
                in_reply_to: None,
                text: "said".into(),
            },
        }
    }

    fn run(id: Uuid) -> Run {
        Run {
            id,
            name: "VEL-9".into(),
            engine: "Claude Code".into(),
            state: "working".into(),
            wheel: Wheel::Boss,
            goal: String::new(),
            folder: "/src".into(),
            last_reply: None,
        }
    }

    fn frame(id: &str, subject: Uuid) -> Frame {
        Frame {
            id: id.into(),
            subject,
            line: format!("{id} happened"),
            hop: 0,
        }
    }

    #[test]
    fn what_a_take_hands_back_is_gathered_once_by_session() {
        let s = Uuid::new_v4();
        let gone = Uuid::new_v4();
        let ask = line(s, LineKind::Ask);
        let report = line(s, LineKind::Tell);
        let t = Taken {
            now: 1,
            sessions: vec![run(s)],
            events: vec![frame("e1", s), frame("e2", gone)],
            asks: vec![ask.clone()],
            reports: vec![report.clone()],
            requests: vec![frame("r1", s)],
        };
        let mut p = Pump::default();
        p.heard(&t);
        let due = p.due();
        assert_eq!(due.len(), 1, "only a session it still runs");
        let (session, work) = &due[0];
        assert_eq!(*session, s);
        assert_eq!(work.ended, vec!["e1 happened", "r1 happened"]);
        assert_eq!(work.said, vec![ask.clone(), report.clone()]);
        assert!(p.due().is_empty(), "its turn runs: nothing more is due");
        // The same take again (the cursor's event comes back): nothing new.
        p.turned(s, &[report.said.id], true);
        p.heard(&t);
        assert!(p.due().is_empty());
        assert_eq!(
            p.acked,
            vec![report.said.id],
            "acknowledged on the next take"
        );
    }

    #[test]
    fn reports_a_turn_didnt_take_come_again_later_and_closed_ones_go() {
        let s = Uuid::new_v4();
        let report = line(s, LineKind::Tell);
        let t = Taken {
            now: 1,
            sessions: vec![run(s)],
            reports: vec![report.clone()],
            ..Taken::default()
        };
        let mut p = Pump::default();
        p.heard(&t);
        assert_eq!(p.due().len(), 1);
        p.turned(s, &[report.said.id], false);
        assert!(p.acked.is_empty());
        p.heard(&t);
        assert!(p.due().is_empty(), "not before NOT_RUN_AGAIN");
        p.offered
            .insert(report.said.id.to_string(), Some(Instant::now()));
        p.heard(&t);
        assert_eq!(p.due().len(), 1, "given again once due");
        p.heard(&Taken {
            sessions: vec![run(s)],
            ..Taken::default()
        });
        assert!(p.offered.is_empty(), "what is no longer open is forgotten");
    }

    #[test]
    fn a_home_reads_while_it_takes_and_one_never_heard_only_at_first() {
        let mut h = Homes::default();
        assert!(h.reads("home/desk"), "just started: its home may take yet");
        h.grace = Instant::now();
        assert!(!h.reads("home/desk"));
        h.took
            .insert(("home/desk".into(), "g".into()), Instant::now());
        assert!(h.reads("home/desk"));
        h.took.insert(
            ("home/desk".into(), "g".into()),
            Instant::now() - WORK_FRESH,
        );
        assert!(!h.reads("home/desk"), "its home stopped taking");
    }

    #[test]
    fn a_machine_that_doesnt_serve_a_call_is_said_to_need_updating() {
        let refused = |error: &str| super::super::relay::RelayAnswer {
            status: ServedStatus::Refused,
            body: json!({ "error": error }),
        };
        assert_eq!(
            super::super::relay::older("devbox", WORK_TAKE, &refused("crew.work_take isn't served to a boss here.")),
            Some("Desk on devbox is older and can't do crew.work_take yet: tell the person to update Desk there.".into())
        );
        assert_eq!(
            super::super::relay::older(
                "devbox",
                "crew.take_on",
                &refused("That session isn't yours to run.")
            ),
            None
        );
    }

    #[test]
    fn a_take_crosses_the_wire_whole() {
        let s = Uuid::new_v4();
        let t = Taken {
            now: 42,
            sessions: vec![run(s)],
            events: vec![frame("e", s)],
            asks: vec![line(s, LineKind::Ask)],
            reports: vec![line(s, LineKind::Tell)],
            requests: vec![frame("r", s)],
        };
        let back: Taken = serde_json::from_value(json!(t)).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn a_turns_end_reaches_the_boss_only_while_it_has_the_wheel() {
        let s = Uuid::new_v4();
        let mut e = Event::new(
            Kind::Finished,
            workforce_desk::crew::Party::you(),
            None,
            "ended its turn",
        );
        let base = Some(json!({"line": "VEL-9 finished"}));
        let mut c = ChargeView {
            session: s,
            group: None,
            boss: "home/desk".into(),
            boss_name: "Desk".into(),
            how: workforce_desk::charge::How::Taken,
            wheel: Wheel::Boss,
            wheel_since: None,
            hold_minutes: 10,
            since: Utc::now(),
            released_at: None,
            released_by: None,
        };
        assert_eq!(
            work_frame(base.clone(), &e, &c).unwrap().line,
            "VEL-9 finished"
        );
        c.wheel = Wheel::Person;
        assert!(work_frame(base.clone(), &e, &c).is_none());
        e.kind = Kind::HandedBack;
        assert!(work_frame(None, &e, &c).is_some(), "the wheel coming back");
        e.kind = Kind::Failed;
        assert!(
            work_frame(None, &e, &c).is_none(),
            "nothing the grant doesn't tell"
        );
    }
}

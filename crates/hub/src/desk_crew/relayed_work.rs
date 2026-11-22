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

use std::collections::{HashMap, HashSet, VecDeque};
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
use workforce_desk::crew::{self, By, Event, Kind};
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
/// How long after its time an event may still be saved: each take reads
/// that far behind its cursor, and never hands back one it handed.
const SAVED_WITHIN: chrono::TimeDelta = chrono::TimeDelta::seconds(10);
/// How many of the events it heard last the Mac knows again.
const HEARD_KEPT: usize = 4 * MOST_EVENTS as usize;
/// How long a line message a turn was shown stays known for its tools.
const LINES_KEPT: Duration = Duration::from_secs(3_600);
/// How often the machine looks for questions whose home stopped reading.
const SWEEP_EVERY: Duration = Duration::from_secs(5);

const UNREAD: &str = "Its boss on the Mac can't be reached, so its question comes to you.";
const NOT_TAKEN: &str =
    "Its boss on the Mac doesn't reach this session just now, so its question comes to you.";

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
    /// Whether its boss's question answerer has what it asks, there
    /// ([`super::answer`]): it isn't raised to the boss's turns.
    #[serde(default)]
    pub answered: bool,
    /// It as the overview there shows it, while what it waits on is raised
    /// to its boss here ([`CrewHub::raised_elsewhere`]): what the boss's
    /// check-in watches, as for a session here.
    #[serde(default)]
    pub item: Option<crew::Item>,
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
    /// When it was made.
    pub now: i64,
    /// The latest event read so far, if any: the next take reads on past
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<ReadTo>,
    pub sessions: Vec<Run>,
    pub events: Vec<Frame>,
    pub asks: Vec<Line>,
    pub reports: Vec<Line>,
    pub requests: Vec<Frame>,
}

/// The last event a take read: its time and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadTo {
    pub at: DateTime<Utc>,
    pub id: String,
}

/// Where a take reads from: the first take's time, or past an event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadFrom {
    Since(DateTime<Utc>),
    After(ReadTo),
}

impl ReadFrom {
    fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Since(at) | Self::After(ReadTo { at, .. }) => *at,
        }
    }

    /// The time a take reads from: past an event, [`SAVED_WITHIN`] behind
    /// it, for one saved late.
    fn since(&self) -> DateTime<Utc> {
        match self {
            Self::Since(at) => *at,
            Self::After(r) => r.at - SAVED_WITHIN,
        }
    }
}

/// A boss and grant: the boss's party (`home/<agent>`) and the group's id.
type Key = (String, String);

/// What a machine keeps of its relayed bosses' takes.
pub(crate) struct Homes {
    /// When each boss and grant's home last took.
    took: HashMap<Key, Instant>,
    /// Where the next take of each reads from when its home doesn't say.
    cursor: HashMap<Key, ReadFrom>,
    /// The events each was handed within [`SAVED_WITHIN`] of its cursor,
    /// with their times: read again, as one saved late is looked for, but
    /// not handed again while its home reads on from that cursor.
    handed: HashMap<Key, HashMap<String, DateTime<Utc>>>,
    /// Until then, a boss no home took for yet may still be read: one
    /// restarted here is taken for again within [`WORK_FRESH`].
    grace: Instant,
    swept: Option<Instant>,
    /// The grants issued by then count as issued [`WORK_FRESH`] earlier
    /// ([`CrewHub::homes_gone`]).
    #[cfg(test)]
    aged_to: Option<DateTime<Utc>>,
}

impl Default for Homes {
    fn default() -> Self {
        Self {
            took: HashMap::new(),
            cursor: HashMap::new(),
            handed: HashMap::new(),
            grace: Instant::now() + WORK_FRESH,
            swept: None,
            #[cfg(test)]
            aged_to: None,
        }
    }
}

impl Homes {
    /// Whether `party`'s home took lately: under `grant`'s (its group, and
    /// when it was issued), else under any. One it never took under here,
    /// or not since it was issued, may be taken under yet: at first, and
    /// while the grant is new.
    fn reads(&self, party: &str, grant: Option<(&str, DateTime<Utc>)>) -> bool {
        let fresh = |at: &Instant| at.elapsed() < WORK_FRESH;
        let Some((group, issued)) = grant else {
            let mut took = self.took.iter().filter(|((p, _), _)| p == party).peekable();
            return match took.peek() {
                None => Instant::now() < self.grace,
                Some(_) => took.any(|(_, at)| fresh(at)),
            };
        };
        let taken_at = |at: &Instant| {
            chrono::TimeDelta::from_std(at.elapsed())
                .map_or(DateTime::<Utc>::MIN_UTC, |d| Utc::now() - d)
        };
        #[cfg(test)]
        let issued = match self.aged_to {
            Some(to) if issued <= to => issued - chrono::TimeDelta::from_std(WORK_FRESH).unwrap(),
            _ => issued,
        };
        let new = (Utc::now() - issued)
            .to_std()
            .ok()
            .is_none_or(|age| age < WORK_FRESH);
        match self
            .took
            .get(&(party.to_string(), group.to_string()))
            .filter(|at| taken_at(at) >= issued)
        {
            Some(at) => fresh(at),
            None => Instant::now() < self.grace || new,
        }
    }

    /// Forgets the takes under grants no longer here: one made again is
    /// new.
    fn forget_gone(&mut self, groups: &HashSet<String>) {
        self.took.retain(|(_, g), _| groups.contains(g));
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
    /// Each machine session a boss runs, by `<machine>/<id>`, as the
    /// latest take said.
    runs: HashMap<String, AwayRun>,
    lines: HashMap<Uuid, Known>,
}

/// A machine session a boss runs, as a take said it.
struct AwayRun {
    /// Its grant's key.
    grant: (String, String),
    boss: String,
    run: Run,
    /// When the take was made.
    at: Instant,
}

impl AwayRun {
    /// It as `boss`'s, said lately: a take older than [`WORK_FRESH`] says
    /// nothing of now.
    fn of(&self, boss: &str) -> Option<&Run> {
        (self.boss == boss && self.at.elapsed() < WORK_FRESH).then_some(&self.run)
    }

    /// Whether it holds one of `boss`'s slots: as its last take said, however
    /// old, so a link that drops never frees a slot the machine still uses.
    fn holds_slot(&self, boss: &str) -> bool {
        self.boss == boss && self.run.busy()
    }
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
    /// Whether the boss `party` relayed from the home reads what `session`
    /// says on Desk's line here: its home took lately under the grant that
    /// holds it (none holding it, never). Of no session, under any grant.
    pub(crate) fn home_reads(&self, party: &str, session: Option<Uuid>) -> bool {
        let homes = || self.work_homes.lock();
        super::relayed_decide::relayed_party(party)
            && match session.map(|s| self.desk.grant_held_of(s)) {
                None => homes().reads(party, None),
                Some(None) => false,
                Some(Some((g, issued))) => homes().reads(party, Some((&g, issued))),
            }
    }

    /// The boss relayed from the home that runs `session` here, while it
    /// reads its line.
    pub(super) fn reading_home(&self, session: Uuid) -> Option<String> {
        self.desk
            .charge_of(session)
            .filter(workforce_desk::charge::Charge::live)
            .map(|c| c.boss)
            .filter(|b| self.home_reads(b, Some(session)))
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
    /// whose home no longer reads its line, at most every [`SWEEP_EVERY`]:
    /// while a relayed boss runs a session here, or any session waits on a
    /// question (one whose relayed boss was let go since included).
    pub(super) async fn sweep_unread_homes(&self) {
        let relayed = self
            .desk
            .charges()
            .iter()
            .any(|c| super::relayed_decide::relayed_party(&c.boss));
        if !relayed && !self.desk.asks_waited_on() {
            return;
        }
        {
            let mut h = self.work_homes.lock();
            if h.swept.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
                return;
            }
            h.swept = Some(Instant::now());
        }
        let groups: HashSet<String> = self
            .desk
            .grants_bossed()
            .into_iter()
            .map(|(g, _)| g.group)
            .collect();
        self.work_homes.lock().forget_gone(&groups);
        let home = format!("{}/", workforce_desk::grant::HOME_HUB);
        let asks = match self.desk.store().line_open_asks_to(&home).await {
            Ok(asks) => asks,
            Err(e) => {
                tracing::warn!(error = %e, "reading questions to bosses on the Mac failed");
                return;
            }
        };
        for (ask, to, asker) in asks.iter().filter(|(_, to, a)| !self.home_reads(to, *a)) {
            // Its home takes, but not under the asking session's grant.
            let why = match asker.is_some() && self.home_reads(to, None) {
                true => NOT_TAKEN,
                false => UNREAD,
            };
            if let Err(e) = self
                .escalate(*ask, &By::desk(why), why, Vec::new(), None)
                .await
            {
                tracing::info!(error = %e, "a question wasn't given to the person");
            }
        }
    }

    /// The home's take of `boss`'s work under `group`'s grant
    /// ([`WORK_TAKE`]): past `after`, the last event it read (else where
    /// its last take ended, else now), acknowledging the reports it says it
    /// `taken`. One that only `peek`s reads what is open now and leaves
    /// where the next take reads from.
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
        let (from, skip) = {
            let mut h = self.work_homes.lock();
            h.took.insert(key.clone(), Instant::now());
            let said = serde_json::from_value::<ReadTo>(input["after"].clone())
                .ok()
                .filter(|r| r.at <= now)
                .map(ReadFrom::After);
            // The home reads on from somewhere else than this last handed
            // it: that answer never reached it, so what it handed is handed
            // again (the Mac knows what it heard).
            if !peek && said.as_ref().is_some_and(|f| h.cursor.get(&key) != Some(f)) {
                h.handed.remove(&key);
            }
            let from = match peek {
                true => ReadFrom::Since(now),
                false => said
                    .or_else(|| h.cursor.get(&key).cloned())
                    .unwrap_or(ReadFrom::Since(now)),
            };
            let skip: Vec<String> = h
                .handed
                .get(&key)
                .into_iter()
                .flat_map(HashMap::keys)
                .cloned()
                .chain(match &from {
                    ReadFrom::After(r) => Some(r.id.clone()),
                    ReadFrom::Since(_) => None,
                })
                .collect();
            (from, skip)
        };
        match self
            .relayed_work_of(&g, &key.0, &from, &skip, now, input)
            .await
        {
            Ok((taken, read)) => {
                if !peek {
                    let mut h = self.work_homes.lock();
                    let next = taken.after.clone().map_or(from, ReadFrom::After);
                    let floor = next.at() - SAVED_WITHIN;
                    let handed = h.handed.entry(key.clone()).or_default();
                    handed.extend(read);
                    handed.retain(|_, at| *at >= floor);
                    h.cursor.insert(key, next);
                }
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

    /// What `party` is to hear of the sessions it runs under `g`, and each
    /// event read for it (but those in `skip`, handed already), with its
    /// time.
    async fn relayed_work_of(
        &self,
        g: &Grant,
        party: &str,
        from: &ReadFrom,
        skip: &[String],
        now: DateTime<Utc>,
        input: &Value,
    ) -> eyre::Result<(Taken, Vec<(String, DateTime<Utc>)>)> {
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
        let views: Vec<_> = runs
            .iter()
            .filter_map(|c| self.desk.view(c.session).ok())
            .collect();
        let answered: HashSet<Uuid> = views
            .iter()
            .filter(|v| self.answers_question_itself(v))
            .map(|v| v.id)
            .collect();
        let items = self.desk.session_items(self, &views);
        let ids: Vec<String> = runs.iter().map(|c| c.session.to_string()).collect();
        let events = match ids.is_empty() {
            true => Vec::new(),
            false => {
                self.desk
                    .store()
                    .events_from(from.since(), skip, &ids, MOST_EVENTS)
                    .await?
            }
        };
        let read: Vec<(String, DateTime<Utc>)> =
            events.iter().map(|e| (e.id.clone(), e.at)).collect();
        // The latest event read so far: never past one not read yet.
        let after = events
            .iter()
            .map(|e| ReadTo {
                at: e.at,
                id: e.id.clone(),
            })
            .chain(match from {
                ReadFrom::After(r) => Some(r.clone()),
                ReadFrom::Since(_) => None,
            })
            .max_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        let events = events
            .iter()
            .filter_map(|e| {
                let run = run_of(e.subject.as_ref()?.id.parse().ok()?)?;
                let asked = matches!(e.kind, Kind::Finished | Kind::Question);
                if asked && answered.contains(&run.session) {
                    return None;
                }
                work_frame(self.grant_event(g, e), e, run)
            })
            .collect();
        let replies = workforce_desk::sees::shows(g.sees, workforce_desk::sees::Part::Reply);
        let unbound = self.desk.boss_unbound(party);
        let (sessions, requests): (Vec<Run>, Vec<Option<Frame>>) = runs
            .iter()
            .filter_map(|c| {
                let v = views.iter().find(|v| v.id == c.session)?;
                let item = items
                    .iter()
                    .find(|i| i.id == c.session.to_string())
                    .filter(|i| !self.raised_elsewhere(i, party))
                    .map(|i| super::served::seen_item(g, i.clone()));
                let request = v
                    .pending
                    .as_ref()
                    .filter(|p| {
                        p.answerable && p.request.is_some() && c.wheel == Wheel::Boss && !unbound
                    })
                    .map(|p| Frame {
                        id: p.id.to_string(),
                        subject: c.session,
                        line: super::served::request_line(g, v, p),
                        hop: 0,
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
                    answered: answered.contains(&c.session),
                    item,
                };
                Some((run, request))
            })
            .unzip();
        Ok((
            Taken {
                now: now.timestamp_millis(),
                after,
                sessions,
                events,
                asks,
                reports: reports.iter().map(Line::from).collect(),
                requests: requests.into_iter().flatten().collect(),
            },
            read,
        ))
    }
}

#[cfg(test)]
impl CrewHub {
    /// Every home here as if it stopped taking [`WORK_FRESH`] ago, and every
    /// grant here as if issued as long ago.
    pub(crate) fn homes_gone(&self) {
        let mut h = self.work_homes.lock();
        let long_ago = Instant::now()
            .checked_sub(WORK_FRESH)
            .unwrap_or_else(Instant::now);
        h.grace = long_ago;
        h.swept = None;
        h.took.values_mut().for_each(|at| *at = long_ago);
        h.aged_to = Some(Utc::now());
    }
}

// ── The Mac's side ──────────────────────────────────────────────────────

/// How a turn on a machine session went: whether it ran, and the
/// questions it couldn't settle there, for a turn again.
#[derive(Debug, Default)]
struct Turned {
    ran: bool,
    unsettled: Vec<Uuid>,
}

impl Turned {
    fn ran(ran: bool) -> Self {
        Self {
            ran,
            unsettled: Vec::new(),
        }
    }
}

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
    /// Where the next take reads from: past the latest event read.
    cursor: Option<ReadTo>,
    /// The events heard last ([`HEARD_KEPT`]): one a machine hands again,
    /// as it restarted, is not heard twice.
    seen: VecDeque<String>,
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
            seen: VecDeque::new(),
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
        for e in &t.events {
            if !self.seen.contains(&e.id) {
                self.seen.push_back(e.id.clone());
            }
        }
        while self.seen.len() > HEARD_KEPT {
            self.seen.pop_front();
        }
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

    /// A turn on `session` ended: its reports were taken if it ran, else
    /// given to a turn again `after` (a crew minute), as are the questions
    /// it couldn't settle on its machine.
    fn turned(&mut self, session: Uuid, reports: &[Uuid], t: &Turned, after: Duration) {
        self.turning.remove(&session);
        let again = Instant::now() + after;
        let unread = reports.iter().filter(|_| !t.ran);
        if t.ran {
            self.acked.extend_from_slice(reports);
        }
        for id in unread.chain(&t.unsettled) {
            self.offered.insert(id.to_string(), Some(again));
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
    /// (`planned`), hold a slot, as each machine's last take said.
    pub(crate) fn machine_runs_busy(&self, boss: &str, planned: impl Fn(&str) -> bool) -> usize {
        self.work_away
            .lock()
            .runs
            .iter()
            .filter(|(k, r)| r.holds_slot(boss) && !planned(k))
            .count()
    }

    /// What `t`, a take made `at`, says of `k`'s machine sessions run by
    /// `agent`, kept for the slots and the tools.
    fn away_heard(&self, k: &(String, String), agent: &str, t: &Taken, at: Instant) {
        let (machine, group) = k;
        let mut away = self.work_away.lock();
        away.runs.retain(|_, r| r.grant != *k);
        for s in &t.sessions {
            away.runs.insert(
                format!("{machine}/{}", s.id),
                AwayRun {
                    grant: k.clone(),
                    boss: agent.to_string(),
                    run: s.clone(),
                    at,
                },
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

    /// The sessions `boss` runs on machines, as the overview there shows
    /// them, each by `<machine>/<id>`: what its check-in watches with its
    /// sessions here ([`CrewHub::thresholds`]).
    pub(crate) fn machine_items(&self, boss: &str) -> Vec<crew::Item> {
        self.work_away
            .lock()
            .runs
            .iter()
            .filter_map(|(named, r)| {
                let group = &r.grant.1;
                let mut item = r.of(boss)?.item.clone()?;
                item.id.clone_from(named);
                item.group_id = (group != workforce_desk::UNGROUPED
                    && workforce_desk::grant::plan_of(group).is_none())
                .then(|| group.clone());
                item.group = None;
                Some(item)
            })
            .collect()
    }

    /// Whether the boss's question answerer on its machine has what the
    /// session `<machine>/<id>` asks.
    pub(crate) fn answered_on_machine(&self, named: &str) -> bool {
        self.work_away
            .lock()
            .runs
            .get(named)
            .and_then(|r| r.of(&r.boss))
            .is_some_and(|run| run.answered)
    }

    fn away_forget(&self, keep: impl Fn(&(String, String)) -> bool) {
        let mut away = self.work_away.lock();
        away.runs.retain(|_, r| keep(&r.grant));
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
        // Desk not relaying (its link dropped): what the machines last said
        // of their sessions still holds their slots, until a take says
        // otherwise.
        if !crew.relaying() {
            pumps.lock().clear();
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
        (p.cursor.clone(), p.acked.clone())
    };
    let input = match cursor {
        Some(r) => json!({ "after": r, "taken": sent }),
        None => json!({ "taken": sent }),
    };
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
                if let Some(r) = &t.after {
                    p.cursor = Some(r.clone());
                }
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
    crew.away_heard(&k, &agent, &t, started);
    for (session, work) in due {
        let Some(run) = t.sessions.iter().find(|s| s.id == session).cloned() else {
            pumps.lock().entry(k.clone()).or_default().turned(
                session,
                &[],
                &Turned::ran(true),
                crew.timing.minute,
            );
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
    let turned = crew
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
        .turned(run.id, &reports, &turned, crew.timing.minute);
}

impl CrewHub {
    /// Relays `tool` with `input` under `k`'s grant for `agent`: whether
    /// it reached the machine. A refusal is only logged, as for a line
    /// turn's own settling here.
    async fn relay_settle(
        &self,
        agent: &str,
        o: &Oversight,
        k: &(String, String),
        tool: &str,
        input: Value,
    ) -> bool {
        let ceiling = self.ceiling_now(agent, o);
        match self.relay(&k.0, &k.1, tool, input, agent, &ceiling).await {
            Ok(a) => {
                if a.status == ServedStatus::Refused {
                    tracing::info!(machine = %k.0, %tool, answer = %a.body, "a machine refused a boss's settling");
                }
                true
            }
            Err(why) => {
                tracing::info!(machine = %k.0, %tool, %why, "a boss's settling didn't reach its machine");
                false
            }
        }
    }

    /// Gives each question in `asks` to the person on its machine, saying
    /// `why`: those that didn't reach it.
    async fn machine_to_person(
        &self,
        agent: &str,
        o: &Oversight,
        k: &(String, String),
        asks: &[&Line],
        why: &str,
    ) -> Vec<Uuid> {
        let mut unsettled = Vec::new();
        for a in asks {
            let input = json!({"message_id": a.said.id.to_string(), "reason": why});
            if !self.relay_settle(agent, o, k, "crew.escalate", input).await {
                unsettled.push(a.said.id);
            }
        }
        unsettled
    }

    /// [`work_turn`]'s own: whether its turn ran, and what of its
    /// questions it couldn't settle on the machine.
    async fn work_turn_now(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        k: &(String, String),
        run: &Run,
        work: &mut Work,
    ) -> Turned {
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
            return Turned {
                ran: false,
                unsettled: self.machine_to_person(agent, o, k, &asked, &why).await,
            };
        }
        let hop = work.said.iter().map(|l| l.hop).max().unwrap_or(0);
        let mut unsettled = Vec::new();
        if let Some(why) = straight_to_person(o, hop, self.unbound(o)) {
            unsettled = self.machine_to_person(agent, o, k, &asked, why).await;
            work.said.retain(|l| l.said.kind != LineKind::Ask);
            if work.is_empty() {
                return Turned {
                    ran: true,
                    unsettled,
                };
            }
        }
        let Some(owner) = self.person(Some(service)) else {
            return Turned {
                ran: true,
                unsettled: self
                    .machine_to_person(
                        agent,
                        o,
                        k,
                        &asked,
                        "Desk has no conversation with you to work from yet.",
                    )
                    .await,
            };
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
            return Turned {
                ran: ran.is_ok(),
                unsettled,
            };
        }
        let ceiling = self.ceiling_now(agent, o);
        let peeked = self
            .relay(
                &k.0,
                &k.1,
                WORK_TAKE,
                json!({"peek": true}),
                agent,
                &ceiling,
            )
            .await;
        let open: Vec<Uuid> = match peeked {
            Ok(a) if a.status == ServedStatus::Ok => serde_json::from_value::<Taken>(a.body)
                .map(|t| {
                    t.asks
                        .into_iter()
                        .map(|l| l.said.id)
                        .filter(|id| asks.contains(id))
                        .collect()
                })
                .unwrap_or_default(),
            // Not read: what is still open is given to a turn again.
            _ => {
                unsettled.extend(asks);
                return Turned {
                    ran: ran.is_ok(),
                    unsettled,
                };
            }
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
            if !self.relay_settle(agent, o, k, tool, input).await {
                unsettled.push(*ask);
            }
        }
        Turned {
            ran: ran.is_ok(),
            unsettled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

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
            answered: false,
            item: None,
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
            after: None,
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
        p.turned(s, &[report.said.id], &Turned::ran(true), MINUTE);
        p.heard(&t);
        assert!(p.due().is_empty());
        assert_eq!(
            p.acked,
            vec![report.said.id],
            "acknowledged on the next take"
        );
    }

    #[test]
    fn an_event_a_machine_hands_again_is_acted_on_once() {
        let s = Uuid::new_v4();
        let taken = |events: Vec<Frame>| Taken {
            sessions: vec![run(s)],
            events,
            ..Taken::default()
        };
        let mut p = Pump::default();
        p.heard(&taken(vec![frame("e1", s)]));
        assert_eq!(p.due()[0].1.ended, vec!["e1 happened"]);
        p.turned(s, &[], &Turned::ran(true), MINUTE);
        // Its answer was lost: the next take hands both.
        p.heard(&taken(vec![frame("e1", s), frame("e2", s)]));
        assert_eq!(p.due()[0].1.ended, vec!["e2 happened"]);
        p.turned(s, &[], &Turned::ran(true), MINUTE);
        p.heard(&taken(vec![frame("e1", s), frame("e2", s)]));
        assert!(p.due().is_empty());
    }

    #[test]
    fn reports_a_turn_didnt_take_come_again_later_and_closed_ones_go() {
        let s = Uuid::new_v4();
        let report = line(s, LineKind::Tell);
        let t = Taken {
            now: 1,
            after: None,
            sessions: vec![run(s)],
            reports: vec![report.clone()],
            ..Taken::default()
        };
        let mut p = Pump::default();
        p.heard(&t);
        assert_eq!(p.due().len(), 1);
        p.turned(s, &[report.said.id], &Turned::ran(false), MINUTE);
        assert!(p.acked.is_empty());
        p.heard(&t);
        assert!(p.due().is_empty(), "not before a minute");
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
    fn a_question_a_turn_couldnt_settle_on_its_machine_comes_again_later() {
        let s = Uuid::new_v4();
        let (settled, lost) = (line(s, LineKind::Ask), line(s, LineKind::Ask));
        let t = Taken {
            now: 1,
            after: None,
            sessions: vec![run(s)],
            asks: vec![settled.clone(), lost.clone()],
            ..Taken::default()
        };
        let mut p = Pump::default();
        p.heard(&t);
        assert_eq!(p.due().len(), 1);
        let turned = Turned {
            ran: true,
            unsettled: vec![lost.said.id],
        };
        p.turned(s, &[], &turned, Duration::ZERO);
        p.heard(&t);
        let due = p.due();
        assert_eq!(due.len(), 1, "given again once due");
        assert_eq!(due[0].1.said, vec![lost], "only the one it couldn't settle");
        p.turned(s, &[], &Turned::ran(true), MINUTE);
        p.heard(&t);
        assert!(p.due().is_empty(), "settled: never again while it is open");
    }

    #[test]
    fn a_home_reads_while_it_takes_and_one_never_heard_only_at_first() {
        let mut h = Homes::default();
        let long_ago = Utc::now() - chrono::TimeDelta::hours(1);
        assert!(
            h.reads("home/desk", None),
            "just started: its home may take yet"
        );
        assert!(h.reads("home/desk", Some(("g", long_ago))));
        h.grace = Instant::now();
        assert!(!h.reads("home/desk", None));
        assert!(!h.reads("home/desk", Some(("g", long_ago))));
        // A grant just written, none taken under it yet: its home may take
        // under it yet, until it is no longer new.
        assert!(h.reads("home/desk", Some(("acme", Utc::now()))));
        let aged = Utc::now() - chrono::TimeDelta::from_std(WORK_FRESH).unwrap();
        assert!(!h.reads("home/desk", Some(("acme", aged))));
        h.took
            .insert(("home/desk".into(), "g".into()), Instant::now());
        assert!(h.reads("home/desk", None));
        assert!(h.reads("home/desk", Some(("g", long_ago))));
        assert!(
            !h.reads("home/desk", Some(("ungrouped", long_ago))),
            "taking under another grant alone"
        );
        h.took.insert(
            ("home/desk".into(), "g".into()),
            Instant::now() - WORK_FRESH,
        );
        assert!(!h.reads("home/desk", None), "its home stopped taking");
        assert!(!h.reads("home/desk", Some(("g", long_ago))));
        // Issued again since that take (written anew, or renewed): new.
        assert!(h.reads("home/desk", Some(("g", Utc::now()))));
        // Gone, then: its takes are forgotten.
        h.forget_gone(&HashSet::from(["ungrouped".to_string()]));
        assert!(h.took.is_empty());
    }

    #[test]
    fn a_run_is_its_bosss_only_while_its_take_is_fresh() {
        let away = |at: Instant| AwayRun {
            grant: ("devbox".into(), "acme".into()),
            boss: "home/desk".into(),
            run: run(Uuid::new_v4()),
            at,
        };
        assert!(away(Instant::now()).of("home/desk").is_some());
        assert!(away(Instant::now()).of("home/sarah").is_none());
        assert!(
            away(Instant::now() - WORK_FRESH).of("home/desk").is_none(),
            "a take that old says nothing of now"
        );
        // Its slot is still held until a take says otherwise.
        assert!(away(Instant::now() - WORK_FRESH).holds_slot("home/desk"));
    }

    #[test]
    fn a_machine_that_doesnt_serve_a_call_is_said_to_need_updating() {
        let refused = |error: &str| super::super::relay::RelayAnswer {
            status: ServedStatus::Refused,
            body: json!({ "error": error }),
        };
        assert_eq!(
            super::super::relay::older("devbox", WORK_TAKE, &refused(&super::super::served::not_served(WORK_TAKE))),
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
            after: Some(ReadTo {
                at: Utc::now(),
                id: "e".into(),
            }),
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

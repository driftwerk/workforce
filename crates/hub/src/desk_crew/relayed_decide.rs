//! A boss relayed from the home at Act fully: the way back for its
//! decisions (designs/relayed-boss-act-fully-design.md). The home's own
//! overseer bosses a machine's sessions through the relay ([`super::relay`]),
//! which runs one way: the Mac's hub queues a call, Desk's native side
//! carries it to the machine, and the machine's answer comes back. Its
//! decisions ride the same relay, the Mac asking.
//!
//! - **On the machine** a request of a session the relayed boss runs is put
//!   in an outbox, its prompt written as any boss's is ([`super::decide`]),
//!   and waits for its answer, as a remote boss's does on its uplink
//!   ([`super::remote_decide`]).
//! - **On the Mac**, while Act fully is in force for the overseer there, its
//!   pump relays [`TAKE`] under each of its grants on each machine (its
//!   group's, or each of its plans', [`CrewHub::machine_grants_of`]) every
//!   second. Each
//!   take hands back what waits, once, and says the home holds it at Act
//!   fully. The Mac makes one decision call on the overseer's own model for
//!   each, as for a local boss, and relays [`DECIDED`] with its text.
//! - **Both must hold.** The machine unbinds the boss only while its own
//!   grant puts it at Act fully (as a remote boss's,
//!   [`CrewHub::served_full`]) and the home said so within [`FRESH`]. A take
//!   saying it isn't in force ends it at once, and what waits is the
//!   person's. Whatever is decided is judged by the machine as it is done,
//!   under its grant, budget and Stop.
//!
//! Both tools are the Mac hub's own: no crew tool sends them, and the
//! machine takes them only on its native relay route, from `local:<agent>`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::grant::BossRef;

use super::left::Left;
use super::remote_decide::Failed;
use super::served::{Ceiling, ServedStatus};
use super::CrewHub;
use crate::boss_links::decide::{Code, MAX_PROMPT};
use crate::serve::AgentService;

/// The take the Mac relays to a machine for what its boss is to decide.
pub const TAKE: &str = "crew.decisions_take";
/// The answer the Mac relays to a machine for one decision.
pub const DECIDED: &str = "crew.decided";

/// How long the home's word that Act fully is in force for its boss holds
/// on a machine.
pub(crate) const FRESH: Duration = Duration::from_secs(5);
/// How often the Mac takes from a machine while Act fully is in force.
const TAKE_EVERY: Duration = Duration::from_secs(1);
/// How often, when the machine says its grant doesn't put the boss at Act
/// fully, or can't be reached.
const IDLE_EVERY: Duration = Duration::from_secs(10);
/// How long after Act fully lapses, or after its last notice for the person
/// since, the Mac still takes from the machine for what it left to them.
const NOTED_FOR: Duration = Duration::from_secs(60);
/// The longest a machine that can't be reached waits between takes, each
/// that fails in a row waiting twice as long as the last, from
/// [`IDLE_EVERY`].
const MOST_IDLE: Duration = Duration::from_secs(300);
/// The takes in a row that may fail before a machine is no longer taken
/// from for what it left the person, until Act fully is in force again.
const GIVEN_UP_AFTER: u32 = 6;
/// The most decisions one take hands out: no more than the Mac has room
/// to decide at once, and fewer when it says so (`most`).
const MOST_TAKEN: usize = AT_ONCE;
/// The most decisions waiting for one boss and group on a machine.
const MOST_WAITING: usize = 32;
/// The most decision calls the Mac makes for one machine's group a minute,
/// and at once.
const PER_MINUTE: usize = 30;
const AT_ONCE: usize = 3;
/// The longest answer taken, in characters.
const MAX_ANSWER: usize = 4_000;
/// The oldest a take may be when it reaches the machine, by the time the
/// Mac sent it (`sentAt`): one later than this (a courier held up) says
/// nothing of Act fully now. Within it, its word holds [`FRESH`] from when
/// it was sent, never from when it came. Assumes the two clocks agree to
/// within a second or so (both keep network time); a Mac clock behind the
/// machine's only binds the boss sooner.
const TAKE_AGE: Duration = Duration::from_millis(2_500);
/// How long the overseer's model may take: 5 s under the machine's wait.
const HERE_WITHIN: Duration = super::decide::WITHIN.saturating_sub(Duration::from_secs(5));

/// How long a machine's answer that it counts the shared budget
/// ([`super::shared_budget`]) lets the Mac say Act fully is in force there.
const CONFIRMED_FOR: Duration = Duration::from_secs(10);

/// A boss and group, as the machine keys its relayed decisions: the
/// boss's party (`home/<agent>`) and the group's id.
type Key = (String, String);

/// Whether `party` is a boss relayed from the home (`home/<agent>`).
pub(crate) fn relayed_party(party: &str) -> bool {
    party
        .strip_prefix(workforce_desk::grant::HOME_HUB)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The home's last word that Act fully is in force for a boss and group,
/// with what its one daily budget counts (designs/act-fully-machines-design.md).
#[derive(Debug, Clone)]
struct Home {
    /// When the home sent it.
    at: Instant,
    /// Where the home counts today from.
    from: chrono::DateTime<chrono::Utc>,
    /// What its sessions here spent since `from`, as read for that take (or
    /// since, [`CrewHub::refresh_relayed_spent`]).
    here: f64,
    /// What the home said was spent everywhere else since `from`.
    elsewhere: f64,
    /// What the home counted of each of the boss's other grants here, by
    /// group: in `elsewhere`, and perhaps behind what they said since.
    others: HashMap<String, f64>,
    /// What it takes beyond requests.
    takes: HashSet<Call>,
}

/// The most any of a boss's fresh words, by grant, counts as spent: its
/// grant's spend here, what the home counted everywhere else, and what each
/// of its other grants here spent past what that word counted of it.
fn most_spent(fresh: &[(&str, &Home)]) -> Option<f64> {
    fresh
        .iter()
        .map(|(g, h)| {
            let ahead: f64 = fresh
                .iter()
                .filter(|(o, _)| o != g)
                .map(|(o, x)| (x.here - h.others.get(*o).copied().unwrap_or(0.0)).max(0.0))
                .sum();
            h.here + h.elsewhere + ahead
        })
        .reduce(f64::max)
}

/// What a relayed boss's decision call is for: a session's request, a
/// question it asks, or the plan it presents ([`super::answer`]). The home
/// answers each with its own instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Call {
    Request,
    Question,
    Plan,
}

impl Call {
    /// What a home takes beyond requests, which every home takes: what its
    /// take says it does, so a machine hands an older home none of the
    /// others.
    pub(crate) const BEYOND: [Self; 2] = [Self::Question, Self::Plan];

    /// How a take names it: a request's is said as none, as an older
    /// machine's is.
    fn said(self) -> Option<&'static str> {
        match self {
            Self::Request => None,
            Self::Question => Some("question"),
            Self::Plan => Some("plan"),
        }
    }

    fn of(kind: Option<&str>) -> Option<Self> {
        match kind {
            None => Some(Self::Request),
            Some(k) => Self::BEYOND.into_iter().find(|c| c.said() == Some(k)),
        }
    }

    pub(crate) fn system(self) -> &'static str {
        match self {
            Self::Request => super::decide::SYSTEM,
            Self::Question => super::answer::SYSTEM,
            Self::Plan => super::answer::PLAN_SYSTEM,
        }
    }
}

/// The most notices for the person a machine keeps for one boss until its
/// home takes them.
const MOST_NOTES: usize = 16;
/// How long each word of a notice for the person may be.
const MAX_NOTE: usize = 1_000;
/// The most notices the Mac says in one boss's DM a minute: the rest wait
/// on the machine for later takes.
const NOTES_PER_MINUTE: usize = 6;
/// How long the Mac remembers a notice it said, so one a later take hands
/// back again is said once.
const NOTE_KEPT: Duration = Duration::from_secs(600);

/// What a boss relayed from its home leaves to the person, as its machine
/// keeps it for the home's takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Note {
    pub id: Uuid,
    #[serde(flatten)]
    pub left: Left,
}

/// It with each word capped, its secrets hidden.
fn tidied(left: Left) -> Left {
    left.with_text(|t| workforce_desk::sees::capped(&crate::redact::hide_secrets(&t), MAX_NOTE))
}

/// What a machine holds of its relayed bosses' decisions.
#[derive(Default)]
pub(crate) struct Outbox {
    /// When the home last said Act fully is in force for each, and what
    /// was spent.
    homes: HashMap<Key, Home>,
    /// What waits to be taken, oldest first.
    queued: HashMap<Key, VecDeque<(Uuid, String, Call)>>,
    /// What each boss (`home/<agent>`) leaves to the person, oldest first,
    /// and whether a take handed it yet: each of its home's takes hands it
    /// until one says it was taken.
    notes: HashMap<String, VecDeque<(Note, bool)>>,
    /// How many of each boss's notices, never handed, made room for newer
    /// ones: each count said as a notice of its own until a take says it
    /// was taken, and one handed already never grows.
    crowded: HashMap<String, Vec<(Note, bool)>>,
    /// Who waits for each answer, and whose it is.
    waiting: HashMap<Uuid, (Key, oneshot::Sender<Result<String, Failed>>)>,
    /// What a take handed out: only these are answered.
    taken: HashSet<Uuid>,
    /// When the Mac sent the latest take heard for each (its `sentAt`).
    latest: HashMap<Key, i64>,
    /// The irreversible jobs the home said its boss holds, with its latest
    /// answer for each (none when it didn't say): what a request here is
    /// compared with before it is allowed (R5). Each decision the home
    /// sends for the boss and group replaces it, whichever request it was
    /// for: each is the home's newest reading of the same record.
    held: HashMap<Key, Option<Vec<workforce_desk::assignment::Assignment>>>,
}

/// A notice kept for a take, as it is handed out.
fn handed((note, handed): &mut (Note, bool)) -> Note {
    *handed = true;
    note.clone()
}

impl Outbox {
    /// Keeps `left` for `party`'s home to take. Once [`MOST_NOTES`] wait,
    /// the oldest never handed makes room (or `left` itself, when each was
    /// handed and waits to be said taken), and is counted, never lost
    /// unsaid.
    fn leave(&mut self, party: &str, left: Left) {
        let note = Note {
            id: Uuid::new_v4(),
            left,
        };
        let notes = self.notes.entry(party.to_string()).or_default();
        if notes.len() < MOST_NOTES {
            notes.push_back((note, false));
            return;
        }
        if let Some(oldest) = notes.iter().position(|(_, handed)| !handed) {
            notes.remove(oldest);
            notes.push_back((note, false));
        }
        let counts = self.crowded.entry(party.to_string()).or_default();
        match counts.last_mut().filter(|(_, handed)| !handed) {
            Some((
                Note {
                    left: Left::Crowded { more },
                    ..
                },
                _,
            )) => *more += 1,
            _ => counts.push((
                Note {
                    id: Uuid::new_v4(),
                    left: Left::Crowded { more: 1 },
                },
                false,
            )),
        }
    }

    /// What `party`'s home takes now, once those it says it took (`said`)
    /// are gone, each handed from now: each notice, then each count of
    /// those that made room.
    fn notes_of(&mut self, party: &str, said: &HashSet<Uuid>) -> Vec<Note> {
        let mut out = Vec::new();
        if let Some(n) = self.notes.get_mut(party) {
            n.retain(|(note, _)| !said.contains(&note.id));
            out.extend(n.iter_mut().map(handed));
        }
        if let Some(c) = self.crowded.get_mut(party) {
            c.retain(|(note, _)| !said.contains(&note.id));
            out.extend(c.iter_mut().map(handed));
        }
        self.notes.retain(|_, n| !n.is_empty());
        self.crowded.retain(|_, c| !c.is_empty());
        out
    }

    fn forget(&mut self, id: Uuid) {
        self.waiting.remove(&id);
        self.taken.remove(&id);
        for q in self.queued.values_mut() {
            q.retain(|(i, ..)| *i != id);
        }
        self.queued.retain(|_, q| !q.is_empty());
    }

    /// Ends `key`'s Act fully: what waits for it hears why.
    fn end(&mut self, key: &Key) {
        self.homes.remove(key);
        self.queued.remove(key);
        let ids: Vec<Uuid> = self
            .waiting
            .iter()
            .filter(|(_, (k, _))| k == key)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.taken.remove(&id);
            if let Some((_, tx)) = self.waiting.remove(&id) {
                let _ = tx.send(Err(not_in_force()));
            }
        }
    }
}

fn not_in_force() -> Failed {
    Failed {
        why: "Act fully isn't in force for it at home".into(),
        code: Some(Code::NotInForce),
    }
}

/// Forgets a decision as its call ends, however it ends.
struct Forget<'a> {
    crew: &'a CrewHub,
    id: Uuid,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.crew.relayed.lock().forget(self.id);
    }
}

pub(super) fn key_of(boss: &BossRef, group: &str) -> Option<Key> {
    matches!(boss, BossRef::Local { .. }).then(|| (boss.party_id(), group.to_string()))
}

// ── The machine's side ──────────────────────────────────────────────────

impl CrewHub {
    /// The irreversible jobs the home said, with its latest answer, that
    /// `party` (`home/<agent>`) holds, for `group`'s requests: none when it
    /// didn't say.
    pub(crate) fn relayed_held(
        &self,
        party: &str,
        group: &str,
    ) -> Option<Vec<workforce_desk::assignment::Assignment>> {
        self.relayed
            .lock()
            .held
            .get(&(party.to_string(), group.to_string()))
            .cloned()
            .flatten()
    }

    /// Whether the home said, lately, that Act fully is in force for
    /// `party` (`home/<agent>`) in `group`, and that it takes `call`.
    pub(crate) fn home_takes(&self, party: &str, group: &str, call: Call) -> bool {
        self.relayed
            .lock()
            .homes
            .get(&(party.to_string(), group.to_string()))
            .is_some_and(|h| {
                h.at.elapsed() < FRESH && (call == Call::Request || h.takes.contains(&call))
            })
    }

    /// Whether the home said, lately, that Act fully is in force for
    /// `party` (`home/<agent>`) in `group`.
    pub(crate) fn home_holds_full(&self, party: &str, group: &str) -> bool {
        self.home_takes(party, group, Call::Request)
    }

    /// What Act fully's one budget counts as spent today for `party`
    /// (`home/<agent>`), by its home's words that are still fresh: each is
    /// one grant's sessions' spend here and what the home counts everywhere
    /// else, its other grants here included, with what those have spent
    /// here past what the home last counted of them; so the most of them.
    /// None without one, so it doesn't act fully.
    pub(crate) fn relayed_spent(&self, party: &str) -> Option<f64> {
        let out = self.relayed.lock();
        let fresh: Vec<(&str, &Home)> = out
            .homes
            .iter()
            .filter(|((p, _), h)| p == party && h.at.elapsed() < FRESH)
            .map(|((_, g), h)| (g.as_str(), h))
            .collect();
        most_spent(&fresh)
    }

    /// What `group`'s sessions here spent since `from` in real money (calls
    /// a key paid for, never the person's plans); none when it can't be
    /// read.
    async fn group_spent(&self, group: &str, from: chrono::DateTime<chrono::Utc>) -> Option<f64> {
        let now = chrono::Utc::now();
        let filter = workforce_desk::ledger_store::Filter {
            seen: Some(
                self.desk
                    .seen_by(&workforce_desk::grant::scope_of_group(group)),
            ),
            ..Default::default()
        };
        match self.desk.billed_usd(from.min(now), now, &filter).await {
            Ok(usd) => super::shared_budget::usd_of(&json!(usd)),
            Err(e) => {
                tracing::warn!(%group, error = %e, "couldn't read a relayed boss's spend; Act fully waits until it can");
                None
            }
        }
    }

    /// Reads again what `party`'s sessions here spent, for each of its
    /// home's words that is still fresh, from where that word counts: after
    /// a decision, before its verdict is judged. One that can't be read
    /// ends the word.
    pub(crate) async fn refresh_relayed_spent(&self, party: &str) {
        let fresh: Vec<(Key, chrono::DateTime<chrono::Utc>)> = self
            .relayed
            .lock()
            .homes
            .iter()
            .filter(|((p, _), h)| p == party && h.at.elapsed() < FRESH)
            .map(|(k, h)| (k.clone(), h.from))
            .collect();
        for (key, from) in fresh {
            let read = self.group_spent(&key.1, from).await;
            let mut out = self.relayed.lock();
            match read {
                Some(usd) => {
                    if let Some(h) = out.homes.get_mut(&key).filter(|h| h.from == from) {
                        h.here = h.here.max(usd);
                    }
                }
                None => out.end(&key),
            }
        }
    }

    /// Keeps what the boss `party`, relayed from the home, leaves to the
    /// person, for its home's takes to say in the boss's DM there; the
    /// oldest goes first once [`MOST_NOTES`] wait.
    pub(crate) fn relayed_note(&self, party: &str, left: Left) {
        self.relayed.lock().leave(party, tidied(left));
    }

    /// One decision call by `boss` (`home/<agent>`), relayed from the home,
    /// for a session of `group`, on `prompt`, for `call`: the text its model
    /// answered at home, or why there is none. Its secrets are hidden
    /// before it leaves. The caller bounds how long it waits.
    pub(crate) async fn relayed_decision_call(
        &self,
        boss: &str,
        group: &str,
        prompt: &str,
        call: Call,
    ) -> Result<String, Failed> {
        let prompt = crate::redact::hide_secrets(prompt);
        if prompt.chars().count() > MAX_PROMPT {
            return Err(Failed {
                why: "the request is too long for its home to decide".into(),
                code: Some(Code::TooLong),
            });
        }
        let key = (boss.to_string(), group.to_string());
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        {
            let mut out = self.relayed.lock();
            if out.homes.get(&key).is_none_or(|h| h.at.elapsed() >= FRESH) {
                return Err(Failed {
                    why: "its home isn't connected".into(),
                    code: Some(Code::Link),
                });
            }
            let q = out.queued.entry(key.clone()).or_default();
            if q.len() >= MOST_WAITING {
                return Err(Failed {
                    why: "its home has too many decisions waiting just now".into(),
                    code: Some(Code::Paced),
                });
            }
            q.push_back((id, prompt, call));
            out.waiting.insert(id, (key, tx));
        }
        let _forget = Forget { crew: self, id };
        rx.await.unwrap_or_else(|_| {
            Err(Failed {
                why: "its home's link went down".into(),
                code: Some(Code::Link),
            })
        })
    }

    /// The home's take for `boss`'s decisions in `group` ([`TAKE`]): it
    /// says whether Act fully is in force for it there (`inForce`, at the
    /// level `ceiling` says), where today's count starts there
    /// (`countFrom`) and what was spent everywhere but this grant here
    /// (`spentElsewhereUsd`), and is handed what waits, each once. Its
    /// answer says what the grant's sessions here spent since `countFrom`
    /// (`spentUsd`), for the home to count ([`super::shared_budget`]).
    pub(crate) async fn take_relayed(
        &self,
        boss: &BossRef,
        group: &str,
        input: &Value,
        ceiling: &Ceiling,
    ) -> (ServedStatus, Value) {
        let Some(key) = key_of(boss, group) else {
            return refused("only the home's own boss takes its decisions");
        };
        let ours = self
            .desk
            .grant_live(group)
            .is_ok_and(|g| g.boss.as_ref() == Some(boss));
        // When it came, before anything is read: its word holds from when
        // it was sent, never from when its reading ended.
        let came = Instant::now();
        let sent_at = input["sentAt"].as_i64();
        // How long ago the Mac sent it; none for one too old, from the
        // future, or that doesn't say.
        let age = input["sentAt"]
            .as_i64()
            .map(|at| chrono::Utc::now().timestamp_millis() - at)
            .filter(|ms| (-1_000..TAKE_AGE.as_millis() as i64).contains(ms))
            .map(|ms| Duration::from_millis(ms.max(0) as u64));
        // Where the home counts today from: not ahead of the clock, and not
        // over a month back.
        let now = chrono::Utc::now();
        let from = input["countFrom"]
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .filter(|f| *f <= now + chrono::Duration::seconds(5))
            .filter(|f| now - *f <= chrono::Duration::days(32));
        let here = match (ours, from) {
            (true, Some(f)) => self.group_spent(group, f).await,
            _ => None,
        };
        let elsewhere = super::shared_budget::usd_of(&input["spentElsewhereUsd"]);
        // A count that can't be read is none: what that grant spent here
        // is then all counted again, never left out.
        let others: HashMap<String, f64> = input["grantsUsd"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(g, v)| Some((g.clone(), super::shared_budget::usd_of(v)?)))
            .collect();
        let in_force = ours
            && ceiling.level == Level::Full
            && input["inForce"].as_bool() == Some(true)
            && age.is_some()
            && here.is_some()
            && elsewhere.is_some();
        let takes: HashSet<Call> = input["takes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| Call::of(Some(k.as_str()?)))
            .collect();
        let most = input["most"]
            .as_u64()
            .map_or(MOST_TAKEN, |m| (m as usize).min(MOST_TAKEN));
        let decisions: Vec<Value> = {
            let mut out = self.relayed.lock();
            // Homes not heard from within FRESH: forgotten.
            out.homes.retain(|_, h| h.at.elapsed() < FRESH);
            // A take the Mac sent before one already heard (held up on its
            // way, or while its spend was read) says nothing of now. A Mac
            // clock that jumps back makes its takes look late for as long:
            // Act fully there lapses within FRESH, which fails safe.
            let latest = out.latest.get(&key).copied();
            let stale = sent_at.is_some_and(|at| latest.is_some_and(|l| at < l));
            // Only a take sent lately counts as the latest: one from the future
            // would hold off every take after it.
            if let Some(at) = sent_at.filter(|_| !stale && age.is_some()) {
                out.latest.insert(key.clone(), at);
            }
            if stale {
                Vec::new()
            } else if !in_force {
                out.end(&key);
                Vec::new()
            } else {
                let sent = age.and_then(|a| came.checked_sub(a)).unwrap_or(came);
                if let (Some(from), Some(here), Some(elsewhere)) = (from, here, elsewhere) {
                    out.homes.insert(
                        key.clone(),
                        Home {
                            at: sent,
                            from,
                            here,
                            elsewhere,
                            others,
                            takes,
                        },
                    );
                }
                let mut handed = Vec::new();
                while handed.len() < most {
                    let Some((id, prompt, call)) =
                        out.queued.get_mut(&key).and_then(VecDeque::pop_front)
                    else {
                        break;
                    };
                    if out.waiting.contains_key(&id) {
                        out.taken.insert(id);
                        let mut d = json!({"decideId": id, "prompt": prompt});
                        if let Some(kind) = call.said() {
                            d["kind"] = json!(kind);
                        }
                        handed.push(d);
                    }
                }
                out.queued.retain(|_, q| !q.is_empty());
                handed
            }
        };
        if !ours {
            return refused(&workforce_desk::scope::not_ours(None));
        }
        let said: HashSet<Uuid> = input["notesTaken"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str()?.parse().ok())
            .collect();
        let notes = self.relayed.lock().notes_of(&key.0, &said);
        let full = in_force && self.served_full(&key.0).is_some();
        let mut body = json!({ "full": full, "decisions": decisions, "notes": notes });
        if let (Some(f), Some(usd)) = (from, here) {
            body["countFrom"] = json!(f.timestamp_millis());
            body["spentUsd"] = json!(usd);
        }
        (ServedStatus::Ok, body)
    }

    /// The home's answer for one of `boss`'s decisions in `group`
    /// ([`DECIDED`]): taken once, and only for one a take handed to that
    /// boss and group. Recorded as the boss's call.
    pub(crate) async fn relayed_decided(
        &self,
        boss: &BossRef,
        group: &str,
        input: &Value,
    ) -> (ServedStatus, Value) {
        let Some(key) = key_of(boss, group) else {
            return refused("only the home's own boss answers its decisions");
        };
        let id = input["decideId"]
            .as_str()
            .and_then(|i| i.parse::<Uuid>().ok());
        let answer = match (input["text"].as_str(), input["error"].as_str()) {
            (Some(t), _) => Ok(t.chars().take(MAX_ANSWER).collect::<String>()),
            (None, Some(e)) => Err(Failed {
                why: workforce_desk::crew::one_line(e, 160),
                code: input["code"].as_str().and_then(Code::parse),
            }),
            (None, None) => Err(Failed::from(
                "its home's answer couldn't be read".to_string(),
            )),
        };
        let said = match &answer {
            Ok(t) => workforce_desk::sees::capped(t, 500),
            Err(f) => f.why.clone(),
        };
        let held = input
            .get("heldJobs")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let taken = id.and_then(|id| {
            let mut out = self.relayed.lock();
            out.held.insert(key.clone(), held);
            let ours =
                out.waiting.get(&id).is_some_and(|(k, _)| *k == key) && out.taken.contains(&id);
            if !ours {
                return None;
            }
            // Its home not heard from within FRESH: what it decided isn't
            // taken as said now.
            let fresh = out.homes.get(&key).is_some_and(|h| h.at.elapsed() < FRESH);
            out.taken.remove(&id);
            out.waiting.remove(&id).map(|(_, tx)| (tx, fresh))
        });
        let outcome = match taken {
            Some((tx, fresh)) => {
                let _ = tx.send(if fresh { answer } else { Err(not_in_force()) });
                "ok"
            }
            None => "refused",
        };
        let row = workforce_desk::grant_store::CallRow {
            caller: format!("boss:{}", key.0),
            boss: Some(boss.key()),
            group: Some(group.to_string()),
            tool: DECIDED,
            input: json!({"decideId": id,
                          "code": input["code"].as_str().and_then(Code::parse).map(Code::as_str)})
            .to_string(),
            input_hash: {
                use sha2::Digest;
                hex::encode(sha2::Sha256::digest(input.to_string().as_bytes()))
            },
            outcome,
            said: Some(said),
            result: json!({}),
        };
        if let Err(e) = self.desk.store().boss_call(&row).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "a relayed decision's answer wasn't recorded"
            );
        }
        match outcome {
            "ok" => (ServedStatus::Ok, json!({"taken": true})),
            _ => refused("no decision waits for that answer"),
        }
    }
}

pub(super) fn refused(why: &str) -> (ServedStatus, Value) {
    (ServedStatus::Refused, json!({ "error": why }))
}

// ── The Mac's side ──────────────────────────────────────────────────────

/// One machine's group, as the Mac's pump keeps it.
struct Pump {
    /// When to take next.
    next: Instant,
    /// A take is out.
    busy: bool,
    /// The last take said Act fully is in force.
    said: bool,
    /// Until when it is still taken, Act fully having lapsed, for the
    /// notices the machine leaves the person ([`NOTED_FOR`]).
    noting: Option<Instant>,
    /// When the machine last answered with its spend for the count that
    /// starts then ([`super::shared_budget`]): only a machine that counts
    /// the one budget is told Act fully is in force, within
    /// [`CONFIRMED_FOR`] of its answer.
    counts: Option<(chrono::DateTime<chrono::Utc>, Instant)>,
    /// The decision calls made lately, and the room for them at once.
    calls: VecDeque<Instant>,
    at_once: Arc<tokio::sync::Semaphore>,
    /// The notices for the person the last takes handed that were said,
    /// for the next take to say were taken.
    notes_taken: Vec<Uuid>,
    /// The machine may keep notices for the person not yet said taken:
    /// its last take handed some, or none reached it yet.
    notes_waiting: bool,
    /// The takes in a row that didn't reach the machine, and when the last
    /// of them began.
    failed: u32,
    unreached: Option<Instant>,
}

/// How long after a take a machine is taken from again while Act fully
/// isn't in force, `failed` takes in a row not reaching it.
fn idle_after(failed: u32) -> Duration {
    match failed {
        0 => IDLE_EVERY,
        n => IDLE_EVERY
            .saturating_mul(1 << (n - 1).min(8))
            .min(MOST_IDLE),
    }
}

/// The notices for the person the Mac said lately, from every machine.
#[derive(Default)]
struct Told {
    /// Each by its id, when it was said.
    said: HashMap<Uuid, Instant>,
    /// When each boss's DM was last said one in.
    at: HashMap<String, VecDeque<Instant>>,
}

impl Told {
    /// Whether `note` is to be said in `agent`'s DM now: not said already,
    /// and within [`NOTES_PER_MINUTE`]. One that is, is counted said.
    fn says(&mut self, agent: &str, note: Uuid, now: Instant) -> bool {
        self.said
            .retain(|_, at| now.duration_since(*at) < NOTE_KEPT);
        if self.said.contains_key(&note) {
            return false;
        }
        let at = self.at.entry(agent.to_string()).or_default();
        while at
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            at.pop_front();
        }
        if at.len() >= NOTES_PER_MINUTE {
            return false;
        }
        at.push_back(now);
        self.said.insert(note, now);
        true
    }

    fn said(&self, note: Uuid) -> bool {
        self.said.contains_key(&note)
    }
}

impl Pump {
    /// How many decision calls there is room for now: at once, and within
    /// the minute's.
    fn room(&mut self, now: Instant) -> usize {
        while self
            .calls
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            self.calls.pop_front();
        }
        self.at_once
            .available_permits()
            .min(PER_MINUTE.saturating_sub(self.calls.len()))
    }
}

impl Pump {
    /// Whether to take now. Once Act fully lapses here the machine is told
    /// at once; after, it is still taken now and then for a while, for what
    /// was left to the person there, and while what it left waits.
    fn due(&self, in_force: bool, now: Instant) -> bool {
        let lapsed = !in_force && self.said;
        let left = self.notes_waiting || !self.notes_taken.is_empty();
        let wanted = in_force
            || self.noting.is_some_and(|until| now < until)
            || (left && self.failed < GIVEN_UP_AFTER);
        !self.busy && (lapsed || (wanted && now >= self.next))
    }

    /// The machine answered a call `at`, a take or any other: one that
    /// couldn't be reached since is taken from again now, given up on or not.
    fn reached(&mut self, at: Instant, now: Instant) {
        if self.unreached.is_some_and(|u| at > u) {
            self.failed = 0;
            self.unreached = None;
            self.next = self.next.min(now);
        }
    }
}

impl Default for Pump {
    fn default() -> Self {
        Self {
            next: Instant::now(),
            busy: false,
            said: false,
            noting: None,
            counts: None,
            calls: VecDeque::new(),
            at_once: Arc::new(tokio::sync::Semaphore::new(AT_ONCE)),
            notes_taken: Vec::new(),
            notes_waiting: true,
            failed: 0,
            unreached: None,
        }
    }
}

type Pumps = Arc<Mutex<HashMap<(String, String), Pump>>>;

impl CrewHub {
    /// The machines named in `group`'s sidebar items here.
    pub(crate) fn machines_in(&self, group: &str) -> Vec<String> {
        let layout = self.desk.layout();
        let mut out: Vec<String> = layout
            .groups
            .iter()
            .filter(|x| x.id == group)
            .flat_map(|x| x.items.iter())
            .filter_map(|i| i.strip_prefix("session:"))
            .filter_map(super::relay::machine_session)
            .map(|(m, _)| m.to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether Act fully is in force for the overseer `agent` here, as a
    /// relayed boss's decisions need: its level now, its budget read and
    /// not spent, and nothing of Desk's holding it.
    pub(crate) fn relay_in_force(&self, agent: &str, o: &Oversight) -> bool {
        self.level_now(agent, o).0 == Level::Full
            && self.unbound_given(agent, o)
            && self.desk.agent_held(agent).is_none()
    }
}

/// The Mac's pump: while Desk relays, each overseer with grants on the
/// person's machines takes its decisions under each, as the module says,
/// and says so when Act fully ends.
pub(crate) async fn pump_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let pumps: Pumps = Arc::default();
    let told: Arc<Mutex<Told>> = Arc::default();
    let mut tick = tokio::time::interval(TAKE_EVERY / 4);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        // What each pump keeps (the notices said and still to be said taken)
        // waits for the link to come back.
        if !crew.relaying() {
            continue;
        }
        let mut seen = HashSet::new();
        for (service, o) in crew.overseers() {
            let agent = service.agent_id.as_str().to_string();
            let grants = crew.machine_grants_of(&agent, &o);
            if grants.is_empty() {
                continue;
            }
            let in_force = crew.relay_in_force(&agent, &o);
            for k in grants {
                seen.insert(k.clone());
                {
                    let now = Instant::now();
                    let mut p = pumps.lock();
                    let p = p.entry(k.clone()).or_default();
                    if let Some(at) = crew.machine_reached(&k.0) {
                        p.reached(at, now);
                    }
                    if !p.due(in_force, now) {
                        continue;
                    }
                    p.busy = true;
                }
                tokio::spawn(take(
                    Arc::clone(&crew),
                    Arc::clone(&service),
                    o.clone(),
                    k,
                    in_force,
                    Arc::clone(&pumps),
                    Arc::clone(&told),
                ));
            }
        }
        // A machine's grant no overseer here takes from now: dropped. Its
        // machine stops holding it within [`FRESH`].
        pumps.lock().retain(|k, p| seen.contains(k) || p.busy);
    }
}

/// One take from `k`'s machine under its grant, and the decisions it
/// hands back, each decided and answered.
async fn take(
    crew: Arc<CrewHub>,
    service: Arc<AgentService>,
    o: Oversight,
    k: (String, String),
    in_force: bool,
    pumps: Pumps,
    told: Arc<Mutex<Told>>,
) {
    let (machine, group) = &k;
    let agent = service.agent_id.as_str().to_string();
    let ceiling = crew.ceiling_now(&agent, &o);
    let started = Instant::now();
    // One budget across this Mac and every machine: where today's count
    // starts here, what was spent everywhere but this grant there, and what
    // of that is the boss's other grants there.
    let from = crew.count_from(&o);
    let elsewhere = from.and_then(|f| crew.spent_elsewhere(&agent, machine, group, f));
    let others = from.map(|f| crew.grants_spent_on(&agent, machine, group, f));
    // As many as there is room to decide now: none is refused for pace.
    // In force there only once the machine said it counts this budget, so
    // an engine that counts only its own spend never acts fully.
    let (most, counts, notes_taken) = {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        let counts = p
            .counts
            .is_some_and(|(f, at)| Some(f) == from && started.duration_since(at) < CONFIRMED_FOR);
        (p.room(started), counts, p.notes_taken.clone())
    };
    let says = in_force && counts && elsewhere.is_some();
    let mut input = json!({
        "inForce": says,
        "sentAt": chrono::Utc::now().timestamp_millis(),
        "most": most,
        "takes": Call::BEYOND.map(Call::said),
        "notesTaken": notes_taken,
    });
    if let Some(f) = from {
        input["countFrom"] = json!(f.timestamp_millis());
    }
    if let Some(e) = elsewhere {
        input["spentElsewhereUsd"] = json!(e);
    }
    if let Some(o) = others {
        input["grantsUsd"] = json!(o);
    }
    let got = crew
        .relay(machine, group, TAKE, input, &agent, &ceiling)
        .await;
    let (full, decisions) = match &got {
        Ok(a) if a.status == ServedStatus::Ok => (
            says && a.body["full"].as_bool() == Some(true),
            a.body["decisions"].as_array().cloned().unwrap_or_default(),
        ),
        _ => (false, Vec::new()),
    };
    // What the machine said it spent, for the count this take named.
    let reported = match (&got, from) {
        (Ok(a), Some(f)) if a.status == ServedStatus::Ok => {
            let echoed = a.body["countFrom"].as_i64() == Some(f.timestamp_millis());
            super::shared_budget::usd_of(&a.body["spentUsd"])
                .filter(|_| echoed)
                .map(|usd| (f, usd))
        }
        _ => None,
    };
    if let Some((f, usd)) = reported {
        crew.machine_reported(&agent, machine, group, f, usd).await;
    }
    let notes: Vec<Note> = match &got {
        Ok(a) if a.status == ServedStatus::Ok => a.body["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .take(2 * MOST_NOTES)
            .filter_map(|n| serde_json::from_value(n.clone()).ok())
            .collect(),
        _ => Vec::new(),
    };
    {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        p.busy = false;
        p.noting = match (in_force, p.said || !notes.is_empty()) {
            (true, _) => None,
            (false, true) => Some(Instant::now() + NOTED_FOR),
            (false, false) => p.noting,
        };
        p.said = says && got.is_ok();
        match &got {
            Ok(a) => {
                p.failed = 0;
                p.unreached = None;
                p.notes_waiting = a.status == ServedStatus::Ok && !notes.is_empty();
            }
            Err(_) => {
                p.failed = p.failed.saturating_add(1);
                p.unreached = Some(started);
            }
        }
        if let Some((f, _)) = reported {
            p.counts = Some((f, Instant::now()));
        }
        // From when it began, so a slow round trip doesn't let Act fully
        // lapse between takes; one that failed is tried again soon. A
        // machine that just said it counts the budget is told at once.
        p.next = started
            + match (&got, full) {
                (_, true) => TAKE_EVERY,
                (Ok(_), false) if in_force && reported.is_some() => TAKE_EVERY,
                (Err(_), false) if in_force => TAKE_EVERY * 3,
                _ => idle_after(p.failed),
            };
    }
    if got.as_ref().is_ok_and(|a| a.status == ServedStatus::Ok) {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        p.notes_taken.retain(|id| !notes_taken.contains(id));
    }
    for note in notes {
        let (say, said) = {
            let mut t = told.lock();
            let say = t.says(&agent, note.id, Instant::now());
            (say, say || t.said(note.id))
        };
        if say {
            let words = tidied(note.left).words();
            crew.tell_dm(&agent, &format!("On {machine}: {words}"))
                .await;
        }
        if said {
            pumps
                .lock()
                .entry(k.clone())
                .or_default()
                .notes_taken
                .push(note.id);
        }
    }
    for d in decisions {
        let (Some(id), Some(prompt)) = (d["decideId"].as_str(), d["prompt"].as_str()) else {
            continue;
        };
        tokio::spawn(decide_one(
            Arc::clone(&crew),
            Arc::clone(&service),
            o.clone(),
            k.clone(),
            id.to_string(),
            prompt.to_string(),
            Call::of(d["kind"].as_str()),
            Arc::clone(&pumps),
        ));
    }
}

/// One decision a machine handed back, made on the overseer's own model
/// here and answered to it.
async fn decide_one(
    crew: Arc<CrewHub>,
    service: Arc<AgentService>,
    o: Oversight,
    k: (String, String),
    id: String,
    prompt: String,
    call: Option<Call>,
    pumps: Pumps,
) {
    let (machine, group) = &k;
    let agent = service.agent_id.as_str().to_string();
    let refuse = |code: Code, why: &str| Err::<String, _>((code, why.to_string()));
    let checked = match call.filter(|_| prompt.starts_with(super::decide::MARK)) {
        None => Err((Code::NotADecision, "not a decision its home takes")),
        Some(_) if prompt.chars().count() > MAX_PROMPT => Err((
            Code::TooLong,
            "the request is too long for its home to decide",
        )),
        Some(c) => Ok(c),
    };
    // Room for it: a take hands out no more than there is, so this waits
    // only on calls just begun; a call counts once it is to be made.
    let slot = match checked {
        Err(_) => None,
        Ok(_) => {
            let at_once = {
                let mut p = pumps.lock();
                let p = p.entry(k.clone()).or_default();
                (p.room(Instant::now()) > 0).then(|| Arc::clone(&p.at_once))
            };
            match at_once {
                Some(s) => tokio::time::timeout(TAKE_EVERY, s.acquire_owned())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .inspect(|_| {
                        let mut p = pumps.lock();
                        p.entry(k.clone())
                            .or_default()
                            .calls
                            .push_back(Instant::now());
                    }),
                None => None,
            }
        }
    };
    let decided = match checked {
        Err((code, why)) => refuse(code, why),
        Ok(_) if slot.is_none() => refuse(Code::Paced, "its home decides no more for it just now"),
        Ok(_) if !crew.relay_in_force(&agent, &o) => {
            refuse(Code::NotInForce, "Act fully isn't in force for it at home")
        }
        Ok(call) => match tokio::time::timeout(
            HERE_WITHIN,
            crew.decision_call(&service, call.system(), &prompt),
        )
        .await
        {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => {
                tracing::warn!(%machine, %group, error = %e, "a relayed decision call failed");
                refuse(Code::ModelFailed, "its model failed")
            }
            Err(_) => refuse(
                Code::Timeout,
                &format!(
                    "its model didn't answer within {} seconds",
                    HERE_WITHIN.as_secs()
                ),
            ),
        },
    };
    drop(slot);
    // Stopped, held or spent at home while its model answered: what it
    // decided isn't sent.
    let decided = match decided {
        Ok(_) if !crew.relay_in_force(&agent, &o) => {
            refuse(Code::NotInForce, "Act fully isn't in force for it at home")
        }
        d => d,
    };
    let mut input = match decided {
        Ok(text) => json!({"decideId": id,
                           "text": text.chars().take(MAX_ANSWER).collect::<String>()}),
        Err((code, why)) => json!({"decideId": id, "error": why, "code": code.as_str()}),
    };
    // The jobs the boss holds here, for the machine to compare the request
    // with (R5); none said when they couldn't be read, and the machine then
    // leaves an irreversible one to the person.
    if let Ok(held) = crew.held_irreversible(&agent).await {
        input["heldJobs"] = json!(held);
    }
    tracing::info!(%machine, %group, %agent, ok = input.get("text").is_some(), "a relayed boss decided a machine's request");
    let ceiling = crew.ceiling_now(&agent, &o);
    if let Err(why) = crew
        .relay(machine, group, DECIDED, input, &agent, &ceiling)
        .await
    {
        tracing::warn!(%machine, %group, why, "a relayed decision's answer didn't reach its machine");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(here: f64, elsewhere: f64, others: &[(&str, f64)]) -> Home {
        Home {
            at: Instant::now(),
            from: chrono::Utc::now(),
            here,
            elsewhere,
            others: others.iter().map(|(g, u)| (g.to_string(), *u)).collect(),
            takes: HashSet::new(),
        }
    }

    #[test]
    fn a_machine_is_taken_from_at_act_fully_and_for_a_while_after_it_lapses() {
        let now = Instant::now();
        let mut p = Pump {
            next: now,
            ..Pump::default()
        };
        assert!(p.due(false, now), "once, for what it may have left");
        p.notes_waiting = false;
        assert!(!p.due(false, now), "never at Act fully");
        assert!(p.due(true, now));
        p.next = now + TAKE_EVERY;
        assert!(!p.due(true, now));
        p.said = true;
        assert!(p.due(false, now), "told at once that it lapsed");
        p.busy = true;
        assert!(!p.due(false, now));
        p.busy = false;
        p.said = false;
        p.noting = Some(now + NOTED_FOR);
        assert!(!p.due(false, now));
        assert!(p.due(false, now + IDLE_EVERY));
        assert!(!p.due(false, now + NOTED_FOR));
        p.notes_taken.push(Uuid::new_v4());
        assert!(
            p.due(false, now + NOTED_FOR),
            "until it hears what was said"
        );
        p.notes_taken.clear();
        p.notes_waiting = true;
        assert!(p.due(false, now + NOTED_FOR), "while what it left waits");
        p.failed = GIVEN_UP_AFTER;
        assert!(!p.due(false, now + NOTED_FOR), "not reached: given up");
        assert!(p.due(true, now + NOTED_FOR));
    }

    #[test]
    fn a_machine_given_up_on_is_taken_from_again_once_it_is_reached() {
        let now = Instant::now();
        let later = now + MOST_IDLE;
        let mut p = Pump {
            failed: GIVEN_UP_AFTER,
            unreached: Some(now),
            next: later + MOST_IDLE,
            ..Pump::default()
        };
        assert!(!p.due(false, later), "given up, its notes waiting");
        // What reached it before the last take that didn't counts for nothing.
        p.reached(now - TAKE_EVERY, later);
        assert!(!p.due(false, later));
        // Reached since, by any call: its notes are taken at once.
        p.reached(now + TAKE_EVERY, later);
        assert!(p.due(false, later), "taken for the notes it keeps");
        assert_eq!((p.failed, p.unreached), (0, None));
        p.notes_waiting = false;
        assert!(!p.due(false, later), "and no more once none waits");
    }

    #[test]
    fn a_machine_that_cant_be_reached_is_taken_from_less_and_less_often() {
        assert_eq!(idle_after(0), IDLE_EVERY);
        assert_eq!(idle_after(1), IDLE_EVERY);
        assert_eq!(idle_after(2), IDLE_EVERY * 2);
        assert_eq!(idle_after(3), IDLE_EVERY * 4);
        assert_eq!(idle_after(40), MOST_IDLE);
    }

    #[test]
    fn notices_past_the_most_kept_are_counted_and_said_never_lost() {
        let mut out = Outbox::default();
        let asks = |i: usize| Left::Asks {
            session: format!("VEL-{i}"),
            ask: "Ship it?".into(),
        };
        for i in 0..MOST_NOTES + 3 {
            out.leave(SARAH, asks(i));
        }
        let notes = out.notes_of(SARAH, &HashSet::new());
        assert_eq!(notes.len(), MOST_NOTES + 1);
        assert_eq!(notes[0].left, asks(3), "the oldest made room");
        assert_eq!(notes[MOST_NOTES].left, Left::Crowded { more: 3 });
        // Each handed and not yet said taken: none makes room, and the
        // count handed doesn't grow; the newer are counted on their own.
        out.leave(SARAH, asks(90));
        out.leave(SARAH, asks(91));
        let again = out.notes_of(SARAH, &HashSet::new());
        assert_eq!(&again[..MOST_NOTES + 1], &notes[..], "as handed");
        assert_eq!(again[MOST_NOTES + 1].left, Left::Crowded { more: 2 });
        let said: HashSet<Uuid> = notes.iter().map(|n| n.id).collect();
        let left = out.notes_of(SARAH, &said);
        assert_eq!(left, again[MOST_NOTES + 1..], "the newer count alone");
        // Taken: each is handed no more.
        let said: HashSet<Uuid> = left.iter().map(|n| n.id).collect();
        assert!(out.notes_of(SARAH, &said).is_empty());
        assert!(out.notes.is_empty() && out.crowded.is_empty());
    }

    const SARAH: &str = "home/sarah";

    /// A word that counted its boss's other grant here before that grant
    /// spent more is never behind it: what it spent since is added, once.
    #[test]
    fn a_grants_spend_since_its_homes_last_word_of_it_is_never_left_out() {
        // The home counted plan B's $4 in plan A's word, and B's $4 in its
        // own; B has spent $3 more since A's word.
        let a = home(3.0, 4.0, &[("plan-b", 4.0)]);
        let b = home(7.0, 3.0, &[("plan-a", 3.0)]);
        assert_eq!(most_spent(&[("plan-a", &a), ("plan-b", &b)]), Some(10.0));
        // A's word, B's own gone stale: A alone, as the home last said.
        assert_eq!(most_spent(&[("plan-a", &a)]), Some(7.0));
        // A word that says nothing of B counts all B spent here again,
        // never less.
        let blind = home(3.0, 4.0, &[]);
        assert_eq!(
            most_spent(&[("plan-a", &blind), ("plan-b", &b)]),
            Some(14.0)
        );
        // A count of B past what B says (B read before) adds nothing.
        let ahead = home(3.0, 9.0, &[("plan-b", 9.0)]);
        assert_eq!(
            most_spent(&[("plan-a", &ahead), ("plan-b", &b)]),
            Some(12.0)
        );
        assert_eq!(most_spent(&[]), None);
    }
}

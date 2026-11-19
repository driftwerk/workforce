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
//!   pump relays [`TAKE`] to each machine of its group every second. Each
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
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::grant::BossRef;

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
#[derive(Debug, Clone, Copy)]
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
}

/// What a machine holds of its relayed bosses' decisions.
#[derive(Default)]
pub(crate) struct Outbox {
    /// When the home last said Act fully is in force for each, and what
    /// was spent.
    homes: HashMap<Key, Home>,
    /// What waits to be taken, oldest first.
    queued: HashMap<Key, VecDeque<(Uuid, String)>>,
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

impl Outbox {
    fn forget(&mut self, id: Uuid) {
        self.waiting.remove(&id);
        self.taken.remove(&id);
        for q in self.queued.values_mut() {
            q.retain(|(i, _)| *i != id);
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

fn key_of(boss: &BossRef, group: &str) -> Option<Key> {
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
    /// `party` (`home/<agent>`) in `group`.
    pub(crate) fn home_holds_full(&self, party: &str, group: &str) -> bool {
        self.relayed
            .lock()
            .homes
            .get(&(party.to_string(), group.to_string()))
            .is_some_and(|h| h.at.elapsed() < FRESH)
    }

    /// What Act fully's one budget counts as spent today for `party`
    /// (`home/<agent>`), by its home's word that is still fresh: its
    /// sessions' spend here and everywhere else. None without one, so it
    /// doesn't act fully.
    pub(crate) fn relayed_spent(&self, party: &str) -> Option<f64> {
        let out = self.relayed.lock();
        let fresh: Vec<&Home> = out
            .homes
            .iter()
            .filter(|((p, _), h)| p == party && h.at.elapsed() < FRESH)
            .map(|(_, h)| h)
            .collect();
        let elsewhere = fresh.iter().map(|h| h.elsewhere).reduce(f64::max)?;
        Some(fresh.iter().map(|h| h.here).sum::<f64>() + elsewhere)
    }

    /// What `group`'s sessions here spent since `from` in real money (calls
    /// a key paid for, never the person's plans); none when it can't be
    /// read.
    async fn group_spent(&self, group: &str, from: chrono::DateTime<chrono::Utc>) -> Option<f64> {
        let now = chrono::Utc::now();
        let filter = workforce_desk::ledger_store::Filter {
            seen: Some(
                self.desk
                    .seen_by(&workforce_common::oversight::Scope::Group(
                        group.to_string(),
                    )),
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

    /// One decision call by `boss` (`home/<agent>`), relayed from the home,
    /// for a session of `group`, on `prompt`: the text its model answered
    /// at home, or why there is none. Its secrets are hidden before it
    /// leaves. The caller bounds how long it waits.
    pub(crate) async fn relayed_decision_call(
        &self,
        boss: &str,
        group: &str,
        prompt: &str,
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
            q.push_back((id, prompt));
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
    /// (`countFrom`) and what was spent everywhere else
    /// (`spentElsewhereUsd`), and is handed what waits, each once. Its
    /// answer says what the group's sessions here spent since `countFrom`
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
        let in_force = ours
            && ceiling.level == Level::Full
            && input["inForce"].as_bool() == Some(true)
            && age.is_some()
            && here.is_some()
            && elsewhere.is_some();
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
                        },
                    );
                }
                let mut handed = Vec::new();
                while handed.len() < most {
                    let Some((id, prompt)) = out.queued.get_mut(&key).and_then(VecDeque::pop_front)
                    else {
                        break;
                    };
                    if out.waiting.contains_key(&id) {
                        out.taken.insert(id);
                        handed.push(json!({"decideId": id, "prompt": prompt}));
                    }
                }
                out.queued.retain(|_, q| !q.is_empty());
                handed
            }
        };
        if !ours {
            return refused(&workforce_desk::scope::not_ours(None));
        }
        let full = in_force && self.served_full(&key.0).is_some();
        let mut body = json!({ "full": full, "decisions": decisions });
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

fn refused(why: &str) -> (ServedStatus, Value) {
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
    /// When the machine last answered with its spend for the count that
    /// starts then ([`super::shared_budget`]): only a machine that counts
    /// the one budget is told Act fully is in force, within
    /// [`CONFIRMED_FOR`] of its answer.
    counts: Option<(chrono::DateTime<chrono::Utc>, Instant)>,
    /// The decision calls made lately, and the room for them at once.
    calls: VecDeque<Instant>,
    at_once: Arc<tokio::sync::Semaphore>,
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

impl Default for Pump {
    fn default() -> Self {
        Self {
            next: Instant::now(),
            busy: false,
            said: false,
            counts: None,
            calls: VecDeque::new(),
            at_once: Arc::new(tokio::sync::Semaphore::new(AT_ONCE)),
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

/// The Mac's pump: while Desk relays, each overseer of a group with
/// machine sessions takes its decisions from each machine, as the module
/// says, and says so when Act fully ends.
pub(crate) async fn pump_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let pumps: Pumps = Arc::default();
    let mut tick = tokio::time::interval(TAKE_EVERY / 4);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        if !crew.relaying() {
            pumps.lock().clear();
            continue;
        }
        let mut seen = HashSet::new();
        for (service, o) in crew.overseers() {
            let Some(group) = o.scope.group().map(str::to_string) else {
                continue;
            };
            let agent = service.agent_id.as_str().to_string();
            let in_force = crew.relay_in_force(&agent, &o);
            for machine in crew.machines_in(&group) {
                let k = (machine.clone(), group.clone());
                seen.insert(k.clone());
                {
                    let mut p = pumps.lock();
                    let p = p.entry(k.clone()).or_default();
                    if p.busy || (!in_force && !p.said) || (in_force && Instant::now() < p.next) {
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
                ));
            }
        }
        // A machine's group no overseer here takes from now: dropped. Its
        // machine stops holding it within [`FRESH`].
        pumps.lock().retain(|k, p| seen.contains(k) || p.busy);
    }
}

/// One take from `k`'s machine for its group, and the decisions it hands
/// back, each decided and answered.
async fn take(
    crew: Arc<CrewHub>,
    service: Arc<AgentService>,
    o: Oversight,
    k: (String, String),
    in_force: bool,
    pumps: Pumps,
) {
    let (machine, group) = &k;
    let agent = service.agent_id.as_str().to_string();
    let ceiling = crew.ceiling_now(&agent, &o);
    let started = Instant::now();
    // One budget across this Mac and every machine: where today's count
    // starts here, and what was spent everywhere but this machine.
    let from = crew.count_from(&o);
    let elsewhere = from.and_then(|f| crew.spent_elsewhere(&agent, machine, f));
    // As many as there is room to decide now: none is refused for pace.
    // In force there only once the machine said it counts this budget, so
    // an engine that counts only its own spend never acts fully.
    let (most, counts) = {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        let counts = p
            .counts
            .is_some_and(|(f, at)| Some(f) == from && started.duration_since(at) < CONFIRMED_FOR);
        (p.room(started), counts)
    };
    let says = in_force && counts && elsewhere.is_some();
    let mut input = json!({
        "inForce": says,
        "sentAt": chrono::Utc::now().timestamp_millis(),
        "most": most,
    });
    if let Some(f) = from {
        input["countFrom"] = json!(f.timestamp_millis());
    }
    if let Some(e) = elsewhere {
        input["spentElsewhereUsd"] = json!(e);
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
        crew.machine_reported(&agent, machine, f, usd).await;
    }
    {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        p.busy = false;
        p.said = says && got.is_ok();
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
                _ => IDLE_EVERY,
            };
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
    pumps: Pumps,
) {
    let (machine, group) = &k;
    let agent = service.agent_id.as_str().to_string();
    let refuse = |code: Code, why: &str| Err::<String, _>((code, why.to_string()));
    let checked = if !prompt.starts_with(super::decide::MARK) {
        Some(refuse(Code::NotADecision, "not a decision its home takes"))
    } else if prompt.chars().count() > MAX_PROMPT {
        Some(refuse(
            Code::TooLong,
            "the request is too long for its home to decide",
        ))
    } else {
        None
    };
    // Room for it: a take hands out no more than there is, so this waits
    // only on calls just begun; a call counts once it is to be made.
    let slot = match checked {
        Some(_) => None,
        None => {
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
    let decided = if let Some(refused) = checked {
        refused
    } else if slot.is_none() {
        refuse(Code::Paced, "its home decides no more for it just now")
    } else if !crew.relay_in_force(&agent, &o) {
        refuse(Code::NotInForce, "Act fully isn't in force for it at home")
    } else {
        match tokio::time::timeout(
            HERE_WITHIN,
            crew.decision_call(&service, super::decide::SYSTEM, &prompt),
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
        }
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

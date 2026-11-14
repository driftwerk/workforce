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
/// The most decisions one take hands out.
const MOST_TAKEN: usize = 4;
/// The most decisions waiting for one boss and group on a machine.
const MOST_WAITING: usize = 32;
/// The most decision calls the Mac makes for one machine's group a minute,
/// and at once.
const PER_MINUTE: usize = 30;
const AT_ONCE: usize = 3;
/// The longest answer taken, in characters.
const MAX_ANSWER: usize = 4_000;
/// How long the overseer's model may take: 5 s under the machine's wait.
const HERE_WITHIN: Duration = super::decide::WITHIN.saturating_sub(Duration::from_secs(5));

/// A boss and group, as the machine keys its relayed decisions: the
/// boss's party (`home/<agent>`) and the group's id.
type Key = (String, String);

/// What a machine holds of its relayed bosses' decisions.
#[derive(Default)]
pub(crate) struct Outbox {
    /// When the home last said Act fully is in force for each.
    homes: HashMap<Key, Instant>,
    /// What waits to be taken, oldest first.
    queued: HashMap<Key, VecDeque<(Uuid, String)>>,
    /// Who waits for each answer, and whose it is.
    waiting: HashMap<Uuid, (Key, oneshot::Sender<Result<String, Failed>>)>,
    /// What a take handed out: only these are answered.
    taken: HashSet<Uuid>,
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
    /// Whether the home said, lately, that Act fully is in force for
    /// `party` (`home/<agent>`) in `group`.
    pub(crate) fn home_holds_full(&self, party: &str, group: &str) -> bool {
        self.relayed
            .lock()
            .homes
            .get(&(party.to_string(), group.to_string()))
            .is_some_and(|at| at.elapsed() < FRESH)
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
            if out.homes.get(&key).is_none_or(|at| at.elapsed() >= FRESH) {
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
    /// level `ceiling` says), and is handed what waits, each once.
    pub(crate) fn take_relayed(
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
        let in_force =
            ours && ceiling.level == Level::Full && input["inForce"].as_bool() == Some(true);
        let decisions: Vec<Value> = {
            let mut out = self.relayed.lock();
            if !in_force {
                out.end(&key);
                Vec::new()
            } else {
                out.homes.insert(key.clone(), Instant::now());
                let mut handed = Vec::new();
                while handed.len() < MOST_TAKEN {
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
        (
            ServedStatus::Ok,
            json!({ "full": full, "decisions": decisions }),
        )
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
        let taken = id.and_then(|id| {
            let mut out = self.relayed.lock();
            let ours =
                out.waiting.get(&id).is_some_and(|(k, _)| *k == key) && out.taken.contains(&id);
            if !ours {
                return None;
            }
            out.taken.remove(&id);
            out.waiting.remove(&id).map(|(_, tx)| tx)
        });
        let outcome = match taken {
            Some(tx) => {
                let _ = tx.send(answer);
                "ok"
            }
            None => "refused",
        };
        let row = workforce_desk::grant_store::CallRow {
            caller: format!("boss:{}", key.0),
            boss: Some(boss.key()),
            group: Some(group.to_string()),
            tool: DECIDED,
            input: json!({"decideId": id, "code": input["code"]}).to_string(),
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
    /// The decision calls made lately, and the room for them at once.
    calls: VecDeque<Instant>,
    at_once: Arc<tokio::sync::Semaphore>,
}

impl Default for Pump {
    fn default() -> Self {
        Self {
            next: Instant::now(),
            busy: false,
            said: false,
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
    let ceiling = Ceiling {
        level: o.level,
        act: o.act,
    };
    let got = crew
        .relay(
            machine,
            group,
            TAKE,
            json!({ "inForce": in_force }),
            &agent,
            &ceiling,
        )
        .await;
    let (full, decisions) = match &got {
        Ok(a) if a.status == ServedStatus::Ok => (
            a.body["full"].as_bool() == Some(true),
            a.body["decisions"].as_array().cloned().unwrap_or_default(),
        ),
        _ => (false, Vec::new()),
    };
    {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        p.busy = false;
        p.said = in_force && got.is_ok();
        p.next = Instant::now() + if full { TAKE_EVERY } else { IDLE_EVERY };
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
    let slot = {
        let mut p = pumps.lock();
        let p = p.entry(k.clone()).or_default();
        let now = Instant::now();
        while p
            .calls
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            p.calls.pop_front();
        }
        match (p.calls.len() < PER_MINUTE)
            .then(|| Arc::clone(&p.at_once).try_acquire_owned().ok())
            .flatten()
        {
            Some(slot) => {
                p.calls.push_back(now);
                Some(slot)
            }
            None => None,
        }
    };
    let decided = if !prompt.starts_with(super::decide::MARK) {
        refuse(Code::NotADecision, "not a decision its home takes")
    } else if prompt.chars().count() > MAX_PROMPT {
        refuse(
            Code::TooLong,
            "the request is too long for its home to decide",
        )
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
    let input = match decided {
        Ok(text) => json!({"decideId": id,
                           "text": text.chars().take(MAX_ANSWER).collect::<String>()}),
        Err((code, why)) => json!({"decideId": id, "error": why, "code": code.as_str()}),
    };
    tracing::info!(%machine, %group, %agent, ok = input.get("text").is_some(), "a relayed boss decided a machine's request");
    let ceiling = Ceiling {
        level: o.level,
        act: o.act,
    };
    if let Err(why) = crew
        .relay(machine, group, DECIDED, input, &agent, &ceiling)
        .await
    {
        tracing::warn!(%machine, %group, why, "a relayed decision's answer didn't reach its machine");
    }
}

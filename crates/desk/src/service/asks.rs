//! What each running session asks the person, kept where a hub restart
//! can't lose it. An ask knows the call that raised it: its wait on the
//! floor keeps the ask's id across a restart, and ends with what names the
//! call (its end, its
//! refusal, its thread's next call or end, the turn's end), and is never
//! left open on the floor once nothing asks it. After a restart each wait
//! is either shown again, as the terminal's to answer, or closed.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use serde_json::Value;

use super::{Behind, Desk, Live, OpenTurn, Persist, Raised, State};
use crate::floor::{Answer, Subject, Turn, TurnOp, Wait};
use crate::model::{Engine, PendingApproval, SessionState};
use crate::store::DeskStore;

/// What a session asks that only its terminal shows, when nothing says
/// what it is.
const IN_TERMINAL: &str = "Answer in the terminal";

/// One open ask, as kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeptAsk {
    pub id: Uuid,
    /// Its wait's key on the floor.
    pub key: String,
    pub tool: String,
    pub detail: String,
    pub since: DateTime<Utc>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// The call that raised it (`tool_use_id`).
    #[serde(default)]
    pub call: Option<String>,
    /// The sub-agent that raised it.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub cli_session: Option<String>,
    /// A question (or a plan) a tool asks, not a permission.
    #[serde(default)]
    pub question: bool,
    /// The main thread's open question.
    #[serde(default)]
    pub main_question: bool,
}

impl KeptAsk {
    pub(super) fn of(p: &PendingApproval, raised: Option<&Raised>, main_question: bool) -> Self {
        Self {
            id: p.id,
            key: p.key(),
            tool: p.tool.clone(),
            detail: p.detail.clone(),
            since: p.since,
            why: p.why.clone(),
            options: p.options.clone(),
            call: p
                .call
                .clone()
                .or_else(|| raised.and_then(|r| r.call.clone())),
            agent: p
                .agent
                .clone()
                .or_else(|| raised.and_then(|r| r.agent.clone())),
            cli_session: p.cli_session.clone(),
            question: raised.is_some_and(|r| r.question),
            main_question,
        }
    }

    /// It, shown again: only its terminal answers it now, as the hook call
    /// that could have was the old hub's.
    fn pending(&self) -> PendingApproval {
        PendingApproval {
            id: self.id,
            tool: self.tool.clone(),
            detail: self.detail.clone(),
            since: self.since,
            answerable: false,
            choices: Vec::new(),
            why: self.why.clone(),
            options: self.options.clone(),
            request: None,
            call: self.call.clone(),
            agent: self.agent.clone(),
            cli_session: self.cli_session.clone(),
        }
    }

    fn raised(&self) -> Raised {
        Raised {
            pending: self.id,
            agent: self.agent.clone(),
            question: self.question,
            call: self.call.clone(),
        }
    }
}

/// Each open turn of a session the desk carries on across a restart, with
/// the waits the floor still has open in it.
pub(super) struct OpenWaits {
    pub session: Uuid,
    pub turn: Uuid,
    pub waits: Vec<Wait>,
}

/// The open waits of `logged`'s turns that the desk carries on (`turns`).
pub(super) fn open_waits(logged: &[Turn], turns: &HashMap<Uuid, OpenTurn>) -> Vec<OpenWaits> {
    logged
        .iter()
        .filter_map(|t| {
            let Subject::Session { id, .. } = t.subject else {
                return None;
            };
            let carried = turns.get(&id).is_some_and(|o| o.id == t.id) && t.ended_at.is_none();
            let waits: Vec<Wait> = t.waits.iter().filter(|w| w.to.is_none()).cloned().collect();
            (carried && !waits.is_empty()).then_some(OpenWaits {
                session: id,
                turn: t.id,
                waits,
            })
        })
        .collect()
}

/// `l` asking `kept` again: the first is what shows, and the main
/// thread's question waits behind it.
fn restore(l: &mut Live, kept: &[KeptAsk]) {
    let Some(first) = kept.first() else { return };
    l.pending = Some(first.pending());
    l.held = Some(first.raised());
    l.answer = None;
    l.open_question = kept
        .iter()
        .find(|k| k.main_question)
        .map(|k| (k.pending(), k.raised()));
    l.behind = kept
        .iter()
        .skip(1)
        .filter(|k| !k.main_question)
        .map(|k| Behind {
            pending: k.pending(),
            raised: k.raised(),
            answer: None,
        })
        .collect();
    // An ask's call runs until it ends: one started beside it after the
    // restart doesn't end it.
    for k in kept {
        if let Some(call) = &k.call {
            l.started(k.agent.clone(), call.clone(), &k.tool, Value::Null);
        }
    }
}

/// `kept` without the asks whose call the session's transcript already has
/// a result for: answered (or refused) while no hub heard its end.
pub(super) async fn not_ended(
    mut kept: HashMap<Uuid, Vec<KeptAsk>>,
    transcripts: HashMap<Uuid, String>,
) -> HashMap<Uuid, Vec<KeptAsk>> {
    for (id, asks) in kept.iter_mut() {
        let Some(path) = transcripts.get(id).cloned() else {
            continue;
        };
        let calls: HashSet<String> = asks.iter().filter_map(|a| a.call.clone()).collect();
        if calls.is_empty() {
            continue;
        }
        let ended = tokio::task::spawn_blocking(move || {
            crate::transcript::results_among(std::path::Path::new(&path), &calls)
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        asks.retain(|a| a.call.as_ref().is_none_or(|c| !ended.contains(c)));
    }
    kept
}

impl State {
    /// What each session asks now, by its waits' keys: the only waits the
    /// floor lists for a session.
    pub(super) fn ask_keys(&self) -> HashMap<Uuid, HashSet<String>> {
        self.asks
            .iter()
            .map(|(id, asks)| (*id, asks.iter().map(|a| a.key.clone()).collect()))
            .collect()
    }
}

/// Whether what a session of `engine`'s asks outlives the hub it asked:
/// a Claude or Devin hook that loses its hub fails, and the engine asks in
/// its own terminal instead (Devin: past the hook, its TUI's own prompt);
/// a Codex request is the old hub's connection's, and went with it, and no
/// other engine's is kept.
pub(super) fn asks_outlive_the_hub(engine: Engine) -> bool {
    matches!(engine, Engine::Claude | Engine::Devin)
}

impl Desk {
    /// Keeps what session `id` asks now, if it changed since last kept, and
    /// closes the floor's wait of every ask that ended, in the turn it was
    /// open in, however it ended: the one place a session's asks are
    /// written down, so none is left open by a change no transition reads
    /// (a turn that ends while its background work runs, say).
    ///
    /// The order of writes: the kept asks go on the persist queue before the
    /// floor's ops for this change (`saved` logs those after), and both
    /// leave in queue order. A hub that stops between them is put right by
    /// the next start either way: an ask kept whose wait wasn't written yet
    /// is written there (`carry_asks`), and a wait whose ask is no longer
    /// kept is closed there.
    pub(super) fn keep_asks(
        &self,
        st: &mut State,
        id: Uuid,
        turn: Option<Uuid>,
        ops: &mut Vec<TurnOp>,
    ) {
        if self.0.cancel.is_cancelled() {
            // Its sessions are let go, not ended: what they ask stays kept
            // for the next hub.
            return;
        }
        let now = st.live.get(&id).map(Live::open_asks).unwrap_or_default();
        let was = st.asks.get(&id).cloned().unwrap_or_default();
        if now == was {
            return;
        }
        let at = Utc::now();
        if let Some(turn) = turn {
            let keys: HashSet<&str> = now.iter().map(|a| a.key.as_str()).collect();
            for gone in was.iter().filter(|a| !keys.contains(a.key.as_str())) {
                let closed = ops
                    .iter()
                    .any(|op| matches!(op, TurnOp::Unwait { key, .. } if *key == gone.key));
                if !closed {
                    ops.push(TurnOp::Unwait {
                        turn,
                        key: gone.key.clone(),
                        at,
                    });
                }
            }
        }
        match now.is_empty() {
            true => st.asks.remove(&id),
            false => st.asks.insert(id, now.clone()),
        };
        let _ = self.0.persist.send(Persist::Asks(id, now));
    }

    /// What each session asks now, by its waits' keys.
    pub fn ask_keys(&self) -> HashMap<Uuid, HashSet<String>> {
        self.0.state.lock().ask_keys()
    }

    /// After a restart, once the sessions still running are taken back:
    /// each asks again what it was kept asking, its terminal's to answer
    /// now; one left waiting from before Desk kept its asks shows its
    /// latest wait again the same way; and every other wait the floor has
    /// open for the turns carried on is closed, as nothing asks it.
    pub(super) fn carry_asks(&self, mut kept: HashMap<Uuid, Vec<KeptAsk>>, open: Vec<OpenWaits>) {
        let mut waits: HashMap<Uuid, Vec<Wait>> = HashMap::new();
        for o in &open {
            waits
                .entry(o.session)
                .or_default()
                .extend(o.waits.iter().cloned());
        }
        let running: Vec<Uuid> = self.0.state.lock().live.keys().copied().collect();
        for id in running {
            let reshown = {
                let mut guard = self.0.state.lock();
                let st = &mut *guard;
                let Some(r) = st.records.get(&id) else {
                    continue;
                };
                let (engine, blocked, cli) = (
                    r.engine,
                    r.state == SessionState::Blocked,
                    r.cli_session_id.clone(),
                );
                let turn = st.turns.get(&id).map(|o| o.id);
                // One asking already (a Codex request seen again) keeps
                // what it asks; it is kept below with the rest.
                let Some(l) = st.live.get_mut(&id).filter(|l| l.pending.is_none()) else {
                    kept.remove(&id);
                    continue;
                };
                // A Codex request was the old hub's connection's: it went.
                // Kept, but each one's call has ended since: it asks nothing.
                let kept_here = kept.remove(&id).filter(|_| asks_outlive_the_hub(engine));
                let answered = kept_here.as_ref().is_some_and(Vec::is_empty);
                let asks = kept_here.unwrap_or_default();
                if answered {
                    Some((Vec::new(), SessionState::Working))
                } else if !asks.is_empty() {
                    restore(l, &asks);
                    let ops = turn.map_or_else(Vec::new, |turn| {
                        asks.iter()
                            .map(|a| TurnOp::Wait {
                                turn,
                                wait: super::transition::wait_of(id, &a.pending()),
                            })
                            .collect()
                    });
                    Some((ops, SessionState::Blocked))
                } else if blocked {
                    // Asked before Desk kept what it asks (or the floor
                    // has no wait for it): its terminal shows it, as the
                    // latest wait says.
                    let latest = waits
                        .get(&id)
                        .and_then(|w| w.iter().max_by_key(|w| w.from))
                        .cloned();
                    // An older wait's key is its ask's id.
                    let (pid, call) = match latest.as_ref().map(|w| w.key.parse::<Uuid>()) {
                        Some(Ok(pid)) => (pid, None),
                        Some(Err(_)) => (Uuid::new_v4(), latest.as_ref().map(|w| w.key.clone())),
                        None => (Uuid::new_v4(), None),
                    };
                    let pending = PendingApproval {
                        id: pid,
                        tool: "Input".into(),
                        detail: latest
                            .as_ref()
                            .map_or_else(|| IN_TERMINAL.to_string(), |w| w.ask.clone()),
                        since: latest.as_ref().map_or_else(Utc::now, |w| w.from),
                        answerable: false,
                        choices: Vec::new(),
                        why: None,
                        options: latest.as_ref().and_then(|w| w.options.clone()),
                        request: None,
                        call: call.clone(),
                        agent: None,
                        cli_session: cli,
                    };
                    let wait = Wait {
                        key: pending.key(),
                        from: pending.since,
                        to: None,
                        ask: pending.detail.clone(),
                        answer: Answer::Terminal,
                        options: pending.options.clone(),
                    };
                    l.held = Some(Raised {
                        pending: pid,
                        agent: None,
                        question: false,
                        call,
                    });
                    l.pending = Some(pending);
                    let ops = turn.map_or_else(Vec::new, |turn| vec![TurnOp::Wait { turn, wait }]);
                    Some((ops, SessionState::Blocked))
                } else {
                    None
                }
            };
            if let Some((ops, state)) = reshown {
                self.log_turns(ops);
                self.change(id, |r, _| match state {
                    SessionState::Blocked => r.state = state,
                    // What it was kept asking was answered while no hub ran.
                    _ if r.state == SessionState::Blocked => r.state = state,
                    _ => {}
                });
            }
        }
        // Every running session's asks, as they stand now, are what is
        // kept and what backs the floor; nothing runs to ask the rest.
        {
            let mut st = self.0.state.lock();
            let running: Vec<(Uuid, Vec<KeptAsk>)> =
                st.live.iter().map(|(id, l)| (*id, l.open_asks())).collect();
            for (id, asks) in running {
                kept.remove(&id);
                match asks.is_empty() {
                    true => st.asks.remove(&id),
                    false => st.asks.insert(id, asks.clone()),
                };
                let _ = self.0.persist.send(Persist::Asks(id, asks));
            }
        }
        for id in kept.into_keys() {
            let _ = self.0.persist.send(Persist::Asks(id, Vec::new()));
        }
        let backed = self.ask_keys();
        let at = Utc::now();
        let ops: Vec<TurnOp> = open
            .into_iter()
            .flat_map(|o| {
                let asks = backed.get(&o.session).cloned().unwrap_or_default();
                o.waits
                    .into_iter()
                    .filter(move |w| !asks.contains(&w.key))
                    .map(move |w| TurnOp::Unwait {
                        turn: o.turn,
                        key: w.key,
                        at,
                    })
            })
            .collect();
        if !ops.is_empty() {
            tracing::info!(
                closed = ops.len(),
                "closed the floor's waits nothing asks any more"
            );
        }
        self.log_turns(ops);
    }
}

impl DeskStore {
    /// What each session was last kept asking.
    pub(crate) async fn open_asks(&self) -> Result<HashMap<Uuid, Vec<KeptAsk>>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT session, asks FROM open_asks", &[])
            .await?;
        let mut out = HashMap::new();
        for row in rows {
            let session: Uuid = row.get(0);
            match serde_json::from_value::<Vec<KeptAsk>>(row.get(1)) {
                Ok(asks) => {
                    out.insert(session, asks);
                }
                Err(e) => tracing::warn!(%session, error = %e, "unreadable kept asks; dropped"),
            }
        }
        Ok(out)
    }

    /// Keeps what `session` asks now (nothing: forgets it).
    pub(crate) async fn put_open_asks(&self, session: Uuid, asks: &[KeptAsk]) -> Result<()> {
        let client = self.pool().get().await?;
        if asks.is_empty() {
            client
                .execute("DELETE FROM open_asks WHERE session = $1", &[&session])
                .await?;
        } else {
            client
                .execute(
                    "INSERT INTO open_asks (session, asks, updated_at) VALUES ($1, $2, now()) \
                     ON CONFLICT (session) DO UPDATE SET asks = $2, updated_at = now()",
                    &[&session, &serde_json::to_value(asks)?],
                )
                .await?;
        }
        Ok(())
    }
}

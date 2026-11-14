//! A remote boss's decision call, made on its own hub (the engine's side of
//! [`crate::boss_links::decide`]): at Act fully a request in a session a
//! boss on another hub runs is decided by that boss's own model, as a local
//! boss's is ([`super::decide`]). This engine writes the decision's text
//! (the session and its whole request, fenced as the session's words, its
//! secrets hidden), sends it up the boss's link as a `decide` frame, and
//! waits for the `decided` that answers it on the same link of the same
//! socket. Only the text comes back: what is done with it, and whether it
//! still may be, this engine judges, under the grant, budget and Stop as
//! they are when it acts.
//!
//! A boss hub that doesn't take `decide` frames (it doesn't say so at
//! `ready`), or no socket up, means no decision: the request is the
//! person's.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::CrewHub;
use crate::boss_links::decide::Code;

/// A decision call that made no decision: why, in plain words, and its
/// code when its boss's hub (or this engine) said one. What is done next
/// is judged by the code alone ([`Self::again`]), never by the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failed {
    pub(crate) why: String,
    pub(crate) code: Option<Code>,
}

impl Failed {
    fn new(code: Code, why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            code: Some(code),
        }
    }

    /// Whether asking again at once could change it: not for what is too
    /// long, no decision, or held by its hub for its pace or budget. A
    /// code missing or unknown (an older or odd hub) is asked again, once,
    /// as any failure is: the worst it costs is one more refused call.
    pub(crate) fn again(&self) -> bool {
        self.code.is_none_or(Code::again)
    }
}

impl From<String> for Failed {
    fn from(why: String) -> Self {
        Self { why, code: None }
    }
}

/// The decisions sent up one socket and not answered yet, by their id:
/// the link each went on, and who waits.
type Waiting = Arc<Mutex<HashMap<Uuid, (Uuid, oneshot::Sender<Result<String, Failed>>)>>>;

/// One link a boss's decisions go up, on one socket.
pub(crate) struct DecideLink {
    socket: Uuid,
    link: Uuid,
    tx: mpsc::Sender<Value>,
    waiting: Waiting,
}

/// The links bosses' decisions go up now, by `(<hub>/<agent>, group)`:
/// each socket that carries one, the latest last.
pub(crate) type Deciders = Mutex<HashMap<(String, String), Vec<DecideLink>>>;

/// One socket's links for decisions, held while the socket is up: each
/// is forgotten as it ends, and its waiting decisions with it.
pub(crate) struct DecideUp {
    crew: Arc<CrewHub>,
    socket: Uuid,
    waiting: Waiting,
}

impl Drop for DecideUp {
    fn drop(&mut self) {
        self.crew.deciders.lock().retain(|_, up| {
            up.retain(|d| d.socket != self.socket);
            !up.is_empty()
        });
        // Who waits hears the socket went.
        for (_, (_, tx)) in self.waiting.lock().drain() {
            let _ = tx.send(Err(Failed::new(Code::Link, "its hub's link went down")));
        }
    }
}

impl CrewHub {
    /// Marks `links` (each its id, its boss's `<hub>/<agent>` and its group)
    /// as where those bosses' decisions go up, through `tx`, while the guard
    /// lives: a socket whose boss hub takes `decide` frames.
    pub(crate) fn decide_up(
        self: &Arc<Self>,
        links: Vec<(Uuid, String, String)>,
        tx: mpsc::Sender<Value>,
    ) -> DecideUp {
        let socket = Uuid::new_v4();
        let waiting: Waiting = Arc::default();
        {
            let mut deciders = self.deciders.lock();
            for (link, boss, group) in links {
                deciders.entry((boss, group)).or_default().push(DecideLink {
                    socket,
                    link,
                    tx: tx.clone(),
                    waiting: Arc::clone(&waiting),
                });
            }
        }
        DecideUp {
            crew: Arc::clone(self),
            socket,
            waiting,
        }
    }

    /// Takes a `decided` frame that came up `up`'s socket: it answers only
    /// a decision sent on that socket, on the link it names, once.
    pub(crate) fn decided(&self, up: &DecideUp, frame: &Value) {
        let Some(id) = frame["decideId"]
            .as_str()
            .and_then(|i| i.parse::<Uuid>().ok())
        else {
            return;
        };
        let link = frame["linkId"]
            .as_str()
            .and_then(|l| l.parse::<Uuid>().ok());
        let mut waiting = up.waiting.lock();
        if waiting.get(&id).is_none_or(|(l, _)| Some(*l) != link) {
            return;
        }
        let Some((_, tx)) = waiting.remove(&id) else {
            return;
        };
        let answer = match (frame["text"].as_str(), frame["error"].as_str()) {
            // As long as its hub sends at most: what it says reaches the
            // session as a deny's reason, and Activity.
            (Some(t), _) => Ok(t.chars().take(MAX_ANSWER).collect()),
            (None, Some(e)) => Err(Failed {
                // Its control characters gone first: what it says is shown.
                why: workforce_desk::crew::one_line(&workforce_core::desk::one_line(e, 1_000), 160),
                code: frame["code"].as_str().and_then(Code::parse),
            }),
            (None, None) => Err("its hub's answer couldn't be read".to_string().into()),
        };
        let _ = tx.send(answer);
    }

    /// Whether `boss`'s decisions for `group` can go up now.
    #[cfg(test)]
    pub(crate) fn decides_remotely(&self, boss: &str, group: &str) -> bool {
        self.deciders
            .lock()
            .contains_key(&(boss.to_string(), group.to_string()))
    }

    /// One decision call by `boss` (`<hub>/<agent>`) on its own hub, for a
    /// session of `group`, on `prompt`: the text its model answered, or why
    /// there is none. Its secrets are hidden before it leaves this engine.
    /// The caller bounds how long it waits.
    pub(crate) async fn remote_decision_call(
        &self,
        boss: &str,
        group: &str,
        prompt: &str,
    ) -> Result<String, Failed> {
        let prompt = crate::redact::hide_secrets(prompt);
        // Longer than its hub takes: never sent (its hub would count it
        // against the socket), so the person decides it.
        if prompt.chars().count() > crate::boss_links::decide::MAX_PROMPT {
            return Err(Failed::new(Code::TooLong, TOO_LONG));
        }
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let (send, link, waiting) = {
            let deciders = self.deciders.lock();
            let Some(d) = deciders
                .get(&(boss.to_string(), group.to_string()))
                .and_then(|up| up.last())
            else {
                return Err(Failed::new(Code::Link, "its hub isn't connected"));
            };
            d.waiting.lock().insert(id, (d.link, tx));
            (d.tx.clone(), d.link, Arc::clone(&d.waiting))
        };
        let frame = json!({
            "type": "decide",
            "linkId": link,
            "decideId": id,
            "prompt": prompt,
        });
        // Forgotten however this ends, its caller's time running out too:
        // an answer after it is dropped.
        let _forget = Forget { waiting, id };
        if send.try_send(frame).is_err() {
            return Err(Failed::new(
                Code::Link,
                "its hub's link has no room just now",
            ));
        }
        rx.await
            .unwrap_or_else(|_| Err(Failed::new(Code::Link, "its hub's link went down")))
    }
}

/// Forgets a decision sent up as it is dropped, from the socket it went up.
struct Forget {
    waiting: Waiting,
    id: Uuid,
}

impl Drop for Forget {
    fn drop(&mut self) {
        self.waiting.lock().remove(&self.id);
    }
}

/// The longest answer taken from a boss's hub, in characters.
const MAX_ANSWER: usize = 4_000;

/// Why a decision too long for its boss's hub isn't asked of it.
const TOO_LONG: &str = "the request is too long for its hub to decide";

/// A decision of a boss from the decide loop being done ([`CrewHub::deciding`]).
pub(crate) struct Deciding<'a> {
    crew: &'a CrewHub,
    boss: String,
}

impl Drop for Deciding<'_> {
    fn drop(&mut self) {
        let mut d = self.crew.served_deciding.lock();
        if let Some(n) = d.get_mut(&self.boss) {
            *n -= 1;
            if *n == 0 {
                d.remove(&self.boss);
            }
        }
    }
}

/// Who makes a boss's decision call, and how.
pub(crate) struct Decider {
    /// The boss's name, as its decisions say it.
    pub(crate) name: String,
    /// Its oversight here: what its preferences are scoped by.
    pub(crate) oversight: workforce_common::oversight::Oversight,
    how: How,
}

enum How {
    /// This hub's own boss: its own model, here.
    Here(Arc<crate::serve::AgentService>),
    /// A boss on another hub (`<hub>/<agent>`), for a session of `group`:
    /// its own model, on its hub.
    Remote { boss: String, group: String },
    /// The home's own boss of a machine's sessions (`home/<agent>`), for a
    /// session of `group`: its own model, at home, by the relay
    /// ([`super::relayed_decide`]).
    Relayed { boss: String, group: String },
}

/// `what`, naming the boss's hub when it is on another (`<hub>/<agent>`):
/// "… (by allan on driftwerk)", or "… (by sarah at home)" for the home's
/// own.
pub(crate) fn named(boss: &str, what: String) -> String {
    match boss.split_once('/') {
        Some((workforce_desk::grant::HOME_HUB, agent)) => format!("{what} (by {agent} at home)"),
        Some((hub, agent)) => format!("{what} (by {agent} on {hub})"),
        None => what,
    }
}

impl CrewHub {
    /// Who makes `boss`'s decision call on `view`'s request: this hub's own
    /// boss, or a boss on another hub whose live grant here serves it and
    /// holds the session. None for any other.
    pub(crate) fn decider_of(
        &self,
        boss: &str,
        view: &workforce_desk::model::SessionView,
    ) -> Option<Decider> {
        if !workforce_desk::grant::is_served(boss) {
            let (service, oversight) = self.overseer_named(boss)?;
            return Some(Decider {
                name: super::name_of(&service),
                oversight,
                how: How::Here(service),
            });
        }
        let g = self.served_grant_of(boss, view.id)?;
        let relayed = matches!(g.boss, Some(workforce_desk::grant::BossRef::Local { .. }));
        let (boss, group) = (boss.to_string(), g.group.clone());
        Some(Decider {
            name: super::served::boss_name(&g),
            oversight: super::served::served_oversight(&g),
            how: match relayed {
                true => How::Relayed { boss, group },
                false => How::Remote { boss, group },
            },
        })
    }

    /// One decision call by `d` on `prompt`: its model's text, or why there
    /// is none.
    pub(crate) async fn decide_by(&self, d: &Decider, prompt: &str) -> Result<String, Failed> {
        match &d.how {
            How::Here(service) => self
                .decision_call(service, super::decide::SYSTEM, prompt)
                .await
                .map_err(Failed::from),
            How::Remote { boss, group } => self.remote_decision_call(boss, group, prompt).await,
            How::Relayed { boss, group } => self.relayed_decision_call(boss, group, prompt).await,
        }
    }

    /// After `d`'s decision calls: a boss on another hub has its spend
    /// today read again, so a budget spent meanwhile holds what it decided.
    pub(crate) async fn after_decided(&self, d: &Decider) {
        if let How::Remote { boss, .. } | How::Relayed { boss, .. } = &d.how {
            self.read_served_spent(boss).await;
        }
    }

    /// Marks `boss`'s decision from the decide loop as being done, while the
    /// guard lives: a boss on another hub's is then recorded here whatever
    /// its grant reads as by the time it is (judged just before it is done).
    pub(crate) fn deciding(&self, boss: &str) -> Deciding<'_> {
        *self
            .served_deciding
            .lock()
            .entry(boss.to_string())
            .or_default() += 1;
        Deciding {
            crew: self,
            boss: boss.to_string(),
        }
    }

    /// Whether a decision of `boss` from the decide loop is being done now.
    pub(crate) fn is_deciding(&self, boss: &str) -> bool {
        self.served_deciding
            .lock()
            .get(boss)
            .is_some_and(|n| *n > 0)
    }

    /// Whether a decision of `boss` on session `id` may still be done as
    /// far as its grant goes: always for this hub's own boss; for one on
    /// another hub, while a live grant here still serves it and holds the
    /// session, judged now, as it is done.
    pub(crate) fn served_still_holds(&self, boss: &str, id: Uuid) -> bool {
        !workforce_desk::grant::is_served(boss) || self.served_grant_of(boss, id).is_some()
    }

    /// `boss`, on another hub, left `session`'s request `approval` to the
    /// person: it waits on them now, and goes up its link as an ask, to
    /// the conversation the person has with that boss there
    /// ([`super::remote_asks`]).
    pub(crate) fn left_by_served(&self, boss: &str, session: Uuid, approval: Uuid) {
        if workforce_desk::grant::is_served(boss) {
            self.desk.leave_to_person(session, approval);
        }
    }

    /// [`Self::left_by_served`] for request `approval`, wherever it is open.
    pub(crate) fn served_waits(&self, boss: &str, approval: Uuid) {
        if !workforce_desk::grant::is_served(boss) {
            return;
        }
        if let Some(v) = self
            .desk
            .list()
            .into_iter()
            .find(|v| v.pending.as_ref().is_some_and(|p| p.id == approval))
        {
            self.desk.leave_to_person(v.id, approval);
        }
    }
}

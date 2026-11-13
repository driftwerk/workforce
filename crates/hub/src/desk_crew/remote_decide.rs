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

/// The decisions sent up one socket and not answered yet, by their id:
/// the link each went on, and who waits.
type Waiting = Arc<Mutex<HashMap<Uuid, (Uuid, oneshot::Sender<Result<String, String>>)>>>;

/// One link a boss's decisions go up, on one socket.
pub(crate) struct DecideLink {
    socket: Uuid,
    link: Uuid,
    tx: mpsc::Sender<Value>,
    waiting: Waiting,
}

/// The links bosses' decisions go up now, by `(<hub>/<agent>, group)`.
pub(crate) type Deciders = Mutex<HashMap<(String, String), DecideLink>>;

/// One socket's links for decisions, held while the socket is up: each
/// is forgotten as it ends, and its waiting decisions with it.
pub(crate) struct DecideUp {
    crew: Arc<CrewHub>,
    socket: Uuid,
    waiting: Waiting,
}

impl Drop for DecideUp {
    fn drop(&mut self) {
        self.crew
            .deciders
            .lock()
            .retain(|_, d| d.socket != self.socket);
        // Who waits hears the socket went.
        for (_, (_, tx)) in self.waiting.lock().drain() {
            let _ = tx.send(Err("its hub's link went down".into()));
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
                deciders.insert(
                    (boss, group),
                    DecideLink {
                        socket,
                        link,
                        tx: tx.clone(),
                        waiting: Arc::clone(&waiting),
                    },
                );
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
            (Some(t), _) => Ok(t.to_string()),
            (None, Some(e)) => Err(workforce_desk::crew::one_line(e, 160)),
            (None, None) => Err("its hub's answer couldn't be read".into()),
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
    ) -> Result<String, String> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let (send, link) = {
            let deciders = self.deciders.lock();
            let Some(d) = deciders.get(&(boss.to_string(), group.to_string())) else {
                return Err("its hub isn't connected".into());
            };
            d.waiting.lock().insert(id, (d.link, tx));
            (d.tx.clone(), d.link)
        };
        let frame = json!({
            "type": "decide",
            "linkId": link,
            "decideId": id,
            "prompt": crate::redact::hide_secrets(prompt),
        });
        // Forgotten however this ends, its caller's time running out too:
        // an answer after it is dropped.
        let _forget = Forget {
            crew: self,
            key: (boss.to_string(), group.to_string()),
            id,
        };
        if send.try_send(frame).is_err() {
            return Err("its hub's link has no room just now".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("its hub's link went down".into()))
    }
}

/// Forgets a decision sent up as it is dropped.
struct Forget<'a> {
    crew: &'a CrewHub,
    key: (String, String),
    id: Uuid,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.crew.deciders.lock().get(&self.key) {
            d.waiting.lock().remove(&self.id);
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
}

/// `what`, naming the boss's hub when it is on another (`<hub>/<agent>`):
/// "… (by allan on driftwerk)".
pub(crate) fn named(boss: &str, what: String) -> String {
    match boss.split_once('/') {
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
        Some(Decider {
            name: super::served::boss_name(&g),
            oversight: super::served::served_oversight(&g),
            how: How::Remote {
                boss: boss.to_string(),
                group: g.group,
            },
        })
    }

    /// One decision call by `d` on `prompt`: its model's text, or why there
    /// is none.
    pub(crate) async fn decide_by(&self, d: &Decider, prompt: &str) -> Result<String, String> {
        match &d.how {
            How::Here(service) => {
                self.decision_call(service, super::decide::SYSTEM, prompt)
                    .await
            }
            How::Remote { boss, group } => {
                let answer = self.remote_decision_call(boss, group, prompt).await;
                // Its spend today read again before what it decided is
                // judged: a budget spent meanwhile holds it now.
                self.read_served_spent(boss).await;
                answer
            }
        }
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

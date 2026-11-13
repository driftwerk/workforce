//! Act fully is unbound: every permission request a session asks, of
//! those its boss oversees, is allowed the moment it shows, by Desk, as
//! that boss. No model turn, check-in or threshold stands between the
//! request and its answer, so a boss whose model is refused, limited or
//! slow holds nothing up. Each one is kept as the boss's decision, with why.
//!
//! The person keeps their Stop and kill switches, the budget (spent, Act
//! fully isn't in force and the boss acts within Desk), and what reads or
//! changes an agent's own settings, where that budget is
//! ([`workforce_desk::Desk::decide`] refuses it). What can't be allowed
//! (one of those, or a request asked in a terminal Desk can't answer) is
//! said once in the boss's DM, so nothing waits on the person unsaid.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Level;
use workforce_desk::crew::{By, Kind, Party};
use workforce_desk::model::{DeskEvent, SessionView};

use super::actions::Action;
use super::watcher::tracked;
use super::{name_of, CrewHub};

/// How often every waiting request is looked at again, whatever events
/// were missed.
const SWEEP: Duration = Duration::from_secs(2);

/// Why an unbound boss allowed a request, as its decision says.
pub(super) const WHY: &str = "Act fully: every request is allowed at once";

/// What became of a request Desk looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Allowed, as the boss named.
    Allowed(String),
    /// Not the unbound boss's: the usual rules answer it.
    NotOurs,
    /// Its boss is unbound but it can't be allowed: why, as said in its DM.
    Waits(String),
}

pub(super) async fn allow_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    let mut sweep = tokio::time::interval(SWEEP);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The requests whose wait was said in a DM: each once.
    let mut told: HashSet<Uuid> = HashSet::new();
    loop {
        let sessions: Vec<Uuid> = tokio::select! {
            () = cancel.cancelled() => return,
            received = events.recv() => match received {
                Ok(DeskEvent::Session { session }) if session.pending.is_some() => vec![session.id],
                Ok(DeskEvent::Activity { event }) if event.kind == Kind::NeedsYou => event
                    .subject
                    .as_ref()
                    .and_then(|s| s.id.parse().ok())
                    .into_iter()
                    .collect(),
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => waiting(&crew),
                Err(RecvError::Closed) => return,
            },
            _ = sweep.tick() => waiting(&crew),
        };
        for id in sessions {
            crew.allow_waiting(id, &mut told).await;
        }
        // Only what still waits is remembered.
        if !told.is_empty() {
            let open: HashSet<Uuid> = crew
                .desk
                .list()
                .into_iter()
                .filter_map(|v| v.pending.map(|p| p.id))
                .collect();
            told.retain(|a| open.contains(a));
        }
    }
}

/// The sessions with a request waiting.
fn waiting(crew: &CrewHub) -> Vec<Uuid> {
    crew.desk
        .list()
        .into_iter()
        .filter(|v| v.pending.is_some())
        .map(|v| v.id)
        .collect()
}

impl CrewHub {
    /// The boss over `view`: the one that runs it, else the overseer whose
    /// scope sees it and tracks its kind.
    fn boss_over(&self, view: &SessionView) -> Option<String> {
        if let Some(c) = self.desk.charge_of(view.id).filter(|c| c.live()) {
            return Some(c.boss);
        }
        let (s, o) = self.overseer_of_group(view.group.as_ref().map(|g| g.id.as_str()))?;
        (o.level != Level::Watch && tracked(&o, &Party::session(view)))
            .then(|| s.agent_id.as_str().to_string())
    }

    /// Whether Desk allows `view`'s waiting request at once: its boss is
    /// unbound and Desk can answer it.
    pub(crate) fn answers_at_once(&self, view: &SessionView) -> bool {
        view.pending.as_ref().is_some_and(|p| p.answerable)
            && self
                .boss_over(view)
                .is_some_and(|b| self.desk.boss_unbound(&b))
    }

    /// Allows session `id`'s waiting request, when its boss is unbound.
    /// What can't be allowed is said in the boss's DM, once (`told`).
    pub(crate) async fn allow_waiting(&self, id: Uuid, told: &mut HashSet<Uuid>) -> Outcome {
        let outcome = self.allow_now(id).await;
        let (Outcome::Waits(why), Ok(view)) = (&outcome, self.desk.view(id)) else {
            return outcome;
        };
        let Some(p) = view.pending.as_ref() else {
            return outcome;
        };
        if told.insert(p.id) {
            if let Some(boss) = self.boss_over(&view) {
                let ask = format!("{}({})", p.tool, p.detail);
                let ask = workforce_desk::crew::clip(ask.trim_end_matches("()"), 200);
                self.tell_dm(
                    &boss,
                    &format!("{} is waiting on you to allow {ask}: {why}", view.label()),
                )
                .await;
            }
        }
        outcome
    }

    async fn allow_now(&self, id: Uuid) -> Outcome {
        let Ok(view) = self.desk.view(id) else {
            return Outcome::NotOurs;
        };
        let Some(p) = view.pending.clone() else {
            return Outcome::NotOurs;
        };
        let Some(boss) = self.boss_over(&view) else {
            return Outcome::NotOurs;
        };
        if !self.desk.boss_unbound(&boss) {
            // At Act fully but not in force (its budget spent, its spend
            // unread): the person is told, and the boss acts within Desk.
            if let Some((_, o)) = self.overseer_named(&boss) {
                if o.level == Level::Full {
                    if let Err(why) = self.fully(&boss, &o) {
                        return Outcome::Waits(format!("I can't answer it myself: {why}"));
                    }
                }
            }
            return Outcome::NotOurs;
        }
        if !p.answerable {
            return Outcome::Waits(
                "it asks in its own terminal, where Desk can't answer it; answer it there".into(),
            );
        }
        let name = self
            .overseer_named(&boss)
            .map_or_else(|| boss.clone(), |(s, _)| name_of(&s));
        let ask = match p.detail.trim() {
            "" => p.tool.clone(),
            d => format!("{}({d})", p.tool),
        };
        let by = By::manager(&boss, &name, &format!("{WHY}: {ask}"));
        let action = Action::Approve {
            session: id,
            approval: p.id,
        };
        match self.perform(&action, by).await {
            Ok(done) => {
                tracing::info!(session = %id, boss = %boss, "allowed at once at Act fully");
                Outcome::Allowed(done)
            }
            // Answered meanwhile (in its terminal, by the person, by the
            // last look): nothing waits.
            Err(_)
                if self
                    .desk
                    .view(id)
                    .ok()
                    .and_then(|v| v.pending)
                    .map(|q| q.id)
                    != Some(p.id) =>
            {
                Outcome::NotOurs
            }
            Err(why) => {
                tracing::warn!(session = %id, boss = %boss, %why, "couldn't allow a request at Act fully");
                Outcome::Waits(format!("I couldn't allow it: {why}"))
            }
        }
    }

    /// Says `text` in `boss`'s DM, as a notice.
    async fn tell_dm(&self, boss: &str, text: &str) {
        let Some((service, _)) = self.overseer_named(boss) else {
            tracing::warn!(%boss, text, "no DM to say a waiting request in");
            return;
        };
        let Some(owner) = self.person(Some(&service)) else {
            return;
        };
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        if let Err(e) = crate::desk_surface::deliver(
            &service,
            &owner,
            &target,
            crate::desk_surface::Delivery::text(DeliveryKind::Notice, text.to_string()),
        )
        .await
        {
            tracing::warn!(%boss, error = %e, "saying a waiting request in the DM failed");
        }
    }
}

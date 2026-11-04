//! Charges ([`crate::charge`]) as the desk runs them: a boss takes a
//! session on (or the person hands it one), and lets it go; the person
//! takes the wheel by giving the session input, and hands it back. Each
//! change is stored, recorded in Activity and announced as
//! `{type: "charge", charge}`.
//!
//! While the person has the wheel the boss's messages to the session wait
//! on its line ([`crate::line::Wait::PersonDriving`]); who has it is read
//! from the person's last input each time it is asked, so a hold starts the
//! moment they type. A loop of its own writes each change of hands down and
//! lets what waited go once the wheel is back.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::charge::{self, Charge, How, NotChargeable, Wheel};
use crate::crew::{self, By, Kind, Party, PartyKind};
use crate::line::{Delivery, LineKind, LineMessage};
use crate::model::{DeskEvent, SessionState};

/// What the desk keeps of charges in memory: every row, and when the
/// person last gave each session input Desk doesn't see in its terminal.
#[derive(Default)]
pub(crate) struct Charges {
    rows: HashMap<Uuid, Charge>,
    /// The person's own prompts and composer messages, by session.
    driven: HashMap<Uuid, DateTime<Utc>>,
    /// What each boss that took a session on is called, by agent id.
    names: HashMap<String, String>,
    /// Whether the rows were read. Until they are, Desk doesn't know which
    /// sessions a boss runs or who drives them: every boss's message to a
    /// session waits, and nothing is taken on.
    loaded: bool,
}

/// A charge as Desk shows it: its boss by name, and who has the wheel now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargeView {
    pub session: Uuid,
    pub group: Option<String>,
    pub boss: String,
    pub boss_name: String,
    pub how: How,
    pub wheel: Wheel,
    pub wheel_since: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

/// How often who has the wheel is looked at.
const SETTLE_EVERY: Duration = Duration::from_secs(5);

impl Desk {
    /// Reads the charges as kept; whether they were. What the person
    /// typed before a restart isn't known after it, so a wheel of theirs
    /// counts its quiet from now.
    pub(super) async fn charge_load(&self) -> bool {
        match self.0.store.charges().await {
            Ok(rows) => {
                let now = crew::now();
                let rows = rows.into_iter().map(|mut c| {
                    if c.wheel == Wheel::Person {
                        c.wheel_since = Some(now);
                    }
                    (c.session, c)
                });
                let mut st = self.0.state.lock();
                st.charges.rows = rows.collect();
                st.charges.loaded = true;
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read charges; bosses' messages wait until they are"
                );
                false
            }
        }
    }

    /// Whether Desk knows which sessions bosses run.
    pub fn charges_loaded(&self) -> bool {
        self.0.state.lock().charges.loaded
    }

    /// The boss there is now: the overseer of the hub's crew.
    pub fn boss(&self) -> Option<crew::Overseer> {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)?;
        crew.overseer()
    }

    fn boss_name(&self, id: &str) -> String {
        if let Some(o) = self.boss().filter(|o| o.id == id) {
            return o.name;
        }
        let st = self.0.state.lock();
        st.charges
            .names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// The person gave `id` a prompt or wrote to it: they take its wheel.
    pub(super) fn person_drove(&self, id: Uuid) {
        self.0.state.lock().charges.driven.insert(id, Utc::now());
    }

    /// When the person last gave `id` input: typed in its terminal,
    /// prompted it, or wrote to it.
    fn last_input(st: &super::State, id: Uuid) -> Option<DateTime<Utc>> {
        let typed = st.live.get(&id).and_then(|l| l.pty.typed_at());
        typed.max(st.charges.driven.get(&id).copied())
    }

    /// `c`'s wheel now, as `st` reads.
    fn wheel_now(st: &super::State, c: &Charge) -> Wheel {
        let state = st
            .records
            .get(&c.session)
            .map_or(SessionState::Closed, |r| r.state);
        let since = st.state_since.get(&c.session).copied();
        charge::wheel(c, Self::last_input(st, c.session), state, since, Utc::now())
    }

    fn charge_view(&self, c: &Charge, wheel: Wheel) -> ChargeView {
        ChargeView {
            session: c.session,
            group: c.group.clone(),
            boss: c.boss.clone(),
            boss_name: self.boss_name(&c.boss),
            how: c.how,
            wheel,
            wheel_since: c.wheel_since,
            since: c.since,
            released_at: c.released_at,
        }
    }

    /// `id`'s charge, released or not.
    pub fn charge_of(&self, id: Uuid) -> Option<Charge> {
        self.0.state.lock().charges.rows.get(&id).cloned()
    }

    /// Every live charge, with its boss's name and who has the wheel.
    pub fn charges(&self) -> Vec<ChargeView> {
        let live: Vec<(Charge, Wheel)> = {
            let st = self.0.state.lock();
            st.charges
                .rows
                .values()
                .filter(|c| c.live())
                .map(|c| (c.clone(), Self::wheel_now(&st, c)))
                .collect()
        };
        let mut views: Vec<ChargeView> =
            live.iter().map(|(c, w)| self.charge_view(c, *w)).collect();
        views.sort_by_key(|v| v.since);
        views
    }

    /// Who drives `id` now, while a boss runs it.
    pub fn wheel_of(&self, id: Uuid) -> Option<Wheel> {
        let st = self.0.state.lock();
        let c = st.charges.rows.get(&id).filter(|c| c.live())?;
        Some(Self::wheel_now(&st, c))
    }

    /// Whether the person has the wheel of `id`, which a boss runs: its
    /// boss's messages to it wait.
    pub(crate) fn person_driving(&self, id: Uuid) -> bool {
        // Not knowing who drives, it is held.
        !self.charges_loaded() || self.wheel_of(id).is_some_and(Wheel::person)
    }

    /// Where a boss's answers for its sessions are judged, as the hub
    /// running it says.
    pub fn boss_bounds_with(&self, bounds: charge::Bounds) {
        *self.0.boss_bounds.lock() = Some(bounds);
    }

    /// Whether the boss running `id` may answer its open request
    /// `approval` itself; why not, in the person's words, when it may not.
    pub fn boss_answerable(&self, id: Uuid, approval: Uuid) -> Result<(), charge::Irreversible> {
        let no = |why: &str| Err(charge::Irreversible(why.into()));
        let Some(bounds) = self.0.boss_bounds.lock().clone() else {
            return no("Desk can't judge requests for a boss yet");
        };
        let Ok(view) = self.view(id) else {
            return no("that session isn't there any more");
        };
        let Some(pending) = self.pending_of(id, approval) else {
            return no("already answered");
        };
        let facts = charge::facts_of(view.engine, &pending, &view, &bounds.home);
        charge::answerable(&facts, &bounds)
    }

    /// Whether `by`, when it is a boss, may act on `id` now: a session a
    /// boss runs is acted on only by that boss, and only while it has the
    /// wheel. Anyone else, and a session no boss runs, is the hub's rule.
    pub(crate) fn boss_holds(&self, id: Uuid, by: Option<&By>) -> Result<(), DeskError> {
        let Some(boss) = by.filter(|b| b.party.kind == PartyKind::Manager) else {
            return Ok(());
        };
        let Some(c) = self.charge_of(id).filter(Charge::live) else {
            return Ok(());
        };
        let name = self
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "that session".into());
        if c.boss != boss.party.id {
            return Err(DeskError::Conflict(format!(
                "another boss runs {name}; it's theirs or the person's to act on"
            )));
        }
        if self.wheel_of(id).is_some_and(Wheel::person) {
            return Err(DeskError::Conflict(format!(
                "the person is driving {name}, so it's theirs to act on now"
            )));
        }
        Ok(())
    }

    /// `id`'s open request `approval`: the one it waits on, or another of
    /// Codex's.
    fn pending_of(&self, id: Uuid, approval: Uuid) -> Option<crate::model::PendingApproval> {
        let st = self.0.state.lock();
        let l = st.live.get(&id)?;
        l.pending
            .iter()
            .chain(l.codex_asks.iter().map(|a| &a.pending))
            .find(|p| p.id == approval)
            .cloned()
    }

    /// Whether `boss` may make `decision` on `id`'s request `approval`:
    /// on a session it runs, only while it has the wheel and Desk's breaker
    /// doesn't hold it, never for the rest of the session, and to allow
    /// only what [`charge::answerable`] says it may. A session no boss runs
    /// is the hub's rule to judge.
    pub(super) fn boss_decides(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: crate::model::Decision,
        boss: &By,
    ) -> Result<(), DeskError> {
        use crate::model::Decision;
        if !self.charge_of(id).is_some_and(|c| c.live()) {
            return Ok(());
        }
        self.boss_holds(id, Some(boss))?;
        self.breaker_refuses_prompt(id, Some(boss))?;
        if self.pending_of(id, approval).is_none() {
            return Err(DeskError::Conflict("already answered".into()));
        }
        match decision {
            Decision::Deny | Decision::Cancel => Ok(()),
            Decision::Allow => self
                .boss_answerable(id, approval)
                .map_err(|why| DeskError::Conflict(format!("{why}: it's the person's to allow"))),
            Decision::AllowForSession | Decision::Terminal => Err(DeskError::Invalid(
                "only the person answers for the rest of a session".into(),
            )),
        }
    }

    /// How many of `boss`'s sessions are starting or working: the slots
    /// they hold.
    pub fn charges_busy(&self, boss: &str) -> usize {
        let st = self.0.state.lock();
        let mine: Vec<Charge> = st
            .charges
            .rows
            .values()
            .filter(|c| c.boss == boss)
            .cloned()
            .collect();
        let states: HashMap<Uuid, SessionState> = mine
            .iter()
            .filter_map(|c| st.records.get(&c.session).map(|r| (c.session, r.state)))
            .collect();
        charge::busy(&mine, &states)
    }

    async fn put_charge(&self, c: Charge, wheel: Wheel) -> Result<ChargeView, DeskError> {
        self.0.store.put_charge(&c).await?;
        self.0
            .state
            .lock()
            .charges
            .rows
            .insert(c.session, c.clone());
        let view = self.charge_view(&c, wheel);
        let _ = self.0.events.send(DeskEvent::Charge {
            charge: Box::new(view.clone()),
        });
        Ok(view)
    }

    /// Notes on `id`'s line, in its thread, `text` from `boss`: never typed
    /// in, so it starts no turn (a session just started, or one the person
    /// now runs alone, does nothing because of it). What goes wrong is only
    /// logged: the charge stands either way.
    async fn charge_note(&self, view: &crate::model::SessionView, boss: &Party, text: &str) {
        let mut m = LineMessage::new(
            boss.clone(),
            Party::session(view),
            LineKind::Tell,
            text.to_string(),
            1,
        );
        m.delivery = Delivery::Noted;
        if let Err(e) = self.0.store.put_line(&m).await {
            tracing::info!(session = %view.id, error = %e, "a session's charge wasn't noted on its line");
            return;
        }
        let _ = self.0.events.send(DeskEvent::Line {
            message: Box::new(m),
            removed: false,
        });
    }

    /// What `id`'s boss said to it that hasn't gone in never will: it no
    /// longer runs the session. Each is marked withdrawn in the thread.
    async fn charge_withdraw(&self, id: Uuid) {
        match self.0.store.line_withdraw_overseers(&id.to_string()).await {
            Ok(rows) => {
                for m in rows {
                    let _ = self.0.events.send(DeskEvent::Line {
                        message: Box::new(m),
                        removed: false,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't withdraw a boss's messages")
            }
        }
    }

    /// `boss` takes `id` on (`how`), as `by`: the boss itself, or the
    /// person handing it over. `bossed` are the groups with a boss of their
    /// own, which this one doesn't run. Taking on one already the boss's
    /// changes nothing.
    pub async fn take_on(
        &self,
        id: Uuid,
        boss: &Party,
        how: How,
        bossed: &HashSet<String>,
        by: &By,
    ) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        if !self.charges_loaded() {
            return Err(DeskError::Conflict(
                "Desk can't read which sessions bosses run just now; try again shortly.".into(),
            ));
        }
        charge::chargeable(&view, bossed)
            .map_err(|e: NotChargeable| DeskError::Conflict(e.to_string()))?;
        let existing = self.charge_of(id).filter(Charge::live);
        if let Some(c) = existing.as_ref().filter(|c| c.boss == boss.id) {
            let wheel = self.wheel_of(id).unwrap_or(Wheel::Boss);
            return Ok(self.charge_view(c, wheel));
        }
        self.0
            .state
            .lock()
            .charges
            .names
            .insert(boss.id.clone(), boss.name.clone());
        let mut c = Charge::new(id, &boss.id, how, view.group.as_ref().map(|g| g.id.clone()));
        // From another boss: the person driving it keeps driving.
        let wheel = match existing {
            Some(_) => self.wheel_of(id).unwrap_or(Wheel::Boss),
            None => Wheel::Boss,
        };
        if wheel.person() {
            c.wheel = wheel;
            c.wheel_since = Some(c.since);
        }
        let shown = self.put_charge(c, wheel).await?;
        let name = view.label().to_string();
        let text = match by.party.kind {
            PartyKind::You => format!("handed {name} to {}", boss.name),
            _ => format!("took on {name}"),
        };
        self.note(by.event(Kind::TakenOn, Some(Party::session(&view)), text));
        self.charge_note(
            &view,
            boss,
            "I'm running this session with the person now. Ask me (to: \"desk\") when you're unsure; they can take over at any time.",
        )
        .await;
        Ok(shown)
    }

    /// `id`, which a boss no longer overseeing ran, passes to `boss`; or,
    /// when it can't be run any more (it ended), is let go.
    pub async fn pass_charge(
        &self,
        id: Uuid,
        boss: &Party,
        bossed: &HashSet<String>,
        by: &By,
    ) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        match charge::chargeable(&view, bossed) {
            Ok(()) => {
                let how = self.charge_of(id).map_or(How::Taken, |c| c.how);
                self.take_on(id, boss, how, bossed, by).await
            }
            Err(_) => self.release(id, by).await,
        }
    }

    /// `id`'s boss lets it go, or the person takes it back (`by`). Its row
    /// stays, so a boss that takes on every session never takes it again.
    pub async fn release(&self, id: Uuid, by: &By) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(mut c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        // What it said and the person held goes nowhere: before the release
        // lets it through, and again for anything said meanwhile.
        self.charge_withdraw(id).await;
        let now = crew::now();
        c.released_at = Some(now);
        c.updated_at = now;
        let boss = Party::manager(&c.boss, &self.boss_name(&c.boss));
        let wheel = c.wheel;
        let shown = self.put_charge(c, wheel).await?;
        self.charge_withdraw(id).await;
        let name = view.label().to_string();
        let text = match by.party.kind {
            PartyKind::You => format!("took {name} back from {}", boss.name),
            _ => format!("let {name} go"),
        };
        self.note(by.event(Kind::Released, Some(Party::session(&view)), text));
        self.charge_note(
            &view,
            &boss,
            "I'm no longer running this session: the person runs it on their own now.",
        )
        .await;
        Ok(shown)
    }

    /// The person takes the wheel of `id` until they hand it back.
    pub async fn drive(&self, id: Uuid) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(mut c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        let now = crew::now();
        c.wheel = Wheel::PersonPinned;
        c.wheel_since = Some(now);
        c.updated_at = now;
        let shown = self.put_charge(c, Wheel::PersonPinned).await?;
        self.note(By::you().event(
            Kind::Driving,
            Some(Party::session(&view)),
            format!("is driving {} until they hand it back", view.label()),
        ));
        Ok(shown)
    }

    /// The person hands `id` back to its boss: what the boss said to it
    /// while they drove goes in now.
    pub async fn hand_back(&self, id: Uuid) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        let boss = self.boss_name(&c.boss);
        let shown = self.wheel_back(c).await?;
        self.note(By::you().event(
            Kind::HandedBack,
            Some(Party::session(&view)),
            format!("handed {} back to {boss}", view.label()),
        ));
        Ok(shown)
    }

    /// The boss has `c`'s wheel again, from now: input before doesn't take
    /// it back.
    async fn wheel_back(&self, mut c: Charge) -> Result<ChargeView, DeskError> {
        let now = crew::now();
        c.wheel = Wheel::Boss;
        c.wheel_since = Some(now);
        c.updated_at = now;
        let id = c.session;
        let shown = self.put_charge(c, Wheel::Boss).await?;
        self.charge_release_held(id).await;
        Ok(shown)
    }

    /// What `id`'s boss said to it while the person drove goes in: typed in
    /// now, and Desk's plugin, waiting for a message, hears of it.
    async fn charge_release_held(&self, id: Uuid) {
        match self.0.store.line_inbox(&id.to_string()).await {
            Ok(rows) => {
                for m in rows
                    .into_iter()
                    .filter(|m| m.from.kind == PartyKind::Manager && m.kind == LineKind::Tell)
                {
                    let _ = self.0.events.send(DeskEvent::Line {
                        message: Box::new(m),
                        removed: false,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't read what waited for a session")
            }
        }
        let desk = self.clone();
        tokio::spawn(async move { desk.line_pump(id).await });
    }

    /// Writes down each change of hands since last looked: the person took
    /// a wheel by their input, or it came back after they went quiet.
    async fn charge_settle(&self) {
        let changed: Vec<(Charge, Wheel, Option<DateTime<Utc>>)> = {
            let st = self.0.state.lock();
            st.charges
                .rows
                .values()
                .filter(|c| c.live())
                .filter_map(|c| {
                    let now = Self::wheel_now(&st, c);
                    let was = c.wheel;
                    (now != was && was != Wheel::PersonPinned)
                        .then(|| (c.clone(), now, Self::last_input(&st, c.session)))
                })
                .collect()
        };
        for (mut c, now, input) in changed {
            let Ok(view) = self.view(c.session) else {
                continue;
            };
            let result = match now {
                Wheel::Person => {
                    c.wheel = Wheel::Person;
                    c.wheel_since = input.or(Some(crew::now()));
                    c.updated_at = crew::now();
                    let done = self.put_charge(c, Wheel::Person).await;
                    if done.is_ok() {
                        self.note(By::you().event(
                            Kind::Driving,
                            Some(Party::session(&view)),
                            format!("is driving {}", view.label()),
                        ));
                    }
                    done
                }
                Wheel::Boss => {
                    let boss = Party::manager(&c.boss, &self.boss_name(&c.boss));
                    let done = self.wheel_back(c).await;
                    if done.is_ok() {
                        self.note(crew::Event::new(
                            Kind::HandedBack,
                            boss,
                            Some(Party::session(&view)),
                            format!(
                                "has {} back after {} quiet minutes",
                                view.label(),
                                charge::QUIET.num_minutes()
                            ),
                        ));
                    }
                    done
                }
                Wheel::PersonPinned => continue,
            };
            if let Err(e) = result {
                tracing::warn!(session = %view.id, error = %e, "couldn't write down who drives a session");
            }
        }
    }

    /// The charges' own loop: who has each wheel is written down as it
    /// changes hands, and a forgotten session's charge goes with it.
    pub(super) async fn charge_loop(self) {
        // Read again until they are, waiting longer each time.
        let mut wait = Duration::from_secs(1);
        while !self.charges_loaded() {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            if self.charge_load().await {
                break;
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
        let mut rx = self.0.events.subscribe();
        let mut tick = tokio::time::interval(SETTLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => self.charge_settle().await,
                ev = rx.recv() => match ev {
                    Ok(DeskEvent::Removed { id }) => {
                        let mut st = self.0.state.lock();
                        st.charges.rows.remove(&id);
                        st.charges.driven.remove(&id);
                    }
                    Ok(DeskEvent::Session { session })
                        if self.charge_of(session.id).is_some_and(|c| c.live()) =>
                    {
                        self.charge_settle().await
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }
}

#[cfg(test)]
#[path = "charge_tests.rs"]
mod tests;

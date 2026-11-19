//! A boss's urgent words to a working session (design note §4): steered
//! into its running turn rather than queued for the turn's end.
//!
//! - Claude Code and Devin read them beside their next tool call's result
//!   (`additionalContext` from Desk's PostToolUse hook, as the breaker's
//!   warnings are): the row waits in the session's steer queue
//!   ([`super::breaker::Steer::Line`]) and stays `queued` until a hook takes
//!   it. Codex takes them with `turn/steer`, as the person's Send now does.
//! - A row is delivered once: whoever moves it from `queued` (the hook, the
//!   idle pump, the plugin, the session's inbox) delivers it, and every
//!   other path finds it moved. While its session is in a turn, the pump
//!   and the plugin leave a row kept as urgent ([`Desk::steer_reserved`],
//!   read from the row itself, so it holds from the moment it is kept and
//!   across a restart); once the turn is over, the row goes out at idle.
//! - A session that can't take words mid-turn gets them at its turn's end,
//!   and the sender is told so.
//! - A stand-down that escalates, not read within the steer wait in the
//!   turn it was sent in, has that turn interrupted through the boss's own
//!   gate ([`crate::crew::Crew::steer_overdue`], [`Desk::interrupt_turn`]):
//!   never a turn begun since.
//! - At most [`line::URGENT_PER_SESSION`] urgent orders per session in
//!   [`line::URGENT_WINDOW`]; stand-downs aren't counted.
//! - After a restart, what was queued urgently is held for its session's
//!   running turn, an escalating stand-down is timed again at once, and the
//!   rest is steered again from the session's first tool call heard.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use super::breaker::Steer;
use super::line::LineError;
use super::{Desk, DeskError};
use crate::crew::{By, Event, Kind, Party, PartyKind};
use crate::engine::Engine;
use crate::line::{self, LineMessage, MidTurn, Rushed, TurnMark, Urgency};
use crate::model::SessionView;

/// What the desk keeps for urgent delivery.
pub(crate) struct Rush {
    /// How long an escalating stand-down waits to be read, in ms.
    wait_ms: AtomicU64,
    /// Held from counting a session's urgent orders until the new one is
    /// kept, so two sent at once can't both pass the limit.
    write: tokio::sync::Mutex<()>,
    /// What a restart found queued urgently, by session: steered again once
    /// the session's first tool call is heard in this run.
    unarmed: parking_lot::Mutex<HashMap<Uuid, Vec<Unarmed>>>,
}

/// A row a restart found queued urgently.
#[derive(Debug, Clone, Copy)]
struct Unarmed {
    row: Uuid,
    /// Its escalation was timed again at the restart already.
    armed: bool,
}

impl Default for Rush {
    fn default() -> Self {
        Self {
            wait_ms: AtomicU64::new(line::STEER_WAIT.as_millis() as u64),
            write: tokio::sync::Mutex::new(()),
            unarmed: parking_lot::Mutex::default(),
        }
    }
}

/// How an urgent send is to go, decided before its row is kept.
pub(super) struct Plan<'a> {
    pub(super) rushed: Rushed,
    /// The turn it was sent in, while the session was in one.
    pub(super) mark: Option<TurnMark>,
    /// The limit's count is held until the row is kept.
    pub(super) counted: Option<tokio::sync::MutexGuard<'a, ()>>,
}

/// What a hook made of one of the boss's rows it took from the queue.
enum Took {
    /// Delivered: its words, as the session reads them.
    Read(String),
    /// Held from the session now (its gate, a wait on it): back in the
    /// queue for a later call, or the turn's end.
    Back,
    /// Gone in another way, or never to go in.
    Gone,
}

impl Desk {
    /// How long an escalating stand-down waits to be read in a running turn
    /// before the turn is interrupted: [`line::STEER_WAIT`] unless set.
    pub fn steer_wait(&self) -> Duration {
        Duration::from_millis(self.0.rush.wait_ms.load(Ordering::Relaxed))
    }

    /// Sets [`Self::steer_wait`] (tests wait less than a minute).
    pub fn set_steer_wait(&self, wait: Duration) {
        self.0
            .rush
            .wait_ms
            .store(wait.as_millis() as u64, Ordering::Relaxed);
    }

    /// The turn `id` is in now, while it is in one.
    pub fn turn_mark(&self, id: Uuid) -> Option<TurnMark> {
        let st = self.0.state.lock();
        Self::mark_in(&st, id)
    }

    pub(super) fn mark_in(st: &super::State, id: Uuid) -> Option<TurnMark> {
        let state = st.records.get(&id)?.state;
        let l = st.live.get(&id)?;
        line::in_turn(state).then_some(TurnMark {
            run: l.breaker.run.0,
            ends: l.ends,
            turns: l.breaker.turns,
        })
    }

    /// How a boss's urgent `m` would reach `v` now, decided before `m` is
    /// kept: the limit is counted under a lock held (the plan's `counted`)
    /// until `m` is written, and `m` carries its urgency only when it is to
    /// go into the running turn, or wait for its end as urgent.
    pub(super) async fn rush_plan(
        &self,
        v: &SessionView,
        m: &mut LineMessage,
        urgency: Urgency,
    ) -> Result<Plan<'_>, LineError> {
        let (engine, heard, dialog, link, mark) = {
            let st = self.0.state.lock();
            let engine = st.records.get(&v.id).map(|r| r.engine);
            let mark = Self::mark_in(&st, v.id);
            match st.live.get(&v.id) {
                Some(l) => (
                    engine,
                    l.breaker.heard,
                    l.pending.is_some(),
                    l._codex.is_some(),
                    mark,
                ),
                None => (engine, false, false, false, None),
            }
        };
        let plan = |rushed| Plan {
            rushed,
            mark,
            counted: None,
        };
        let heard = match engine {
            Some(Engine::Codex) => link,
            _ => heard,
        };
        let how = match engine {
            Some(engine) => line::mid_turn(engine, v.state, heard, dialog),
            None => Ok(None),
        };
        if matches!(how, Ok(None)) || !v.live {
            return Ok(plan(Rushed::NotWorking));
        }
        // What holds the boss's words from it at idle holds them now too.
        if let Some(wait) = self.line_gate(v, m) {
            return Ok(plan(Rushed::Held(wait)));
        }
        let counted = match urgency.counts() {
            true => {
                let guard = self.0.rush.write.lock().await;
                let since = Utc::now()
                    - chrono::Duration::from_std(line::URGENT_WINDOW).unwrap_or_default();
                let sent = self
                    .0
                    .store
                    .line_urgent_since(&v.id.to_string(), since)
                    .await?;
                if sent >= line::URGENT_PER_SESSION {
                    return Ok(plan(Rushed::TooMany));
                }
                Some(guard)
            }
            false => None,
        };
        m.urgency = Some(urgency);
        // Reserved for this turn from before it is kept: the plugin, woken
        // by its announcement, finds it held.
        if let Some(mark) = mark {
            if let Some(l) = self.0.state.lock().live.get_mut(&v.id) {
                l.breaker.rushed.insert(m.id, mark);
            }
        }
        let rushed = match how {
            Ok(Some(MidTurn::Hook)) => Rushed::Steered,
            Ok(Some(MidTurn::TurnSteer)) => Rushed::SaidIn,
            Err(why) => Rushed::CantSteer(why),
            Ok(None) => Rushed::NotWorking,
        };
        Ok(Plan {
            rushed,
            mark,
            counted,
        })
    }

    /// Sends the kept urgent `m` on to session `id` as `rushed` planned, as
    /// `by`, sent in turn `mark` (an escalating stand-down is timed in it).
    /// What can't go into the running turn goes as any message.
    pub(super) async fn rush_deliver(
        &self,
        m: &mut LineMessage,
        id: Uuid,
        rushed: Rushed,
        mark: Option<TurnMark>,
        by: &By,
    ) -> Result<(), LineError> {
        let urgency = m.urgency;
        match rushed {
            Rushed::Steered => {
                if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                    l.breaker.steers.push_back(Steer::Line { row: m.id });
                }
                // A turn that ended as it was queued: the idle takes it.
                self.line_pump_later(id);
            }
            Rushed::SaidIn => self.rush_codex(id, m, by).await?,
            Rushed::NotWorking | Rushed::CantSteer(_) | Rushed::Held(_) | Rushed::TooMany => {
                self.line_pump_later(id)
            }
        }
        if let (true, Some(mark)) = (urgency.is_some_and(Urgency::escalates), mark) {
            if matches!(rushed, Rushed::Steered | Rushed::CantSteer(_)) {
                self.rush_arm(id, m.id, mark);
            }
        }
        Ok(())
    }

    /// Says a boss's `m` into Codex session `id`'s running turn, marked so
    /// it can be answered: delivered as it is said, and back in the queue if
    /// Codex refuses it ([`Self::rush_refused`]).
    async fn rush_codex(&self, id: Uuid, m: &mut LineMessage, by: &By) -> Result<(), LineError> {
        let Some(taken) = self
            .0
            .store
            .line_take(m.id, line::Delivery::Delivered)
            .await?
        else {
            return Ok(());
        };
        *m = taken;
        let text = self.rendered(m).await;
        // Codex says it back as the turn's input: Desk's, not the person's.
        self.breaker_typed(id, &text, Some(by));
        self.line_delivered_into(id, m);
        let sent = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .and_then(|l| l._codex.as_ref())
            .is_some_and(|c| c.line_steers.send((m.id, text)).is_ok());
        if !sent {
            self.line_steer_refused(id, m.id).await;
        }
        Ok(())
    }

    /// Codex refused `m`, back in the queue: an escalating stand-down is
    /// timed in the turn still running, as one queued for a hook is.
    pub(super) fn rush_refused(&self, id: Uuid, m: &LineMessage) {
        if !m.urgency.is_some_and(Urgency::escalates) {
            return;
        }
        if let Some(mark) = self.turn_mark(id) {
            self.rush_arm(id, m.id, mark);
        }
    }

    /// Whether `m` waits for `id`'s running turn: kept as urgent for it.
    /// The pump and the plugin leave it meanwhile; the turn's next tool call
    /// takes it, or its end lets it go. Only a row this turn holds: one in
    /// its steer queue, being taken or escalated, sent in this turn, or left
    /// by a restart for it. An older urgent row (one a gate held at an
    /// idle) is any message's, in any later turn.
    pub(super) fn steer_reserved(&self, id: Uuid, m: &LineMessage) -> bool {
        if m.urgency.is_none() {
            return false;
        }
        let left = self
            .0
            .rush
            .unarmed
            .lock()
            .get(&id)
            .is_some_and(|rows| rows.iter().any(|u| u.row == m.id));
        let st = self.0.state.lock();
        let Some(mark) = Self::mark_in(&st, id) else {
            return false;
        };
        let Some(l) = st.live.get(&id) else {
            return false;
        };
        let b = &l.breaker;
        if b.stopped == Some(mark) {
            return false;
        }
        left || b.rushed.get(&m.id) == Some(&mark)
            || b.taking.contains(&m.id)
            || b.escalating.contains(&m.id)
            || b.steers
                .iter()
                .any(|s| matches!(s, Steer::Line { row } if *row == m.id))
    }

    /// Esc ended `id`'s turn: what waited for its next call goes in at the
    /// idle, never into a call the person stopped, even while Desk still
    /// reads the session as working.
    pub(super) fn steers_stopped(&self, id: Uuid) {
        {
            let mut st = self.0.state.lock();
            let mark = Self::mark_in(&st, id);
            if let Some(l) = st.live.get_mut(&id) {
                l.breaker.stopped = mark;
            }
        }
        self.steers_released(id);
    }

    /// What goes with `id`'s tool call result (hook `event`): the breaker's
    /// warning, and, on the main thread's call (`main`), the boss's urgent
    /// messages still queued, each taken as it is delivered. None when
    /// nothing is. A row taken as the turn ended or was interrupted is put
    /// back for the idle: that call's result may never be read.
    pub(super) async fn steers_take(&self, id: Uuid, event: &str, main: bool) -> Option<Value> {
        let (steers, mark) = {
            let mut st = self.0.state.lock();
            let mark = Self::mark_in(&st, id);
            let b = &mut st.live.get_mut(&id)?.breaker;
            let mut taken = Vec::new();
            let mut kept = VecDeque::new();
            for s in b.steers.drain(..) {
                match &s {
                    Steer::Breaker(..) => taken.push(s),
                    Steer::Line { row } if main && !b.escalating.contains(row) => {
                        b.taking.insert(*row);
                        taken.push(s);
                    }
                    Steer::Line { .. } => kept.push_back(s),
                }
            }
            b.steers = kept;
            (taken, mark)
        };
        let drained: Vec<Uuid> = steers
            .iter()
            .filter_map(|s| match s {
                Steer::Line { row } => Some(*row),
                Steer::Breaker(..) => None,
            })
            .collect();
        let mut warned = Vec::new();
        let mut said = Vec::new();
        let mut read = Vec::new();
        let mut back = Vec::new();
        for s in steers {
            match s {
                Steer::Breaker(_, text) => warned.push(text),
                Steer::Line { row } => match self.rush_take(id, row).await {
                    Ok(Took::Read(text)) => {
                        said.push(text);
                        read.push(row);
                    }
                    Ok(Took::Back) => back.push(row),
                    Ok(Took::Gone) => {}
                    Err(e) => {
                        // Left queued: a later call, or the idle, takes it.
                        tracing::warn!(session = %id, %row, error = %e, "an urgent line message couldn't be taken into a turn");
                        back.push(row);
                    }
                },
            }
        }
        let over = mark.is_none() || self.turn_mark(id) != mark;
        if over && !read.is_empty() {
            // Taken as its turn ended: that result may never reach the
            // session. Back in the queue, for the idle.
            // The breaker's warning stands: it was always said with the
            // call that took it.
            for row in std::mem::take(&mut read) {
                let _ = self.0.store.line_untake(row, &id.to_string()).await;
            }
            said.clear();
        }
        {
            let mut st = self.0.state.lock();
            if let Some(l) = st.live.get_mut(&id) {
                for row in &drained {
                    l.breaker.taking.remove(row);
                }
                if !over {
                    for row in back.into_iter().rev() {
                        l.breaker.steers.push_front(Steer::Line { row });
                    }
                }
            }
        }
        if over && !drained.is_empty() {
            // What it took or held back goes in at the idle.
            self.steers_released(id);
        }
        warned.extend(said);
        (!warned.is_empty()).then(|| crate::claude::context(event, &warned.join("\n\n")))
    }

    /// Takes the boss's `row` into `id`'s turn.
    async fn rush_take(&self, id: Uuid, row: Uuid) -> Result<Took, LineError> {
        let Some(m) = self.0.store.line_message(row).await? else {
            return Ok(Took::Gone);
        };
        if !m.waits() || self.line_overtaken(&m).await {
            return Ok(Took::Gone);
        }
        let view = self.view(id)?;
        if self.waited_on(&m) || self.line_gate(&view, &m).is_some() {
            return Ok(Took::Back);
        }
        let Some(taken) = self
            .0
            .store
            .line_take(row, line::Delivery::Delivered)
            .await?
        else {
            return Ok(Took::Gone);
        };
        self.line_delivered_into(id, &taken);
        tracing::info!(session = %id, %row, "an urgent line message was read mid-turn");
        Ok(Took::Read(self.rendered(&taken).await))
    }

    /// `id`'s boss messages waiting for its next tool call no longer do (its
    /// turn ended, or was interrupted): they go in at the idle, and Desk's
    /// plugin, if it waits on the line, hears they are there.
    pub(super) fn steers_released(&self, id: Uuid) {
        let rows: Vec<Uuid> = {
            let mut st = self.0.state.lock();
            let mark = Self::mark_in(&st, id);
            let Some(l) = st.live.get_mut(&id) else {
                return;
            };
            // What was reserved for a turn that is over is no longer.
            l.breaker.rushed.retain(|_, m| Some(*m) == mark);
            let mut rows = Vec::new();
            l.breaker.steers.retain(|s| match s {
                Steer::Line { row } => {
                    rows.push(*row);
                    false
                }
                Steer::Breaker(..) => true,
            });
            rows
        };
        // What else waits urgently for it was reserved for the turn too
        // (one that couldn't be steered, one a restart left): a plugin
        // already waiting on the line hears it now, not at its next look.
        let desk = self.clone();
        tokio::spawn(async move {
            let mut rows = rows;
            match desk.0.store.line_urgent_queued_to(&id.to_string()).await {
                Ok(more) => rows.extend(more.into_iter().map(|m| m.id)),
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "couldn't read what waits urgently for a turn that ended");
                }
            }
            let mut seen = std::collections::HashSet::new();
            rows.retain(|r| seen.insert(*r));
            if rows.is_empty() {
                return;
            }
            for row in rows {
                if let Ok(Some(m)) = desk.0.store.line_message(row).await {
                    if m.waits() {
                        desk.line_announce(&m);
                    }
                }
            }
            desk.line_pump(id).await;
        });
    }

    /// A tool call of `id`'s was heard: words beside a call's result reach
    /// it. The first in this run steers again what a restart left queued
    /// urgently for it, before the hook answers, so that call's result can
    /// carry it.
    pub(super) async fn steer_heard(&self, id: Uuid) {
        let first = {
            let mut st = self.0.state.lock();
            match st.live.get_mut(&id) {
                Some(l) => !std::mem::replace(&mut l.breaker.heard, true),
                None => false,
            }
        };
        if !first {
            return;
        }
        let Some(rows) = self.0.rush.unarmed.lock().remove(&id) else {
            return;
        };
        self.rush_rearm(id, rows).await;
    }

    /// What waits on the lines urgently, after a restart: read once the
    /// sessions that still run were taken back. The plugin and the pump
    /// leave each row for its session's running turn (it is kept as urgent);
    /// an escalating stand-down to a session in a turn is timed again now,
    /// as a session inside a long tool says nothing until it returns; the
    /// rest are steered again from each session's first tool call heard
    /// ([`Self::steer_heard`]).
    pub(super) async fn rush_load(&self) {
        // A suggestion to interrupt from before is bound to a run that is
        // over: withdrawn, so a yes to it interrupts nothing, and the
        // escalation timed again below suggests it once, for this run.
        match self.0.store.suggestions_stand_down_dismiss().await {
            Ok(gone) => {
                for g in gone {
                    let mut by = By::desk("Desk restarted: its turn is over.");
                    by.suggestion_id = Some(g.id.clone());
                    self.note(by.event(
                        Kind::Dismissed,
                        g.subject.clone(),
                        format!("withdrew the suggestion: {}", g.text),
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't withdraw the suggestions to interrupt a restart left");
            }
        }
        let rows = match self.0.store.line_urgent_queued().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read the urgent line messages a restart left queued");
                return;
            }
        };
        for m in rows {
            let (Some(to), Some(u)) = (m.to_session(), m.urgency) else {
                continue;
            };
            let mark = self.turn_mark(to);
            let armed = u.escalates() && mark.is_some();
            if let (true, Some(mark)) = (armed, mark) {
                tracing::info!(session = %to, row = %m.id, "an escalating stand-down a restart left queued is timed again");
                self.rush_arm(to, m.id, mark);
            }
            self.0
                .rush
                .unarmed
                .lock()
                .entry(to)
                .or_default()
                .push(Unarmed { row: m.id, armed });
        }
    }

    /// `id` is gone: nothing a restart left for it is steered again.
    pub(super) fn rush_forget(&self, id: Uuid) {
        self.0.rush.unarmed.lock().remove(&id);
    }

    /// Steers `rows` (queued urgently before a restart) into `id`'s running
    /// turn again, when it still works and can take them.
    async fn rush_rearm(&self, id: Uuid, rows: Vec<Unarmed>) {
        for Unarmed { row, armed } in rows {
            let Ok(view) = self.view(id) else { return };
            let Ok(Some(mut m)) = self.0.store.line_message(row).await else {
                continue;
            };
            if m.delivery != line::Delivery::Queued {
                continue;
            }
            let by = By {
                party: m.from.clone(),
                reason: None,
                suggestion_id: None,
                hop: m.hop,
                preference: None,
            };
            let (engine, dialog, mark) = {
                let st = self.0.state.lock();
                (
                    st.records.get(&id).map(|r| r.engine),
                    st.live.get(&id).is_some_and(|l| l.pending.is_some()),
                    Self::mark_in(&st, id),
                )
            };
            let how = engine.map(|e| line::mid_turn(e, view.state, true, dialog));
            let rushed = match how {
                Some(Ok(Some(MidTurn::Hook))) => Rushed::Steered,
                Some(Ok(Some(MidTurn::TurnSteer))) => Rushed::SaidIn,
                // Timed already if it escalates; the turn's end takes it.
                Some(Err(_)) => continue,
                _ => continue,
            };
            if self.line_gate(&view, &m).is_some() {
                continue;
            }
            tracing::info!(session = %id, %row, "an urgent line message a restart left queued is steered again");
            let mark = if armed { None } else { mark };
            if let Err(e) = self.rush_deliver(&mut m, id, rushed, mark, &by).await {
                tracing::warn!(session = %id, %row, error = %e, "couldn't steer an urgent line message again");
            }
        }
    }

    /// In the steer wait, looks again whether `id` read the escalating
    /// stand-down `row` in the turn `mark` it was sent in.
    fn rush_arm(&self, id: Uuid, row: Uuid, mark: TurnMark) {
        let desk = self.clone();
        let wait = self.steer_wait();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            desk.rush_overdue(id, row, mark).await;
        });
    }

    /// Whether `row` (to `id`) is still unread in turn `mark`: that turn
    /// runs, the row is still queued or handed to Desk's plugin, and no
    /// hook is taking it now. What an escalation, and a suggestion to
    /// interrupt, hold on. A handed row is unread: the plugin holds it for
    /// the session's idle, and may drop it back to the queue
    /// ([`Self::row_unread`], as [`Self::steer_settled`] reads it too). A
    /// row a hook is taking is the one difference from that: not unread
    /// here, so nothing interrupts the call reading it, and not settled
    /// there, as the take may put it back.
    pub async fn steer_unread(&self, id: Uuid, mark: TurnMark, row: Uuid) -> bool {
        let unread = self.row_unread(row).await;
        let st = self.0.state.lock();
        unread
            && Self::mark_in(&st, id) == Some(mark)
            && st
                .live
                .get(&id)
                .is_some_and(|l| !l.breaker.taking.contains(&row))
    }

    /// The escalating stand-down `row` wasn't read by `id` in the turn it
    /// was sent in (`mark`): its boss interrupts that turn, through its own
    /// gate, and it goes in at the idle that follows. Not while the boss's
    /// words are held from it (the person drives it, its breaker paused it,
    /// its mode is ending): those are theirs.
    async fn rush_overdue(&self, id: Uuid, row: Uuid, mark: TurnMark) {
        if !self.steer_unread(id, mark, row).await {
            return;
        }
        let (Ok(Some(m)), Ok(view)) = (self.0.store.line_message(row).await, self.view(id)) else {
            return;
        };
        if m.from.kind != PartyKind::Manager {
            return;
        }
        if let Some(wait) = self.line_gate(&view, &m) {
            tracing::info!(session = %id, %row, ?wait, "a stand-down wasn't read in time, but the boss's words are held from the session: no interrupt");
            return;
        }
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let Some(crew) = crew else {
            tracing::warn!(session = %id, %row, "a stand-down wasn't read in time and no boss is here to interrupt its turn");
            return;
        };
        match crew.steer_overdue(&m.from.id, id, row, mark).await {
            Ok(done) => {
                tracing::info!(session = %id, %row, %done, "a stand-down wasn't read in time: its turn was interrupted");
            }
            Err(why) => {
                tracing::info!(session = %id, %row, %why, "a stand-down wasn't read in time, and its turn wasn't interrupted");
                let wait = self.steer_wait().as_secs();
                self.note(Event::new(
                    Kind::Steered,
                    m.from.clone(),
                    Some(Party::session(&view)),
                    format!(
                        "{} hasn't read its stand-down in {wait}s, and it wasn't interrupted: {why}. It gets it once its turn ends",
                        view.label()
                    ),
                ));
            }
        }
    }

    /// Whether a suggestion to interrupt `id`'s turn `mark` for its
    /// stand-down `row` is over: that turn ended, or the row went in (taken
    /// and delivered, or withdrawn). Unread as [`Self::steer_unread`] reads
    /// it ([`Self::row_unread`]): a row handed to Desk's plugin isn't in
    /// the session yet and may come back to the queue, so the person's yes
    /// would still interrupt for it, and the suggestion stands. A row a hook
    /// is taking isn't over either, though it isn't unread there: the take
    /// may put it back.
    pub async fn steer_settled(&self, id: Uuid, mark: TurnMark, row: Uuid) -> bool {
        self.turn_mark(id) != Some(mark) || !self.row_unread(row).await
    }

    /// Interrupts `id`'s turn `mark` as [`Self::interrupt`] does, for its
    /// unread escalating stand-down `row`, and only then: refused when that
    /// turn is over, or the row was read or is being read. No hook takes
    /// the row while the interrupt goes in; it goes in at the idle after.
    pub async fn interrupt_turn(
        &self,
        id: Uuid,
        mark: TurnMark,
        row: Uuid,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        let moved = || DeskError::Conflict("it read its stand-down, or that turn is over".into());
        // Held from hooks first, then read: a hook that takes it after this
        // finds it held, and one that took it before shows in the row.
        {
            let mut st = self.0.state.lock();
            if Self::mark_in(&st, id) != Some(mark) {
                return Err(moved());
            }
            let l = st.live.get_mut(&id).ok_or_else(moved)?;
            if l.breaker.taking.contains(&row) {
                return Err(moved());
            }
            l.breaker.escalating.insert(row);
        }
        let done = match self.row_unread(row).await {
            true => self.interrupt_when(id, by, Some(mark)),
            false => Err(moved()),
        };
        if let Some(l) = self.0.state.lock().live.get_mut(&id) {
            l.breaker.escalating.remove(&row);
        }
        done
    }

    /// Whether `row` is still queued, or handed to Desk's plugin unread.
    async fn row_unread(&self, row: Uuid) -> bool {
        matches!(
            self.0.store.line_message(row).await,
            Ok(Some(m)) if matches!(m.delivery, line::Delivery::Queued | line::Delivery::Handed)
        )
    }
}

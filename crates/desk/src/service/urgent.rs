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
//!   other path finds it moved. While the session works, the pump and the
//!   plugin leave a row the queue holds for the hook alone; once it doesn't
//!   (its turn ended, it was interrupted), the row goes out at idle.
//! - A session that can't take words mid-turn gets them at its turn's end,
//!   and the sender is told so.
//! - A stand-down that escalates, not read within the steer wait, has its
//!   turn interrupted through the boss's own gate ([`crate::crew::Crew::steer_overdue`]).
//! - At most [`line::URGENT_PER_SESSION`] urgent orders per session in
//!   [`line::URGENT_WINDOW`]; stand-downs aren't counted.
//! - After a restart, what was queued urgently is steered again from the
//!   session's first tool call heard.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use super::breaker::Steer;
use super::line::LineError;
use super::Desk;
use crate::crew::{By, Event, Kind, Party, PartyKind};
use crate::engine::Engine;
use crate::line::{self, LineMessage, MidTurn, Rushed, Urgency};
use crate::model::{SessionState, SessionView};

/// What the desk keeps for urgent delivery.
pub(crate) struct Rush {
    /// How long an escalating stand-down waits to be read, in ms.
    wait_ms: AtomicU64,
    /// Held from counting a session's urgent orders until the new one is
    /// kept, so two sent at once can't both pass the limit.
    write: tokio::sync::Mutex<()>,
    /// What a restart found queued urgently, by session: steered again once
    /// the session's first tool call is heard in this run.
    unarmed: parking_lot::Mutex<HashMap<Uuid, Vec<(Uuid, Urgency)>>>,
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

    /// How a boss's urgent `m` would reach `v` now, decided before `m` is
    /// kept: the limit is counted under a lock the caller holds (the guard
    /// returned) until `m` is written, and `m` carries its urgency only
    /// when it is to go into the running turn, or wait for its end as
    /// urgent.
    pub(super) async fn rush_plan(
        &self,
        v: &SessionView,
        m: &mut LineMessage,
        urgency: Urgency,
    ) -> Result<(Rushed, Option<tokio::sync::MutexGuard<'_, ()>>), LineError> {
        let (engine, heard, dialog, link) = {
            let st = self.0.state.lock();
            let engine = st.records.get(&v.id).map(|r| r.engine);
            match st.live.get(&v.id) {
                Some(l) => (
                    engine,
                    l.breaker.heard,
                    l.pending.is_some(),
                    l._codex.is_some(),
                ),
                None => (engine, false, false, false),
            }
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
            return Ok((Rushed::NotWorking, None));
        }
        // What holds the boss's words from it at idle holds them now too.
        if let Some(wait) = self.line_gate(v, m) {
            return Ok((Rushed::Held(wait), None));
        }
        let guard = match urgency.counts() {
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
                    return Ok((Rushed::TooMany, None));
                }
                Some(guard)
            }
            false => None,
        };
        m.urgency = Some(urgency);
        let rushed = match how {
            Ok(Some(MidTurn::Hook)) => Rushed::Steered,
            Ok(Some(MidTurn::TurnSteer)) => Rushed::SaidIn,
            Err(why) => Rushed::CantSteer(why),
            Ok(None) => Rushed::NotWorking,
        };
        Ok((rushed, guard))
    }

    /// Sends the kept urgent `m` on to session `id` as `rushed` planned, as
    /// `by`. What can't go into the running turn goes as any message.
    pub(super) async fn rush_deliver(
        &self,
        m: &mut LineMessage,
        id: Uuid,
        rushed: Rushed,
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
        if urgency.is_some_and(Urgency::escalates)
            && matches!(rushed, Rushed::Steered | Rushed::CantSteer(_))
        {
            self.rush_arm(id, m.id);
        }
        Ok(())
    }

    /// Says a boss's `m` into Codex session `id`'s running turn, marked so
    /// it can be answered: delivered as it is said, and back in the queue if
    /// Codex refuses it.
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

    /// Whether `row` waits for `id`'s next tool call: the pump and the
    /// plugin leave it while the session works.
    pub(super) fn steer_reserved(&self, id: Uuid, row: Uuid) -> bool {
        let st = self.0.state.lock();
        let working = st.records.get(&id).map(|r| r.state) == Some(SessionState::Working);
        working
            && st.live.get(&id).is_some_and(|l| {
                l.breaker
                    .steers
                    .iter()
                    .any(|s| matches!(s, Steer::Line { row: r } if *r == row))
            })
    }

    /// What goes with `id`'s tool call result (hook `event`): the breaker's
    /// warning, and, on the main thread's call (`main`), the boss's urgent
    /// messages still queued, each taken as it is delivered. None when
    /// nothing is.
    pub(super) async fn steers_take(&self, id: Uuid, event: &str, main: bool) -> Option<Value> {
        let steers = {
            let mut st = self.0.state.lock();
            let l = st.live.get_mut(&id)?;
            Desk::take_steers(l, main)
        };
        let mut said = Vec::new();
        for s in steers {
            match s {
                Steer::Breaker(_, text) => said.push(text),
                Steer::Line { row } => match self.rush_take(id, row).await {
                    Ok(Some(text)) => said.push(text),
                    Ok(None) => {}
                    Err(e) => {
                        // Left queued: it goes in at the idle.
                        tracing::warn!(session = %id, %row, error = %e, "an urgent line message couldn't be taken into a turn");
                    }
                },
            }
        }
        (!said.is_empty()).then(|| crate::claude::context(event, &said.join("\n\n")))
    }

    /// Takes the boss's `row` into `id`'s turn: its words as the session
    /// reads them, or none when it went in another way, or something holds
    /// it now (it waits at the idle).
    async fn rush_take(&self, id: Uuid, row: Uuid) -> Result<Option<String>, LineError> {
        let Some(m) = self.0.store.line_message(row).await? else {
            return Ok(None);
        };
        let view = self.view(id)?;
        if !m.waits()
            || self.waited_on(&m)
            || self.line_gate(&view, &m).is_some()
            || self.line_overtaken(&m).await
        {
            return Ok(None);
        }
        let Some(taken) = self
            .0
            .store
            .line_take(row, line::Delivery::Delivered)
            .await?
        else {
            return Ok(None);
        };
        self.line_delivered_into(id, &taken);
        tracing::info!(session = %id, %row, "an urgent line message was read mid-turn");
        Ok(Some(self.rendered(&taken).await))
    }

    /// `id`'s boss messages waiting for its next tool call no longer do (its
    /// turn ended, or was interrupted): they go in at the idle, and Desk's
    /// plugin, if it waits on the line, hears they are there.
    pub(super) fn steers_released(&self, id: Uuid) {
        let rows: Vec<Uuid> = {
            let mut st = self.0.state.lock();
            let Some(l) = st.live.get_mut(&id) else {
                return;
            };
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
        if rows.is_empty() {
            return;
        }
        let desk = self.clone();
        tokio::spawn(async move {
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
    /// urgently for it.
    pub(super) fn steer_heard(&self, id: Uuid) {
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
        let desk = self.clone();
        tokio::spawn(async move { desk.rush_rearm(id, rows).await });
    }

    /// What waits on the lines urgently, after a restart: read once the
    /// sessions that still run were taken back, and steered again from
    /// each one's first tool call heard ([`Self::steer_heard`]).
    pub(super) async fn rush_load(&self) {
        let rows = match self.0.store.line_urgent_queued().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read the urgent line messages a restart left queued");
                return;
            }
        };
        let mut unarmed = self.0.rush.unarmed.lock();
        for m in rows {
            if let (Some(to), Some(u)) = (m.to_session(), m.urgency) {
                unarmed.entry(to).or_default().push((m.id, u));
            }
        }
    }

    /// Steers `rows` (queued urgently before a restart) into `id`'s running
    /// turn again, when it still works and can take them.
    async fn rush_rearm(&self, id: Uuid, rows: Vec<(Uuid, Urgency)>) {
        for (row, urgency) in rows {
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
            let (engine, dialog) = {
                let st = self.0.state.lock();
                (
                    st.records.get(&id).map(|r| r.engine),
                    st.live.get(&id).is_some_and(|l| l.pending.is_some()),
                )
            };
            let how = engine.map(|e| line::mid_turn(e, view.state, true, dialog));
            let rushed = match how {
                Some(Ok(Some(MidTurn::Hook))) => Rushed::Steered,
                Some(Ok(Some(MidTurn::TurnSteer))) => Rushed::SaidIn,
                Some(Err(why)) => Rushed::CantSteer(why),
                _ => continue,
            };
            if self.line_gate(&view, &m).is_some() {
                continue;
            }
            tracing::info!(session = %id, %row, ?urgency, "an urgent line message a restart left queued is steered again");
            if let Err(e) = self.rush_deliver(&mut m, id, rushed, &by).await {
                tracing::warn!(session = %id, %row, error = %e, "couldn't steer an urgent line message again");
            }
        }
    }

    /// In the steer wait, looks again whether `id` read the escalating
    /// stand-down `row`.
    fn rush_arm(&self, id: Uuid, row: Uuid) {
        let at = {
            let st = self.0.state.lock();
            st.live.get(&id).map(|l| (l.ends, l.breaker.turns))
        };
        let Some(at) = at else { return };
        let desk = self.clone();
        let wait = self.steer_wait();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            desk.rush_overdue(id, row, at).await;
        });
    }

    /// The escalating stand-down `row` wasn't read by `id` in the turn it
    /// was sent in (`at`: its turn counts then): its boss interrupts that
    /// turn, through its own gate, and it goes in at the idle that follows.
    async fn rush_overdue(&self, id: Uuid, row: Uuid, at: (u64, u64)) {
        let same_turn = {
            let st = self.0.state.lock();
            let working = st.records.get(&id).is_some_and(|r| {
                matches!(
                    r.state,
                    SessionState::Working | SessionState::Blocked | SessionState::Asking
                )
            });
            working
                && st
                    .live
                    .get(&id)
                    .is_some_and(|l| (l.ends, l.breaker.turns) == at)
        };
        if !same_turn {
            return;
        }
        let m = match self.0.store.line_message(row).await {
            Ok(Some(m)) => m,
            _ => return,
        };
        // Read, withdrawn, or handed on: nothing to do.
        if !matches!(m.delivery, line::Delivery::Queued | line::Delivery::Handed)
            || m.from.kind != PartyKind::Manager
        {
            return;
        }
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let name = self
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_default();
        let wait = self.steer_wait().as_secs();
        let Some(crew) = crew else {
            tracing::warn!(session = %id, %row, "a stand-down wasn't read in time and no boss is here to interrupt its turn");
            return;
        };
        match crew.steer_overdue(&m.from.id, id, row).await {
            Ok(done) => {
                tracing::info!(session = %id, %row, %done, "a stand-down wasn't read in time: its turn was interrupted");
                self.steers_released(id);
            }
            Err(why) => {
                tracing::info!(session = %id, %row, %why, "a stand-down wasn't read in time, and its turn wasn't interrupted");
                let to = self.view(id).ok().map(|v| Party::session(&v));
                self.note(Event::new(
                    Kind::Steered,
                    m.from.clone(),
                    to,
                    format!(
                        "{name} hasn't read its stand-down in {wait}s, and it wasn't interrupted: {why}. It gets it once its turn ends"
                    ),
                ));
            }
        }
    }
}

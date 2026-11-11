//! A session's permission mode, as a boss sets it ([`crate::mode`]): kept
//! with Desk, started in, and read back from what Claude Code says.
//!
//! A mode that still asks before it runs a command (manual, accept edits,
//! plan) is kept until it is changed. One that doesn't (auto, bypass) is an
//! unbound boss's alone, on a session it runs, and lasts only as long as
//! both hold: at any later start it is dropped, and the session starts in
//! its own mode, unless the boss that set it is still unbound and still
//! runs it. The person resuming it themselves drops it too.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::crew::{By, Kind, Party, PartyKind};
use crate::mode::Mode;
use crate::model::{Engine, SessionView};

/// Where the modes Desk keeps for sessions are, by session id.
const MODES: &str = "hub.desk.modes";

/// How long Desk waits for Claude Code to say the mode it started in.
const READ_BACK: Duration = Duration::from_secs(15);

/// How often Desk looks again for a session to put back in its own mode
/// while one is mid-turn.
const RESTORE_EVERY: Duration = Duration::from_secs(1);

/// How long a session gets to stop before Desk starts it again.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// A mode Desk keeps for a session, and the boss that set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    mode: Mode,
    boss: String,
}

/// A mode set: the session as it is now, whether it was running, the mode
/// its engine says it runs in when it said so in time (Claude Code only),
/// and why it didn't start again, when it didn't.
#[derive(Debug, Clone)]
pub struct ModeSet {
    pub view: SessionView,
    pub running: bool,
    pub confirmed: Option<Mode>,
    pub not_started: Option<String>,
}

impl Desk {
    async fn kept_modes(&self) -> HashMap<Uuid, Kept> {
        match self.0.store.state(MODES).await {
            Ok(entry) => entry
                .and_then(|e| serde_json::from_value(e.value).ok())
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "reading the sessions' modes failed");
                HashMap::new()
            }
        }
    }

    /// Reads the kept modes, changes them with `f`, and keeps what it made
    /// of them, as one step against every other change of them.
    async fn edit_modes<R>(
        &self,
        f: impl FnOnce(&mut HashMap<Uuid, Kept>) -> (R, bool),
    ) -> Result<R, DeskError> {
        let _one = self.0.modes_write.lock().await;
        let mut modes = self.kept_modes().await;
        let (out, changed) = f(&mut modes);
        if changed {
            self.keep_modes(&modes).await?;
        }
        Ok(out)
    }

    async fn keep_modes(&self, modes: &HashMap<Uuid, Kept>) -> Result<(), DeskError> {
        let value = serde_json::to_value(modes).map_err(|e| DeskError::Internal(e.into()))?;
        self.0
            .store
            .put_state(MODES, &value)
            .await
            .map(drop)
            .map_err(DeskError::Internal)
    }

    /// Whether `boss` may keep a session in a mode that doesn't ask: it is
    /// unbound and runs `id`.
    fn unbound_runs(&self, id: Uuid, boss: &str) -> bool {
        // A boss served from a grant is never unbound.
        !crate::grant::is_served(boss)
            && self.full_of(boss).unbound
            && self
                .charge_of(id)
                .is_some_and(|c| c.live() && c.boss == boss)
    }

    /// The mode `id` starts in now, if Desk keeps one for it: one that
    /// doesn't ask is dropped (and the session starts in its own) unless
    /// the boss that set it is still unbound and still runs it.
    pub(crate) async fn mode_to_start(&self, id: Uuid) -> Option<Mode> {
        let decided = self
            .edit_modes(|modes| {
                let Some(kept) = modes.get(&id).cloned() else {
                    return (None, false);
                };
                if kept.mode.asks() || self.unbound_runs(id, &kept.boss) {
                    return (Some(kept.mode), false);
                }
                tracing::info!(%id, mode = kept.mode.as_str(), "its boss is no longer unbound over it, so it starts in its own mode");
                modes.remove(&id);
                (None, true)
            })
            .await;
        decided.unwrap_or_else(|e| {
            tracing::warn!(%id, error = %e, "dropping a session's kept mode failed; it starts in its own mode");
            None
        })
    }

    /// Whether boss `boss` may act on `id` now, as far as its mode goes:
    /// while it runs in a mode that doesn't ask, only an unbound boss that
    /// runs it may, until Desk has put it back in its own mode.
    pub fn boss_may_drive(&self, id: Uuid, boss: &str) -> Result<(), String> {
        let Some(mode) = self.live_mode(id) else {
            return Ok(());
        };
        if mode.asks() || self.unbound_runs(id, boss) {
            return Ok(());
        }
        Err(format!(
            "it still runs in {} mode, which only an unbound boss that runs it may drive; Desk puts it back in its own mode once its turn ends, and it's the person's until then",
            mode.title()
        ))
    }

    /// The mode that doesn't ask `id` runs in now, as a boss kept it.
    fn live_mode(&self, id: Uuid) -> Option<Mode> {
        let st = self.0.state.lock();
        st.modes_started
            .get(&id)
            .copied()
            .filter(|m| !m.asks() && st.live.contains_key(&id))
    }

    /// Whether `id` runs in a mode that doesn't ask while the boss that
    /// runs it (if any) isn't unbound over it: it is on its way back to its
    /// own mode, and only the person drives it until then.
    pub(crate) fn mode_ending(&self, id: Uuid) -> bool {
        self.live_mode(id).is_some()
            && !self
                .charge_of(id)
                .filter(|c| c.live())
                .is_some_and(|c| self.unbound_runs(id, &c.boss))
    }

    /// [`Self::boss_may_drive`] for `by`, when it is a boss.
    pub(crate) fn mode_lets(&self, id: Uuid, by: Option<&By>) -> Result<(), DeskError> {
        match by.filter(|b| b.party.kind == PartyKind::Manager) {
            Some(b) => self
                .boss_may_drive(id, &b.party.id)
                .map_err(DeskError::Conflict),
            None => Ok(()),
        }
    }

    /// Something that decides whose modes hold changed (a boss's grant, a
    /// charge, a turn's end): the sessions it affects go back to their own
    /// modes. Called from wherever, it runs on the runtime when there is
    /// one.
    pub(crate) fn modes_changed(&self) {
        self.0
            .modes_dirty
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                "no runtime to put sessions back in their own modes on; the next change does"
            );
            return;
        };
        let d = self.clone();
        rt.spawn(async move { d.restore_modes().await });
    }

    /// Drops each kept mode that doesn't ask whose boss is no longer
    /// unbound over its session, and starts each running session still in
    /// one again in its own mode: at once when it is between turns, else
    /// once its turn ends.
    async fn restore_modes(&self) {
        // One at a time: the one running sees what is marked dirty before
        // it ends.
        let Ok(_one) = self.0.modes_restoring.try_lock() else {
            return;
        };
        use crate::model::SessionState as S;
        use std::sync::atomic::Ordering;
        loop {
            self.0.modes_dirty.store(false, Ordering::SeqCst);
            let dropped = self
                .edit_modes(|modes| {
                    let before = modes.len();
                    modes.retain(|id, k| k.mode.asks() || self.unbound_runs(*id, &k.boss));
                    ((), modes.len() != before)
                })
                .await;
            if let Err(e) = dropped {
                tracing::warn!(error = %e, "dropping sessions' kept modes failed");
            }
            let kept = self.kept_modes().await;
            let running: Vec<(Uuid, Mode)> = {
                let mut st = self.0.state.lock();
                // Stopped with its mode gone: nothing is left to restore.
                let live: std::collections::HashSet<Uuid> = st.live.keys().copied().collect();
                st.modes_started
                    .retain(|id, _| live.contains(id) || kept.contains_key(id));
                st.modes_started
                    .iter()
                    .filter(|(id, m)| !m.asks() && live.contains(id))
                    .filter(|(id, _)| !kept.contains_key(id))
                    .map(|(id, m)| (*id, *m))
                    .collect()
            };
            if running.is_empty() {
                if self.0.modes_dirty.load(Ordering::SeqCst) {
                    continue;
                }
                return;
            }
            let mut waiting = false;
            for (id, mode) in running {
                let Ok(view) = self.view(id) else { continue };
                // Mid-turn, waiting on an answer, or the person typing into
                // it: once that is over.
                if view.pending.is_some()
                    || matches!(view.state, S::Working | S::Blocked | S::Asking)
                    || self.person_driving(id)
                {
                    waiting = true;
                    continue;
                }
                match self.start_again(id).await {
                    Ok(v) => self.note(By::desk("Its boss is no longer unbound over it.").event(
                        Kind::Restarted,
                        Some(Party::session(&v)),
                        format!(
                            "put {} back in its own mode from {} mode (started again in place; its conversation is kept)",
                            v.label(),
                            mode.title()
                        ),
                    )),
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "a session didn't go back to its own mode; trying again");
                        waiting = true;
                    }
                }
            }
            if waiting {
                tokio::time::sleep(RESTORE_EVERY).await;
            }
        }
    }

    /// Stops `id` and starts it again in place, resumed by id: within
    /// [`STOP_WITHIN`], or not at all.
    async fn start_again(&self, id: Uuid) -> Result<SessionView, DeskError> {
        self.0.state.lock().modes_seen.remove(&id);
        self.0.state.lock().acted.insert(id);
        self.stop(id, None)?;
        let until = tokio::time::Instant::now() + STOP_WITHIN;
        while self.0.state.lock().live.contains_key(&id) {
            if tokio::time::Instant::now() >= until {
                return Err(DeskError::Conflict(format!(
                    "it didn't stop within {} s",
                    STOP_WITHIN.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        self.0.state.lock().acted.insert(id);
        self.resume_checked(id, None, None).await
    }

    /// The person resumes `id` themselves: a mode that doesn't ask, which a
    /// boss kept it in, is dropped.
    pub(crate) async fn person_resumes(&self, id: Uuid, by: Option<&By>) {
        if !by.is_some_and(|b| b.party.kind == PartyKind::You) {
            return;
        }
        let dropped = self
            .edit_modes(
                |modes| match modes.get(&id).is_some_and(|k| !k.mode.asks()) {
                    true => ((), modes.remove(&id).is_some()),
                    false => ((), false),
                },
            )
            .await;
        if let Err(e) = dropped {
            tracing::warn!(%id, error = %e, "dropping a session's kept mode failed");
        }
    }

    /// The mode `id` last said it runs in (Claude Code says it on each
    /// hook).
    pub fn mode_seen(&self, id: Uuid) -> Option<Mode> {
        self.0.state.lock().modes_seen.get(&id).copied()
    }

    /// Puts `id` in `mode`, as `by`: kept, and, while it runs, started
    /// again in place in it (resumed by id, its conversation kept), since
    /// neither engine takes a new mode reliably while it runs. Claude Code's
    /// own word on the mode it started in is waited for, a while. Only
    /// between turns; and a mode that doesn't ask only by an unbound boss
    /// that runs it.
    pub async fn set_mode(&self, id: Uuid, mode: Mode, by: By) -> Result<ModeSet, DeskError> {
        let r = self.record(id)?;
        if !matches!(r.engine, Engine::Claude | Engine::Codex) {
            return Err(DeskError::Invalid(format!(
                "Desk can't set {}'s mode",
                r.engine.title()
            )));
        }
        let view = self.view(id)?;
        if view.terminal_id.is_some() {
            return Err(DeskError::Invalid(
                "it runs in the person's own Desk terminal, so Desk can't start it again in another mode"
                    .into(),
            ));
        }
        if !mode.asks() {
            let boss = (by.party.kind == PartyKind::Manager).then_some(by.party.id.as_str());
            if !boss.is_some_and(|b| self.full_of(b).unbound) {
                return Err(DeskError::Invalid(format!(
                    "only an unbound boss puts a session in {} mode",
                    mode.title()
                )));
            }
            if !boss.is_some_and(|b| self.unbound_runs(id, b)) {
                return Err(DeskError::Invalid(format!(
                    "take it on first: {} mode lasts only while you run it, unbound",
                    mode.title()
                )));
            }
        }
        // Started again mid-turn, its turn and what it asks would be lost.
        use crate::model::SessionState as S;
        if view.pending.is_some() || matches!(view.state, S::Working | S::Blocked | S::Asking) {
            return Err(DeskError::Conflict(
                "it's working or waiting on an answer: set its mode between turns, once it has finished"
                    .into(),
            ));
        }
        self.boss_holds(id, Some(&by))?;
        self.mode_lets(id, Some(&by))?;
        self.breaker_refuses_prompt(id, Some(&by))?;
        let kept = Kept {
            mode,
            boss: by.party.id.clone(),
        };
        self.edit_modes(|modes| ((), modes.insert(id, kept.clone()) != Some(kept)))
            .await?;
        let running = self.0.state.lock().live.contains_key(&id);
        let mut not_started = None;
        let view = if running {
            match self.start_again(id).await {
                Ok(v) => v,
                Err(DeskError::Conflict(why)) if self.0.state.lock().live.contains_key(&id) => {
                    return Err(DeskError::Conflict(format!(
                        "{why}, so Desk didn't start it again over it; it starts in {} mode once it has",
                        mode.title()
                    )));
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "a session didn't start again in its new mode");
                    not_started = Some(e.to_string());
                    self.view(id)?
                }
            }
        } else {
            view
        };
        let confirmed = match (running && not_started.is_none(), r.engine) {
            (true, Engine::Claude) => {
                let until = tokio::time::Instant::now() + READ_BACK;
                loop {
                    if let Some(m) = self.mode_seen(id) {
                        break Some(m);
                    }
                    if tokio::time::Instant::now() >= until {
                        break None;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            _ => None,
        };
        let how = match (running, &not_started) {
            (true, None) => " (started again in place; its conversation is kept)",
            (true, Some(_)) => " (stopped; it didn't start again, and resumes in it)",
            (false, _) => " (from its next start)",
        };
        self.note(by.event(
            Kind::Restarted,
            Some(Party::session(&view)),
            format!("put {} in {} mode{how}", view.label(), mode.title()),
        ));
        Ok(ModeSet {
            view,
            running,
            confirmed,
            not_started,
        })
    }
}

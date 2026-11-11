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
        self.full_of(boss).unbound
            && self
                .charge_of(id)
                .is_some_and(|c| c.live() && c.boss == boss)
    }

    /// The mode `id` starts in now, if Desk keeps one for it: one that
    /// doesn't ask is dropped (and the session starts in its own) unless
    /// the boss that set it is still unbound and still runs it.
    pub(crate) async fn mode_to_start(&self, id: Uuid) -> Option<Mode> {
        let mut modes = self.kept_modes().await;
        let kept = modes.get(&id)?.clone();
        if kept.mode.asks() || self.unbound_runs(id, &kept.boss) {
            return Some(kept.mode);
        }
        tracing::info!(%id, mode = kept.mode.as_str(), "its boss is no longer unbound over it, so it starts in its own mode");
        modes.remove(&id);
        if let Err(e) = self.keep_modes(&modes).await {
            tracing::warn!(%id, error = %e, "dropping a session's kept mode failed");
        }
        None
    }

    /// The person resumes `id` themselves: a mode that doesn't ask, which a
    /// boss kept it in, is dropped.
    pub(crate) async fn person_resumes(&self, id: Uuid, by: Option<&By>) {
        if !by.is_some_and(|b| b.party.kind == PartyKind::You) {
            return;
        }
        let mut modes = self.kept_modes().await;
        if modes.get(&id).is_some_and(|k| !k.mode.asks()) {
            modes.remove(&id);
            if let Err(e) = self.keep_modes(&modes).await {
                tracing::warn!(%id, error = %e, "dropping a session's kept mode failed");
            }
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
        self.breaker_refuses_prompt(id, Some(&by))?;
        let mut modes = self.kept_modes().await;
        modes.insert(
            id,
            Kept {
                mode,
                boss: by.party.id.clone(),
            },
        );
        self.keep_modes(&modes).await?;
        let running = self.0.state.lock().live.contains_key(&id);
        let mut not_started = None;
        let view = if running {
            self.0.state.lock().modes_seen.remove(&id);
            self.0.state.lock().acted.insert(id);
            self.stop(id, None)?;
            let until = tokio::time::Instant::now() + STOP_WITHIN;
            while self.0.state.lock().live.contains_key(&id) {
                if tokio::time::Instant::now() >= until {
                    return Err(DeskError::Conflict(format!(
                        "it didn't stop within {} s, so Desk didn't start it again over it; it starts in {} mode once it has",
                        STOP_WITHIN.as_secs(),
                        mode.title()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            self.0.state.lock().acted.insert(id);
            match self.resume_checked(id, None, None).await {
                Ok(v) => v,
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

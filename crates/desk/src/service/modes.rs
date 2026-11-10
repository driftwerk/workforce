//! A session's permission mode, as a boss sets it ([`crate::mode`]): kept
//! with Desk, started in, and read back from what Claude Code says.

use std::collections::HashMap;
use std::time::Duration;

use uuid::Uuid;

use super::{Desk, DeskError};
use crate::crew::{By, Kind, Party};
use crate::mode::Mode;
use crate::model::{Engine, SessionView};

/// Where the modes Desk keeps for sessions are, by session id.
const MODES: &str = "hub.desk.modes";

/// How long Desk waits for Claude Code to say the mode it started in.
const READ_BACK: Duration = Duration::from_secs(15);

/// A mode set: the session as it is now, and the mode its engine says it
/// runs in, when it said so in time (Claude Code only).
#[derive(Debug, Clone)]
pub struct ModeSet {
    pub view: SessionView,
    pub running: bool,
    pub confirmed: Option<Mode>,
}

impl Desk {
    /// The mode Desk keeps for `id`, if one was set.
    pub(crate) async fn kept_mode(&self, id: Uuid) -> Option<Mode> {
        self.kept_modes().await.get(&id).copied()
    }

    async fn kept_modes(&self) -> HashMap<Uuid, Mode> {
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

    /// The mode `id` last said it runs in (Claude Code says it on each
    /// hook).
    pub fn mode_seen(&self, id: Uuid) -> Option<Mode> {
        self.0.state.lock().modes_seen.get(&id).copied()
    }

    /// Puts `id` in `mode`, as `by`: kept, and, while it runs, restarted in
    /// place in it (resumed by id, its conversation kept), since neither
    /// engine takes a new mode reliably while it runs. Claude Code's own
    /// word on the mode it started in is waited for, a while.
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
        self.boss_holds(id, Some(&by))?;
        self.breaker_refuses_prompt(id, Some(&by))?;
        let mut modes = self.kept_modes().await;
        modes.insert(id, mode);
        let value = serde_json::to_value(&modes).map_err(|e| DeskError::Internal(e.into()))?;
        self.0
            .store
            .put_state(MODES, &value)
            .await
            .map_err(DeskError::Internal)?;
        let running = self.0.state.lock().live.contains_key(&id);
        let view = if running {
            self.0.state.lock().modes_seen.remove(&id);
            self.0.state.lock().acted.insert(id);
            self.stop(id, None)?;
            for _ in 0..250 {
                if !self.0.state.lock().live.contains_key(&id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            self.0.state.lock().acted.insert(id);
            self.resume_checked(id, None, None).await?
        } else {
            view
        };
        let confirmed = match (running, r.engine) {
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
        let how = match running {
            true => " (started again in place; its conversation is kept)",
            false => " (from its next start)",
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
        })
    }
}

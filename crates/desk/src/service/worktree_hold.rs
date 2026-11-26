//! Who holds a worktree: the session working in it, which no other start or
//! resume shares it with.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use super::{Desk, DeskError, State};
use crate::board::Ticket;
use crate::charge::{How, Wheel, HOLD};
use crate::crew::{By, Party};
use crate::model::{SessionRecord, SessionState};
use crate::worktree;

/// How long a start waits for its earlier session to have stopped.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// Whether `r` works in `folder` (canonical) or below it.
fn within(r: &SessionRecord, folder: &Path) -> bool {
    worktree::canonical(Path::new(&r.cwd)).starts_with(folder)
}

/// The worktree `path` is in or below: `<repo>/.worktrees/<slug>`.
fn root_of(path: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = path.components().collect();
    let at = parts.iter().position(|c| c.as_os_str() == ".worktrees")?;
    parts.get(at + 1)?;
    Some(parts[..=at + 1].iter().collect())
}

/// The worktree a session working in `cwd` holds, by the same rule for a
/// start and a resume: the Desk worktree its folder (links resolved) is
/// in, or, one made on a `branch` elsewhere, its folder.
pub(super) fn held_folder(cwd: &Path, branch: bool) -> Option<PathBuf> {
    let at = worktree::canonical(cwd);
    root_of(&at).or_else(|| branch.then_some(at))
}

/// What a start or resume in a worktree is checked against, under the lock
/// that records it: the worktree's folder (canonical), and the sessions
/// whose process the terminal keeper still runs there.
pub(super) struct Hold {
    folder: PathBuf,
    kept: HashSet<Uuid>,
}

impl Hold {
    /// Why `except` can't start or resume there: a session running there,
    /// starting, or kept running by the terminal keeper holds it.
    pub(super) fn refuses(&self, st: &State, except: Uuid) -> Option<DeskError> {
        st.records
            .values()
            .find(|r| {
                r.id != except
                    && (r.state.is_running()
                        || st.starting.contains(&r.id)
                        || self.kept.contains(&r.id))
                    && within(r, &self.folder)
            })
            .map(|h| DeskError::WorktreeTaken {
                why: format!(
                    "{} is in use: {} still runs in it",
                    self.folder.display(),
                    h.display_name.as_deref().unwrap_or(&h.name)
                ),
                holder: Some(h.id),
            })
    }
}

impl Desk {
    /// The hold on `folder`, a worktree ([`held_folder`]).
    pub(super) async fn worktree_hold(&self, folder: PathBuf) -> Hold {
        let kept = self.kept_in(&folder).await;
        Hold { folder, kept }
    }

    /// The interrupted sessions in `folder` (canonical) whose process the
    /// terminal keeper still runs, though Desk lost hold of it.
    async fn kept_in(&self, folder: &Path) -> HashSet<Uuid> {
        let interrupted: Vec<Uuid> = self
            .0
            .state
            .lock()
            .records
            .values()
            .filter(|r| r.state == SessionState::Interrupted && within(r, folder))
            .map(|r| r.id)
            .collect();
        let held =
            futures::future::join_all(interrupted.iter().map(|id| self.held_untaken(*id))).await;
        interrupted
            .into_iter()
            .zip(held)
            .filter_map(|(id, held)| held.then_some(id))
            .collect()
    }

    /// Frees `tree`, the worktree a plan's ticket `t` (`key`) starts in
    /// again, of the session an earlier start of it left resting there: one
    /// of that name its boss started and still runs, at the boss's wheel
    /// with nothing from the person for [`HOLD`], asking nothing, idle or
    /// done, in Desk and not in a terminal, no longer `t`'s (it was moved
    /// back to To do). Stopped, so this start takes its place rather than
    /// being refused; any other is left alone, and the start refused.
    pub(super) async fn free_resting(
        &self,
        tree: &Path,
        key: &str,
        t: &Ticket,
        boss: &Party,
    ) -> Result<(), DeskError> {
        let folder = worktree::canonical(tree);
        let now = Utc::now();
        let resting = {
            let st = self.0.state.lock();
            st.records
                .values()
                .filter(|r| {
                    r.name == key
                        && matches!(r.state, SessionState::Idle | SessionState::Done)
                        && r.asked.is_none()
                        && within(r, &folder)
                        && st.live.get(&r.id).is_some_and(|l| l.terminal.is_none())
                        && st
                            .charges
                            .driven
                            .get(&r.id)
                            .is_none_or(|at| now - *at >= HOLD)
                        && t.assignee.as_ref().is_none_or(|a| a.id != r.id.to_string())
                })
                .map(|r| r.id)
                .collect::<Vec<_>>()
        };
        let Some(id) = resting.into_iter().find(|id| {
            self.charge_of(*id).is_some_and(|c| {
                c.live()
                    && c.boss == boss.id
                    && c.how == How::Started
                    && c.wheel == Wheel::Boss
                    && c.wheel_since.is_none_or(|at| now - at >= HOLD)
            })
        }) else {
            return Ok(());
        };
        let mut events = self.events();
        let by = By::desk(&format!("{key} starts again in its worktree"));
        if let Err(e) = self.stop(id, Some(by)) {
            tracing::info!(session = %id, error = %e, "an earlier session in a worktree wasn't stopped for its start again");
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        let late = || {
            DeskError::Invalid(format!(
                "{key}'s earlier session in {} didn't stop in time",
                folder.display()
            ))
        };
        while self.record(id).is_ok_and(|r| r.state.is_running()) {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Err(_) | Ok(Err(RecvError::Closed)) => return Err(late()),
                Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "worktree_hold_tests.rs"]
mod tests;

//! Who holds a worktree: the session working in it, which no other start or
//! resume shares it with.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use uuid::Uuid;

use super::{Desk, DeskError, State};
use crate::crew::{By, Party};
use crate::model::{SessionRecord, SessionState};
use crate::worktree;

/// How long a start waits for its earlier session to have stopped.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// Whether `r` works in `folder` (canonical) or below it.
fn within(r: &SessionRecord, folder: &Path) -> bool {
    worktree::canonical(Path::new(&r.cwd)).starts_with(folder)
}

/// Whether `r` works in one of Desk's worktrees.
pub(super) fn in_worktree(r: &SessionRecord) -> bool {
    r.branch.is_some() || worktree::project_of(&r.cwd) != r.cwd
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
            .map(|h| {
                DeskError::WorktreeTaken(format!(
                    "{} is in use: {} still runs in it",
                    self.folder.display(),
                    h.display_name.as_deref().unwrap_or(&h.name)
                ))
            })
    }
}

impl Desk {
    /// The hold on `cwd`, a worktree, once `own`'s session there (its name
    /// and boss), resting from an earlier start of the same work, has
    /// stopped ([`Self::free_for`]).
    pub(super) async fn worktree_hold(&self, cwd: &Path, own: Option<(&str, &Party)>) -> Hold {
        let folder = worktree::canonical(cwd);
        if let Some((name, boss)) = own {
            self.free_for(&folder, name, boss).await;
        }
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

    /// Frees `folder` (canonical) of `boss`'s own earlier session of the
    /// same work (`name`) resting there, idle or done, in its charge still:
    /// stopped, so this start takes its place rather than being refused.
    async fn free_for(&self, folder: &Path, name: &str, boss: &Party) {
        let resting = {
            let st = self.0.state.lock();
            st.records
                .values()
                .find(|r| {
                    r.name == name
                        && matches!(r.state, SessionState::Idle | SessionState::Done)
                        && within(r, folder)
                        && st.live.get(&r.id).is_some_and(|l| l.terminal.is_none())
                })
                .map(|r| r.id)
        };
        let Some(id) = resting.filter(|id| {
            self.charge_of(*id)
                .is_some_and(|c| c.live() && c.boss == boss.id)
        }) else {
            return;
        };
        let by = By::desk(&format!("{name} starts again in its worktree"));
        if let Err(e) = self.stop(id, Some(by)) {
            tracing::info!(session = %id, error = %e, "an earlier session in a worktree wasn't stopped for its start again");
            return;
        }
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        while self.record(id).is_ok_and(|r| r.state.is_running())
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}

#[cfg(test)]
#[path = "worktree_hold_tests.rs"]
mod tests;

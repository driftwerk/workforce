//! A starting session's folder's dependencies ([`crate::deps`]): kept for
//! its card, installed once per folder in the background, and the session
//! told on its line when they're in. The folders still installing are kept,
//! so a Desk started again looks at each again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::transition::Transition;
use super::{Desk, State};
use crate::crew::By;
use crate::deps::{Deps, Found, Install};
use crate::model::SessionState;

/// The folders whose installs were running, and the sessions waiting on
/// each, as kept for the next start.
const INSTALLING: &str = "hub.sessions.deps";

/// A session's folder's dependencies as its card shows them, and whether a
/// turn has begun since they were said.
#[derive(Debug, Clone)]
pub(super) struct Kept {
    pub(super) deps: Deps,
    turned: bool,
}

impl Kept {
    fn new(deps: Deps) -> Self {
        Self {
            deps,
            turned: false,
        }
    }
}

/// What a folder's dependencies are, as Desk finds them in `dir`, read off
/// the runtime's threads; `vouched`: the start vouches for the folder, so
/// Desk may install there.
pub(super) async fn found(dir: PathBuf, vouched: bool) -> Option<Found> {
    let found = tokio::task::spawn_blocking(move || crate::deps::found(&dir))
        .await
        .unwrap_or_else(|e| {
            Some(Found::Left(format!(
                "Desk couldn't look at its package.json: {e}"
            )))
        });
    found.map(|f| match vouched {
        true => f,
        false => f.unvouched(),
    })
}

/// `dir` as one name for it, whichever path reached it.
pub(super) async fn canonical(dir: &Path) -> PathBuf {
    tokio::fs::canonicalize(dir)
        .await
        .unwrap_or_else(|_| dir.to_path_buf())
}

impl Desk {
    /// Keeps what Desk `found` of `id`'s folder `dir`; a stale install is
    /// joined while one runs into `dir`, else begun now.
    pub(super) fn deps_begin(&self, id: Uuid, dir: &Path, found: &Found) {
        let install = match found {
            Found::Stale(i) => Some(i.clone()),
            Found::Current(_) | Found::Left(_) => None,
        };
        let first = {
            let mut st = self.0.state.lock();
            let begun = match (&install, st.installing.get(dir).and_then(|w| w.first())) {
                (Some(_), Some(other)) => st.deps.get(other).map(|k| k.deps.clone()),
                _ => None,
            }
            .unwrap_or_else(|| Deps::begun(found));
            st.deps.insert(id, Kept::new(begun));
            install.filter(|_| {
                let waiting = st.installing.entry(dir.to_path_buf()).or_default();
                if !waiting.contains(&id) {
                    waiting.push(id);
                }
                waiting.len() == 1
            })
        };
        self.publish(id);
        if matches!(found, Found::Stale(_)) {
            self.deps_keep();
        }
        if let Some(i) = first {
            tokio::spawn(self.clone().deps_install(dir.to_path_buf(), i));
        }
    }

    /// Runs `i` in `dir`, then tells every session waiting on it how it
    /// ended.
    async fn deps_install(self, dir: PathBuf, i: Install) {
        let running = crate::deps::running_in(&self.0.opts.run_dir, &dir);
        let ended = match self.home_folder() {
            Some(home) => {
                let env = crate::repos::kept_env(self.login_env());
                crate::deps::install(&i, &dir, &home, &env, crate::deps::INSTALL_FOR, &running)
                    .await
            }
            None => Err("this machine has no home folder to install from".into()),
        };
        match &ended {
            Ok(()) => {
                tracing::info!(folder = %dir.display(), command = %i.command(), "a folder's dependencies were installed")
            }
            Err(why) => {
                tracing::warn!(folder = %dir.display(), command = %i.command(), %why, "a folder's dependencies weren't installed")
            }
        }
        let by = By::desk(match &ended {
            Ok(()) => "Desk installed the folder's dependencies the session started on.",
            Err(_) => "Desk couldn't install the folder's dependencies the session started on.",
        });
        let deps = Deps::ended(i.command(), ended);
        let waiting: Vec<Uuid> = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let known: Vec<Uuid> = (st.installing.remove(&dir).unwrap_or_default())
                .into_iter()
                .filter(|id| st.records.contains_key(id))
                .collect();
            for id in &known {
                st.deps.insert(*id, Kept::new(deps.clone()));
            }
            known
        };
        self.deps_keep();
        for id in waiting {
            self.publish(id);
            self.tell_session(id, deps.told(), &by).await;
        }
    }

    /// Keeps which folders install now, and for whom: written in turn, each
    /// write the state as it is then.
    pub(super) fn deps_keep(&self) {
        let desk = self.clone();
        let writes = Arc::clone(&self.0.state.lock().deps_writes);
        tokio::spawn(async move {
            let _turn = writes.lock().await;
            let installing: BTreeMap<String, Vec<Uuid>> = desk
                .0
                .state
                .lock()
                .installing
                .iter()
                .map(|(dir, ids)| (dir.to_string_lossy().into_owned(), ids.clone()))
                .collect();
            if let Err(e) = desk
                .0
                .store
                .put_state(INSTALLING, &serde_json::json!(installing))
                .await
            {
                tracing::warn!(error = %e, "couldn't keep which folders' dependencies Desk installs");
            }
        });
    }

    /// The folders a stopped Desk was installing into, looked at again for
    /// their sessions still open: each told what is there now, or the
    /// install begun again.
    pub(super) async fn deps_recover(&self) {
        let kept = match self.0.store.state(INSTALLING).await {
            Ok(kept) => kept,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read which folders' dependencies Desk was installing");
                None
            }
        };
        let kept: BTreeMap<String, Vec<Uuid>> = kept
            .and_then(|e| serde_json::from_value(e.value).ok())
            .unwrap_or_default();
        for (dir, ids) in kept {
            let open: Vec<Uuid> = {
                let st = self.0.state.lock();
                ids.into_iter()
                    .filter(|id| {
                        st.records
                            .get(id)
                            .is_some_and(|r| r.state != SessionState::Closed)
                    })
                    .collect()
            };
            if open.is_empty() {
                continue;
            }
            let dir = PathBuf::from(dir);
            let Some(found) = found(dir.clone(), true).await else {
                continue;
            };
            let by = By::desk("Desk looked at the folder's dependencies again as it started.");
            for id in open {
                self.deps_begin(id, &dir, &found);
                if !matches!(found, Found::Stale(_)) {
                    self.tell_session(id, Deps::begun(&found).told(), &by).await;
                }
            }
        }
        self.deps_keep();
    }

    /// Whether `id`'s failed or left dependencies, said, are read: a turn
    /// that began after they were said has ended (`turn_over`), and they
    /// leave its card.
    pub(super) fn deps_read(
        st: &mut State,
        id: Uuid,
        transitions: &[Transition<'_>],
        turn_over: bool,
    ) -> bool {
        let Some(kept) = st.deps.get_mut(&id).filter(|k| k.deps.said_only()) else {
            return false;
        };
        if kept.turned && turn_over {
            st.deps.remove(&id);
            return true;
        }
        kept.turned |= transitions.iter().any(|t| {
            matches!(
                t,
                Transition::Started | Transition::Prompted { .. } | Transition::Spoke { .. }
            )
        });
        false
    }
}

#[cfg(test)]
#[path = "deps_tests.rs"]
mod tests;

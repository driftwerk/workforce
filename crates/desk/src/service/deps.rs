//! A starting session's folder's dependencies ([`crate::deps`]): kept for
//! its card, installed once per folder in the background, and the session
//! told on its line when they're in.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::Desk;
use crate::crew::By;
use crate::deps::{Deps, Found, Install};

impl Desk {
    /// Keeps `begun` for `id`, whose folder `dir` Desk `found` so; a stale
    /// install is joined while one runs into `dir`, else begun now.
    pub(super) fn deps_begin(&self, id: Uuid, dir: &Path, found: Found, begun: Deps) {
        let install = match found {
            Found::Stale(i) => Some(i),
            Found::Current(_) | Found::Left(_) => None,
        };
        let first = {
            let mut st = self.0.state.lock();
            let begun = match (&install, st.installing.get(dir).and_then(|w| w.first())) {
                (Some(_), Some(other)) => st.deps.get(other).cloned().unwrap_or(begun),
                _ => begun,
            };
            st.deps.insert(id, begun);
            install.filter(|_| {
                let waiting = st.installing.entry(dir.to_path_buf()).or_default();
                waiting.push(id);
                waiting.len() == 1
            })
        };
        self.publish(id);
        if let Some(i) = first {
            tokio::spawn(self.clone().deps_install(dir.to_path_buf(), i));
        }
    }

    /// Runs `i` in `dir`, then tells every session waiting on it how it
    /// ended.
    async fn deps_install(self, dir: PathBuf, i: Install) {
        let ended = match self.home_folder() {
            Some(home) => {
                let env = crate::repos::kept_env(self.login_env());
                crate::deps::install(&i, &dir, &home, &env).await
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
        let deps = Deps::ended(i.command(), ended);
        let waiting: Vec<Uuid> = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let known: Vec<Uuid> = (st.installing.remove(&dir).unwrap_or_default())
                .into_iter()
                .filter(|id| st.records.contains_key(id))
                .collect();
            for id in &known {
                st.deps.insert(*id, deps.clone());
            }
            known
        };
        let by = By::desk("Desk installed the folder's dependencies as its session started.");
        for id in waiting {
            self.publish(id);
            self.tell_session(id, deps.told(), &by).await;
        }
    }
}

#[cfg(test)]
#[path = "deps_tests.rs"]
mod tests;

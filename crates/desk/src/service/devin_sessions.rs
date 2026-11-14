//! Devin sessions' own files and conversations ([`crate::devin`]): the
//! config and hook files each runs on, written as it starts and ended
//! (what Devin saved written back to the person's config, then removed)
//! as it ends or, for one that ended while the hub was away, as the hub
//! starts; and which of Devin's conversations a resume carries on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::devin::{self, SessionFiles};
use crate::devin_config;
use crate::engine::Engine;
use crate::model::SessionRecord;

/// How long `devin list` may take.
const LIST_WAIT: Duration = Duration::from_secs(10);

/// How much earlier than its record a session Devin made for it may say
/// it was made: their clocks are the same computer's, read a moment apart.
const MADE_SLACK: chrono::Duration = chrono::Duration::seconds(5);

/// A Devin session's last run while this hub runs.
#[derive(Debug, Clone, Copy)]
pub(super) struct Run {
    pub(super) launched: DateTime<Utc>,
    pub(super) ended: Option<DateTime<Utc>>,
}

/// What a resume of a Devin session carries on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Carries {
    /// Devin's conversation of this id.
    On(String),
    /// None: it starts a new one, and the person is told why.
    Afresh(String),
}

impl Desk {
    /// Held while session `id`'s files are ended or written, and its run
    /// ended or started: the end of one run never ends the next's files,
    /// nor says its state over the next's.
    pub(super) fn devin_lock(&self, id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.0.state.lock().devin_ending.entry(id).or_default())
    }

    /// Session `id` is forgotten, its files ended: what Desk kept of its runs goes.
    pub(super) fn devin_forgotten(&self, id: Uuid) {
        let mut st = self.0.state.lock();
        st.devin_runs.remove(&id);
        st.devin_ending.remove(&id);
    }

    /// Session `id` launched at `at` (or was taken back, launched then).
    pub(super) fn devin_launched(&self, id: Uuid, at: DateTime<Utc>) {
        self.0.state.lock().devin_runs.insert(
            id,
            Run {
                launched: at,
                ended: None,
            },
        );
    }

    /// The engine session `id` runs on.
    pub(super) fn engine_of(&self, id: Uuid) -> Option<Engine> {
        self.0.state.lock().records.get(&id).map(|r| r.engine)
    }

    /// The environment Devin sessions run with, as Desk adds to it.
    fn devin_env(&self) -> std::collections::BTreeMap<String, String> {
        self.with_added(&self.0.state.lock().session_env.clone())
    }

    /// Session `id`'s files, about to be written for its start. Any its run
    /// before left (it ended while the hub was away) are ended first, so
    /// what Devin saved then is written back, never lost under the new copy.
    pub(super) async fn devin_files(
        &self,
        id: Uuid,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<SessionFiles, DeskError> {
        let files = SessionFiles::of(&self.0.opts.run_dir, id);
        if [&files.config, &files.base, &files.hook]
            .iter()
            .any(|f| f.exists())
        {
            self.end_devin_files(id, files.clone(), devin_config::person_path(env))
                .await;
        }
        Ok(files)
    }

    /// Session `id` ended: what Devin saved to its config is written back to
    /// the person's, and its files go.
    pub(super) async fn devin_ended(&self, id: Uuid) {
        let files = SessionFiles::of(&self.0.opts.run_dir, id);
        let person = devin_config::person_path(&self.devin_env());
        self.end_devin_files(id, files, person).await;
    }

    async fn end_devin_files(&self, id: Uuid, files: SessionFiles, person: Option<PathBuf>) {
        let ended = tokio::task::spawn_blocking(move || files.end(person.as_deref())).await;
        match ended {
            Ok(Ok(true)) => {
                tracing::info!(session = %id, "wrote what Devin saved in a session back to the person's Devin config")
            }
            Ok(Ok(false)) => {}
            Ok(Err(e)) => {
                tracing::warn!(session = %id, error = %e, "couldn't write what Devin saved in a session back to the person's Devin config")
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "ending a Devin session's files failed")
            }
        }
    }

    /// Ends the files of Devin sessions no longer running: ones that ended
    /// while the hub was away. Run once every held session is taken back;
    /// `running` are those whose processes the holder still runs, taken
    /// back or not, whose files stay.
    pub(super) async fn sweep_devin_files(&self, running: &HashSet<Uuid>) {
        let run_dir = self.0.opts.run_dir.clone();
        let ids = tokio::task::spawn_blocking(move || devin::sessions_with_files(&run_dir))
            .await
            .unwrap_or_default();
        for id in ids {
            if running.contains(&id) {
                continue;
            }
            let lock = self.devin_lock(id);
            let _ending = lock.lock().await;
            if !self.0.state.lock().live.contains_key(&id) {
                self.devin_ended(id).await;
            }
        }
    }

    /// Devin's sessions in `cwd`, as `devin list --format json` says them;
    /// none when it can't be read.
    async fn devin_listed(&self, cwd: &Path) -> Option<Vec<devin::Listed>> {
        let Some(program) = self.program(Engine::Devin).map(Path::to_path_buf) else {
            tracing::warn!(cwd = %cwd.display(), "Desk has no `devin` to list its sessions for a resume");
            return None;
        };
        let mut env = self.devin_env();
        devin::scrub_env(&mut env);
        let run = tokio::process::Command::new(program)
            .args(["list", "--format", "json"])
            .current_dir(cwd)
            .env_clear()
            .envs(&env)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(LIST_WAIT, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                tracing::warn!(cwd = %cwd.display(), error = %e, "couldn't run `devin list` for a resume");
                return None;
            }
            Err(_) => {
                tracing::warn!(cwd = %cwd.display(), wait_s = LIST_WAIT.as_secs(), "`devin list` took too long for a resume");
                return None;
            }
        };
        if !out.status.success() {
            // Its code only: what it printed may say the person's sessions.
            tracing::warn!(cwd = %cwd.display(), code = ?out.status.code(), "`devin list` failed for a resume");
            return None;
        }
        let listed = devin::listed(&String::from_utf8_lossy(&out.stdout));
        if listed.is_none() {
            tracing::warn!(cwd = %cwd.display(), bytes = out.stdout.len(), "couldn't read what `devin list` printed for a resume");
        }
        listed
    }

    /// Which of Devin's conversations a resume of `r` carries on: the one
    /// its `SessionStart` said, while Devin still lists it in its folder; for
    /// one whose start never reached Desk (the hub was down), the one Devin
    /// made there since, when exactly one was. Otherwise a new one, and why.
    pub(super) async fn devin_carries(&self, r: &SessionRecord) -> Carries {
        let listed = self.devin_listed(Path::new(&r.cwd)).await;
        let (run, bound) = {
            let st = self.0.state.lock();
            let bound: HashSet<String> = st
                .records
                .values()
                .filter(|o| o.id != r.id && o.engine == Engine::Devin)
                .filter_map(|o| o.cli_session_id.clone())
                .collect();
            (st.devin_runs.get(&r.id).copied(), bound)
        };
        // Its last launch, while this hub knows it; else as old as the
        // session itself.
        let since = run.map_or(r.created_at, |run| run.launched) - MADE_SLACK;
        let until = run.and_then(|run| run.ended).unwrap_or_else(Utc::now);
        carries(
            r.cli_session_id.as_deref(),
            listed.as_deref(),
            (since, until),
            &bound,
        )
    }
}

/// [`Desk::devin_carries`], from what Devin listed (`None`: unread), the
/// span its last run could have made a conversation in, and the
/// conversations other sessions are already bound to.
pub(super) fn carries(
    said: Option<&str>,
    listed: Option<&[devin::Listed]>,
    (since, until): (DateTime<Utc>, DateTime<Utc>),
    bound: &HashSet<String>,
) -> Carries {
    match (said, listed) {
        (Some(id), Some(l)) if !l.iter().any(|s| s.id == id) => Carries::Afresh(
            "Devin no longer has this session's conversation in its folder, so it started a new one."
                .into(),
        ),
        // Listed, or Devin's list unread: Devin says itself if it's gone.
        (Some(id), _) => Carries::On(id.to_string()),
        (None, Some(l)) => {
            let free: Vec<devin::Listed> =
                l.iter().filter(|s| !bound.contains(&s.id)).cloned().collect();
            match devin::made_after(&free, since, until) {
                Some(id) => Carries::On(id.to_string()),
                None => Carries::Afresh(
                    "Desk never heard which conversation this Devin session was, so it started a new one."
                        .into(),
                ),
            }
        }
        (None, None) => Carries::Afresh(
            "Desk couldn't read Devin's sessions, so it started a new one.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn listed(ids: &[(&str, &str)]) -> Vec<devin::Listed> {
        ids.iter()
            .map(|(id, t)| devin::Listed {
                id: id.to_string(),
                created: Some(at(t)),
            })
            .collect()
    }

    fn span(since: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        (at(since), at("2026-11-15T00:00:00Z"))
    }

    #[test]
    fn a_resume_carries_on_the_conversation_devin_still_has() {
        let made = span("2026-11-14T09:00:00Z");
        let none = HashSet::new();
        let l = listed(&[("ses_a", "2026-11-14T09:00:01Z")]);
        assert_eq!(
            carries(Some("ses_a"), Some(&l), made, &none),
            Carries::On("ses_a".into())
        );
        assert!(matches!(
            carries(Some("ses_gone"), Some(&l), made, &none),
            Carries::Afresh(why) if why.contains("no longer has")
        ));
        // Its list unread: resumed, and Devin says if it's gone.
        assert_eq!(
            carries(Some("ses_a"), None, made, &none),
            Carries::On("ses_a".into())
        );
    }

    #[test]
    fn a_session_that_never_said_its_id_takes_only_the_one_devin_made_for_it() {
        let made = span("2026-11-14T09:00:00Z");
        let none = HashSet::new();
        let one = listed(&[
            ("before", "2026-11-13T09:00:00Z"),
            ("ses_new", "2026-11-14T09:00:02Z"),
        ]);
        assert_eq!(
            carries(None, Some(&one), made, &none),
            Carries::On("ses_new".into())
        );
        // The person's own devin there too, since: neither is taken.
        let two = listed(&[
            ("ses_new", "2026-11-14T09:00:02Z"),
            ("theirs", "2026-11-14T09:30:00Z"),
        ]);
        assert!(matches!(
            carries(None, Some(&two), made, &none),
            Carries::Afresh(_)
        ));
        assert!(matches!(
            carries(None, Some(&[]), made, &none),
            Carries::Afresh(_)
        ));
        // Its list unread: said so, not that Desk never heard.
        assert!(matches!(
            carries(None, None, made, &none),
            Carries::Afresh(why) if why == "Desk couldn't read Devin's sessions, so it started a new one."
        ));
    }

    #[test]
    fn recovery_looks_only_in_its_last_run_and_past_conversations_others_have() {
        let l = listed(&[
            // Made by its run before last.
            ("earlier", "2026-11-14T08:00:00Z"),
            ("ses_new", "2026-11-14T09:00:02Z"),
            // The person's own devin, after the session ended.
            ("theirs", "2026-11-14T09:30:00Z"),
        ]);
        let none = HashSet::new();
        // Its last run, 09:00 to 09:10: only the one made then.
        let run = (at("2026-11-14T08:59:55Z"), at("2026-11-14T09:10:00Z"));
        assert_eq!(
            carries(None, Some(&l), run, &none),
            Carries::On("ses_new".into())
        );
        // Another Desk session is already bound to it: never taken twice.
        let bound: HashSet<String> = ["ses_new".to_string()].into();
        assert!(matches!(
            carries(None, Some(&l), run, &bound),
            Carries::Afresh(_)
        ));
        // Two free in the span, one bound: the free one.
        let l = listed(&[
            ("other_desk", "2026-11-14T09:00:01Z"),
            ("ses_new", "2026-11-14T09:00:02Z"),
        ]);
        let bound: HashSet<String> = ["other_desk".to_string()].into();
        assert_eq!(
            carries(None, Some(&l), run, &bound),
            Carries::On("ses_new".into())
        );
    }
}

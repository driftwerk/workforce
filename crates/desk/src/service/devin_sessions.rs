//! Devin sessions' own files and conversations ([`crate::devin`]): the
//! config and hook files each runs on, written as it starts and ended
//! (what Devin saved written back to the person's config, then removed)
//! as it ends or, for one that ended while the hub was away, as the hub
//! starts; and which of Devin's conversations a resume carries on.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// What a resume of a Devin session carries on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Carries {
    /// Devin's conversation of this id.
    On(String),
    /// None: it starts a new one, and the person is told why.
    Afresh(String),
}

impl Desk {
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
    /// while the hub was away. Run once every held session is taken back.
    pub(super) async fn sweep_devin_files(&self) {
        let run_dir = self.0.opts.run_dir.clone();
        let ids = tokio::task::spawn_blocking(move || devin::sessions_with_files(&run_dir))
            .await
            .unwrap_or_default();
        for id in ids {
            if !self.0.state.lock().live.contains_key(&id) {
                self.devin_ended(id).await;
            }
        }
    }

    /// Devin's sessions in `cwd`, as `devin list --format json` says them;
    /// none when it can't be read.
    async fn devin_listed(&self, cwd: &Path) -> Option<Vec<devin::Listed>> {
        let program = self.program(Engine::Devin)?.to_path_buf();
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
        let out = tokio::time::timeout(LIST_WAIT, run).await.ok()?.ok()?;
        if !out.status.success() {
            return None;
        }
        devin::listed(&String::from_utf8_lossy(&out.stdout))
    }

    /// Which of Devin's conversations a resume of `r` carries on: the one
    /// its `SessionStart` said, while Devin still lists it in its folder; for
    /// one whose start never reached Desk (the hub was down), the one Devin
    /// made there since, when exactly one was. Otherwise a new one, and why.
    pub(super) async fn devin_carries(&self, r: &SessionRecord) -> Carries {
        let listed = self.devin_listed(Path::new(&r.cwd)).await;
        carries(r.cli_session_id.as_deref(), listed.as_deref(), r.created_at)
    }
}

/// [`Desk::devin_carries`], from what Devin listed (`None`: unread).
pub(super) fn carries(
    said: Option<&str>,
    listed: Option<&[devin::Listed]>,
    created: DateTime<Utc>,
) -> Carries {
    match (said, listed) {
        (Some(id), Some(l)) if !l.iter().any(|s| s.id == id) => Carries::Afresh(
            "Devin no longer has this session's conversation in its folder, so it started a new one."
                .into(),
        ),
        // Listed, or Devin's list unread: Devin says itself if it's gone.
        (Some(id), _) => Carries::On(id.to_string()),
        (None, Some(l)) => match devin::made_after(l, created - MADE_SLACK) {
            Some(id) => Carries::On(id.to_string()),
            None => Carries::Afresh(
                "Desk never heard which conversation this Devin session was, so it started a new one."
                    .into(),
            ),
        },
        (None, None) => Carries::Afresh(
            "Desk never heard which conversation this Devin session was, so it started a new one."
                .into(),
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

    #[test]
    fn a_resume_carries_on_the_conversation_devin_still_has() {
        let made = at("2026-11-14T09:00:00Z");
        let l = listed(&[("ses_a", "2026-11-14T09:00:01Z")]);
        assert_eq!(
            carries(Some("ses_a"), Some(&l), made),
            Carries::On("ses_a".into())
        );
        assert!(matches!(
            carries(Some("ses_gone"), Some(&l), made),
            Carries::Afresh(why) if why.contains("no longer has")
        ));
        // Its list unread: resumed, and Devin says if it's gone.
        assert_eq!(
            carries(Some("ses_a"), None, made),
            Carries::On("ses_a".into())
        );
    }

    #[test]
    fn a_session_that_never_said_its_id_takes_only_the_one_devin_made_for_it() {
        let made = at("2026-11-14T09:00:00Z");
        let one = listed(&[
            ("before", "2026-11-13T09:00:00Z"),
            ("ses_new", "2026-11-14T09:00:02Z"),
        ]);
        assert_eq!(
            carries(None, Some(&one), made),
            Carries::On("ses_new".into())
        );
        // The person's own devin there too, since: neither is taken.
        let two = listed(&[
            ("ses_new", "2026-11-14T09:00:02Z"),
            ("theirs", "2026-11-14T09:30:00Z"),
        ]);
        assert!(matches!(
            carries(None, Some(&two), made),
            Carries::Afresh(_)
        ));
        assert!(matches!(carries(None, Some(&[]), made), Carries::Afresh(_)));
        assert!(matches!(carries(None, None, made), Carries::Afresh(_)));
    }
}

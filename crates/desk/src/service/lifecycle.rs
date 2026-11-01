//! A session's life after its work: ending it (closed, its history kept and
//! resumable), removing it from Desk (its record, Activity, read points and
//! floor turns; its engine's transcript only when asked), clearing finished
//! ones in bulk, and the CLI sessions the person removed, which adoption
//! never takes again until they start tracking them again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError, Persist};
use crate::adopt;
use crate::crew::{By, Kind, Party};
use crate::model::{Engine, Origin, SessionRecord, SessionState, SessionView};
use crate::store::{record, DeskStore};

/// A CLI session the person removed from Desk: adoption leaves it alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hidden {
    pub engine: Engine,
    pub cli_session_id: String,
    /// What Desk called it, and where it ran, when it was removed.
    pub name: String,
    pub cwd: String,
    pub hidden_at: DateTime<Utc>,
}

/// How a session is removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    /// Delete the engine's own transcript too, so it can't be resumed.
    #[serde(default)]
    pub transcript: bool,
}

/// What a removal did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Removed {
    pub removed: u64,
    /// Transcript files and folders deleted.
    pub transcripts: u64,
}

/// Clears finished sessions in bulk.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clearing {
    /// Finished (stopped, interrupted or closed) more than this long ago.
    pub older_than_days: u32,
    /// Only count them.
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub transcripts: bool,
}

/// How many of the most recent closed sessions the hub lists.
const CLOSED_LISTED: i64 = 200;

/// How long ending a running session waits for its process to go.
const END_WAIT: Duration = Duration::from_secs(10);

impl DeskStore {
    pub async fn hidden(&self) -> Result<Vec<Hidden>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT engine, cli_session_id, name, cwd, hidden_at FROM hidden_sessions \
                 ORDER BY hidden_at DESC",
                &[],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(Hidden {
                    engine: Engine::parse(row.try_get("engine")?)?,
                    cli_session_id: row.try_get("cli_session_id")?,
                    name: row.try_get("name")?,
                    cwd: row.try_get("cwd")?,
                    hidden_at: row.try_get("hidden_at")?,
                })
            })
            .collect()
    }

    pub async fn hide(&self, h: &Hidden) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO hidden_sessions (engine, cli_session_id, name, cwd, hidden_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (engine, cli_session_id) DO UPDATE SET name = $3, cwd = $4, \
                 hidden_at = $5",
                &[
                    &h.engine.as_str(),
                    &h.cli_session_id,
                    &h.name,
                    &h.cwd,
                    &h.hidden_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn unhide(&self, engine: Engine, cli: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM hidden_sessions WHERE engine = $1 AND cli_session_id = $2",
                &[&engine.as_str(), &cli],
            )
            .await?;
        Ok(())
    }

    /// The most recently closed sessions, newest first.
    pub async fn closed(&self, limit: i64) -> Result<Vec<SessionRecord>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM sessions WHERE state = 'closed' \
                 ORDER BY COALESCE(state_at, updated_at) DESC LIMIT $1",
                &[&limit],
            )
            .await?;
        rows.iter().map(record).collect()
    }

    /// Sessions that finished (stopped, interrupted or closed) before `at`.
    pub async fn finished_before(&self, at: DateTime<Utc>) -> Result<Vec<Uuid>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT id FROM sessions \
                 WHERE state IN ('stopped', 'interrupted', 'closed') \
                 AND COALESCE(state_at, updated_at) < $1",
                &[&at],
            )
            .await?;
        rows.iter().map(|row| Ok(row.try_get("id")?)).collect()
    }
}

/// The folders each engine keeps its transcripts in, under `home`.
fn transcript_roots(home: &Path, engine: Engine) -> Vec<PathBuf> {
    match engine {
        Engine::Claude => vec![home.join(".claude/projects")],
        Engine::Codex => vec![home.join(".codex/sessions")],
        _ => vec![],
    }
}

/// What deleting `transcript` deletes: the file and, for Claude Code, the
/// folder beside it named after it (its sub-agents' transcripts and tool
/// results). Only within the engine's own transcript folders, and only a
/// `.jsonl` file: never anything else a stored path might name.
fn transcript_files(home: &Path, engine: Engine, transcript: &Path) -> Vec<PathBuf> {
    let Ok(file) = transcript.canonicalize() else {
        return vec![];
    };
    let inside = transcript_roots(home, engine)
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|root| file.starts_with(&root) && file != root);
    if !inside || !file.is_file() || file.extension().is_none_or(|e| e != "jsonl") {
        return vec![];
    }
    let mut out = vec![file.clone()];
    let beside = file.with_extension("");
    if engine == Engine::Claude && beside.is_dir() {
        out.push(beside);
    }
    out
}

fn delete(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

impl Desk {
    /// The session, from memory or, when the hub doesn't hold it (a closed
    /// one isn't loaded at start), from the registry, taken back.
    pub(super) async fn load(&self, id: Uuid) -> Result<SessionRecord, DeskError> {
        if let Ok(r) = self.record(id) {
            return Ok(r);
        }
        let kept = self.0.store.get(id).await?.ok_or(DeskError::NotFound)?;
        let mut st = self.0.state.lock();
        if st.forgotten.contains(&id) {
            return Err(DeskError::NotFound);
        }
        Ok(st.records.entry(id).or_insert(kept).clone())
    }

    /// Whether the session's process runs, here or in another terminal.
    fn runs(&self, r: &SessionRecord) -> bool {
        self.0.state.lock().live.contains_key(&r.id)
            || (r.origin == Origin::External
                && r.state.is_running()
                && r.external_pid.is_some_and(adopt::alive))
    }

    /// Whether adoption leaves `engine`'s CLI session `cli` alone.
    pub(super) fn is_hidden(&self, engine: Engine, cli: &str) -> bool {
        self.0
            .state
            .lock()
            .hidden
            .contains(&(engine, cli.to_string()))
    }

    /// Removes a session from Desk: its record, what Activity says of it,
    /// how far it was read, its turns on the floor and its place in the
    /// sidebar, and remembers its CLI session so it is never adopted again.
    /// One running here is stopped; one in another terminal keeps running
    /// there, untracked. Its engine's transcript stays unless asked, and
    /// can't be deleted while it runs.
    pub async fn remove(&self, id: Uuid, how: Removal) -> Result<Removed, DeskError> {
        let r = self.load(id).await?;
        let files = if how.transcript {
            if self.runs(&r) {
                return Err(DeskError::Conflict(
                    "it's still running, so its transcript can't be deleted; stop it first".into(),
                ));
            }
            self.transcript_files_of(&r)
        } else {
            vec![]
        };
        if let Some(cli) = r.cli_session_id.clone() {
            self.hide(Hidden {
                engine: r.engine,
                cli_session_id: cli,
                name: r.display_name.clone().unwrap_or_else(|| r.name.clone()),
                cwd: r.cwd.clone(),
                hidden_at: Utc::now(),
            });
        }
        self.forget(id).await?;
        let mut transcripts = 0;
        for f in files {
            match delete(&f) {
                Ok(()) => transcripts += 1,
                Err(e) => {
                    tracing::warn!(%id, path = %f.display(), error = %e, "couldn't delete a removed session's transcript");
                }
            }
        }
        Ok(Removed {
            removed: 1,
            transcripts,
        })
    }

    fn transcript_files_of(&self, r: &SessionRecord) -> Vec<PathBuf> {
        let Some(home) = self.0.env.get("HOME").map(PathBuf::from) else {
            return vec![];
        };
        let path = r.transcript.clone().or_else(|| {
            (r.engine == Engine::Claude)
                .then(|| super::find_transcript(&home, r.cli_session_id.as_deref()?))
                .flatten()
        });
        path.map_or_else(Vec::new, |p| {
            transcript_files(&home, r.engine, Path::new(&p))
        })
    }

    fn hide(&self, h: Hidden) {
        self.0
            .state
            .lock()
            .hidden
            .insert((h.engine, h.cli_session_id.clone()));
        let _ = self.0.persist.send(Persist::Hide(Box::new(h)));
    }

    /// Starts tracking a removed CLI session again: adoption takes it the
    /// next time it runs in another terminal.
    pub fn unhide(&self, engine: Engine, cli: &str) {
        let was = self
            .0
            .state
            .lock()
            .hidden
            .remove(&(engine, cli.to_string()));
        if was {
            let _ = self
                .0
                .persist
                .send(Persist::Unhide(engine, cli.to_string()));
        }
    }

    pub async fn hidden_sessions(&self) -> Result<Vec<Hidden>, DeskError> {
        Ok(self.0.store.hidden().await?)
    }

    /// The most recently closed sessions: ended, or closed in their own
    /// terminals; each resumable where its engine allows.
    pub async fn closed_sessions(&self) -> Result<Vec<SessionView>, DeskError> {
        let kept = self.0.store.closed(CLOSED_LISTED).await?;
        let st = self.0.state.lock();
        let mut seen = HashSet::new();
        let mut out: Vec<SessionView> = st
            .records
            .values()
            .filter(|r| r.state == SessionState::Closed)
            .chain(kept.iter().filter(|r| !st.records.contains_key(&r.id)))
            .filter(|r| seen.insert(r.id))
            .map(|r| Self::view_of(&st, r))
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out.truncate(CLOSED_LISTED as usize);
        Ok(out)
    }

    /// Removes every session that finished (stopped, interrupted or
    /// closed) more than `older_than_days` ago, or only counts them.
    pub async fn clear_finished(&self, c: Clearing) -> Result<Removed, DeskError> {
        let before = Utc::now() - chrono::Duration::days(i64::from(c.older_than_days));
        let finished = |s: SessionState| {
            matches!(
                s,
                SessionState::Stopped | SessionState::Interrupted | SessionState::Closed
            )
        };
        let mut ids: HashSet<Uuid> = self
            .0
            .store
            .finished_before(before)
            .await?
            .into_iter()
            .collect();
        {
            let st = self.0.state.lock();
            // The hub's own view wins over what the registry has yet to be told.
            ids.retain(|id| {
                st.records.get(id).is_none_or(|r| finished(r.state)) && !st.live.contains_key(id)
            });
            ids.extend(
                st.records
                    .values()
                    .filter(|r| {
                        finished(r.state)
                            && !st.live.contains_key(&r.id)
                            && st.state_since.get(&r.id).copied().unwrap_or(r.updated_at) < before
                    })
                    .map(|r| r.id),
            );
        }
        if c.dry_run {
            return Ok(Removed {
                removed: ids.len() as u64,
                transcripts: 0,
            });
        }
        let mut done = Removed::default();
        for id in ids {
            match self
                .remove(
                    id,
                    Removal {
                        transcript: c.transcripts,
                    },
                )
                .await
            {
                Ok(r) => {
                    done.removed += r.removed;
                    done.transcripts += r.transcripts;
                }
                Err(DeskError::NotFound) => {}
                Err(e) => tracing::warn!(%id, error = %e, "couldn't clear a finished session"),
            }
        }
        Ok(done)
    }

    /// Ends a session: stops it if it runs here and closes it, so it leaves
    /// the sessions Desk shows and its history stays, resumable. One in
    /// another terminal or in one of Desk's ends there.
    pub async fn end(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        let r = self.load(id).await?;
        let running_here = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            match st.live.get_mut(&id) {
                Some(live) if live.terminal.is_some() => {
                    return Err(DeskError::Conflict(
                        "this session runs in a terminal; end it there".into(),
                    ))
                }
                Some(live) => {
                    live.stopping = true;
                    live.closing = true;
                    let pty = std::sync::Arc::clone(&live.pty);
                    st.acted.insert(id);
                    Some(pty)
                }
                None => None,
            }
        };
        match running_here {
            Some(pty) => {
                pty.kill();
                let deadline = tokio::time::Instant::now() + END_WAIT;
                while self.0.state.lock().live.contains_key(&id) {
                    if tokio::time::Instant::now() > deadline {
                        return Err(DeskError::Conflict(
                            "it didn't stop in time; try again".into(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
            }
            None if self.runs(&r) => {
                return Err(DeskError::Conflict(
                    "still running in another terminal; end it there".into(),
                ))
            }
            None if r.state != SessionState::Closed => {
                self.0.state.lock().acted.insert(id);
                self.change(id, |r, _| r.state = SessionState::Closed);
            }
            None => {}
        }
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Stopped,
                Some(Party::session(&view)),
                format!("ended {}", view.label()),
            ));
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_transcript_in_the_engines_own_folder_is_deleted() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-Users-k-dev-app");
        std::fs::create_dir_all(project.join("s1/subagents")).unwrap();
        std::fs::write(project.join("s1.jsonl"), "{}").unwrap();
        std::fs::write(project.join("s1/subagents/agent-a.jsonl"), "{}").unwrap();
        let elsewhere = home.path().join("notes.jsonl");
        std::fs::write(&elsewhere, "{}").unwrap();
        let readme = project.join("README.md");
        std::fs::write(&readme, "").unwrap();

        let files = transcript_files(home.path(), Engine::Claude, &project.join("s1.jsonl"));
        let want = [
            project.join("s1.jsonl").canonicalize().unwrap(),
            project.join("s1").canonicalize().unwrap(),
        ];
        assert_eq!(files, want);
        assert!(transcript_files(home.path(), Engine::Claude, &elsewhere).is_empty());
        assert!(transcript_files(home.path(), Engine::Claude, &readme).is_empty());
        // Climbing out of the folder doesn't get past it.
        let climbing = project.join("../../../notes.jsonl");
        assert!(transcript_files(home.path(), Engine::Claude, &climbing).is_empty());
        // Another engine's folder isn't Claude Code's.
        assert!(transcript_files(home.path(), Engine::Codex, &project.join("s1.jsonl")).is_empty());
    }
}

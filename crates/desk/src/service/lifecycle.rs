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

use super::{Desk, DeskError, Persist, State};
use crate::adopt;
use crate::codex;
use crate::crew::{By, Kind, Party};
use crate::memory_store::Memory;
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

const HIDE: &str = "INSERT INTO hidden_sessions (engine, cli_session_id, name, cwd, hidden_at) \
     VALUES ($1, $2, $3, $4, $5) \
     ON CONFLICT (engine, cli_session_id) DO UPDATE SET name = $3, cwd = $4, hidden_at = $5";

async fn hide_in(client: &impl tokio_postgres::GenericClient, h: &Hidden) -> Result<()> {
    client
        .execute(
            HIDE,
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
        hide_in(&**client, h).await
    }

    /// Forgets sessions as [`DeskStore::forget_sessions`] does and hides
    /// `hidden` in the same transaction: a removal is all written or none
    /// of it is.
    pub async fn remove_sessions(
        &self,
        ids: &[Uuid],
        hidden: &[Hidden],
        owner: &str,
        memory: crate::memory_store::Memory,
    ) -> Result<u64> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for h in hidden {
            hide_in(&*tx, h).await?;
        }
        let gone = crate::store::forget_in(&tx, ids, owner, memory).await?;
        tx.commit().await?;
        Ok(gone)
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

/// The folder `engine` keeps its transcripts in, under `home`.
fn transcript_root(home: &Path, engine: Engine) -> Option<PathBuf> {
    match engine {
        Engine::Claude => Some(home.join(".claude/projects")),
        Engine::Codex => Some(home.join(".codex/sessions")),
        _ => None,
    }
}

/// Whether `cli` is a CLI session id as the engines make them (a UUID, or
/// letters, digits, `-` and `_`): never a path, so nothing derived from it
/// leaves the folder it is looked for in.
fn plain_id(cli: &str) -> bool {
    !cli.is_empty()
        && cli.len() <= 128
        && cli
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What deleting a transcript deletes: the file, or the folder beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Doomed {
    File(PathBuf),
    Folder(PathBuf),
}

impl Doomed {
    fn path(&self) -> &Path {
        match self {
            Self::File(p) | Self::Folder(p) => p,
        }
    }
}

/// `path`, when it is `engine`'s transcript of CLI session `cli` in its
/// own folder `root` (canonical): named for it (`<cli>.jsonl` in a project
/// folder for Claude Code, `rollout-…-<cli>.jsonl` under a day's folder
/// for Codex), a file itself and not a link, in a folder that is really
/// where it says. As its canonical folder joined with its name.
fn own_transcript(root: &Path, engine: Engine, cli: &str, path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let named = match engine {
        Engine::Claude => name == format!("{cli}.jsonl"),
        Engine::Codex => name.starts_with("rollout-") && name.ends_with(&format!("-{cli}.jsonl")),
        _ => false,
    };
    if !named || !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let folder = path.parent()?.canonicalize().ok()?;
    let inside = match engine {
        // `projects/<project>/<cli>.jsonl`, never deeper (a sub-agent's).
        Engine::Claude => folder.parent() == Some(root),
        _ => folder.starts_with(root) && folder != root,
    };
    inside.then(|| folder.join(name))
}

/// What deleting CLI session `cli`'s transcript deletes: the file and, for
/// Claude Code, the real folder beside it named after it (its sub-agents'
/// transcripts and tool results). The transcript is the one `stored` names
/// when that is `cli`'s own, else the one found for `cli`; never anything
/// else a stored path, or an id, might name.
pub(super) fn transcript_files(
    home: &Path,
    engine: Engine,
    cli: &str,
    stored: Option<&Path>,
) -> Vec<Doomed> {
    if !plain_id(cli) {
        return vec![];
    }
    let Some(root) = transcript_root(home, engine).and_then(|r| r.canonicalize().ok()) else {
        return vec![];
    };
    let found = || match engine {
        Engine::Claude => super::find_transcript(home, cli).map(PathBuf::from),
        Engine::Codex => codex::find_rollout(home, cli),
        _ => None,
    };
    let Some(file) = stored
        .and_then(|p| own_transcript(&root, engine, cli, p))
        .or_else(|| own_transcript(&root, engine, cli, &found()?))
    else {
        return vec![];
    };
    let beside = file.with_file_name(cli);
    let mut out = vec![Doomed::File(file)];
    if engine == Engine::Claude
        && std::fs::symlink_metadata(&beside).is_ok_and(|m| m.file_type().is_dir())
    {
        out.push(Doomed::Folder(beside));
    }
    out
}

/// Deletes `doomed` if it is still what it was when it was found: its
/// folder where it was (no link put in its way since) and itself the same
/// kind of thing, not a link. Whether it was.
fn delete(doomed: &Doomed) -> std::io::Result<bool> {
    let path = doomed.path();
    let Some(folder) = path.parent() else {
        return Ok(false);
    };
    if folder.canonicalize().ok().as_deref() != Some(folder) {
        return Ok(false);
    }
    let Ok(kind) = std::fs::symlink_metadata(path).map(|m| m.file_type()) else {
        return Ok(false);
    };
    match doomed {
        Doomed::File(_) if kind.is_file() => std::fs::remove_file(path).map(|()| true),
        // Removes what is inside, never what a link inside names.
        Doomed::Folder(_) if kind.is_dir() => std::fs::remove_dir_all(path).map(|()| true),
        _ => Ok(false),
    }
}

/// Whether a session in `state` has finished: stopped, interrupted or
/// closed.
fn finished(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Stopped | SessionState::Interrupted | SessionState::Closed
    )
}

/// Whether `r` finished before `before`, and doesn't run here.
pub(super) fn finished_before(st: &State, r: &SessionRecord, before: DateTime<Utc>) -> bool {
    finished(r.state)
        && !st.live.contains_key(&r.id)
        && st.state_since.get(&r.id).copied().unwrap_or(r.updated_at) < before
}

const STILL_RUNNING: &str = "it's still running, so its transcript can't be deleted; stop it first";

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

    /// Whether `engine`'s CLI session `cli` runs under any record the hub
    /// holds: here, or adopted from another terminal.
    fn cli_runs(&self, engine: Engine, cli: &str) -> bool {
        let st = self.0.state.lock();
        st.records
            .values()
            .filter(|r| r.engine == engine && r.cli_session_id.as_deref() == Some(cli))
            .any(|r| {
                st.live.contains_key(&r.id)
                    || (r.origin == Origin::External
                        && r.state.is_running()
                        && r.external_pid.is_some_and(adopt::alive))
            })
    }

    /// The Claude Code sessions running in other terminals now, tracked or
    /// not, as `claude agents` says; an error when it can't say, so no
    /// transcript is deleted on a guess.
    async fn running_elsewhere(&self) -> Result<HashSet<String>, DeskError> {
        let Some(claude) = self.program(Engine::Claude) else {
            return Ok(HashSet::new());
        };
        match adopt::list(claude, &self.0.env).await {
            Ok(found) => Ok(found.into_iter().map(|e| e.session_id).collect()),
            Err(e) => Err(DeskError::Conflict(format!(
                "couldn't tell whether it runs in another terminal, so its transcript can't be \
                 deleted now: {e}"
            ))),
        }
    }

    /// Removes a session from Desk: its record, what Activity says of it,
    /// how far it was read, its turns on the floor and its place in the
    /// sidebar, and remembers its CLI session so it is never adopted again.
    /// One running here is stopped; one in another terminal keeps running
    /// there, untracked. Its engine's transcript stays unless asked, and
    /// can't be deleted while it runs anywhere.
    pub async fn remove(&self, id: Uuid, how: Removal) -> Result<Removed, DeskError> {
        let elsewhere = match how.transcript {
            // Only Claude Code says what runs in other terminals.
            true if self.load(id).await?.engine == Engine::Claude => {
                Some(self.running_elsewhere().await?)
            }
            true => Some(HashSet::new()),
            false => None,
        };
        self.remove_as(id, elsewhere.as_ref(), Memory::Forget, |_, _| Ok(()))
            .await
    }

    /// [`Self::remove`], deleting its transcript when `elsewhere` (what
    /// runs in other terminals) is given, once `may` agrees under the lock
    /// it is removed under.
    pub(super) async fn remove_as(
        &self,
        id: Uuid,
        elsewhere: Option<&HashSet<String>>,
        memory: Memory,
        may: impl FnOnce(&State, &SessionRecord) -> Result<(), DeskError>,
    ) -> Result<Removed, DeskError> {
        let r = self.load(id).await?;
        let files = match elsewhere {
            Some(elsewhere) => {
                let cli = r.cli_session_id.as_deref();
                if self.runs(&r)
                    || cli.is_some_and(|cli| {
                        self.cli_runs(r.engine, cli)
                            || (r.engine == Engine::Claude && elsewhere.contains(cli))
                    })
                {
                    return Err(DeskError::Conflict(STILL_RUNNING.into()));
                }
                self.transcript_files_of(&r)
            }
            None => vec![],
        };
        let hidden = r.cli_session_id.clone().map(|cli| Hidden {
            engine: r.engine,
            cli_session_id: cli,
            name: r.display_name.clone().unwrap_or_else(|| r.name.clone()),
            cwd: r.cwd.clone(),
            hidden_at: Utc::now(),
        });
        let deleting = !files.is_empty();
        let written = self
            .forget_as(id, hidden, memory, |st, now| {
                if deleting && st.live.contains_key(&id) {
                    // Started since it was looked at.
                    return Err(DeskError::Conflict(STILL_RUNNING.into()));
                }
                may(st, now)
            })
            .await?;
        let mut transcripts = 0;
        if deleting {
            // Only once the removal is written: a transcript never goes
            // while the registry could still bring the session back.
            if written.await.unwrap_or(false) {
                transcripts = self.delete_transcripts(&r, &files);
            } else {
                tracing::warn!(%id, "a removed session's transcript stays: the registry didn't write its removal");
            }
        }
        Ok(Removed {
            removed: 1,
            transcripts,
        })
    }

    /// Deletes `files`, `r`'s transcript, unless its CLI session came back
    /// since it was removed (started again, so tracked again). How many
    /// went.
    fn delete_transcripts(&self, r: &SessionRecord, files: &[Doomed]) -> u64 {
        let Some(cli) = r.cli_session_id.as_deref() else {
            return 0;
        };
        let back = !self.is_hidden(r.engine, cli)
            || self
                .0
                .state
                .lock()
                .records
                .values()
                .any(|o| o.engine == r.engine && o.cli_session_id.as_deref() == Some(cli));
        if back {
            tracing::warn!(id = %r.id, "a removed session's transcript stays: it was started again");
            return 0;
        }
        let mut n = 0;
        for f in files {
            match delete(f) {
                Ok(true) => n += 1,
                Ok(false) => {
                    tracing::warn!(id = %r.id, path = %f.path().display(), "a removed session's transcript stays: it changed since it was checked")
                }
                Err(e) => {
                    tracing::warn!(id = %r.id, path = %f.path().display(), error = %e, "couldn't delete a removed session's transcript");
                }
            }
        }
        n
    }

    fn transcript_files_of(&self, r: &SessionRecord) -> Vec<Doomed> {
        let (Some(home), Some(cli)) = (self.0.env.get("HOME"), r.cli_session_id.as_deref()) else {
            return vec![];
        };
        transcript_files(
            Path::new(home),
            r.engine,
            cli,
            r.transcript.as_deref().map(Path::new),
        )
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
    /// closed) more than `older_than_days` ago, or only counts them. Each
    /// is looked at again as it is removed: one that started since stays.
    pub async fn clear_finished(&self, c: Clearing) -> Result<Removed, DeskError> {
        let before = chrono::Duration::try_days(i64::from(c.older_than_days))
            .and_then(|d| Utc::now().checked_sub_signed(d))
            .ok_or_else(|| DeskError::Invalid("that's further back than any session".into()))?;
        let kept = self.0.store.finished_before(before).await?;
        let ids: HashSet<Uuid> = {
            let st = self.0.state.lock();
            // The hub's own view of what it holds wins over what the
            // registry has yet to be told.
            kept.into_iter()
                .filter(|id| !st.records.contains_key(id) && !st.forgotten.contains(id))
                .chain(
                    st.records
                        .values()
                        .filter(|r| finished_before(&st, r, before))
                        .map(|r| r.id),
                )
                .collect()
        };
        if c.dry_run {
            return Ok(Removed {
                removed: ids.len() as u64,
                transcripts: 0,
            });
        }
        let elsewhere = if c.transcripts {
            Some(self.running_elsewhere().await?)
        } else {
            None
        };
        let mut done = Removed::default();
        for id in ids {
            let still = |st: &State, r: &SessionRecord| {
                if finished_before(st, r, before) {
                    Ok(())
                } else {
                    Err(DeskError::Conflict("it started again".into()))
                }
            };
            // Clearing only tidies the cards: what Desk remembers of them stays.
            match self
                .remove_as(id, elsewhere.as_ref(), Memory::Keep, still)
                .await
            {
                Ok(r) => {
                    done.removed += r.removed;
                    done.transcripts += r.transcripts;
                }
                Err(DeskError::NotFound) => {}
                Err(e) => tracing::info!(%id, error = %e, "a finished session wasn't cleared"),
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
                // Until its exit has closed it, so what this answers is so.
                while self.record(id)?.state != SessionState::Closed {
                    if tokio::time::Instant::now() > deadline {
                        return Err(DeskError::Conflict(
                            "it's taking a while to stop; it closes once it has".into(),
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
    use std::os::unix::fs::symlink;

    /// A home with Claude Code's projects and Codex's sessions in it.
    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("home/.claude/projects")).unwrap();
            std::fs::create_dir_all(dir.path().join("home/.codex/sessions")).unwrap();
            Self { dir }
        }
        fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }
        /// Outside the home: what nothing may ever delete.
        fn outside(&self) -> PathBuf {
            let p = self.dir.path().join("outside");
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        fn project(&self, name: &str) -> PathBuf {
            let p = self.home().join(".claude/projects").join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        /// Claude Code's transcript of `cli`, with a sub-agent's beside it.
        fn claude(&self, project: &str, cli: &str) -> PathBuf {
            let p = self.project(project);
            std::fs::create_dir_all(p.join(cli).join("subagents")).unwrap();
            std::fs::write(p.join(cli).join("subagents/agent-a.jsonl"), "{}").unwrap();
            std::fs::write(p.join(format!("{cli}.jsonl")), "{}").unwrap();
            p.join(format!("{cli}.jsonl")).canonicalize().unwrap()
        }
        fn files(&self, engine: Engine, cli: &str, stored: Option<&Path>) -> Vec<Doomed> {
            transcript_files(&self.home(), engine, cli, stored)
        }
    }

    fn paths(d: &[Doomed]) -> Vec<PathBuf> {
        d.iter().map(|d| d.path().to_path_buf()).collect()
    }

    #[test]
    fn a_claude_transcript_and_its_sub_agents_folder_are_found_by_its_id() {
        let h = Home::new();
        let file = h.claude("-Users-k-dev-app", "cc-1");
        let folder = file.with_file_name("cc-1");
        let want = vec![Doomed::File(file.clone()), Doomed::Folder(folder)];
        assert_eq!(h.files(Engine::Claude, "cc-1", Some(&file)), want);
        assert_eq!(h.files(Engine::Claude, "cc-1", None), want, "found");
        // Climbing out and back in is still its own.
        let climbing = h
            .project("-Users-k-dev-app")
            .join("../-Users-k-dev-app/cc-1.jsonl");
        assert_eq!(h.files(Engine::Claude, "cc-1", Some(&climbing)), want);
    }

    #[test]
    fn an_id_that_is_a_path_finds_nothing() {
        let h = Home::new();
        h.claude("-Users-k-dev-other", "victim");
        for cli in [
            "../-Users-k-dev-other/victim",
            "..",
            ".",
            "",
            "a/b",
            "a\\b",
            "cc\0",
        ] {
            assert!(h.files(Engine::Claude, cli, None).is_empty(), "{cli:?}");
            assert!(h.files(Engine::Codex, cli, None).is_empty(), "{cli:?}");
        }
        assert!(!plain_id(&"a".repeat(129)));
        assert!(plain_id("0b0c8f0e-3f7a-4b9e-9a51-3e1f2c7d9a10"));
    }

    #[test]
    fn a_stored_path_naming_another_sessions_transcript_is_never_taken() {
        let h = Home::new();
        let other = h.claude("-Users-k-dev-app", "someone-else");
        let readme = h.project("-Users-k-dev-app").join("README.md");
        std::fs::write(&readme, "").unwrap();
        let sub = other.with_file_name("someone-else/subagents/agent-a.jsonl");
        let outside = h.outside().join("cc-2.jsonl");
        std::fs::write(&outside, "{}").unwrap();
        for stored in [&other, &readme, &sub, &outside] {
            assert!(
                h.files(Engine::Claude, "cc-2", Some(stored)).is_empty(),
                "{}",
                stored.display()
            );
        }
        // Its own, when it has one, is what goes instead.
        let own = h.claude("-Users-k-dev-app", "cc-2");
        assert_eq!(
            paths(&h.files(Engine::Claude, "cc-2", Some(&other)))[0],
            own
        );
    }

    #[test]
    fn a_transcript_deeper_than_a_project_folder_is_a_sub_agents_not_the_sessions() {
        let h = Home::new();
        let p = h.project("-Users-k-dev-app");
        std::fs::create_dir_all(p.join("parent/subagents")).unwrap();
        let deep = p.join("parent/subagents/cc-3.jsonl");
        std::fs::write(&deep, "{}").unwrap();
        assert!(h.files(Engine::Claude, "cc-3", Some(&deep)).is_empty());
        // And the folder itself is never one.
        let root = h.home().join(".claude/projects/cc-3.jsonl");
        std::fs::write(&root, "{}").unwrap();
        assert!(h.files(Engine::Claude, "cc-3", Some(&root)).is_empty());
    }

    #[test]
    fn links_never_lead_a_deletion_out() {
        let h = Home::new();
        let outside = h.outside();
        std::fs::write(outside.join("cc-4.jsonl"), "{}").unwrap();
        std::fs::create_dir_all(outside.join("cc-4")).unwrap();
        std::fs::write(outside.join("cc-4/keep.txt"), "").unwrap();

        // A transcript that is a link: not a transcript.
        let p = h.project("-Users-k-dev-app");
        symlink(outside.join("cc-4.jsonl"), p.join("cc-4.jsonl")).unwrap();
        assert!(h.files(Engine::Claude, "cc-4", None).is_empty());
        // A project folder that is a link out of the projects folder.
        symlink(&outside, h.home().join(".claude/projects/-linked")).unwrap();
        let through = h.home().join(".claude/projects/-linked/cc-4.jsonl");
        assert!(h.files(Engine::Claude, "cc-4", Some(&through)).is_empty());

        // A sub-agents folder that is a link: the transcript goes, it stays.
        let file = h.claude("-Users-k-dev-web", "cc-5");
        let folder = file.with_file_name("cc-5");
        std::fs::remove_dir_all(&folder).unwrap();
        symlink(outside.join("cc-4"), &folder).unwrap();
        let doomed = h.files(Engine::Claude, "cc-5", None);
        assert_eq!(doomed, vec![Doomed::File(file)]);
        for d in &doomed {
            assert!(delete(d).unwrap());
        }
        assert!(outside.join("cc-4/keep.txt").is_file());
        assert!(outside.join("cc-4.jsonl").is_file());
    }

    #[test]
    fn what_changed_since_it_was_checked_is_not_deleted() {
        let h = Home::new();
        let outside = h.outside();
        std::fs::create_dir_all(outside.join("cc-6/subagents")).unwrap();
        std::fs::write(outside.join("cc-6.jsonl"), "{}").unwrap();
        let file = h.claude("-Users-k-dev-app", "cc-6");
        let doomed = h.files(Engine::Claude, "cc-6", None);
        assert_eq!(doomed.len(), 2);

        // Its project folder swapped for a link out, after the check.
        let project = file.parent().unwrap().to_path_buf();
        std::fs::rename(&project, h.dir.path().join("moved")).unwrap();
        symlink(&outside, &project).unwrap();
        for d in &doomed {
            assert!(!delete(d).unwrap(), "{}", d.path().display());
        }
        assert!(outside.join("cc-6.jsonl").is_file());
        assert!(outside.join("cc-6").is_dir());

        // The file swapped for a link, and the folder for a file.
        std::fs::remove_file(&project).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        symlink(outside.join("cc-6.jsonl"), &file).unwrap();
        std::fs::write(file.with_file_name("cc-6"), "").unwrap();
        for d in &doomed {
            assert!(!delete(d).unwrap(), "{}", d.path().display());
        }
        assert!(outside.join("cc-6.jsonl").is_file());
    }

    #[test]
    fn a_codex_rollout_is_its_threads_alone() {
        let h = Home::new();
        let day = h.home().join(".codex/sessions/2026/10/27");
        std::fs::create_dir_all(&day).unwrap();
        let own = day.join("rollout-2026-10-27T17-06-33-t1.jsonl");
        let other = day.join("rollout-2026-10-27T18-00-00-t2.jsonl");
        std::fs::write(&own, "{}").unwrap();
        std::fs::write(&other, "{}").unwrap();
        let own = own.canonicalize().unwrap();
        assert_eq!(
            h.files(Engine::Codex, "t1", Some(&own)),
            vec![Doomed::File(own.clone())]
        );
        assert_eq!(
            h.files(Engine::Codex, "t1", None),
            vec![Doomed::File(own.clone())]
        );
        assert_eq!(
            h.files(Engine::Codex, "t1", Some(&other)),
            vec![Doomed::File(own)]
        );
        assert!(h.files(Engine::Codex, "t3", Some(&other)).is_empty());
        // Not Claude Code's, and not a rollout's name.
        assert!(h.files(Engine::Claude, "t1", Some(&other)).is_empty());
        let plain = day.join("t4.jsonl");
        std::fs::write(&plain, "{}").unwrap();
        assert!(h.files(Engine::Codex, "t4", Some(&plain)).is_empty());
        // Claude Code's transcripts aren't Codex's.
        let claude = h.claude("-Users-k-dev-app", "t5");
        assert!(h.files(Engine::Codex, "t5", Some(&claude)).is_empty());
    }

    #[test]
    fn outside_its_engines_folder_nothing_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let stray = dir.path().join("cc-7.jsonl");
        std::fs::write(&stray, "{}").unwrap();
        // Without the folder at all.
        assert!(transcript_files(dir.path(), Engine::Claude, "cc-7", Some(&stray)).is_empty());
        std::fs::create_dir_all(dir.path().join(".claude/projects")).unwrap();
        assert!(transcript_files(dir.path(), Engine::Claude, "cc-7", Some(&stray)).is_empty());
        // A Windows path, as a hub in WSL might be told one.
        let windows = Path::new(r"C:\Users\k\.claude\projects\-app\cc-7.jsonl");
        assert!(transcript_files(dir.path(), Engine::Claude, "cc-7", Some(windows)).is_empty());
    }
}

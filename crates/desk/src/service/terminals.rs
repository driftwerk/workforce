//! Terminals: plain login shells Desk opens, with whatever environment Desk
//! asks them to carry (so a `claude` started in one loads Desk's hooks).
//! The shell's working directory is followed as it moves and kept, with its
//! environment, in Postgres: a terminal the holder kept is taken back as it
//! is after a hub restart; one a reboot ended is reopened where it was.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_postgres::Row;
use uuid::Uuid;

use super::{Desk, DeskError, Persist, RestorePolicy};
use crate::holder::{Held, Holder};
use crate::model::DeskEvent;
use crate::pty::{Exit, Pty, Spawn};
use crate::store::DeskStore;

/// How often a running shell's working directory is looked at.
pub(super) const CWD_EVERY: Duration = Duration::from_secs(3);
/// Most variables a terminal's environment may add.
const MAX_ENV: usize = 64;
const MAX_ENV_VALUE: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalState {
    /// Its shell is running.
    Running,
    /// Its shell ended (`exit`, or it was stopped).
    Exited,
    /// Its shell ended without anyone ending it (a reboot) and wasn't
    /// reopened (restore is off).
    Lost,
}

impl TerminalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => Self::Running,
            "exited" => Self::Exited,
            "lost" => Self::Lost,
            other => eyre::bail!("unknown terminal state {other:?}"),
        })
    }
}

/// A terminal as Postgres keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalRecord {
    pub id: Uuid,
    pub name: String,
    /// Where the shell is (last seen), where it reopens.
    pub cwd: String,
    /// What the shell carries on top of the hub's login environment.
    pub env: BTreeMap<String, String>,
    pub shell: String,
    pub state: TerminalState,
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The wire shape of a terminal. Its environment's values are not sent
/// back (they can hold tokens); their names are.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalView {
    pub id: Uuid,
    pub name: String,
    pub cwd: String,
    pub env_keys: Vec<String>,
    pub shell: String,
    pub state: TerminalState,
    pub live: bool,
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TerminalView {
    fn of(r: &TerminalRecord, live: bool) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            cwd: r.cwd.clone(),
            env_keys: r.env.keys().cloned().collect(),
            shell: r.shell.clone(),
            state: r.state,
            live,
            exit_code: r.exit_code,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTerminal {
    /// Desk's own id for it, when it wants to know it before the shell starts.
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: String,
    /// Added to the shell's environment, e.g. `CLAUDE_CODE_PLUGIN_DIRS`,
    /// `DESK_HOOK_TOKEN`. `DESK_TERMINAL_ID` is always the terminal's id.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TerminalPatch {
    pub name: Option<String>,
}

fn check_env(env: &BTreeMap<String, String>) -> Result<(), DeskError> {
    if env.len() > MAX_ENV {
        return Err(DeskError::Invalid(format!(
            "a terminal's environment adds at most {MAX_ENV} variables"
        )));
    }
    for (k, v) in env {
        let name_ok = k
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(DeskError::Invalid(format!(
                "{k:?} is not an environment variable name"
            )));
        }
        if v.len() > MAX_ENV_VALUE || v.contains('\0') {
            return Err(DeskError::Invalid(format!("{k}'s value is not usable")));
        }
    }
    Ok(())
}

fn folder_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map_or_else(|| cwd.to_string(), |n| n.to_string_lossy().into_owned())
}

impl DeskStore {
    pub async fn terminals(&self) -> Result<Vec<TerminalRecord>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT * FROM terminals ORDER BY created_at", &[])
            .await?;
        rows.iter().map(terminal).collect()
    }

    pub async fn put_terminal(&self, t: &TerminalRecord) -> Result<()> {
        let client = self.pool().get().await?;
        let env = serde_json::to_value(&t.env)?;
        client
            .execute(
                "INSERT INTO terminals (id, name, cwd, env, shell, state, exit_code, created_at, \
                 updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
                 ON CONFLICT (id) DO UPDATE SET name = $2, cwd = $3, env = $4, shell = $5, \
                 state = $6, exit_code = $7, updated_at = $9",
                &[
                    &t.id,
                    &t.name,
                    &t.cwd,
                    &env,
                    &t.shell,
                    &t.state.as_str(),
                    &t.exit_code,
                    &t.created_at,
                    &t.updated_at,
                ],
            )
            .await
            .context("saving desk terminal")?;
        Ok(())
    }

    pub async fn delete_terminal(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute("DELETE FROM terminals WHERE id = $1", &[&id])
            .await?;
        Ok(())
    }
}

fn terminal(row: &Row) -> Result<TerminalRecord> {
    Ok(TerminalRecord {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        cwd: row.try_get("cwd")?,
        env: serde_json::from_value(row.try_get("env")?)?,
        shell: row.try_get("shell")?,
        state: TerminalState::parse(row.try_get("state")?)?,
        exit_code: row.try_get("exit_code")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl Desk {
    fn terminal_view(&self, r: &TerminalRecord) -> TerminalView {
        let live = self.0.state.lock().terminal_ptys.contains_key(&r.id);
        TerminalView::of(r, live)
    }

    pub fn terminals(&self) -> Vec<TerminalView> {
        let st = self.0.state.lock();
        let mut all: Vec<_> = st
            .terminals
            .values()
            .map(|r| TerminalView::of(r, st.terminal_ptys.contains_key(&r.id)))
            .collect();
        all.sort_by_key(|t| t.created_at);
        all
    }

    pub fn terminal(&self, id: Uuid) -> Result<TerminalView, DeskError> {
        let r = self.terminal_record(id)?;
        Ok(self.terminal_view(&r))
    }

    fn terminal_record(&self, id: Uuid) -> Result<TerminalRecord, DeskError> {
        self.0
            .state
            .lock()
            .terminals
            .get(&id)
            .cloned()
            .ok_or(DeskError::NotFound)
    }

    /// Saves and announces a terminal.
    fn save_terminal(&self, mut r: TerminalRecord) {
        r.updated_at = Utc::now();
        self.0.state.lock().terminals.insert(r.id, r.clone());
        let view = self.terminal_view(&r);
        let _ = self.0.persist.send(Persist::Terminal(Box::new(r)));
        self.announce(DeskEvent::Terminal {
            terminal: Box::new(view),
        });
    }

    pub fn attach_terminal(&self, id: Uuid) -> Result<Arc<Pty>, DeskError> {
        self.0
            .state
            .lock()
            .terminal_ptys
            .get(&id)
            .cloned()
            .ok_or_else(|| DeskError::Conflict("terminal is not running".into()))
    }

    /// Opens a login shell in `cwd`.
    pub async fn create_terminal(&self, new: NewTerminal) -> Result<TerminalView, DeskError> {
        if !Path::new(&new.cwd).is_dir() {
            return Err(DeskError::Invalid(format!("{} is not a folder", new.cwd)));
        }
        check_env(&new.env)?;
        let id = new.id.unwrap_or_else(Uuid::new_v4);
        if self.0.state.lock().terminals.contains_key(&id) {
            return Err(DeskError::Conflict(format!(
                "there is already a terminal {id}"
            )));
        }
        let now = Utc::now();
        let r = TerminalRecord {
            id,
            name: new
                .name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| folder_name(&new.cwd)),
            cwd: new.cwd,
            env: new.env,
            shell: self.login_shell(),
            state: TerminalState::Running,
            exit_code: None,
            created_at: now,
            updated_at: now,
        };
        self.open_terminal(r).await
    }

    pub fn update_terminal(&self, id: Uuid, p: TerminalPatch) -> Result<TerminalView, DeskError> {
        let mut r = self.terminal_record(id)?;
        if let Some(name) = p.name {
            let name = name.trim();
            if name.is_empty() {
                return Err(DeskError::Invalid("a terminal needs a name".into()));
            }
            r.name = name.to_string();
        }
        self.save_terminal(r);
        self.terminal(id)
    }

    /// Opens a terminal that isn't running again, where it last was and
    /// with the environment it had.
    pub async fn reopen_terminal(&self, id: Uuid) -> Result<TerminalView, DeskError> {
        let mut r = self.terminal_record(id)?;
        if self.0.state.lock().terminal_ptys.contains_key(&id) {
            return Err(DeskError::Conflict("terminal is already running".into()));
        }
        if !Path::new(&r.cwd).is_dir() {
            // Where it was is gone: its home instead.
            r.cwd = self
                .0
                .env
                .get("HOME")
                .cloned()
                .unwrap_or_else(|| "/".into());
        }
        r.state = TerminalState::Running;
        r.exit_code = None;
        self.open_terminal(r).await
    }

    /// Ends a terminal's shell and forgets it.
    pub fn close_terminal(&self, id: Uuid) -> Result<(), DeskError> {
        let pty = {
            let mut st = self.0.state.lock();
            st.terminals.remove(&id).ok_or(DeskError::NotFound)?;
            st.terminal_ptys.remove(&id)
        };
        if let Some(pty) = pty {
            pty.kill();
        }
        let _ = self.0.persist.send(Persist::DeleteTerminal(id));
        self.announce(DeskEvent::TerminalRemoved { id });
        Ok(())
    }

    /// The shell terminals run: the login environment's `SHELL`.
    fn login_shell(&self) -> String {
        self.0
            .env
            .get("SHELL")
            .filter(|s| Path::new(s).is_file())
            .cloned()
            .unwrap_or_else(|| "/bin/zsh".into())
    }

    fn terminal_spawn(&self, r: &TerminalRecord) -> Spawn {
        let mut env = self.0.env.clone();
        env.extend(r.env.clone());
        env.insert("DESK_TERMINAL_ID".into(), r.id.to_string());
        Spawn {
            program: PathBuf::from(&r.shell),
            args: vec!["-l".into()],
            cwd: PathBuf::from(&r.cwd),
            env,
        }
    }

    async fn open_terminal(&self, r: TerminalRecord) -> Result<TerminalView, DeskError> {
        let spec = self.terminal_spawn(&r);
        let meta = serde_json::json!({ "kind": "terminal", "terminal": r.id });
        let (pty, exit) = self.start_pty(&spec, meta).await?;
        let id = r.id;
        self.0.state.lock().terminal_ptys.insert(id, Arc::new(pty));
        tokio::spawn(self.clone().on_terminal_exit(id, exit));
        self.save_terminal(r);
        tracing::info!(%id, "desk terminal opened");
        self.terminal(id)
    }

    async fn on_terminal_exit(self, id: Uuid, exit: oneshot::Receiver<Exit>) {
        let code = exit.await.ok().flatten();
        if self.0.cancel.is_cancelled() {
            return;
        }
        let pty = self.0.state.lock().terminal_ptys.remove(&id);
        let Some(pty) = pty else { return };
        let cwd = pty.cwd();
        let Ok(mut r) = self.terminal_record(id) else {
            return;
        };
        if let Some(cwd) = cwd {
            r.cwd = cwd;
        }
        r.state = TerminalState::Exited;
        r.exit_code = code.and_then(|c| i32::try_from(c).ok());
        self.save_terminal(r);
    }

    /// Loads the terminals; takes back the ones the holder kept and reopens
    /// (or, with restore off, marks lost) the ones that were running.
    pub(super) async fn adopt_terminals(
        &self,
        holder: Option<&Holder>,
        mut held: Vec<(Uuid, Held)>,
    ) {
        let records = match self.0.store.terminals().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "could not load desk terminals");
                return;
            }
        };
        {
            let mut st = self.0.state.lock();
            st.terminals = records.iter().map(|r| (r.id, r.clone())).collect();
        }
        let history = crate::pty::history_rows(self.0.opts.scrollback_bytes);
        for r in records {
            let kept = held.iter().position(|(id, _)| *id == r.id);
            match (kept, holder) {
                (Some(i), Some(holder)) => {
                    let (_, h) = held.swap_remove(i);
                    let (pty, exit) = holder.adopt(&h, history);
                    self.0
                        .state
                        .lock()
                        .terminal_ptys
                        .insert(r.id, Arc::new(pty));
                    tokio::spawn(self.clone().on_terminal_exit(r.id, exit));
                    self.save_terminal(TerminalRecord {
                        state: TerminalState::Running,
                        ..r
                    });
                }
                _ if r.state == TerminalState::Running => {
                    if self.0.opts.restore == RestorePolicy::Off {
                        self.save_terminal(TerminalRecord {
                            state: TerminalState::Lost,
                            ..r
                        });
                    } else if let Err(e) = self.reopen_terminal(r.id).await {
                        tracing::warn!(id = %r.id, error = %e, "desk terminal could not be reopened");
                        self.save_terminal(TerminalRecord {
                            state: TerminalState::Lost,
                            ..r
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some(holder) = holder {
            for (_, stray) in held {
                holder.adopt(&stray, 0).0.kill();
            }
        }
    }

    /// Follows each running shell's working directory, saving it when it
    /// changes.
    pub(super) async fn follow_cwds(self) {
        let mut tick = tokio::time::interval(CWD_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            self.note_cwds();
        }
    }

    pub(super) fn note_cwds(&self) {
        let running: Vec<(Uuid, Arc<Pty>)> = self
            .0
            .state
            .lock()
            .terminal_ptys
            .iter()
            .map(|(id, p)| (*id, Arc::clone(p)))
            .collect();
        for (id, pty) in running {
            let Some(cwd) = pty.cwd() else { continue };
            let Ok(r) = self.terminal_record(id) else {
                continue;
            };
            if r.cwd != cwd {
                self.save_terminal(TerminalRecord { cwd, ..r });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use crate::testing::{desk_on, options};
    use tokio_util::sync::CancellationToken;
    use workforce_postgres::test_support::with_test_pool;

    async fn wait_until(mut ok: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never happened");
    }

    #[test]
    fn environment_names_are_checked() {
        let env = |k: &str| BTreeMap::from([(k.to_string(), "v".to_string())]);
        assert!(check_env(&env("CLAUDE_CODE_PLUGIN_DIRS")).is_ok());
        assert!(check_env(&env("_X1")).is_ok());
        for bad in ["", "1X", "A-B", "A B"] {
            assert!(check_env(&env(bad)).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_terminal_carries_its_environment_follows_its_cwd_and_reopens_there() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("sub dir");
            std::fs::create_dir(&sub).unwrap();
            let mut opts = options(dir.path());
            opts.env = BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SHELL".into(), "/bin/sh".into()),
                ("HOME".into(), dir.path().to_string_lossy().into_owned()),
            ]);
            crate::store::DeskStore::migrate(&pool).await.unwrap();
            let cancel = CancellationToken::new();
            let d = Desk::start(
                crate::store::DeskStore::from_pool(pool.clone()),
                opts.clone(),
                cancel.clone(),
            )
            .await
            .unwrap();
            let id = Uuid::new_v4();
            let t = d
                .create_terminal(NewTerminal {
                    id: Some(id),
                    name: None,
                    cwd: dir.path().to_string_lossy().into_owned(),
                    env: BTreeMap::from([
                        ("CLAUDE_CODE_PLUGIN_DIRS".into(), "/plugins/desk".into()),
                        ("DESK_HOOK_TOKEN".into(), "tok".into()),
                    ]),
                })
                .await
                .unwrap();
            assert_eq!((t.id, t.live, t.state), (id, true, TerminalState::Running));
            assert_eq!(
                t.env_keys,
                vec!["CLAUDE_CODE_PLUGIN_DIRS", "DESK_HOOK_TOKEN"]
            );
            let pty = d.attach_terminal(id).unwrap();
            pty.write(
                format!(
                    "echo \"plug=$CLAUDE_CODE_PLUGIN_DIRS id=$DESK_TERMINAL_ID\"; cd '{}'\n",
                    sub.display()
                )
                .as_bytes(),
            )
            .unwrap();
            crate::pty::tests::wait_for(&pty, &format!("plug=/plugins/desk id={id}")).await;
            let want = sub.canonicalize().unwrap().to_string_lossy().into_owned();
            wait_until(|| {
                d.note_cwds();
                d.terminal(id).unwrap().cwd == want
            })
            .await;

            // A reboot: the hub and the shell are gone.
            cancel.cancel();
            pty.kill();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let store = crate::store::DeskStore::from_pool(pool.clone());
            let saved = store.terminals().await.unwrap();
            assert_eq!(
                (saved[0].cwd.as_str(), saved[0].state),
                (want.as_str(), TerminalState::Running)
            );
            assert_eq!(saved[0].env["DESK_HOOK_TOKEN"], "tok");

            let d = Desk::start(store, opts, CancellationToken::new())
                .await
                .unwrap();
            let back = d.terminal(id).unwrap();
            assert!(back.live, "reopened");
            let pty = d.attach_terminal(id).unwrap();
            pty.write(b"echo \"at=$(pwd) tok=$DESK_HOOK_TOKEN\"\n")
                .unwrap();
            crate::pty::tests::wait_for(&pty, &format!("at={want} tok=tok")).await;

            pty.write(b"exit 4\n").unwrap();
            wait_until(|| d.terminal(id).unwrap().state == TerminalState::Exited).await;
            assert_eq!(d.terminal(id).unwrap().exit_code, Some(4));
            d.close_terminal(id).unwrap();
            assert!(d.terminals().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn bad_terminals_are_refused() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk_on(pool, dir.path()).await;
            let new = |cwd: &str, env: &[(&str, &str)]| NewTerminal {
                id: None,
                name: None,
                cwd: cwd.into(),
                env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            };
            assert!(matches!(
                d.create_terminal(new("/no/such/dir", &[])).await,
                Err(DeskError::Invalid(_))
            ));
            let cwd = dir.path().to_string_lossy();
            assert!(matches!(
                d.create_terminal(new(&cwd, &[("A=B", "x")])).await,
                Err(DeskError::Invalid(_))
            ));
            assert!(matches!(
                d.reopen_terminal(Uuid::new_v4()).await,
                Err(DeskError::NotFound)
            ));
        })
        .await;
    }
}

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

use serde_json::Value;

use super::{Desk, DeskError, Live, Persist, RestorePolicy};
use crate::claude::{self, Hook};
use crate::holder::{Held, Holder};
use crate::model::DeskEvent;
use crate::model::{Approvals, Engine, Origin, SessionRecord, SessionState};
use crate::pty::{Exit, Pty, Spawn};
use crate::store::DeskStore;

/// How long a terminal's session may hold an approval open: Desk's hook
/// plugin gives its PermissionRequest hook a day.
pub(super) const TERMINAL_APPROVAL_WAIT: Duration = Duration::from_secs(86_400);
/// The variable Desk puts its hook plugin's folder in; ours is added to
/// any the login environment already has (`:`-separated, as Claude Code
/// splits it).
const PLUGIN_DIRS: &str = "CLAUDE_CODE_PLUGIN_DIRS";
/// The token a terminal's hooks present.
const HOOK_TOKEN: &str = "DESK_HOOK_TOKEN";

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
    /// The shell's process id while it runs.
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TerminalView {
    fn of(r: &TerminalRecord, pty: Option<&Arc<Pty>>) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            cwd: r.cwd.clone(),
            env_keys: r.env.keys().cloned().collect(),
            shell: r.shell.clone(),
            state: r.state,
            live: pty.is_some(),
            pid: pty.and_then(|p| p.pid()),
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
        let st = self.0.state.lock();
        TerminalView::of(r, st.terminal_ptys.get(&r.id))
    }

    pub fn terminals(&self) -> Vec<TerminalView> {
        let st = self.0.state.lock();
        let mut all: Vec<_> = st
            .terminals
            .values()
            .map(|r| TerminalView::of(r, st.terminal_ptys.get(&r.id)))
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
        let mut env = new.env;
        if env.get(HOOK_TOKEN).is_none_or(|t| t.trim().is_empty()) {
            env.insert(HOOK_TOKEN.into(), Uuid::new_v4().simple().to_string());
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
            env,
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
        // Desk's plugin folder joins any the person already loads.
        if let (Some(ours), Some(theirs)) = (
            r.env.get(PLUGIN_DIRS),
            self.0
                .opts
                .env
                .get(PLUGIN_DIRS)
                .filter(|v| !v.trim().is_empty()),
        ) {
            let mut dirs: Vec<&str> = theirs.split(':').filter(|d| !d.is_empty()).collect();
            for dir in ours.split(':').filter(|d| !d.is_empty()) {
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
            env.insert(PLUGIN_DIRS.into(), dirs.join(":"));
        }
        env.insert("DESK_TERMINAL_ID".into(), r.id.to_string());
        let token = r.env.get(HOOK_TOKEN).cloned().unwrap_or_default();
        env.insert("DESK_HOOK_BASE".into(), self.0.hook_base.clone());
        env.insert(
            "DESK_HOOK_URL".into(),
            format!("{}/hook/terminal/{}/{token}", self.0.hook_base, r.id),
        );
        Spawn {
            program: PathBuf::from(&r.shell),
            args: vec!["-l".into()],
            cwd: PathBuf::from(&r.cwd),
            env,
        }
    }

    async fn open_terminal(&self, r: TerminalRecord) -> Result<TerminalView, DeskError> {
        let spec = self.terminal_spawn(&r);
        let meta = serde_json::json!({
            "kind": "terminal",
            "terminal": r.id,
            "hookPort": self.0.hook_port,
        });
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
        // Whatever ran in it ended with it.
        let hosted: Vec<Uuid> = self
            .0
            .state
            .lock()
            .live
            .iter()
            .filter(|(_, l)| l.terminal == Some(id))
            .map(|(s, _)| *s)
            .collect();
        for session in hosted {
            self.end_terminal_session(session, false);
        }
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
        self.relink_terminal_sessions();
    }

    /// One hook call from a Claude Code session started in terminal
    /// `terminal` (Desk's hook plugin, loaded through the terminal's
    /// environment, posts to `DESK_HOOK_URL`). `None` when the terminal isn't
    /// running or the token isn't its own. SessionStart registers the
    /// session (or takes back the one with that `session_id`); from then on
    /// its hooks drive it exactly as a Desk-launched session's do.
    pub async fn terminal_hook(&self, terminal: Uuid, token: &str, body: &[u8]) -> Option<Value> {
        let pty = {
            let st = self.0.state.lock();
            let expected = st.terminals.get(&terminal)?.env.get(HOOK_TOKEN)?;
            if !crate::holder::same_token(expected, token) {
                return None;
            }
            Arc::clone(st.terminal_ptys.get(&terminal)?)
        };
        let fields: Value = serde_json::from_slice(body).unwrap_or_default();
        let hook = match claude::parse_hook(body) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(%terminal, error = %e, "unreadable Claude Code hook from a terminal");
                return Some(Value::Object(Default::default()));
            }
        };
        let cli = fields["session_id"].as_str().map(str::to_string);
        let cwd = fields["cwd"].as_str().map(str::to_string);
        let found = self.terminal_session(terminal, cli.as_deref());
        let id = match (&hook, found) {
            (Hook::SessionStart { .. }, _) | (_, None) => {
                if matches!(hook, Hook::SessionEnd) {
                    return Some(Value::Object(Default::default()));
                }
                self.register_terminal_session(terminal, pty, cli, cwd)
            }
            (_, Some(id)) => id,
        };
        let ending = matches!(hook, Hook::SessionEnd);
        let answer = self.apply_hook(id, hook).await;
        if ending {
            self.end_terminal_session(id, true);
        }
        Some(answer)
    }

    /// The running session in `terminal` with this Claude Code session id
    /// (any, when the hook didn't say).
    fn terminal_session(&self, terminal: Uuid, cli: Option<&str>) -> Option<Uuid> {
        let st = self.0.state.lock();
        st.live
            .iter()
            .filter(|(_, l)| l.terminal == Some(terminal))
            .map(|(id, _)| *id)
            .find(|id| {
                cli.is_none()
                    || st
                        .records
                        .get(id)
                        .is_some_and(|r| r.cli_session_id.as_deref() == cli)
            })
    }

    fn register_terminal_session(
        &self,
        terminal: Uuid,
        pty: Arc<Pty>,
        cli: Option<String>,
        cwd: Option<String>,
    ) -> Uuid {
        // One Claude Code at a time in a terminal: one still marked running
        // there under another id has gone.
        let others: Vec<Uuid> = {
            let st = self.0.state.lock();
            st.live
                .iter()
                .filter(|(_, l)| l.terminal == Some(terminal))
                .map(|(id, _)| *id)
                .filter(|id| {
                    st.records.get(id).and_then(|r| r.cli_session_id.as_deref()) != cli.as_deref()
                })
                .collect()
        };
        for other in others {
            self.end_terminal_session(other, true);
        }
        let (existing, term) = {
            let st = self.0.state.lock();
            let existing = cli.as_deref().and_then(|c| {
                st.records
                    .values()
                    .find(|r| r.engine == Engine::Claude && r.cli_session_id.as_deref() == Some(c))
                    .map(|r| r.id)
            });
            (existing, st.terminals.get(&terminal).cloned())
        };
        let token = term
            .as_ref()
            .and_then(|t| t.env.get(HOOK_TOKEN).cloned())
            .unwrap_or_default();
        let id = existing.unwrap_or_else(Uuid::new_v4);
        let live = Live {
            pty,
            token,
            current_tool: None,
            pending: None,
            answer: None,
            ended: false,
            stopping: false,
            _codex: None,
            terminal: Some(terminal),
        };
        if existing.is_some_and(|id| self.0.state.lock().live.contains_key(&id)) {
            // Already running here: nothing to take back.
            return id;
        }
        self.0.state.lock().live.insert(id, live);
        match existing {
            Some(id) => self.change(id, |r, _| {
                r.origin = Origin::Desk;
                r.terminal_id = Some(terminal);
                r.external_pid = None;
                r.restore_offer = false;
                r.state = SessionState::Starting;
                if let Some(cwd) = cwd {
                    r.cwd = cwd;
                }
            }),
            None => {
                let now = Utc::now();
                let cwd = cwd
                    .or_else(|| term.as_ref().map(|t| t.cwd.clone()))
                    .unwrap_or_default();
                self.save(SessionRecord {
                    id,
                    engine: Engine::Claude,
                    name: term.map_or_else(|| folder_name(&cwd), |t| t.name),
                    goal: String::new(),
                    cwd,
                    branch: None,
                    origin: Origin::Desk,
                    state: SessionState::Starting,
                    approvals: Approvals::Desk,
                    cli_session_id: cli,
                    transcript: None,
                    last_prompt: None,
                    last_reply: None,
                    external_pid: None,
                    restore_offer: false,
                    terminal_id: Some(terminal),
                    created_at: now,
                    updated_at: now,
                });
            }
        }
        tracing::info!(%id, %terminal, "a session started in a desk terminal");
        id
    }

    /// A session in a terminal is no longer running: it ended there
    /// (`clean`), or the terminal itself went.
    fn end_terminal_session(&self, id: Uuid, clean: bool) {
        let live = self.0.state.lock().live.remove(&id);
        if live.is_none() {
            return;
        }
        drop(live);
        self.change(id, |r, _| {
            r.state = if clean {
                SessionState::Stopped
            } else {
                r.state.after_loss()
            };
        });
    }

    /// After a restart: sessions running in terminals the holder kept are
    /// running again; ones whose terminal didn't survive are not.
    fn relink_terminal_sessions(&self) {
        let hosted: Vec<(Uuid, Uuid)> = self
            .0
            .state
            .lock()
            .records
            .values()
            .filter(|r| r.origin == Origin::Desk && r.state.is_running())
            .filter_map(|r| Some((r.id, r.terminal_id?)))
            .collect();
        for (id, terminal) in hosted {
            let found = {
                let st = self.0.state.lock();
                st.terminal_ptys.get(&terminal).cloned().map(|pty| {
                    let token = st
                        .terminals
                        .get(&terminal)
                        .and_then(|t| t.env.get(HOOK_TOKEN).cloned())
                        .unwrap_or_default();
                    (pty, token)
                })
            };
            match found {
                Some((pty, token)) => {
                    self.0.state.lock().live.insert(
                        id,
                        Live {
                            pty,
                            token,
                            current_tool: None,
                            pending: None,
                            answer: None,
                            ended: false,
                            stopping: false,
                            _codex: None,
                            terminal: Some(terminal),
                        },
                    );
                    self.publish(id);
                }
                None => self.change(id, |r, _| r.state = r.state.after_loss()),
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

    /// A desk whose terminals run `/bin/sh`, with a plugin folder of the
    /// person's own in its login environment.
    async fn shell_desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
        use std::os::unix::fs::PermissionsExt;
        let mut opts = options(dir);
        let cli = dir.join("fake-claude");
        std::fs::write(&cli, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        opts.claude = cli;
        opts.env = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("SHELL".into(), "/bin/sh".into()),
            ("CLAUDE_CODE_PLUGIN_DIRS".into(), "/mine".into()),
        ]);
        crate::store::DeskStore::migrate(&pool).await.unwrap();
        Desk::start(
            crate::store::DeskStore::from_pool(pool),
            opts,
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    async fn open(d: &Desk, dir: &Path) -> (Uuid, String) {
        let t = d
            .create_terminal(NewTerminal {
                id: None,
                name: Some("api".into()),
                cwd: dir.to_string_lossy().into_owned(),
                env: BTreeMap::from([
                    ("CLAUDE_CODE_PLUGIN_DIRS".into(), "/plugins/desk".into()),
                    ("DESK_HOOK_TOKEN".into(), "hook-token-1".into()),
                ]),
            })
            .await
            .unwrap();
        (t.id, "hook-token-1".into())
    }

    fn hook(event: &str, session: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut body = serde_json::json!({ "hook_event_name": event, "session_id": session });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        body.to_string().into_bytes()
    }

    #[tokio::test]
    async fn a_claude_started_in_a_terminal_is_tracked_through_its_hooks() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = shell_desk(pool, dir.path()).await;
            let (tid, token) = open(&d, dir.path()).await;
            assert!(d.terminal(tid).unwrap().pid.is_some());

            // The shell carries the hook URL, and Desk's plugins after the person's own.
            let pty = d.attach_terminal(tid).unwrap();
            pty.write(b"echo \"url=$DESK_HOOK_URL plugins=$CLAUDE_CODE_PLUGIN_DIRS\"\n").unwrap();
            let want = format!("url={}/hook/terminal/{tid}/{token} plugins=/mine:/plugins/desk", d.0.hook_base);
            crate::pty::tests::wait_for(&pty, &want).await;

            // SessionStart, posted where the shell was told to.
            let repo = dir.path().to_string_lossy().into_owned();
            let url = format!("{}/hook/terminal/{tid}/{token}", d.0.hook_base);
            let started = reqwest::Client::new()
                .post(&url)
                .body(hook("SessionStart", "cc-1", serde_json::json!({ "cwd": repo })))
                .send()
                .await
                .unwrap();
            assert!(started.status().is_success());
            let s = d.list().into_iter().find(|s| s.terminal_id == Some(tid)).unwrap();
            assert_eq!(
                (s.origin, s.state, s.live, s.cli_session_id.as_deref(), s.cwd.as_str(), s.name.as_str()),
                (Origin::Desk, SessionState::Idle, true, Some("cc-1"), repo.as_str(), "api")
            );
            assert!(d.stop(s.id, None).is_err(), "ended in its terminal, not by Desk");

            // An approval goes round Desk as a launched session's does.
            let waiting = tokio::spawn({
                let d = d.clone();
                let token = token.clone();
                async move {
                    let body = hook("PermissionRequest", "cc-1",
                        serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf target" } }));
                    d.terminal_hook(tid, &token, &body).await.unwrap()
                }
            });
            let mut pending = None;
            wait_until(|| {
                pending = d.view(s.id).unwrap().pending;
                pending.is_some()
            })
            .await;
            let pending = pending.unwrap();
            assert!(pending.answerable);
            d.decide(s.id, pending.id, crate::model::Decision::Allow, None).unwrap();
            assert_eq!(waiting.await.unwrap(), claude::allow());

            // The adopter sees the same claude and leaves it alone.
            let found = crate::adopt::parse(
                serde_json::json!([{ "pid": std::process::id(), "cwd": "/repo", "kind": "interactive",
                    "sessionId": "cc-1", "status": "busy" }]).to_string().as_bytes(),
            ).unwrap();
            d.reconcile_external(found).await;
            assert_eq!(d.list().iter().filter(|v| v.cli_session_id.as_deref() == Some("cc-1")).count(), 1);
            assert_eq!(d.view(s.id).unwrap().origin, Origin::Desk);

            // It ends; started again with the same id it is the same session.
            d.terminal_hook(tid, &token, &hook("SessionEnd", "cc-1", serde_json::json!({}))).await.unwrap();
            let v = d.view(s.id).unwrap();
            assert_eq!((v.live, v.state), (false, SessionState::Stopped));
            d.terminal_hook(tid, &token, &hook("SessionStart", "cc-1", serde_json::json!({}))).await.unwrap();
            let v = d.view(s.id).unwrap();
            assert_eq!((v.live, v.state, v.terminal_id), (true, SessionState::Idle, Some(tid)));

            // A new conversation in the same terminal is a new session; the old one stops.
            d.terminal_hook(tid, &token, &hook("SessionStart", "cc-2", serde_json::json!({}))).await.unwrap();
            assert!(!d.view(s.id).unwrap().live);
            assert_eq!(d.list().iter().filter(|v| v.terminal_id == Some(tid) && v.live).count(), 1);

            // Resumed from Desk, the old one runs as Desk's own, out of the terminal.
            let resumed = d.resume(s.id, None, None).await.unwrap();
            assert_eq!((resumed.live, resumed.terminal_id), (true, None));
            d.forget(s.id).unwrap();

            // Wrong token, unknown terminal: refused.
            assert!(d.terminal_hook(tid, "nope", &hook("SessionStart", "x", serde_json::json!({}))).await.is_none());
            assert!(d.terminal_hook(Uuid::new_v4(), &token, &hook("SessionStart", "x", serde_json::json!({}))).await.is_none());
            let refused = reqwest::Client::new().post(format!("{}/hook/terminal/{tid}/nope", d.0.hook_base))
                .body(hook("Stop", "cc-2", serde_json::json!({}))).send().await.unwrap();
            assert_eq!(refused.status(), reqwest::StatusCode::NOT_FOUND);

            // The terminal going ends what ran in it.
            pty.kill();
            wait_until(|| d.list().iter().all(|v| !v.live)).await;
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_the_adopter_found_first_is_taken_over_by_its_terminal() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = shell_desk(pool, dir.path()).await;
            let (tid, token) = open(&d, dir.path()).await;
            let found = crate::adopt::parse(
                serde_json::json!([{ "pid": std::process::id(), "cwd": "/repo", "kind": "interactive",
                    "sessionId": "cc-9", "name": "early", "status": "idle" }]).to_string().as_bytes(),
            ).unwrap();
            d.reconcile_external(found.clone()).await;
            let early = d.list().into_iter().find(|s| s.name == "early").unwrap();
            assert_eq!(early.origin, Origin::External);
            d.terminal_hook(tid, &token, &hook("SessionStart", "cc-9", serde_json::json!({}))).await.unwrap();
            let v = d.view(early.id).unwrap();
            assert_eq!((v.origin, v.live, v.terminal_id), (Origin::Desk, true, Some(tid)));
            d.reconcile_external(found).await;
            assert_eq!(d.view(early.id).unwrap().origin, Origin::Desk);
            assert_eq!(d.list().len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn a_held_terminal_and_the_claude_in_it_come_back_after_a_hub_restart() {
        with_test_pool(build_pool, |pool| async move {
            let (_hdir, socket, holder_cancel) = crate::holder::tests::holder().await;
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
            opts.env = BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SHELL".into(), "/bin/sh".into()),
            ]);
            opts.holder = Some(super::super::HolderOptions {
                socket,
                token: crate::holder::tests::TOKEN.into(),
            });
            crate::store::DeskStore::migrate(&pool).await.unwrap();
            let cancel = CancellationToken::new();
            let d = Desk::start(
                crate::store::DeskStore::from_pool(pool.clone()),
                opts.clone(),
                cancel.clone(),
            )
            .await
            .unwrap();
            let (tid, token) = open(&d, dir.path()).await;
            let pid = d.terminal(tid).unwrap().pid;
            d.terminal_hook(
                tid,
                &token,
                &hook("SessionStart", "cc-5", serde_json::json!({})),
            )
            .await
            .unwrap();
            d.terminal_hook(
                tid,
                &token,
                &hook(
                    "UserPromptSubmit",
                    "cc-5",
                    serde_json::json!({ "prompt": "go" }),
                ),
            )
            .await
            .unwrap();
            let s = d
                .list()
                .into_iter()
                .find(|s| s.terminal_id == Some(tid))
                .unwrap();
            let port = d.0.hook_port;
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
            tokio::time::sleep(Duration::from_millis(300)).await;

            let d = Desk::start(
                crate::store::DeskStore::from_pool(pool),
                opts,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let t = d.terminal(tid).unwrap();
            assert_eq!((t.live, t.pid), (true, pid), "the same shell");
            let v = d.view(s.id).unwrap();
            assert_eq!((v.live, v.state), (true, SessionState::Working));
            assert_eq!(d.0.hook_port, port);
            d.terminal_hook(
                tid,
                &token,
                &hook(
                    "Stop",
                    "cc-5",
                    serde_json::json!({ "last_assistant_message": "Done." }),
                ),
            )
            .await
            .unwrap();
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);
            d.close_terminal(tid).unwrap();
            holder_cancel.cancel();
        })
        .await;
    }
}

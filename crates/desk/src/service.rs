//! The desk: every session the hub hosts or has seen, their live terminals,
//! and the registry that lets them come back after a restart.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use eyre::Result;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::adopt::{self, External};
use crate::background::{Background, TaskKind};
use crate::claude::{self, Hook};
use crate::codex::{self, AppServer, CodexEvent, Seen};
use crate::crew::{self, By, Event, Kind, Party};
use crate::floor::{Outcome, TurnLog, TurnOp};
use crate::holder::{Held, Holder};
use crate::model::{
    Approvals, Decision, DeskEvent, Engine, NewSession, Origin, PendingApproval, SessionPatch,
    SessionRecord, SessionState, SessionView,
};
use crate::procs;
use crate::pty::{Exit, Pty};
use crate::rooms::{
    channel_for, place_in_timeline, slug_for, Member, NewMessage, NewRoom, Posted, Room,
    RoomMessage, RoomPatch, SearchHit, DESK, MAX_HOPS, THREAD_GONE, TURN_BUDGET, YOU,
};
use crate::store::DeskStore;
use crate::transcript;
use crate::usage::{self, MacPlan, MacSignIns, PlanSink};
use crate::worktree;

mod groups;
mod terminals;
mod transition;

use transition::Transition;

pub use groups::{
    local_agent_item, session_item, Group, Import, Imported, Layout, Ungrouped, UNGROUPED,
};

pub use terminals::{NewTerminal, TerminalPatch, TerminalRecord, TerminalState, TerminalView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePolicy {
    Ask,
    Auto,
    Off,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub claude: PathBuf,
    pub codex: PathBuf,
    /// Environment the CLIs run with: the user's login environment.
    /// `CLAUDE*` variables are removed before use.
    pub env: BTreeMap<String, String>,
    /// Where per-session sockets live (keep the path short: macOS caps
    /// socket paths at 104 bytes).
    pub run_dir: PathBuf,
    pub scrollback_bytes: usize,
    pub approval_timeout: Duration,
    pub restore: RestorePolicy,
    /// How often other terminals' Claude Code sessions are looked for;
    /// `None` turns adoption off.
    pub adopt_every: Option<Duration>,
    /// The PTY holder that keeps terminals across hub restarts; without
    /// one (or while it can't be reached) they are the hub's own children.
    pub holder: Option<HolderOptions>,
}

#[derive(Debug, Clone)]
pub struct HolderOptions {
    pub socket: PathBuf,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DeskError {
    #[error("no such session")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] eyre::Report),
}

type Answer = oneshot::Sender<Value>;

pub(crate) struct Live {
    pub(crate) pty: Arc<Pty>,
    token: String,
    current_tool: Option<String>,
    pending: Option<PendingApproval>,
    answer: Option<Answer>,
    ended: bool,
    stopping: bool,
    _codex: Option<CodexLink>,
    /// The Desk terminal it runs in: its PTY is the terminal's, which
    /// outlives it.
    terminal: Option<Uuid>,
    /// What it started that still runs.
    background: Background,
    /// What already ran under its Codex app-server when its turn began.
    baseline: Option<HashSet<u32>>,
}

impl Live {
    /// Ends the session's processes, wherever they run (a terminal's shell
    /// is the terminal's to end).
    fn end(&self) {
        if self.terminal.is_none() {
            self.pty.kill();
        }
        if let Some(Server::Held(server)) = self._codex.as_ref().map(|c| &c.server) {
            server.kill();
        }
    }
}

/// A Codex session's app-server: the hub's child, or the holder's.
enum Server {
    Local(AppServer),
    Held(Arc<Pty>),
}

impl Server {
    fn socket(&self, fallback: &Path) -> PathBuf {
        match self {
            Self::Local(s) => s.socket.clone(),
            Self::Held(_) => fallback.to_path_buf(),
        }
    }

    fn pid(&self) -> Option<u32> {
        match self {
            Self::Local(s) => s.pid(),
            Self::Held(pty) => pty.pid(),
        }
    }
}

/// A Codex session's app-server and observer; dropping it stops the
/// observer, and the app-server when it is the hub's own.
struct CodexLink {
    server: Server,
    watch: CancellationToken,
}

impl Drop for CodexLink {
    fn drop(&mut self) {
        self.watch.cancel();
    }
}

#[derive(Default)]
struct State {
    records: HashMap<Uuid, SessionRecord>,
    rooms: HashMap<Uuid, Room>,
    live: HashMap<Uuid, Live>,
    /// Transcript mtimes already read for adopted sessions.
    seen_transcripts: HashMap<Uuid, SystemTime>,
    /// When each session came into the state it is in.
    state_since: HashMap<Uuid, DateTime<Utc>>,
    /// When each session's last `working` event was recorded.
    working_noted: HashMap<Uuid, std::time::Instant>,
    /// Sessions whose next stop or resume someone's action already recorded.
    acted: HashSet<Uuid>,
    /// Requests Desk answered, until the change that clears them is seen.
    decided: HashSet<Uuid>,
    /// The turn each session has open in the floor's log.
    turns: HashMap<Uuid, OpenTurn>,
    /// What Desk last asked its sessions' environment to add.
    session_env: BTreeMap<String, String>,
    terminals: HashMap<Uuid, TerminalRecord>,
    terminal_ptys: HashMap<Uuid, Arc<Pty>>,
    /// The sidebar's groups.
    layout: Layout,
}

/// How often a busy session's tool is recorded as `working`.
const WORKING_EVERY: Duration = Duration::from_secs(60);

/// How often a busy session's progress is written to the floor's log.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
struct OpenTurn {
    id: Uuid,
    progressed: Option<std::time::Instant>,
}

/// Where the desk reports what it sees beyond its own registry: the floor's
/// turn log, and this Mac's plan readings.
#[derive(Clone, Default)]
pub struct Floor {
    pub turns: Option<Arc<dyn TurnLog>>,
    pub plans: Option<Arc<dyn PlanSink>>,
}

/// How often sessions' background work is looked at.
const LOOK_EVERY: Duration = Duration::from_secs(10);

/// How often this Mac's sign-ins are read again.
const SIGN_INS_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long a CLI may take to say who it is signed in as.
const SIGN_IN_WAIT: Duration = Duration::from_secs(20);

enum Persist {
    Terminal(Box<TerminalRecord>),
    DeleteTerminal(Uuid),
    Put(Box<SessionRecord>),
    Delete(Uuid),
    Event(Box<Event>),
    Turn(Box<TurnOp>),
}

struct Inner {
    opts: Options,
    store: Arc<DeskStore>,
    env: BTreeMap<String, String>,
    state: Mutex<State>,
    events: broadcast::Sender<DeskEvent>,
    persist: mpsc::UnboundedSender<Persist>,
    hook_base: String,
    hook_port: u16,
    cancel: CancellationToken,
    holder: Mutex<Option<Holder>>,
    layout_write: tokio::sync::Mutex<()>,
    plans: Option<Arc<dyn PlanSink>>,
    sign_ins: Mutex<MacSignIns>,
}

#[derive(Clone)]
pub struct Desk(Arc<Inner>);

/// One of the hub's own secrets, which Desk Helper or the person's setup
/// hands the hub (its admin token, JWT secret, vault key, the PTY holder's
/// token, its Postgres password): a `WORKFORCE_`/`DESK_` name that says
/// it is a token, secret, key or password. The person's own keys (say
/// `ANTHROPIC_API_KEY`) are theirs and carry on; a terminal's
/// `DESK_HOOK_TOKEN` is added after this, per terminal.
fn hub_secret(name: &str) -> bool {
    const SECRET: [&str; 5] = ["TOKEN", "SECRET", "KEY", "PASSWORD", "CREDENTIAL"];
    (name.starts_with("WORKFORCE_") || name.starts_with("DESK_"))
        && name
            .split('_')
            .any(|part| SECRET.iter().any(|s| part.starts_with(s)))
}

/// The variable Desk puts its hook plugin's folder in; ours is added to
/// any the login environment already has (`:`-separated, as Claude Code
/// splits it).
const PLUGIN_DIRS: &str = "CLAUDE_CODE_PLUGIN_DIRS";
/// Most variables Desk may add to a CLI's environment.
const MAX_ENV: usize = 64;
const MAX_ENV_VALUE: usize = 32 * 1024;

fn check_env(env: &BTreeMap<String, String>) -> Result<(), DeskError> {
    if env.len() > MAX_ENV {
        return Err(DeskError::Invalid(format!(
            "an environment adds at most {MAX_ENV} variables"
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

/// The person's plugin folders, then Desk's, each once; `None` for none.
fn plugin_dirs(theirs: Option<&String>, ours: Option<&String>) -> Option<String> {
    let mut dirs: Vec<&str> = Vec::new();
    for dir in [theirs, ours]
        .into_iter()
        .flatten()
        .flat_map(|v| v.split(':'))
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    (!dirs.is_empty()).then(|| dirs.join(":"))
}

/// "1 background task", "2 background tasks".
fn background_tasks(count: u32) -> String {
    match count {
        1 => "1 background task".into(),
        n => format!("{n} background tasks"),
    }
}

/// How a session's pending request reads in Activity.
fn asking(p: &PendingApproval) -> String {
    let name = p.tool.split('(').next().unwrap_or(&p.tool);
    let detail = if p.detail.trim().is_empty() {
        p.tool.clone()
    } else {
        p.detail.clone()
    };
    match name {
        "Input" => format!("is asking: {}", crew::clip(&p.detail, 160)),
        "Bash" => format!("wants to run {}", crew::clip(&detail, 160)),
        "Edit" | "Write" | "MultiEdit" => format!("wants to edit {}", crew::clip(&detail, 160)),
        _ if !p.answerable && p.detail == "Answer in the terminal" => {
            "is waiting for an answer in its terminal".into()
        }
        _ => format!("wants to use {}", crew::clip(&p.tool, 160)),
    }
}

/// The environment sessions, terminals and the CLIs Desk runs start from:
/// the hub's, without what a parent Claude Code left in it (`CLAUDE*`) and
/// without the hub's own secrets.
fn cli_env(base: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .iter()
        .filter(|(k, _)| !k.starts_with("CLAUDE") && !hub_secret(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env
}

/// The layout without sessions the registry no longer has (forgotten
/// before the hub took them out of it, or by an older hub). A failure to
/// save it is only logged: the next start tries again.
async fn prune_layout(
    store: &DeskStore,
    layout: Layout,
    records: &HashMap<Uuid, SessionRecord>,
) -> Layout {
    let pruned = layout.without_sessions(|id| !records.contains_key(&id));
    if pruned == layout {
        return layout;
    }
    match store.put_layout(&pruned).await {
        Ok(()) => pruned,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't take forgotten sessions out of the sidebar's layout");
            layout
        }
    }
}

impl Desk {
    /// Loads the registry, marks what was running as interrupted or stopped
    /// (its processes did not survive the hub), and starts the hook endpoint
    /// and the adoption poller. `cancel` stops all of it and every session.
    pub async fn start(
        store: DeskStore,
        opts: Options,
        floor: Floor,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let store = Arc::new(store);
        let layout = store.layout().await?;
        let (holder, held) = connect_holder(opts.holder.as_ref()).await;
        let rooms = store
            .rooms()
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();
        let mut records = HashMap::new();
        let mut recovered = Vec::new();
        for mut r in store.all().await? {
            let lost = r.state.is_running()
                && match r.origin {
                    Origin::Desk => match r.terminal_id {
                        // Runs in a terminal: lost with it.
                        Some(t) => !held.terminals.contains_key(&t),
                        None => !held.sessions.contains_key(&r.id),
                    },
                    Origin::External => !r.external_pid.is_some_and(adopt::alive),
                };
            if lost {
                r.state = r.state.after_loss();
                // One started in a terminal is started there again, not by Desk.
                r.restore_offer = opts.restore != RestorePolicy::Off && r.terminal_id.is_none();
                r.updated_at = Utc::now();
                recovered.push(r.clone());
            }
            records.insert(r.id, r);
        }
        let layout = prune_layout(&store, layout, &records).await;

        let listener = hook_listener(held.hook_port).await?;
        let hook_port = listener.local_addr()?.port();
        let hook_base = format!("http://127.0.0.1:{hook_port}");
        let (persist, persist_rx) = mpsc::unbounded_channel();
        let desk = Self(Arc::new(Inner {
            env: cli_env(&opts.env),
            opts,
            store: Arc::clone(&store),
            state: Mutex::new(State {
                records,
                rooms,
                layout,
                ..State::default()
            }),
            events: broadcast::channel(256).0,
            persist,
            hook_base,
            hook_port,
            cancel: cancel.clone(),
            holder: Mutex::new(holder),
            layout_write: tokio::sync::Mutex::new(()),
            plans: floor.plans,
            sign_ins: Mutex::new(MacSignIns::default()),
        }));
        tokio::spawn(persist_loop(store, floor.turns, persist_rx));
        tokio::spawn(desk.clone().read_sign_ins());
        tokio::spawn(desk.clone().watch_background());
        recovered.into_iter().for_each(|r| desk.save(r));
        desk.adopt_held(held).await;

        let hooks = crate::api::hook_router(desk.clone());
        let shutdown = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, hooks)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
            {
                tracing::error!(error = %e, "desk hook endpoint stopped");
            }
        });
        if let Some(every) = desk.0.opts.adopt_every {
            tokio::spawn(desk.clone().adopt_loop(every));
        }
        if desk.0.opts.restore == RestorePolicy::Auto {
            let ids = desk
                .restore_offer()
                .into_iter()
                .map(|s| s.id)
                .collect::<Vec<_>>();
            desk.restore(&ids, None).await;
        }
        tokio::spawn({
            let desk = desk.clone();
            async move {
                cancel.cancelled().await;
                desk.shutdown();
            }
        });
        Ok(desk)
    }

    /// Lets go of every session: the hub's own processes end with it, the
    /// holder's carry on for the next hub to take back.
    fn shutdown(&self) {
        let (live, terminals): (Vec<_>, Vec<_>) = {
            let mut st = self.0.state.lock();
            (
                st.live.drain().collect(),
                st.terminal_ptys.drain().collect(),
            )
        };
        drop(live);
        drop(terminals);
        if let Some(holder) = self.0.holder.lock().take() {
            holder.close();
        }
    }

    /// The holder, reconnecting if it went away; `None` without one.
    pub(crate) async fn holder(&self) -> Option<Holder> {
        let current = self.0.holder.lock().clone();
        if let Some(h) = current.filter(Holder::is_connected) {
            return Some(h);
        }
        let opts = self.0.opts.holder.as_ref()?;
        match Holder::connect(&opts.socket, &opts.token).await {
            Ok(h) => {
                tracing::info!("reconnected to the PTY holder");
                *self.0.holder.lock() = Some(h.clone());
                Some(h)
            }
            Err(e) => {
                tracing::warn!(error = %e, "PTY holder unavailable; this terminal runs in the hub");
                None
            }
        }
    }

    /// Starts `spec` in the holder when there is one, else as the hub's
    /// own child.
    pub(crate) async fn start_pty(
        &self,
        spec: &crate::pty::Spawn,
        meta: Value,
    ) -> Result<(Pty, oneshot::Receiver<Exit>), DeskError> {
        let history = crate::pty::history_rows(self.0.opts.scrollback_bytes);
        let (cols, rows) = (crate::pty::DEFAULT_COLS, crate::pty::DEFAULT_ROWS);
        if let Some(holder) = self.holder().await {
            return Ok(holder.spawn(spec, cols, rows, history, meta).await?);
        }
        Ok(Pty::spawn(spec, cols, rows, history)?)
    }

    /// Takes back the sessions whose processes the holder kept while the
    /// hub was away, as they were: same processes, same hook tokens.
    async fn adopt_held(&self, mut held: HeldSet) {
        let holder = self.0.holder.lock().clone();
        let terminals = std::mem::take(&mut held.terminals).into_iter().collect();
        self.adopt_terminals(holder.as_ref(), terminals).await;
        tokio::spawn(self.clone().follow_cwds());
        let Some(holder) = holder else {
            return;
        };
        let history = crate::pty::history_rows(self.0.opts.scrollback_bytes);
        for (id, h) in std::mem::take(&mut held.sessions) {
            let known = self.0.state.lock().records.get(&id).cloned();
            let Some(record) = known else {
                held.strays.push(h);
                continue;
            };
            let (pty, exit) = holder.adopt(&h, history);
            let codex = match held.servers.remove(&id) {
                Some(server) if record.engine == Engine::Codex => {
                    let socket = server.meta["socket"].as_str().map(PathBuf::from);
                    let (server_pty, _) = holder.adopt(&server, history);
                    let server = Server::Held(Arc::new(server_pty));
                    socket.map(|socket| self.observe_codex(id, server, socket))
                }
                other => {
                    held.strays.extend(other);
                    None
                }
            };
            self.0.state.lock().live.insert(
                id,
                Live {
                    pty: Arc::new(pty),
                    token: h.meta["token"].as_str().unwrap_or_default().to_string(),
                    current_tool: None,
                    // An approval the old hub held died with it: the
                    // terminal asks now.
                    pending: (record.state == SessionState::Blocked).then(|| PendingApproval {
                        id: Uuid::new_v4(),
                        tool: "Input".into(),
                        detail: "Answer in the terminal".into(),
                        since: Utc::now(),
                        answerable: false,
                    }),
                    answer: None,
                    ended: false,
                    stopping: false,
                    _codex: codex,
                    terminal: None,
                    background: Default::default(),
                    baseline: Default::default(),
                },
            );
            tokio::spawn(self.clone().on_exit(id, exit));
            if !record.state.is_running() {
                self.change(id, |r, _| r.state = SessionState::Idle);
            } else {
                self.publish(id);
            }
            tracing::info!(%id, "desk session taken back from the PTY holder");
        }
        held.strays.extend(held.servers.into_values());
        for stray in held.strays {
            if stray.exited {
                holder.reap(stray.id);
            } else {
                tracing::info!(id = %stray.id, "ending a held terminal no session owns");
                holder.adopt(&stray, 0).0.kill();
            }
        }
    }

    fn view_of(state: &State, r: &SessionRecord) -> SessionView {
        let live = state.live.get(&r.id);
        let mut view = SessionView::new(
            r,
            live.is_some(),
            live.and_then(|l| l.current_tool.clone()),
            live.and_then(|l| l.pending.clone()),
        );
        view.group = Self::group_ref(&state.layout, &groups::session_item(r.id));
        view.background = live.and_then(|l| l.background.view());
        view
    }

    fn save(&self, r: SessionRecord) {
        let mut st = self.0.state.lock();
        let before = st.records.get(&r.id).map(|old| Self::view_of(&st, old));
        self.saved(&mut st, before, r);
    }

    /// Saves `r`, recording what changed since `before` as activity, under
    /// the lock it was changed under: what it writes leaves in the order the
    /// session changed.
    fn saved(&self, st: &mut State, before: Option<SessionView>, r: SessionRecord) {
        st.records.insert(r.id, r.clone());
        let view = Self::view_of(st, &r);
        if before.as_ref().is_none_or(|b| b.state != view.state) {
            st.state_since.insert(r.id, view.updated_at);
        }
        let transitions = Transition::of(before.as_ref(), &view, &st.decided);
        transitions.iter().for_each(|t| {
            if let Transition::Unblocked { pending, .. } = t {
                st.decided.remove(&pending.id);
            }
        });
        let events = Self::session_events(st, &transitions, &view);
        let ops = Self::turn_ops(st, &transitions, &view);
        let _ = self.0.persist.send(Persist::Put(Box::new(r)));
        let _ = self.0.events.send(DeskEvent::Session {
            session: Box::new(view),
        });
        events.into_iter().for_each(|e| self.note(e));
        self.log_turns(ops);
    }

    fn log_turns(&self, ops: Vec<TurnOp>) {
        ops.into_iter().for_each(|op| {
            let _ = self.0.persist.send(Persist::Turn(Box::new(op)));
        });
    }

    /// The floor's writes for a session's change, keeping its open turn and
    /// writing its progress no more often than [`PROGRESS_EVERY`].
    fn turn_ops(st: &mut State, transitions: &[Transition<'_>], now: &SessionView) -> Vec<TurnOp> {
        let open = st.turns.get(&now.id).copied();
        let (ops, after) = transition::turn_ops(
            transitions,
            now,
            open.map(|o| o.id),
            now.updated_at,
            Uuid::new_v4,
        );
        let progressed = open
            .filter(|o| Some(o.id) == after)
            .and_then(|o| o.progressed);
        let due = progressed.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY);
        let ops: Vec<TurnOp> = ops
            .into_iter()
            .filter(|op| due || !matches!(op, TurnOp::Progress { .. }))
            .collect();
        let wrote_progress = ops.iter().any(|op| matches!(op, TurnOp::Progress { .. }));
        match after {
            Some(id) => {
                st.turns.insert(
                    now.id,
                    OpenTurn {
                        id,
                        progressed: if wrote_progress {
                            Some(std::time::Instant::now())
                        } else {
                            progressed
                        },
                    },
                );
            }
            None => {
                st.turns.remove(&now.id);
            }
        }
        ops
    }

    /// The turns the floor's log has open for sessions running now.
    pub fn open_turns(&self) -> HashSet<Uuid> {
        self.0.state.lock().turns.values().map(|t| t.id).collect()
    }

    /// Who this Mac's Claude Code and Codex are signed in as, as last read.
    pub fn sign_ins(&self) -> MacSignIns {
        self.0.sign_ins.lock().clone()
    }

    /// Reads this Mac's sign-ins now and every [`SIGN_INS_EVERY`], until
    /// the desk stops.
    async fn read_sign_ins(self) {
        let mut tick = tokio::time::interval(SIGN_INS_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let (claude, codex) = tokio::join!(
                self.cli_says(&self.0.opts.claude, &["auth", "status", "--json"]),
                self.cli_says(&self.0.opts.codex, &["login", "status"]),
            );
            *self.0.sign_ins.lock() = MacSignIns {
                claude: claude.and_then(|(out, _)| usage::claude_sign_in(&out)),
                // Codex says it on stderr.
                codex: codex.is_some_and(|(out, err)| {
                    usage::codex_signed_in(&out) || usage::codex_signed_in(&err)
                }),
            };
        }
    }

    /// What `program args` prints, on stdout and on stderr, run as the
    /// person's sessions are; `None` when it can't be run or doesn't answer
    /// in time.
    async fn cli_says(&self, program: &Path, args: &[&str]) -> Option<(String, String)> {
        let run = tokio::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(&self.0.env)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(SIGN_IN_WAIT, run).await.ok()?.ok()?;
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        Some((text(&out.stdout), text(&out.stderr)))
    }

    /// Passes the plan's rate limits a Claude Code session on this Mac
    /// reported to the hub.
    fn report_usage(&self, limits: Value) {
        if let Some(plans) = &self.0.plans {
            let account = self.sign_ins().claude.unwrap_or_default();
            plans.mac(MacPlan::Claude, limits, account);
        }
    }

    /// The process a session's own background work runs under: its
    /// app-server for Codex; for Claude Code, the CLI, or the terminal's
    /// shell it runs in.
    fn root_of(l: &Live, engine: Engine) -> Option<u32> {
        match engine {
            Engine::Claude => l.pty.pid(),
            Engine::Codex => l._codex.as_ref()?.server.pid(),
        }
    }

    /// What already runs under a session's own process, as its turn begins.
    async fn already_running(&self, id: Uuid) -> Option<HashSet<u32>> {
        let root = {
            let st = self.0.state.lock();
            let engine = st.records.get(&id)?.engine;
            Self::root_of(st.live.get(&id)?, engine)?
        };
        let table = procs::table().await?;
        Some(procs::under(&table, root).iter().map(|p| p.pid).collect())
    }

    /// Looks at what runs under each of `ids` whose turn is done and that
    /// has background work, or may have it unreported: Claude Code's tool
    /// shells; for Codex, what its turn started and left running.
    async fn look_at_background(&self, ids: &[Uuid]) {
        let due: Vec<(Uuid, u32, Option<HashSet<u32>>, Engine)> = {
            let st = self.0.state.lock();
            ids.iter()
                .filter_map(|id| {
                    let r = st.records.get(id)?;
                    let l = st.live.get(id)?;
                    (r.state == SessionState::Done && l.background.worth_a_look()).then_some(())?;
                    Some((
                        *id,
                        Self::root_of(l, r.engine)?,
                        l.baseline.clone(),
                        r.engine,
                    ))
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let Some(table) = procs::table().await else {
            return;
        };
        for (id, root, baseline, engine) in due {
            let running = procs::under(&table, root)
                .into_iter()
                .filter(|p| match (engine, &baseline) {
                    (Engine::Claude, _) => procs::is_tool_shell(p),
                    (Engine::Codex, Some(before)) => !before.contains(&p.pid),
                    (Engine::Codex, None) => false,
                })
                .count();
            let now = Utc::now();
            let looked = {
                let st = self.0.state.lock();
                st.live
                    .get(&id)
                    .map(|l| (l.background.clone().looked(running, now), &l.background))
                    .filter(|(after, before)| after != *before)
                    .map(|(after, _)| after)
            };
            if let Some(after) = looked {
                self.change(id, |_, l| {
                    if let Some(l) = l {
                        l.background = after;
                    }
                });
            }
        }
    }

    /// Looks at every session's background work every [`LOOK_EVERY`],
    /// until the desk stops.
    async fn watch_background(self) {
        let mut tick = tokio::time::interval(LOOK_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let ids: Vec<Uuid> = self.0.state.lock().live.keys().copied().collect();
            self.look_at_background(&ids).await;
        }
    }

    /// The environment a CLI Desk starts runs in: the base, with what Desk
    /// added, the person's own plugin folders kept ahead of Desk's.
    fn with_added(&self, added: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut env = self.0.env.clone();
        env.extend(added.clone());
        match plugin_dirs(self.0.opts.env.get(PLUGIN_DIRS), added.get(PLUGIN_DIRS)) {
            Some(dirs) => env.insert(PLUGIN_DIRS.into(), dirs),
            None => env.remove(PLUGIN_DIRS),
        };
        env
    }

    /// Tells every Desk watching.
    pub(crate) fn announce(&self, event: DeskEvent) {
        let _ = self.0.events.send(event);
    }

    /// Records `event` in Activity and tells every Desk watching.
    pub fn note(&self, event: Event) {
        let _ = self.0.persist.send(Persist::Event(Box::new(event.clone())));
        let _ = self.0.events.send(DeskEvent::Activity {
            event: Box::new(event),
        });
    }

    /// The events a session's own change makes: what it started, needs,
    /// finished, asked or hit.
    fn session_events(
        st: &mut State,
        transitions: &[Transition<'_>],
        now: &SessionView,
    ) -> Vec<Event> {
        let me = Party::session(now);
        let own = |kind: Kind, text: String| Event::new(kind, me.clone(), Some(me.clone()), text);
        transitions
            .iter()
            .filter_map(|t| match *t {
                Transition::Started => Some(own(
                    Kind::Started,
                    match now.origin {
                        Origin::External => "started in another terminal".into(),
                        Origin::Desk => "started".into(),
                    },
                )),
                Transition::Prompted { prompt } => Some(own(
                    Kind::Started,
                    format!("started “{}”", crew::one_line(prompt, 100)),
                )),
                Transition::Blocked { pending } => {
                    Some(own(Kind::NeedsYou, asking(pending)).approval(pending.id))
                }
                Transition::Unblocked {
                    pending,
                    by_desk: false,
                } => Some(
                    own(Kind::Answered, "was answered in its terminal".into()).approval(pending.id),
                ),
                Transition::Unblocked { by_desk: true, .. }
                | Transition::Spoke { .. }
                | Transition::ToTerminal { .. } => None,
                Transition::Finished { reply } => Some(match crew::outcome_of(Ok(reply)) {
                    (Outcome::Limit, _, _) => own(Kind::Limit, "hit its usage limit".into()),
                    (Outcome::Asked, _, Some(q)) => {
                        own(Kind::Question, format!("asked “{}”", q.text))
                            .detail(serde_json::json!({ "question": q }))
                    }
                    (_, Some(answer), _) => own(Kind::Finished, format!("finished · {answer}")),
                    _ => own(Kind::Finished, "finished".into()),
                }),
                Transition::Stopped { closed } => (!st.acted.remove(&now.id)).then(|| {
                    own(
                        Kind::Stopped,
                        if closed {
                            "closed in its terminal"
                        } else {
                            "stopped"
                        }
                        .into(),
                    )
                }),
                Transition::Background { from, count } => (from == 0 && count > 0).then(|| {
                    own(
                        Kind::Working,
                        format!("is waiting on {}", background_tasks(count)),
                    )
                }),
                Transition::Interrupted => {
                    st.acted.remove(&now.id);
                    Some(own(Kind::Failed, "stopped unexpectedly".into()))
                }
                Transition::Resumed => {
                    (!st.acted.remove(&now.id)).then(|| own(Kind::Resumed, "resumed".into()))
                }
                Transition::Tool { tool } => {
                    let due = st
                        .working_noted
                        .get(&now.id)
                        .is_none_or(|at| at.elapsed() >= WORKING_EVERY);
                    due.then(|| {
                        st.working_noted.insert(now.id, std::time::Instant::now());
                        own(
                            Kind::Working,
                            format!("is running {}", crew::clip(tool, 120)),
                        )
                    })
                }
            })
            .collect()
    }

    /// The registry and the crew's tables.
    pub fn store(&self) -> &DeskStore {
        &self.0.store
    }

    /// When the session came into the state it is in.
    pub fn state_since(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.0.state.lock().state_since.get(&id).copied()
    }

    fn publish(&self, id: Uuid) {
        let view = {
            let st = self.0.state.lock();
            st.records.get(&id).map(|r| Self::view_of(&st, r))
        };
        if let Some(session) = view {
            let _ = self.0.events.send(DeskEvent::Session {
                session: Box::new(session),
            });
        }
    }

    /// Applies `f` to a session's record (and its live part, if running),
    /// then persists and announces the result.
    fn change(&self, id: Uuid, f: impl FnOnce(&mut SessionRecord, Option<&mut Live>)) {
        self.change_since(None, id, f);
    }

    /// [`Self::change`], reading the change from `seen` when the session's
    /// live part already moved on from it.
    fn change_since(
        &self,
        seen: Option<SessionView>,
        id: Uuid,
        f: impl FnOnce(&mut SessionRecord, Option<&mut Live>),
    ) {
        let mut guard = self.0.state.lock();
        let st = &mut *guard;
        let Some(old) = st.records.get(&id) else {
            return;
        };
        let before = seen.unwrap_or_else(|| Self::view_of(st, old));
        let r = st.records.get_mut(&id).expect("just read");
        f(r, st.live.get_mut(&id));
        r.updated_at = Utc::now();
        let r = r.clone();
        self.saved(st, Some(before), r);
    }

    fn record(&self, id: Uuid) -> Result<SessionRecord, DeskError> {
        self.0
            .state
            .lock()
            .records
            .get(&id)
            .cloned()
            .ok_or(DeskError::NotFound)
    }

    pub fn view(&self, id: Uuid) -> Result<SessionView, DeskError> {
        let st = self.0.state.lock();
        st.records
            .get(&id)
            .map(|r| Self::view_of(&st, r))
            .ok_or(DeskError::NotFound)
    }

    pub fn list(&self) -> Vec<SessionView> {
        let st = self.0.state.lock();
        let mut all: Vec<_> = st
            .records
            .values()
            .filter(|r| r.state != SessionState::Closed)
            .map(|r| Self::view_of(&st, r))
            .collect();
        all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        all
    }

    pub fn restore_offer(&self) -> Vec<SessionView> {
        let st = self.0.state.lock();
        st.records
            .values()
            .filter(|r| r.restore_offer && !st.live.contains_key(&r.id))
            .map(|r| Self::view_of(&st, r))
            .collect()
    }

    /// The current snapshot and every change after it.
    pub fn subscribe(&self) -> (DeskEvent, broadcast::Receiver<DeskEvent>) {
        let rx = self.0.events.subscribe();
        (
            DeskEvent::Snapshot {
                sessions: self.list(),
                restore: self.restore_offer(),
                rooms: self.rooms(),
                terminals: self.terminals(),
                layout: self.layout(),
            },
            rx,
        )
    }

    pub fn attach(&self, id: Uuid) -> Result<Arc<Pty>, DeskError> {
        self.0
            .state
            .lock()
            .live
            .get(&id)
            .map(|l| Arc::clone(&l.pty))
            .ok_or_else(|| DeskError::Conflict("session is not running".into()))
    }

    /// Starts a session; `by` is who asked (none: the session's own
    /// `started` is recorded).
    pub async fn create(&self, new: NewSession, by: Option<By>) -> Result<SessionView, DeskError> {
        let name = new.name.trim();
        if name.is_empty() {
            return Err(DeskError::Invalid("a session needs a name".into()));
        }
        check_env(&new.env)?;
        if !new.env.is_empty() {
            self.0.state.lock().session_env = new.env.clone();
        }
        let cwd = PathBuf::from(&new.cwd);
        if !cwd.is_dir() {
            return Err(DeskError::Invalid(format!("{} is not a folder", new.cwd)));
        }
        let (cwd, branch) = match new
            .worktree
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            Some(branch) => (
                worktree::create(&cwd, branch)
                    .await
                    .map_err(|e| DeskError::Invalid(e.to_string()))?,
                Some(branch.to_string()),
            ),
            None => (cwd, None),
        };
        let now = Utc::now();
        let id = Uuid::new_v4();
        let record = SessionRecord {
            id,
            engine: new.engine,
            name: name.to_string(),
            goal: new.goal.trim().to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            branch,
            origin: Origin::Desk,
            state: SessionState::Starting,
            approvals: match new.engine {
                Engine::Claude => new.approvals.unwrap_or(Approvals::Desk),
                Engine::Codex => Approvals::Terminal,
            },
            cli_session_id: (new.engine == Engine::Claude).then(|| id.to_string()),
            transcript: None,
            last_prompt: new.prompt.clone(),
            last_reply: None,
            external_pid: None,
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            created_at: now,
            updated_at: now,
        };
        self.0.state.lock().records.insert(id, record.clone());
        if let Err(e) = self.launch(&record, false, new.prompt.as_deref()).await {
            self.0.state.lock().records.remove(&id);
            return Err(e);
        }
        // Persist what hooks may already have changed since launch.
        self.change(id, |_, _| {});
        let view = self.view(id)?;
        let me = Party::session(&view);
        self.note(match by {
            Some(by) => by.event(Kind::Started, Some(me), format!("started {}", view.label())),
            None => Event::new(Kind::Started, me.clone(), Some(me), "started"),
        });
        Ok(view)
    }

    pub fn update(&self, id: Uuid, patch: SessionPatch) -> Result<SessionView, DeskError> {
        self.record(id)?;
        if patch.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(DeskError::Invalid("a session needs a name".into()));
        }
        let display_name = patch
            .display_name
            .map(|n| n.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()));
        if display_name
            .as_ref()
            .is_some_and(|n| n.as_ref().is_some_and(|n| n.chars().count() > 200))
        {
            return Err(DeskError::Invalid(
                "a name is at most 200 characters".into(),
            ));
        }
        self.change(id, |r, _| {
            if let Some(n) = patch.name {
                r.name = n.trim().to_string();
            }
            if let Some(g) = patch.goal {
                r.goal = g.trim().to_string();
            }
            if let Some(a) = patch.approvals.filter(|_| r.engine == Engine::Claude) {
                r.approvals = a;
            }
            if let Some(n) = display_name {
                r.display_name = n;
            }
        });
        self.view(id)
    }

    pub fn stop(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        let pty = {
            let mut st = self.0.state.lock();
            let live = st
                .live
                .get_mut(&id)
                .ok_or_else(|| DeskError::Conflict("session is not running".into()))?;
            if live.terminal.is_some() {
                return Err(DeskError::Conflict(
                    "this session runs in a terminal; end it there".into(),
                ));
            }
            live.stopping = true;
            let pty = Arc::clone(&live.pty);
            if by.is_some() {
                st.acted.insert(id);
            }
            pty
        };
        pty.kill();
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Stopped,
                Some(Party::session(&view)),
                format!("stopped {}", view.label()),
            ));
        }
        Ok(view)
    }

    pub async fn resume(
        &self,
        id: Uuid,
        prompt: Option<String>,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        let r = self.record(id)?;
        if self.0.state.lock().live.contains_key(&id) {
            return Err(DeskError::Conflict("session is already running".into()));
        }
        if r.origin == Origin::External && r.external_pid.is_some_and(adopt::alive) {
            return Err(DeskError::Conflict(
                "still running in another terminal — close it there first".into(),
            ));
        }
        if r.cli_session_id.is_none() {
            return Err(DeskError::Conflict(
                "this session never started a conversation, so there is nothing to resume".into(),
            ));
        }
        if by.is_some() {
            self.0.state.lock().acted.insert(id);
        }
        self.change(id, |r, _| {
            r.origin = Origin::Desk;
            r.external_pid = None;
            // Desk runs it now, not the terminal it started in.
            r.terminal_id = None;
            r.state = SessionState::Starting;
            r.restore_offer = false;
            if let Some(p) = prompt.clone() {
                r.last_prompt = Some(p);
            }
        });
        if let Err(e) = self.launch(&r, true, prompt.as_deref()).await {
            self.0.state.lock().acted.remove(&id);
            self.save(SessionRecord {
                updated_at: Utc::now(),
                ..r
            });
            return Err(e);
        }
        let view = self.view(id)?;
        if let Some(by) = by {
            let text = match &prompt {
                Some(p) => format!("resumed {} with “{}”", view.label(), crew::one_line(p, 100)),
                None => format!("resumed {}", view.label()),
            };
            self.note(by.event(Kind::Resumed, Some(Party::session(&view)), text));
        }
        Ok(view)
    }

    /// Stops a running session (if it runs) and resumes its conversation,
    /// with `prompt` if given: one `restarted` event by `by`.
    pub async fn restart(
        &self,
        id: Uuid,
        prompt: Option<String>,
        by: By,
    ) -> Result<SessionView, DeskError> {
        self.record(id)?;
        let running = self.0.state.lock().live.contains_key(&id);
        if running {
            self.0.state.lock().acted.insert(id);
            self.stop(id, None)?;
            for _ in 0..250 {
                if !self.0.state.lock().live.contains_key(&id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        self.0.state.lock().acted.insert(id);
        let view = self.resume(id, prompt.clone(), None).await?;
        let text = match &prompt {
            Some(p) => format!(
                "restarted {} with “{}”",
                view.label(),
                crew::one_line(p, 100)
            ),
            None => format!("restarted {}", view.label()),
        };
        self.note(by.event(Kind::Restarted, Some(Party::session(&view)), text));
        Ok(view)
    }

    /// Resumes each id, returning those that came back; failures are logged
    /// and leave the session on offer.
    pub async fn restore(&self, ids: &[Uuid], by: Option<By>) -> Vec<SessionView> {
        let mut back = Vec::new();
        for &id in ids {
            match self.resume(id, None, by.clone()).await {
                Ok(v) => back.push(v),
                Err(e) => tracing::warn!(%id, error = %e, "desk session could not be restored"),
            }
        }
        back
    }

    pub fn dismiss_restore(&self) {
        let ids: Vec<_> = self
            .0
            .state
            .lock()
            .records
            .values()
            .filter(|r| r.restore_offer)
            .map(|r| r.id)
            .collect();
        for id in ids {
            self.change(id, |r, _| r.restore_offer = false);
        }
    }

    /// Forgets a session: ends it and its turn, deletes its record and
    /// takes it out of the sidebar's layout.
    pub async fn forget(&self, id: Uuid) -> Result<(), DeskError> {
        let live = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let view = Self::view_of(st, st.records.get(&id).ok_or(DeskError::NotFound)?);
            let ops = Self::turn_ops(st, &[Transition::Stopped { closed: false }], &view);
            self.log_turns(ops);
            st.records.remove(&id);
            st.seen_transcripts.remove(&id);
            st.state_since.remove(&id);
            st.working_noted.remove(&id);
            st.acted.remove(&id);
            st.live.remove(&id)
        };
        if let Some(live) = &live {
            live.end();
        }
        drop(live);
        let _ = self.0.persist.send(Persist::Delete(id));
        let _ = self.0.events.send(DeskEvent::Removed { id });
        self.drop_from_layout(id).await;
        Ok(())
    }

    pub fn decide(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: Decision,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        let asked = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .and_then(|l| l.pending.clone());
        let (answer, seen) = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let seen = st.records.get(&id).map(|r| Self::view_of(st, r));
            let live = st
                .live
                .get_mut(&id)
                .ok_or_else(|| DeskError::Conflict("session is not running".into()))?;
            if live.pending.as_ref().map(|p| p.id) != Some(approval) {
                return Err(DeskError::Conflict(
                    "that approval is no longer pending".into(),
                ));
            }
            let answer = live
                .answer
                .take()
                .ok_or_else(|| DeskError::Conflict("the terminal is asking for this one".into()))?;
            match decision {
                Decision::Terminal => {
                    if let Some(p) = live.pending.as_mut() {
                        p.answerable = false;
                    }
                }
                Decision::Allow | Decision::Deny => {
                    live.pending = None;
                    st.decided.insert(approval);
                }
            }
            (answer, seen)
        };
        let _ = answer.send(match decision {
            Decision::Allow => claude::allow(),
            Decision::Deny => claude::deny(),
            Decision::Terminal => claude::ask_in_terminal(),
        });
        self.change_since(seen, id, |r, l| {
            if l.is_some_and(|l| l.pending.is_none()) {
                r.state = SessionState::Working;
            }
        });
        let view = self.view(id)?;
        if let (Some(by), Some(p)) = (by, asked) {
            let what = match p.tool.split('(').next().unwrap_or(&p.tool) {
                "Bash" if !p.detail.is_empty() => format!("run {}", crew::clip(&p.detail, 120)),
                "Edit" | "Write" | "MultiEdit" if !p.detail.is_empty() => {
                    format!("edit {}", crew::clip(&p.detail, 120))
                }
                _ => format!("use {}", crew::clip(&p.tool, 120)),
            };
            let me = Some(Party::session(&view));
            let event = match decision {
                Decision::Allow => Some(by.event(
                    Kind::Approved,
                    me,
                    format!("allowed {} to {what}", view.label()),
                )),
                Decision::Deny => Some(by.event(
                    Kind::Denied,
                    me,
                    format!("denied {} to {what}", view.label()),
                )),
                Decision::Terminal => None,
            };
            if let Some(e) = event {
                self.note(e.approval(p.id));
            }
        }
        Ok(view)
    }

    /// Types a prompt into a running session's terminal and submits it, as a
    /// person at the keyboard would (bracketed paste, so newlines stay in
    /// the prompt).
    pub fn prompt(&self, id: Uuid, text: &str, by: Option<By>) -> Result<SessionView, DeskError> {
        if text.trim().is_empty() {
            return Err(DeskError::Invalid("the prompt is empty".into()));
        }
        let pty = self.attach(id)?;
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(text.as_bytes());
        input.extend_from_slice(b"\x1b[201~\r");
        pty.write(&input)?;
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Nudged,
                Some(Party::session(&view)),
                format!("asked {} “{}”", view.label(), crew::one_line(text, 120)),
            ));
        }
        Ok(view)
    }

    /// Waits until the session reaches one of `states` (or stops running),
    /// up to `timeout`; returns where it is either way.
    pub async fn wait_for(
        &self,
        id: Uuid,
        states: &[SessionState],
        timeout: Duration,
    ) -> Result<SessionView, DeskError> {
        let mut rx = self.0.events.subscribe();
        let settled = |v: &SessionView| states.contains(&v.state) || !v.live;
        let now = self.view(id)?;
        if settled(&now) {
            return Ok(now);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return self.view(id),
                Ok(Ok(DeskEvent::Session { session })) if session.id == id && settled(&session) => {
                    return Ok(*session)
                }
                Ok(Ok(DeskEvent::Removed { id: gone })) if gone == id => {
                    return Err(DeskError::NotFound)
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    let v = self.view(id)?;
                    if settled(&v) {
                        return Ok(v);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => return self.view(id),
                Ok(Ok(_)) => {}
            }
        }
    }

    /// The end of what the terminal shows, as plain text.
    pub fn terminal_text(&self, id: Uuid, max_chars: usize) -> Result<String, DeskError> {
        let text = crate::pty::plain_text(self.attach(id)?.text().as_bytes());
        let skip = text.chars().count().saturating_sub(max_chars);
        Ok(text.chars().skip(skip).collect())
    }

    pub fn rooms(&self) -> Vec<Room> {
        let mut rooms: Vec<_> = self.0.state.lock().rooms.values().cloned().collect();
        rooms.sort_by(|a, b| a.name.cmp(&b.name));
        rooms
    }

    /// The room `id`, while this desk has it.
    pub fn room(&self, id: Uuid) -> Option<Room> {
        self.0.state.lock().rooms.get(&id).cloned()
    }

    fn room_of(&self, id: Uuid) -> Result<Room, DeskError> {
        self.0
            .state
            .lock()
            .rooms
            .get(&id)
            .cloned()
            .ok_or(DeskError::NotFound)
    }

    async fn save_room(&self, room: Room) -> Result<Room, DeskError> {
        if room.name.trim().is_empty() || room.slug.is_empty() {
            return Err(DeskError::Invalid("a room needs a name".into()));
        }
        if let Some(d) = room.default_member.as_deref() {
            if !room.members.iter().any(|m| m.key() == d) {
                return Err(DeskError::Invalid(format!(
                    "{d} is not a member of the room"
                )));
            }
        }
        if self.0.store.slug_taken(&room.slug, Some(room.id)).await? {
            return Err(DeskError::Conflict(format!(
                "there is already a room called {}",
                room.name
            )));
        }
        self.0.store.put_room(&room).await?;
        self.0.state.lock().rooms.insert(room.id, room.clone());
        let _ = self.0.events.send(DeskEvent::Room {
            room: Box::new(room.clone()),
        });
        Ok(room)
    }

    pub async fn create_room(&self, new: NewRoom) -> Result<Room, DeskError> {
        let now = Utc::now();
        let members = dedup(new.members);
        let id = Uuid::new_v4();
        self.save_room(Room {
            id,
            channel: channel_for(id),
            slug: slug_for(&new.name),
            name: new.name.trim().to_string(),
            default_member: new
                .default_member
                .or_else(|| (members.len() == 1).then(|| members[0].key())),
            members,
            created_at: now,
            updated_at: now,
        })
        .await
    }

    pub async fn update_room(&self, id: Uuid, patch: RoomPatch) -> Result<Room, DeskError> {
        let mut room = self.room_of(id)?;
        if let Some(name) = patch.name {
            room.slug = slug_for(&name);
            room.name = name.trim().to_string();
        }
        if let Some(members) = patch.members {
            room.members = dedup(members);
            if room
                .default_member
                .as_deref()
                .is_some_and(|d| !room.members.iter().any(|m| m.key() == d))
            {
                room.default_member = None;
            }
        }
        if let Some(d) = patch.default_member {
            room.default_member = d;
        }
        room.updated_at = Utc::now();
        self.save_room(room).await
    }

    pub async fn delete_room(&self, id: Uuid) -> Result<(), DeskError> {
        if !self.0.store.delete_room(id).await? {
            return Err(DeskError::NotFound);
        }
        self.0.state.lock().rooms.remove(&id);
        let _ = self.0.events.send(DeskEvent::RoomRemoved { id });
        Ok(())
    }

    pub async fn room_messages(
        &self,
        id: Uuid,
        thread_id: Option<Uuid>,
        before: Option<chrono::DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<RoomMessage>, DeskError> {
        self.room_of(id)?;
        Ok(self
            .0
            .store
            .messages(id, thread_id, before, limit.clamp(1, 200))
            .await?)
    }

    /// Adds a message to a room. Agent messages are held to the loop guard:
    /// at most [`MAX_HOPS`] hand-offs and [`TURN_BUDGET`] agent messages
    /// before you speak again. A delivery (a reminder, a job's answer) is
    /// no hand-off and starts none, so it is held to neither, and may come
    /// from an agent that has left the room since it was set up. A message
    /// with a source is kept once: posting it again answers with the one
    /// kept, and a thread it names that has gone takes it to the timeline.
    pub async fn post(&self, id: Uuid, new: NewMessage) -> Result<Posted, DeskError> {
        let room = self.room_of(id)?;
        if let Some(kept) = match new.source.as_deref() {
            Some(source) => self.0.store.by_source(source).await?,
            None => None,
        } {
            return Ok(Posted {
                message: kept,
                created: false,
            });
        }
        if new.content.trim().is_empty() && new.attachments.is_empty() {
            return Err(DeskError::Invalid("the message is empty".into()));
        }
        if new.attachments.len() > crate::attachments::MAX_PER_MESSAGE {
            return Err(DeskError::Invalid(format!(
                "a message carries at most {} files",
                crate::attachments::MAX_PER_MESSAGE
            )));
        }
        if let Some(why) = new.attachments.iter().find_map(|a| a.invalid()) {
            return Err(DeskError::Invalid(why));
        }
        let is_agent = new.author != YOU && new.author != DESK;
        let delivered = new.delivery.is_some();
        if delivered && (!is_agent || Member::parse(&new.author).is_none()) {
            return Err(DeskError::Invalid(format!(
                "a delivery comes from an agent, as hub/agent, not {}",
                new.author
            )));
        }
        if is_agent && !delivered && !room.members.iter().any(|m| m.key() == new.author) {
            return Err(DeskError::Invalid(format!(
                "{} is not a member of the room",
                new.author
            )));
        }
        let parent = match new.thread_id {
            Some(t) => self.0.store.message(t).await?,
            None => None,
        };
        let thread_id = place_in_timeline(id, new.thread_id, parent.as_ref());
        let thread_gone = new.thread_id.is_some() && thread_id.is_none();
        if thread_gone && new.source.is_none() {
            return Err(DeskError::Invalid(
                "threads hang off a message in this room's timeline".into(),
            ));
        }
        if is_agent && !delivered {
            if new.hop > MAX_HOPS {
                return Err(DeskError::Conflict(format!(
                    "loop guard: agents have handed this on {MAX_HOPS} times already"
                )));
            }
            if self.0.store.agent_run(id, thread_id).await? >= TURN_BUDGET {
                return Err(DeskError::Conflict(format!(
                    "loop guard: {TURN_BUDGET} agent messages since you last spoke"
                )));
            }
        }
        let posted = self
            .0
            .store
            .add_message(&RoomMessage {
                id: Uuid::new_v4(),
                room_id: id,
                thread_id,
                author: new.author,
                content: match thread_gone {
                    true => format!("{THREAD_GONE} {}", new.content)
                        .trim_end()
                        .to_string(),
                    false => new.content,
                },
                hop: if is_agent && !delivered {
                    new.hop.max(0)
                } else {
                    0
                },
                created_at: Utc::now(),
                attachments: new.attachments,
                source: new.source,
                request_id: new.request_id.filter(|_| new.delivery.is_some()),
                delivery: new.delivery,
                replies: None,
            })
            .await?;
        if posted.created {
            let _ = self.0.events.send(DeskEvent::RoomMessage {
                message: Box::new(posted.message.clone()),
            });
        }
        Ok(posted)
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, DeskError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        Ok(self.0.store.search(query, 50).await?)
    }

    async fn launch(
        &self,
        r: &SessionRecord,
        resume: bool,
        prompt: Option<&str>,
    ) -> Result<(), DeskError> {
        let token = Uuid::new_v4().simple().to_string();
        let cwd = Path::new(&r.cwd);
        if !cwd.is_dir() {
            return Err(DeskError::Conflict(format!("{} no longer exists", r.cwd)));
        }
        let env = self.with_added(&self.0.state.lock().session_env.clone());
        let (spec, codex) = match r.engine {
            Engine::Claude => {
                let session_id = r.cli_session_id.as_deref().unwrap_or_default();
                let hook_url = format!("{}/hook/{}/{token}", self.0.hook_base, r.id);
                let spec = claude::spawn(&claude::ClaudeSpawn {
                    program: &self.0.opts.claude,
                    cwd,
                    name: &r.name,
                    launch: if resume {
                        claude::Launch::Resume { session_id }
                    } else {
                        claude::Launch::New { session_id }
                    },
                    prompt,
                    hook_url: &hook_url,
                    approval_timeout_secs: self.0.opts.approval_timeout.as_secs(),
                    env: &env,
                });
                (spec, None)
            }
            Engine::Codex => {
                let socket = self.0.opts.run_dir.join(format!("{}.sock", r.id.simple()));
                let server = self.start_app_server(r.id, &socket).await?;
                let spec = codex::spawn(&codex::CodexSpawn {
                    program: &self.0.opts.codex,
                    cwd,
                    socket: &socket,
                    launch: match (resume, r.cli_session_id.as_deref()) {
                        (true, Some(thread_id)) => codex::Launch::Resume { thread_id },
                        _ => codex::Launch::New,
                    },
                    prompt,
                    env: &env,
                });
                let link = self.observe_codex(r.id, server, socket);
                (spec, Some(link))
            }
        };
        let meta = serde_json::json!({
            "kind": "session",
            "session": r.id,
            "token": token,
            "hookPort": self.0.hook_port,
        });
        let started = self.start_pty(&spec, meta).await;
        let (pty, exit) = match started {
            Ok(started) => started,
            Err(e) => {
                if let Some(Server::Held(server)) = codex.as_ref().map(|c| &c.server) {
                    server.kill();
                }
                return Err(e);
            }
        };
        self.0.state.lock().live.insert(
            r.id,
            Live {
                pty: Arc::new(pty),
                token,
                current_tool: None,
                pending: None,
                answer: None,
                ended: false,
                stopping: false,
                _codex: codex,
                terminal: None,
                background: Default::default(),
                baseline: Default::default(),
            },
        );
        tokio::spawn(self.clone().on_exit(r.id, exit));
        Ok(())
    }

    /// Starts a Codex session's app-server on `socket`: in the holder when
    /// there is one, so it outlives the hub with the TUI attached to it.
    async fn start_app_server(&self, id: Uuid, socket: &Path) -> Result<Server, DeskError> {
        let Some(holder) = self.holder().await else {
            let server =
                AppServer::start(&self.0.opts.codex, socket.to_path_buf(), &self.0.env).await?;
            return Ok(Server::Local(server));
        };
        codex::prepare_socket(socket).await?;
        let spec = codex::app_server(&self.0.opts.codex, socket, &self.0.env);
        let meta = serde_json::json!({
            "kind": "codex-app-server",
            "session": id,
            "socket": socket,
        });
        let (pty, _) = holder.spawn(&spec, 80, 24, 0, meta).await?;
        let pty = Arc::new(pty);
        if let Err(e) = codex::wait_for_socket(socket).await {
            pty.kill();
            return Err(e.into());
        }
        Ok(Server::Held(pty))
    }

    /// Joins a Codex session's app-server as a second client to follow it.
    fn observe_codex(&self, id: Uuid, server: Server, socket: PathBuf) -> CodexLink {
        let watch = self.0.cancel.child_token();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn({
            let socket = server.socket(&socket);
            let watch = watch.clone();
            let plans = self.0.plans.clone();
            async move {
                if let Err(e) = codex::observe(socket, tx, plans, watch).await {
                    tracing::warn!(error = %e, "codex observer stopped");
                }
            }
        });
        tokio::spawn(self.clone().codex_loop(id, rx));
        CodexLink { server, watch }
    }

    async fn on_exit(self, id: Uuid, exit: oneshot::Receiver<Exit>) {
        let code = exit.await.ok().flatten();
        if self.0.cancel.is_cancelled() {
            // The hub is stopping: whatever ends now is not the session's doing.
            return;
        }
        let live = self.0.state.lock().live.remove(&id);
        let Some(live) = live else { return };
        tracing::info!(%id, ?code, "desk session exited");
        let clean = live.ended || live.stopping;
        live.end();
        drop(live);
        self.change(id, |r, _| {
            r.state = if clean {
                SessionState::Stopped
            } else {
                r.state.after_loss()
            };
        });
    }

    /// Handles one Claude Code hook call. Returns `None` when the URL's token
    /// doesn't match a running session.
    pub async fn hook(&self, id: Uuid, token: &str, body: &[u8]) -> Option<Value> {
        let authorised = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .is_some_and(|l| l.token == token);
        if !authorised {
            return None;
        }
        let hook = match claude::parse_hook(body) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(%id, error = %e, "unreadable Claude Code hook");
                return Some(Value::Object(Default::default()));
            }
        };
        Some(self.apply_hook(id, hook, claude::agent_of(body)).await)
    }

    /// Applies one hook, fired inside the sub-agent `inside` when it says:
    /// a sub-agent's tool calls while the session's own turn is done are
    /// its background work, not the session working.
    async fn apply_hook(&self, id: Uuid, hook: Hook, inside: Option<String>) -> Value {
        let empty = || Value::Object(Default::default());
        if let Some(agent) = &inside {
            let idle = {
                let mut st = self.0.state.lock();
                let done = st.records.get(&id).map(|r| r.state) == Some(SessionState::Done);
                if let Some(l) = st.live.get_mut(&id) {
                    l.background = std::mem::take(&mut l.background).heard(agent, Utc::now());
                }
                done
            };
            if idle && matches!(hook, Hook::ToolStart(_) | Hook::ToolEnd) {
                return empty();
            }
        }
        match hook {
            Hook::SessionStart {
                session_id,
                transcript,
                ..
            } => {
                self.change(id, |r, _| {
                    r.cli_session_id = session_id.or(r.cli_session_id.take());
                    r.transcript = transcript.or(r.transcript.take());
                    if r.state == SessionState::Starting {
                        r.state = SessionState::Idle;
                    }
                });
            }
            Hook::Prompt(p) => self.change(id, |r, l| {
                r.state = SessionState::Working;
                r.last_prompt = Some(p);
                r.restore_offer = false;
                if let Some(l) = l {
                    l.pending = None;
                }
            }),
            Hook::ToolStart(tool) => self.change(id, |r, l| {
                r.state = SessionState::Working;
                if let Some(l) = l {
                    l.current_tool = Some(tool);
                }
            }),
            Hook::ToolEnd => self.change(id, |r, l| {
                r.state = SessionState::Working;
                if let Some(l) = l {
                    l.current_tool = None;
                    l.pending = None;
                    l.answer = None;
                }
            }),
            Hook::NeedsInput(message) => self.change(id, |r, l| {
                if let Some(l) = l.filter(|l| l.pending.is_none()) {
                    r.state = SessionState::Blocked;
                    l.pending = Some(PendingApproval {
                        id: Uuid::new_v4(),
                        tool: "Input".into(),
                        detail: message,
                        since: Utc::now(),
                        answerable: false,
                    });
                }
            }),
            Hook::Woken => self.change(id, |r, l| {
                r.state = SessionState::Working;
                if let Some(l) = l {
                    l.pending = None;
                }
            }),
            Hook::SubagentStart(agent) => self.change(id, |_, l| {
                if let Some(l) = l {
                    l.background = std::mem::take(&mut l.background).started(
                        &agent,
                        TaskKind::Agent,
                        Utc::now(),
                    );
                }
            }),
            Hook::SubagentStop(agent) => self.change(id, |r, l| {
                if let Some(l) = l {
                    let before = l.background.count();
                    l.background = std::mem::take(&mut l.background).ended(&agent, Utc::now());
                    // Claude Code goes on by itself once the last of it ends;
                    // its next Stop says whether the turn is done.
                    if r.state == SessionState::Done && before > 0 && l.background.count() == 0 {
                        r.state = SessionState::Working;
                    }
                }
            }),
            Hook::Stop {
                transcript,
                reply,
                background,
            } => {
                let path = transcript.or_else(|| self.record(id).ok().and_then(|r| r.transcript));
                // The hook fires before the transcript has the reply, so the
                // transcript is only a fallback for older CLIs.
                let reply = match (reply, path.clone()) {
                    (Some(r), _) => Some(transcript::clip(&r)),
                    (None, Some(p)) => tokio::task::spawn_blocking(move || {
                        transcript::read_tail(Path::new(&p))
                            .ok()
                            .and_then(|t| t.last_reply)
                    })
                    .await
                    .ok()
                    .flatten(),
                    (None, None) => None,
                };
                self.change(id, |r, l| {
                    r.state = SessionState::Done;
                    r.transcript = path.or(r.transcript.take());
                    if let Some(reply) = reply {
                        r.last_reply = Some(reply);
                    }
                    if let Some(l) = l {
                        l.current_tool = None;
                        l.pending = None;
                        l.answer = None;
                        l.background =
                            std::mem::take(&mut l.background).reported(background, Utc::now());
                    }
                });
                self.look_at_background(&[id]).await;
            }
            Hook::SessionEnd => {
                if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                    l.ended = true;
                }
            }
            Hook::Permission { tool, detail } => {
                return self.await_permission(id, tool, detail).await
            }
            Hook::Usage(limits) => self.report_usage(limits),
            Hook::Other => {}
        }
        empty()
    }

    async fn await_permission(&self, id: Uuid, tool: String, detail: String) -> Value {
        let desk_answers = self
            .record(id)
            .is_ok_and(|r| r.approvals == Approvals::Desk);
        let pending = PendingApproval {
            id: Uuid::new_v4(),
            tool,
            detail,
            since: Utc::now(),
            answerable: desk_answers,
        };
        let (tx, rx) = oneshot::channel();
        self.change(id, |r, l| {
            r.state = SessionState::Blocked;
            if let Some(l) = l {
                l.pending = Some(pending.clone());
                l.answer = desk_answers.then_some(tx);
            }
        });
        if !desk_answers {
            return claude::ask_in_terminal();
        }
        // Answer a little before Claude Code's own hook timeout, so a slow
        // human falls back to the terminal dialog rather than an error.
        let in_terminal = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .is_some_and(|l| l.terminal.is_some());
        let wait = if in_terminal {
            terminals::TERMINAL_APPROVAL_WAIT
        } else {
            self.0.opts.approval_timeout
        }
        .saturating_sub(Duration::from_secs(5));
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(answer)) => answer,
            _ => {
                self.change(id, |_, l| {
                    if let Some(l) = l {
                        if let Some(p) = l.pending.as_mut().filter(|p| p.id == pending.id) {
                            p.answerable = false;
                        }
                        l.answer = None;
                    }
                });
                claude::ask_in_terminal()
            }
        }
    }

    async fn codex_loop(self, id: Uuid, mut rx: mpsc::Receiver<Seen>) {
        let home = self.0.env.get("HOME").map(PathBuf::from);
        let mut children = HashSet::new();
        while let Some(Seen { thread, event }) = rx.recv().await {
            let ours = self.record(id).ok().and_then(|r| r.cli_session_id);
            let child = match &event {
                CodexEvent::Child(child) if ours.as_deref().is_none_or(|t| t == thread) => {
                    children.insert(child.clone());
                    Some((child.clone(), true))
                }
                CodexEvent::Working { .. } if children.contains(&thread) => {
                    Some((thread.clone(), true))
                }
                CodexEvent::Idle if children.contains(&thread) => Some((thread.clone(), false)),
                _ => None,
            };
            if let Some((child, running)) = child {
                self.change(id, |_, l| {
                    if let Some(l) = l {
                        let now = Utc::now();
                        let b = std::mem::take(&mut l.background);
                        l.background = match running {
                            true => b.started(&child, TaskKind::Agent, now),
                            false => b.ended(&child, now),
                        };
                    }
                });
                continue;
            }
            if ours.as_deref().is_some_and(|t| t != thread) {
                continue;
            }
            let turn_begins = matches!(event, CodexEvent::Working { .. })
                && self.record(id).is_ok_and(|r| {
                    !matches!(r.state, SessionState::Working | SessionState::Blocked)
                });
            let baseline = match turn_begins {
                true => self.already_running(id).await,
                false => None,
            };
            let tail = match &event {
                CodexEvent::Idle => {
                    let rollout = self
                        .record(id)
                        .ok()
                        .and_then(|r| r.transcript)
                        .map(PathBuf::from)
                        .or_else(|| {
                            home.as_deref()
                                .and_then(|h| codex::find_rollout(h, &thread))
                        });
                    match rollout {
                        Some(p) => tokio::task::spawn_blocking(move || {
                            transcript::read_codex_tail(&p).ok().map(|t| (p, t))
                        })
                        .await
                        .ok()
                        .flatten(),
                        None => None,
                    }
                }
                _ => None,
            };
            self.change(id, |r, mut l| {
                r.cli_session_id.get_or_insert_with(|| thread.clone());
                match event {
                    CodexEvent::Thread { rollout } => {
                        r.transcript = rollout.or(r.transcript.take());
                        if r.state == SessionState::Starting {
                            r.state = SessionState::Idle;
                        }
                    }
                    CodexEvent::Working { waiting } => {
                        if let Some(l) = l.as_deref_mut().filter(|_| baseline.is_some()) {
                            l.baseline = baseline;
                        }
                        r.state = if waiting {
                            SessionState::Blocked
                        } else {
                            SessionState::Working
                        };
                        r.restore_offer = false;
                        if let Some(l) = l {
                            // A repeated notice is the same request, still asked.
                            let asked = l.pending.take();
                            l.pending = waiting.then(|| {
                                asked.unwrap_or_else(|| PendingApproval {
                                    id: Uuid::new_v4(),
                                    tool: l.current_tool.clone().unwrap_or_else(|| "Codex".into()),
                                    detail: "Answer in the terminal".into(),
                                    since: Utc::now(),
                                    answerable: false,
                                })
                            });
                        }
                    }
                    CodexEvent::Idle => {
                        if r.state != SessionState::Starting {
                            r.state = SessionState::Done;
                        }
                        if let Some((path, t)) = tail {
                            r.transcript = Some(path.to_string_lossy().into_owned());
                            r.last_prompt = t.last_prompt.or(r.last_prompt.take());
                            r.last_reply = t.last_reply.or(r.last_reply.take());
                        }
                        if let Some(l) = l {
                            l.current_tool = None;
                            l.pending = None;
                        }
                    }
                    CodexEvent::Prompt(p) => r.last_prompt = Some(p),
                    CodexEvent::Tool(t) => {
                        r.state = SessionState::Working;
                        if let Some(l) = l {
                            l.current_tool = Some(t);
                        }
                    }
                    CodexEvent::Reply(text) => r.last_reply = Some(transcript::clip(&text)),
                    CodexEvent::Child(_) => {}
                }
            });
        }
    }

    async fn adopt_loop(self, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            match adopt::list(&self.0.opts.claude, &self.0.env).await {
                Ok(found) => self.reconcile_external(found).await,
                Err(e) => tracing::debug!(error = %e, "could not list other Claude Code sessions"),
            }
        }
    }

    async fn reconcile_external(&self, found: Vec<External>) {
        let home = self.0.env.get("HOME").map(PathBuf::from);
        let (ours, by_cli): (HashSet<String>, HashMap<String, Uuid>) = {
            let st = self.0.state.lock();
            (
                // Running here, or started in one of Desk's terminals (whose
                // hooks report it): never another terminal's.
                st.records
                    .values()
                    .filter(|r| st.live.contains_key(&r.id) || r.terminal_id.is_some())
                    .filter_map(|r| r.cli_session_id.clone())
                    .collect(),
                st.records
                    .values()
                    .filter(|r| r.engine == Engine::Claude)
                    .filter_map(|r| Some((r.cli_session_id.clone()?, r.id)))
                    .collect(),
            )
        };
        let mut present = HashSet::new();
        for ext in found.into_iter().filter(|e| !ours.contains(&e.session_id)) {
            let id = by_cli.get(&ext.session_id).copied();
            let transcript = id
                .and_then(|id| self.record(id).ok()?.transcript)
                .or_else(|| {
                    home.as_deref()
                        .and_then(|h| find_transcript(h, &ext.session_id))
                });
            let tail = transcript
                .as_deref()
                .and_then(|t| self.fresh_tail(id, Path::new(t)));
            let state = if ext.busy() {
                SessionState::Working
            } else {
                SessionState::Idle
            };
            match id {
                Some(id) => {
                    present.insert(id);
                    let unchanged = self.record(id).is_ok_and(|r| {
                        r.origin == Origin::External
                            && r.state == state
                            && r.external_pid == ext.pid
                            && tail.is_none()
                    });
                    if !unchanged {
                        self.change(id, |r, _| {
                            r.origin = Origin::External;
                            r.state = state;
                            r.external_pid = ext.pid;
                            r.restore_offer = false;
                            r.cwd = ext.cwd.clone();
                            r.transcript = transcript.clone().or(r.transcript.take());
                            if let Some(t) = tail {
                                r.last_prompt = t.last_prompt.or(r.last_prompt.take());
                                r.last_reply = t.last_reply.or(r.last_reply.take());
                            }
                        });
                    }
                }
                None => {
                    let now = Utc::now();
                    let tail = tail.unwrap_or_default();
                    let r = SessionRecord {
                        id: Uuid::new_v4(),
                        engine: Engine::Claude,
                        name: ext
                            .name
                            .clone()
                            .or(tail.title.clone())
                            .unwrap_or_else(|| folder_name(&ext.cwd)),
                        goal: String::new(),
                        cwd: ext.cwd.clone(),
                        branch: None,
                        origin: Origin::External,
                        state,
                        approvals: Approvals::Desk,
                        cli_session_id: Some(ext.session_id.clone()),
                        transcript,
                        last_prompt: tail.last_prompt,
                        last_reply: tail.last_reply,
                        external_pid: ext.pid,
                        restore_offer: false,
                        terminal_id: None,
                        display_name: None,
                        created_at: now,
                        updated_at: now,
                    };
                    present.insert(r.id);
                    self.save(r);
                }
            }
        }
        let gone: Vec<Uuid> = {
            let st = self.0.state.lock();
            st.records
                .values()
                .filter(|r| r.origin == Origin::External && r.state.is_running())
                .filter(|r| !present.contains(&r.id))
                .map(|r| r.id)
                .collect()
        };
        for id in gone {
            // Closed in its own terminal while the hub watched: not a loss.
            self.change(id, |r, _| r.state = SessionState::Closed);
        }
    }

    /// The transcript's tail if it changed since last read.
    fn fresh_tail(&self, id: Option<Uuid>, path: &Path) -> Option<transcript::Tail> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some(id) = id {
            let mut st = self.0.state.lock();
            if st.seen_transcripts.get(&id) == Some(&mtime) {
                return None;
            }
            st.seen_transcripts.insert(id, mtime);
        }
        transcript::read_tail(path).ok()
    }
}

/// What the holder still had when the hub started, by owner.
#[derive(Default)]
pub(crate) struct HeldSet {
    sessions: HashMap<Uuid, Held>,
    servers: HashMap<Uuid, Held>,
    pub(crate) terminals: HashMap<Uuid, Held>,
    /// Ended, or owned by nothing the hub knows.
    pub(crate) strays: Vec<Held>,
    /// The port the held sessions' hooks post to.
    hook_port: Option<u16>,
}

impl HeldSet {
    fn sort(all: Vec<Held>) -> Self {
        let mut set = Self::default();
        for h in all {
            let owner = |key: &str| h.meta[key].as_str().and_then(|s| Uuid::parse_str(s).ok());
            if h.exited {
                set.strays.push(h);
                continue;
            }
            match (h.meta["kind"].as_str(), owner("session"), owner("terminal")) {
                (Some("session"), Some(id), _) => {
                    set.hook_port = set.hook_port.or_else(|| {
                        h.meta["hookPort"]
                            .as_u64()
                            .and_then(|p| u16::try_from(p).ok())
                    });
                    set.sessions.insert(id, h);
                }
                (Some("codex-app-server"), Some(id), _) => {
                    set.servers.insert(id, h);
                }
                (Some("terminal"), _, Some(id)) => {
                    set.hook_port = set.hook_port.or_else(|| {
                        h.meta["hookPort"]
                            .as_u64()
                            .and_then(|p| u16::try_from(p).ok())
                    });
                    set.terminals.insert(id, h);
                }
                _ => set.strays.push(h),
            }
        }
        set
    }
}

/// Connects to the holder, when there is one, and lists what it kept.
async fn connect_holder(opts: Option<&HolderOptions>) -> (Option<Holder>, HeldSet) {
    let Some(opts) = opts else {
        return (None, HeldSet::default());
    };
    let holder = match Holder::connect(&opts.socket, &opts.token).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, socket = %opts.socket.display(),
                "PTY holder unavailable; terminals run in the hub and end with it");
            return (None, HeldSet::default());
        }
    };
    match holder.list().await {
        Ok(all) => {
            tracing::info!(held = all.len(), "connected to the PTY holder");
            (Some(holder), HeldSet::sort(all))
        }
        Err(e) => {
            tracing::warn!(error = %e, "PTY holder did not list its terminals");
            (Some(holder), HeldSet::default())
        }
    }
}

/// The hooks' listener, on the port held sessions already post to when
/// there are any (so they keep reporting), else on any free port.
async fn hook_listener(port: Option<u16>) -> Result<tokio::net::TcpListener> {
    if let Some(port) = port {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) => tracing::error!(port, error = %e,
                "the held sessions' hook port is taken; their hooks go unheard until they are restarted"),
        }
    }
    Ok(tokio::net::TcpListener::bind("127.0.0.1:0").await?)
}

fn dedup(members: Vec<Member>) -> Vec<Member> {
    let mut seen = HashSet::new();
    members
        .into_iter()
        .filter(|m| seen.insert(m.key()))
        .collect()
}

fn folder_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

fn find_transcript(home: &Path, session_id: &str) -> Option<String> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|d| d.path().join(&file))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

async fn persist_loop(
    store: Arc<DeskStore>,
    turns: Option<Arc<dyn TurnLog>>,
    mut rx: mpsc::UnboundedReceiver<Persist>,
) {
    while let Some(op) = rx.recv().await {
        let result = match op {
            Persist::Put(r) => store.put(&r).await,
            Persist::Delete(id) => store.delete(id).await,
            Persist::Event(e) => store.add_event(&e).await,
            Persist::Terminal(t) => store.put_terminal(&t).await,
            Persist::DeleteTerminal(id) => store.delete_terminal(id).await,
            Persist::Turn(op) => {
                if let Some(turns) = &turns {
                    turns.send(*op);
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::error!(error = %e, "desk registry write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{build_pool, tests::record_fixture};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use workforce_postgres::test_support::with_test_pool;

    /// A stand-in CLI that stays up until killed, ignoring its arguments.
    fn fake_cli(dir: &Path) -> PathBuf {
        let path = dir.join("fake-cli");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn options(dir: &Path) -> Options {
        Options {
            claude: fake_cli(dir),
            codex: fake_cli(dir),
            env: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("CLAUDE_CODE_CHILD_SESSION".into(), "1".into()),
            ]),
            run_dir: dir.join("run"),
            scrollback_bytes: 4096,
            approval_timeout: Duration::from_secs(30),
            restore: RestorePolicy::Ask,
            adopt_every: None,
            holder: None,
        }
    }

    async fn desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        Desk::start(
            DeskStore::from_pool(pool),
            options(dir),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    fn token(d: &Desk, id: Uuid) -> String {
        d.0.state.lock().live[&id].token.clone()
    }

    async fn new_session(d: &Desk, dir: &Path) -> SessionView {
        d.create(
            NewSession {
                engine: Engine::Claude,
                name: "auth-fix".into(),
                cwd: dir.to_string_lossy().into_owned(),
                goal: "fix the race".into(),
                worktree: None,
                prompt: None,
                approvals: None,
                env: Default::default(),
            },
            None,
        )
        .await
        .unwrap()
    }

    async fn post(d: &Desk, id: Uuid, body: Value) -> Value {
        let t = token(d, id);
        d.hook(id, &t, body.to_string().as_bytes()).await.unwrap()
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

    #[test]
    fn plugin_dirs_are_the_persons_then_desks_each_once() {
        let v = |s: &str| Some(s.to_string());
        let dirs = |a: Option<String>, b: Option<String>| plugin_dirs(a.as_ref(), b.as_ref());
        assert_eq!(dirs(v("/mine:/also"), None), v("/mine:/also"));
        assert_eq!(dirs(None, v("/plugins/desk")), v("/plugins/desk"));
        assert_eq!(
            dirs(v("/mine"), v("/plugins/desk")),
            v("/mine:/plugins/desk")
        );
        assert_eq!(
            dirs(v("/mine:/plugins/desk"), v("/plugins/desk:/mine")),
            v("/mine:/plugins/desk")
        );
        assert_eq!(dirs(v(" : "), v("")), None);
    }

    #[test]
    fn cli_env_drops_inherited_claude_markers() {
        let env = cli_env(&BTreeMap::from([
            ("CLAUDE_CODE_CHILD_SESSION".into(), "1".into()),
            ("CLAUDECODE".into(), "1".into()),
            ("HOME".into(), "/Users/k".into()),
            ("ANTHROPIC_API_KEY".into(), "theirs".into()),
            ("WORKFORCE_GATEWAY_URL".into(), "http://127.0.0.1:1".into()),
            ("DESK_HOME".into(), "/tmp/desk".into()),
            ("KEYBOARD_LAYOUT".into(), "uk".into()),
        ]));
        assert!(!env.keys().any(|k| k.starts_with("CLAUDE")));
        assert_eq!(env["HOME"], "/Users/k");
        assert_eq!(env["ANTHROPIC_API_KEY"], "theirs", "the person's own");
        assert_eq!(env["WORKFORCE_GATEWAY_URL"], "http://127.0.0.1:1");
        assert_eq!(env["DESK_HOME"], "/tmp/desk");
        assert_eq!(env["KEYBOARD_LAYOUT"], "uk");
        assert_eq!(env["TERM"], "xterm-256color");
    }

    /// What Desk Helper hands the hub, and the PTY holder's token, never
    /// reach a session, a terminal or a CLI the hub runs.
    #[test]
    fn cli_env_drops_the_hubs_secrets() {
        let secrets = [
            crate::holder::TOKEN_ENV,
            "WORKFORCE_GATEWAY_TOKEN",
            "WORKFORCE_JWT_SECRET",
            "WORKFORCE_VAULT_KEY",
            "WORKFORCE_VAULT_NEW_KEY",
            "WORKFORCE_PAIRING_TOKEN",
            "WORKFORCE_GOOGLE_CLIENT_SECRET",
            "WORKFORCE_VELLEV_API_KEY",
            "DESK_POSTGRES_PASSWORD",
        ];
        let env = cli_env(
            &secrets
                .iter()
                .map(|k| (k.to_string(), "s3cret".to_string()))
                .collect(),
        );
        for k in secrets {
            assert!(!env.contains_key(k), "{k}");
        }
        assert!(!env.values().any(|v| v == "s3cret"));
    }

    #[tokio::test]
    async fn sessions_running_at_shutdown_are_offered_back_after_a_restart() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let working = record_fixture("working");
            let mut idle = record_fixture("idle");
            idle.state = SessionState::Idle;
            let mut stopped = record_fixture("stopped");
            stopped.state = SessionState::Stopped;
            for r in [&working, &idle, &stopped] {
                store.put(r).await.unwrap();
            }

            let d = desk(pool.clone(), dir.path()).await;
            let mut offer: Vec<_> = d
                .restore_offer()
                .into_iter()
                .map(|s| (s.name, s.state))
                .collect();
            offer.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                offer,
                vec![
                    ("idle".to_string(), SessionState::Stopped),
                    ("working".to_string(), SessionState::Interrupted)
                ]
            );

            d.dismiss_restore();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(d.restore_offer().is_empty());
            let saved = DeskStore::from_pool(pool)
                .get(working.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.state, SessionState::Interrupted);
            assert!(!saved.restore_offer);
        })
        .await;
    }

    #[tokio::test]
    async fn sessions_the_holder_kept_are_taken_back_as_they_were_after_a_hub_restart() {
        with_test_pool(build_pool, |pool| async move {
            let (_hdir, socket, holder_cancel) = crate::holder::tests::holder().await;
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
            opts.restore = RestorePolicy::Auto;
            opts.holder = Some(HolderOptions {
                socket: socket.clone(),
                token: crate::holder::tests::TOKEN.into(),
            });
            DeskStore::migrate(&pool).await.unwrap();
            let first_cancel = CancellationToken::new();
            let first = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts.clone(),
                Default::default(),
                first_cancel.clone(),
            )
            .await
            .unwrap();
            let s = new_session(&first, dir.path()).await;
            let token = token(&first, s.id);
            let pid = first.attach(s.id).unwrap().pid();
            post(
                &first,
                s.id,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"}),
            )
            .await;
            let port = first.0.hook_port;
            tokio::time::sleep(Duration::from_millis(200)).await;
            first_cancel.cancel();
            tokio::time::sleep(Duration::from_millis(300)).await;

            let second = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts.clone(),
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let v = second.view(s.id).unwrap();
            assert!(v.live, "taken back");
            assert_eq!(v.state, SessionState::Working, "not interrupted");
            assert!(second.restore_offer().is_empty(), "nothing to restore");
            assert_eq!(
                second.attach(s.id).unwrap().pid(),
                pid,
                "the same process, not relaunched"
            );
            assert_eq!(second.0.hook_port, port, "hooks post where they did");
            // The hook URL the running CLI was given still works.
            let url = format!("http://127.0.0.1:{port}/hook/{}/{token}", s.id);
            let answer = reqwest::Client::new()
                .post(url)
                .json(&json!({"hook_event_name": "Stop", "last_assistant_message": "All green."}))
                .send()
                .await
                .unwrap();
            assert!(answer.status().is_success());
            let v = second.view(s.id).unwrap();
            assert_eq!(
                (v.state, v.last_reply.as_deref()),
                (SessionState::Done, Some("All green."))
            );

            // Stopping it ends the held process for good.
            second.stop(s.id, None).unwrap();
            for _ in 0..100 {
                if !second.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(second.view(s.id).unwrap().state, SessionState::Stopped);
            let h = crate::holder::Holder::connect(&socket, crate::holder::tests::TOKEN)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(h.list().await.unwrap().is_empty());
            holder_cancel.cancel();
        })
        .await;
    }

    #[tokio::test]
    async fn without_its_holder_the_hub_runs_terminals_itself() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
            opts.holder = Some(HolderOptions {
                socket: dir.path().join("nobody.sock"),
                token: "irrelevant-token-123".into(),
            });
            DeskStore::migrate(&pool).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool),
                opts,
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let s = new_session(&d, dir.path()).await;
            assert!(s.live);
        })
        .await;
    }

    /// Keeps every reading the desk passes on.
    #[derive(Default)]
    struct Readings(Mutex<Vec<(MacPlan, Value, crate::usage::MacAccount)>>);

    impl PlanSink for Readings {
        fn mac(&self, plan: MacPlan, payload: Value, account: crate::usage::MacAccount) {
            self.0.lock().push((plan, payload, account));
        }
    }

    /// A stand-in for both CLIs that answers who it is signed in as, and
    /// otherwise keeps its environment where the test reads it.
    fn signed_in_cli(dir: &Path) -> PathBuf {
        let path = dir.join("signed-in-cli");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n\
                 'auth status') echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"orgId\":\"org-1\",\"email\":\"person@example.com\",\"subscriptionType\":\"max\"}}' ;;\n\
                 'login status') echo 'Logged in using ChatGPT' >&2 ;;\n\
                 *) env > \"{}/env-$$\"; exec sleep 60 ;;\n\
                 esac\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn this_macs_plan_readings_reach_the_hub_as_its_sign_in() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let readings = Arc::new(Readings::default());
            let cli = signed_in_cli(dir.path());
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    claude: cli.clone(),
                    codex: cli,
                    ..options(dir.path())
                },
                Floor {
                    turns: None,
                    plans: Some(readings.clone()),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let max = crate::usage::MacAccount {
                ids: vec!["org-1".into(), "person@example.com".into()],
                plan: Some("max".into()),
            };
            for _ in 0..200 {
                if d.sign_ins().claude.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(
                d.sign_ins(),
                MacSignIns {
                    claude: Some(max.clone()),
                    codex: true
                }
            );

            let s = d
                .create(
                    NewSession {
                        engine: Engine::Claude,
                        name: "auth-fix".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: String::new(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        env: BTreeMap::from([(PLUGIN_DIRS.into(), "/plugins/desk".into())]),
                    },
                    None,
                )
                .await
                .unwrap();
            let spawned = loop {
                let found = std::fs::read_dir(dir.path())
                    .unwrap()
                    .flatten()
                    .find(|e| e.file_name().to_string_lossy().starts_with("env-"))
                    .and_then(|e| std::fs::read_to_string(e.path()).ok())
                    .filter(|env| env.contains("DESK_USAGE_URL"));
                if let Some(env) = found {
                    break env;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            assert!(
                spawned.contains(&format!("DESK_USAGE_URL=http://127.0.0.1:{}/hook/{}/", d.0.hook_port, s.id)),
                "{spawned}"
            );
            assert!(spawned.contains("CLAUDE_CODE_PLUGIN_DIRS=/plugins/desk"), "{spawned}");

            let limits = json!([{"kind": "five_hour", "percentUsed": 62, "resetsAt": "2026-10-29T19:10:00Z"}]);
            post(&d, s.id, json!({"hook_event_name": "DeskUsage", "rate_limits": limits})).await;
            assert_eq!(
                readings.0.lock().clone(),
                [(MacPlan::Claude, limits, max)],
                "the reading is the session's sign-in's"
            );
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Starting, "and changes nothing of it");

            assert!(d
                .create(
                    NewSession {
                        engine: Engine::Claude,
                        name: "bad".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: String::new(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        env: BTreeMap::from([("NOT A NAME".into(), "x".into())]),
                    },
                    None,
                )
                .await
                .is_err());
        })
        .await;
    }

    /// Keeps every op the desk logs.
    #[derive(Default)]
    struct Logged(Mutex<Vec<TurnOp>>);

    #[async_trait::async_trait]
    impl TurnLog for Logged {
        fn send(&self, op: TurnOp) {
            self.0.lock().push(op);
        }

        async fn relevant(
            &self,
            _: &crate::floor::Scope,
            _: DateTime<Utc>,
        ) -> eyre::Result<Vec<crate::floor::Turn>> {
            Ok(vec![])
        }
    }

    /// Whether what `logged` holds comes to satisfy `f`.
    async fn logs(logged: &Logged, f: impl Fn(&[TurnOp]) -> bool) -> bool {
        for _ in 0..200 {
            if f(&logged.0.lock()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// A stand-in for Claude Code that leaves one of its Bash tool shells
    /// running, its pid where the test reads it once it runs as that shell.
    fn shell_cli(dir: &Path) -> PathBuf {
        let path = dir.join("shell-cli");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n/bin/sh -c 'echo $$ > \"$1\"; sleep 60; true' \
                 {dir}/shell-snapshots/snapshot-zsh-1.sh \"{dir}/shell-$$\" &\nexec sleep 60\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn work_a_turn_left_running_keeps_it_from_done_until_the_last_of_it_ends() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let logged = Arc::new(Logged::default());
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    claude: shell_cli(dir.path()),
                    ..options(dir.path())
                },
                Floor {
                    turns: Some(logged.clone()),
                    plans: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let shell_of = |id: Uuid| {
                let cli = d.0.state.lock().live[&id].pty.pid().unwrap();
                let file = dir.path().join(format!("shell-{cli}"));
                (0..200)
                    .find_map(|_| {
                        let pid = std::fs::read_to_string(&file)
                            .ok()
                            .and_then(|p| p.trim().parse::<i32>().ok());
                        if pid.is_none() {
                            std::thread::sleep(Duration::from_millis(25));
                        }
                        pid
                    })
                    .expect("the shell started")
            };

            let s = new_session(&d, dir.path()).await;
            let shell = shell_of(s.id);
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "start the server and a check"})).await;
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Started both.",
                "background_tasks": [
                    {"id": "a1", "type": "subagent", "status": "running"},
                    {"id": "b1", "type": "shell", "status": "running"}],
                "session_crons": []})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.waiting_on()), (SessionState::Done, 2), "not done");
            let ended = || {
                logged
                    .0
                    .lock()
                    .iter()
                    .filter(|op| matches!(op, TurnOp::End { .. }))
                    .count()
            };
            assert!(
                logs(&logged, |ops| ops
                    .iter()
                    .any(|op| matches!(op, TurnOp::Background { count: 2, .. })))
                .await
            );
            assert_eq!(ended(), 0);

            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash", "tool_input": {"command": "cargo test"}})).await;
            assert_eq!(
                d.view(s.id).unwrap().state,
                SessionState::Done,
                "the sub-agent's tool is its own, not the session working"
            );
            unsafe { libc::kill(shell, libc::SIGKILL) };
            for _ in 0..200 {
                d.look_at_background(&[s.id]).await;
                if d.view(s.id).unwrap().waiting_on() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(d.view(s.id).unwrap().waiting_on(), 1, "the shell is gone");

            post(&d, s.id, json!({"hook_event_name": "SubagentStop", "agent_id": "a1"})).await;
            assert_eq!(
                d.view(s.id).unwrap().state,
                SessionState::Working,
                "Claude Code goes on once the last of it ends"
            );
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit",
                "prompt": "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"})).await;
            assert_eq!(
                d.view(s.id).unwrap().last_prompt.as_deref(),
                Some("start the server and a check"),
                "its own prompt is not the person's"
            );
            assert_eq!(ended(), 0);
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Both done.",
                "background_tasks": []})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.background), (SessionState::Done, None));
            assert!(
                logs(&logged, |ops| ops.iter().any(|op| matches!(op, TurnOp::End { .. }))).await,
                "the turn ended"
            );
            assert_eq!(ended(), 1, "one turn, ended once");
            assert_eq!(
                logged
                    .0
                    .lock()
                    .iter()
                    .filter(|op| matches!(op, TurnOp::Begin { .. }))
                    .count(),
                1
            );

            let older = new_session(&d, dir.path()).await;
            shell_of(older.id);
            post(&d, older.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "serve it"})).await;
            post(&d, older.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Serving."})).await;
            assert_eq!(
                d.view(older.id).unwrap().waiting_on(),
                1,
                "a CLI that reports nothing: its shell is found running"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_forgotten_session_leaves_no_turn_running() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let logged = Arc::new(Logged::default());
            let d = Desk::start(
                DeskStore::from_pool(pool),
                options(dir.path()),
                Floor {
                    turns: Some(logged.clone()),
                    plans: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let s = new_session(&d, dir.path()).await;
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"})).await;
            assert_eq!(d.open_turns().len(), 1);
            d.forget(s.id).await.unwrap();
            assert!(d.open_turns().is_empty(), "nothing runs it");
            assert!(
                logs(&logged, |ops| ops.iter().any(|op| matches!(
                    op,
                    TurnOp::End {
                        outcome: Outcome::Stopped,
                        ..
                    }
                )) && ops.iter().any(|op| matches!(op, TurnOp::Settle { .. })))
                .await,
                "its turn ended, and what it asked with it"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn hooks_drive_the_session_through_its_states() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            assert!(s.live);
            assert_eq!(s.cli_session_id.as_deref(), Some(s.id.to_string().as_str()));

            let transcript = dir.path().join("t.jsonl");
            std::fs::write(
                &transcript,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All green."}]}}"#,
            )
            .unwrap();
            post(&d, s.id, json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string(), "transcript_path": transcript})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Idle);

            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo test"}})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.last_prompt.as_deref(), v.current_tool.as_deref()),
                (SessionState::Working, Some("run the tests"), Some("Bash(cargo test)")));

            post(&d, s.id, json!({"hook_event_name": "Stop"})).await;
            assert_eq!(d.view(s.id).unwrap().last_reply.as_deref(), Some("All green."));
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Pushed."})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.last_reply.as_deref(), v.current_tool), (SessionState::Done, Some("Pushed."), None));

            assert!(d.hook(s.id, "wrong-token", b"{}").await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn an_approval_answered_in_desk_is_returned_to_claude_code() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            let waiting = tokio::spawn({
                let d = d.clone();
                let id = s.id;
                async move {
                    post(&d, id, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "rm -rf target"}})).await
                }
            });
            let pending = loop {
                if let Some(p) = d.view(s.id).unwrap().pending {
                    break p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert!(pending.answerable);
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
            assert!(d.decide(s.id, Uuid::new_v4(), Decision::Allow, None).is_err());

            d.decide(s.id, pending.id, Decision::Allow, None).unwrap();
            assert_eq!(waiting.await.unwrap(), claude::allow());
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Working, None));
        })
        .await;
    }

    /// The kinds and texts of everything recorded, oldest first, once
    /// `n` events are in.
    async fn recorded(d: &Desk, n: usize) -> Vec<(crew::Kind, String, crew::PartyKind)> {
        for _ in 0..200 {
            let page = d.activity(crew::Filter::All, None, 100).await.unwrap();
            if page.events.len() >= n {
                return page
                    .events
                    .into_iter()
                    .rev()
                    .map(|e| (e.kind, e.text, e.actor.kind))
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fewer than {n} events were recorded");
    }

    #[tokio::test]
    async fn a_sessions_life_is_recorded_as_activity() {
        use crew::{Kind as K, PartyKind as P};
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let (_, mut live) = d.subscribe();
            let s = d
                .create(
                    NewSession {
                        engine: Engine::Claude,
                        name: "acme-web".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: "upgrade the date picker".into(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        env: Default::default(),
                    },
                    Some(By::you()),
                )
                .await
                .unwrap();
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "Upgrade the picker. Then test."})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "pwd"}})).await;
            let waiting = tokio::spawn({
                let d = d.clone();
                let id = s.id;
                async move {
                    post(&d, id, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "npm install @acme/ds@4.2.0"}})).await
                }
            });
            let pending = loop {
                if let Some(p) = d.view(s.id).unwrap().pending {
                    break p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            d.decide(s.id, pending.id, Decision::Allow, Some(By::you())).unwrap();
            waiting.await.unwrap();
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Upgraded. Should I deploy main or release/4.2?"})).await;
            d.stop(s.id, Some(By::manager("desk", "Desk", "It is done."))).unwrap();
            let events = recorded(&d, 7).await;
            assert_eq!(
                events,
                vec![
                    (K::Started, "started acme-web".into(), P::You),
                    (K::Started, "started “Upgrade the picker.”".into(), P::Session),
                    (K::Working, "is running Bash(ls)".into(), P::Session),
                    (K::NeedsYou, "wants to run npm install @acme/ds@4.2.0".into(), P::Session),
                    (K::Approved, "allowed acme-web to run npm install @acme/ds@4.2.0".into(), P::You),
                    (K::Question, "asked “Should I deploy main or release/4.2?”".into(), P::Session),
                    (K::Stopped, "stopped acme-web".into(), P::Manager),
                ]
            );
            let page = d.activity(crew::Filter::All, None, 100).await.unwrap();
            let needs = page.events.iter().find(|e| e.kind == K::NeedsYou).unwrap();
            assert_eq!(needs.approval_id.as_deref(), Some(pending.id.to_string().as_str()));
            assert_eq!(needs.subject.as_ref().unwrap().id, s.id.to_string());
            let stopped = &page.events[0];
            assert_eq!(stopped.reason.as_deref(), Some("It is done."));
            // The stop the overseer asked for is not recorded twice.
            for _ in 0..100 {
                if !d.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(d.activity(crew::Filter::All, None, 100).await.unwrap().events.len(), 7);
            let mut seen = false;
            while let Ok(e) = live.try_recv() {
                seen |= matches!(e, DeskEvent::Activity { .. });
            }
            assert!(seen, "activity goes out live");
        })
        .await;
    }

    #[tokio::test]
    async fn terminal_approvals_answer_at_once_and_only_show_as_waiting() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            d.update(s.id, SessionPatch { approvals: Some(Approvals::Terminal), ..Default::default() }).unwrap();
            let answer = post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "tool_name": "Edit", "tool_input": {"file_path": "a.rs"}})).await;
            assert_eq!(answer, claude::ask_in_terminal());
            let p = d.view(s.id).unwrap().pending.unwrap();
            assert!(!p.answerable);
            assert!(d.decide(s.id, p.id, Decision::Allow, None).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn stopping_a_session_records_it_as_stopped_and_resume_relaunches_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            d.stop(s.id, None).unwrap();
            for _ in 0..100 {
                if !d.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Stopped);
            let back = d.resume(s.id, Some("continue".into()), None).await.unwrap();
            assert!(back.live);
            assert_eq!(back.last_prompt.as_deref(), Some("continue"));
            assert!(d.resume(s.id, None, None).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn an_agent_can_prompt_a_session_and_wait_for_it_to_finish() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            assert!(d.prompt(s.id, "  ", None).is_err());
            d.prompt(s.id, "run the tests\nthen push", None).unwrap();

            let quick = d
                .wait_for(s.id, &[SessionState::Done], Duration::from_millis(50))
                .await
                .unwrap();
            assert_eq!(quick.state, SessionState::Starting);

            let waiting = tokio::spawn({
                let d = d.clone();
                async move {
                    d.wait_for(
                        s.id,
                        &[SessionState::Done, SessionState::Blocked],
                        Duration::from_secs(10),
                    )
                    .await
                }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            post(
                &d,
                s.id,
                json!({"hook_event_name": "Stop", "last_assistant_message": "Pushed."}),
            )
            .await;
            let done = waiting.await.unwrap().unwrap();
            assert_eq!(
                (done.state, done.last_reply.as_deref()),
                (SessionState::Done, Some("Pushed."))
            );
        })
        .await;
    }

    #[tokio::test]
    async fn rooms_hold_their_members_to_the_loop_guard() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let allan = Member {
                hub: "dw".into(),
                agent: "allan".into(),
            };
            let scout = Member {
                hub: "local".into(),
                agent: "scout".into(),
            };
            let room = d
                .create_room(NewRoom {
                    name: "Release".into(),
                    members: vec![allan.clone(), scout.clone(), allan.clone()],
                    default_member: None,
                })
                .await
                .unwrap();
            assert_eq!((room.slug.as_str(), room.members.len()), ("release", 2));
            assert!(d
                .create_room(NewRoom {
                    name: "release".into(),
                    members: vec![],
                    default_member: None
                })
                .await
                .is_err());

            let post = |author: &str, hop: i16| NewMessage {
                author: author.into(),
                content: "on it".into(),
                thread_id: None,
                hop,
                attachments: Vec::new(),
                source: None,
                delivery: None,
                request_id: None,
            };
            d.post(room.id, post(YOU, 0)).await.unwrap();
            // Files may stand in for text, and come back with the message.
            let report: crate::attachments::Attachment =
                serde_json::from_value(serde_json::json!({
                    "artifact_id": "art_1", "media_type": "application/pdf",
                    "filename": "report.pdf", "size": 9, "hub": "dw", "agent": "allan",
                }))
                .unwrap();
            let filed = d
                .post(
                    room.id,
                    NewMessage {
                        content: String::new(),
                        attachments: vec![report.clone()],
                        ..post(YOU, 0)
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                d.room_messages(room.id, None, None, 50)
                    .await
                    .unwrap()
                    .last()
                    .unwrap()
                    .attachments,
                vec![report.clone()]
            );
            assert_eq!(filed.message.attachments, vec![report.clone()]);
            assert!(matches!(
                d.post(
                    room.id,
                    NewMessage {
                        content: String::new(),
                        ..post(YOU, 0)
                    }
                )
                .await,
                Err(DeskError::Invalid(_))
            ));
            let unnamed = crate::attachments::Attachment {
                filename: String::new(),
                ..report
            };
            assert!(matches!(
                d.post(
                    room.id,
                    NewMessage {
                        attachments: vec![unnamed],
                        ..post(YOU, 0)
                    }
                )
                .await,
                Err(DeskError::Invalid(_))
            ));
            assert!(matches!(
                d.post(room.id, post("dw/nobody", 0)).await,
                Err(DeskError::Invalid(_))
            ));
            assert!(matches!(
                d.post(room.id, post("dw/allan", MAX_HOPS + 1)).await,
                Err(DeskError::Conflict(_))
            ));
            for i in 0..TURN_BUDGET {
                let who = if i % 2 == 0 {
                    "dw/allan"
                } else {
                    "local/scout"
                };
                d.post(room.id, post(who, 1)).await.unwrap();
            }
            assert!(matches!(
                d.post(room.id, post("dw/allan", 1)).await,
                Err(DeskError::Conflict(_))
            ));
            d.post(room.id, post(YOU, 0)).await.unwrap();
            d.post(room.id, post("dw/allan", 1)).await.unwrap();

            let removed = d
                .update_room(
                    room.id,
                    RoomPatch {
                        members: Some(vec![scout]),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(removed.default_member, None);
            d.delete_room(room.id).await.unwrap();
            assert!(d.rooms().is_empty());
        })
        .await;
    }

    /// A room's channel outlives its name; a sourced message is kept once,
    /// announced once, and placed in the timeline when its thread is gone;
    /// a delivery skips the loop guard and may be from a former member.
    #[tokio::test]
    async fn rooms_keep_their_channel_and_take_sourced_deliveries_once() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let scout = Member {
                hub: crate::rooms::LOCAL_HUB.into(),
                agent: "scout".into(),
            };
            let room = d
                .create_room(NewRoom {
                    name: "Release".into(),
                    members: vec![scout.clone()],
                    default_member: None,
                })
                .await
                .unwrap();
            assert_eq!(room.channel, channel_for(room.id));
            let renamed = d
                .update_room(
                    room.id,
                    RoomPatch {
                        name: Some("Launch".into()),
                        members: Some(vec![]),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                (renamed.slug.as_str(), renamed.channel.as_str()),
                ("launch", room.channel.as_str())
            );
            assert_eq!(d.room(room.id), Some(renamed));

            let (_, mut events) = d.subscribe();
            let ask = d
                .post(
                    room.id,
                    NewMessage {
                        author: YOU.into(),
                        content: "remind me at five".into(),
                        thread_id: None,
                        hop: 0,
                        attachments: Vec::new(),
                        source: None,
                        delivery: None,
                        request_id: None,
                    },
                )
                .await
                .unwrap()
                .message;
            for _ in 0..TURN_BUDGET {
                d.post(
                    room.id,
                    NewMessage {
                        author: "dw/allan".into(),
                        content: "noted".into(),
                        thread_id: Some(ask.id),
                        hop: 0,
                        attachments: Vec::new(),
                        source: None,
                        delivery: Some(workforce_core::desk::DeliveryKind::Notice),
                        request_id: None,
                    },
                )
                .await
                .unwrap();
            }
            let reminder = NewMessage {
                author: "local/scout".into(),
                content: "🔔 Reminder: five o'clock".into(),
                thread_id: Some(ask.id),
                hop: 2,
                attachments: Vec::new(),
                source: Some(crate::rooms::source_key("local", "scout", "m-1")),
                delivery: Some(workforce_core::desk::DeliveryKind::Reminder),
                request_id: None,
            };
            let first = d.post(room.id, reminder.clone()).await.unwrap();
            assert!(first.created, "past the loop guard, from a former member");
            assert_eq!(
                (first.message.thread_id, first.message.hop),
                (Some(ask.id), 0)
            );
            let again = d.post(room.id, reminder.clone()).await.unwrap();
            assert_eq!(
                (again.created, again.message),
                (false, first.message.clone())
            );
            let announced: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
                .filter(
                    |e| matches!(e, DeskEvent::RoomMessage { message } if message.source.is_some()),
                )
                .collect();
            assert_eq!(announced.len(), 1, "a repeat is not announced again");

            let lost = d
                .post(
                    room.id,
                    NewMessage {
                        thread_id: Some(Uuid::new_v4()),
                        source: Some(crate::rooms::source_key("local", "scout", "m-2")),
                        content: "job done".into(),
                        ..reminder.clone()
                    },
                )
                .await
                .unwrap()
                .message;
            assert_eq!(lost.thread_id, None);
            assert_eq!(lost.content, format!("{THREAD_GONE} job done"));
            assert!(matches!(
                d.post(
                    room.id,
                    NewMessage {
                        thread_id: Some(Uuid::new_v4()),
                        source: None,
                        ..reminder.clone()
                    }
                )
                .await,
                Err(DeskError::Invalid(_))
            ));
            for author in [YOU, "scout"] {
                assert!(matches!(
                    d.post(
                        room.id,
                        NewMessage {
                            author: author.into(),
                            source: None,
                            ..reminder.clone()
                        }
                    )
                    .await,
                    Err(DeskError::Invalid(_))
                ));
            }
        })
        .await;
    }

    #[tokio::test]
    async fn sessions_in_other_terminals_are_adopted_and_closed_when_they_go() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let ext = |busy: bool| {
                adopt::parse(
                    json!([{ "pid": std::process::id(), "cwd": "/Users/k/dev/vellev", "kind": "interactive",
                        "sessionId": "ext-1", "name": "vellev-36", "status": if busy { "busy" } else { "idle" } }])
                    .to_string()
                    .as_bytes(),
                )
                .unwrap()
            };
            d.reconcile_external(ext(true)).await;
            let s = d.list().into_iter().find(|s| s.name == "vellev-36").unwrap();
            assert_eq!((s.origin, s.state, s.live), (Origin::External, SessionState::Working, false));
            assert!(d.resume(s.id, None, None).await.is_err(), "still running elsewhere");

            d.reconcile_external(ext(false)).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Idle);

            d.reconcile_external(vec![]).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Closed);
            assert!(d.list().iter().all(|v| v.id != s.id));
        })
        .await;
    }
}

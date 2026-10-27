//! The desk: every session the hub hosts or has seen, their live terminals,
//! and the registry that lets them come back after a restart.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use eyre::Result;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::adopt::{self, External};
use crate::claude::{self, Hook};
use crate::codex::{self, AppServer, CodexEvent, Seen};
use crate::model::{
    Approvals, Decision, DeskEvent, Engine, NewSession, Origin, PendingApproval, SessionPatch,
    SessionRecord, SessionState, SessionView,
};
use crate::pty::{Exit, Pty};
use crate::rooms::{
    slug_for, Member, NewMessage, NewRoom, Room, RoomMessage, RoomPatch, SearchHit, DESK, MAX_HOPS,
    TURN_BUDGET, YOU,
};
use crate::store::DeskStore;
use crate::transcript;
use crate::worktree;

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

struct Live {
    pty: Arc<Pty>,
    token: String,
    current_tool: Option<String>,
    pending: Option<PendingApproval>,
    answer: Option<Answer>,
    ended: bool,
    stopping: bool,
    _codex: Option<CodexLink>,
}

/// A Codex session's app-server and observer; dropping it stops both.
struct CodexLink {
    _server: AppServer,
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
}

enum Persist {
    Put(Box<SessionRecord>),
    Delete(Uuid),
}

struct Inner {
    opts: Options,
    store: Arc<DeskStore>,
    env: BTreeMap<String, String>,
    state: Mutex<State>,
    events: broadcast::Sender<DeskEvent>,
    persist: mpsc::UnboundedSender<Persist>,
    hook_base: String,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct Desk(Arc<Inner>);

fn cli_env(base: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .iter()
        .filter(|(k, _)| !k.starts_with("CLAUDE"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env
}

impl Desk {
    /// Loads the registry, marks what was running as interrupted or stopped
    /// (its processes did not survive the hub), and starts the hook endpoint
    /// and the adoption poller. `cancel` stops all of it and every session.
    pub async fn start(store: DeskStore, opts: Options, cancel: CancellationToken) -> Result<Self> {
        let store = Arc::new(store);
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
                    Origin::Desk => true,
                    Origin::External => !r.external_pid.is_some_and(adopt::alive),
                };
            if lost {
                r.state = r.state.after_loss();
                r.restore_offer = opts.restore != RestorePolicy::Off;
                r.updated_at = Utc::now();
                recovered.push(r.clone());
            }
            records.insert(r.id, r);
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let hook_base = format!("http://{}", listener.local_addr()?);
        let (persist, persist_rx) = mpsc::unbounded_channel();
        let desk = Self(Arc::new(Inner {
            env: cli_env(&opts.env),
            opts,
            store: Arc::clone(&store),
            state: Mutex::new(State {
                records,
                rooms,
                ..State::default()
            }),
            events: broadcast::channel(256).0,
            persist,
            hook_base,
            cancel: cancel.clone(),
        }));
        tokio::spawn(persist_loop(store, persist_rx));
        recovered.into_iter().for_each(|r| desk.save(r));

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
            desk.restore(&ids).await;
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

    fn shutdown(&self) {
        let live: Vec<_> = self.0.state.lock().live.drain().collect();
        drop(live);
    }

    fn view_of(state: &State, r: &SessionRecord) -> SessionView {
        let live = state.live.get(&r.id);
        SessionView::new(
            r,
            live.is_some(),
            live.and_then(|l| l.current_tool.clone()),
            live.and_then(|l| l.pending.clone()),
        )
    }

    fn save(&self, r: SessionRecord) {
        let view = {
            let mut st = self.0.state.lock();
            st.records.insert(r.id, r.clone());
            Self::view_of(&st, &r)
        };
        let _ = self.0.persist.send(Persist::Put(Box::new(r)));
        let _ = self.0.events.send(DeskEvent::Session {
            session: Box::new(view),
        });
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
        let r = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let Some(r) = st.records.get_mut(&id) else {
                return;
            };
            f(r, st.live.get_mut(&id));
            r.updated_at = Utc::now();
            r.clone()
        };
        self.save(r);
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

    pub async fn create(&self, new: NewSession) -> Result<SessionView, DeskError> {
        let name = new.name.trim();
        if name.is_empty() {
            return Err(DeskError::Invalid("a session needs a name".into()));
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
        self.view(id)
    }

    pub fn update(&self, id: Uuid, patch: SessionPatch) -> Result<SessionView, DeskError> {
        self.record(id)?;
        if patch.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(DeskError::Invalid("a session needs a name".into()));
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
        });
        self.view(id)
    }

    pub fn stop(&self, id: Uuid) -> Result<SessionView, DeskError> {
        let pty = {
            let mut st = self.0.state.lock();
            let live = st
                .live
                .get_mut(&id)
                .ok_or_else(|| DeskError::Conflict("session is not running".into()))?;
            live.stopping = true;
            Arc::clone(&live.pty)
        };
        pty.kill();
        self.view(id)
    }

    pub async fn resume(&self, id: Uuid, prompt: Option<String>) -> Result<SessionView, DeskError> {
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
        self.change(id, |r, _| {
            r.origin = Origin::Desk;
            r.external_pid = None;
            r.state = SessionState::Starting;
            r.restore_offer = false;
            if let Some(p) = prompt.clone() {
                r.last_prompt = Some(p);
            }
        });
        if let Err(e) = self.launch(&r, true, prompt.as_deref()).await {
            self.save(SessionRecord {
                updated_at: Utc::now(),
                ..r
            });
            return Err(e);
        }
        self.view(id)
    }

    /// Resumes each id, returning those that came back; failures are logged
    /// and leave the session on offer.
    pub async fn restore(&self, ids: &[Uuid]) -> Vec<SessionView> {
        let mut back = Vec::new();
        for &id in ids {
            match self.resume(id, None).await {
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

    pub fn forget(&self, id: Uuid) -> Result<(), DeskError> {
        let live = {
            let mut st = self.0.state.lock();
            st.records.remove(&id).ok_or(DeskError::NotFound)?;
            st.seen_transcripts.remove(&id);
            st.live.remove(&id)
        };
        drop(live);
        let _ = self.0.persist.send(Persist::Delete(id));
        let _ = self.0.events.send(DeskEvent::Removed { id });
        Ok(())
    }

    pub fn decide(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: Decision,
    ) -> Result<SessionView, DeskError> {
        let answer = {
            let mut st = self.0.state.lock();
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
                Decision::Allow | Decision::Deny => live.pending = None,
            }
            answer
        };
        let _ = answer.send(match decision {
            Decision::Allow => claude::allow(),
            Decision::Deny => claude::deny(),
            Decision::Terminal => claude::ask_in_terminal(),
        });
        self.change(id, |r, l| {
            if l.is_some_and(|l| l.pending.is_none()) {
                r.state = SessionState::Working;
            }
        });
        self.view(id)
    }

    /// Types a prompt into a running session's terminal and submits it, as a
    /// person at the keyboard would (bracketed paste, so newlines stay in
    /// the prompt).
    pub fn prompt(&self, id: Uuid, text: &str) -> Result<SessionView, DeskError> {
        if text.trim().is_empty() {
            return Err(DeskError::Invalid("the prompt is empty".into()));
        }
        let pty = self.attach(id)?;
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(text.as_bytes());
        input.extend_from_slice(b"\x1b[201~\r");
        pty.write(&input)?;
        self.view(id)
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
        let (bytes, _) = self.attach(id)?.attach();
        let text = crate::pty::plain_text(&bytes);
        let skip = text.chars().count().saturating_sub(max_chars);
        Ok(text.chars().skip(skip).collect())
    }

    pub fn rooms(&self) -> Vec<Room> {
        let mut rooms: Vec<_> = self.0.state.lock().rooms.values().cloned().collect();
        rooms.sort_by(|a, b| a.name.cmp(&b.name));
        rooms
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
        self.save_room(Room {
            id: Uuid::new_v4(),
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
    /// before you speak again.
    pub async fn post(&self, id: Uuid, new: NewMessage) -> Result<RoomMessage, DeskError> {
        let room = self.room_of(id)?;
        if new.content.trim().is_empty() {
            return Err(DeskError::Invalid("the message is empty".into()));
        }
        let is_agent = new.author != YOU && new.author != DESK;
        if is_agent && !room.members.iter().any(|m| m.key() == new.author) {
            return Err(DeskError::Invalid(format!(
                "{} is not a member of the room",
                new.author
            )));
        }
        if let Some(t) = new.thread_id {
            let parent = self.0.store.message(t).await?;
            if parent.is_none_or(|p| p.room_id != id || p.thread_id.is_some()) {
                return Err(DeskError::Invalid(
                    "threads hang off a message in this room's timeline".into(),
                ));
            }
        }
        if is_agent {
            if new.hop > MAX_HOPS {
                return Err(DeskError::Conflict(format!(
                    "loop guard: agents have handed this on {MAX_HOPS} times already"
                )));
            }
            if self.0.store.agent_run(id, new.thread_id).await? >= TURN_BUDGET {
                return Err(DeskError::Conflict(format!(
                    "loop guard: {TURN_BUDGET} agent messages since you last spoke"
                )));
            }
        }
        let message = RoomMessage {
            id: Uuid::new_v4(),
            room_id: id,
            thread_id: new.thread_id,
            author: new.author,
            content: new.content,
            hop: if is_agent { new.hop.max(0) } else { 0 },
            created_at: Utc::now(),
        };
        self.0.store.add_message(&message).await?;
        let _ = self.0.events.send(DeskEvent::RoomMessage {
            message: Box::new(message.clone()),
        });
        Ok(message)
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
                    env: &self.0.env,
                });
                (spec, None)
            }
            Engine::Codex => {
                let socket = self.0.opts.run_dir.join(format!("{}.sock", r.id.simple()));
                let server = AppServer::start(&self.0.opts.codex, socket, &self.0.env).await?;
                let spec = codex::spawn(&codex::CodexSpawn {
                    program: &self.0.opts.codex,
                    cwd,
                    socket: &server.socket,
                    launch: match (resume, r.cli_session_id.as_deref()) {
                        (true, Some(thread_id)) => codex::Launch::Resume { thread_id },
                        _ => codex::Launch::New,
                    },
                    prompt,
                    env: &self.0.env,
                });
                let watch = self.0.cancel.child_token();
                let (tx, rx) = mpsc::channel(64);
                tokio::spawn({
                    let socket = server.socket.clone();
                    let watch = watch.clone();
                    async move {
                        if let Err(e) = codex::observe(socket, tx, watch).await {
                            tracing::warn!(error = %e, "codex observer stopped");
                        }
                    }
                });
                tokio::spawn(self.clone().codex_loop(r.id, rx));
                (
                    spec,
                    Some(CodexLink {
                        _server: server,
                        watch,
                    }),
                )
            }
        };
        let (pty, exit) = Pty::spawn(&spec, self.0.opts.scrollback_bytes)?;
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
            },
        );
        tokio::spawn(self.clone().on_exit(r.id, exit));
        Ok(())
    }

    async fn on_exit(self, id: Uuid, exit: oneshot::Receiver<Exit>) {
        let code = exit.await.ok().flatten();
        let live = self.0.state.lock().live.remove(&id);
        let Some(live) = live else { return };
        tracing::info!(%id, ?code, "desk session exited");
        let clean = live.ended || live.stopping;
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
        Some(self.apply_hook(id, hook).await)
    }

    async fn apply_hook(&self, id: Uuid, hook: Hook) -> Value {
        let empty = || Value::Object(Default::default());
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
            Hook::Stop { transcript, reply } => {
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
                    }
                });
            }
            Hook::SessionEnd => {
                if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                    l.ended = true;
                }
            }
            Hook::Permission { tool, detail } => {
                return self.await_permission(id, tool, detail).await
            }
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
        let wait = self
            .0
            .opts
            .approval_timeout
            .saturating_sub(Duration::from_secs(5));
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(answer)) => answer,
            _ => {
                if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                    if let Some(p) = l.pending.as_mut().filter(|p| p.id == pending.id) {
                        p.answerable = false;
                    }
                    l.answer = None;
                }
                self.publish(id);
                claude::ask_in_terminal()
            }
        }
    }

    async fn codex_loop(self, id: Uuid, mut rx: mpsc::Receiver<Seen>) {
        let home = self.0.env.get("HOME").map(PathBuf::from);
        while let Some(Seen { thread, event }) = rx.recv().await {
            let ours = self.record(id).ok().and_then(|r| r.cli_session_id);
            if ours.as_deref().is_some_and(|t| t != thread) {
                continue;
            }
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
            self.change(id, |r, l| {
                r.cli_session_id.get_or_insert_with(|| thread.clone());
                match event {
                    CodexEvent::Thread { rollout } => {
                        r.transcript = rollout.or(r.transcript.take());
                        if r.state == SessionState::Starting {
                            r.state = SessionState::Idle;
                        }
                    }
                    CodexEvent::Working { waiting } => {
                        r.state = if waiting {
                            SessionState::Blocked
                        } else {
                            SessionState::Working
                        };
                        r.restore_offer = false;
                        if let Some(l) = l {
                            l.pending = waiting.then(|| PendingApproval {
                                id: Uuid::new_v4(),
                                tool: l.current_tool.clone().unwrap_or_else(|| "Codex".into()),
                                detail: "Answer in the terminal".into(),
                                since: Utc::now(),
                                answerable: false,
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
                st.live
                    .keys()
                    .filter_map(|id| st.records.get(id)?.cli_session_id.clone())
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

async fn persist_loop(store: Arc<DeskStore>, mut rx: mpsc::UnboundedReceiver<Persist>) {
    while let Some(op) = rx.recv().await {
        let result = match &op {
            Persist::Put(r) => store.put(r).await,
            Persist::Delete(id) => store.delete(*id).await,
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
        }
    }

    async fn desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        Desk::start(
            DeskStore::from_pool(pool),
            options(dir),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    fn token(d: &Desk, id: Uuid) -> String {
        d.0.state.lock().live[&id].token.clone()
    }

    async fn new_session(d: &Desk, dir: &Path) -> SessionView {
        d.create(NewSession {
            engine: Engine::Claude,
            name: "auth-fix".into(),
            cwd: dir.to_string_lossy().into_owned(),
            goal: "fix the race".into(),
            worktree: None,
            prompt: None,
            approvals: None,
        })
        .await
        .unwrap()
    }

    async fn post(d: &Desk, id: Uuid, body: Value) -> Value {
        let t = token(d, id);
        d.hook(id, &t, body.to_string().as_bytes()).await.unwrap()
    }

    #[test]
    fn cli_env_drops_inherited_claude_markers() {
        let env = cli_env(&BTreeMap::from([
            ("CLAUDE_CODE_CHILD_SESSION".into(), "1".into()),
            ("CLAUDECODE".into(), "1".into()),
            ("HOME".into(), "/Users/k".into()),
        ]));
        assert!(!env.keys().any(|k| k.starts_with("CLAUDE")));
        assert_eq!(env["HOME"], "/Users/k");
        assert_eq!(env["TERM"], "xterm-256color");
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
            assert!(d.decide(s.id, Uuid::new_v4(), Decision::Allow).is_err());

            d.decide(s.id, pending.id, Decision::Allow).unwrap();
            assert_eq!(waiting.await.unwrap(), claude::allow());
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Working, None));
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
            assert!(d.decide(s.id, p.id, Decision::Allow).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn stopping_a_session_records_it_as_stopped_and_resume_relaunches_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            d.stop(s.id).unwrap();
            for _ in 0..100 {
                if !d.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Stopped);
            let back = d.resume(s.id, Some("continue".into())).await.unwrap();
            assert!(back.live);
            assert_eq!(back.last_prompt.as_deref(), Some("continue"));
            assert!(d.resume(s.id, None).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn an_agent_can_prompt_a_session_and_wait_for_it_to_finish() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            assert!(d.prompt(s.id, "  ").is_err());
            d.prompt(s.id, "run the tests\nthen push").unwrap();

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
            };
            d.post(room.id, post(YOU, 0)).await.unwrap();
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
            assert!(d.resume(s.id, None).await.is_err(), "still running elsewhere");

            d.reconcile_external(ext(false)).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Idle);

            d.reconcile_external(vec![]).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Closed);
            assert!(d.list().iter().all(|v| v.id != s.id));
        })
        .await;
    }
}

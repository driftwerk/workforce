//! The desk: every session the hub hosts or has seen, their live terminals,
//! and the registry that lets them come back after a restart.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use eyre::Result;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::adopt::{self, External, HubInternal};
use crate::asking::{self, Asked, Call, Denied, Reading};
use crate::background::{Background, TaskKind};
use crate::claude::{self, Hook};
use crate::codex::{self, AppServer, CodexEvent, Seen};
use crate::crew::{self, By, Event, Kind, Party};
use crate::engine::{self, Installed, SignIns, View};
use crate::first_run;
use crate::floor::{Outcome, Scope, Subject, SubjectKey, Turn, TurnLog, TurnOp};
use crate::holder::{Held, Holder};
use crate::model::{
    Approvals, Decision, DeskEvent, Engine, NewSession, Origin, PendingApproval, SessionPatch,
    SessionRecord, SessionState, SessionView,
};
use crate::procs;
use crate::pty::{Exit, Pty};
use crate::rooms::{
    channel_for, place_in_timeline, slug_for, steps_refused, Member, NewMessage, NewRoom, Page,
    Posted, Room, RoomMessage, RoomPatch, SearchFilters, SearchHit, Span, DESK, MAX_HOPS,
    THREAD_GONE, TURN_BUDGET, YOU,
};
use crate::store::DeskStore;
use crate::transcript;
use crate::usage::{LocalPlan, PlanSink};
use crate::worktree;

mod accounts;
mod asks;
mod engine_sign_ins;
mod identities;
pub use accounts::Added;
mod board;
mod board_sync;
pub use board::{
    AssignTo, BoardPatch, ChargedTo, Move, NewBoard, NewSessionFor, NewTicket, TicketPatch,
    TicketQuery, TicketView,
};
pub use board_sync::{check_hub_id, check_hub_url, LinkBoard};
mod breaker;
mod charge;
pub use charge::ChargeView;
mod codex_terminals;
mod copies;
mod grants;
#[cfg(test)]
mod grants_tests;
mod groups;
mod handoff;
#[cfg(test)]
mod handoff_tests;
pub use handoff::{HandedOff, HandoffPreview, Hop, LineageView};
mod lifecycle;
mod line;
pub use lifecycle::{Clearing, Hidden, Removal, Removed};
#[cfg(test)]
mod accounts_tests;
#[cfg(test)]
mod asks_tests;
#[cfg(test)]
mod breaker_tests;
#[cfg(test)]
mod codex_asks_tests;
mod ledger;
mod plans;
pub use plans::{
    Changed, NewPlan, OnDrop, PlanDetail, PlanEdit, PlanSummary, PlanTicketView, StartRefused,
};
#[cfg(test)]
mod lifecycle_tests;
mod memory;
mod memory_derive;
#[cfg(test)]
mod memory_derive_tests;
mod memory_index;
#[cfg(test)]
mod memory_index_tests;
mod memory_line;
#[cfg(test)]
mod memory_line_tests;
mod memory_recall;
mod modes;
mod moves;
mod session_account;
#[cfg(test)]
mod session_account_tests;
pub use session_account::{AccountRead, Moment as AccountMoment};
#[cfg(test)]
mod moves_tests;
#[cfg(test)]
mod rooms_tests;
pub use moves::{EffectView, HandoverView, MoveAsk, MoveCommit, MoveDone, MovePreview, SentNote};
mod scopes;
pub use scopes::Boss;
#[cfg(test)]
mod scopes_tests;
mod terminals;
mod transition;
mod unread;

use transition::Transition;

pub use grants::{GrantView, GrantWritten, GrantsView, Native, Paused};
pub(crate) use groups::put_layout_in;
pub use groups::{
    local_agent_item, session_item, Group, Import, Imported, Layout, Ungrouped, UNGROUPED,
};

pub use breaker::{
    agent_subject, plan_subject, pool_subject, session_subject, AgentSpend, AgentVerdict,
    BreakerOf, BreakerView, BudgetPatch, BudgetView, BudgetsView, NewBudget, Rules,
};
pub use ledger::{LedgerPage, SpendQuery};
pub use line::{LineError, Sender, SessionMcp};
pub use memory::{
    GroupMemoryChange, MemoryHitView, MemorySearch, MemorySettingsChange, SessionMemory,
};
pub use memory_derive::ItemChange;
pub use terminals::{NewTerminal, TerminalPatch, TerminalRecord, TerminalState, TerminalView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePolicy {
    Ask,
    Auto,
    Off,
}

/// Whether the agent with this id runs in Fast mode (`[llm] fast`).
pub type AgentFast = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Options {
    /// Each engine's program, as a path or a name looked up on `PATH`. An
    /// engine without one is not installed.
    pub programs: BTreeMap<Engine, PathBuf>,
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
    /// What the hub runs `claude` in and as itself: never adopted.
    pub hub_internal: HubInternal,
    /// Where the person's engine accounts' sign-ins are kept; none: Desk
    /// keeps no accounts, and every session runs on its CLI's own sign-in.
    pub account_keys: Option<Arc<dyn crate::engine_accounts::Keys>>,
    /// Which sessions may message each other on Desk's line.
    pub line_reach: crate::line::Reach,
    /// What session memory keeps, from `[desk.memory]`.
    pub memory: crate::memory::MemoryOptions,
    /// Whether this is a machine's hub, and how long grants last
    /// (`[desk.bosses]`).
    pub bosses: crate::grant::BossOptions,
}

#[cfg(test)]
impl Options {
    /// These options with `engine` run as `program`.
    pub(crate) fn with_program(mut self, engine: Engine, program: PathBuf) -> Self {
        self.programs.insert(engine, program);
        self
    }
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
    /// A change only a move may make: the sidebar's own routes would put a
    /// session in another boss's part of Desk. Answered as a 409 with its
    /// own `code`, so a client asks the person rather than reading the
    /// layout again and retrying.
    #[error(
        "That would put a session in a group another boss oversees. Move it with the Move \
         dialog, which shows what moves and lets you write the new boss's note."
    )]
    CrossesBoss,
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
    /// The terminal ground its CLI took its colours from at its start, for
    /// a CLI that reads them only then (Codex): [`SessionView::fixed_ground`].
    fixed_ground: Option<crate::vt::Ground>,
    /// What it started that still runs.
    background: Background,
    /// What already ran under its Codex app-server when its turn began.
    baseline: Option<HashSet<u32>>,
    /// How many prompts the person has given it while it ran here.
    prompts: u64,
    /// What auto mode refused in the turn it is on.
    denials: Vec<Denied>,
    /// Tool calls the person allowed for the rest of the session.
    allowed: HashSet<Call>,
    /// Its CLI was launched with the pre-tool hook that fails closed
    /// ([`claude::hook_settings_for`]): only then may a boss served from a
    /// grant run it.
    fail_closed: bool,
    /// Its turn was ended from Desk (Esc), which Claude Code reports no
    /// hook for.
    interrupted: bool,
    /// How many of its turns have ended while it ran here.
    ends: u64,
    /// The turn end whose reply the hub's model is reading, while it is:
    /// its turn stays open on the floor until the model says.
    reading: Option<u64>,
    /// Ended from Desk: closed, not stopped, once its process has gone.
    closing: bool,
    /// The Codex requests Desk can answer, oldest first: its pending
    /// approval is the first's.
    codex_asks: Vec<CodexAsk>,
    /// One of its commands changed directory: where a later one runs is
    /// what its engine says, or unknown.
    moved: bool,
    /// Answers Desk sent Codex, by request, until Codex is told them (or
    /// was answered in the terminal first): what Activity then says.
    codex_sent: HashMap<String, CodexSent>,
    /// What its breaker has for it in this run: see [`breaker::LiveBreaker`].
    breaker: breaker::LiveBreaker,
    /// Who raised its pending wait, while one is: the main thread's (a
    /// question, a permission) is not a sub-agent's to end.
    held: Option<Raised>,
    /// The question the main thread asks, while it is open: kept apart, so
    /// a sub-agent's permission shown in front of it, once that ends, gives
    /// way to it again.
    open_question: Option<(PendingApproval, Raised)>,
    /// The question Codex asks in its terminal (`item/tool/requestUserInput`,
    /// an MCP server's request), by its request's id: only the terminal
    /// answers it.
    codex_question: Option<(Value, PendingApproval)>,
    /// The calls each thread (the main thread's, `None`, or a sub-agent's)
    /// started that haven't ended, oldest first: Claude Code runs some side
    /// by side, and a permission request names none of them.
    flights: HashMap<Option<String>, Vec<Flight>>,
    /// Asks raised while another showed, oldest first: each shows once
    /// those in front of it end, and none of them is lost meanwhile.
    behind: Vec<Behind>,
    /// On the CLI's own sign-in, the identity key it was signed in as when
    /// this session started on it (or signed in again inside): a `/login`
    /// elsewhere since may not have changed it for this process.
    own_key: Option<String>,
    /// When its process started here, or the hub took it back: a `/login`
    /// its transcript shows after this is a sign-in inside it.
    started_at: DateTime<Utc>,
}

/// A call a thread started that hasn't ended.
#[derive(Debug, Clone)]
pub(crate) struct Flight {
    id: String,
    tool: String,
    input: Value,
}

/// An ask waiting behind the one shown: what it asks, who raised it, and
/// Desk's way to answer it, while Desk holds its hook open.
pub(crate) struct Behind {
    pending: PendingApproval,
    raised: Raised,
    answer: Option<Answer>,
}

/// What raised a Claude Code session's pending wait (the one with `pending`'s
/// id): the sub-agent it came from, if any, and the call that asked it, for
/// a question, which only that call's end answers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Raised {
    pending: Uuid,
    agent: Option<String>,
    /// It is a question (or a plan) a tool asks, not a permission.
    question: bool,
    call: Option<String>,
}

impl Live {
    /// The main thread's question is over: it no longer waits, nor shows.
    fn end_question(&mut self) {
        if self.open_question.take().is_some() && self.holder().is_some_and(|h| h.question) {
            self.pending = None;
            self.held = None;
        }
    }

    /// What raised its pending wait, while it waits.
    fn holder(&self) -> Option<&Raised> {
        let pending = self.pending.as_ref()?;
        self.held.as_ref().filter(|h| h.pending == pending.id)
    }

    /// Whether a hook from `inside` (a sub-agent, or the main thread) about
    /// the call `call`, which ended, leaves its pending wait be: a
    /// sub-agent's tools don't end what another thread waits on; an ask
    /// raised by a call waits for that call's own end, not another's run
    /// beside it; and one whose call isn't known waits while its thread
    /// still has a call running, which may be its own.
    fn holds_against(&self, inside: Option<&str>, call: Option<&str>) -> bool {
        if self.pending.is_none() {
            return false;
        }
        let held = self.holder();
        if let Some(agent) = inside {
            if held.is_none_or(|h| h.agent.as_deref() != Some(agent)) {
                return true;
            }
        }
        held.is_some_and(|h| match (h.call.as_deref(), call) {
            (Some(asked), Some(ended)) => asked != ended,
            // Not its own thread's call: whichever it asks about, it isn't
            // this one.
            (None, Some(_)) if h.agent.as_deref() != inside => true,
            (None, Some(_)) => self.flights.get(&h.agent).is_some_and(|f| !f.is_empty()),
            _ => false,
        })
    }

    /// `thread` started the call `id`, `tool` on `input`.
    fn started(&mut self, thread: Option<String>, id: String, tool: &str, input: Value) {
        let flights = self.flights.entry(thread).or_default();
        if flights.iter().all(|f| f.id != id) {
            flights.push(Flight {
                id,
                tool: tool.to_string(),
                input,
            });
        }
    }

    /// The call `id` ended (or was refused): it runs no more, and an ask
    /// of it waiting behind the one shown goes. Whether one did.
    fn landed(&mut self, id: &str) -> bool {
        self.flights
            .values_mut()
            .for_each(|f| f.retain(|f| f.id != id));
        let before = self.behind.len();
        self.behind.retain(|b| b.raised.call.as_deref() != Some(id));
        before != self.behind.len()
    }

    /// The call a permission `thread` asks for, `tool` on `input`, is
    /// about: the one of its calls running that matches it, or, with no
    /// match, the only one it has running. Two alike, or several and none
    /// alike, and which isn't known.
    fn call_asked(&self, thread: &Option<String>, tool: &str, input: &Value) -> Option<String> {
        let flights = self.flights.get(thread)?;
        let alike: Vec<&Flight> = flights
            .iter()
            .filter(|f| f.tool == tool && f.input == *input)
            .collect();
        match (alike.as_slice(), flights.as_slice()) {
            ([one], _) => Some(one.id.clone()),
            ([], [only]) => Some(only.id.clone()),
            _ => None,
        }
    }

    /// `thread` went on to the call `call`: what it asked of a call that
    /// has since ended is over. Not an ask whose call still runs (one run
    /// beside it may start meanwhile), nor one whose call isn't known, nor
    /// one whose hook Desk still holds open. Whether one ended.
    fn went_past(&mut self, thread: Option<&str>, call: Option<&str>) -> bool {
        let stale = self.answer.is_none()
            && self.holder().is_some_and(|h| {
                h.agent.as_deref() == thread
                    && h.call.as_deref().is_some_and(|asked| {
                        Some(asked) != call
                            && !self
                                .flights
                                .get(&h.agent)
                                .is_some_and(|f| f.iter().any(|f| f.id == asked))
                    })
            });
        if stale {
            self.ask_ended();
        }
        stale
    }

    /// Its pending ask is over: the oldest waiting behind it shows next,
    /// else the main thread's question, while it is open behind it.
    fn ask_ended(&mut self) {
        self.pending = None;
        self.held = None;
        self.answer = None;
        if !self.behind.is_empty() {
            let next = self.behind.remove(0);
            self.pending = Some(next.pending);
            self.held = Some(next.raised);
            self.answer = next.answer;
        } else if let Some((question, raised)) = self.open_question.clone() {
            self.pending = Some(question);
            self.held = Some(raised);
        }
    }

    /// What `agent` (a sub-agent) asked and ran is over with it.
    fn agent_ended(&mut self, agent: &str) {
        self.flights.remove(&Some(agent.to_string()));
        self.behind
            .retain(|b| b.raised.agent.as_deref() != Some(agent));
        if self
            .holder()
            .is_some_and(|h| h.agent.as_deref() == Some(agent))
        {
            self.ask_ended();
        }
    }

    /// The main thread's turn is over: nothing it ran runs, and nothing
    /// asked in it waits; but what a sub-agent still running asked waits
    /// on, Desk's way to answer it too, and shows once nothing else does.
    /// Whether an ask shows.
    fn turn_over(&mut self) -> bool {
        self.flights.remove(&None);
        let (background, flights) = (&self.background, &self.flights);
        self.behind.retain(|b| {
            b.raised.agent.as_deref().is_some_and(|agent| {
                background.runs(agent) || flights.contains_key(&Some(agent.to_string()))
            })
        });
        if self.pending.is_none() && !self.behind.is_empty() {
            self.ask_ended();
        }
        self.pending.is_some()
    }

    /// Every ask it has open now, each once: what Desk keeps of them, and
    /// what backs the floor's waits.
    fn open_asks(&self) -> Vec<asks::KeptAsk> {
        let main = self.open_question.as_ref().map(|(p, _)| p.id);
        let shown = self.pending.iter().map(|p| {
            let raised = self.held.as_ref().filter(|h| h.pending == p.id);
            asks::KeptAsk::of(p, raised, main == Some(p.id))
        });
        let question = self
            .open_question
            .iter()
            .map(|(p, raised)| asks::KeptAsk::of(p, Some(raised), true));
        let codex = self
            .codex_asks
            .iter()
            .map(|a| &a.pending)
            .chain(self.codex_question.iter().map(|(_, p)| p))
            .map(|p| asks::KeptAsk::of(p, None, false));
        let behind = self
            .behind
            .iter()
            .map(|b| asks::KeptAsk::of(&b.pending, Some(&b.raised), false));
        let mut seen = HashSet::new();
        shown
            .chain(behind)
            .chain(question)
            .chain(codex)
            .filter(|a| seen.insert(a.key.clone()))
            .collect()
    }
}

/// A Codex request Desk can answer: its JSON-RPC id, and the pending
/// approval it is.
#[derive(Debug, Clone)]
struct CodexAsk {
    request: Value,
    pending: PendingApproval,
}

/// An answer Desk sent one of Codex's requests.
#[derive(Debug, Clone)]
struct CodexSent {
    pending: PendingApproval,
    decision: Decision,
    by: By,
}

impl Live {
    /// Ends the session's processes, wherever they run (a terminal's shell
    /// is the terminal's to end).
    fn end(&self) {
        if self.terminal.is_none() {
            self.pty.kill();
        }
        if let Some(codex) = &self._codex {
            codex.stop();
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
    /// Where the app-server listens.
    socket: PathBuf,
    watch: CancellationToken,
    /// Answers to what it asks, to its observer.
    replies: mpsc::UnboundedSender<codex::Reply>,
    /// What its breaker says into its running turn, to its observer.
    steers: mpsc::UnboundedSender<String>,
    /// The person's line messages (row and text) said into its running turn.
    line_steers: mpsc::UnboundedSender<(Uuid, String)>,
}

impl CodexLink {
    /// Stops the app-server and what it started, wherever it runs, without
    /// waiting for them (see [`codex::stop_app_server`]).
    fn stop(&self) {
        match &self.server {
            Server::Local(server) => server.stop(),
            Server::Held(pty) => stop_held_server(Arc::clone(pty), self.socket.clone()),
        }
    }
}

/// Stops a holder's app-server and its workers; when its process can't be
/// confirmed to be the server's, the holder is asked to end it instead.
fn stop_held_server(pty: Arc<Pty>, socket: PathBuf) {
    let Some(pid) = pty.pid() else {
        pty.kill();
        return;
    };
    tokio::spawn(async move {
        if codex::stop_app_server(pid, &socket, codex::STOP_GRACE).await
            == codex::Stopped::Unconfirmed
        {
            tracing::warn!(
                pid,
                "codex app-server's process not confirmed; asking the holder to end it"
            );
            pty.kill();
        }
    });
}

impl Drop for CodexLink {
    fn drop(&mut self) {
        self.watch.cancel();
    }
}

#[derive(Default)]
struct State {
    /// The permission mode each Claude Code session last said it runs in
    /// ([`crate::mode`]).
    modes_seen: HashMap<Uuid, crate::mode::Mode>,
    /// The mode that doesn't ask each session was last started in, as a
    /// boss kept it ([`SessionView::mode`]).
    modes_started: HashMap<Uuid, crate::mode::Mode>,
    records: HashMap<Uuid, SessionRecord>,
    rooms: HashMap<Uuid, Room>,
    live: HashMap<Uuid, Live>,
    /// The `codex` process each terminal-hosted Codex session was bound to.
    codex_pids: HashMap<Uuid, u32>,
    /// How far each Codex session's rollout was read, and what it said.
    codex_marks: HashMap<Uuid, crate::codex_session::Mark>,
    /// Transcript mtimes already read for adopted sessions.
    seen_transcripts: HashMap<Uuid, SystemTime>,
    /// When each session came into the state it is in.
    state_since: HashMap<Uuid, DateTime<Utc>>,
    /// When each session last did something worth the person's attention:
    /// what it is unread by.
    attention: HashMap<Uuid, DateTime<Utc>>,
    /// When each session's last `working` event was recorded.
    working_noted: HashMap<Uuid, std::time::Instant>,
    /// Sessions whose next stop or resume someone's action already recorded.
    acted: HashSet<Uuid>,
    /// Requests Desk answered, until the change that clears them is seen.
    decided: HashSet<Uuid>,
    /// The turn each session has open in the floor's log.
    turns: HashMap<Uuid, OpenTurn>,
    /// What each running session asks the person, as last kept.
    asks: HashMap<Uuid, Vec<asks::KeptAsk>>,
    /// What Desk last asked its sessions' environment to add.
    session_env: BTreeMap<String, String>,
    terminals: HashMap<Uuid, TerminalRecord>,
    terminal_ptys: HashMap<Uuid, Arc<Pty>>,
    /// The sidebar's groups.
    layout: Layout,
    /// CLI sessions the person removed: never adopted again until they
    /// start tracking them again.
    hidden: HashSet<(Engine, String)>,
    /// Sessions forgotten while this hub runs: the registry may still hold
    /// one until the persist queue reaches it, and it is never taken back.
    forgotten: HashSet<Uuid>,
    /// The person's engine accounts.
    accounts: Vec<crate::engine_accounts::Account>,
    /// Signing in to new accounts, by sign-in.
    sign_in_runs: HashMap<Uuid, accounts::SignInRun>,
    /// Engines' own sign-ins under way (Codex's device sign-in), by sign-in.
    engine_sign_ins: HashMap<Uuid, engine_sign_ins::EngineSignInRun>,
    /// Sessions moving to another account once their turn ends (`None`:
    /// the CLI's own sign-in).
    switches: HashMap<Uuid, Option<Uuid>>,
    /// What Desk last read of each account's plan (`None`: the CLI's own
    /// sign-in).
    standing: HashMap<Option<Uuid>, crate::engine_accounts::Standing>,
    /// Accounts sessions were moved off at their limit, until they reset
    /// and the person is asked back.
    moved_off: Vec<identities::RawMove>,
    /// How each session's last turn ended when an API error ended it, until
    /// it is asked again ([`crate::limit`]).
    turn_failures: HashMap<Uuid, crate::limit::TurnFailure>,
    /// Which Anthropic account each of the person's accounts is, when known
    /// ([`crate::identity`]).
    links: HashMap<Uuid, crate::identity::Link>,
    /// Which one this computer's own Claude Code sign-in is, as last read.
    own_identity: Option<crate::identity::Identity>,
    /// Each side's recent plan readings, to tell which account the CLI's
    /// own sign-in is by its usage (`None`: the CLI's own).
    prints: HashMap<identities::Raw, crate::identity::Prints>,
    /// What this computer's CLI sign-in reported, by the identity it was
    /// signed in as: never another's ([`identities::Raw`]).
    own_standing: HashMap<String, crate::engine_accounts::Standing>,
    /// Sessions signed in again inside (`/login`): on the CLI's own
    /// sign-in until they start afresh.
    relogged: HashSet<Uuid>,
    /// The CLI sign-ins the person said are none of their accounts.
    own_none: HashSet<String>,
    /// Whether the CLI's own sign-in was read yet: until it is, its
    /// readings wait, never filed as nobody's.
    own_read: bool,
    own_waiting: Vec<(Value, Option<Uuid>, DateTime<Utc>)>,
    /// The last `/login` each session's transcript showed.
    logins_seen: HashMap<Uuid, String>,
    /// Who wrote each credential's standing as it is now: its last reading,
    /// its last stop on the limit. Only what a session wrote itself is taken
    /// back when it turns out to have run on another sign-in.
    written_by: HashMap<identities::Raw, identities::WrittenBy>,
    /// Sessions taken back on an account whose transcript is still being
    /// read for a `/login` inside them: what they report waits until it is
    /// known which sign-in they run on.
    scan_pending: HashSet<Uuid>,
    /// What those sessions reported meanwhile, as it came.
    scan_waiting: Vec<identities::Waiting>,
    /// The stop on a limit each session's last turn marked, on which side
    /// and when: a refusal later read as not the plan's undoes only that,
    /// never another session's stop on the same account.
    limit_marks: HashMap<Uuid, LimitMark>,
    /// Sessions whose rate limit's transcript line is still to be read.
    verdict_pending: HashSet<Uuid>,
    /// Account needs the person put off.
    put_off: HashSet<String>,
    /// Desk moves sessions off an account at its limit by itself.
    auto_switch: bool,
    /// The accounts whose sign-in was in the store when last looked at.
    signed_in_accounts: HashSet<Uuid>,
    /// What went wrong with accounts' sign-ins, by account: shown until it
    /// is put right.
    sign_in_problems: HashMap<Uuid, crate::engine_accounts::SignInProblem>,
    /// The names of accounts removed while sessions still run on them,
    /// until those sessions restart.
    removed_names: HashMap<Uuid, String>,
    /// Sessions whose switch of account is being carried out now.
    switching: HashSet<Uuid>,
    /// What Desk's lines keep in memory.
    lines: line::Lines,
    /// Which boss runs which session, and the person's input to each.
    charges: charge::Charges,
}

/// The most readings of the CLI's own sign-in held until it is first read.
const OWN_WAITING_MAX: usize = 64;

/// How long a held session's transcript is read for a `/login` before what
/// it reported meanwhile is filed as it was.
const LOGIN_SCAN_WAIT: Duration = Duration::from_secs(60);

/// The stop on a limit a session's turn marked: on which side, when, and
/// what the side's standing said of its last stop before, to go back to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct LimitMark {
    side: identities::Raw,
    at: DateTime<Utc>,
    before: (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<u32>),
}

/// How often a busy session's tool is recorded as `working`.
const WORKING_EVERY: Duration = Duration::from_secs(60);

/// How often a busy session's progress is written to the floor's log.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// Where a session's background work runs.
struct Look {
    root: u32,
    engine: Engine,
    baseline: Option<HashSet<u32>>,
}

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
    /// Prices the ledger's calls: the hub's price table.
    pub pricer: Option<Arc<dyn crate::ledger::Pricer>>,
    /// Hides secrets in what session memory keeps: the hub's patterns.
    /// Without one, nothing is harvested.
    pub redactor: Option<Arc<dyn crate::memory::Redactor>>,
    /// Session memory's search index: the hub's vellev node. Without one,
    /// what Desk remembers is searched by keyword only.
    pub index: Option<Arc<dyn crate::memory::index::MemoryIndex>>,
    /// Notes decisions, facts and to-dos from sessions: the hub's cheap
    /// model. Without one, nothing is noted.
    pub deriver: Option<Arc<dyn crate::memory::derive::Deriver>>,
    /// Keeps each overseen session's running account: the hub's cheap
    /// model, which is handed each turn end's gist too. Without one, no
    /// account is kept.
    pub accountant: Option<Arc<dyn crate::account::Accountant>>,
}

/// How long the hub's model may take to read whether a turn's reply asks
/// the person something. It reads after the turn has ended (a subscription
/// CLI's call rarely answers in the seconds a hook can wait), so a yes
/// moves a done session to asking.
const READ_LATER: Duration = Duration::from_secs(45);

/// How often sessions' background work is looked at.
const LOOK_EVERY: Duration = Duration::from_secs(10);

/// When an engine was last read.
#[derive(Debug, Clone, Copy)]
pub(super) struct EngineRead {
    began: std::time::Instant,
    ended: std::time::Instant,
}

/// How many times a minute asking for every engine to be read again
/// ([`Desk::engines_read_now`]) reads them.
pub(crate) const ENGINES_READS_A_MINUTE: usize = 12;

/// Which read since a moment serves a caller: one that began after it
/// (what changed just before is in it), or one that ended after it (a read
/// under way when it asked is shared).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Since {
    Began,
    Ended,
}

/// How often the engines' versions and sign-ins are read again.
const ENGINES_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long a CLI may take to say its version or who it is signed in as.
const SIGN_IN_WAIT: Duration = Duration::from_secs(20);
/// How long a stopped session's process has after each signal before the
/// next, firmer one: SIGHUP, then SIGTERM, then SIGKILL. Well inside what
/// Restart waits for it to end (10 s).
#[cfg(not(test))]
const STOP_GRACE: Duration = Duration::from_secs(3);
#[cfg(test)]
const STOP_GRACE: Duration = Duration::from_millis(150);

enum Persist {
    Terminal(Box<TerminalRecord>),
    DeleteTerminal(Uuid),
    Put(Box<SessionRecord>),
    Attention(Uuid, DateTime<Utc>),
    /// Sessions forgotten with all Activity says of them and how far they
    /// were read, and the CLI sessions of those removed hidden, in one
    /// transaction; `done` hears whether it was written.
    Forget {
        ids: Vec<Uuid>,
        hidden: Vec<lifecycle::Hidden>,
        /// Whether what Desk remembers of them goes too.
        memory: crate::memory_store::Memory,
        done: Option<oneshot::Sender<bool>>,
    },
    Unhide(Engine, String),
    Event(Box<Event>),
    Turn(Box<TurnOp>),
    /// What a session asks the person now (nothing: it asks nothing).
    Asks(Uuid, Vec<asks::KeptAsk>),
}

struct Inner {
    opts: Options,
    /// What the person chose in Settings → Memory, over `opts.memory`.
    memory_choices: Mutex<crate::memory::MemoryChoices>,
    /// One change of those choices at a time: written, then cached.
    memory_choose: tokio::sync::Mutex<()>,
    /// The colours Desk draws its terminals in, which their colour queries
    /// are answered from ([`crate::vt::Palette`]).
    palette: crate::vt::Palette,
    /// Saves of the colours, one at a time, so the last said is the one kept.
    palette_save: tokio::sync::Mutex<()>,
    store: Arc<DeskStore>,
    env: BTreeMap<String, String>,
    /// The shell terminals run.
    shell: String,
    state: Mutex<State>,
    events: broadcast::Sender<DeskEvent>,
    persist: mpsc::UnboundedSender<Persist>,
    hook_base: String,
    hook_port: u16,
    cancel: CancellationToken,
    holder: Mutex<Option<Holder>>,
    layout_write: tokio::sync::Mutex<()>,
    plans: Option<Arc<dyn PlanSink>>,
    /// What each engine's program said of its version and sign-in, as
    /// last read.
    engines: Mutex<BTreeMap<Engine, Installed>>,
    /// Each engine's reads, one at a time: when the last began and ended.
    engine_reads: BTreeMap<Engine, Arc<tokio::sync::Mutex<Option<EngineRead>>>>,
    /// When every engine was last asked to be read again, in the last
    /// minute, by who asked ([`Desk::engines_read_now`]).
    engines_read_asked:
        Mutex<HashMap<Option<String>, std::collections::VecDeque<std::time::Instant>>>,
    /// While the sessions running at the last stop are restored: since
    /// when a read of an engine serves them all.
    restoring_since: Mutex<Option<std::time::Instant>>,
    ledger: ledger::LedgerRun,
    /// Session memory's search index and its worker.
    index: memory_index::IndexRun,
    /// Session memory's notes and their worker.
    derive: memory_derive::DeriveRun,
    accounts: session_account::AccountRun,
    /// Handoff's lineage, and its preview's wait.
    handoff: handoff::HandoffRun,
    /// Reads a turn's last reply where the rules can't tell whether it
    /// asks: the hub's crew, once its routes are mounted.
    reader: Mutex<Option<Weak<dyn crew::Crew>>>,
    /// How long it may take ([`READ_LATER`]; shorter in tests).
    read_wait: Mutex<Duration>,
    /// Held while the accounts' needs are acted on.
    accounts_moving: tokio::sync::Mutex<()>,
    /// The accounts' standing as last written ([`accounts::STANDING`]).
    standing_kept: Mutex<Option<accounts::KeptStanding>>,
    /// The budgets and every subject's breaker.
    breakers: Mutex<breaker::BreakerRun>,
    /// Reads the local agents' spend for budgets: the hub's telemetry.
    agent_spend: Mutex<Option<Arc<dyn breaker::AgentSpend>>>,
    /// The person's timezone, which budgets' periods are in.
    budget_tz: Mutex<Option<chrono_tz::Tz>>,
    /// Held while a session's line messages are delivered: one typed in
    /// at a time.
    line_pump: tokio::sync::Mutex<()>,
    /// What answers sessions' MCP requests on their lines.
    line_mcp: std::sync::OnceLock<Arc<dyn line::SessionMcp>>,
    /// The crew whose overseer answers sessions on their lines.
    line_crew: Mutex<Option<Weak<dyn crew::Crew>>>,
    /// Where a boss's answers for its sessions are judged; none until the
    /// hub says, and until then no boss answers a request.
    boss_bounds: Mutex<Option<crate::charge::Bounds>>,
    /// Whether an agent runs in Fast mode (its `[llm] fast`), by its id:
    /// the Codex sessions it starts do too. None until the hub says.
    agent_fast: Mutex<Option<AgentFast>>,
    /// Whose scope each row is: members' groups and the bosses there are.
    /// Nothing is held under it, so events are stamped wherever noted.
    scopes: Mutex<scopes::Scopes>,
    /// The groups' budgets as last counted for the Floor.
    group_spend: Mutex<breaker::GroupSpend>,
    /// Told when a count of the groups' budgets ends.
    group_counted: Arc<tokio::sync::Notify>,
    /// How many counts of the groups' budgets have begun.
    group_counts: std::sync::atomic::AtomicUsize,
    /// The plans waiting on the person.
    boss_plans: Mutex<plans::Plans>,
    /// Held from reading a plan to writing it back: a plan is written
    /// whole, so two changes at once would undo one another.
    plan_write: tokio::sync::Mutex<()>,
    /// Held while the modes Desk keeps are read and written
    /// ([`crate::mode`]).
    modes_write: tokio::sync::Mutex<()>,
    /// Held while sessions are put back in their own modes.
    modes_restoring: tokio::sync::Mutex<()>,
    /// Something changed that may end a mode since the restore last looked.
    modes_dirty: std::sync::atomic::AtomicBool,
    /// Held while a move is made: two previews of one item never both go
    /// through, the second finding what it does changed.
    move_write: tokio::sync::Mutex<()>,
    /// Held while a plan's ticket is started, and while a plan is dropped:
    /// a drop never misses a start under way.
    plan_starting: tokio::sync::Mutex<()>,
    /// This engine's copy of the home's grants, and its pause.
    grants: Mutex<grants::GrantsRun>,
    /// Held while a grant is written: one at a time, from reading the copy
    /// to keeping it.
    grant_write: tokio::sync::Mutex<()>,
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
/// Where what Desk last asked its sessions' environment to add is kept, so
/// a session the hub starts or resumes after a restart still has it.
const SESSION_ENV: &str = "hub.sessions.env";
/// The colours Desk last said its terminals are drawn in.
const TERMINAL_COLORS: &str = "hub.terminal.colors";
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

/// What Desk last asked its sessions' environment to add, as kept; none
/// when nothing usable is.
async fn kept_session_env(store: &DeskStore) -> BTreeMap<String, String> {
    let kept = match store.state(SESSION_ENV).await {
        Ok(kept) => kept,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read what Desk adds to its sessions' environment");
            None
        }
    };
    kept.and_then(|e| serde_json::from_value::<BTreeMap<String, String>>(e.value).ok())
        .filter(|env| check_env(env).is_ok())
        .unwrap_or_default()
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

/// What a turn that ended on `reply` waits on the person for, as far as can
/// be told at once: what auto mode refused in it, else what the rules read
/// in the reply; and whether the rules couldn't tell, so the hub's model is
/// to read it.
fn asked_at_end(denials: &[Denied], reply: Option<&str>) -> (Option<Asked>, bool) {
    if let Some(refused) = Asked::refused(denials) {
        return (Some(refused), false);
    }
    match reply.map(asking::read) {
        Some(Reading::Asks(asked)) => (Some(asked), false),
        Some(Reading::Unsure) => (None, true),
        Some(Reading::Done) | None => (None, false),
    }
}

/// "1 background task", "2 background tasks".
fn background_tasks(count: u32) -> String {
    match count {
        1 => "1 background task".into(),
        n => format!("{n} background tasks"),
    }
}

/// Codex asks what Desk can answer: kept with the others it waits on (it
/// can ask several at once), the same request again if it asks it again,
/// as a late subscription hears. The first it asks becomes the "Answer in
/// the terminal" request it already said it waits on.
fn codex_asks(r: &mut SessionRecord, l: &mut Live, ask: codex::Ask) {
    let known = l.codex_asks.iter().position(|a| a.request == ask.request);
    let was = match known {
        Some(i) => Some(l.codex_asks[i].pending.clone()),
        None if l.codex_asks.is_empty() => l.pending.clone().filter(|p| !p.answerable),
        None => None,
    };
    let pending = PendingApproval {
        id: was.as_ref().map_or_else(Uuid::new_v4, |p| p.id),
        since: was.as_ref().map_or_else(Utc::now, |p| p.since),
        tool: ask.tool,
        detail: ask.detail,
        answerable: true,
        choices: ask.choices,
        why: ask.why,
        options: None,
        request: ask.whole,
        call: None,
        agent: None,
        cli_session: None,
    };
    let asked = CodexAsk {
        request: ask.request,
        pending,
    };
    match known {
        Some(i) => l.codex_asks[i] = asked,
        None => l.codex_asks.push(asked),
    }
    codex_front(r, l);
}

/// Its pending approval is the oldest of Codex's open requests; none left,
/// it works on.
fn codex_front(r: &mut SessionRecord, l: &mut Live) {
    match l.codex_asks.first() {
        Some(first) => {
            l.pending = Some(first.pending.clone());
            r.state = SessionState::Blocked;
            r.asked = None;
        }
        None => match &l.codex_question {
            // What it asks in its terminal still waits.
            Some((_, question)) => {
                l.pending = Some(question.clone());
                r.state = SessionState::Blocked;
                r.asked = None;
            }
            None => {
                l.pending = None;
                if r.state == SessionState::Blocked {
                    r.state = SessionState::Working;
                }
            }
        },
    }
}

/// Codex asks the person something only its terminal answers: shown with
/// its words and options, after what Desk can answer.
fn codex_question(r: &mut SessionRecord, l: &mut Live, request: Value, q: claude::Question) {
    // The generic wait its status said becomes this question.
    let was = l
        .pending
        .clone()
        .filter(|p| !p.answerable && l.codex_asks.is_empty());
    let pending = PendingApproval {
        id: Uuid::new_v4(),
        since: was.as_ref().map_or_else(Utc::now, |p| p.since),
        tool: q.tool,
        detail: q.detail,
        answerable: false,
        choices: Vec::new(),
        why: q.why,
        options: q.options,
        request: None,
        call: None,
        agent: None,
        cli_session: None,
    };
    l.codex_question = Some((request, pending));
    if l.codex_asks.is_empty() {
        codex_front(r, l);
    }
}

/// Codex's request `request` was answered, in Desk or its terminal: only
/// that one goes.
fn codex_resolved(r: &mut SessionRecord, l: &mut Live, request: &Value) {
    let before = l.codex_asks.len();
    l.codex_asks.retain(|a| a.request != *request);
    let asked = l.codex_question.as_ref().is_some_and(|(q, _)| q == request);
    if asked {
        l.codex_question = None;
    }
    if l.codex_asks.len() != before || asked {
        codex_front(r, l);
    }
}

/// What Activity says of an answer `by` gave `p`.
fn decided_note(
    view: &SessionView,
    p: &PendingApproval,
    decision: Decision,
    by: By,
) -> Option<Event> {
    let what = match p.tool.split('(').next().unwrap_or(&p.tool) {
        "Bash" if !p.detail.is_empty() => format!("run {}", crew::clip(&p.detail, 120)),
        "Edit" | "Write" | "MultiEdit" if !p.detail.is_empty() => {
            format!("edit {}", crew::clip(&p.detail, 120))
        }
        _ => format!("use {}", crew::clip(&p.tool, 120)),
    };
    let me = Some(Party::session(view));
    let label = view.label();
    let (kind, text) = match decision {
        Decision::Allow => (Kind::Approved, format!("allowed {label} to {what}")),
        Decision::Deny => (Kind::Denied, format!("denied {label} to {what}")),
        Decision::AllowForSession => (
            Kind::Approved,
            format!("allowed {label} to {what}, for the session"),
        ),
        Decision::Cancel => (
            Kind::Denied,
            format!("denied {label} to {what}, and stopped its turn"),
        ),
        Decision::Terminal => return None,
    };
    Some(by.event(kind, me, text).approval(p.id))
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
        "Input" | "Question" => format!("is asking: {}", crew::clip(&p.detail, 160)),
        "Plan" => "wants its plan approved".into(),
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
/// save it is only logged: the next start tries again. With it, the boards
/// it ungrouped (their group was gone).
async fn prune_layout(
    store: &DeskStore,
    layout: Layout,
    records: &HashMap<Uuid, SessionRecord>,
) -> (Layout, Vec<Uuid>) {
    // A closed session is still the registry's: it keeps its group, which
    // what is harvested of it and its ledger rows are stamped with.
    let closed: HashSet<Uuid> = match store.every_session().await {
        Ok(all) => all.into_iter().map(|r| r.id).collect(),
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "couldn't read the closed sessions; the sidebar's layout is left as it is"
            );
            return (layout, vec![]);
        }
    };
    let pruned = layout.without_sessions(|id| !records.contains_key(&id) && !closed.contains(&id));
    if pruned == layout {
        return (layout, vec![]);
    }
    match store.put_layout(&pruned).await {
        Ok(ungrouped) => (pruned, ungrouped),
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "couldn't take forgotten sessions out of the sidebar's layout"
            );
            (layout, vec![])
        }
    }
}

/// Forgets every session of the hub's own that an earlier hub took for the
/// person's, closed ones too: its record, how far it was read and its
/// turns and gists on the floor. Only another terminal's sessions are
/// looked at; one started in Desk is the person's wherever it runs.
async fn forget_hub_internal(store: &DeskStore, hub: &HubInternal, turns: Option<&dyn TurnLog>) {
    let places = match store.external_places().await {
        Ok(places) => places,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "couldn't look for the hub's own sessions to forget"
            );
            return;
        }
    };
    let ids: Vec<Uuid> = places
        .into_iter()
        .filter(|(_, cwd)| hub.holds(Path::new(cwd)))
        .map(|(id, _)| id)
        .collect();
    if ids.is_empty() {
        return;
    }
    match store.forget_sessions(&ids, YOU).await {
        Ok(forgotten) => {
            tracing::info!(
                forgotten,
                "forgot the hub's own sessions taken for the person's"
            )
        }
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "couldn't forget the hub's own sessions"
            );
            return;
        }
    }
    if let Some(turns) = turns {
        turns.send(TurnOp::Forget {
            subjects: ids.into_iter().map(SubjectKey::session).collect(),
        });
    }
}

/// The turns the floor's log has open for the sessions still at work
/// across a restart (`records`, once the lost are marked), each session's
/// latest: the desk carries on with them, so the floor sees them run as the
/// sidebar does, and writes how they end. A lost session's turn is left
/// for the floor to settle where it was last heard.
///
/// With them, the waits the floor still has open in those turns, for the
/// desk to show again or close once it knows what its sessions ask.
async fn reopened_turns(
    log: &dyn TurnLog,
    records: &HashMap<Uuid, SessionRecord>,
) -> (HashMap<Uuid, OpenTurn>, Vec<asks::OpenWaits>) {
    let sessions = Scope {
        owner: String::new(),
        personal: true,
    };
    match log.relevant(&sessions, Utc::now()).await {
        Ok(logged) => {
            let turns = still_open(records, &logged)
                .into_iter()
                .map(|(session, id)| {
                    (
                        session,
                        OpenTurn {
                            id,
                            progressed: None,
                        },
                    )
                })
                .collect();
            let waits = asks::open_waits(&logged, &turns);
            (turns, waits)
        }
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "couldn't read the turns the floor has open; running sessions start new ones"
            );
            (HashMap::new(), Vec::new())
        }
    }
}

/// For each of `records` at work, the latest of its turns `logged` has open.
fn still_open(records: &HashMap<Uuid, SessionRecord>, logged: &[Turn]) -> HashMap<Uuid, Uuid> {
    let mut latest: HashMap<Uuid, &Turn> = HashMap::new();
    for t in logged.iter().filter(|t| t.ended_at.is_none()) {
        let Subject::Session { id, .. } = t.subject else {
            continue;
        };
        let at_work = records.get(&id).is_some_and(|r| {
            matches!(
                r.state,
                SessionState::Starting | SessionState::Working | SessionState::Blocked
            )
        });
        if at_work && latest.get(&id).is_none_or(|l| t.started_at > l.started_at) {
            latest.insert(id, t);
        }
    }
    latest.into_iter().map(|(s, t)| (s, t.id)).collect()
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
        let restoring_since = (opts.restore == RestorePolicy::Auto).then(std::time::Instant::now);
        let store = Arc::new(store);
        forget_hub_internal(&store, &opts.hub_internal, floor.turns.as_deref()).await;
        let layout = store.layout().await?;
        let session_env = kept_session_env(&store).await;
        // What a session restored before Desk connects answers with.
        let palette = crate::vt::Palette::default();
        match store.state(TERMINAL_COLORS).await {
            Ok(Some(kept)) => {
                if let Ok(c) = serde_json::from_value::<crate::vt::Colors>(kept.value) {
                    if c.check().is_ok() {
                        palette.set(c);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "couldn't read the terminals' colours"),
        }
        let engine_accounts = store.accounts().await?;
        let account_links = store.account_links().await?;
        let kept = |key: &'static str| {
            let store = Arc::clone(&store);
            async move {
                store
                    .state(key)
                    .await
                    .ok()
                    .flatten()
                    .map(|e| e.value)
                    .unwrap_or(Value::Null)
            }
        };
        let own_none: HashSet<String> =
            serde_json::from_value(kept(identities::OWN_NONE).await).unwrap_or_default();
        let relogged: HashSet<Uuid> =
            serde_json::from_value(kept(identities::RELOGGED).await).unwrap_or_default();
        let put_off: HashSet<String> =
            serde_json::from_value(kept(accounts::PUT_OFF).await).unwrap_or_default();
        let prints: HashMap<identities::Raw, crate::identity::Prints> =
            serde_json::from_value::<Vec<(identities::Raw, crate::identity::Prints)>>(
                kept(identities::PRINTS).await,
            )
            .unwrap_or_default()
            .into_iter()
            .collect();
        // "Full until 9am" and "switch back?" outlive a restart.
        let kept_standing: accounts::KeptStanding = match store.state(accounts::STANDING).await {
            Ok(kept) => kept
                .and_then(|e| serde_json::from_value(e.value).ok())
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read accounts' standing");
                Default::default()
            }
        };
        let (standing, own_standing, moved_off, limit_marks, written_by) =
            kept_standing.clone().into_state();
        let auto_switch = match store.state(accounts::AUTO_SWITCH).await {
            Ok(kept) => kept.is_some_and(|e| e.value == serde_json::json!(true)),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read whether accounts switch by themselves");
                false
            }
        };
        let hidden = store
            .hidden()
            .await?
            .into_iter()
            .map(|h| (h.engine, h.cli_session_id))
            .collect();
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
        let (layout, ungrouped_boards) = prune_layout(&store, layout, &records).await;
        // Taken up before anything the sessions do is seen, so what they do
        // next carries on the turns they were in.
        let (turns, open_waits) = match floor.turns.as_deref() {
            Some(log) => reopened_turns(log, &records).await,
            None => (HashMap::new(), Vec::new()),
        };
        // What the sessions asked when the last hub stopped.
        let kept_asks = match store.open_asks().await {
            Ok(kept) => kept,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read what sessions were asking; their waits on the floor close"
                );
                HashMap::new()
            }
        };
        let transcripts = records
            .values()
            .filter_map(|r| Some((r.id, r.transcript.clone()?)))
            .collect();
        let kept_asks = asks::not_ended(kept_asks, transcripts).await;
        let state_since = store.state_times().await?;
        let mut attention = store.attention_times().await?;
        // One lost while it worked failed: worth your attention, as one
        // that stops unexpectedly while the hub runs is. One lost while
        // done or idle isn't.
        let failed: Vec<(Uuid, DateTime<Utc>)> = recovered
            .iter()
            .filter(|r| r.state == SessionState::Interrupted)
            .map(|r| (r.id, r.updated_at))
            .collect();
        attention.extend(failed.iter().copied());

        let listener = hook_listener(held.hook_port).await?;
        let hook_port = listener.local_addr()?.port();
        let hook_base = format!("http://127.0.0.1:{hook_port}");
        let (persist, persist_rx) = mpsc::unbounded_channel();
        let env = cli_env(&opts.env);
        let (ledger, ledger_jobs) =
            ledger::LedgerRun::new(floor.pricer.clone(), floor.redactor.clone());
        let desk = Self(Arc::new(Inner {
            memory_choices: Default::default(),
            memory_choose: Default::default(),
            palette,
            palette_save: Default::default(),
            shell: terminals::login_shell(&env, terminals::account_shell, |p| p.is_file()),
            env,
            opts,
            store: Arc::clone(&store),
            state: Mutex::new(State {
                records,
                rooms,
                layout,
                session_env,
                state_since,
                attention,
                hidden,
                turns,
                accounts: engine_accounts,
                links: account_links,
                own_none,
                relogged,
                prints,
                standing,
                own_standing,
                moved_off,
                limit_marks,
                written_by,
                auto_switch,
                put_off,
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
            engines: Mutex::new(BTreeMap::new()),
            engine_reads: Engine::ALL
                .into_iter()
                .map(|e| (e, Arc::default()))
                .collect(),
            engines_read_asked: Mutex::default(),
            restoring_since: Mutex::new(restoring_since),
            ledger,
            index: memory_index::IndexRun::new(floor.index.clone()),
            derive: memory_derive::DeriveRun::new(floor.deriver.clone()),
            accounts: session_account::AccountRun::new(floor.accountant.clone()),
            handoff: Default::default(),
            reader: Mutex::new(None),
            read_wait: Mutex::new(READ_LATER),
            accounts_moving: tokio::sync::Mutex::new(()),
            standing_kept: Mutex::new(Some(kept_standing.clone())),
            breakers: Mutex::new(breaker::BreakerRun::default()),
            agent_spend: Mutex::new(None),
            budget_tz: Mutex::new(None),
            line_pump: tokio::sync::Mutex::new(()),
            line_mcp: std::sync::OnceLock::new(),
            line_crew: Mutex::new(None),
            boss_bounds: Mutex::new(None),
            agent_fast: Mutex::new(None),
            scopes: Mutex::default(),
            group_spend: Mutex::default(),
            group_counted: Arc::new(tokio::sync::Notify::new()),
            group_counts: std::sync::atomic::AtomicUsize::new(0),
            boss_plans: Mutex::default(),
            plan_write: tokio::sync::Mutex::new(()),
            modes_write: tokio::sync::Mutex::new(()),
            modes_restoring: tokio::sync::Mutex::new(()),
            modes_dirty: std::sync::atomic::AtomicBool::new(false),
            move_write: tokio::sync::Mutex::new(()),
            plan_starting: tokio::sync::Mutex::new(()),
            grants: Mutex::default(),
            grant_write: tokio::sync::Mutex::new(()),
        }));
        desk.scopes_follow(&desk.layout());
        desk.scopes_load().await;
        desk.grants_load().await;
        desk.lineage_load().await;
        desk.memory_choices_load().await;
        tokio::spawn(persist_loop(store, floor.turns, persist_rx));
        tokio::spawn(desk.clone().read_engines());
        tokio::spawn(desk.clone().look_for_sign_ins());
        tokio::spawn(desk.clone().ledger_loop(ledger_jobs));
        tokio::spawn(desk.clone().index_loop());
        tokio::spawn(desk.clone().derive_loop());
        tokio::spawn(desk.clone().account_loop());
        desk.breaker_load().await;
        tokio::spawn(desk.clone().breaker_loop());
        desk.charge_load().await;
        tokio::spawn(desk.clone().charge_loop());
        desk.plans_load().await;
        desk.boards_ungrouped(&ungrouped_boards).await;
        tokio::spawn(desk.clone().watch_background());
        tokio::spawn(desk.clone().line_loop());
        recovered.into_iter().for_each(|r| desk.save(r));
        for (id, at) in failed {
            let _ = desk.0.persist.send(Persist::Attention(id, at));
        }
        desk.adopt_held(held).await;
        desk.carry_asks(kept_asks, open_waits);

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
        *desk.0.restoring_since.lock() = None;
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
        let palette = Some(self.0.palette.clone());
        if let Some(holder) = self.holder().await {
            return Ok(holder
                .spawn(spec, cols, rows, history, meta, palette)
                .await?);
        }
        Ok(Pty::spawn_with(spec, cols, rows, history, palette)?)
    }

    /// When each held session's process started: as the hub kept it at its
    /// start, else (one started before that was kept) as the system says,
    /// all asked at once, else now, said.
    async fn held_since(held: &HashMap<Uuid, Held>) -> HashMap<Uuid, DateTime<Utc>> {
        let kept = |h: &Held| {
            h.meta
                .get("startedAt")
                .and_then(|v| serde_json::from_value::<DateTime<Utc>>(v.clone()).ok())
        };
        let asked = held
            .iter()
            .filter(|(_, h)| kept(h).is_none())
            .map(|(id, h)| {
                let pid = h.pid;
                async move {
                    let at = match pid {
                        Some(pid) => procs::started_at(pid).await,
                        None => None,
                    };
                    (*id, at)
                }
            });
        let os: HashMap<Uuid, Option<DateTime<Utc>>> =
            futures::future::join_all(asked).await.into_iter().collect();
        held.iter()
            .map(|(id, h)| {
                let at = kept(h).or_else(|| os.get(id).copied().flatten()).unwrap_or_else(|| {
                    tracing::warn!(session = %id, "couldn't tell when a held session's process started: a /login inside it before now isn't looked for");
                    Utc::now()
                });
                (*id, at)
            })
            .collect()
    }

    /// Whether the holder still runs a process for session `id` that this
    /// hub doesn't hold: one it couldn't take back as it started (the holder
    /// out of reach, or its list unread). Resuming it would start a second
    /// engine on the same conversation.
    async fn held_untaken(&self, id: Uuid) -> bool {
        let Some(holder) = self.holder().await else {
            return false;
        };
        let id = id.to_string();
        match holder.list().await {
            Ok(all) => all.iter().any(|h| {
                !h.exited
                    && h.meta["kind"].as_str() == Some("session")
                    && h.meta["session"].as_str() == Some(id.as_str())
            }),
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "the PTY holder didn't list its terminals; resuming as if it holds none");
                false
            }
        }
    }

    /// Takes back the sessions whose processes the holder kept while the
    /// hub was away, as they were: same processes, same hook tokens.
    async fn adopt_held(&self, mut held: HeldSet) {
        let holder = self.0.holder.lock().clone();
        let terminals = std::mem::take(&mut held.terminals).into_iter().collect();
        self.adopt_terminals(holder.as_ref(), terminals).await;
        tokio::spawn(self.clone().follow_cwds());
        tokio::spawn(self.clone().follow_codex_terminals());
        let Some(holder) = holder else {
            return;
        };
        let history = crate::pty::history_rows(self.0.opts.scrollback_bytes);
        let since = Self::held_since(&held.sessions).await;
        for (id, h) in std::mem::take(&mut held.sessions) {
            let known = self.0.state.lock().records.get(&id).cloned();
            let Some(record) = known else {
                held.strays.push(h);
                continue;
            };
            let (pty, exit) = holder.adopt(&h, history, Some(self.0.palette.clone()));
            let started_at = since.get(&id).copied().unwrap_or_else(Utc::now);
            let codex = match held.servers.remove(&id) {
                Some(server) if record.engine == Engine::Codex => {
                    let socket = server.meta["socket"].as_str().map(PathBuf::from);
                    let (server_pty, _) = holder.adopt(&server, history, None);
                    let server = Server::Held(Arc::new(server_pty));
                    socket.map(|socket| {
                        self.observe_codex(id, server, socket, record.cli_session_id.clone(), true)
                    })
                }
                other => {
                    held.strays.extend(other);
                    None
                }
            };
            // On an account, its transcript changed since its process
            // started: read for a `/login` inside it before what it reports
            // is filed.
            let scan = match (record.engine, &record.transcript, record.account) {
                (Engine::Claude, Some(path), Some(_)) => std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| DateTime::<Utc>::from(m) >= started_at)
                    .then(|| PathBuf::from(path)),
                _ => None,
            };
            let mut st = self.0.state.lock();
            let scan = scan.filter(|_| !st.relogged.contains(&id));
            if scan.is_some() {
                st.scan_pending.insert(id);
            }
            st.live.insert(
                id,
                Live {
                    pty: Arc::new(pty),
                    token: h.meta["token"].as_str().unwrap_or_default().to_string(),
                    fail_closed: h.meta["failClosed"].as_bool().unwrap_or(false),
                    current_tool: None,
                    // What it asked is shown again once every session is
                    // back (`carry_asks`): its terminal's to answer now.
                    pending: None,
                    answer: None,
                    ended: false,
                    stopping: false,
                    _codex: codex,
                    terminal: None,
                    fixed_ground: None,
                    background: Default::default(),
                    baseline: Default::default(),
                    prompts: 0,
                    denials: Vec::new(),
                    allowed: HashSet::new(),
                    interrupted: false,
                    ends: 0,
                    reading: None,
                    closing: false,
                    codex_asks: Vec::new(),
                    moved: false,
                    codex_sent: HashMap::new(),
                    held: None,
                    open_question: None,
                    codex_question: None,
                    flights: HashMap::new(),
                    behind: Vec::new(),
                    own_key: None,
                    started_at,
                    breaker: Default::default(),
                },
            );
            drop(st);
            tokio::spawn(self.clone().on_exit(id, exit));
            if !record.state.is_running() {
                self.change(id, |r, _| r.state = SessionState::Idle);
            } else {
                self.publish(id);
            }
            tracing::info!(%id, "desk session taken back from the PTY holder");
            // A `/login` inside it while the hub was away, or before this hub
            // could tell: it runs on that sign-in.
            if let Some(path) = scan {
                let desk = self.clone();
                tokio::spawn(async move {
                    // Whatever ends it (done, too long, a panic): what waited
                    // is filed.
                    let _release = identities::ScanRelease::new(desk.clone(), id);
                    let scan = async {
                        let read = tokio::task::spawn_blocking(move || {
                            transcript::last_login_since(&path, started_at)
                        });
                        let found = match read.await {
                            Ok(Ok(found)) => found,
                            Ok(Err(e)) => {
                                tracing::warn!(session = %id, error = %e, "couldn't read a session's transcript for a /login inside it");
                                None
                            }
                            Err(e) => {
                                tracing::warn!(session = %id, error = %e, "reading a session's transcript for a /login failed");
                                None
                            }
                        };
                        desk.login_scan_done(id, found).await;
                    };
                    if tokio::time::timeout(LOGIN_SCAN_WAIT, scan).await.is_err() {
                        tracing::warn!(session = %id, "reading a session's transcript for a /login took too long: what it reports is filed as before");
                    }
                });
            }
        }
        held.strays.extend(held.servers.into_values());
        for stray in held.strays {
            if stray.exited {
                holder.reap(stray.id);
            } else {
                tracing::info!(id = %stray.id, "ending a held terminal no session owns");
                let socket = stray.meta["socket"].as_str().map(PathBuf::from);
                let pty = Arc::new(holder.adopt(&stray, 0, None).0);
                match socket {
                    // An app-server and its workers are stopped as a session's are.
                    Some(socket) => stop_held_server(pty, socket),
                    None => pty.kill(),
                }
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
        (view.account, view.switching_to) = Self::account_refs(state, r);
        view.background = live.and_then(|l| l.background.view());
        view.fixed_ground = live.and_then(|l| l.fixed_ground);
        view.interrupted = live.is_some_and(|l| l.interrupted);
        view.reading = live.is_some_and(|l| l.reading.is_some());
        view.prompts = live.map_or(0, |l| l.prompts);
        view.failure = state.turn_failures.get(&r.id).cloned();
        view.mode = live.and(state.modes_started.get(&r.id).copied());
        view.waits_on_you = Self::waits_on_you(state, &view);
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
        if st.forgotten.contains(&r.id) {
            // Forgotten while it was changed elsewhere (a resume that failed
            // to launch): it stays forgotten.
            return;
        }
        st.records.insert(r.id, r.clone());
        // Closed: whatever it was signed in to inside is over.
        if r.state == SessionState::Closed && st.relogged.remove(&r.id) {
            let ids: Vec<Uuid> = st.relogged.iter().copied().collect();
            self.keep_state(identities::RELOGGED, serde_json::json!(ids));
        }
        let view = Self::view_of(st, &r);
        if before.as_ref().is_none_or(|b| b.state != view.state) {
            st.state_since.insert(r.id, view.updated_at);
        }
        let transitions = Transition::of(before.as_ref(), &view, &st.decided);
        let noticed = transitions
            .iter()
            .any(|t| transition::wants_attention(t, st.acted.contains(&r.id)));
        if noticed {
            st.attention.insert(r.id, view.updated_at);
        }
        transitions.iter().for_each(|t| {
            if let Transition::Unblocked { pending, .. } = t {
                st.decided.remove(&pending.id);
            }
        });
        let events = Self::session_events(st, &transitions, &view);
        let open_turn = st.turns.get(&r.id).map(|o| o.id);
        let mut ops = Self::turn_ops(st, &transitions, &view);
        self.keep_asks(st, r.id, open_turn, &mut ops);
        let id = r.id;
        // A switch of account that waited for its turn to end.
        let turn_over = transitions.iter().any(|t| {
            matches!(
                t,
                Transition::Finished { .. }
                    | Transition::Asked { .. }
                    | Transition::Halted
                    | Transition::Dismissed
                    | Transition::Stopped { .. }
                    | Transition::Interrupted
            )
        });
        // A session running here whose turn an API error ended: how it
        // ended is turn_ended's and its transcript's to say.
        let reported = st.live.contains_key(&id) && view.failure.is_some();
        let hit_limit = view.engine == Engine::Claude
            && !reported
            && transitions.iter().any(|t| match t {
                Transition::Finished { reply } => {
                    transition::finished(reply, &view).0 == crate::floor::Outcome::Limit
                }
                _ => false,
            });
        if hit_limit {
            // When Claude Code said it resets; else what a reading says. The
            // account it stopped on, whichever credential of it it ran on.
            let raw = Self::raw_running(st, id);
            // What Claude Code said; else what the words name, if not the
            // plan's own window.
            let in_words = view
                .failure
                .is_none()
                .then(|| {
                    transitions.iter().find_map(|t| match t {
                        Transition::Finished { reply } => {
                            Some(crate::limit::LimitHit::in_words(reply))
                        }
                        _ => None,
                    })
                })
                .flatten();
            Self::mark_hit(
                st,
                id,
                &raw,
                view.failure
                    .as_ref()
                    .and_then(|f| f.limit())
                    .or(in_words.as_ref()),
                Utc::now(),
            );
            let desk = self.clone();
            tokio::spawn(async move { desk.accounts_moved().await });
        }
        // A rate limit whose transcript hasn't said yet whether it was the
        // plan's: its switch waits for that (the limit carries on with
        // "Continue").
        let verdict_pending = reported
            && matches!(&view.failure, Some(crate::limit::TurnFailure::Failed(e)) if e == "rate_limit")
            && st.verdict_pending.contains(&id);
        // Carried out by the one driver of this session's switches.
        if turn_over && !verdict_pending && st.switches.contains_key(&id) {
            let desk = self.clone();
            tokio::spawn(async move {
                if let Err(e) = desk.drive_switch(id, None).await {
                    tracing::warn!(session = %id, error = %e, "couldn't move a session to its account");
                }
            });
        }
        let _ = self.0.persist.send(Persist::Put(Box::new(r)));
        if noticed {
            let _ = self.0.persist.send(Persist::Attention(id, view.updated_at));
        }
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

    /// Who each engine on this computer is signed in as, as last read.
    pub fn sign_ins(&self) -> SignIns {
        SignIns {
            by_engine: self
                .0
                .engines
                .lock()
                .iter()
                .map(|(e, i)| (*e, i.sign_in.clone()))
                .collect(),
        }
    }

    /// Every engine read again now (one read under way is shared), as after
    /// Desk installed one or signed one in: then as [`Desk::engines`].
    ///
    /// Asked more than [`ENGINES_READS_A_MINUTE`] times in a minute by one
    /// `caller` (the signed-in person; none for the hub's own token), it
    /// answers them what was last read without reading: each read runs
    /// every engine's program, which asking without end mustn't do, and one
    /// asker going on doesn't hold back another.
    pub async fn engines_read_now(&self, caller: Option<&str>) -> Vec<engine::EngineView> {
        let asked = std::time::Instant::now();
        {
            let mut all = self.0.engines_read_asked.lock();
            let minute = Duration::from_secs(60);
            all.retain(|_, r| r.back().is_some_and(|t| asked.duration_since(*t) < minute));
            let recent = all.entry(caller.map(str::to_string)).or_default();
            while recent
                .front()
                .is_some_and(|t| asked.duration_since(*t) >= minute)
            {
                recent.pop_front();
            }
            if recent.len() >= ENGINES_READS_A_MINUTE {
                tracing::debug!("engines asked to be read too often: answering the last read");
                return self.engines();
            }
            recent.push_back(asked);
        }
        let reads = Engine::ALL
            .into_iter()
            .map(|e| self.read_engine_since(e, asked, Since::Ended));
        futures::future::join_all(reads).await;
        self.spend_signed_in();
        self.engines()
    }

    /// How many reads of every engine `caller` asked for in the last minute.
    #[cfg(test)]
    pub(crate) fn engines_reads_this_minute(&self, caller: Option<&str>) -> usize {
        self.0
            .engines_read_asked
            .lock()
            .get(&caller.map(str::to_string))
            .map_or(0, |r| r.len())
    }

    /// Every engine as Settings → Engines and the new session dialog show
    /// it: its program, whether it is installed, its version and sign-in,
    /// and where Desk runs it.
    pub fn engines(&self) -> Vec<engine::EngineView> {
        let read = self.0.engines.lock().clone();
        Engine::ALL
            .into_iter()
            .map(|e| engine::view_of(e, self.program(e), read.get(&e)))
            .collect()
    }

    /// The person's timezone, as the hub's crew says it (UTC before its
    /// routes are mounted).
    pub fn timezone(&self) -> chrono_tz::Tz {
        self.reader().map_or(chrono_tz::UTC, |c| c.timezone())
    }

    /// `engine`'s program, when the hub names one.
    fn program(&self, engine: Engine) -> Option<&Path> {
        self.0.opts.programs.get(&engine).map(PathBuf::as_path)
    }

    /// `engine`'s program, which a session of it can't start without.
    fn program_of(&self, engine: Engine) -> Result<&Path, DeskError> {
        self.program(engine).ok_or_else(|| {
            DeskError::Invalid(format!("Desk has no program set for {}", engine.title()))
        })
    }

    /// Reads every engine's version and sign-in now and every
    /// [`ENGINES_EVERY`], until the desk stops.
    async fn read_engines(self) {
        let mut tick = tokio::time::interval(ENGINES_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let asked = std::time::Instant::now();
            let reads = Engine::ALL
                .into_iter()
                .map(|e| self.read_engine_since(e, asked, Since::Ended));
            futures::future::join_all(reads).await;
            self.spend_signed_in();
        }
    }

    /// Reads `engine` again now, rather than at the next look: its sign-in
    /// has just changed, so only a read begun from now on says it.
    async fn read_engine_now(&self, engine: Engine) {
        self.read_engine_since(engine, std::time::Instant::now(), Since::Began)
            .await;
        self.spend_signed_in();
    }

    /// Reads `engine` and keeps what it said, unless a read since `asked`
    /// already did ([`Since`]). One read of an engine at a time: a caller
    /// that comes while one is under way waits for it.
    pub(super) async fn read_engine_since(
        &self,
        engine: Engine,
        asked: std::time::Instant,
        since: Since,
    ) {
        let Some(program) = self.program(engine).map(Path::to_path_buf) else {
            // No Claude Code here: its sign-in is read, as none, so what
            // waited on it is filed and nothing waits on it again.
            if engine == Engine::Claude && !self.0.state.lock().own_read {
                self.own_identity_read(None);
            }
            return;
        };
        let Some(gate) = self.0.engine_reads.get(&engine).map(Arc::clone) else {
            return;
        };
        let mut last = gate.lock().await;
        let fresh = last.is_some_and(|r| match since {
            Since::Began => r.began >= asked,
            Since::Ended => r.ended >= asked,
        });
        if fresh {
            return;
        }
        let began = std::time::Instant::now();
        let read = self.read_engine(engine, &program).await;
        if engine == Engine::Claude {
            let own = match &read.sign_in {
                engine::SignIn::Unknown => None,
                said => Some(said.claude_identity()),
            };
            self.own_identity_read(own);
        }
        self.0.engines.lock().insert(engine, read);
        *last = Some(EngineRead {
            began,
            ended: std::time::Instant::now(),
        });
    }

    /// Whether `engine`'s sign-in was ever read here (unreadable is read).
    pub(super) fn engine_read(&self, engine: Engine) -> Option<engine::SignIn> {
        self.0
            .engines
            .lock()
            .get(&engine)
            .map(|i| i.sign_in.clone())
    }

    /// Since when a read of an engine serves a session starting now: while
    /// the last stop's sessions are restored, any since the hub started;
    /// else one that ends after now (one under way is shared).
    pub(super) fn read_asked(&self) -> std::time::Instant {
        self.0
            .restoring_since
            .lock()
            .unwrap_or_else(std::time::Instant::now)
    }

    /// What `engine`'s `program` says of its version and its sign-in.
    async fn read_engine(&self, engine: Engine, program: &Path) -> Installed {
        let args = engine.version_args().unwrap_or_default();
        let version = self.cli_says(program, args).await.and_then(|(out, err)| {
            engine::Version::read(&out).or_else(|| engine::Version::read(&err))
        });
        let sign_in = match (engine, version) {
            (_, None) => engine::SignIn::Unknown,
            (Engine::Claude, Some(_)) => self
                .cli_says(program, &["auth", "status", "--json"])
                .await
                .map_or(engine::SignIn::Unknown, |(out, _)| {
                    engine::claude_sign_in(&out)
                }),
            // Codex says it on stderr.
            (Engine::Codex, Some(_)) => self.cli_says(program, &["login", "status"]).await.map_or(
                engine::SignIn::Unknown,
                |(out, err)| match engine::codex_sign_in(&out) {
                    engine::SignIn::Unknown => engine::codex_sign_in(&err),
                    known => known,
                },
            ),
            (Engine::Devin, Some(_)) => engine::devin_sign_in(&self.0.env, Path::is_file),
            (Engine::Copilot, Some(_)) => engine::copilot_sign_in(&self.0.env),
            (Engine::Gemini | Engine::OpenCode | Engine::Cursor | Engine::Custom, Some(_)) => {
                engine::SignIn::Unknown
            }
        };
        Installed { version, sign_in }
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

    /// Passes the plan's rate limits a Claude Code session on this computer
    /// reported to the hub: of its account when it runs on one, else of the
    /// CLI's own sign-in.
    fn report_usage(&self, limits: Value, session: Option<Uuid>) {
        self.report_usage_at(limits, session, Utc::now());
    }

    /// [`Self::report_usage`], of a reading taken at `at`.
    pub(super) fn report_usage_at(&self, limits: Value, session: Option<Uuid>, at: DateTime<Utc>) {
        let Some(plans) = &self.0.plans else {
            return;
        };
        // Filed by the credential the session runs on now (a `/login`
        // inside moves it to the CLI's own sign-in), under the account that
        // is: one card per account, whichever credential of it reported.
        let (raw, ids) = {
            let mut st = self.0.state.lock();
            // Its transcript still being read for a `/login` inside it: kept
            // until it is known which sign-in it runs on.
            if let Some(id) = session.filter(|id| st.scan_pending.contains(id)) {
                identities::Waiting::Usage(limits, id, at).wait(&mut st);
                return;
            }
            let raw = match session {
                Some(id) => Self::raw_running(&st, id),
                None => Self::raw_of(&st, None),
            };
            // The CLI's own sign-in, not read yet: held until it is.
            if matches!(raw, identities::Raw::Own(_)) && !st.own_read {
                if st.own_waiting.len() >= OWN_WAITING_MAX {
                    st.own_waiting.remove(0);
                }
                st.own_waiting.push((limits, session, at));
                return;
            }
            let ids = Self::plan_ids(&st, &raw);
            (raw, ids)
        };
        let on_own = matches!(raw, identities::Raw::Own(_));
        let own = self.sign_ins().of(Engine::Claude).plan_account();
        let account = match ids {
            Some(ids) => crate::usage::LocalAccount {
                ids,
                plan: own.and_then(|o| o.plan).filter(|_| on_own),
            },
            None => own.unwrap_or_default(),
        };
        let windows = plans.windows(LocalPlan::Claude, &limits);
        plans.local_at(LocalPlan::Claude, limits, account, at);
        self.observe_print(&raw, &windows, at);
        let by = session.map_or(
            identities::Writer::Unattributed,
            identities::Writer::Session,
        );
        self.plan_read(&raw, windows, at, by);
    }

    /// The process a session's own background work runs under: its
    /// app-server for Codex; for Claude Code, the CLI, or the terminal's
    /// shell it runs in.
    fn root_of(l: &Live, engine: Engine) -> Option<u32> {
        match engine {
            Engine::Claude => l.pty.pid(),
            Engine::Codex => l._codex.as_ref()?.server.pid(),
            // Desk doesn't run these yet ([`engine::views`]).
            Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => None,
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

    /// Where a session's background work is looked for: its own process,
    /// its engine, and what ran under it as its turn began.
    fn look_of(st: &State, id: Uuid) -> Option<Look> {
        let r = st.records.get(&id)?;
        let l = st.live.get(&id)?;
        Some(Look {
            root: Self::root_of(l, r.engine)?,
            engine: r.engine,
            baseline: l.baseline.clone(),
        })
    }

    /// How many of a session's background processes `table` lists running:
    /// Claude Code's tool shells; for Codex, what its app-server started in
    /// the turn and left running, each once whatever it runs under it.
    fn running(table: &[procs::Proc], look: &Look) -> usize {
        match (look.engine, &look.baseline) {
            (Engine::Claude, _) => procs::under(table, look.root)
                .into_iter()
                .filter(|p| procs::is_tool_shell(p))
                .count(),
            (Engine::Codex, Some(before)) => procs::children(table, look.root)
                .filter(|p| !before.contains(&p.pid))
                .count(),
            (Engine::Codex, None) => 0,
            (
                Engine::Gemini
                | Engine::Copilot
                | Engine::OpenCode
                | Engine::Cursor
                | Engine::Devin
                | Engine::Custom,
                _,
            ) => 0,
        }
    }

    /// What of a session's background work runs now, read as its turn
    /// ends so the end and what it left running are one change.
    async fn running_now(&self, id: Uuid) -> Option<usize> {
        let look = Self::look_of(&self.0.state.lock(), id)?;
        Some(Self::running(&procs::table().await?, &look))
    }

    /// Looks at what runs under each of `ids` whose turn is done and that
    /// has background work, or may have it unreported.
    async fn look_at_background(&self, ids: &[Uuid]) {
        let due: Vec<(Uuid, Look)> = {
            let st = self.0.state.lock();
            ids.iter()
                .filter(|id| {
                    st.records.get(id).is_some_and(|r| {
                        matches!(r.state, SessionState::Done | SessionState::Asking)
                    }) && st.live.get(id).is_some_and(|l| l.background.worth_a_look())
                })
                .filter_map(|id| Some((*id, Self::look_of(&st, *id)?)))
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let Some(table) = procs::table().await else {
            return;
        };
        for (id, look) in due {
            let running = Self::running(&table, &look);
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
    pub fn note(&self, mut event: Event) {
        self.stamp(&mut event);
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
                | Transition::ToTerminal { .. }
                | Transition::Answerable { .. } => None,
                Transition::Asked { asked, .. } => {
                    let text = match &asked.denied {
                        Some(d) => format!("stopped: {} was not allowed", d.name()),
                        None => format!("asked “{}”", asked.ask),
                    };
                    let question = crew::ItemQuestion {
                        text: asked.ask.clone(),
                        options: asked.options.clone(),
                    };
                    Some(
                        own(Kind::Question, text)
                            .detail(serde_json::json!({ "question": question, "why": asked.why })),
                    )
                }
                // Whoever did it recorded it.
                Transition::Halted | Transition::Dismissed => None,
                Transition::Finished { reply } => Some(match transition::finished(reply, now) {
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

    /// What agents may touch on this computer, on this desk's registry.
    pub fn computer(&self) -> crate::computer::Computer {
        crate::computer::Computer::new(Arc::clone(&self.0.store))
    }

    /// Reads the replies the rules can't tell about through `crew` (the
    /// hub's cheap model), for as long as it is there.
    pub fn read_replies_with(&self, crew: &Arc<dyn crew::Crew>) {
        *self.0.reader.lock() = Some(Arc::downgrade(crew));
    }

    /// When the session last did something worth the person's attention.
    pub fn attention_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.0.state.lock().attention.get(&id).copied()
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

    /// The folders live sessions run in.
    pub fn live_folders(&self) -> Vec<PathBuf> {
        let st = self.0.state.lock();
        st.live
            .keys()
            .filter_map(|id| st.records.get(id))
            .map(|r| PathBuf::from(&r.cwd))
            .collect()
    }

    /// The hub user's home folder.
    pub fn home_folder(&self) -> Option<PathBuf> {
        (self.0.env.get("HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
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
        self.start_session(new, None, None, by, false).await
    }

    /// Starts a session the person asked for in Desk, in a folder they
    /// chose: the one start whose CLI may trust its folder (see
    /// [`SessionRecord::folder_chosen`]).
    pub async fn create_chosen(&self, new: NewSession) -> Result<SessionView, DeskError> {
        self.start_session(new, None, None, Some(By::you()), true)
            .await
    }

    /// Starts a session for `boss`, which runs it from the start: it is in
    /// the boss's charge ([`crate::charge::How::Started`]), and in
    /// `new.group`, before it runs, so nothing it does or spends is ever
    /// outside them.
    pub async fn create_for(
        &self,
        new: NewSession,
        boss: &Party,
        by: By,
    ) -> Result<SessionView, DeskError> {
        self.start_session(new, None, Some(boss), Some(by), false)
            .await
    }

    /// [`Self::create_for`], as the session `id`: a plan's runner names its
    /// session before it starts, so a start cut off between the two starts
    /// that same session, and never another. An `id` Desk already has is
    /// refused: it started.
    /// Say whether an agent runs in Fast mode, by its id: a Codex session it
    /// starts (by a plan, a handoff or the crew) runs in Fast mode then too.
    pub fn agent_fast_with(&self, fast: AgentFast) {
        *self.0.agent_fast.lock() = Some(fast);
    }

    /// Whether a Codex session `party` starts runs in Fast mode: an agent's
    /// own `[llm] fast`; never for the person or Desk, who choose it per
    /// session.
    pub(crate) fn fast_for(&self, engine: Engine, party: &crew::Party) -> bool {
        let agent = matches!(
            party.kind,
            crew::PartyKind::Agent | crew::PartyKind::Manager
        );
        engine == Engine::Codex
            && agent
            && self
                .0
                .agent_fast
                .lock()
                .as_ref()
                .is_some_and(|fast| fast(&party.id))
    }

    pub(super) async fn create_for_as(
        &self,
        id: Uuid,
        new: NewSession,
        boss: &Party,
        by: By,
    ) -> Result<SessionView, DeskError> {
        self.start_session(new, Some(id), Some(boss), Some(by), false)
            .await
    }

    async fn start_session(
        &self,
        new: NewSession,
        id: Option<Uuid>,
        boss: Option<&Party>,
        by: Option<By>,
        folder_chosen: bool,
    ) -> Result<SessionView, DeskError> {
        let name = new.name.trim();
        if name.is_empty() {
            return Err(DeskError::Invalid("a session needs a name".into()));
        }
        let named = id.is_some();
        let id = match id {
            Some(id) if self.0.state.lock().records.contains_key(&id) => {
                return Err(DeskError::Conflict(format!(
                    "the session {id} has started already"
                )))
            }
            Some(id) => id,
            None => Uuid::new_v4(),
        };
        self.breaker_refuses_start(by.as_ref())?;
        let group = new
            .group
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty() && *g != UNGROUPED)
            .map(str::to_string);
        if let Some(g) = &group {
            if !self.layout().groups.iter().any(|x| &x.id == g) {
                return Err(DeskError::Invalid(format!(
                    "there's no group {g} in the sidebar"
                )));
            }
        }
        if boss.is_some() && !self.charges_loaded() {
            return Err(DeskError::Conflict(
                "Desk can't read which sessions bosses run just now; try again shortly.".into(),
            ));
        }
        check_env(&new.env)?;
        let view = new
            .view
            .or_else(|| engine::views(new.engine).first().copied())
            .filter(|&v| engine::runs(new.engine, v))
            .ok_or_else(|| {
                DeskError::Invalid(match new.view {
                    Some(v) => format!(
                        "Desk can't start {} sessions in its {} view",
                        new.engine.title(),
                        v.as_str()
                    ),
                    None => format!("Desk can't start {} sessions yet", new.engine.title()),
                })
            })?;
        self.program_of(new.engine)?;
        if boss.is_some() && engine::caps(new.engine, view).line == engine::Deliver::None {
            return Err(DeskError::Invalid(format!(
                "{} sessions have no line for a boss to talk on",
                new.engine.title()
            )));
        }
        let changed = !new.env.is_empty() && {
            let mut st = self.0.state.lock();
            let changed = st.session_env != new.env;
            st.session_env.clone_from(&new.env);
            changed
        };
        if changed {
            let store = Arc::clone(&self.0.store);
            let env = serde_json::json!(new.env);
            tokio::spawn(async move {
                if let Err(e) = store.put_state(SESSION_ENV, &env).await {
                    tracing::warn!(error = %e, "couldn't keep what Desk adds to its sessions' environment");
                }
            });
        }
        if new.approve_for_me && new.engine != Engine::Codex {
            return Err(DeskError::Invalid(
                "only Codex sessions can approve for you".into(),
            ));
        }
        if new.fast && new.engine != Engine::Codex {
            return Err(DeskError::Invalid(
                "only Codex sessions run in Fast mode".into(),
            ));
        }
        let account = self.account_for(new.engine, new.account)?;
        if new.engine == Engine::Claude {
            self.started_on(account);
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
            Some(base) if new.fresh_worktree => {
                let (path, branch) = worktree::create_fresh(&cwd, base)
                    .await
                    .map_err(|e| DeskError::Invalid(e.to_string()))?;
                (path, Some(branch))
            }
            Some(branch) => (
                worktree::create(&cwd, branch)
                    .await
                    .map_err(|e| DeskError::Invalid(e.to_string()))?,
                Some(branch.to_string()),
            ),
            None => (cwd, None),
        };
        let now = Utc::now();
        let caps = engine::caps(new.engine, view);
        let record = SessionRecord {
            id,
            engine: new.engine,
            view,
            engine_version: self.version_of(new.engine),
            approve_for_me: new.approve_for_me && new.engine == Engine::Codex,
            fast: new.fast && new.engine == Engine::Codex,
            account,
            name: name.to_string(),
            goal: new.goal.trim().to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            branch,
            origin: Origin::Desk,
            state: SessionState::Starting,
            approvals: match caps.approvals {
                engine::Answerer::Desk => new.approvals.unwrap_or(Approvals::Desk),
                engine::Answerer::Terminal | engine::Answerer::None => Approvals::Terminal,
            },
            // Claude Code takes the id Desk gives it; Codex says its thread's.
            cli_session_id: (new.engine == Engine::Claude).then(|| id.to_string()),
            transcript: None,
            last_prompt: new.prompt.clone(),
            last_reply: None,
            external_pid: None,
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            folder_chosen: folder_chosen && boss.is_none(),
            asked: None,
            created_at: now,
            updated_at: now,
        };
        {
            let mut st = self.0.state.lock();
            st.records.insert(id, record.clone());
            if let Some(boss) = boss {
                Self::reserve_charge(&mut st, id, boss, group.as_deref());
            }
        }
        // Where it belongs, before it can do or spend anything.
        let mut placed = self.start_placed(id, group.as_deref(), boss).await;
        // A session named before it starts (a plan's) is written down
        // before its process is: after a stop between the two, its id is
        // known, and the process the holder kept for it is adopted rather
        // than a second one started, so its first prompt goes in once.
        if placed.is_ok() && named {
            placed = self.0.store.put(&record).await.map_err(DeskError::Internal);
        }
        let launched = match placed {
            Ok(()) => self.launch(&record, false, new.prompt.as_deref()).await,
            Err(e) => Err(e),
        };
        if let Err(e) = launched {
            self.0.state.lock().records.remove(&id);
            self.start_unplaced(id, group.is_some(), boss.is_some())
                .await;
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
        if let Some(boss) = boss {
            self.started_note(&view, boss).await;
        }
        Ok(view)
    }

    /// Puts `id`, about to run, in `group` and in `boss`'s charge.
    async fn start_placed(
        &self,
        id: Uuid,
        group: Option<&str>,
        boss: Option<&Party>,
    ) -> Result<(), DeskError> {
        // A session a boss served from a grant starts is born in that
        // grant first, whichever engine this is: it is the boss's only while
        // its grant names it.
        if let (Some(g), Some(b)) = (group, boss) {
            if crate::grant::is_served(&b.id) {
                self.served_born(id, g, &b.id).await?;
            }
        }
        if let Some(boss) = boss {
            self.charge_started(id, boss, group).await?;
        }
        if let Some(g) = group {
            // On a machine whose groups are its grants, it is born in its
            // group's grant.
            if self.born_in(id, g).await? {
                return Ok(());
            }
            let item = groups::session_item(id);
            // Starting in its group is no move: nothing of a move is done.
            self.0.scopes.lock().placing.insert(id);
            let put = self.change_layout(|l| l.put(&item, g, None)).await;
            self.0.scopes.lock().placing.remove(&id);
            put?;
        }
        Ok(())
    }

    /// Undoes [`Self::start_placed`] for a session that didn't run.
    async fn start_unplaced(&self, id: Uuid, grouped: bool, charged: bool) {
        if charged {
            self.charge_forget(id);
            if let Err(e) = self.0.store.delete(id).await {
                tracing::warn!(%id, error = format!("{e:#}"), "a session that didn't start keeps its charge's row until it is forgotten");
            }
        }
        if grouped {
            self.born_undo(id).await;
            self.drop_from_layout(id).await;
        }
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
        self.change(id, |r, l| {
            if let Some(n) = patch.name {
                r.name = n.trim().to_string();
            }
            // Back to the terminal while Desk holds a request: the terminal
            // asks it, as Claude Code's own dialog.
            if patch.approvals == Some(Approvals::Terminal) {
                if let Some(l) = l {
                    if let Some(answer) = l.answer.take() {
                        let _ = answer.send(claude::ask_in_terminal());
                    }
                    if let Some(p) = l.pending.as_mut() {
                        p.answerable = false;
                    }
                }
            }
            if let Some(g) = patch.goal {
                r.goal = g.trim().to_string();
            }
            if let Some(a) = patch
                .approvals
                .filter(|_| engine::caps(r.engine, r.view).approvals == engine::Answerer::Desk)
            {
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
                    "this session runs in a terminal; end it there, or interrupt its turn (desk.interrupt)"
                        .into(),
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
        self.stop_firmly(id, &pty);
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

    /// After Stop's SIGHUP (the PTY's, which a program may handle and
    /// outlive), SIGTERM and then SIGKILL to its process group, each
    /// [`STOP_GRACE`] after the last, while the hub still holds that same PTY
    /// for the session and its process is still there. The process is held
    /// from now ([`crate::leader`]: a pidfd on Linux), so a number used again
    /// is never signalled. Sent by the hub itself, so a holder started by an
    /// older Desk, which only hangs up, ends it too.
    fn stop_firmly(&self, id: Uuid, pty: &Arc<Pty>) {
        let Some(leader) = pty.pid().and_then(crate::leader::Leader::of) else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (desk, held) = (self.clone(), Arc::downgrade(pty));
        rt.spawn(async move {
            for (signal, name) in [(libc::SIGTERM, "SIGTERM"), (libc::SIGKILL, "SIGKILL")] {
                tokio::time::sleep(STOP_GRACE).await;
                let still = {
                    let st = desk.0.state.lock();
                    match (st.live.get(&id), held.upgrade()) {
                        (Some(l), Some(p)) => Arc::ptr_eq(&l.pty, &p),
                        _ => false,
                    }
                };
                if !still || !leader.signal_group(signal) {
                    return;
                }
                tracing::warn!(%id, signal = name, "a stopped session was still running");
            }
        });
    }

    pub async fn resume(
        &self,
        id: Uuid,
        prompt: Option<String>,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        // A session Desk's breaker holds goes on only by the person: anyone
        // else's resume, or a prompt with it, is refused. A restore at start
        // (no one, no prompt) starts no turn, and the hold stays.
        self.boss_holds(id, by.as_ref())?;
        self.mode_lets(id, by.as_ref())?;
        self.person_resumes(id, by.as_ref()).await;
        if by.is_some() || prompt.is_some() {
            self.breaker_refuses_prompt(id, by.as_ref())?;
        }
        self.resume_checked(id, prompt, by).await
    }

    /// [`Self::resume`], the breaker already asked.
    async fn resume_checked(
        &self,
        id: Uuid,
        prompt: Option<String>,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        let r = self.load(id).await?;
        if self.0.state.lock().live.contains_key(&id) {
            return Err(DeskError::Conflict("session is already running".into()));
        }
        if r.origin == Origin::External && r.external_pid.is_some_and(adopt::alive) {
            return Err(DeskError::Conflict(
                "still running in another terminal — close it there first".into(),
            ));
        }
        if self.held_untaken(id).await {
            return Err(DeskError::Conflict(
                "this session is still running in Desk's terminal keeper, which Desk's helper couldn't reach when it started; restart Desk's helper on this computer to take it back"
                    .into(),
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
        let launched = self.launch(&r, true, prompt.as_deref()).await;
        if launched.is_ok() {
            self.breaker_launched(id);
            if let Some(p) = &prompt {
                // Its first prompt is the resumer's, never the terminal's.
                self.breaker_typed(id, p, None);
            }
        }
        if let Err(e) = launched {
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

    /// Takes the colours Desk draws its terminals in: programs asking (OSC
    /// 4, 10, 11, 12) are answered with them while no pane is there to, the
    /// CLIs started next read their light or dark from them, and when the
    /// background or its ground changed every session's CLI that asked to be
    /// told (mode 2031) is told, so it asks again. Only a session's: a
    /// terminal's shell would take the notice as typing. Kept for the hub's
    /// next start, the last said last.
    pub async fn set_terminal_colors(&self, colors: crate::vt::Colors) -> Result<(), DeskError> {
        colors.check().map_err(DeskError::Invalid)?;
        let ground = colors.ground;
        let kept = serde_json::json!(colors);
        let saving = self.0.palette_save.lock().await;
        if self.0.palette.set(colors) {
            // A session in a Desk terminal shares it with the terminal's shell.
            let ptys: Vec<(Arc<Pty>, bool)> = self
                .0
                .state
                .lock()
                .live
                .values()
                .map(|l| (Arc::clone(&l.pty), l.terminal.is_some()))
                .collect();
            let told = tokio::task::spawn_blocking(move || {
                for (pty, shell) in ptys {
                    pty.tell_ground(ground, shell);
                }
            });
            if let Err(e) = told.await {
                tracing::warn!(error = %e, "programs weren't told the terminal's ground");
            }
        }
        if let Err(e) = self.0.store.put_state(TERMINAL_COLORS, &kept).await {
            tracing::warn!(error = %e, "couldn't keep the terminals' colours");
        }
        drop(saving);
        Ok(())
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
        self.boss_holds(id, Some(&by))?;
        self.mode_lets(id, Some(&by))?;
        // A session Desk's breaker holds goes on only by the person.
        self.breaker_refuses_prompt(id, Some(&by))?;
        self.person_resumes(id, Some(&by)).await;
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
        let view = self.resume_checked(id, prompt.clone(), None).await?;
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

    /// Forgets a session: ends it and its turn, deletes its record, what
    /// Activity says of it, how far it was read and its turns and gists on
    /// the floor, and takes it out of the sidebar's layout. Its engine's
    /// transcript is the engine's, and stays.
    pub async fn forget(&self, id: Uuid) -> Result<(), DeskError> {
        let forgot = self
            .forget_as(id, None, crate::memory_store::Memory::Forget, |_, _| Ok(()))
            .await
            .map(drop);
        if forgot.is_ok() {
            self.memory_forgotten();
        }
        forgot
    }

    /// [`Self::forget`], once `may` agrees under the lock it is forgotten
    /// under, and with `hide` hidden in the same transaction that forgets
    /// it: removed. What it returns says whether the registry wrote it.
    async fn forget_as(
        &self,
        id: Uuid,
        hide: Option<lifecycle::Hidden>,
        memory: crate::memory_store::Memory,
        may: impl FnOnce(&State, &SessionRecord) -> Result<(), DeskError>,
    ) -> Result<oneshot::Receiver<bool>, DeskError> {
        self.load(id).await?;
        let gone;
        let live = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let r = st.records.get(&id).ok_or(DeskError::NotFound)?;
            may(st, r)?;
            let view = Self::view_of(st, r);
            gone = view.clone();
            if let Some(h) = &hide {
                // Hidden before the lock goes: adoption never sees it unhidden.
                st.hidden.insert((h.engine, h.cli_session_id.clone()));
            }
            let ops = Self::turn_ops(st, &[Transition::Stopped { closed: false }], &view);
            self.log_turns(ops);
            st.records.remove(&id);
            st.forgotten.insert(id);
            st.seen_transcripts.remove(&id);
            st.state_since.remove(&id);
            st.attention.remove(&id);
            st.working_noted.remove(&id);
            st.acted.remove(&id);
            st.live.remove(&id)
        };
        if let Some(live) = &live {
            if live.terminal.is_some() {
                live.pty.forget_color_scheme_events();
            }
            live.end();
        }
        drop(live);
        // Before its gist goes with it, what it handed on goes too.
        self.carried_forgotten(id);
        self.line_forgotten(&gone).await;
        self.board_forgotten(id).await;
        self.lineage_forgotten(id).await;
        let (done, written) = oneshot::channel();
        let _ = self.0.persist.send(Persist::Forget {
            ids: vec![id],
            hidden: hide.into_iter().collect(),
            memory,
            done: Some(done),
        });
        self.log_turns(vec![TurnOp::Forget {
            subjects: vec![SubjectKey::session(id)],
        }]);
        let _ = self.0.events.send(DeskEvent::Removed { id });
        self.drop_from_layout(id).await;
        Ok(written)
    }

    /// Whether `r` is a session the hub ran itself: one found in another
    /// terminal, running in one of the hub's own folders.
    fn hubs_own(hub: &HubInternal, r: &SessionRecord) -> bool {
        r.origin == Origin::External && hub.holds(Path::new(&r.cwd))
    }

    /// Whether session `id` is one the hub ran itself, never the person's.
    pub fn hub_internal(&self, id: Uuid) -> bool {
        let st = self.0.state.lock();
        st.records
            .get(&id)
            .is_some_and(|r| Self::hubs_own(&self.0.opts.hub_internal, r))
    }

    /// Forgets a session the hub ran itself: it was never the person's.
    async fn forget_hubs_own(&self, id: Uuid) {
        if let Err(e) = self.forget(id).await {
            tracing::warn!(%id, error = %e, "couldn't forget one of the hub's own sessions");
        }
    }

    pub fn decide(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: Decision,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        if let Some(boss) = by
            .as_ref()
            .filter(|b| b.party.kind == crew::PartyKind::Manager)
        {
            self.boss_decides(id, approval, decision, boss)?;
        }
        // Allowing what it asks drives it: in a mode that doesn't ask, only
        // the person's or its unbound boss's.
        if decision != Decision::Deny && decision != Decision::Cancel {
            self.mode_lets_anyone(id, by.as_ref())?;
        }
        let codex = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .is_some_and(|l| l.codex_asks.iter().any(|a| a.pending.id == approval));
        if codex {
            return self.decide_codex(id, approval, decision, by);
        }
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
                return Err(DeskError::Conflict("already answered".into()));
            }
            if matches!(decision, Decision::AllowForSession | Decision::Cancel) {
                return Err(DeskError::Invalid(
                    "only Codex's requests take that answer".into(),
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
                Decision::Allow | Decision::Deny | Decision::AllowForSession | Decision::Cancel => {
                    // Answered: what waits behind it shows next.
                    live.ask_ended();
                    st.decided.insert(approval);
                }
            }
            (answer, seen)
        };
        let _ = answer.send(match decision {
            Decision::Allow => claude::allow(),
            Decision::Deny => claude::deny(),
            Decision::Terminal | Decision::AllowForSession | Decision::Cancel => {
                claude::ask_in_terminal()
            }
        });
        self.change_since(seen, id, |r, l| {
            if l.is_some_and(|l| l.pending.is_none()) {
                r.state = SessionState::Working;
            }
        });
        let view = self.view(id)?;
        if let Some(e) = by
            .zip(asked)
            .and_then(|(by, p)| decided_note(&view, &p, decision, by))
        {
            self.note(e);
        }
        Ok(view)
    }

    /// Answers one of Codex's requests. Only the person does: what Codex
    /// asks is to leave its sandbox, which no agent's tool may grant (the
    /// overseer's read-only rule reads Codex's words, not what runs).
    /// Activity says it once Codex is told, or that the terminal answered
    /// first.
    fn decide_codex(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: Decision,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        // The boss of a session it runs, checked in `decide`; an unbound
        // boss answers Codex too, wherever it asks.
        let charged = |b: &By| {
            b.party.kind == crew::PartyKind::Manager
                && (self.full_of(&b.party.id).unbound
                    || self
                        .charge_of(id)
                        .is_some_and(|c| c.live() && c.boss == b.party.id))
        };
        let by = by
            .filter(|b| b.party.kind == crew::PartyKind::You || charged(b))
            .ok_or_else(|| DeskError::Invalid("only the person answers Codex's requests".into()))?;
        let seen = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let seen = st.records.get(&id).map(|r| Self::view_of(st, r));
            let live = st
                .live
                .get_mut(&id)
                .ok_or_else(|| DeskError::Conflict("session is not running".into()))?;
            let at = live
                .codex_asks
                .iter()
                .position(|a| a.pending.id == approval)
                .ok_or_else(|| DeskError::Conflict("already answered".into()))?;
            if decision == Decision::Terminal {
                // Its terminal asks it too: Desk just stops offering it.
                let asked = live.codex_asks.remove(at);
                if live.pending.as_ref().is_some_and(|p| p.id == approval) {
                    live.pending = Some(PendingApproval {
                        answerable: false,
                        ..asked.pending
                    });
                }
                return self.view_after(seen, id);
            }
            let asked = &live.codex_asks[at];
            if !asked.pending.choices.contains(&decision) {
                return Err(DeskError::Invalid(
                    "Codex doesn't offer that answer to this request".into(),
                ));
            }
            let request = asked.request.clone();
            let sent = live._codex.as_ref().is_some_and(|link| {
                link.replies
                    .send(codex::Reply {
                        request: request.clone(),
                        decision,
                    })
                    .is_ok()
            });
            if !sent {
                return Err(DeskError::Conflict(
                    "the terminal is asking for this one".into(),
                ));
            }
            let asked = live.codex_asks.remove(at);
            live.codex_sent.insert(
                request.to_string(),
                CodexSent {
                    pending: asked.pending,
                    decision,
                    by,
                },
            );
            st.decided.insert(approval);
            seen
        };
        self.view_after(seen, id)
    }

    /// The session's view once a decision changed what it waits on.
    fn view_after(&self, seen: Option<SessionView>, id: Uuid) -> Result<SessionView, DeskError> {
        self.change_since(seen, id, |r, l| {
            // Handed back to the terminal, it still waits; else on to the
            // next it asks, or work.
            if let Some(l) = l.filter(|l| l.pending.as_ref().is_none_or(|p| p.answerable)) {
                codex_front(r, l);
            }
        });
        self.view(id)
    }

    /// Types a prompt into a running session's terminal and submits it, as a
    /// person at the keyboard would (bracketed paste, so newlines stay in
    /// the prompt).
    pub fn prompt(&self, id: Uuid, text: &str, by: Option<By>) -> Result<SessionView, DeskError> {
        self.mode_lets_anyone(id, by.as_ref())?;
        let view = self.type_prompt(id, text, by.as_ref())?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Nudged,
                Some(Party::session(&view)),
                format!("asked {} “{}”", view.label(), crew::one_line(text, 120)),
            ));
        }
        Ok(view)
    }

    /// [`Self::prompt`] without the Activity note, for a line delivery that
    /// has its own. The text goes in without its control characters: one
    /// carrying ESC could end the paste and type keys into the terminal.
    pub(super) fn type_prompt(
        &self,
        id: Uuid,
        text: &str,
        by: Option<&By>,
    ) -> Result<SessionView, DeskError> {
        let text = crate::line::typable(text);
        if text.trim().is_empty() {
            return Err(DeskError::Invalid("the prompt is empty".into()));
        }
        self.breaker_refuses_prompt(id, by)?;
        let pty = self.attach(id)?;
        self.breaker_typed(id, &text, by);
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(text.as_bytes());
        input.extend_from_slice(b"\x1b[201~\r");
        pty.write(&input)?;
        self.view(id)
    }

    /// Ends the turn a running session is working on, as Esc in its terminal
    /// does, for Claude Code and Codex alike. Its process (and a tracked
    /// one's shell) goes on. Not working, it does nothing.
    pub fn interrupt(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        self.boss_holds(id, by.as_ref())?;
        let pty = {
            let st = self.0.state.lock();
            let live = st
                .live
                .get(&id)
                .ok_or_else(|| DeskError::Conflict("session is not running".into()))?;
            let record = st.records.get(&id);
            let keys = record.and_then(|r| engine::interrupt_keys(r.engine));
            let working = record.map(|r| r.state) == Some(SessionState::Working);
            keys.filter(|_| working)
                .map(|keys| (Arc::clone(&live.pty), keys))
        };
        let Some((pty, keys)) = pty else {
            return self.view(id);
        };
        pty.write(keys)?;
        // Claude Code runs no hook for an interrupted turn.
        self.change(id, |r, l| {
            if let Some(l) = l.filter(|_| r.state == SessionState::Working) {
                r.state = SessionState::Done;
                l.current_tool = None;
                l.interrupted = true;
            }
        });
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Interrupted,
                Some(Party::session(&view)),
                format!("interrupted {}", view.label()),
            ));
        }
        Ok(view)
    }

    /// Allows the tool call auto mode refused, that its turn stopped on, for
    /// the rest of the session, and tells it to carry on. Desk's PreToolUse
    /// hook answers the same call `allow` from then on, which runs it
    /// without the classifier. The person's alone to do.
    pub fn allow_asked(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        let mut denied = None;
        let mut refusal = None;
        self.change(id, |r, l| {
            let Some(l) = l.filter(|_| r.state == SessionState::Asking) else {
                refusal = Some("it isn't asking anything");
                return;
            };
            // Taken: allowed once, it isn't offered again.
            match r
                .asked
                .as_mut()
                .and_then(|a| a.denied.take_if(|d| d.allowable))
            {
                Some(d) => {
                    l.allowed.insert(d.call.clone());
                    denied = Some(d);
                }
                None => refusal = Some("there is no refused tool Desk can allow"),
            }
        });
        let Some(denied) = denied else {
            return Err(DeskError::Conflict(
                refusal.unwrap_or("session is not running").into(),
            ));
        };
        let what = match denied.call.input.as_deref() {
            Some(input) => format!("{} `{input}`", denied.name()),
            None => denied.name().to_string(),
        };
        self.prompt(
            id,
            &format!(
                "I allowed {what} for the rest of this session from Desk. Run it again exactly as before, then carry on."
            ),
            None,
        )?;
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Approved,
                Some(Party::session(&view)),
                format!(
                    "allowed {} to use {} for the session",
                    view.label(),
                    crew::clip(&denied.tool, 120)
                ),
            ));
        }
        Ok(view)
    }

    /// Leaves what a session asked unanswered: it is done.
    pub fn dismiss_asked(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        if self.record(id)?.state != SessionState::Asking {
            return Err(DeskError::Conflict("it isn't asking anything".into()));
        }
        self.change(id, |r, _| {
            r.state = SessionState::Done;
            r.asked = None;
        });
        let view = self.view(id)?;
        if let Some(by) = by {
            self.note(by.event(
                Kind::Dismissed,
                Some(Party::session(&view)),
                format!("dismissed what {} asked", view.label()),
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
        // A turn that ended asking is a finished one, to whoever waits for it.
        let settled = |v: &SessionView| {
            states.contains(&v.state)
                || (v.state == SessionState::Asking && states.contains(&SessionState::Done))
                || !v.live
        };
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
        let room = Room {
            id,
            channel: channel_for(id),
            slug: slug_for(&new.name),
            name: new.name.trim().to_string(),
            default_member: new
                .default_member
                .or_else(|| (members.len() == 1).then(|| members[0].key())),
            members,
            group: new.group,
            shared_with: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let room = self.room_bounded(room, &[], &new.share_with)?;
        self.save_room(room).await
    }

    /// Whether this hub's agent `agent` may be given a turn in room `id`:
    /// one its group's boundary lets in now. A boss moved to another group
    /// no longer hears its old group's rooms, whatever a Desk app sends. A
    /// room this desk doesn't keep (another hub's Desk's) is that desk's to
    /// bound; one that can't be read is refused.
    pub async fn agent_reaches_room(&self, id: Uuid, agent: &str) -> bool {
        let room = match self.0.store.room(id).await {
            Ok(Some(room)) => room,
            Ok(None) => return true,
            Err(e) => {
                tracing::warn!(%id, error = %e, "a room couldn't be read, so its turn wasn't run");
                return false;
            }
        };
        let m = Member {
            hub: crate::rooms::LOCAL_HUB.into(),
            agent: agent.to_string(),
        };
        let (layout, bossed, bosses) = (self.layout(), self.bossed(), self.boss_members());
        crate::scope::allowed_member(room.group.as_deref(), &m, &layout, &bossed, &bosses)
    }

    /// The room an older Desk names by its id or its slug, when this desk
    /// keeps it.
    pub async fn room_named(&self, name: &str) -> Option<Uuid> {
        let name = name.trim().trim_start_matches('#');
        if let Ok(id) = Uuid::parse_str(name) {
            return Some(id);
        }
        let rooms = self.0.store.rooms().await.ok()?;
        rooms
            .iter()
            .find(|r| r.slug == name || r.slug == slug_for(name))
            .map(|r| r.id)
    }

    pub async fn update_room(&self, id: Uuid, patch: RoomPatch) -> Result<Room, DeskError> {
        let mut room = self.room_of(id)?;
        let before = room.members.clone();
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
        if let Some(g) = patch.group {
            // A room is its conversation: it never crosses to another
            // boss's scope (boss-gaps W8).
            let bossed = self.bossed();
            if crate::scope::scope_of(room.group.as_deref(), &bossed)
                != crate::scope::scope_of(g.as_deref(), &bossed)
            {
                return Err(DeskError::Conflict(
                    "A room keeps its conversation, so it can't move to a group another boss \
                     oversees. Start a new room there instead."
                        .into(),
                ));
            }
            room.group = g;
        }
        room.updated_at = Utc::now();
        let room = self.room_bounded(room, &before, &patch.share_with)?;
        self.save_room(room).await
    }

    /// `room` as it may be saved, its members having been `before`: its
    /// group is one of the sidebar's, everyone in it may join a room of
    /// that group (a bossed group's room takes only its own agents, and a
    /// bossed group's agents join only its rooms), and, for a room of a
    /// group with a boss, the person agreed to every hub its conversation
    /// goes to that it didn't go to before, by `share_with` now or once
    /// before (refused with a conflict naming the hubs to ask about). Any
    /// other room mixes hubs as before: asking about them everywhere waits
    /// for Desk to ask (BD9).
    fn room_bounded(
        &self,
        mut room: Room,
        before: &[Member],
        share_with: &[String],
    ) -> Result<Room, DeskError> {
        if let Some(g) = room.group.as_deref() {
            if !self.is_group(g) {
                return Err(DeskError::Invalid(format!(
                    "there's no group {g} in the sidebar"
                )));
            }
        }
        let layout = self.layout();
        let bossed = self.bossed();
        let bosses = self.boss_members();
        if let Some(m) = room.members.iter().find(|m| {
            !crate::scope::allowed_member(room.group.as_deref(), m, &layout, &bossed, &bosses)
        }) {
            return Err(DeskError::Invalid(format!(
                "{} is in another client's group, so it can't be in this room",
                m.key()
            )));
        }
        let mut shared = room.shared_with.clone();
        shared.extend(share_with.iter().cloned());
        let bossed_room = room.group.as_ref().is_some_and(|g| bossed.contains(g));
        let asks = match bossed_room {
            true => crate::scope::unshared_hubs(before, &room.members, &shared),
            false => Vec::new(),
        };
        if !asks.is_empty() {
            return Err(DeskError::Conflict(room_shares(&asks)));
        }
        shared.extend(room.members.iter().map(|m| m.hub.clone()));
        shared.sort();
        shared.dedup();
        room.shared_with = shared;
        Ok(room)
    }

    /// The hubs a room of `group` (`None`: the room's own, or none) with
    /// `members` would go to that the person hasn't agreed to: what the
    /// `409` asks about. Only a room of a group with a boss asks.
    pub fn room_asks(
        &self,
        id: Option<Uuid>,
        group: Option<Option<&str>>,
        members: &[Member],
        share_with: &[String],
    ) -> Vec<String> {
        let room = id.and_then(|id| self.room(id));
        let group = match group {
            Some(g) => g.map(str::to_string),
            None => room.as_ref().and_then(|r| r.group.clone()),
        };
        if !group.is_some_and(|g| self.bossed().contains(&g)) {
            return Vec::new();
        }
        let (before, mut shared) = room.map(|r| (r.members, r.shared_with)).unwrap_or_default();
        shared.extend(share_with.iter().cloned());
        crate::scope::unshared_hubs(&before, members, &shared)
    }

    pub async fn delete_room(&self, id: Uuid) -> Result<(), DeskError> {
        if !self.0.store.delete_room(id).await? {
            return Err(DeskError::NotFound);
        }
        self.0.state.lock().rooms.remove(&id);
        self.forget_room_reads(id).await?;
        let _ = self.0.events.send(DeskEvent::RoomRemoved { id });
        Ok(())
    }

    /// A page of a room's main timeline or of one of its threads. A bound
    /// that isn't a message of the room is not found; a message to page
    /// around that is in another place is refused, so Desk asks there.
    pub async fn room_messages(&self, id: Uuid, page: Page) -> Result<Vec<RoomMessage>, DeskError> {
        self.room_of(id)?;
        if let Span::Around(target) = page.span {
            let (_, _, thread) = self
                .0
                .store
                .cursor(id, target)
                .await?
                .ok_or(DeskError::NotFound)?;
            if thread != page.thread {
                return Err(DeskError::Invalid(match thread {
                    Some(_) => "that message is in a thread".into(),
                    None => "that message is in the room's timeline".into(),
                }));
            }
        }
        self.0
            .store
            .messages(id, &page)
            .await?
            .ok_or(DeskError::NotFound)
    }

    /// One message of a room.
    pub async fn room_message(&self, room: Uuid, id: Uuid) -> Result<RoomMessage, DeskError> {
        self.room_of(room)?;
        self.0
            .store
            .message(id)
            .await?
            .filter(|m| m.room_id == room)
            .ok_or(DeskError::NotFound)
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
        if new.also_in_room && (new.author != YOU || new.thread_id.is_none()) {
            return Err(DeskError::Invalid(
                "only your reply in a thread is also posted to the room".into(),
            ));
        }
        if let Some(why) = steps_refused(&new.steps, is_agent && !delivered) {
            return Err(DeskError::Invalid(why));
        }
        if let Some(answered) = new.in_reply_to {
            match self.0.store.message(answered).await? {
                Some(m) if m.room_id == id => {}
                Some(_) => {
                    return Err(DeskError::Invalid(
                        "it answers a message in another room".into(),
                    ))
                }
                None => {
                    return Err(DeskError::Invalid(
                        "it answers a message that isn't there".into(),
                    ))
                }
            }
        }
        if delivered && (!is_agent || Member::parse(&new.author).is_none()) {
            return Err(DeskError::Invalid(format!(
                "a delivery comes from an agent, as hub/agent, not {}",
                new.author
            )));
        }
        // Whoever posts, a bossed group's agent stays within its rooms and
        // a bossed group's room takes only its agents, a delivery too.
        if let Some(m) = is_agent.then(|| Member::parse(&new.author)).flatten() {
            let (layout, bossed, bosses) = (self.layout(), self.bossed(), self.boss_members());
            if !crate::scope::allowed_member(room.group.as_deref(), &m, &layout, &bossed, &bosses) {
                return Err(DeskError::Invalid(format!(
                    "{} is in another client's group, so it can't post in this room",
                    new.author
                )));
            }
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
                in_reply_to: new.in_reply_to,
                steps: new.steps,
                copy_of: None,
                replies: None,
                reading: None,
            })
            .await?;
        if posted.created {
            let _ = self.0.events.send(DeskEvent::RoomMessage {
                message: Box::new(posted.message.clone()),
            });
            if let Some(event) = crate::rooms::post_activity(&posted.message) {
                self.note(event);
            }
            // The reply is kept: a copy that fails is said, not returned, so
            // a retry doesn't post the reply twice; Post to room copies it.
            if new.also_in_room && posted.message.thread_id.is_some() {
                if let Err(e) = self.copy_to_room(id, posted.message.id).await {
                    tracing::warn!(
                        room = %id,
                        message = %posted.message.id,
                        error = %e,
                        "couldn't also post a thread reply to its room"
                    );
                }
            }
        }
        Ok(posted)
    }

    pub async fn search(&self, filters: &SearchFilters) -> Result<Vec<SearchHit>, DeskError> {
        if filters.is_empty() {
            return Ok(vec![]);
        }
        Ok(self.0.store.search(filters).await?)
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
        let mut env = self.with_added(&self.0.state.lock().session_env.clone());
        // Read only now; it reaches the CLI on a descriptor, never in an
        // environment ([`crate::token_pipe`]).
        let account_token = match r.account {
            Some(_) => self.account_token(r.engine, r.account).await?,
            // No sign-in of the CLI's own here: on an account, kept as the
            // session's so its plan readings and switches are that account's.
            None => match self.instead_of_own(r.engine).await? {
                Some((account, token)) => {
                    self.change(r.id, |r, _| r.account = Some(account));
                    Some(token)
                }
                None => None,
            },
        };
        if account_token.is_some() {
            for k in crate::engine_accounts::OUTRANKS_CLAUDE_TOKEN {
                env.remove(k);
            }
        }
        self.settle_first_run(r, cwd, &env).await;
        // The colours Desk draws its terminals in, as it last said them: the
        // CLI reads its light or dark from them ([`crate::vt::Colors`]).
        let colors = self.0.palette.get();
        if let Some(c) = &colors {
            env.insert("COLORFGBG".into(), c.colorfgbg().into());
        }
        let mut fixed_ground = None;
        // The mode Desk keeps for it, as an overseer set it; one that
        // doesn't ask is shown on its card while it runs in it.
        let mode = self.mode_to_start(r.id).await;
        {
            let mut st = self.0.state.lock();
            match mode.filter(|m| !m.asks()) {
                Some(m) => st.modes_started.insert(r.id, m),
                None => st.modes_started.remove(&r.id),
            };
        }
        // A boss served from a grant runs it: Desk's check of each call fails
        // closed. Kept with its process, so a hub that takes it back knows.
        let fail_closed = r.engine == Engine::Claude && self.served_runs(r.id);
        let (spec, codex) = match r.engine {
            Engine::Claude => {
                let follows = {
                    let (env, cwd) = (env.clone(), cwd.to_path_buf());
                    tokio::task::spawn_blocking(move || {
                        first_run::claude_follows_terminal(&env, &cwd)
                    })
                    .await
                    .unwrap_or(false)
                };
                let theme = follows.then_some("auto");
                let session_id = r.cli_session_id.as_deref().unwrap_or_default();
                let hook_url = format!("{}/hook/{}/{token}", self.0.hook_base, r.id);
                let spec = claude::spawn(&claude::ClaudeSpawn {
                    program: self.program_of(Engine::Claude)?,
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
                    theme,
                    mode,
                    fail_closed,
                });
                (spec, None)
            }
            Engine::Codex => {
                let socket = self.0.opts.run_dir.join(format!("{}.sock", r.id.simple()));
                let line = format!("{}/mcp/{}", self.0.hook_base, r.id);
                let line = codex::LineMcp {
                    url: &line,
                    token: &token,
                };
                let server = self.start_app_server(r.id, &socket, line).await?;
                let thread = r.cli_session_id.as_deref().filter(|_| resume);
                let spec = codex::spawn(&codex::CodexSpawn {
                    program: self.program_of(Engine::Codex)?,
                    cwd,
                    socket: &socket,
                    launch: match thread {
                        Some(thread_id) => codex::Launch::Resume { thread_id },
                        None => codex::Launch::New,
                    },
                    prompt,
                    approve_for_me: r.approve_for_me,
                    fast: r.fast,
                    mode,
                    env: &env,
                });
                let link =
                    self.observe_codex(r.id, server, socket, thread.map(str::to_string), false);
                // Codex asks the terminal's colours once, at its start.
                fixed_ground = colors.as_ref().map(|c| c.ground);
                (spec, Some(link))
            }
            Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => {
                return Err(DeskError::Invalid(format!(
                    "Desk can't start {} sessions yet",
                    r.engine.title()
                )))
            }
        };
        // When its process started, kept with it in the holder: a `/login`
        // inside it after this is one, whenever the hub takes it back.
        let started_at = Utc::now();
        let meta = serde_json::json!({
            "kind": "session",
            "session": r.id,
            "token": token,
            "hookPort": self.0.hook_port,
            "startedAt": started_at,
            "failClosed": fail_closed,
        });
        let spec = match account_token {
            Some(account_token) => {
                let fifo = crate::token_pipe::TokenFifo::new(&self.0.opts.run_dir)
                    .map_err(DeskError::Internal)?;
                let spec = fifo.wrap(spec);
                let id = r.id;
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = fifo.hand(&account_token) {
                        tracing::warn!(session = %id, error = %e, "a session didn't take its account's token");
                    }
                });
                spec
            }
            None => spec,
        };
        let started = self.start_pty(&spec, meta).await;
        let (pty, exit) = match started {
            Ok(started) => started,
            Err(e) => {
                if let Some(codex) = &codex {
                    codex.stop();
                }
                return Err(e);
            }
        };
        self.0.state.lock().live.insert(
            r.id,
            Live {
                pty: Arc::new(pty),
                token,
                fail_closed,
                current_tool: None,
                pending: None,
                answer: None,
                ended: false,
                stopping: false,
                _codex: codex,
                terminal: None,
                fixed_ground,
                background: Default::default(),
                baseline: Default::default(),
                prompts: 0,
                denials: Vec::new(),
                allowed: HashSet::new(),
                interrupted: false,
                ends: 0,
                reading: None,
                closing: false,
                codex_asks: Vec::new(),
                moved: false,
                codex_sent: HashMap::new(),
                held: None,
                open_question: None,
                codex_question: None,
                flights: HashMap::new(),
                behind: Vec::new(),
                own_key: None,
                started_at,
                breaker: Default::default(),
            },
        );
        // Started afresh: on its own account again, or on the CLI's sign-in
        // as it is now.
        self.started_afresh(r.id);
        tokio::spawn(self.clone().on_exit(r.id, exit));
        // What its line held while it wasn't running is looked at again now
        // it is: its state said Starting before it was (settling its CLI's
        // first run waits on a file), and a pump then still found it stopped.
        self.line_went_live(r.id);
        let version = self.version_of(r.engine);
        if version.is_some() && version != r.engine_version {
            self.change(r.id, |r, _| r.engine_version = version);
        }
        Ok(())
    }

    /// Settles what `r`'s CLI asks on its first run before it works in
    /// `cwd` (see [`first_run`]): the CLI may never have run where the hub
    /// is (a machine, a fresh Mac). Its onboarding always; its folder's
    /// trust only as [`SessionRecord::folder_chosen`] and the folder allow.
    /// A config that can't be settled is said, and the session starts
    /// anyway: its CLI asks in the terminal.
    async fn settle_first_run(
        &self,
        r: &SessionRecord,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) {
        let engine = r.engine;
        let configs: Vec<first_run::Config> = match engine {
            Engine::Claude => first_run::claude_config(env).into_iter().collect(),
            // Its TUI (in the session's environment) asks, and its
            // app-server (in the hub's CLI environment) keeps the answer.
            Engine::Codex => {
                let mut c: Vec<first_run::Config> = [env, &self.0.env]
                    .into_iter()
                    .filter_map(first_run::codex_config)
                    .collect();
                c.dedup();
                c
            }
            _ => return,
        };
        let version = self.version_of(engine);
        let folder = cwd.to_path_buf();
        let home = first_run::home_of(env, std::env::var_os("HOME").map(PathBuf::from));
        let chosen = r.folder_chosen;
        let settled = tokio::task::spawn_blocking(move || {
            let s = first_run::Settle {
                folder: &folder,
                home: home.as_deref(),
                chosen,
                version: version.as_deref(),
            };
            configs
                .iter()
                .map(|config| {
                    let done = match engine {
                        Engine::Claude => first_run::ready_claude(config, &s),
                        _ => first_run::trust_codex(config, &s),
                    };
                    (config.path.clone(), done)
                })
                .collect::<Vec<_>>()
        })
        .await;
        for (config, done) in settled.unwrap_or_default() {
            match done {
                Ok(true) => tracing::info!(
                    engine = engine.title(),
                    config = %config.display(),
                    folder = %cwd.display(),
                    "settled the CLI's first run for a session"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    engine = engine.title(),
                    config = %config.display(),
                    error = %e,
                    "couldn't settle the CLI's first run; it may ask in the terminal"
                ),
            }
        }
    }

    /// `engine`'s version as last read, as a session records it.
    fn version_of(&self, engine: Engine) -> Option<String> {
        let read = self.0.engines.lock();
        read.get(&engine)?.version.map(|v| v.to_string())
    }

    /// Starts a Codex session's app-server on `socket`: in the holder when
    /// there is one, so it outlives the hub with the TUI attached to it.
    async fn start_app_server(
        &self,
        id: Uuid,
        socket: &Path,
        line: codex::LineMcp<'_>,
    ) -> Result<Server, DeskError> {
        let program = self.program_of(Engine::Codex)?;
        let Some(holder) = self.holder().await else {
            let server =
                AppServer::start(program, socket.to_path_buf(), &self.0.env, Some(line)).await?;
            return Ok(Server::Local(server));
        };
        codex::prepare_socket(socket).await?;
        let spec = codex::app_server(program, socket, &self.0.env, Some(line));
        let meta = serde_json::json!({
            "kind": "codex-app-server",
            "session": id,
            "socket": socket,
        });
        let (pty, _) = holder.spawn(&spec, 80, 24, 0, meta, None).await?;
        let pty = Arc::new(pty);
        if let Err(e) = codex::wait_for_socket(socket).await {
            pty.kill();
            return Err(e.into());
        }
        Ok(Server::Held(pty))
    }

    /// Joins a Codex session's app-server as a second client to follow it,
    /// subscribed to `thread` when it is known (resumed, or taken back):
    /// `at_once` when its TUI already runs it (taken back).
    fn observe_codex(
        &self,
        id: Uuid,
        server: Server,
        socket: PathBuf,
        thread: Option<String>,
        at_once: bool,
    ) -> CodexLink {
        let watch = self.0.cancel.child_token();
        let (tx, rx) = mpsc::channel(64);
        let (replies, answers) = mpsc::unbounded_channel();
        let (steers, steered) = mpsc::unbounded_channel();
        let (line_steers, line_said) = mpsc::unbounded_channel();
        tokio::spawn({
            let socket = server.socket(&socket);
            let watch = watch.clone();
            let plans = self.0.plans.clone();
            let follower = codex::Follower::new(thread, self.0.env.get("HOME").map(PathBuf::from));
            let follower = if at_once {
                follower.at_once()
            } else {
                follower
            };
            async move {
                if let Err(e) = codex::observe(
                    socket, follower, tx, answers, steered, line_said, plans, watch,
                )
                .await
                {
                    tracing::warn!(error = %e, "codex observer stopped");
                }
            }
        });
        tokio::spawn(self.clone().codex_loop(id, rx));
        CodexLink {
            server,
            socket,
            watch,
            replies,
            steers,
            line_steers,
        }
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
        let closing = live.closing;
        live.end();
        drop(live);
        self.spend_read_ending(id);
        self.change(id, |r, _| {
            // Ended from Desk, whether before its exit was seen or since.
            r.state = if closing || r.state == SessionState::Closed {
                SessionState::Closed
            } else if clean {
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
        if let Some(mode) = crate::mode::of_hook(body) {
            self.0.state.lock().modes_seen.insert(id, mode);
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
        // The calls each thread has running, whatever comes of its hook
        // below: what a permission it then asks for is matched to, and
        // what keeps an ask of a call running from being taken as over.
        let unqueued = {
            let mut st = self.0.state.lock();
            match (st.live.get_mut(&id), &hook) {
                (
                    Some(l),
                    Hook::ToolStart {
                        id: Some(call),
                        call: c,
                        input,
                        ..
                    },
                ) => {
                    l.started(inside.clone(), call.clone(), &c.tool, input.clone());
                    false
                }
                (Some(l), Hook::Question { id: Some(call), .. }) => {
                    l.started(inside.clone(), call.clone(), "", Value::Null);
                    false
                }
                (
                    Some(l),
                    Hook::ToolEnd { id: Some(call), .. } | Hook::Denied { id: Some(call), .. },
                ) => l.landed(call),
                _ => false,
            }
        };
        // An ask waiting behind the one shown went with its call.
        if unqueued {
            self.change(id, |_, _| {});
        }
        // The session goes on to another call, or asks again: the ask shown
        // is over if its call has a result already. A "No" with words for
        // Claude in the terminal's dialog fires no hook, and the turn goes on.
        match &hook {
            Hook::ToolStart { id: call, .. } => self.end_if_answered(id, call.as_deref()).await,
            Hook::Permission { .. } | Hook::Question { .. } => self.end_if_answered(id, None).await,
            _ => {}
        }
        if let Hook::ToolStart { call, .. } = &hook {
            let cd = call.tool == "Bash"
                && call.input.as_deref().is_some_and(|c| {
                    c.split(|ch: char| ch.is_whitespace() || ";&|(".contains(ch))
                        .any(|w| matches!(w, "cd" | "pushd" | "popd"))
                });
            if cd {
                if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                    l.moved = true;
                }
            }
        }
        // A call the person allowed for the session runs without asking.
        let allowed = match &hook {
            Hook::ToolStart { call, .. } => self
                .0
                .state
                .lock()
                .live
                .get(&id)
                .is_some_and(|l| l.allowed.contains(call)),
            _ => false,
        };
        // Desk's breaker holding it from this turn refuses the call, why
        // said; otherwise the call is watched (when loops are).
        let held = match &hook {
            Hook::ToolStart { tool, .. } => self.breaker_tool(id, tool),
            _ => None,
        };
        // A session a boss served from a grant runs: every call Desk doesn't
        // judge safe for that boss is refused before it runs, the person's
        // own allow rules notwithstanding, unless the person allowed it in
        // Desk. Sub-agents' calls too.
        let served = match (&hook, held.is_some() || allowed) {
            (Hook::ToolStart { call, input, .. }, false) => {
                self.served_refuses(id, &call.tool, input, None)
            }
            _ => None,
        };
        let answer = move || match (held, allowed, served) {
            (Some(why), ..) => claude::deny_call(&why),
            (None, true, _) => claude::allow_call(),
            (None, false, Some(why)) => claude::deny_call(&why),
            (None, false, None) => empty(),
        };
        // A steer waiting for this tool call's result goes with it.
        let steer = |desk: &Desk, event: &str| {
            let text = desk
                .0
                .state
                .lock()
                .live
                .get_mut(&id)
                .and_then(Desk::take_steer)?;
            Some(claude::context(event, &text))
        };
        if let Some(agent) = &inside {
            let (idle, held) = {
                let mut st = self.0.state.lock();
                let done = st
                    .records
                    .get(&id)
                    .is_some_and(|r| matches!(r.state, SessionState::Done | SessionState::Asking));
                let held = st.live.get_mut(&id).is_some_and(|l| {
                    l.background = std::mem::take(&mut l.background).heard(agent, Utc::now());
                    l.holds_against(Some(agent), None)
                });
                (done, held)
            };
            // What the main thread waits on (a question, a permission)
            // still waits while a sub-agent works on.
            if (idle || held) && matches!(hook, Hook::ToolStart { .. } | Hook::ToolEnd { .. }) {
                if let Hook::ToolEnd {
                    failed: Some(failed),
                    ..
                } = &hook
                {
                    self.breaker_failed(id, failed);
                }
                return answer();
            }
        }
        match hook {
            Hook::SessionStart {
                session_id,
                transcript,
                ..
            } => {
                // Started again (on another account, maybe): its last
                // turn's failure was the run before's.
                self.0.state.lock().turn_failures.remove(&id);
                self.change(id, |r, _| {
                    r.cli_session_id = session_id.or(r.cli_session_id.take());
                    r.transcript = transcript.or(r.transcript.take());
                    if r.state == SessionState::Starting {
                        r.state = SessionState::Idle;
                    }
                });
            }
            Hook::Prompt(p) => {
                // The person's own words: a session its breaker holds goes
                // on, and its line's loop guard starts again.
                let person = self.person_prompted(id, &p);
                if person {
                    self.person_spoke(id);
                    self.person_drove(id);
                }
                self.breaker_turn(id, person);
                self.prompted(id, p, true);
            }
            // A line message (Desk's plugin submitted it, or Desk typed it
            // in): a turn starts as for a prompt, but the person didn't
            // speak, and a hold stays.
            Hook::Delivered { prompt, mark } => {
                let _ = self.person_prompted(id, &prompt);
                self.breaker_turn(id, false);
                self.line_heard(id, &mark).await;
                self.prompted(id, prompt, false);
            }
            Hook::ToolStart { tool, id: call, .. } => self.change(id, |r, l| {
                r.state = SessionState::Working;
                r.asked = None;
                if let Some(l) = l {
                    // Its thread went on to another call: what it asked
                    // before is over (answered, or turned down in the
                    // terminal, which says nothing).
                    let past = l.went_past(inside.as_deref(), call.as_deref());
                    l.current_tool = Some(tool);
                    l.interrupted = false;
                    l.reading = None;
                    // It did something after a transient refusal.
                    if inside.is_none() {
                        asking::went_on(&mut l.denials, false);
                    }
                    // The main thread went on: the question it asked is over
                    // (its own hook turned it down, or it was answered).
                    if inside.is_none() {
                        l.end_question();
                    }
                    // What still asks (a call run beside this one, or
                    // the main thread's question shown again) waits on.
                    if (past || l.holder().is_some()) && l.pending.is_some() {
                        r.state = SessionState::Blocked;
                    }
                }
            }),
            Hook::Denied { denied, id: call } => {
                // What the refused call asked is over.
                let asked = call.as_deref().is_some_and(|call| {
                    self.0.state.lock().live.get(&id).is_some_and(|l| {
                        l.holder().is_some_and(|h| h.call.as_deref() == Some(call))
                    })
                });
                if asked {
                    self.change(id, |r, l| {
                        if let Some(l) = l {
                            l.ask_ended();
                            if l.pending.is_none() && r.state == SessionState::Blocked {
                                r.state = SessionState::Working;
                            }
                        }
                    });
                }
                // A sub-agent's refusal is its own to work around.
                if inside.is_none() {
                    if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                        l.denials.push(denied);
                    }
                }
            }
            Hook::Question { id: call, question } => self.change(id, |r, l| {
                let Some(l) = l else { return };
                // The same call again (its permission check after its
                // start) is the same question, still asked.
                let same = |h: &Raised| h.question && (call.is_none() || h.call == call);
                if l.holder().is_some_and(same)
                    || l.open_question.as_ref().is_some_and(|(_, h)| same(h))
                {
                    return;
                }
                r.state = SessionState::Blocked;
                r.asked = None;
                l.current_tool = None;
                l.reading = None;
                let pending = PendingApproval {
                    id: Uuid::new_v4(),
                    tool: question.tool,
                    detail: question.detail,
                    since: Utc::now(),
                    // Only its terminal's dialog can answer it.
                    answerable: false,
                    choices: Vec::new(),
                    why: question.why,
                    options: question.options,
                    request: None,
                    call: call.clone(),
                    agent: inside.clone(),
                    cli_session: r.cli_session_id.clone(),
                };
                let raised = Raised {
                    pending: pending.id,
                    agent: inside.clone(),
                    question: true,
                    call,
                };
                if inside.is_none() {
                    l.open_question = Some((pending.clone(), raised.clone()));
                }
                l.held = Some(raised);
                l.pending = Some(pending);
                l.answer = None;
            }),
            Hook::ToolEnd {
                id: call,
                interrupt,
                failed,
            } => {
                if let Some(failed) = &failed {
                    self.breaker_failed(id, failed);
                }
                let skip = self.0.state.lock().live.get(&id).is_some_and(|l| {
                    // Another call ended (one run beside it, or another
                    // thread's): what was asked waits on, but not past Esc,
                    // which ends the turn. An interrupted call with nothing
                    // waiting says nothing of the session: it isn't working
                    // on.
                    let esc = interrupt && inside.is_none();
                    (!esc && l.holds_against(inside.as_deref(), call.as_deref()))
                        || (interrupt && l.pending.is_none())
                });
                if skip {
                    return answer();
                }
                self.change(id, |r, l| {
                    r.state = match (interrupt, &inside) {
                        // Esc ended the main thread's turn: no Stop follows.
                        (true, None) => SessionState::Done,
                        _ => SessionState::Working,
                    };
                    if let Some(l) = l {
                        l.current_tool = None;
                        // A tool ran: the turn got past what it was refused.
                        if inside.is_none() {
                            asking::went_on(&mut l.denials, true);
                        }
                        let esc = interrupt && inside.is_none();
                        let ends_question = inside.is_none()
                            && l.open_question.as_ref().is_some_and(|(_, h)| {
                                call.is_none() || h.call.is_none() || h.call == call
                            });
                        if ends_question || esc {
                            l.open_question = None;
                        }
                        if esc {
                            // What shows behind is shown below.
                            let _ = l.turn_over();
                        }
                        // What showed ended: what waits behind it (another
                        // ask, the main thread's question) shows next.
                        l.ask_ended();
                        if l.pending.is_some() {
                            r.state = SessionState::Blocked;
                        }
                    }
                });
                self.spend_read_while_working(id);
                let event = match (failed.is_some(), interrupt) {
                    (false, false) => "PostToolUse",
                    _ => "PostToolUseFailure",
                };
                if let Some(said) = steer(self, event) {
                    return said;
                }
            }
            Hook::NeedsInput(message) => self.change(id, |r, l| {
                if let Some(l) = l.filter(|l| l.pending.is_none()) {
                    r.state = SessionState::Blocked;
                    let pending = PendingApproval {
                        id: Uuid::new_v4(),
                        tool: "Input".into(),
                        detail: message,
                        since: Utc::now(),
                        answerable: false,
                        choices: Vec::new(),
                        why: None,
                        options: None,
                        request: None,
                        call: None,
                        agent: inside.clone(),
                        cli_session: r.cli_session_id.clone(),
                    };
                    l.held = Some(Raised {
                        pending: pending.id,
                        agent: inside.clone(),
                        question: false,
                        call: None,
                    });
                    l.pending = Some(pending);
                }
            }),
            // Back at its prompt, nothing on screen: a question it asked was
            // turned down (Esc fires no hook of its own).
            Hook::AtPrompt => self.change(id, |r, l| {
                if let Some(l) = l.filter(|l| l.open_question.is_some()) {
                    l.end_question();
                    if l.pending.is_none() && r.state == SessionState::Blocked {
                        r.state = SessionState::Done;
                    }
                }
            }),
            Hook::InputAnswered => self.change(id, |r, l| {
                if let Some(l) = l {
                    if l.pending.as_ref().is_some_and(|p| p.tool == "Input") {
                        l.pending = None;
                        l.held = None;
                        r.state = SessionState::Working;
                    }
                }
            }),
            Hook::Woken => {
                self.breaker_turn(id, false);
                self.0.state.lock().turn_failures.remove(&id);
                self.change(id, |r, l| {
                    r.state = SessionState::Working;
                    if let Some(l) = l {
                        l.pending = None;
                        l.open_question = None;
                        // A running sub-agent's ask waits on.
                        if l.turn_over() {
                            r.state = SessionState::Blocked;
                        }
                        l.denials.clear();
                        l.reading = None;
                        r.asked = None;
                    }
                })
            }
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
                    // What it asked and ran is over with it.
                    let asked = l.pending.is_some();
                    l.agent_ended(&agent);
                    if asked {
                        r.state = match l.pending {
                            Some(_) => SessionState::Blocked,
                            None if r.state == SessionState::Blocked => SessionState::Working,
                            None => r.state,
                        };
                    }
                    let before = l.background.count();
                    l.background = std::mem::take(&mut l.background).ended(&agent, Utc::now());
                    // Claude Code goes on by itself once the last of it ends;
                    // its next Stop says whether the turn is done.
                    if matches!(r.state, SessionState::Done | SessionState::Asking)
                        && before > 0
                        && l.background.count() == 0
                    {
                        r.state = SessionState::Working;
                        r.asked = None;
                    }
                }
            }),
            Hook::Stop {
                transcript,
                reply,
                background,
            } => {
                self.turn_ended(id, transcript, reply, background, None)
                    .await
            }
            // A sub-agent's own failure isn't its session's turn ending: the
            // main thread's StopFailure follows if it ends the turn.
            Hook::StopFailure { .. } if inside.is_some() => {}
            Hook::StopFailure {
                transcript,
                reply,
                failure,
                background,
            } => {
                self.turn_ended(id, transcript, reply, background, Some(failure))
                    .await
            }
            // Its record is kept before Claude Code compacts it; the hook is
            // answered at once.
            Hook::PreCompact => self.spend_read_soon(id),
            Hook::SessionEnd => {
                // Nothing it asked stands once it has ended.
                self.change(id, |_, l| {
                    if let Some(l) = l {
                        l.ended = true;
                        l.pending = None;
                        l.held = None;
                        l.answer = None;
                        l.open_question = None;
                        l.behind.clear();
                        l.flights.clear();
                    }
                });
                self.spend_read_ending(id);
            }
            Hook::Permission {
                tool,
                detail,
                request,
            } => {
                return self
                    .await_permission(id, tool, detail, Some(request), inside)
                    .await
            }
            Hook::Usage(limits) => self.report_usage(limits, Some(id)),
            Hook::SignedIn => self.signed_in_inside(id).await,
            Hook::Other => {}
        }
        answer()
    }

    /// Ends session `id`'s shown ask if the transcript has its call's
    /// result (it was answered, or turned down, in the terminal), unless
    /// `next` is that call or Desk holds its hook open.
    async fn end_if_answered(&self, id: Uuid, next: Option<&str>) {
        let shown = {
            let st = self.0.state.lock();
            let call = st
                .live
                .get(&id)
                .filter(|l| l.answer.is_none())
                .and_then(|l| l.holder()?.call.clone())
                .filter(|c| Some(c.as_str()) != next);
            call.zip(st.records.get(&id).and_then(|r| r.transcript.clone()))
        };
        let Some((call, path)) = shown else { return };
        let calls = HashSet::from([call.clone()]);
        let ended = tokio::task::spawn_blocking(move || {
            transcript::results_among(Path::new(&path), &calls).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        if !ended.contains(&call) {
            return;
        }
        self.change(id, |r, l| {
            let Some(l) = l else { return };
            l.landed(&call);
            if l.holder()
                .is_some_and(|h| h.call.as_deref() == Some(call.as_str()))
            {
                l.ask_ended();
                r.state = match l.pending {
                    Some(_) => SessionState::Blocked,
                    None => SessionState::Working,
                };
            }
        });
    }

    /// A turn started on `p`: the person's own words when `person`.
    fn prompted(&self, id: Uuid, p: String, person: bool) {
        // Asked again: how its last turn failed is behind it.
        self.0.state.lock().turn_failures.remove(&id);
        self.change(id, |r, l| {
            r.state = SessionState::Working;
            r.last_prompt = Some(p);
            r.restore_offer = false;
            if let Some(l) = l {
                l.pending = None;
                l.held = None;
                l.open_question = None;
                // A running sub-agent's ask waits on.
                if l.turn_over() {
                    r.state = SessionState::Blocked;
                }
                if person {
                    l.prompts += 1;
                }
                l.denials.clear();
                l.reading = None;
                r.asked = None;
                l.interrupted = false;
            }
        })
    }

    /// The hub's crew, to read replies with, while it is there.
    fn reader(&self) -> Option<Arc<dyn crew::Crew>> {
        self.0.reader.lock().as_ref().and_then(Weak::upgrade)
    }

    /// Has the hub's model read the reply turn end `end` left, which the
    /// rules couldn't tell about, for up to [`READ_LATER`]. A yes moves the
    /// session to asking, if it is still done on that turn (nobody has
    /// spoken, and it hasn't worked or ended again since); anything else
    /// leaves it done.
    /// A Claude Code session's turn ended: by itself (`Stop`), or on an API
    /// error (`StopFailure`, with how it failed). Either way it is done and
    /// nothing it waited on still waits; a turn that failed asks nothing.
    async fn turn_ended(
        &self,
        id: Uuid,
        transcript: Option<String>,
        reply: Option<String>,
        background: Option<Vec<(String, TaskKind)>>,
        failure: Option<crate::limit::TurnFailure>,
    ) {
        let failed = failure.is_some();
        let path = transcript.or_else(|| self.record(id).ok().and_then(|r| r.transcript));
        // The hook fires before the transcript has the reply, so the
        // transcript is only a fallback for older CLIs. A failure's words
        // are its hook's: the transcript's last reply is an older turn's.
        let said = match (reply, path.clone()) {
            (Some(r), _) => Some(r),
            (None, _) if failed => None,
            (None, Some(p)) => tokio::task::spawn_blocking(move || {
                transcript::read_tail(Path::new(&p))
                    .ok()
                    .and_then(|t| t.said)
            })
            .await
            .ok()
            .flatten(),
            (None, None) => None,
        };
        let (heard, prompts) = {
            let st = self.0.state.lock();
            st.live
                .get(&id)
                .map_or((Vec::new(), 0), |l| (l.denials.clone(), l.prompts))
        };
        // The refusals its hooks didn't report (an older plugin, an older
        // CLI) are in the transcript.
        let denials = match (heard.is_empty() && !failed, path.clone()) {
            (true, Some(p)) => tokio::task::spawn_blocking(move || {
                transcript::denials(Path::new(&p)).unwrap_or_default()
            })
            .await
            .unwrap_or_default(),
            _ => heard,
        };
        let (asked, unsure) = match failed {
            true => (None, false),
            false => asked_at_end(&denials, said.as_deref()),
        };
        let said_whole = said.clone().filter(|_| unsure);
        let reader = unsure.then(|| self.reader()).flatten();
        let running = self.running_now(id).await;
        // A refusal whose words aren't the plan's, on an account whose
        // readings show a window full: the plan's limit after all.
        let failure = failure.map(|f| self.cross_checked(id, f));
        let limit = failure.as_ref().and_then(|f| f.limit()).cloned();
        // A rate limit's transcript line decides what it was.
        let rate_limit = match &failure {
            Some(crate::limit::TurnFailure::Limit(_)) => true,
            Some(crate::limit::TurnFailure::Failed(e)) => e == "rate_limit",
            None => false,
        };
        let background_known = background.as_ref().is_some_and(|b| !b.is_empty());
        let pending_verdict = matches!(&failure, Some(crate::limit::TurnFailure::Failed(e)) if e == "rate_limit")
            && path.is_some();
        {
            let mut st = self.0.state.lock();
            st.limit_marks.remove(&id);
            if pending_verdict {
                st.verdict_pending.insert(id);
            } else {
                st.verdict_pending.remove(&id);
            }
            match failure {
                Some(f) => st.turn_failures.insert(id, f),
                None => st.turn_failures.remove(&id),
            };
        }
        let mut read = None;
        // The credential it stopped on, before any switch moves it.
        let on = {
            let st = self.0.state.lock();
            st.records
                .contains_key(&id)
                .then(|| Self::raw_running(&st, id))
        };
        self.change(id, |r, mut l| {
            // The person spoke while it was read: that answers it.
            let asked = asked.filter(|_| l.as_ref().is_some_and(|l| l.prompts == prompts));
            r.state = match asked {
                Some(_) => SessionState::Asking,
                None => SessionState::Done,
            };
            if let Some(l) = l.as_deref_mut() {
                l.ends += 1;
                l.reading = reader.as_ref().map(|_| l.ends);
                read = l.reading;
            }
            r.transcript = path.clone().or(r.transcript.take());
            if let Some(reply) = said {
                r.last_reply = Some(transcript::clip(&reply));
            }
            r.asked = asked;
            if let Some(l) = l {
                l.current_tool = None;
                l.pending = None;
                l.held = None;
                l.open_question = None;
                l.answer = None;
                // A running sub-agent's ask waits on: the turn is over, the
                // session's wait on the person isn't.
                if l.turn_over() {
                    r.state = SessionState::Blocked;
                }
                l.denials.clear();
                let now = Utc::now();
                l.background = match (failed, background) {
                    // A failure says nothing of what it left running: what
                    // was known is kept, and looked at again.
                    (true, None) => std::mem::take(&mut l.background),
                    (_, background) => std::mem::take(&mut l.background).reported(background, now),
                }
                .looked_if(running, now);
            }
        });
        if let (Some(reader), Some(end), Some(reply)) = (reader, read, said_whole) {
            self.read_later(id, end, reader, reply);
        }
        // A `/login` inside it whose `auth_success` went unheard.
        if let Some(p) = path.clone() {
            let desk = self.clone();
            tokio::spawn(async move {
                let login = tokio::task::spawn_blocking(move || {
                    transcript::read_tail(Path::new(&p))
                        .ok()
                        .and_then(|t| t.last_login)
                })
                .await
                .ok()
                .flatten();
                desk.login_in_transcript(id, login).await;
            });
        }
        if let Some(on) = on {
            if let Some(hit) = &limit {
                // On its limit: the account is full now, whatever it left
                // running (a turn waiting on background work isn't over to
                // the Floor, but it is to its account).
                Self::mark_session_hit(&mut self.0.state.lock(), id, &on, Some(hit));
                let desk = self.clone();
                tokio::spawn(async move { desk.accounts_moved().await });
            }
            // A switch that waited for its turn: over on the limit, whatever
            // still runs; over on another failure once nothing does. A rate
            // limit waits for its transcript's word ([`Self::failure_known`]).
            let waiting = background_known || self.view(id).is_ok_and(|v| v.waiting_on() > 0);
            let other_failure = failed && !rate_limit && !waiting;
            if limit.is_some() || other_failure {
                self.drive_waiting_switch(id);
            }
            if let (true, Some(p)) = (rate_limit, path) {
                self.failure_read_later(id, on, PathBuf::from(p));
            }
        }
        // The turn's calls are in its transcript now.
        self.spend_read_ending(id);
        self.breaker_settled(id);
    }

    /// A switch of account session `id` waits on, carried out now.
    fn drive_waiting_switch(&self, id: Uuid) {
        if !self.0.state.lock().switches.contains_key(&id) {
            return;
        }
        let desk = self.clone();
        tokio::spawn(async move {
            if let Err(e) = desk.drive_switch(id, None).await {
                tracing::warn!(session = %id, error = %e, "couldn't move a session to its account");
            }
        });
    }

    /// `account` stopped on its plan's limit now: full until `hit` says it
    /// resets, else until a reading says it has room.
    ///
    /// Session `id` stopped it: while its transcript is still read for a
    /// `/login` inside it, the stop waits (`None`) to be marked on the
    /// sign-in it ran on.
    fn mark_hit(
        st: &mut State,
        id: Uuid,
        raw: &identities::Raw,
        hit: Option<&crate::limit::LimitHit>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        if st.scan_pending.contains(&id) {
            identities::Waiting::Hit(id, hit.cloned(), now).wait(st);
            return None;
        }
        // Only the plan's own window fills the account. A model's weekly
        // window holds that model alone; extra usage or credits spent stops
        // no plan session.
        let mut crossed = None;
        match hit.map(|h| h.reach()) {
            Some(crate::limit::Reach::Credit) => {
                // Extra usage is spent only once the plan's own windows are
                // too: if the last reading shows one at 100%, the account is
                // full on it; else (or no reading) it isn't.
                let full = Self::standing_mut(st, raw).plan_window_full(now);
                let Some(full) = full else {
                    tracing::info!(session = %id, window = hit.and_then(|h| h.window.as_deref()), "a session stopped on extra usage; no plan window reads full");
                    return None;
                };
                tracing::info!(session = %id, "a session stopped on extra usage with a plan window full: the account is full");
                let kind = match full.minutes {
                    Some(300) => "five_hour",
                    _ => "seven_day",
                };
                crossed = Some(crate::limit::LimitHit {
                    window: Some(kind.into()),
                    resets_at: full.until,
                });
            }
            Some(crate::limit::Reach::Model(model)) => {
                // Desk has no model of its own per session to hold, and the
                // plan has room for the others.
                tracing::info!(session = %id, %model, "a session stopped on one model's weekly limit; the account isn't full");
                return None;
            }
            _ => {}
        }
        let hit = crossed.as_ref().or(hit);
        let standing = Self::standing_mut(st, raw);
        standing.hit_at = Some(now);
        standing.hit_until = hit
            .and_then(|h| h.resets_at)
            .or(standing.hit_until.filter(|u| *u > now));
        standing.hit_window = hit.and_then(|h| h.minutes());
        Self::wrote_hit(st, raw, identities::Writer::Session(id));
        Some(now)
    }

    /// Session `id`'s turn stopped `account` on its limit now; kept as the
    /// session's own mark.
    fn mark_session_hit(
        st: &mut State,
        id: Uuid,
        raw: &identities::Raw,
        hit: Option<&crate::limit::LimitHit>,
    ) {
        Self::mark_session_hit_at(st, id, raw, hit, Utc::now());
    }

    /// [`Self::mark_session_hit`], of a stop at `now`.
    fn mark_session_hit_at(
        st: &mut State,
        id: Uuid,
        raw: &identities::Raw,
        hit: Option<&crate::limit::LimitHit>,
        now: DateTime<Utc>,
    ) {
        let s = Self::standing_mut(st, raw);
        let before = (s.hit_at, s.hit_until, s.hit_window);
        let Some(at) = Self::mark_hit(st, id, raw, hit, now) else {
            return;
        };
        st.limit_marks.insert(
            id,
            LimitMark {
                side: raw.clone(),
                at,
                before,
            },
        );
    }

    /// A refusal of `error` "rate_limit" whose words weren't the plan's, on
    /// an account whose last reading has a window full: its limit.
    fn cross_checked(
        &self,
        id: Uuid,
        failure: crate::limit::TurnFailure,
    ) -> crate::limit::TurnFailure {
        use crate::limit::{LimitHit, TurnFailure};
        let TurnFailure::Failed(error) = &failure else {
            return failure;
        };
        if error != "rate_limit" {
            return failure;
        }
        let st = self.0.state.lock();
        let now = Utc::now();
        let full = Self::side_of_session(&st, id)
            .map(|side| Self::standing_of(&st, side))
            .and_then(|s| {
                s.windows
                    .iter()
                    .filter(|w| w.used_percent >= 100.0 && w.resets_at.is_some_and(|r| r > now))
                    .filter_map(|w| w.resets_at)
                    .max()
            });
        match full {
            Some(resets_at) => TurnFailure::Limit(LimitHit {
                window: None,
                resets_at: Some(resets_at),
            }),
            None => failure,
        }
    }

    /// What a rate limit really was, and, for the plan's own, which window
    /// and when it resets: Claude Code writes that line to the transcript
    /// only after its `StopFailure`, so it is read again a little later.
    fn failure_read_later(&self, id: Uuid, account: identities::Raw, path: PathBuf) {
        let desk = self.clone();
        tokio::spawn(async move {
            for wait in [250, 1000, 3000, 8000] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let p = path.clone();
                let said = tokio::task::spawn_blocking(move || {
                    transcript::read_tail(&p).ok().and_then(|t| t.failure)
                })
                .await
                .ok()
                .flatten();
                let Some(said) = said else {
                    continue;
                };
                desk.failure_known(id, account.clone(), said);
                return;
            }
            // Nothing said: as the hook said it.
            if desk.0.state.lock().verdict_pending.remove(&id) {
                desk.drive_waiting_switch(id);
            }
        });
    }

    /// Takes back the stop session `id` marked on its account, when it was
    /// the account's last: back to what the stop before it said.
    fn undo_mark(st: &mut State, id: Uuid) {
        if let Some(mark) = st.limit_marks.remove(&id) {
            let s = Self::standing_mut(st, &mark.side);
            if s.hit_at == Some(mark.at) {
                (s.hit_at, s.hit_until, s.hit_window) = mark.before;
                if let Some(by) = st.written_by.get_mut(&mark.side) {
                    by.hit = None;
                }
            }
        }
    }

    /// What the transcript said of the API error session `id`'s turn ended
    /// on, on `account`; it decides over the hook's words. The plan's limit
    /// makes the account full until its reset, though the words didn't say
    /// so; a refusal that isn't the plan's undoes a limit read from words.
    pub(super) fn failure_known(
        &self,
        id: Uuid,
        account: identities::Raw,
        said: crate::limit::TurnFailure,
    ) {
        use crate::limit::TurnFailure;
        let settled = {
            let mut st = self.0.state.lock();
            let pending = st.verdict_pending.remove(&id);
            // Asked again since: the turn it was is behind it, though the
            // account's standing still learns when it resets.
            let current = st.turn_failures.get(&id).cloned();
            match (current, said) {
                (Some(TurnFailure::Limit(_)), TurnFailure::Limit(hit)) => {
                    if hit.reach() == crate::limit::Reach::Plan {
                        let standing = Self::standing_mut(&mut st, &account);
                        standing.hit_until = hit.resets_at.or(standing.hit_until);
                        standing.hit_window = hit.minutes().or(standing.hit_window);
                    } else {
                        // Words read as the plan's; the transcript names a
                        // model's or extra usage: the account isn't full.
                        Self::undo_mark(&mut st, id);
                        Self::mark_session_hit(&mut st, id, &account, Some(&hit));
                    }
                    st.turn_failures.insert(id, TurnFailure::Limit(hit));
                    false
                }
                (Some(TurnFailure::Failed(_)), TurnFailure::Limit(hit)) => {
                    tracing::info!(session = %id, "its transcript says the plan's limit ended its turn");
                    Self::mark_session_hit(&mut st, id, &account, Some(&hit));
                    st.turn_failures.insert(id, TurnFailure::Limit(hit));
                    true
                }
                (Some(TurnFailure::Limit(_)), TurnFailure::Failed(error)) => {
                    tracing::info!(session = %id, "its transcript says a refusal that isn't the plan's ended its turn");
                    // Only the stop this session marked: another session's
                    // stop on the same account since stands.
                    Self::undo_mark(&mut st, id);
                    st.turn_failures.insert(id, TurnFailure::Failed(error));
                    true
                }
                (None, TurnFailure::Limit(hit)) => {
                    let standing = Self::standing_mut(&mut st, &account);
                    if standing.hit_at.is_some() && hit.reach() == crate::limit::Reach::Plan {
                        standing.hit_until = hit.resets_at.or(standing.hit_until);
                        standing.hit_window = hit.minutes().or(standing.hit_window);
                    }
                    false
                }
                // A rate limit that wasn't the plan's: its turn is over.
                (Some(TurnFailure::Failed(_)), TurnFailure::Failed(_)) => pending,
                (None, TurnFailure::Failed(_)) => false,
            }
        };
        // Its turn's end is settled: a switch that waited for it goes on
        // (on the limit, with "Continue"), unless work it left still runs
        // and it isn't the limit.
        let limit = matches!(
            self.0.state.lock().turn_failures.get(&id),
            Some(TurnFailure::Limit(_))
        );
        let waiting = self.view(id).is_ok_and(|v| v.waiting_on() > 0);
        if settled && (limit || !waiting) {
            self.drive_waiting_switch(id);
        }
        let desk = self.clone();
        tokio::spawn(async move { desk.accounts_moved().await });
    }

    fn read_later(&self, id: Uuid, end: u64, reader: Arc<dyn crew::Crew>, reply: String) {
        let desk = self.clone();
        let wait = *self.0.read_wait.lock();
        let group = self.group_of_member(&session_item(id));
        tokio::spawn(async move {
            let yes = match tokio::time::timeout(wait, reader.asks(&reply, group.as_deref())).await
            {
                Ok(said) => said == Some(true),
                Err(_) => {
                    tracing::info!(%id, "reading a turn's reply took too long; it is done");
                    false
                }
            };
            drop(reader);
            let still = desk
                .0
                .state
                .lock()
                .live
                .get(&id)
                .is_some_and(|l| l.reading == Some(end));
            if !still {
                return;
            }
            desk.change(id, |r, l| {
                if let Some(l) = l {
                    l.reading = None;
                }
                if yes && r.state == SessionState::Done {
                    r.state = SessionState::Asking;
                    r.asked = Some(Asked::read_by_model(&reply));
                }
            });
        });
    }

    async fn await_permission(
        &self,
        id: Uuid,
        tool: String,
        detail: String,
        request: Option<crate::model::Request>,
        inside: Option<String>,
    ) -> Value {
        let desk_answers = self
            .record(id)
            .is_ok_and(|r| r.approvals == Approvals::Desk);
        // Which call it asks about, as Claude Code's permission request
        // doesn't say: the one of its thread's running calls it matches.
        let (call, cli_session) = {
            let st = self.0.state.lock();
            (
                st.live.get(&id).and_then(|l| {
                    let r = request.as_ref()?;
                    l.call_asked(&inside, &r.tool, &r.input)
                }),
                st.records.get(&id).and_then(|r| r.cli_session_id.clone()),
            )
        };
        let pending = PendingApproval {
            id: Uuid::new_v4(),
            tool,
            detail,
            since: Utc::now(),
            answerable: desk_answers,
            choices: Vec::new(),
            why: None,
            options: None,
            request,
            call: call.clone(),
            agent: inside.clone(),
            cli_session,
        };
        let (tx, rx) = oneshot::channel();
        self.change(id, |r, l| {
            r.state = SessionState::Blocked;
            if let Some(l) = l {
                let raised = Raised {
                    pending: pending.id,
                    agent: inside,
                    question: false,
                    call,
                };
                let answer = desk_answers.then_some(tx);
                // Another ask shows (not the main thread's question, which
                // a permission shows in front of): this one waits behind
                // it, as the terminal shows one dialog at a time.
                let shown_question = l
                    .open_question
                    .as_ref()
                    .is_some_and(|(q, _)| l.pending.as_ref().is_some_and(|p| p.id == q.id));
                if l.pending.is_some() && !shown_question {
                    l.behind.push(Behind {
                        pending: pending.clone(),
                        raised,
                        answer,
                    });
                } else {
                    l.pending = Some(pending.clone());
                    l.held = Some(raised);
                    l.answer = answer;
                }
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
                            l.answer = None;
                        }
                        if let Some(b) = l.behind.iter_mut().find(|b| b.pending.id == pending.id) {
                            b.pending.answerable = false;
                            b.answer = None;
                        }
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
            // A call of its own thread's or a sub-agent's is the session's spend.
            if let CodexEvent::Usage(spend) = event {
                self.spend_live(id, spend);
                continue;
            }
            if let CodexEvent::SteerRefused(row) = &event {
                self.line_steer_refused(id, *row).await;
                continue;
            }
            // A line message Desk typed in is not the person speaking; whose
            // a prompt is, is read once.
            let prompted = match &event {
                CodexEvent::Prompt(p) => Some(self.person_prompted(id, p)),
                _ => None,
            };
            let delivery = match &event {
                CodexEvent::Prompt(p) => match crate::line::is_delivery(p) {
                    Some(mark) => {
                        self.line_heard(id, &mark).await;
                        true
                    }
                    None => {
                        if prompted == Some(true) {
                            self.person_spoke(id);
                            self.person_drove(id);
                        }
                        false
                    }
                },
                _ => false,
            };
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
            if let CodexEvent::Answered(request) | CodexEvent::Late(request) = &event {
                // Said once Codex is told, or that its terminal had answered.
                let sent = self
                    .0
                    .state
                    .lock()
                    .live
                    .get_mut(&id)
                    .and_then(|l| l.codex_sent.remove(&request.to_string()));
                if let (Some(sent), Ok(view)) = (sent, self.view(id)) {
                    let note = match event {
                        CodexEvent::Answered(_) => {
                            decided_note(&view, &sent.pending, sent.decision, sent.by)
                        }
                        _ => {
                            let me = Party::session(&view);
                            Some(
                                Event::new(
                                    Kind::Answered,
                                    me.clone(),
                                    Some(me),
                                    "was answered in its terminal before Desk's answer reached it",
                                )
                                .approval(sent.pending.id),
                            )
                        }
                    };
                    if let Some(note) = note {
                        self.note(note);
                    }
                }
                continue;
            }
            if let CodexEvent::Reviewer(reviewer) = &event {
                // Approve for me is the TUI's to set: if Codex says otherwise,
                // the session's word for it is wrong.
                let on = self.record(id).is_ok_and(|r| r.approve_for_me);
                if on != (reviewer == "auto_review") {
                    tracing::warn!(%id, %reviewer, approve_for_me = on, "codex runs with other approvals than the session says");
                }
                continue;
            }
            if let CodexEvent::Reviewed { what, verdict } = &event {
                // What "Approve for me" decided shows as the session's activity.
                if let Ok(view) = self.view(id) {
                    let me = Party::session(&view);
                    let kind = match *verdict {
                        "allowed" => Kind::Approved,
                        _ => Kind::Denied,
                    };
                    let text = format!("Codex's reviewer {verdict} {}", crew::clip(what, 160));
                    self.note(Event::new(kind, me.clone(), Some(me), text));
                }
                continue;
            }
            if matches!(event, CodexEvent::Idle) {
                // Its rollout has the turn's calls, with their model.
                self.spend_read_ending(id);
                self.breaker_settled(id);
            }
            match &event {
                CodexEvent::Tool(t) => {
                    // Codex takes no answer before a call: a turn its breaker
                    // holds (one the person didn't start) is told why and
                    // ended at its first call.
                    if let Some(why) = self.breaker_tool(id, t) {
                        self.steer_codex(id, why);
                        let breaker = crew::By {
                            party: Party::breaker(),
                            reason: None,
                            suggestion_id: None,
                            hop: 0,
                        };
                        if let Err(e) = self.interrupt(id, Some(breaker)) {
                            tracing::warn!(%id, error = %e, "couldn't end a held Codex turn");
                        }
                    }
                }
                CodexEvent::Failed(t) => self.breaker_failed(id, t),
                CodexEvent::Prompt(_) => {
                    self.breaker_turn(id, prompted == Some(true) && !delivery);
                }
                _ => {}
            }
            let turn_begins = matches!(event, CodexEvent::Working { .. })
                && self.record(id).is_ok_and(|r| {
                    !matches!(r.state, SessionState::Working | SessionState::Blocked)
                });
            let baseline = match turn_begins {
                true => self.already_running(id).await,
                false => None,
            };
            let running = match &event {
                CodexEvent::Idle => self.running_now(id).await,
                _ => None,
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
            // Its turn's end, read as Claude Code's is: whether it asks.
            let prompts = self.0.state.lock().live.get(&id).map(|l| l.prompts);
            let (asked, unsure) = match &tail {
                Some((_, t)) => asked_at_end(&[], t.said.as_deref()),
                None => (None, false),
            };
            let reader = unsure.then(|| self.reader()).flatten();
            let said_whole = tail
                .as_ref()
                .and_then(|(_, t)| t.said.clone())
                .filter(|_| reader.is_some());
            let mut read = None;
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
                            r.asked = None;
                            // A repeated notice is the same request, still asked.
                            let asked = l.pending.take();
                            if !waiting {
                                l.codex_asks.clear();
                                l.codex_question = None;
                            }
                            l.pending = waiting.then(|| {
                                asked.unwrap_or_else(|| PendingApproval {
                                    id: Uuid::new_v4(),
                                    tool: l.current_tool.clone().unwrap_or_else(|| "Codex".into()),
                                    detail: "Answer in the terminal".into(),
                                    since: Utc::now(),
                                    answerable: false,
                                    choices: Vec::new(),
                                    why: None,
                                    options: None,
                                    request: None,
                                    call: None,
                                    agent: None,
                                    cli_session: None,
                                })
                            });
                        }
                    }
                    CodexEvent::Idle => {
                        let asked = asked.filter(|_| l.as_deref().map(|l| l.prompts) == prompts);
                        if r.state != SessionState::Starting {
                            r.state = match asked {
                                Some(_) => SessionState::Asking,
                                None => SessionState::Done,
                            };
                            if let Some(l) = l.as_deref_mut() {
                                l.ends += 1;
                                l.reading = reader.as_ref().map(|_| l.ends);
                                read = l.reading;
                            }
                        }
                        if let Some((path, t)) = tail {
                            r.transcript = Some(path.to_string_lossy().into_owned());
                            r.last_prompt = t.last_prompt.or(r.last_prompt.take());
                            r.last_reply = t.last_reply.or(r.last_reply.take());
                        }
                        r.asked = asked;
                        if let Some(l) = l {
                            l.current_tool = None;
                            l.pending = None;
                            l.codex_asks.clear();
                            l.codex_question = None;
                            l.background =
                                std::mem::take(&mut l.background).looked_if(running, Utc::now());
                        }
                    }
                    CodexEvent::Prompt(p) => {
                        r.last_prompt = Some(p);
                        if let Some(l) = l.filter(|_| !delivery) {
                            l.prompts += 1;
                        }
                    }
                    CodexEvent::Tool(t) => {
                        // A command it asks to run starts before it asks:
                        // what it waits on still holds it.
                        if l.as_ref().is_none_or(|l| l.pending.is_none()) {
                            r.state = SessionState::Working;
                        }
                        if let Some(l) = l {
                            l.current_tool = Some(t);
                        }
                    }
                    CodexEvent::Asks(ask) => {
                        if let Some(l) = l.filter(|_| !ask.choices.is_empty()) {
                            codex_asks(r, l, ask);
                        }
                    }
                    CodexEvent::Question { request, question } => {
                        if let Some(l) = l {
                            codex_question(r, l, request, question);
                        }
                    }
                    CodexEvent::Resolved(request) => {
                        if let Some(l) = l {
                            codex_resolved(r, l, &request);
                        }
                    }
                    CodexEvent::Answered(_) | CodexEvent::Late(_) | CodexEvent::Failed(_) => {}
                    CodexEvent::Reply(text) => r.last_reply = Some(transcript::clip(&text)),
                    CodexEvent::Child(_)
                    | CodexEvent::Usage(_)
                    | CodexEvent::SteerRefused(_)
                    | CodexEvent::Reviewed { .. }
                    | CodexEvent::Reviewer(_) => {}
                }
            });
            if let (Some(reader), Some(end), Some(reply)) = (reader, read, said_whole) {
                self.read_later(id, end, reader, reply);
            }
        }
        // Its server went away: what it asked can't be answered from Desk.
        self.change(id, |r, l| {
            if let Some(l) = l {
                if !l.codex_asks.is_empty() || l.codex_question.is_some() {
                    l.codex_asks.clear();
                    l.codex_question = None;
                    codex_front(r, l);
                }
            }
        });
    }

    async fn adopt_loop(self, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let Some(claude) = self.program(Engine::Claude) else {
                return;
            };
            match adopt::list(claude, &self.0.env).await {
                Ok(found) => self.reconcile_external(found).await,
                Err(e) => tracing::debug!(error = %e, "could not list other Claude Code sessions"),
            }
        }
    }

    /// Takes back the record the registry keeps for `engine`'s CLI session
    /// `cli` when the hub doesn't hold it: a closed one isn't loaded at
    /// start, and the CLI session can run again (resumed in another
    /// terminal, or in one of Desk's). It is the same session, so it keeps
    /// its record, name, group and history; a second record for the same
    /// CLI session can't be saved (one per CLI session id, closed ones too).
    pub(crate) async fn recall(&self, engine: Engine, cli: &str) -> Option<Uuid> {
        if self.is_hidden(engine, cli) {
            // Removed from Desk: only starting it again tracks it again.
            return None;
        }
        let held = |st: &State| {
            st.records
                .values()
                .find(|r| r.engine == engine && r.cli_session_id.as_deref() == Some(cli))
                .map(|r| r.id)
        };
        if let Some(id) = held(&self.0.state.lock()) {
            return Some(id);
        }
        let kept = match self.0.store.by_cli_session(engine, cli).await {
            Ok(kept) => kept?,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    cli,
                    "couldn't look up a session the registry may keep"
                );
                return None;
            }
        };
        let mut st = self.0.state.lock();
        // Taken while the registry was asked.
        if let Some(id) = held(&st) {
            return Some(id);
        }
        if st.forgotten.contains(&kept.id) {
            return None;
        }
        let id = kept.id;
        st.records.insert(id, kept);
        Some(id)
    }

    async fn reconcile_external(&self, found: Vec<External>) {
        let hub = &self.0.opts.hub_internal;
        let held: Vec<Uuid> = self
            .0
            .state
            .lock()
            .records
            .values()
            .filter(|r| Self::hubs_own(hub, r))
            .map(|r| r.id)
            .collect();
        for id in held {
            self.forget_hubs_own(id).await;
        }
        let found: Vec<External> = found
            .into_iter()
            .filter(|e| !hub.holds(Path::new(&e.cwd)))
            .collect();
        let table = if hub.knows_its_own() && found.iter().any(|e| e.pid.is_some()) {
            procs::table().await.unwrap_or_default()
        } else {
            Vec::new()
        };
        let found = found
            .into_iter()
            .filter(|e| !e.pid.is_some_and(|pid| hub.started(&table, pid)));
        // Ones the person removed stay removed, until they track them again.
        let found: Vec<External> = found
            .filter(|e| !self.is_hidden(Engine::Claude, &e.session_id))
            .collect();
        for ext in &found {
            self.recall(Engine::Claude, &ext.session_id).await;
        }
        let home = self.0.env.get("HOME").map(PathBuf::from);
        let (ours, by_cli): (HashSet<String>, HashMap<String, Uuid>) = {
            let st = self.0.state.lock();
            (
                // Running here, or in one of Desk's terminals (whose hooks
                // report it): never another terminal's.
                st.records
                    .values()
                    .filter(|r| {
                        st.live.contains_key(&r.id)
                            || (r.terminal_id.is_some() && r.state.is_running())
                    })
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
                    if let Some(t) = &tail {
                        self.login_in_transcript(id, t.last_login.clone()).await;
                    }
                    if !unchanged {
                        // Its transcript says when its turn ended on an API
                        // error: the plan's limit (and when it resets), or
                        // another failure, never read from its words.
                        if let Some(t) = &tail {
                            let mut st = self.0.state.lock();
                            match t.failure.clone() {
                                Some(f) => st.turn_failures.insert(id, f),
                                None => st.turn_failures.remove(&id),
                            };
                        }
                        self.change(id, |r, _| {
                            r.origin = Origin::External;
                            r.state = state;
                            r.external_pid = ext.pid;
                            r.restore_offer = false;
                            r.moved_to(ext.cwd.clone());
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
                    let mut tail = tail.unwrap_or_default();
                    let failure = tail.failure.take();
                    let r = SessionRecord {
                        id: Uuid::new_v4(),
                        engine: Engine::Claude,
                        view: View::Terminal,
                        engine_version: None,
                        approve_for_me: false,
                        fast: false,
                        account: None,
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
                        folder_chosen: false,
                        asked: None,
                        created_at: now,
                        updated_at: now,
                    };
                    present.insert(r.id);
                    if let Some(f) = failure {
                        self.0.state.lock().turn_failures.insert(r.id, f);
                    }
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
            drop(st);
            // A session in another terminal: what it did is read as it changes.
            self.spend_read(id);
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

/// What the person is asked before a room's conversation goes to `hubs`
/// it didn't go to before.
fn room_shares(hubs: &[String]) -> String {
    let named = match hubs {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!(
        "Agents on {named} will see this room's conversation: its thread context goes to {}. \
         Say it may, with shareWith.",
        if hubs.len() == 1 {
            "that hub"
        } else {
            "those hubs"
        }
    )
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
    // What a session asks that couldn't be written: written again with the
    // next save, until it is (or a newer write of it is).
    let mut unwritten: HashMap<Uuid, Vec<asks::KeptAsk>> = HashMap::new();
    while let Some(op) = rx.recv().await {
        let result = match op {
            Persist::Put(r) => {
                let put = store.put(&r).await;
                for (session, asks) in std::mem::take(&mut unwritten) {
                    if let Err(e) = store.put_open_asks(session, &asks).await {
                        tracing::warn!(%session, error = format!("{e:#}"), "couldn't keep what a session asks; trying again");
                        unwritten.insert(session, asks);
                    }
                }
                put
            }
            Persist::Attention(id, at) => store.set_attention(id, at).await,
            Persist::Forget {
                ids,
                hidden,
                memory,
                done,
            } => {
                let result = store.remove_sessions(&ids, &hidden, YOU, memory).await;
                if let Some(done) = done {
                    let _ = done.send(result.is_ok());
                }
                result.map(drop)
            }
            Persist::Unhide(engine, cli) => store.unhide(engine, &cli).await,
            Persist::Event(e) => store.add_event(&e).await,
            Persist::Terminal(t) => store.put_terminal(&t).await,
            Persist::DeleteTerminal(id) => store.delete_terminal(id).await,
            Persist::Turn(op) => {
                if let Some(turns) = &turns {
                    turns.send(*op);
                }
                Ok(())
            }
            Persist::Asks(session, asks) => {
                let put = store.put_open_asks(session, &asks).await;
                match put {
                    Ok(()) => unwritten.remove(&session),
                    Err(_) => unwritten.insert(session, asks),
                };
                put
            }
        };
        if let Err(e) = result {
            // The whole chain: the outermost context alone ("saving desk
            // session") never says why.
            tracing::error!(error = format!("{e:#}"), "desk registry write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{build_pool, tests::record_fixture};
    use chrono::SubsecRound;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use workforce_postgres::test_support::with_test_pool;

    /// A stand-in CLI that stays up until killed, ignoring its arguments.
    pub(super) fn fake_cli(dir: &Path) -> PathBuf {
        let path = dir.join("fake-cli");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub(super) fn options(dir: &Path) -> Options {
        Options {
            programs: crate::testing::programs(fake_cli(dir), fake_cli(dir)),
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
            hub_internal: HubInternal::default(),
            account_keys: None,
            line_reach: crate::line::Reach::Group,
            memory: Default::default(),
            bosses: Default::default(),
        }
    }

    pub(super) async fn desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir),
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        d.count_budgets_in(chrono_tz::UTC);
        d
    }

    fn token(d: &Desk, id: Uuid) -> String {
        d.0.state.lock().live[&id].token.clone()
    }

    pub(super) async fn new_session(d: &Desk, dir: &Path) -> SessionView {
        d.create(
            NewSession {
                fresh_worktree: false,
                engine: Engine::Claude,
                view: None,
                name: "auth-fix".into(),
                cwd: dir.to_string_lossy().into_owned(),
                goal: "fix the race".into(),
                worktree: None,
                prompt: None,
                approvals: None,
                approve_for_me: false,
                fast: false,
                account: None,
                env: Default::default(),
                group: None,
            },
            None,
        )
        .await
        .unwrap()
    }

    pub(super) async fn post(d: &Desk, id: Uuid, body: Value) -> Value {
        let t = token(d, id);
        d.hook(id, &t, body.to_string().as_bytes()).await.unwrap()
    }

    #[test]
    fn a_codex_turns_background_is_what_its_app_server_started_in_it() {
        let table = procs::parse(
            "10 1 codex app-server\n\
             11 10 node mcp-server.js\n\
             12 10 /bin/zsh -c npm run dev\n\
             13 12 node vite\n\
             14 13 esbuild --service\n",
        );
        let look = Look {
            root: 10,
            engine: Engine::Codex,
            baseline: Some(HashSet::from([11])),
        };
        assert_eq!(
            Desk::running(&table, &look),
            1,
            "one terminal, however deep"
        );
        assert_eq!(
            Desk::running(
                &table,
                &Look {
                    baseline: None,
                    ..look
                }
            ),
            0,
            "nothing is known to be the turn's"
        );
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
    async fn a_session_the_holder_still_runs_is_never_resumed_a_second_time() {
        with_test_pool(build_pool, |pool| async move {
            let (hdir, socket, _holder_cancel) = crate::holder::tests::holder().await;
            let dir = tempfile::tempdir().unwrap();
            let mut opts = options(dir.path());
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
            first.change(s.id, |r, _| r.cli_session_id = Some("cli-1".into()));
            tokio::time::sleep(Duration::from_millis(200)).await;
            first_cancel.cancel();
            tokio::time::sleep(Duration::from_millis(300)).await;

            // The next hub starts while the holder is out of its reach: the
            // session looks lost to it, though its process runs on.
            let away = hdir.path().join("away.sock");
            std::fs::rename(&socket, &away).unwrap();
            let second = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts.clone(),
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!second.view(s.id).unwrap().live);
            std::fs::rename(&away, &socket).unwrap();

            let refused = second.resume(s.id, None, None).await.unwrap_err();
            assert!(refused.to_string().contains("still running"), "{refused}");
            assert!(!second.view(s.id).unwrap().live, "a second one was started");
            let holder = Holder::connect(&socket, crate::holder::tests::TOKEN)
                .await
                .unwrap();
            let running = holder
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|h| !h.exited && h.meta["session"].as_str() == Some(&s.id.to_string()))
                .count();
            assert_eq!(running, 1, "one process for the conversation");
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
            let logged = Arc::new(Logged::default());
            let floor = || Floor {
                turns: Some(logged.clone()),
                ..Default::default()
            };
            let first_cancel = CancellationToken::new();
            let first = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts.clone(),
                floor(),
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
            let turns = first.open_turns();
            assert_eq!(turns.len(), 1, "its prompt began a turn");
            // Signed in again inside it, its transcript run on long since.
            let began = first.0.state.lock().live[&s.id].started_at;
            let transcript = dir.path().join("held.jsonl");
            let at = (Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
            let mut lines = vec![
                format!(r#"{{"type":"user","uuid":"in-1","timestamp":"{at}","message":{{"role":"user","content":"<command-name>/login</command-name>"}}}}"#),
                format!(r#"{{"type":"user","uuid":"in-1s","timestamp":"{at}","message":{{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}}}"#),
            ];
            lines.extend(std::iter::repeat_n(
                format!(r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#, "x".repeat(8192)),
                100,
            ));
            std::fs::write(&transcript, lines.join("\n")).unwrap();
            let path = transcript.to_string_lossy().into_owned();
            // On an account: only then is it read for one.
            first.change(s.id, |r, _| {
                r.transcript = Some(path);
                r.account = Some(Uuid::new_v4());
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            first_cancel.cancel();
            tokio::time::sleep(Duration::from_millis(300)).await;

            let second = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts.clone(),
                floor(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let v = second.view(s.id).unwrap();
            assert!(v.live, "taken back");
            assert_eq!(
                second.0.state.lock().live[&s.id].started_at,
                began,
                "its process started when it did, not when it was taken back"
            );
            // Once the CLI's sign-in is read (the stand-in CLI never says:
            // its read gives up after a while).
            for _ in 0..1600 {
                if second.0.state.lock().relogged.contains(&s.id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            {
                let st = second.0.state.lock();
                assert!(
                    st.relogged.contains(&s.id),
                    "the login inside it is found however far back"
                );
                assert_eq!(st.logins_seen.get(&s.id).map(String::as_str), Some("in-1"));
                assert!(st.scan_pending.is_empty(), "what it reports is filed again");
            }
            assert_eq!(v.state, SessionState::Working, "not interrupted");
            assert!(second.restore_offer().is_empty(), "nothing to restore");
            assert_eq!(
                second.open_turns(),
                turns,
                "the turn it is in runs on the floor, as it does in the sidebar"
            );
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
            let turn = turns.into_iter().next().unwrap();
            assert!(
                logs(&logged, |ops| ops.iter().any(|op| matches!(
                    op,
                    TurnOp::End { turn: t, outcome: Outcome::Finished, .. } if *t == turn
                )))
                .await,
                "and ends when it does"
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
    async fn claude_code_follows_desks_terminal_unless_the_person_set_a_theme() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir(&home).unwrap();
            // Records its arguments and what it reads light or dark from.
            let cli = dir.path().join("recording-cli");
            let out = dir.path().join("launched");
            std::fs::write(
                &cli,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$COLORFGBG\" \"$@\" > '{}'\nexec sleep 60\n",
                    out.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut opts = options(dir.path());
            opts.programs = crate::testing::programs(cli, fake_cli(dir.path()));
            opts.env
                .insert("HOME".into(), home.to_string_lossy().into_owned());
            DeskStore::migrate(&pool).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts,
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let day = crate::vt::Colors {
                foreground: "#24251F".into(),
                background: "#FFFFFF".into(),
                cursor: "#3F5A06".into(),
                ansi: vec!["#1B1C18".into(); 16],
                ground: crate::vt::Ground::Light,
            };
            assert!(matches!(
                d.set_terminal_colors(crate::vt::Colors {
                    ansi: vec![],
                    ..day.clone()
                })
                .await,
                Err(DeskError::Invalid(_))
            ));
            d.set_terminal_colors(day).await.unwrap();
            let launched = || async {
                for _ in 0..100 {
                    if let Ok(text) = std::fs::read_to_string(&out) {
                        if text.contains("--settings") {
                            std::fs::remove_file(&out).unwrap();
                            return text;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                panic!("the CLI never started");
            };
            let settings = |text: &str| -> Value {
                let lines: Vec<&str> = text.lines().collect();
                let at = lines.iter().position(|l| *l == "--settings").unwrap();
                serde_json::from_str(lines[at + 1]).unwrap()
            };

            // No theme of the person's: it follows the terminal.
            let s = new_session(&d, dir.path()).await;
            let text = launched().await;
            assert_eq!(text.lines().next(), Some("0;15"), "COLORFGBG says light");
            assert_eq!(settings(&text)["theme"], "auto");
            assert_eq!(s.fixed_ground, None, "it follows the terminal as it runs");
            let config: Value =
                serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                    .unwrap();
            assert_eq!(
                config["theme"], "auto",
                "its onboarding is settled to follow too"
            );

            // One the person set is theirs.
            std::fs::write(
                home.join(".claude.json"),
                r#"{"hasCompletedOnboarding":true,"theme":"dark-daltonized"}"#,
            )
            .unwrap();
            new_session(&d, dir.path()).await;
            let text = launched().await;
            assert!(settings(&text).get("theme").is_none());

            // So is one `/theme` wrote to its settings: the flag would beat it.
            std::fs::write(
                home.join(".claude.json"),
                r#"{"hasCompletedOnboarding":true}"#,
            )
            .unwrap();
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::write(home.join(".claude/settings.json"), r#"{"theme":"light"}"#).unwrap();
            new_session(&d, dir.path()).await;
            let text = launched().await;
            assert!(settings(&text).get("theme").is_none());
            let config: Value =
                serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                    .unwrap();
            assert!(config.get("theme").is_none(), "the config is left alone");
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_finds_claude_code_set_up_and_its_folder_trusted_only_when_the_person_chose_it(
    ) {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir(&home).unwrap();
            let config = home.join(".claude.json");
            std::fs::write(&config, r#"{"userID":"u1","theme":"light"}"#).unwrap();
            let mut opts = options(dir.path());
            opts.env
                .insert("HOME".into(), home.to_string_lossy().into_owned());
            DeskStore::migrate(&pool).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts,
                Floor {
                    turns: None,
                    plans: None,
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let repo = dir.path().join("acme-api");
            std::fs::create_dir(&repo).unwrap();
            for args in [
                &["init", "-q", "-b", "main"][..],
                &[
                    "-c",
                    "user.email=d@e",
                    "-c",
                    "user.name=d",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "init",
                ],
            ] {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap();
                assert!(ok.success());
            }
            let elsewhere = dir.path().join("acme-web");
            std::fs::create_dir(&elsewhere).unwrap();
            let new = |cwd: &Path, worktree: Option<&str>| NewSession {
                fresh_worktree: false,
                engine: Engine::Claude,
                view: None,
                name: "login-race".into(),
                cwd: cwd.to_string_lossy().into_owned(),
                goal: String::new(),
                worktree: worktree.map(str::to_string),
                prompt: None,
                approvals: None,
                approve_for_me: false,
                fast: false,
                account: None,
                env: Default::default(),
                group: None,
            };
            let trusted = |cwd: &str| {
                let doc: Value =
                    serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
                let real = std::fs::canonicalize(cwd).unwrap();
                doc["projects"][real.to_str().unwrap()]["hasTrustDialogAccepted"].clone()
            };
            // An agent's start: onboarding, and no trust it wasn't given.
            let agent = d
                .create(
                    new(&elsewhere, None),
                    Some(By::manager("scout", "Scout", "")),
                )
                .await
                .unwrap();
            let doc: Value =
                serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
            assert_eq!(doc["hasCompletedOnboarding"], Value::Bool(true));
            assert_eq!(doc["theme"], "light", "the person's theme is kept");
            assert_eq!(doc["userID"], "u1");
            assert_eq!(
                trusted(&agent.cwd),
                Value::Null,
                "an agent's start trusts nothing"
            );

            let plain = d.create_chosen(new(&repo, None)).await.unwrap();
            let in_worktree = d
                .create_chosen(new(&repo, Some("desk/login-race")))
                .await
                .unwrap();
            assert!(in_worktree.cwd.contains("/.worktrees/"));
            for s in [&plain, &in_worktree] {
                assert_eq!(trusted(&s.cwd), Value::Bool(true), "{} is trusted", s.cwd);
            }
            let kept = DeskStore::from_pool(pool.clone());
            assert!(kept.get(plain.id).await.unwrap().unwrap().folder_chosen);
            assert!(!kept.get(agent.id).await.unwrap().unwrap().folder_chosen);
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
    struct Readings(Mutex<Vec<(LocalPlan, Value, crate::usage::LocalAccount)>>);

    impl PlanSink for Readings {
        fn local(&self, plan: LocalPlan, payload: Value, account: crate::usage::LocalAccount) {
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
                 '--version ') echo '2.1.300 (Claude Code)' ;;\n\
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
                    programs: crate::testing::programs(cli.clone(), cli),
                    ..options(dir.path())
                },
                Floor {
                    turns: None,
                    plans: Some(readings.clone()),
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let max = crate::usage::LocalAccount {
                ids: vec!["org-1".into(), "person@example.com".into()],
                plan: Some("max".into()),
            };
            for _ in 0..200 {
                if d.sign_ins().of(Engine::Codex).on_plan() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let ins = d.sign_ins();
            assert_eq!(ins.of(Engine::Claude).plan_account(), Some(max.clone()));
            assert!(ins.of(Engine::Codex).on_plan(), "Codex says it on stderr");
            let engines = d.engines();
            let claude = engines.iter().find(|e| e.engine == Engine::Claude).unwrap();
            assert_eq!(
                (claude.installed, claude.version.map(|v| v.to_string())),
                (Some(true), Some("2.1.300".to_string()))
            );
            let gemini = engines.iter().find(|e| e.engine == Engine::Gemini).unwrap();
            assert_eq!(
                (gemini.installed, gemini.views.len()),
                (None, 0),
                "no program named, never run"
            );

            let s = d
                .create(
                    NewSession {
                        fresh_worktree: false,
                        engine: Engine::Claude,
                        view: None,
                        name: "auth-fix".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: String::new(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        approve_for_me: false,
                        fast: false,
                        account: None,
                        env: BTreeMap::from([(PLUGIN_DIRS.into(), "/plugins/desk".into())]),
                        group: None,
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
            // Kept by the account the sign-in is (its identity), whichever
            // credential of it reports.
            let identity = crate::identity::Identity::of("org-1", "person@example.com").unwrap();
            assert_eq!(
                readings.0.lock().clone(),
                [(
                    LocalPlan::Claude,
                    limits,
                    crate::usage::LocalAccount {
                        ids: vec![identity.plan_id()],
                        plan: Some("max".into()),
                    }
                )],
                "the reading is the session's sign-in's"
            );
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Starting, "and changes nothing of it");

            assert!(d
                .create(
                    NewSession {
                        fresh_worktree: false,
                        engine: Engine::Claude,
                        view: None,
                        name: "bad".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: String::new(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        approve_for_me: false,
                        fast: false,
                        account: None,
                        env: BTreeMap::from([("NOT A NAME".into(), "x".into())]),
                        group: None,
                    },
                    None,
                )
                .await
                .is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn what_desk_adds_to_sessions_outlives_the_hub() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let cli = signed_in_cli(dir.path());
            let opts = || Options {
                programs: crate::testing::programs(cli.clone(), cli.clone()),
                ..options(dir.path())
            };
            let no_floor = || Floor {
                turns: None,
                plans: None,
                pricer: None,
                redactor: None,
                index: None,
                deriver: None,
                accountant: None,
            };
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                opts(),
                no_floor(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let added = BTreeMap::from([(PLUGIN_DIRS.to_string(), "/plugins/desk".to_string())]);
            d.create(
                NewSession {
                    fresh_worktree: false,
                    engine: Engine::Claude,
                    view: None,
                    name: "auth-fix".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    account: None,
                    env: added.clone(),
                    group: None,
                },
                None,
            )
            .await
            .unwrap();
            let store = DeskStore::from_pool(pool.clone());
            for _ in 0..200 {
                if !kept_session_env(&store).await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(kept_session_env(&store).await, added);

            let again = Desk::start(store, opts(), no_floor(), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(
                again
                    .with_added(&again.0.state.lock().session_env.clone())
                    .get(PLUGIN_DIRS),
                Some(&"/plugins/desk".to_string()),
                "a session the hub starts or resumes after a restart loads Desk's plugin"
            );
        })
        .await;
    }

    /// Keeps every op the desk logs, and reads back the turns they make.
    #[derive(Default)]
    pub(super) struct Logged(pub(super) Mutex<Vec<TurnOp>>);

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
            Ok(self.0.lock().iter().fold(Vec::new(), crate::floor::apply))
        }
    }

    /// Whether what `logged` holds comes to satisfy `f`.
    pub(super) async fn logs(logged: &Logged, f: impl Fn(&[TurnOp]) -> bool) -> bool {
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
                options(dir.path()).with_program(Engine::Claude, shell_cli(dir.path())),
                Floor {
                    turns: Some(logged.clone()),
                    plans: None,
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
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
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let s = new_session(&d, dir.path()).await;
            post(
                &d,
                s.id,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"}),
            )
            .await;
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
                )) && ops
                    .iter()
                    .any(|op| matches!(op, TurnOp::Settle { .. })))
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

    /// What a session is unread by: its turn ending by itself, not your
    /// prompt, its work, your Stop, nor a restart that lost it.
    #[tokio::test]
    async fn a_session_is_unread_by_what_it_did_not_by_what_was_done_to_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool.clone(), dir.path()).await;
            // Desk begins keeping read points: what's older counts as read.
            let crew = super::unread::tests::NoCrew;
            d.unread(&crew, YOU).await.unwrap();
            let s = new_session(&d, dir.path()).await;
            post(&d, s.id, json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()})).await;
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the tests"})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo test"}})).await;
            assert_eq!(d.attention_at(s.id), None, "starting and working are not");

            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "All green."})).await;
            let finished = d.attention_at(s.id).expect("its turn ended");
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);

            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "push it"})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "git push"}})).await;
            assert_eq!(d.attention_at(s.id), Some(finished), "your prompt and its work are not");

            d.stop(s.id, Some(By::you())).unwrap();
            for _ in 0..100 {
                if !d.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Stopped);
            assert_eq!(d.attention_at(s.id), Some(finished), "your Stop is not");

            // The summary counts it from its turn's end, not from your Stop,
            // and once that's read your Stop leaves it read.
            let listed = d.unread(&crew, YOU).await.unwrap().sessions;
            assert_eq!(
                listed.iter().map(|u| (u.id, u.since)).collect::<Vec<_>>(),
                vec![(s.id, finished)]
            );
            d.put_reads(
                YOU,
                &[crate::reads::ReadPut {
                    place: crate::reads::ReadPlace::Session(s.id),
                    point: crate::reads::ReadPoint {
                        at: finished,
                        message: None,
                    },
                    mark: crate::reads::Mark::Advance,
                }],
            )
            .await
            .unwrap();
            assert!(d.unread(&crew, YOU).await.unwrap().sessions.is_empty());

            // Another, lost while it works when the hub restarts, failed.
            let lost = new_session(&d, dir.path()).await;
            post(&d, lost.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "migrate"})).await;
            // Kept across a restart, to the microsecond.
            let kept = loop {
                let working = d.store().get(lost.id).await.unwrap().map(|r| r.state);
                if let (Some(at), Some(SessionState::Working)) =
                    (d.store().attention_times().await.unwrap().get(&s.id), working)
                {
                    break *at;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert_eq!(kept, finished.trunc_subsecs(6));
            let again = desk(pool, dir.path()).await;
            assert_eq!(again.attention_at(s.id), Some(kept));
            assert_eq!(again.view(lost.id).unwrap().state, SessionState::Interrupted);
            assert!(again.attention_at(lost.id).is_some(), "work lost is");
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
                        fresh_worktree: false,
                        engine: Engine::Claude,
                        view: None,
                        name: "acme-web".into(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        goal: "upgrade the date picker".into(),
                        worktree: None,
                        prompt: None,
                        approvals: None,
                        approve_for_me: false,
                        fast: false,
                        account: None,
                        env: Default::default(),
                        group: None,
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

    /// A turn that asks two questions while a sub-agent it started works on,
    /// replayed in the order its Claude Code 2.1.317 transcript (auto mode)
    /// kept: the question's start, the sub-agent's own calls, then the
    /// question's end once both were answered in the terminal; and the
    /// notice and permission check that may come between, which aren't
    /// recorded.
    #[tokio::test]
    async fn a_question_asked_in_the_terminal_needs_you_until_its_last_answer() {
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
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let s = new_session(&d, dir.path()).await;
            let questions = claude::tests::two_questions();
            let asking = |d: &Desk| {
                let v = d.view(s.id).unwrap();
                let p = v.pending.clone();
                (v.state, p.as_ref().map(|p| p.detail.clone()), p.and_then(|p| p.options).map(|o| o.len()))
            };
            let waiting = (
                SessionState::Blocked,
                Some("Send the three messages at once or one after another?".to_string()),
                Some(3),
            );
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "plan the change"})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Agent", "tool_use_id": "toolu_a",
                "tool_input": {"description": "Open the PR", "prompt": "…", "run_in_background": true}})).await;
            post(&d, s.id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1"})).await;
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "Agent", "tool_use_id": "toolu_a"})).await;
            let answer = post(&d, s.id, json!({"session_id": "s1", "permission_mode": "auto", "hook_event_name": "PreToolUse",
                "tool_name": "AskUserQuestion", "tool_input": questions, "tool_use_id": "toolu_q"})).await;
            assert_eq!(answer, json!({}), "the terminal's dialog asks it");
            assert_eq!(asking(&d), waiting);
            let first = d.view(s.id).unwrap().pending.unwrap();
            assert!(!first.answerable);
            assert_eq!(first.why.as_deref(), Some("(1 of 2 questions)"));

            // The sub-agent works on, and ends: the question still waits.
            for n in 0..3 {
                post(&d, s.id, json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "agent_type": "general-purpose",
                    "tool_name": "Bash", "tool_input": {"command": format!("cargo test {n}")}, "tool_use_id": format!("toolu_s{n}")})).await;
                assert_eq!(asking(&d), waiting, "a sub-agent's call {n}");
                post(&d, s.id, json!({"hook_event_name": "PostToolUse", "agent_id": "a1", "agent_type": "general-purpose",
                    "tool_name": "Bash", "tool_use_id": format!("toolu_s{n}")})).await;
                assert_eq!(asking(&d), waiting, "a sub-agent's call {n} ended");
            }
            post(&d, s.id, json!({"hook_event_name": "SubagentStop", "agent_id": "a1"})).await;
            // Its dialog's own notice, and a sign-in's, change nothing.
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
                "message": "Claude needs your permission to use AskUserQuestion"})).await;
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "auth_success", "message": "Signed in"})).await;
            // Its permission check, were it sent, is the same question.
            let answer = post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "tool_name": "AskUserQuestion",
                "tool_input": claude::tests::two_questions(), "tool_use_id": "toolu_q"})).await;
            assert_eq!(answer, claude::ask_in_terminal());
            assert_eq!(d.view(s.id).unwrap().pending.unwrap().id, first.id, "the same question");
            assert_eq!(asking(&d), waiting);

            // On the floor: a question, with its options, answered in the terminal.
            assert!(logs(&logged, |ops| ops.iter().any(|op| matches!(op, TurnOp::Wait { .. }))).await);
            let needs = crate::floor::open_needs(&logged.0.lock().iter().fold(Vec::new(), crate::floor::apply));
            let [need] = &needs[..] else { panic!("{needs:?}") };
            assert_eq!(need.kind, crate::floor::NeedKind::Question);
            assert_eq!(need.answer, crate::floor::Answer::Terminal);
            assert_eq!(
                need.ask,
                "Send the three messages at once or one after another? (1 of 2 questions)"
            );
            assert_eq!(need.options.as_ref().map(Vec::len), Some(3));

            // Both answered: the call ends, and so does the wait.
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion",
                "tool_use_id": "toolu_q", "tool_response": {"answers": {}}})).await;
            assert_eq!(asking(&d), (SessionState::Working, None, None));
            assert!(
                logs(&logged, |ops| ops.iter().any(|op| matches!(op, TurnOp::Unwait { key, .. } if *key == first.id.to_string())))
                    .await
            );
            assert!(crate::floor::open_needs(&logged.0.lock().iter().fold(Vec::new(), crate::floor::apply)).is_empty());

            // A plan to approve waits the same way. Turned down in its
            // dialog, it fires no hook of its own (2.1.317): the session is
            // back at its prompt, which `idle_prompt` says, and the wait goes.
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_p",
                "tool_input": {"plan": "# Shadow the reads\n1. Mirror the writes."}})).await;
            let p = d.view(s.id).unwrap().pending.unwrap();
            assert_eq!((p.tool.as_str(), p.detail.as_str()), ("Plan", "Shadow the reads"));
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "idle_prompt",
                "message": "Claude is waiting for your input"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Done, None));
            // At its prompt with nothing asked, the notice changes nothing.
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "idle_prompt", "message": "m"})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);
            // An interrupted call with nothing waiting doesn't make it work.
            post(&d, s.id, json!({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
                "tool_use_id": "toolu_x", "error": "Interrupted", "is_interrupt": true})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);

            // A question the next prompt answers (Esc, then typing).
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
                "tool_input": claude::tests::two_questions(), "tool_use_id": "toolu_q2"})).await;
            assert_eq!(asking(&d).0, SessionState::Blocked);
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "do the first"})).await;
            assert_eq!(asking(&d), (SessionState::Working, None, None));

            // A sub-agent's permission shown in front of the person's open
            // question ends with its call; the question shows again.
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
                "tool_input": claude::tests::two_questions(), "tool_use_id": "toolu_q3"})).await;
            let question = d.view(s.id).unwrap().pending.unwrap();
            d.update(s.id, SessionPatch { approvals: Some(Approvals::Terminal), ..Default::default() }).unwrap();
            post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "agent_id": "a2", "tool_name": "Bash",
                "tool_input": {"command": "git push"}})).await;
            assert_eq!(d.view(s.id).unwrap().pending.unwrap().tool, "Bash(git push)");
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "agent_id": "a2", "tool_name": "Bash"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending.map(|p| p.id)), (SessionState::Blocked, Some(question.id)));
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "toolu_q3"})).await;
            assert_eq!(asking(&d), (SessionState::Working, None, None));
        })
        .await;
    }

    /// A sub-agent's calls while the main thread waits on a permission it
    /// asked don't end that wait; the sub-agent's own permission ends with
    /// its call.
    #[tokio::test]
    async fn a_sub_agent_ends_only_its_own_wait() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            d.update(s.id, SessionPatch { approvals: Some(Approvals::Terminal), ..Default::default() }).unwrap();
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "clean up"})).await;
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "rm -rf target"}})).await;
            post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "rm -rf target"}})).await;
            let main = d.view(s.id).unwrap().pending.unwrap();
            post(&d, s.id, json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Read", "tool_input": {"file_path": "a.rs"}})).await;
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "agent_id": "a1", "tool_name": "Read"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending.map(|p| p.id)), (SessionState::Blocked, Some(main.id)));
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "Bash"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Working, None));

            // The sub-agent's own ask ends with its own call.
            post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "agent_id": "a1", "tool_name": "Bash", "tool_input": {"command": "git push"}})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
            post(&d, s.id, json!({"hook_event_name": "PostToolUse", "agent_id": "a1", "tool_name": "Bash"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Working, None));

            // An MCP server's request for input waits until it is answered.
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "elicitation_dialog", "message": "An MCP server needs your input"})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
            post(&d, s.id, json!({"hook_event_name": "Notification", "notification_type": "elicitation_response", "message": "Elicitation response for server \"db\": accept"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.pending), (SessionState::Working, None));
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
    async fn stopping_a_session_that_outlives_a_hang_up_ends_it_all_the_same() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            // As a Claude Code that handles SIGHUP and SIGTERM and never
            // gets to act on them.
            let stubborn = dir.path().join("stubborn-cli");
            std::fs::write(
                &stubborn,
                format!(
                    "#!/bin/sh\ntrap '' HUP TERM\nsleep 300 &\necho $! > {}\nwhile :; do sleep 1; done\n",
                    dir.path().join("below").display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&stubborn, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut opts = options(dir.path());
            opts.programs = crate::testing::programs(stubborn.clone(), stubborn);
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
            // Its shell is up and has set its traps.
            tokio::time::sleep(Duration::from_millis(300)).await;
            d.stop(s.id, None).unwrap();
            for _ in 0..250 {
                if !d.view(s.id).unwrap().live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let v = d.view(s.id).unwrap();
            assert!(!v.live, "the session still runs after Stop");
            assert_eq!(v.state, SessionState::Stopped);
            // What it started, which ignored the hang-up too, went with it.
            let below: i32 = std::fs::read_to_string(dir.path().join("below"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let gone = (0..100).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                // SAFETY: kill with no signal only asks.
                (unsafe { libc::kill(below, 0) }) != 0
            });
            assert!(gone, "what the session started still runs");
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
                    share_with: vec!["dw".into(), "local".into()],
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!((room.slug.as_str(), room.members.len()), ("release", 2));
            assert!(d
                .create_room(NewRoom {
                    name: "release".into(),
                    members: vec![],
                    default_member: None,
                    ..Default::default()
                })
                .await
                .is_err());

            let post = |author: &str, hop: i16| NewMessage {
                author: author.into(),
                content: "on it".into(),
                hop,
                ..NewMessage::default()
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
                d.room_messages(room.id, crate::rooms::Page::newest(None, 50))
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
                    ..Default::default()
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
                        ..NewMessage::default()
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
                        delivery: Some(workforce_core::desk::DeliveryKind::Notice),
                        ..NewMessage::default()
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
                source: Some(crate::rooms::source_key("local", "scout", "m-1")),
                delivery: Some(workforce_core::desk::DeliveryKind::Reminder),
                ..NewMessage::default()
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

    /// What the registry keeps for `id`, once the persist queue has
    /// written what `ready` waits for.
    pub(super) async fn kept_until(
        store: &DeskStore,
        id: Uuid,
        ready: impl Fn(&SessionRecord) -> bool,
    ) -> SessionRecord {
        for _ in 0..200 {
            if let Some(r) = store.get(id).await.unwrap().filter(|r| ready(r)) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the registry never kept what was asked of session {id}");
    }

    #[tokio::test]
    async fn a_session_started_in_desk_is_kept_as_desks_through_the_registry() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            assert_eq!((s.origin, s.live), (Origin::Desk, true));
            // Every column the upsert writes, through the migrated schema.
            let kept = kept_until(&store, s.id, |_| true).await;
            assert_eq!(kept.origin, Origin::Desk);
            assert_eq!(
                kept.cli_session_id.as_deref(),
                Some(s.id.to_string().as_str())
            );
            // Found running by `claude agents`, it is still Desk's own.
            let found = adopt::parse(
                json!([found(&s.id.to_string(), &s.cwd, "auth-fix")])
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            d.reconcile_external(found).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.origin, v.live), (Origin::Desk, true));
            assert_eq!(d.list().len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn a_forgotten_session_is_not_taken_back() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            let cli = s.id.to_string();
            kept_until(&store, s.id, |_| true).await;
            d.forget(s.id).await.unwrap();
            for _ in 0..200 {
                if store.get(s.id).await.unwrap().is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(store.get(s.id).await.unwrap().is_none());
            assert_eq!(d.recall(Engine::Claude, &cli).await, None);
            assert!(d.record(s.id).is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn a_closed_session_running_again_elsewhere_keeps_its_record() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            // Closed in its terminal under an earlier hub: not loaded at start.
            let was = SessionRecord {
                origin: Origin::External,
                state: SessionState::Closed,
                cli_session_id: Some("ext-9".into()),
                display_name: Some("Ledger migration".into()),
                external_pid: None,
                terminal_id: None,
                ..record_fixture("vellev-36")
            };
            store.put(&was).await.unwrap();
            let d = desk(pool, dir.path()).await;
            assert!(d.list().is_empty());

            let found = adopt::parse(
                json!([found("ext-9", "/Users/k/dev/vellev", "vellev-36")])
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            d.reconcile_external(found).await;
            let listed = d.list();
            assert_eq!(listed.len(), 1, "one record per CLI session");
            let s = &listed[0];
            assert_eq!(s.id, was.id, "the same session, not a second record");
            assert_eq!(s.display_name.as_deref(), Some("Ledger migration"));
            assert_eq!(s.state, SessionState::Working);
            // And the registry takes the change: before, the second record's
            // insert broke the one-per-CLI-session index on every write.
            let kept = kept_until(&store, was.id, |r| r.state == SessionState::Working).await;
            assert_eq!(kept.origin, Origin::External);
            assert!(store
                .by_cli_session(Engine::Claude, "ext-9")
                .await
                .unwrap()
                .is_some_and(|r| r.id == was.id));
        })
        .await;
    }

    /// An external session at `cwd`, as `claude agents` lists it.
    pub(super) fn found(id: &str, cwd: &str, name: &str) -> serde_json::Value {
        json!({ "pid": std::process::id(), "cwd": cwd, "kind": "interactive",
                "sessionId": id, "name": name, "status": "busy" })
    }

    #[tokio::test]
    async fn the_hubs_own_cli_runs_are_never_adopted() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    hub_internal: HubInternal::new([PathBuf::from("/data")], None),
                    account_keys: None,
                    line_reach: crate::line::Reach::Group,
                    memory: Default::default(),
                    bosses: Default::default(),
                    ..options(dir.path())
                },
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let found = adopt::parse(
                json!([
                    found("hub-1", "/data/cli-completions/b369e00e-c276-4ae0-8394-6d83707380a0",
                        "b369e00e-c276-4ae0-8394-6d83707380a0-c2"),
                    found("hub-2", "/data/agents/scout/projects/_agent-turn/work/session/at-dm-d4774cbb6c89-2e",
                        "at-dm-d4774cbb6c89-2e"),
                    found("mine-1", "/Users/k/dev/app", "app-36"),
                ])
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            d.reconcile_external(found).await;
            let names: Vec<String> = d.list().into_iter().map(|s| s.name).collect();
            assert_eq!(names, ["app-36"], "the person's, not the hub's");
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_the_hub_started_itself_is_never_adopted_wherever_it_runs() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let mut child = tokio::process::Command::new("sleep")
                .arg("30")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    hub_internal: HubInternal::default().started_by(std::process::id()),
                    account_keys: None,
                    line_reach: crate::line::Reach::Group,
                    memory: Default::default(),
                    bosses: Default::default(),
                    ..options(dir.path())
                },
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let mut hubs = found("hub-1", "/elsewhere", "hub-run");
            hubs["pid"] = json!(child.id().unwrap());
            let found = adopt::parse(
                json!([hubs, found("mine-1", "/Users/k/dev/app", "app-36")])
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            d.reconcile_external(found).await;
            let names: Vec<String> = d.list().into_iter().map(|s| s.name).collect();
            assert_eq!(names, ["app-36"]);
            child.kill().await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_hubs_own_sessions_taken_before_are_forgotten_and_only_those() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let external = |name: &str, cwd: &str, state| SessionRecord {
                origin: Origin::External,
                state,
                cwd: cwd.into(),
                ..record_fixture(name)
            };
            let closed = external(
                "b369e00e-c276-4ae0-8394-6d83707380a0-c2",
                "/data/cli-completions/b369e00e-c276-4ae0-8394-6d83707380a0",
                SessionState::Closed,
            );
            let idle = external(
                "at-dm-d4774cbb6c89-2e",
                "/data/agents/scout/projects/_agent-turn/work/session/at-dm-d4774cbb6c89-2e",
                SessionState::Idle,
            );
            let mine = external("app-36", "/Users/k/dev/app", SessionState::Idle);
            let mine_closed = external("app-35", "/Users/k/dev/app", SessionState::Closed);
            let desks = SessionRecord {
                cwd: "/data/agents/scout".into(),
                state: SessionState::Idle,
                ..record_fixture("started in desk")
            };
            for r in [&closed, &idle, &mine, &mine_closed, &desks] {
                store.put(r).await.unwrap();
            }
            store
                .put_reads(
                    YOU,
                    &[closed.id, mine.id].map(|id| crate::reads::ReadPut {
                        place: crate::reads::ReadPlace::Session(id),
                        point: crate::reads::ReadPoint {
                            at: Utc::now(),
                            message: None,
                        },
                        mark: crate::reads::Mark::Advance,
                    }),
                )
                .await
                .unwrap();

            let logged = Arc::new(Logged::default());
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    hub_internal: HubInternal::new([PathBuf::from("/data")], None),
                    account_keys: None,
                    line_reach: crate::line::Reach::Group,
                    memory: Default::default(),
                    bosses: Default::default(),
                    ..options(dir.path())
                },
                Floor {
                    turns: Some(logged.clone()),
                    plans: None,
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

            for gone in [&closed, &idle] {
                assert!(store.get(gone.id).await.unwrap().is_none(), "{}", gone.name);
            }
            for kept in [&mine, &mine_closed, &desks] {
                assert!(store.get(kept.id).await.unwrap().is_some(), "{}", kept.name);
            }
            let mut listed: Vec<String> = d.list().into_iter().map(|s| s.name).collect();
            listed.sort();
            assert_eq!(listed, ["app-36", "started in desk"]);
            let read: Vec<_> = store
                .reads(YOU)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.place)
                .collect();
            assert_eq!(read, [crate::reads::ReadPlace::Session(mine.id)]);
            let forgot: Vec<SubjectKey> = logged
                .0
                .lock()
                .iter()
                .flat_map(|op| match op {
                    TurnOp::Forget { subjects } => subjects.clone(),
                    _ => vec![],
                })
                .collect();
            let mut expected = vec![SubjectKey::session(closed.id), SubjectKey::session(idle.id)];
            expected.sort();
            let mut forgot = forgot;
            forgot.sort();
            assert_eq!(forgot, expected, "their turns and gists go too");
        })
        .await;
    }

    #[tokio::test]
    async fn one_of_the_hubs_own_taken_while_the_hub_runs_is_forgotten_on_the_next_look() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let logged = Arc::new(Logged::default());
            let d = Desk::start(
                DeskStore::from_pool(pool),
                Options {
                    hub_internal: HubInternal::new([PathBuf::from("/data")], None),
                    account_keys: None,
                    line_reach: crate::line::Reach::Group,
                    memory: Default::default(),
                    bosses: Default::default(),
                    ..options(dir.path())
                },
                Floor {
                    turns: Some(logged.clone()),
                    plans: None,
                    pricer: None,
                    redactor: None,
                    index: None,
                    deriver: None,
                    accountant: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            // As one that slipped in: taken while the hub ran, by a rule
            // that didn't know the folder.
            let taken = SessionRecord {
                origin: Origin::External,
                state: SessionState::Working,
                cwd: "/data/cli-completions/u".into(),
                ..record_fixture("u-c2")
            };
            let id = taken.id;
            d.save(taken);
            let mine = SessionRecord {
                origin: Origin::External,
                state: SessionState::Idle,
                cwd: "/Users/k/dev/app".into(),
                ..record_fixture("app-36")
            };
            d.save(mine.clone());
            assert!(d.hub_internal(id));
            assert!(!d.hub_internal(mine.id));

            let found = adopt::parse(
                json!([found(
                    mine.cli_session_id.as_deref().unwrap(),
                    "/Users/k/dev/app",
                    "app-36"
                )])
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            d.reconcile_external(found).await;
            assert!(d.view(id).is_err());
            let names: Vec<String> = d.list().into_iter().map(|s| s.name).collect();
            assert_eq!(names, ["app-36"]);
            assert!(
                logs(&logged, |ops| ops.iter().any(|op| matches!(
                    op,
                    TurnOp::Forget { subjects } if subjects == &[SubjectKey::session(id)]
                )))
                .await
            );
        })
        .await;
    }

    /// A desk whose floor log is `logged`, its sessions run as `cli`.
    pub(super) async fn logged_desk(
        pool: workforce_postgres::PostgresPool,
        dir: &Path,
        cli: PathBuf,
    ) -> (Desk, Arc<Logged>) {
        DeskStore::migrate(&pool).await.unwrap();
        let logged = Arc::new(Logged::default());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            options(dir).with_program(Engine::Claude, cli),
            Floor {
                turns: Some(logged.clone()),
                plans: None,
                pricer: None,
                redactor: None,
                index: None,
                deriver: None,
                accountant: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        (d, logged)
    }

    /// How the last turn `ops` end ended, and the question it left.
    fn last_question(ops: &[TurnOp]) -> Option<(Outcome, Option<crew::ItemQuestion>)> {
        ops.iter().rev().find_map(|op| match op {
            TurnOp::End {
                outcome, question, ..
            } => Some((*outcome, question.clone())),
            _ => None,
        })
    }

    /// A crew whose model reads every reply as `asks`, after `wait`.
    struct Reader {
        asks: Option<bool>,
        wait: Duration,
    }

    #[async_trait::async_trait]
    impl crew::Crew for Reader {
        fn overseer(&self) -> Option<crew::Overseer> {
            None
        }
        fn agents(&self) -> Vec<crew::CrewAgent> {
            vec![]
        }
        fn midnight(&self) -> DateTime<Utc> {
            Utc::now()
        }
        async fn accept(&self, _: &crew::Suggestion) -> Result<(), String> {
            Ok(())
        }
        async fn asks(&self, _: &str, _: Option<&str>) -> Option<bool> {
            tokio::time::sleep(self.wait).await;
            self.asks
        }
    }

    #[tokio::test]
    async fn a_turn_stopped_on_a_refused_tool_asks_until_the_person_answers_and_can_be_allowed_on()
    {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
            let s = new_session(&d, dir.path()).await;
            let lookup = json!({"command": "security find-generic-password -s deploy -w"});
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "do the steps in TODO.md"})).await;
            assert_eq!(post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": lookup})).await, json!({}));
            // A sub-agent's refusal is its own.
            post(&d, s.id, json!({"hook_event_name": "PermissionDenied", "agent_id": "a1", "tool_name": "WebFetch",
                "tool_input": {"url": "https://example.com"}, "reason": "[Data Exfiltration]"})).await;
            post(&d, s.id, json!({"hook_event_name": "PermissionDenied", "tool_name": "Bash", "tool_input": lookup,
                "tool_use_id": "toolu_1", "reason": "[Credential Exploration]"})).await;
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "I couldn't read the password: the lookup was blocked."})).await;

            let v = d.view(s.id).unwrap();
            assert_eq!(v.state, SessionState::Asking, "not done: stuck on what it wasn't allowed");
            let asked = v.asked.clone().unwrap();
            assert_eq!(asked.ask, "Stopped: Bash was not allowed");
            assert_eq!(asked.why, "auto mode refused Bash ([Credential Exploration])");
            assert!(asked.denied.as_ref().unwrap().allowable);
            assert!(logs(&logged, |ops| last_question(ops).is_some()).await);
            assert_eq!(
                last_question(&logged.0.lock()),
                Some((Outcome::Asked, Some(crew::ItemQuestion { text: "Stopped: Bash was not allowed".into(), options: None })))
            );
            assert!(recorded(&d, 3).await.iter().any(|(k, text, _)| *k == crew::Kind::Question && text == "stopped: Bash was not allowed"));
            let item = crate::overview::session_item_of(&v);
            assert_eq!(item.state, crew::ItemState::NeedsYou);

            // Allowed and continued: the same call runs without asking from then on.
            d.allow_asked(s.id, Some(By::you())).unwrap();
            assert!(d.allow_asked(s.id, None).is_err(), "allowed once");
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "I allowed it, run it again"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.asked), (SessionState::Working, None), "the prompt answered it");
            assert_eq!(
                post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": lookup})).await,
                claude::allow_call()
            );
            assert_eq!(
                post(&d, s.id, json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": "security find-generic-password -s other -w"}})).await,
                json!({}),
                "only the call allowed"
            );
            assert!(d.allow_asked(s.id, None).is_err(), "nothing asked now");
        })
        .await;
    }

    #[tokio::test]
    async fn a_refusal_only_the_transcript_holds_is_read_from_it() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let s = new_session(&d, dir.path()).await;
            let transcript = dir.path().join("t.jsonl");
            let lines = [
                json!({"type": "user", "message": {"role": "user", "content": "clean up the build"}}),
                json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "rm -rf \"$OUT\"/*"}}]}}),
                json!({"type": "user", "toolDenialKind": "permission-rule", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                    "content": "Permission for this command was denied by a built-in Claude Code safety check, not by the user."}]}}),
            ];
            std::fs::write(&transcript, lines.map(|l| l.to_string()).join("\n")).unwrap();
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "clean up the build"})).await;
            post(&d, s.id, json!({"hook_event_name": "Stop", "transcript_path": transcript, "last_assistant_message": "I left the removal to you."})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!(v.state, SessionState::Asking);
            let denied = v.asked.unwrap().denied.unwrap();
            assert!(!denied.allowable, "only the person can get past a safety check");
            assert!(d.allow_asked(s.id, None).is_err());
            d.dismiss_asked(s.id, Some(By::you())).unwrap();
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.asked), (SessionState::Done, None));
        })
        .await;
    }

    #[tokio::test]
    async fn a_reply_that_asks_is_asking_and_a_rhetorical_one_is_done() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
            let s = new_session(&d, dir.path()).await;
            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix the flaky test"})).await;
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message":
                "There are two fixes. Which do you prefer?\n\n1. Raise the timeout\n2. Mock the clock"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!(v.state, SessionState::Asking);
            assert_eq!(v.asked.unwrap().why, "asked you to choose between 2 options");
            assert!(logs(&logged, |ops| matches!(last_question(ops), Some((Outcome::Asked, Some(_))))).await);

            post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "mock the clock"})).await;
            assert_eq!(d.view(s.id).unwrap().state, SessionState::Working, "answered");
            post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message":
                "Mocked the clock, and all 214 pass. Not bad for a one-line change, right?"})).await;
            let v = d.view(s.id).unwrap();
            assert_eq!((v.state, v.asked), (SessionState::Done, None));
            assert!(logs(&logged, |ops| last_question(ops) == Some((Outcome::Finished, None))).await);
        })
        .await;
    }

    #[tokio::test]
    async fn asking_beats_background_and_its_work_shows_again_once_answered() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, _) = logged_desk(pool, dir.path(), shell_cli(dir.path())).await;
            let s = new_session(&d, dir.path()).await;
            // Its tool shell runs before its turn ends.
            let cli = d.0.state.lock().live[&s.id].pty.pid().unwrap();
            let shell = dir.path().join(format!("shell-{cli}"));
            for _ in 0..200 {
                if std::fs::read_to_string(&shell).is_ok_and(|p| !p.trim().is_empty()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            post(
                &d,
                s.id,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": "start the server"}),
            )
            .await;
            post(
                &d,
                s.id,
                json!({"hook_event_name": "Stop",
                "last_assistant_message": "The server is starting. Should I seed the database too?",
                "background_tasks": [{"id": "b1", "type": "shell", "status": "running"}]}),
            )
            .await;
            let v = d.view(s.id).unwrap();
            assert_eq!(
                (v.state, v.waiting_on()),
                (SessionState::Asking, 0),
                "it needs the person"
            );
            let item = crate::overview::session_item_of(&v);
            assert_eq!(item.state, crew::ItemState::NeedsYou);
            assert_eq!(
                item.question.unwrap().text,
                "Should I seed the database too?"
            );
            assert_ne!(item.detail.as_deref(), Some("Waiting on 1 background task"));

            d.dismiss_asked(s.id, None).unwrap();
            assert_eq!(
                d.view(s.id).unwrap().waiting_on(),
                1,
                "its work, still running, shows again"
            );
        })
        .await;
    }

    /// Ends a turn on `reply`, after a prompt.
    async fn turn_ending(d: &Desk, id: Uuid, prompt: &str, reply: &str) {
        post(
            d,
            id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt}),
        )
        .await;
        post(
            d,
            id,
            json!({"hook_event_name": "Stop", "last_assistant_message": reply}),
        )
        .await;
    }

    /// Waits for `id` to be in `state`, up to ten seconds.
    async fn comes_to(d: &Desk, id: Uuid, state: SessionState) -> bool {
        for _ in 0..1000 {
            if d.view(id).unwrap().state == state {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    const UNSURE: &str = "Pushed the branch. Let me know if you want me to open the PR.";

    #[tokio::test]
    async fn a_slow_yes_from_the_model_moves_a_done_turn_to_asking() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
            let s = new_session(&d, dir.path()).await;
            turn_ending(&d, s.id, "push it", UNSURE).await;
            assert_eq!(
                d.view(s.id).unwrap().state,
                SessionState::Done,
                "no model: done"
            );
            assert!(
                logs(&logged, |ops| last_question(ops)
                    == Some((Outcome::Finished, None)))
                .await
            );

            let model = Duration::from_secs(3);
            let yes: Arc<dyn crew::Crew> = Arc::new(Reader {
                asks: Some(true),
                wait: model,
            });
            d.read_replies_with(&yes);
            let at = std::time::Instant::now();
            turn_ending(&d, s.id, "push it again", UNSURE).await;
            assert!(at.elapsed() < model, "the hook doesn't wait for the model");
            let v = d.view(s.id).unwrap();
            assert_eq!(
                (v.state, v.asked),
                (SessionState::Done, None),
                "done until the model says"
            );
            assert!(comes_to(&d, s.id, SessionState::Asking).await);
            assert_eq!(
                d.view(s.id).unwrap().asked.unwrap().why,
                "asked you something"
            );
            assert!(
                logs(&logged, |ops| matches!(
                    last_question(ops),
                    Some((Outcome::Asked, Some(_)))
                ))
                .await,
                "its turn ends asking, as a turn the rules read so"
            );
            let ends = logged
                .0
                .lock()
                .iter()
                .filter(|op| matches!(op, TurnOp::End { .. }))
                .count();
            assert_eq!(ends, 2, "the turn the model read ended once");
            assert!(
                // Started, prompted, finished, prompted, asked.
                recorded(&d, 5)
                    .await
                    .iter()
                    .any(|(k, text, _)| *k == crew::Kind::Question && text.starts_with("asked")),
                "asked, so the overseer hears of it"
            );
            drop(yes);
        })
        .await;
    }

    #[tokio::test]
    async fn a_yes_after_the_person_spoke_changes_nothing_and_a_timeout_leaves_it_done() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
            let s = new_session(&d, dir.path()).await;
            let yes: Arc<dyn crew::Crew> = Arc::new(Reader {
                asks: Some(true),
                wait: Duration::from_millis(300),
            });
            d.read_replies_with(&yes);
            turn_ending(&d, s.id, "push it", UNSURE).await;
            post(
                &d,
                s.id,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": "open the PR"}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(600)).await;
            let v = d.view(s.id).unwrap();
            assert_eq!(
                (v.state, v.asked),
                (SessionState::Working, None),
                "the person answered first"
            );
            assert!(
                logs(&logged, |ops| last_question(ops)
                    == Some((Outcome::Finished, None)))
                .await,
                "the turn they spoke after finished"
            );

            *d.0.read_wait.lock() = Duration::from_millis(100);
            post(
                &d,
                s.id,
                json!({"hook_event_name": "Stop", "last_assistant_message": UNSURE}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(
                d.view(s.id).unwrap().state,
                SessionState::Done,
                "timed out: done, never a false need"
            );
            let ends = logged
                .0
                .lock()
                .iter()
                .filter(|op| matches!(op, TurnOp::End { .. }))
                .count();
            assert_eq!(ends, 2, "its turn ended as finished once the model gave up");

            let unsaid: Arc<dyn crew::Crew> = Arc::new(Reader {
                asks: None,
                wait: Duration::ZERO,
            });
            d.read_replies_with(&unsaid);
            turn_ending(&d, s.id, "again", UNSURE).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                d.view(s.id).unwrap().state,
                SessionState::Done,
                "the model couldn't say: done"
            );
            drop((yes, unsaid));
        })
        .await;
    }

    #[tokio::test]
    async fn interrupting_ends_the_turn_with_esc_and_does_nothing_while_idle() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            // A CLI that prints, in hex, the first byte it is sent.
            let cli = dir.path().join("esc-cli");
            std::fs::write(
                &cli,
                "#!/bin/sh\nstty raw -echo\nhead -c 1 | od -An -tx1\nexec sleep 60\n",
            )
            .unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (d, logged) = logged_desk(pool, dir.path(), cli).await;
            let s = new_session(&d, dir.path()).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                d.interrupt(s.id, None).unwrap().state,
                SessionState::Starting,
                "not working"
            );

            post(
                &d,
                s.id,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": "run the suite"}),
            )
            .await;
            let v = d.interrupt(s.id, Some(By::you())).unwrap();
            crate::pty::tests::wait_for(&d.attach(s.id).unwrap(), "1b").await;
            assert_eq!((v.state, v.live), (SessionState::Done, true));
            assert!(
                logs(&logged, |ops| last_question(ops)
                    == Some((Outcome::Stopped, None)))
                .await,
                "the turn ended, not finished"
            );
            // Started, prompted, interrupted.
            assert!(recorded(&d, 3).await.iter().any(
                |(k, text, _)| *k == crew::Kind::Interrupted && text == "interrupted auth-fix"
            ));
        })
        .await;
    }
}

#[cfg(test)]
mod codex_stop_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn pid_in(file: &Path) -> i32 {
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// A stand-in `codex app-server` that is busy with a turn: it ignores
    /// the polite signals, as does the worker it keeps in a process group
    /// of its own (Codex's `codex-code-mode-host`). It writes its session
    /// file and both pids where the test reads them.
    fn busy_server(dir: &Path) -> PathBuf {
        let path = dir.join("busy-codex");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntrap '' HUP TERM\n\
                 echo turn > '{d}/rollout.jsonl'\n\
                 set -m\nsleep 300 &\necho $! > '{d}/worker.pid'\n\
                 echo $$ > '{d}/server.pid'\n\
                 : > \"${{3#unix://}}\"\n\
                 while :; do sleep 1; done\n",
                d = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn held_server(dir: &Path, program: &Path) -> (CodexLink, i32, tempfile::TempDir) {
        let (hdir, socket, _cancel) = crate::holder::tests::holder().await;
        let holder = Holder::connect(&socket, crate::holder::tests::TOKEN)
            .await
            .unwrap();
        let sock = dir.join("run/s.sock");
        codex::prepare_socket(&sock).await.unwrap();
        let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let spec = codex::app_server(program, &sock, &env, None);
        let (pty, _) = holder
            .spawn(
                &spec,
                80,
                24,
                0,
                serde_json::json!({"kind": "codex-app-server"}),
                None,
            )
            .await
            .unwrap();
        codex::wait_for_socket(&sock).await.unwrap();
        let pid = pty.pid().unwrap() as i32;
        let link = CodexLink {
            server: Server::Held(Arc::new(pty)),
            socket: sock,
            watch: CancellationToken::new(),
            replies: mpsc::unbounded_channel().0,
            steers: mpsc::unbounded_channel().0,
            line_steers: mpsc::unbounded_channel().0,
        };
        (link, pid, hdir)
    }

    #[tokio::test]
    async fn closing_a_session_leaves_no_app_server_or_worker_behind() {
        let dir = tempfile::tempdir().unwrap();
        let program = busy_server(dir.path());
        let (link, server, _hdir) = held_server(dir.path(), &program).await;
        let worker = pid_in(&dir.path().join("worker.pid"));
        assert!(alive(server) && alive(worker));

        link.stop();

        let deadline = std::time::Instant::now() + codex::STOP_GRACE + Duration::from_secs(8);
        while (alive(server) || alive(worker)) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let left = (alive(server), alive(worker));
        // Whatever is left must not outlive the test.
        for pid in [server, worker] {
            // SAFETY: the pids are the stand-in's own.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert_eq!(left, (false, false), "(server, worker) still running");
        assert!(
            dir.path().join("rollout.jsonl").exists(),
            "the session file is Codex's, and stays"
        );
    }
}

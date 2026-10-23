//! Claude Code chat dispatcher — a Slack thread becomes a persistent `claude -p` conversation.
//!
//! Each chat turn spawns a fresh `claude -p` subprocess that resumes the session's stored
//! Claude UUID, so the full prior transcript is in Claude's context. Progress events from
//! the subprocess are rendered into a single "turn message" in the Slack thread, edited
//! in place as tool calls and responses arrive. A Stop button on the turn message cancels
//! the in-flight subprocess.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use eyre::{eyre, Result};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use workforce_agent::runner::{scope_turn, ProgressEvent, SessionContext};
use workforce_agent::tools::approval::{EndsOnDrop, TurnApprovals};

use crate::approvals::slack::{Click, Clicker};
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::session::{Session, SessionStore, TurnPrincipal};
use workforce_core::policy::PermissionChoice;
use workforce_projects::{Project, ProjectRepo, RepoSource};

use crate::agent_registry::AgentRegistry;
use crate::claude_code::{self, transcript_lost, ClaudeCodeResult, CliHealth, Resume, RoundKey};
use crate::cli_session::{workspace_lost, RotationPolicy, RotationReason};
use crate::code::utils::{list_available_repos, repos_named};
use crate::code::worktree::WorktreeManager;
use crate::coding_engine::{
    CodingEngine, CodingRound, Continuity, MemoryLoad, ModelChoices, RoundContext,
};
use crate::coding_sandbox::{
    ensure_with_wait, CodingSandbox, HomeSpec, SandboxLauncher, WorkspaceSpec,
};
use crate::github_local::GithubCloneTool;
use crate::projects::workspace_scope;
use crate::slack_blocks::{
    branch_staleness_blocks, chat_error_blocks, cli_missing_blocks, cloning_blocks,
    engine_unavailable_blocks, permission_prompt_blocks, ready_blocks, rebase_result_blocks,
    render_turn, session_lost_blocks, session_rotated_blocks, thread_spend_threshold_blocks,
    usage_blocks, welcome_blocks, welcome_project_blocks, workspace_reset_blocks, ChatChoice,
    ChatNames, CrashDiagnostic, TurnState, TurnStats, TurnStatus,
};
use crate::vault::Vault;
use workforce_agent::tools::Tool;
use workforce_agent::turn_journal::{ToolCall, OUTCOME_UNKNOWN};

const CHAT_TURN_TIMEOUT_SECS: u64 = 30 * 60;
const CHAT_MAX_TURNS: u32 = 50;
/// How often a turn waiting on another replica's claim on its thread asks again.
const CLAIM_RETRY: Duration = Duration::from_secs(5);
const PSEUDO_AGENT_ID: &str = "claude-chat";
const EDIT_DEBOUNCE_MS: u64 = 500;

/// Cadence at which the renderer re-edits the turn card while the
/// subprocess is still running, even if no progress events have
/// arrived. Without this, a long-running tool call (multi-minute cargo
/// test, slow `git clone --local`) leaves the card frozen at the last
/// rendered state, and the user sees no signal of liveness — the
/// retrospective traced two "How is it going?" nudges to exactly that.
///
/// Floor at 30s: Slack `chat.update` is rate-tier 3 (~50/min/channel),
/// safe at this cadence; tighter buys nothing because the renderer is
/// already debounced to coalesce burst events.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Cadence at which we re-probe `claude --version` after boot. A missed
/// install at deploy time gets picked up the next tick instead of staying
/// broken until process restart. Long enough that the probe cost
/// (subprocess spawn) is invisible in steady state.
const CLI_HEALTH_REPROBE_INTERVAL: Duration = Duration::from_secs(300);

/// Concurrency for the synchronous auto-refresh fired before
/// `materialise_repos` when bases are stale. Mirrors the existing
/// per-project `update` tool default; configurable knobs are not worth
/// adding for a path that only runs once per hour.
const AUTO_REFRESH_CONCURRENCY: usize = 4;

/// Argument that prints the usage blurb. A bare `/cc` opens a bench instead,
/// so help needs a word of its own; a repo of this name is shadowed by it.
const HELP_KEYWORD: &str = "help";

/// The project a bare `/cc <owner>/<name>` thread materialises as: one repo,
/// so both thread shapes go through the launcher the same way.
fn repo_project(agent_id: &str, repo: &str) -> Result<Project> {
    let (owner, name) = repo
        .split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
        .ok_or_else(|| eyre!("repo binding {repo:?} is not owner/name"))?;
    let mut project = Project::new(agent_id, repo_project_name(owner, name));
    project.repos.push(ProjectRepo {
        alias: name.to_string(),
        source: RepoSource::Github {
            owner: owner.to_string(),
            repo: name.to_string(),
        },
        default_branch: None,
        setup: None,
    });
    Ok(project)
}

/// A project name for a repo thread that is safe as a directory and a dataset
/// name wherever the workspace lands.
fn repo_project_name(owner: &str, name: &str) -> String {
    format!("repo-{owner}-{name}")
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') => c,
            _ => '-',
        })
        .collect()
}

/// The keyed home a thread's rounds share on a worker, so its transcript and
/// bench outlive any one scope.
fn home_key(thread_key: &str) -> String {
    format!("cc-{}", WorktreeManager::scope_id_for_session(thread_key))
}

/// Snapshot of the persisted claude-chat state relevant to dispatch routing.
/// Existence of the value implies the session is in claude-chat mode.
#[derive(Debug, Clone)]
pub struct ClaudeChatContext {
    /// Agent pinned when the thread was opened, if any. `None` for threads
    /// that predate agent pinning — callers resolve via Slack routing instead.
    pub pinned_agent_id: Option<String>,
}

/// What a /cc thread is bound to. A thread is set to one shape at /cc time
/// and stays there: legacy single-repo, or multi-repo project.
enum ChatBinding {
    Repo(String),
    Project(String),
}

impl ChatBinding {
    fn from_session(session: &workforce_common::session::Session) -> Result<Self> {
        match (session.claude_chat_repo(), session.claude_chat_project()) {
            (Some(r), _) => Ok(Self::Repo(r.to_string())),
            (None, Some(p)) => Ok(Self::Project(p.to_string())),
            (None, None) => Err(eyre!("claude-chat session has no bound repo or project")),
        }
    }

    /// Free-form label rendered into the turn card.
    fn label(&self) -> String {
        match self {
            Self::Repo(r) => r.clone(),
            Self::Project(p) => format!("project:{p}"),
        }
    }
}

/// Input for one chat turn in a Slack thread.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub workspace_id: String,
    pub session_key: String,
    pub channel: String,
    pub thread_ts: Option<String>,
    pub message_ts: Option<String>,
    pub user: String,
    pub text: String,
    /// Agent this turn is routed to. `None` means "fall back to the default
    /// agent" — `AgentRegistry::resolve(None)` handles that.
    pub agent_id: Option<String>,
    /// Canonical workforce identity UUID for the Slack user, when an
    /// identity store is configured. The turn's hub tools run as it, so
    /// per-user vault paths resolve for them.
    pub identity_id: Option<String>,
}

impl ChatTurn {
    /// The key of the thread this turn's replies and cards are posted in.
    fn thread_key(&self) -> String {
        let thread = self.thread_ts.as_deref().or(self.message_ts.as_deref());
        crate::slack_manager::thread_session_key(
            &self.workspace_id,
            &self.channel,
            thread.unwrap_or_default(),
        )
    }

    /// Session owner for this turn: the canonical identity when resolved, else
    /// the raw Slack id. Matches the agent loop so a thread maps to one session.
    fn owner(&self) -> &str {
        self.identity_id.as_deref().unwrap_or(&self.user)
    }
}

/// Read-only resolution of a `/cc` thread's session: the canonical `owner`
/// first, then a not-yet-adopted legacy raw-Slack-id binding. Creates nothing.
fn find_thread_session(
    sessions: &dyn SessionStore,
    session_key: &str,
    owner: &str,
    legacy_user: &str,
) -> Option<Session> {
    sessions
        .find(session_key, owner)
        .ok()
        .flatten()
        .or_else(|| {
            (owner != legacy_user)
                .then(|| sessions.find(session_key, legacy_user).ok().flatten())
                .flatten()
        })
}

/// Resolve a `/cc` thread's session under the canonical `owner`, adopting a
/// legacy raw-Slack-id binding the first time one is touched. Adoption keeps the
/// session id (and its `claude_session_uuid` resume handle), re-keying only
/// `owner` and `label` so per-user `sessions.search` finds it and no
/// divergently-owned twin survives. Creates a fresh session only when neither
/// owner has one.
fn adopt_thread_session(
    sessions: &dyn SessionStore,
    session_key: &str,
    owner: &str,
    legacy_user: &str,
) -> Result<Session> {
    if let Some(session) = sessions.find(session_key, owner)? {
        return Ok(session);
    }
    if owner != legacy_user {
        if let Some(mut session) = sessions.find(session_key, legacy_user)? {
            session.owner = owner.to_string();
            session.label = Some(format!("{session_key}:{owner}"));
            sessions.save(&session)?;
            return Ok(session);
        }
    }
    sessions
        .get_or_create(session_key, owner, PSEUDO_AGENT_ID)
        .map_err(|e| eyre!("session lookup failed: {e}"))
}

/// What every attempt to seat a thread's workspace during one turn shares.
#[derive(Clone, Copy)]
struct Seating<'a> {
    agent_id: &'a str,
    project: &'a Project,
    tokens: &'a HashMap<String, String>,
    turn: &'a ChatTurn,
    deadline: Instant,
    cancel: &'a CancellationToken,
    state: &'a Arc<Mutex<TurnState>>,
    surface: &'a Arc<dyn ChannelSurface>,
    turn_message_id: &'a str,
}

struct ActiveTurn {
    cancel: CancellationToken,
    channel: String,
    turn_message_id: String,
    /// The key of the thread the turn's cards are posted in, as a click on
    /// one names it.
    thread_key: String,
    /// The Slack user whose message started the turn.
    requester: String,
    state: Arc<Mutex<TurnState>>,
    prompts: Arc<TurnPrompts>,
    /// The turn is over once nothing holds it: a call its round still holds
    /// settles unanswered and never runs on a late click.
    _ends: EndsOnDrop,
}

/// The approval prompts of one `/cc` turn: the ledger its held MCP calls wait
/// on, and which of its prompts a click in the thread answered.
#[derive(Default)]
struct TurnPrompts {
    ledger: Arc<TurnApprovals>,
    answered: parking_lot::Mutex<std::collections::HashSet<String>>,
}

impl TurnPrompts {
    /// Deliver a click to the prompt `request_id` names. Marked answered
    /// before the call it releases can settle, so the card is never taken
    /// for one nobody answered.
    fn answer(&self, request_id: &str, choice: PermissionChoice) -> bool {
        self.answered.lock().insert(request_id.to_string());
        let answered = self.ledger.answer(request_id, choice);
        if !answered {
            self.answered.lock().remove(request_id);
        }
        answered
    }

    fn was_answered(&self, request_id: &str) -> bool {
        self.answered.lock().contains(request_id)
    }
}

/// What names a `/cc` plan among a thread's journalled calls.
const PLAN_TOOL: &str = "cc";

/// A `/cc` round about to run on a worker that keeps its jobs: all a replica
/// needs to collect it once the one running it has stopped.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CcPlan {
    #[serde(flatten)]
    key: RoundKey,
    uuid: Uuid,
    resumed: bool,
    workspace_id: String,
    channel: String,
    thread: Option<String>,
    card: String,
    user: String,
    identity: Option<String>,
    agent_id: String,
    text: String,
    model: Option<String>,
    effort: Option<String>,
}

/// Whether a turn takes its thread's lock and turn claim, or runs under a
/// turn that holds them.
#[derive(Clone, Copy, PartialEq)]
enum Claim {
    Take,
    Held,
}

/// A round an interrupted turn left running, collected from the journal.
struct Collected {
    plan: CcPlan,
    call: String,
    checkpoint: Option<String>,
}

/// Ends a `/cc` turn's record and clears its journal however the turn ends,
/// except while the hub drains: then both stay for the successor that
/// collects the round.
struct TurnEnding {
    sessions: Arc<dyn SessionStore>,
    session_id: String,
    token: String,
    journal: Arc<crate::turn_journal::SessionJournal>,
    /// The turn reached the user, answered or stopped, so nothing is left
    /// for a successor even while the hub drains.
    settled: bool,
}

impl Drop for TurnEnding {
    fn drop(&mut self) {
        if !self.settled && crate::shutdown::in_progress() {
            return;
        }
        self.journal.clear();
        crate::serve::end_turn_record(&self.sessions, &self.session_id, &self.token);
    }
}

/// Which coding chat a thread belongs to: `/cc` on the Claude chain or `/cx`
/// on the Codex chain. Recorded on the thread's session, so every later turn
/// runs on the engine that opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Cc,
    Cx,
}

impl ChatKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Cx => "cx",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Cc => "/cc",
            Self::Cx => "/cx",
        }
    }

    /// The word after `cmd:` or the slash that names this chat, with its
    /// rebase twin.
    pub fn from_word(word: &str) -> Option<(Self, bool)> {
        match word {
            "cc" => Some((Self::Cc, false)),
            "cc-rebase" => Some((Self::Cc, true)),
            "cx" => Some((Self::Cx, false)),
            "cx-rebase" => Some((Self::Cx, true)),
            _ => None,
        }
    }

    fn from_session(session: &Session) -> Self {
        match session.claude_chat_engine() {
            Some("cx") => Self::Cx,
            _ => Self::Cc,
        }
    }

    /// How this chat names itself on its cards.
    pub fn names(self) -> ChatNames {
        ChatNames {
            cli: match self {
                Self::Cc => "Claude Code",
                Self::Cx => "Codex",
            },
            command: self.command(),
        }
    }

    /// The npm package that installs this chat's CLI, for the card shown when
    /// it is missing from the image.
    fn package(self) -> &'static str {
        match self {
            Self::Cc => "@anthropic-ai/claude-code",
            Self::Cx => "@openai/codex",
        }
    }
}

/// The effort a `/cx` turn runs at when neither the thread nor `[codex_chat]`
/// names one: a chat turn rarely needs the seconds a higher effort spends.
pub const DEFAULT_CX_EFFORT: &str = "medium";

/// One engine chain a chat runs turns on, with the defaults its config gives
/// it and the health of the CLI it spawns on this host.
pub struct ChatEngine {
    engine: Arc<dyn CodingEngine>,
    model: Option<String>,
    effort: Option<String>,
    /// `None` once probed for an engine that spawns no CLI. Refreshed on a
    /// slow timer by `spawn_cli_health_probe`; consulted at dispatch time.
    /// Starts "available" so a caller that never wires the probe keeps the
    /// prior dispatch behaviour.
    cli_health: Arc<RwLock<Option<CliHealth>>>,
}

impl ChatEngine {
    fn new(engine: Arc<dyn CodingEngine>, model: Option<String>, effort: Option<String>) -> Self {
        Self {
            engine,
            model,
            effort,
            cli_health: Arc::new(RwLock::new(Some(CliHealth::Available {
                version: "unprobed".into(),
            }))),
        }
    }

    /// The `/cx` chat on `engine`: an effort its config leaves unset is
    /// [`DEFAULT_CX_EFFORT`].
    fn codex(engine: Arc<dyn CodingEngine>, model: Option<String>, effort: Option<String>) -> Self {
        Self::new(
            engine,
            model,
            effort.or_else(|| Some(DEFAULT_CX_EFFORT.to_string())),
        )
    }

    fn choices(&self) -> ModelChoices {
        self.engine.model_choices()
    }

    /// What a thread runs on: its own pinned choice where it made one, this
    /// chat's configured default otherwise.
    fn effective(&self, pinned: &ChatChoice) -> ChatChoice {
        ChatChoice {
            model: pinned.model.clone().or_else(|| self.model.clone()),
            effort: pinned.effort.clone().or_else(|| self.effort.clone()),
        }
    }
}

/// `model:<name>` and `effort:<level>` words taken out of a command's
/// arguments, with what remains. They may stand anywhere before the task
/// text begins: around the leading `@agent` and the binding word, but not
/// inside a task, where `model:foo` is the user's own words. An empty
/// value is an error the caller reports with the usage.
fn split_chat_options(text: &str) -> Result<(ChatChoice, String), String> {
    let mut choice = ChatChoice::default();
    let mut kept: Vec<&str> = Vec::new();
    let mut positional = 0usize;
    for word in text.split_whitespace() {
        let in_task = positional >= 2;
        match (in_task, option_word(word)?) {
            (false, Some(("model", value))) => choice.model = Some(value.to_ascii_lowercase()),
            (false, Some(("effort", value))) => choice.effort = Some(value.to_ascii_lowercase()),
            _ => {
                let leading_agent = kept.is_empty() && word.starts_with('@');
                if !in_task && !leading_agent {
                    positional += 1;
                }
                kept.push(word);
            }
        }
    }
    Ok((choice, kept.join(" ")))
}

/// `model:<value>` / `effort:<value>` as a key and value; any other word is
/// `None`; a bare `model:` or `effort:` is an error naming the word.
fn option_word(word: &str) -> Result<Option<(&'static str, &str)>, String> {
    let Some((key, value)) = word.split_once(':') else {
        return Ok(None);
    };
    let key = match key.to_ascii_lowercase().as_str() {
        "model" => "model",
        "effort" => "effort",
        _ => return Ok(None),
    };
    if value.is_empty() {
        return Err(format!(
            "`{key}:` needs a value, e.g. `{key}:{}`.",
            if key == "model" { "sol" } else { "medium" }
        ));
    }
    Ok(Some((key, value)))
}

/// Check a thread's choice against what its engine offers: a model is one of
/// the engine's aliases or a full model id (one with a dot or a dash in it,
/// which the engines pass through); an effort is one of the engine's levels.
fn validate_chat_choice(choice: &ChatChoice, offered: &ModelChoices) -> Result<(), String> {
    if let Some(model) = choice
        .model
        .as_deref()
        .filter(|m| !offered.accepts_model(m))
    {
        return Err(format!(
            "`model:{model}` is not a choice here: use one of {}{}.",
            list(&offered.models),
            match offered.raw_id_prefixes {
                [] => String::new(),
                prefixes => format!(", or a full model id starting {}", list(prefixes)),
            }
        ));
    }
    if let Some(effort) = choice
        .effort
        .as_deref()
        .filter(|e| !offered.accepts_effort(e))
    {
        return Err(format!(
            "`effort:{effort}` is not a level here: use one of {}.",
            list(offered.efforts)
        ));
    }
    Ok(())
}

fn list<S: AsRef<str>>(choices: &[S]) -> String {
    choices
        .iter()
        .map(|c| format!("`{}`", c.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Shared service wiring for the claude-chat dispatcher.
/// Built once at hub startup, held by the Slack dispatch loop.
pub struct ClaudeChatService {
    sessions: Arc<dyn SessionStore>,
    /// Where a thread's workspace is materialised, on this host or a worker.
    launcher: Arc<dyn SandboxLauncher>,
    /// What runs a `/cc` turn, with its fallback chain.
    cc: ChatEngine,
    /// What runs a `/cx` turn; `None` on a hub that offers no Codex chat.
    cx: Option<ChatEngine>,
    /// Whether the launcher runs rounds on a worker, where this host's CLI
    /// probe says nothing about them.
    remote: bool,
    /// Where `/cc <repo>` resolves bare names and clones sources for the
    /// hub's own code tools.
    agent_workspace: PathBuf,
    /// Hub data root — base for the per-agent project layout used by
    /// project-bound /cc threads (`{data_dir}/agents/{agent_id}/projects/...`).
    data_dir: PathBuf,
    /// Per-agent project manifest store; used by project-bound /cc threads
    /// to look up the repo list before materialising the per-session copy.
    project_store: Arc<dyn workforce_projects::ProjectStore>,
    /// Registry for resolving the agent a `/cc` invocation is scoped to,
    /// so we can read secrets from that agent's vault first.
    agent_registry: Arc<AgentRegistry>,
    /// Shared fallback vault, checked when the agent vault is missing the key.
    global_vault: Option<Arc<Vault>>,
    turn_locks: DashMap<String, Arc<Mutex<()>>>,
    active_turns: DashMap<String, Arc<ActiveTurn>>,
    default_allowed_tools: Vec<String>,
    /// Session-rotation thresholds (`[claude_chat]`).
    rotation: RotationPolicy,
    /// Latest probe of whether the `claude` CLI is installed and runnable.
    /// How long an idle `/cc` workspace survives the sweeper
    /// (`[projects].cc_session_ttl_secs`). Only ever shown to the user, on the
    /// welcome card of a scratch thread — there the workspace files are the
    /// deliverable, so the retention window has to be stated rather than
    /// silently applied.
    scratch_retention: Duration,
    /// Persistent per-thread task brief. Captured at `/cc` welcome time
    /// and surfaced on session rotation / `session_lost` recovery so the
    /// user never has to re-state context — see the retrospective at
    /// `~/.claude/plans/what-happened-in-this-goofy-willow.md` for why
    /// this matters.
    brief_store: crate::projects::cc_brief::BriefStore,
    /// Persistent per-thread spend accumulator. Surfaces the running
    /// total in each turn's stats block and posts a one-shot soft
    /// warning when the thread crosses each
    /// `THRESHOLD_INCREMENT_USD` band — the retrospective traced a
    /// $10+ runaway thread to spend being invisible until the user
    /// happened to add it up by hand.
    cost_ledger: crate::projects::cc_cost::CostLedger,
}

impl ClaudeChatService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sessions: Arc<dyn SessionStore>,
        launcher: Arc<dyn SandboxLauncher>,
        engine: Arc<dyn CodingEngine>,
        remote: bool,
        agent_workspace: PathBuf,
        data_dir: PathBuf,
        project_store: Arc<dyn workforce_projects::ProjectStore>,
        agent_registry: Arc<AgentRegistry>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            sessions,
            launcher,
            cc: ChatEngine::new(engine, None, None),
            cx: None,
            remote,
            agent_workspace,
            brief_store: crate::projects::cc_brief::BriefStore::new(data_dir.clone()),
            cost_ledger: crate::projects::cc_cost::CostLedger::new(data_dir.clone()),
            data_dir,
            project_store,
            agent_registry,
            global_vault,
            turn_locks: DashMap::new(),
            active_turns: DashMap::new(),
            default_allowed_tools: vec![
                "Read".into(),
                "Edit".into(),
                "Write".into(),
                "Bash".into(),
                "Grep".into(),
                "Glob".into(),
            ],
            rotation: RotationPolicy::default(),
            scratch_retention: Duration::from_secs(
                workforce_common::settings::ProjectsConfig::default().cc_session_ttl_secs,
            ),
        }
    }

    /// How long an idle workspace survives the cc-session sweeper, so a
    /// scratch thread's welcome card can state the install's real window.
    pub fn with_scratch_retention(mut self, retention: Duration) -> Self {
        self.scratch_retention = retention;
        self
    }

    /// Override the session-rotation thresholds.
    pub fn with_rotation_policy(mut self, rotation: RotationPolicy) -> Self {
        self.rotation = rotation;
        self
    }

    /// Claude Code model/effort for every `/cc` turn. `None` keeps the
    /// subscription / CC defaults; values are validated by the CC CLI.
    pub fn with_model_defaults(mut self, model: Option<String>, effort: Option<String>) -> Self {
        self.cc.model = model;
        self.cc.effort = effort;
        self
    }

    /// Offer `/cx` on `engine`, the Codex chain, with its own defaults. An
    /// effort left unset runs at [`DEFAULT_CX_EFFORT`].
    pub fn with_codex_engine(
        mut self,
        engine: Arc<dyn CodingEngine>,
        model: Option<String>,
        effort: Option<String>,
    ) -> Self {
        self.cx = Some(ChatEngine::codex(engine, model, effort));
        self
    }

    /// The engine a thread of `kind` runs on, or why it cannot.
    fn chat(&self, kind: ChatKind) -> Result<&ChatEngine> {
        match kind {
            ChatKind::Cc => Ok(&self.cc),
            ChatKind::Cx => self
                .cx
                .as_ref()
                .ok_or_else(|| eyre!("/cx is not configured on this hub: no Codex engine chain")),
        }
    }

    fn engines(&self) -> impl Iterator<Item = &ChatEngine> {
        std::iter::once(&self.cc).chain(self.cx.as_ref())
    }

    /// Probe the Claude Code CLI now and spawn a background task that
    /// re-probes every [`CLI_HEALTH_REPROBE_INTERVAL`]. Should be called
    /// once at hub startup; idempotent if called more than once but each
    /// call leaks a probe task, so don't.
    pub async fn spawn_cli_health_probe(&self) {
        for chat in self.engines() {
            let engine = Arc::clone(&chat.engine);
            let initial = engine.cli_health().await;
            match &initial {
                Some(CliHealth::Available { version }) => {
                    info!(target: "cc", engine = engine.label(), %version, "CLI probe: available");
                }
                Some(CliHealth::Missing { reason }) => {
                    warn!(
                        target: "cc",
                        engine = engine.label(),
                        reason = %reason,
                        "CLI probe: missing — dispatch will return the cli_missing card until the next successful probe"
                    );
                }
                None => {}
            }
            *chat.cli_health.write().await = initial;

            let cli_health = Arc::clone(&chat.cli_health);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(CLI_HEALTH_REPROBE_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The first tick is immediate; skip it (we already probed above).
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let next = engine.cli_health().await;
                    let prior = cli_health
                        .read()
                        .await
                        .as_ref()
                        .is_none_or(CliHealth::is_available);
                    let now_available = next.as_ref().is_none_or(CliHealth::is_available);
                    if prior != now_available {
                        match &next {
                            Some(CliHealth::Available { version }) => {
                                info!(target: "cc", engine = engine.label(), %version, "CLI re-appeared")
                            }
                            Some(CliHealth::Missing { reason }) => {
                                warn!(target: "cc", engine = engine.label(), reason = %reason, "CLI disappeared")
                            }
                            None => {}
                        }
                    }
                    *cli_health.write().await = next;
                }
            });
        }
    }

    /// Spawn a background task that re-pulls every project's base
    /// clones every `interval`. Belt-and-braces with the on-/cc
    /// auto-refresh from PR1: this keeps bases warm overnight so the
    /// first /cc of the day doesn't pay the refresh wallclock at the
    /// user's expense. Failures degrade to warn-and-continue.
    ///
    /// Safe to call once at hub startup; calling more than once leaks
    /// task handles, so don't.
    pub fn spawn_periodic_project_refresh(self: &Arc<Self>, interval: Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so startup doesn't race
            // other init work or pile a refresh on top of a fresh boot.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = svc.refresh_all_projects_once().await {
                    warn!(
                        target: "cc",
                        error = %e,
                        "cc periodic project refresh failed; will retry next tick"
                    );
                }
            }
        });
    }

    /// Periodically drop idle per-session turn locks. `turn_locks` gains one
    /// `Arc<Mutex<()>>` entry per distinct session key (channel × thread ×
    /// user) the first time that session dispatches, and the entry was never
    /// removed — so over a long-lived process the map grows with traffic. An
    /// entry is only safe to drop when nothing currently holds or is waiting on
    /// it, i.e. when the map's own `Arc` is the sole strong reference
    /// (`strong_count == 1`); an in-flight or queued turn keeps a second clone
    /// alive (see the lock acquire in `dispatch`). `DashMap::retain` holds each
    /// shard's lock for the sweep, so it can't race a concurrent
    /// `entry(..).or_insert_with(..)` that would resurrect a key mid-drop.
    ///
    /// Safe to call once at hub startup; calling more than once leaks task
    /// handles, so don't.
    pub fn spawn_turn_lock_gc(self: &Arc<Self>, interval: Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick; nothing has accumulated at boot.
            tick.tick().await;
            loop {
                tick.tick().await;
                let before = svc.turn_locks.len();
                svc.turn_locks.retain(|_, lock| Arc::strong_count(lock) > 1);
                let removed = before.saturating_sub(svc.turn_locks.len());
                if removed > 0 {
                    debug!(
                        target: "cc",
                        removed,
                        remaining = svc.turn_locks.len(),
                        "cc turn-lock GC swept idle session locks"
                    );
                }
            }
        });
    }

    /// Single pass over every agent's projects, refreshing each via
    /// `ProjectUpdater`. Public so a future ops endpoint or test can
    /// trigger it on demand. Per-project failures don't abort the
    /// pass — they're warned and the next project still runs.
    pub async fn refresh_all_projects_once(&self) -> Result<()> {
        let agent_ids: Vec<String> = self
            .agent_registry
            .agent_ids()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let mut total_refreshed = 0usize;
        for agent_id in agent_ids {
            let projects = match self.project_store.list(&agent_id) {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        target: "cc",
                        agent = %agent_id,
                        error = %e,
                        "cc periodic refresh: project_store.list failed"
                    );
                    continue;
                }
            };
            if projects.is_empty() {
                continue;
            }
            for project in projects {
                match self.update_project_now(&agent_id, &project).await {
                    Ok(()) => total_refreshed += 1,
                    Err(e) => warn!(
                        target: "cc",
                        agent = %agent_id,
                        project = %project.name,
                        error = %e,
                        "cc periodic refresh: project update failed"
                    ),
                }
            }
        }
        info!(
            target: "cc",
            refreshed = total_refreshed,
            "cc periodic refresh pass complete"
        );
        Ok(())
    }

    /// Handle a `/cc-rebase` slash command. Looks up the cc-session
    /// behind `session_key`, locates its on-disk worktree, and runs
    /// fetch+rebase across every repo. Posts a single Slack card with
    /// the per-repo outcome — conflicts list the offending files so
    /// the user can ask Claude to resolve them in the next turn.
    ///
    /// `session_key` must point at a thread that's already in
    /// claude-chat mode; otherwise we surface a friendly error rather
    /// than silently doing nothing. Caller has already resolved
    /// (`workspace_id`, `channel`, `thread_ts`) into the same shape
    /// the regular dispatcher uses, so we receive the worktree
    /// coordinates ready-made.
    pub async fn handle_rebase_command(
        self: Arc<Self>,
        session_key: String,
        owner: String,
        legacy_user: String,
        channel: String,
        thread_ts: Option<String>,
        surface: Arc<dyn ChannelSurface>,
    ) -> Result<()> {
        let session = self.thread_session(&session_key, &owner, &legacy_user)?;
        if !session.is_claude_chat() {
            let (sum, blocks) = chat_error_blocks(
                "`/cc-rebase` only works inside an existing `/cc` thread — start one with `/cc <repo>` first.",
            );
            if let Err(e) = surface
                .send_message(&outbound(&channel, thread_ts, sum, blocks))
                .await
            {
                warn!(target: "cc", session = %session_key, error = %e, "cc /cc-rebase scope-error notice failed");
            }
            return Ok(());
        }

        let claude_uuid = match session.claude_session_uuid() {
            Some(u) => u.to_string(),
            None => {
                let (sum, blocks) = chat_error_blocks(
                    "`/cc-rebase` needs an active session — type any /cc turn first to materialise the workspace.",
                );
                let _ = surface
                    .send_message(&outbound(&channel, thread_ts, sum, blocks))
                    .await;
                return Ok(());
            }
        };

        let binding = ChatBinding::from_session(&session)?;
        let agent_id = session
            .claude_chat_agent_id()
            .map(String::from)
            .ok_or_else(|| eyre!("/cc thread has no agent_id to scope its workspace to"))?;
        let project = self.project_for(&binding, &agent_id)?;
        let agent_vault = self.agent_vault(Some(&agent_id));
        let tokens = github_tokens_for(
            &project,
            agent_vault.as_deref(),
            self.global_vault.as_deref(),
        );
        let scope = workspace_scope(!project.repos.is_empty(), &session_key, &claude_uuid);
        let seated = self
            .launcher
            .ensure(&WorkspaceSpec {
                agent_id: &agent_id,
                project: &project,
                scope: &scope,
                tokens: &tokens,
                home: self.home_for(&session_key),
                primary: None,
            })
            .await;
        let sandbox = match seated {
            Ok(sandbox) => sandbox,
            Err(e) => {
                let (sum, blocks) = chat_error_blocks(&format!(
                    "`/cc-rebase` could not reach this thread's workspace: {e}"
                ));
                if let Err(e) = surface
                    .send_message(&outbound(&channel, thread_ts, sum, blocks))
                    .await
                {
                    warn!(target: "cc", session = %session_key, error = %e, "cc /cc-rebase workspace-error notice failed");
                }
                return Ok(());
            }
        };
        let gh_token = round_github_token(
            &project,
            &tokens,
            agent_vault.as_deref(),
            self.global_vault.as_deref(),
        );
        let reports =
            crate::projects::cc_rebase::rebase_in(sandbox.as_ref(), gh_token.as_deref()).await;
        let (sum, blocks) =
            rebase_result_blocks(ChatKind::from_session(&session).names(), &reports);
        if let Err(e) = surface
            .send_message(&outbound(&channel, thread_ts, sum, blocks))
            .await
        {
            warn!(target: "cc", session = %session_key, error = %e, "cc /cc-rebase result card send failed");
        }
        Ok(())
    }

    /// The project a thread's binding stands for: the stored one, or the
    /// single-repo project a bare repo binding implies.
    fn project_for(&self, binding: &ChatBinding, agent_id: &str) -> Result<Project> {
        match binding {
            ChatBinding::Repo(repo) => repo_project(agent_id, repo),
            ChatBinding::Project(name) => self
                .project_store
                .get(agent_id, name)?
                .ok_or_else(|| eyre!("project '{name}' not found for agent '{agent_id}'")),
        }
    }

    /// The home a thread's rounds keep for as long as the bench card
    /// promises the files.
    fn home_for(&self, thread_key: &str) -> HomeSpec {
        HomeSpec::Keyed {
            key: home_key(thread_key),
            ttl_secs: self.scratch_retention.as_secs(),
        }
    }

    /// Force-refresh every repo in `project` from its GitHub upstream.
    /// Shared by the dispatch-time stale check, the periodic background
    /// pass, and `/cc-rebase` (which always wants fresh refs before it
    /// rebases). Per-repo failures are warned and the call still returns
    /// `Ok(())` — falling back to the stale base for one repo beats
    /// blocking the whole project's refresh.
    async fn update_project_now(
        &self,
        agent_id: &str,
        project: &workforce_projects::Project,
    ) -> Result<()> {
        let agent_vault = self.agent_vault(Some(agent_id));
        let mut tokens: HashMap<String, String> = HashMap::new();
        for repo in &project.repos {
            let RepoSource::Github { owner, .. } = &repo.source;
            if tokens.contains_key(owner) {
                continue;
            }
            if let Some(t) = crate::user_config::resolve_github_pat(
                agent_vault.as_deref(),
                self.global_vault.as_deref(),
                owner,
            ) {
                tokens.insert(owner.clone(), t);
            }
        }

        let updater = crate::projects::updater::ProjectUpdater::new(
            tokens,
            self.data_dir.clone(),
            AUTO_REFRESH_CONCURRENCY,
        );
        let reports = updater.update(project).await?;
        let failed: Vec<&str> = reports
            .iter()
            .filter_map(|r| match &r.result {
                crate::projects::updater::RepoUpdateResult::Err { .. } => Some(r.alias.as_str()),
                _ => None,
            })
            .collect();
        if !failed.is_empty() {
            warn!(
                target: "cc",
                project = %project.name,
                failed = ?failed,
                "cc project refresh: some repos failed; continuing with their stale state"
            );
        }
        Ok(())
    }

    /// Warn about meaningful branch divergence in any checkout of the
    /// workspace. Best-effort and local git only: an analyser failure is
    /// logged and the turn runs regardless.
    async fn warn_branch_staleness(
        &self,
        project: &Project,
        sandbox: &dyn CodingSandbox,
        surface: &Arc<dyn ChannelSurface>,
        channel: &str,
        anchor: Option<&str>,
    ) {
        for repo in &project.repos {
            let repo_dir = if project.repos.len() == 1 {
                PathBuf::from(".")
            } else {
                PathBuf::from(&repo.alias)
            };
            let report = match crate::projects::staleness::analyse_in(sandbox, &repo_dir).await {
                Ok(r) => r,
                Err(e) => {
                    debug!(
                        target: "cc",
                        repo = %repo.alias,
                        error = %e,
                        "cc staleness analyser skipped this repo"
                    );
                    continue;
                }
            };
            if !report.warrants_warning() {
                continue;
            }
            let (sum, blocks) = branch_staleness_blocks(&repo.alias, &report);
            if let Err(e) = surface
                .send_message(&outbound(channel, anchor.map(str::to_string), sum, blocks))
                .await
            {
                warn!(
                    target: "cc",
                    repo = %repo.alias,
                    error = %e,
                    "cc staleness card send failed"
                );
            }
        }
    }

    /// Resolve the per-agent vault for this `/cc` invocation. `None` falls back
    /// to the registry's default agent (matching regular-message behavior).
    fn agent_vault(&self, agent_id: Option<&str>) -> Option<Arc<Vault>> {
        self.agent_registry
            .resolve(agent_id)
            .and_then(|svc| svc.vault.clone())
    }

    /// Pick the agent for this `/cc` invocation given an optional explicit
    /// `@<agent>` from the slash-command text and the Slack routing hint.
    ///
    /// Precedence: explicit `@<agent>` > routing hint > single-agent shortcut.
    /// With 2+ agents configured and no signal, the user is asked to pick one
    /// rather than silently defaulting — otherwise an unbound channel would
    /// run `/cc` against the wrong agent's vault and look identical to the
    /// "PAT not configured" bug we just fixed.
    ///
    /// Returns `Err(user_visible_message)` when the caller must post the
    /// message to Slack and abort (unknown agent / ambiguous).
    fn pick_agent(
        &self,
        explicit: Option<&str>,
        hint: Option<&str>,
    ) -> std::result::Result<String, String> {
        let agent_ids: Vec<String> = self
            .agent_registry
            .agent_ids()
            .into_iter()
            .map(|a| a.to_string())
            .collect();
        pick_agent_core(explicit, hint, &agent_ids, self.agent_registry.default_id())
    }

    /// Resolve a `/cc` thread's session under the canonical `owner`, adopting a
    /// legacy raw-Slack-id binding the first time one is touched. Adoption keeps
    /// the session id (and its `claude_session_uuid` resume handle), re-keying
    /// only `owner` and `label` so per-user `sessions.search` finds it and no
    /// divergently-owned twin survives. The owner matches the agent loop, so a
    /// thread maps to exactly one session regardless of route.
    fn thread_session(&self, session_key: &str, owner: &str, legacy_user: &str) -> Result<Session> {
        adopt_thread_session(self.sessions.as_ref(), session_key, owner, legacy_user)
    }

    /// Read-only claude-chat mode probe. Returns `Some` only when the thread's
    /// session — under the canonical `owner`, or a not-yet-adopted legacy
    /// `legacy_user` binding — is in claude-chat mode; `pinned_agent_id`
    /// carries the `/cc @<agent>` pin when present (absent on pre-pinning
    /// threads — callers fall back to Slack routing). Creates nothing: a thread
    /// with no session is not a claude-chat thread, since mode is only ever set
    /// by `/cc`.
    pub fn claude_chat_context(
        &self,
        session_key: &str,
        owner: &str,
        legacy_user: &str,
    ) -> Option<ClaudeChatContext> {
        let session = find_thread_session(self.sessions.as_ref(), session_key, owner, legacy_user)?;
        session.is_claude_chat().then(|| ClaudeChatContext {
            pinned_agent_id: session.claude_chat_agent_id().map(String::from),
        })
    }

    /// Cancel the in-flight turn whose turn-message is `(channel, message_ts)`. This is
    /// the targeting mode used by the Stop button and the reaction fallback — it won't
    /// accidentally cancel a newer turn that has since taken over the same session.
    pub async fn try_cancel_by_message(
        &self,
        channel: &str,
        message_ts: &str,
        surface: Arc<dyn ChannelSurface>,
    ) -> bool {
        let session_key = self.active_turns.iter().find_map(|entry| {
            let h = entry.value();
            (h.channel == channel && h.turn_message_id == message_ts).then(|| entry.key().clone())
        });
        match session_key {
            Some(key) => self.cancel_turn(&key, surface).await,
            None => false,
        }
    }

    /// Answer the approval prompt `request_id` names, which the in-flight
    /// turn of the thread `thread_key` names raised, when `clicker` may.
    /// Matched on the thread the card is in rather than the turn's session
    /// key, whose shape depends on how the thread was started.
    pub fn answer_approval(
        &self,
        thread_key: &str,
        request_id: &str,
        choice: PermissionChoice,
        clicker: &Clicker<'_>,
    ) -> Click {
        let turn = self
            .active_turns
            .iter()
            .find(|entry| entry.value().thread_key == thread_key)
            .map(|entry| Arc::clone(entry.value()));
        let Some(turn) = turn else {
            return Click::Unheld;
        };
        if let Err(refusal) = clicker.may_answer(&turn.requester, choice) {
            return Click::Refused(refusal);
        }
        match turn.prompts.answer(request_id, choice) {
            true => Click::Delivered,
            false => Click::Unheld,
        }
    }

    /// Cancel the in-flight turn for a session by its key. Marks the card as `Stopped`
    /// immediately for responsive UX, then trips the subprocess's cancellation token.
    pub async fn cancel_turn(&self, session_key: &str, surface: Arc<dyn ChannelSurface>) -> bool {
        info!(target: "cc", session_key = %session_key, "cc cancel_turn invoked");
        let Some(entry) = self.active_turns.get(session_key) else {
            return false;
        };
        let handle = Arc::clone(entry.value());
        drop(entry);
        handle.cancel.cancel();
        handle.state.lock().await.status = TurnStatus::Stopped;
        if let Err(e) = edit_card(
            &surface,
            &handle.channel,
            &handle.turn_message_id,
            &handle.state,
        )
        .await
        {
            warn!(
                target: "cc",
                session_key = %session_key,
                error = %e,
                "cc cancel_turn card edit failed; turn is still cancelled hub-side"
            );
        }
        true
    }

    /// Bind a Slack thread to claude-chat mode on a given repo and pin the
    /// resolved agent so subsequent thread turns route to the same vault.
    pub fn mark_thread_claude_chat(
        &self,
        session_key: &str,
        owner: &str,
        repo: &str,
        started_by: &str,
        agent_id: Option<&str>,
        kind: ChatKind,
        choice: &ChatChoice,
    ) -> Result<()> {
        let mut session = self
            .sessions
            .get_or_create(session_key, owner, PSEUDO_AGENT_ID)
            .map_err(|e| eyre!("session lookup failed: {e}"))?;
        session.mark_claude_chat(repo, started_by, agent_id);
        session.set_claude_chat_engine(kind.key());
        session.set_claude_chat_choice(choice.model.as_deref(), choice.effort.as_deref());
        self.sessions
            .save(&session)
            .map_err(|e| eyre!("session save failed: {e}"))
    }

    /// Bind a Slack thread to claude-chat mode on a multi-repo project.
    /// Mutually exclusive with `mark_thread_claude_chat` — a thread is
    /// either single-repo or project-bound at /cc time and stays that way.
    pub fn mark_thread_claude_chat_project(
        &self,
        session_key: &str,
        owner: &str,
        project: &str,
        started_by: &str,
        agent_id: Option<&str>,
        kind: ChatKind,
        choice: &ChatChoice,
    ) -> Result<()> {
        let mut session = self
            .sessions
            .get_or_create(session_key, owner, PSEUDO_AGENT_ID)
            .map_err(|e| eyre!("session lookup failed: {e}"))?;
        session.mark_claude_chat_project(project, started_by, agent_id);
        session.set_claude_chat_engine(kind.key());
        session.set_claude_chat_choice(choice.model.as_deref(), choice.effort.as_deref());
        self.sessions
            .save(&session)
            .map_err(|e| eyre!("session save failed: {e}"))
    }

    /// Start the thread's transcript over under `fresh`, as when the one it
    /// resumed is gone or its workspace was reclaimed.
    fn reset_claude_session_uuid(
        &self,
        session_key: &str,
        owner: &str,
        legacy_user: &str,
        fresh: Uuid,
    ) -> Result<Uuid> {
        let mut session = self.thread_session(session_key, owner, legacy_user)?;
        session.clear_claude_session_uuid();
        session.set_claude_uuid(&fresh.to_string());
        self.sessions
            .save(&session)
            .map_err(|e| eyre!("session save failed during recovery: {e}"))?;
        Ok(fresh)
    }

    /// Handle the `/cc` slash command: resolve the repo (auto-cloning if the
    /// user supplied `owner/name` for a source we don't have yet), bind the
    /// thread to claude-chat mode, and dispatch the first turn if a task was
    /// supplied.
    ///
    /// `agent_id` is the routing output from Slack's `(workspace, channel,
    /// user) → agent_id` chain; `None` means "no explicit binding, use the
    /// registry's default agent." Agent scoping determines which vault is
    /// checked first for secrets like the GitHub PAT.
    pub async fn handle_slash_command(
        self: Arc<Self>,
        workspace_id: String,
        channel: String,
        user: String,
        text: String,
        agent_id: Option<String>,
        identity_id: Option<String>,
        surface: Arc<dyn ChannelSurface>,
        kind: ChatKind,
    ) -> Result<()> {
        let opened_at = Instant::now();
        let chat = match self.chat(kind) {
            Ok(chat) => chat,
            Err(e) => {
                let (sum, blocks) = chat_error_blocks(&e.to_string());
                surface
                    .send_message(&outbound(&channel, None, sum, blocks))
                    .await?;
                return Ok(());
            }
        };
        let offered = chat.choices();
        let parsed = split_chat_options(text.trim()).and_then(|(pinned, arguments)| {
            validate_chat_choice(&pinned, &offered).map(|()| (pinned, arguments))
        });
        let (pinned, arguments) = match parsed {
            Ok(parsed) => parsed,
            Err(reason) => {
                let (summary, mut blocks) =
                    usage_blocks(kind.names(), &offered.models, offered.efforts);
                let (_, error) = chat_error_blocks(&reason);
                blocks.splice(0..0, error);
                if let Err(e) = surface
                    .send_message(&outbound(
                        &channel,
                        None,
                        format!("{reason} {summary}"),
                        blocks,
                    ))
                    .await
                {
                    warn!(target: "cc", channel = %channel, error = %e, "cc invalid model/effort notice send failed");
                }
                return Ok(());
            }
        };
        let choice = chat.effective(&pinned);
        let trimmed = arguments.as_str();

        // Parse optional leading `@<agent>` prefix. Agent ids aren't Slack
        // usernames so the `@` isn't auto-mentioned — passes through literally.
        let (explicit_agent, remainder) = match trimmed.strip_prefix('@') {
            Some(rest) => {
                let (candidate, rest) = rest
                    .split_once(char::is_whitespace)
                    .map(|(c, r)| (c, r.trim_start()))
                    .unwrap_or((rest, ""));
                if candidate.is_empty() {
                    (None, trimmed)
                } else {
                    (Some(candidate.to_string()), rest)
                }
            }
            None => (None, trimmed),
        };

        // Help is answered before agent resolution: asking how to use `/cc`
        // must work even in a channel where the agent is ambiguous.
        if remainder.trim().eq_ignore_ascii_case(HELP_KEYWORD) {
            let (summary, blocks) = usage_blocks(kind.names(), &offered.models, offered.efforts);
            if let Err(e) = surface
                .send_message(&outbound(&channel, None, summary, blocks))
                .await
            {
                warn!(target: "cc", channel = %channel, error = %e, "cc /cc usage send failed; user will not see help text");
            }
            return Ok(());
        }

        let resolved_agent_id = match self
            .pick_agent(explicit_agent.as_deref(), agent_id.as_deref())
        {
            Ok(id) => {
                info!(target: "cc", kind = kind.key(), channel = %channel, phase = "agent-picked", elapsed_ms = opened_at.elapsed().as_millis() as u64, "cc open");
                id
            }
            Err(msg) => {
                let (summary, blocks) = chat_error_blocks(&msg);
                if let Err(e) = surface
                    .send_message(&outbound(&channel, None, summary, blocks))
                    .await
                {
                    warn!(target: "cc", channel = %channel, error = %e, "cc /cc agent-pick error send failed; user will not see why agent resolution failed");
                }
                return Ok(());
            }
        };

        let (binding_arg, initial_task) = match remainder.split_once(char::is_whitespace) {
            Some((r, t)) => (r.to_string(), t.trim().to_string()),
            None => (remainder.to_string(), String::new()),
        };
        let task_opt = (!initial_task.is_empty()).then_some(initial_task.as_str());

        // No binding argument — `/cc` or `/cc @agent`, with or without a task —
        // opens the agent's bench rather than printing help, so starting a
        // coding session needs nothing set up first. Help is `/cc help`.
        let bind_outcome = if binding_arg.is_empty() {
            let bench = match crate::projects::ensure_bench_project(
                self.project_store.as_ref(),
                &resolved_agent_id,
            ) {
                Ok(p) => {
                    info!(target: "cc", kind = kind.key(), channel = %channel, phase = "bench-ready", elapsed_ms = opened_at.elapsed().as_millis() as u64, "cc open");
                    p
                }
                Err(e) => {
                    let msg = format!("Couldn't open your bench workspace: {e}");
                    let (summary, blocks) = chat_error_blocks(&msg);
                    if let Err(send_err) = surface
                        .send_message(&outbound(&channel, None, summary, blocks))
                        .await
                    {
                        warn!(target: "cc", channel = %channel, error = %send_err, "cc bench-open error send failed; user will not see why /cc was rejected");
                    }
                    return Ok(());
                }
            };
            self.open_project_thread(
                kind,
                &choice,
                &channel,
                &bench.name,
                &resolved_agent_id,
                identity_id.as_deref(),
                task_opt,
                surface.as_ref(),
            )
            .await?
        } else if let Some(project_name) = binding_arg.strip_prefix("project:") {
            self.open_project_thread(
                kind,
                &choice,
                &channel,
                project_name.trim(),
                &resolved_agent_id,
                identity_id.as_deref(),
                task_opt,
                surface.as_ref(),
            )
            .await?
        } else {
            self.open_repo_thread(
                kind,
                &choice,
                &channel,
                &binding_arg,
                &resolved_agent_id,
                task_opt,
                surface.as_ref(),
            )
            .await?
        };
        let (binding, thread_ts) = match bind_outcome {
            Some(pair) => pair,
            None => return Ok(()),
        };
        info!(target: "cc", kind = kind.key(), channel = %channel, thread_ts = %thread_ts, phase = "thread-opened", elapsed_ms = opened_at.elapsed().as_millis() as u64, "cc open");

        // Post a placeholder reply in-thread so Slack renders the "1 reply"
        // indicator on the parent — without it the thread is invisible until
        // the user types. Skip when there's an initial_task: the dispatched
        // turn will post a real reply in the same thread shortly anyway.
        if initial_task.is_empty() {
            let (summary, blocks) = ready_blocks();
            if let Err(e) = surface
                .send_message(&outbound(
                    &channel,
                    Some(thread_ts.clone()),
                    summary,
                    blocks,
                ))
                .await
            {
                warn!(target: "cc", channel = %channel, thread_ts = %thread_ts, error = %e, "cc 'ready' placeholder send failed; thread may not be visible until first turn");
            }
        }

        let session_key = format!("slack:{workspace_id}:{channel}:{thread_ts}");
        let owner = identity_id.as_deref().unwrap_or(user.as_str());
        match &binding {
            ChatBinding::Repo(repo) => {
                self.mark_thread_claude_chat(
                    &session_key,
                    owner,
                    repo,
                    &user,
                    Some(&resolved_agent_id),
                    kind,
                    &pinned,
                )?;
            }
            ChatBinding::Project(project_name) => {
                self.mark_thread_claude_chat_project(
                    &session_key,
                    owner,
                    project_name,
                    &user,
                    Some(&resolved_agent_id),
                    kind,
                    &pinned,
                )?;
            }
        }
        info!(target: "cc", kind = kind.key(), session = %session_key, phase = "session-marked", elapsed_ms = opened_at.elapsed().as_millis() as u64, "cc open");

        if !initial_task.is_empty() {
            // Persist the user's initial ask. Recovery paths
            // (`session_lost`, rotation) read this back so the user
            // never has to re-state their goal after a fresh UUID.
            // Failures degrade silently — a missing brief just means
            // we fall through to the original "context lost" wording,
            // which is the prior behaviour.
            if let Err(e) = self.brief_store.save(&session_key, &initial_task).await {
                warn!(
                    target: "cc",
                    session = %session_key,
                    error = %e,
                    "cc brief save failed; future rotations / recoveries will have no recap"
                );
            }
            let turn = ChatTurn {
                workspace_id,
                session_key,
                channel,
                thread_ts: Some(thread_ts.clone()),
                message_ts: Some(thread_ts),
                user,
                text: initial_task,
                agent_id: Some(resolved_agent_id),
                identity_id,
            };
            Arc::clone(&self).dispatch_turn(turn, surface).await?;
        }
        Ok(())
    }

    /// Resolve a `/cc <owner/name>` argument into a thread binding. Posts the
    /// welcome / cloning / error card to Slack as a side-effect; returns
    /// `None` when an error response was sent and the caller should bail.
    async fn open_repo_thread(
        &self,
        kind: ChatKind,
        choice: &ChatChoice,
        channel: &str,
        repo_arg: &str,
        resolved_agent_id: &str,
        initial_task: Option<&str>,
        surface: &dyn ChannelSurface,
    ) -> Result<Option<(ChatBinding, String)>> {
        match resolve_chat_repo(repo_arg, &self.agent_workspace) {
            ResolveOutcome::Ok(repo) => {
                let (summary, blocks) = welcome_blocks(kind.names(), choice, &repo, initial_task);
                let sent = surface
                    .send_message(&outbound(channel, None, summary, blocks))
                    .await
                    .map_err(|e| eyre!("failed to post /cc thread root: {e}"))?;
                Ok(Some((ChatBinding::Repo(repo), sent.message_id)))
            }
            ResolveOutcome::NotCloned { owner, repo: name } => {
                let canonical = format!("{owner}/{name}");
                let (summary, blocks) = cloning_blocks(&canonical);
                let sent = surface
                    .send_message(&outbound(channel, None, summary, blocks))
                    .await
                    .map_err(|e| eyre!("failed to post /cc cloning card: {e}"))?;
                let thread_ts = sent.message_id;
                match self
                    .run_auto_clone(Some(resolved_agent_id), &owner, &name)
                    .await
                {
                    Ok(()) => {
                        let (summary, blocks) =
                            welcome_blocks(kind.names(), choice, &canonical, initial_task);
                        if let Err(e) = surface
                            .update_message(
                                channel,
                                &thread_ts,
                                &outbound(channel, None, summary, blocks),
                            )
                            .await
                        {
                            warn!(target: "cc", channel = %channel, thread_ts = %thread_ts, error = %e, "cc post-clone welcome edit failed; thread root may still show 'cloning…'");
                        }
                        Ok(Some((ChatBinding::Repo(canonical), thread_ts)))
                    }
                    Err(msg) => {
                        let (summary, blocks) = chat_error_blocks(&msg);
                        if let Err(e) = surface
                            .update_message(
                                channel,
                                &thread_ts,
                                &outbound(channel, None, summary, blocks),
                            )
                            .await
                        {
                            warn!(target: "cc", channel = %channel, thread_ts = %thread_ts, error = %e, "cc clone-failure edit failed; thread root may still show 'cloning…' instead of error");
                        }
                        Ok(None)
                    }
                }
            }
            ResolveOutcome::Unrecoverable(msg) => {
                let (summary, blocks) = chat_error_blocks(&msg);
                if let Err(e) = surface
                    .send_message(&outbound(channel, None, summary, blocks))
                    .await
                {
                    warn!(target: "cc", channel = %channel, error = %e, "cc /cc unresolvable-repo error send failed; user will not see why /cc was rejected");
                }
                Ok(None)
            }
        }
    }

    /// Resolve a `/cc project:<name>` argument. The project must already
    /// exist for the agent (managed via `projects.create` / `add_repo`).
    async fn open_project_thread(
        &self,
        kind: ChatKind,
        choice: &ChatChoice,
        channel: &str,
        project_name: &str,
        resolved_agent_id: &str,
        identity_id: Option<&str>,
        initial_task: Option<&str>,
        surface: &dyn ChannelSurface,
    ) -> Result<Option<(ChatBinding, String)>> {
        let project = match self.project_store.get(resolved_agent_id, project_name)? {
            Some(p) => p,
            None => {
                let msg = format!(
                    "Project `{project_name}` does not exist for agent `{resolved_agent_id}`. Create it with `projects.create` first."
                );
                let (summary, blocks) = chat_error_blocks(&msg);
                if let Err(e) = surface
                    .send_message(&outbound(channel, None, summary, blocks))
                    .await
                {
                    warn!(target: "cc", channel = %channel, project = %project_name, error = %e, "cc unknown-project error send failed; user will not see why /cc project was rejected");
                }
                return Ok(None);
            }
        };
        // The card promises only what the round can actually do: a bench with
        // no resolvable token can neither clone nor push, and the user should
        // learn that here rather than a round later.
        let agent_vault = self.agent_vault(Some(resolved_agent_id));
        let has_token = || {
            claude_code::effective_gh_token(
                None,
                agent_vault.as_deref(),
                self.global_vault.as_deref(),
            )
            .is_some()
        };
        let github = !project.repos.is_empty()
            || match identity_id {
                Some(uid) => {
                    workforce_agent::runner::CURRENT_USER_ID.sync_scope(uid.to_string(), has_token)
                }
                None => has_token(),
            };
        let (summary, blocks) = welcome_project_blocks(
            kind.names(),
            choice,
            &project,
            initial_task,
            self.scratch_retention,
            github,
        );
        let sent = surface
            .send_message(&outbound(channel, None, summary, blocks))
            .await
            .map_err(|e| eyre!("failed to post /cc thread root: {e}"))?;
        Ok(Some((ChatBinding::Project(project.name), sent.message_id)))
    }

    /// Invoke `github.clone` for the given owner/repo. Returns `Ok(())` on
    /// success or when the source is already present (idempotent race);
    /// otherwise returns a user-facing Slack string.
    async fn run_auto_clone(
        &self,
        agent_id: Option<&str>,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<(), String> {
        let fail = |detail: &str| clone_failure_message(owner, repo, detail);

        let Some(tool) = GithubCloneTool::with_fallback(
            self.agent_vault(agent_id),
            self.global_vault.clone(),
            self.agent_workspace.clone(),
        ) else {
            return Err(fail("no vault is configured on this workspace."));
        };
        let input = serde_json::json!({ "owner": owner, "repo": repo });

        let result = tool
            .execute(input)
            .await
            .map_err(|e| fail(&truncate_err(&e.to_string())))?;

        if result.success {
            return Ok(());
        }

        let err_text = result.error.unwrap_or_default();
        if err_text.starts_with("Repository already cloned") {
            return Ok(());
        }
        Err(fail(&truncate_err(&err_text)))
    }

    /// Run a thread's turn. The turn is registered with the drain, which ends
    /// it before releasing claims so that only its successor collects it.
    pub async fn dispatch_turn(
        self: Arc<Self>,
        turn: ChatTurn,
        surface: Arc<dyn ChannelSurface>,
    ) -> Result<()> {
        let service = Arc::clone(&self);
        let task = tokio::spawn(service.run_turn(turn, None, surface, Claim::Take));
        let _drain = crate::shutdown::Detached::register(task.abort_handle());
        match task.await {
            Ok(done) => done,
            Err(e) if e.is_cancelled() && crate::shutdown::in_progress() => Ok(()),
            Err(e) => Err(eyre!("the /cc turn did not finish: {e}")),
        }
    }

    /// Settle the round an interrupted turn left in `session`: collect it,
    /// deliver its answer to the thread and update its card. The record is
    /// ended once no such round is left, and kept for the next sweep while
    /// one is, such as when another hub still holds the thread.
    pub async fn settle_interrupted(
        self: &Arc<Self>,
        session: Session,
        token: String,
        surface: Arc<dyn ChannelSurface>,
    ) {
        let left = || self.interrupted(&session.channel, &session.owner, &session.owner);
        match left() {
            Ok(Some((turn, collected))) => {
                let service = Arc::clone(self);
                let task =
                    tokio::spawn(service.run_turn(turn, Some(collected), surface, Claim::Take));
                let _drain = crate::shutdown::Detached::register(task.abort_handle());
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(target: "cc", session = %session.channel, error = %e, "cc interrupted round not collected")
                    }
                    Err(e) if e.is_cancelled() && crate::shutdown::in_progress() => return,
                    Err(e) => {
                        warn!(target: "cc", session = %session.channel, error = %e, "cc interrupted round did not finish")
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(target: "cc", session = %session.channel, error = %e, "cc journal unreadable; the round is left for the next sweep");
                return;
            }
        }
        if !crate::shutdown::in_progress() && matches!(left(), Ok(None)) {
            crate::serve::end_turn_record(&self.sessions, &session.id, &token);
        }
    }

    /// The round an interrupted turn left unfinished in a thread, if its
    /// journal holds one it can read.
    fn interrupted(
        &self,
        session_key: &str,
        owner: &str,
        legacy_user: &str,
    ) -> Result<Option<(ChatTurn, Collected)>> {
        let Some(session) =
            find_thread_session(self.sessions.as_ref(), session_key, owner, legacy_user)
        else {
            return Ok(None);
        };
        let Some(pending) =
            crate::turn_journal::unfinished_calls(self.sessions.as_ref(), &session.id)?
                .into_iter()
                .rfind(|call| call.tool == PLAN_TOOL)
        else {
            return Ok(None);
        };
        let plan: CcPlan = match serde_json::from_value(pending.plan) {
            Ok(plan) => plan,
            Err(e) => {
                warn!(target: "cc", session = %session_key, error = %e, "cc plan unreadable");
                return Ok(None);
            }
        };
        let turn = ChatTurn {
            workspace_id: plan.workspace_id.clone(),
            session_key: session_key.to_string(),
            channel: plan.channel.clone(),
            thread_ts: plan.thread.clone(),
            message_ts: None,
            user: plan.user.clone(),
            text: plan.text.clone(),
            agent_id: Some(plan.agent_id.clone()),
            identity_id: plan.identity.clone(),
        };
        Ok(Some((
            turn,
            Collected {
                plan,
                call: pending.call,
                checkpoint: pending.checkpoint,
            },
        )))
    }

    /// Run one chat turn end-to-end, or collect the round an interrupted one
    /// left. A new turn's card is posted **before** the per-thread lock so a
    /// user whose message is queued behind an in-flight turn still gets
    /// instant visual feedback; serialisation happens on the round itself,
    /// and a round an interrupted turn left is collected first, under the
    /// same lock and claim.
    async fn run_turn(
        self: Arc<Self>,
        mut turn: ChatTurn,
        collected: Option<Collected>,
        surface: Arc<dyn ChannelSurface>,
        claim: Claim,
    ) -> Result<()> {
        info!(
            target: "cc",
            session = %turn.session_key,
            has_identity = turn.identity_id.is_some(),
            agent_id = ?turn.agent_id,
            "cc dispatch_turn entry"
        );

        // Defence in depth: the Slack ingress already tokenised secrets, so
        // this is normally a no-op. It catches anything that reached here
        // through a non-Slack path.
        if collected.is_none() {
            let (tokenized_text, _captures) = workforce_common::redact::tokenize_secrets(
                &turn.text,
                &turn.session_key,
                workforce_common::secret_marker::SecretSource::Slack {
                    workspace_id: turn.workspace_id.clone(),
                    channel: turn.channel.clone(),
                    message_ts: turn.message_ts.clone().unwrap_or_default(),
                    user: turn.user.clone(),
                },
            );
            turn.text = tokenized_text;
        }

        let preview = self.thread_session(&turn.session_key, turn.owner(), &turn.user)?;
        if !preview.is_claude_chat() {
            return Err(eyre!(
                "session {} is not in claude-chat mode",
                turn.session_key
            ));
        }
        let dispatched_at = Instant::now();
        let binding = ChatBinding::from_session(&preview)?;
        let kind = ChatKind::from_session(&preview);
        let chat = self.chat(kind)?;
        let thread_anchor = turn.thread_ts.clone().or_else(|| turn.message_ts.clone());
        let agent_id = turn
            .agent_id
            .clone()
            .or_else(|| preview.claude_chat_agent_id().map(String::from))
            .ok_or_else(|| eyre!("/cc thread has no agent_id to scope its workspace to"))?;
        let service = self
            .agent_registry
            .resolve(Some(&agent_id))
            .ok_or_else(|| eyre!("agent '{agent_id}' is not registered"))?;

        // A hub image without the CLI cannot run a Claude Code turn on this
        // host; say so before the card rather than after a failed spawn. A
        // worker brings its own CLI, and the hub loop needs none.
        let cli_health = (!self.remote)
            .then(|| chat.cli_health.try_read().ok().and_then(|h| h.clone()))
            .flatten();
        if let Some(CliHealth::Missing { reason }) = cli_health {
            warn!(
                target: "cc",
                session = %turn.session_key,
                engine = chat.engine.label(),
                reason = %reason,
                "cc dispatch short-circuited: the engine's CLI is missing"
            );
            let names = kind.names();
            let (sum, blocks) = cli_missing_blocks(names.cli, kind.package(), names.command);
            if let Err(e) = surface
                .send_message(&outbound(&turn.channel, thread_anchor.clone(), sum, blocks))
                .await
            {
                warn!(target: "cc", session = %turn.session_key, error = %e, "cc cli-missing notice send failed; user has no in-thread signal");
            }
            return Ok(());
        }

        // Ask the engine chain whether it can serve this user before the card
        // or the lock, so a turn nothing can run does not spawn, die and
        // grind through retries first. Resolved under the turn's identity:
        // this task does not inherit `CURRENT_USER_ID`.
        let ctx = RoundContext {
            service: &service,
            identity: turn.identity_id.as_deref(),
            stand_in: &turn.user,
            ticket_id: None,
            description: "cc",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        };
        let preflight = {
            let check = || chat.engine.preflight(&ctx);
            match turn.identity_id.clone() {
                Some(uid) => workforce_agent::runner::CURRENT_USER_ID.sync_scope(uid, check),
                None => check(),
            }
        };
        if let (None, Err(reason)) = (&collected, preflight) {
            warn!(
                target: "cc",
                session = %turn.session_key,
                %reason,
                "cc dispatch short-circuited: no coding engine can serve this user"
            );
            let (sum, blocks) = engine_unavailable_blocks(&reason.reason);
            if let Err(e) = surface
                .send_message(&outbound(&turn.channel, thread_anchor.clone(), sum, blocks))
                .await
            {
                warn!(target: "cc", session = %turn.session_key, error = %e, "cc engine-unavailable notice send failed; user has no in-thread signal");
            }
            return Ok(());
        }

        info!(target: "cc", kind = kind.key(), session = %turn.session_key, phase = "preflight-passed", elapsed_ms = dispatched_at.elapsed().as_millis() as u64, "cc turn");
        let started_at = Instant::now();
        let state = Arc::new(Mutex::new(TurnState {
            user_text: turn.text.clone(),
            user_mention: format!("<@{}>", turn.user),
            binding: binding.label(),
            cli: kind.names().cli,
            status: TurnStatus::Starting,
            tool_calls: Vec::new(),
            assistant_text: Vec::new(),
            stats: None,
            stop_value: turn.session_key.clone(),
            started_at,
            crash_diagnostic: None,
        }));

        let turn_message_id = match &collected {
            Some(collected) => collected.plan.card.clone(),
            None => post_card(&surface, &turn.channel, thread_anchor.clone(), &state)
                .await
                .map_err(|e| eyre!("failed to post turn message: {e}"))?,
        };

        let lock = self
            .turn_locks
            .entry(turn.session_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        // A collect never waits and never touches its card, which belongs to
        // a round whose thread is busy: whoever holds it collects the round.
        let waiting = collected.is_none();
        let _guard = match (claim, lock.try_lock()) {
            (Claim::Held, _) => None,
            (Claim::Take, Ok(g)) => Some(g),
            (Claim::Take, Err(_)) if !waiting => return Ok(()),
            (Claim::Take, Err(_)) => {
                self.show(
                    &state,
                    &surface,
                    &turn,
                    &turn_message_id,
                    TurnStatus::Queued,
                )
                .await;
                let g = lock.lock().await;
                self.show(
                    &state,
                    &surface,
                    &turn,
                    &turn_message_id,
                    TurnStatus::Starting,
                )
                .await;
                Some(g)
            }
        };

        // One turn per thread across the fleet: the lock above only orders
        // this replica's turns.
        let claim_deadline = Instant::now() + Duration::from_secs(CHAT_TURN_TIMEOUT_SECS);
        let _claim = loop {
            if claim == Claim::Held {
                break None;
            }
            match crate::session_turn_claim::SessionTurnClaim::try_acquire(
                &self.sessions,
                &preview.id,
            ) {
                Ok(Some(claim)) => break Some(claim),
                Ok(None) if !waiting => return Ok(()),
                Ok(None) if Instant::now() < claim_deadline => {
                    self.show(
                        &state,
                        &surface,
                        &turn,
                        &turn_message_id,
                        TurnStatus::Queued,
                    )
                    .await;
                    tokio::time::sleep(CLAIM_RETRY).await;
                }
                Ok(None) => {
                    let failed = TurnStatus::Failed(
                        "another hub is still running this thread's last turn".into(),
                    );
                    self.end_before_round(&state, &surface, &turn, &turn_message_id, failed)
                        .await;
                    return Ok(());
                }
                Err(e) if !waiting => return Err(e),
                Err(e) => {
                    let failed = TurnStatus::Failed(truncate_err(&e.to_string()));
                    self.end_before_round(&state, &surface, &turn, &turn_message_id, failed)
                        .await;
                    return Err(e);
                }
            }
        };
        if collected.is_none() {
            let left = match self.interrupted(&turn.session_key, turn.owner(), &turn.user) {
                Ok(left) => left,
                Err(e) => {
                    let failed = TurnStatus::Failed(truncate_err(&format!(
                        "this thread's record of its last round is unreadable: {e}"
                    )));
                    self.end_before_round(&state, &surface, &turn, &turn_message_id, failed)
                        .await;
                    return Err(e);
                }
            };
            if let Some((left, earlier)) = left {
                let collecting = Arc::clone(&self).run_turn(
                    left,
                    Some(earlier),
                    Arc::clone(&surface),
                    Claim::Held,
                );
                if let Err(e) = Box::pin(collecting).await {
                    warn!(target: "cc", session = %turn.session_key, error = %e, "cc interrupted round not collected");
                }
            }
        }
        // A round found unfinished before the lock may have finished while
        // this waited for it, and its card with it.
        if let Some(collected) = &collected {
            let still_open =
                crate::turn_journal::unfinished_calls(self.sessions.as_ref(), &preview.id)?
                    .iter()
                    .any(|call| call.call == collected.call);
            if !still_open {
                return Ok(());
            }
            self.show(
                &state,
                &surface,
                &turn,
                &turn_message_id,
                TurnStatus::Starting,
            )
            .await;
        }
        let journal =
            crate::turn_journal::SessionJournal::detached(Arc::clone(&self.sessions), &preview.id);
        let mut ending = TurnEnding {
            token: self
                .sessions
                .begin_turn(
                    &preview.id,
                    None,
                    &TurnPrincipal::run_as(turn.identity_id.clone()),
                )
                .map_err(|e| eyre!("recording the /cc turn: {e}"))?,
            sessions: Arc::clone(&self.sessions),
            session_id: preview.id.clone(),
            journal: Arc::clone(&journal),
            settled: false,
        };

        let cancel = CancellationToken::new();
        let prompts = Arc::new(TurnPrompts::default());
        info!(
            target: "cc",
            session = %turn.session_key,
            channel = %turn.channel,
            message_id = %turn_message_id,
            "cc active_turn inserted"
        );
        self.active_turns.insert(
            turn.session_key.clone(),
            Arc::new(ActiveTurn {
                cancel: cancel.clone(),
                channel: turn.channel.clone(),
                turn_message_id: turn_message_id.clone(),
                thread_key: turn.thread_key(),
                requester: turn.user.clone(),
                state: Arc::clone(&state),
                prompts: Arc::clone(&prompts),
                _ends: Arc::clone(&prompts.ledger).end_on_drop(),
            }),
        );
        let agent_vault = self.agent_vault(Some(&agent_id));
        let deadline = Instant::now() + Duration::from_secs(CHAT_TURN_TIMEOUT_SECS);
        let attempt = AttemptScope {
            session: SessionContext {
                key: Some(turn.session_key.clone()),
                id: Some(preview.id.clone()),
                user_id: turn.identity_id.clone(),
                channel_id: Some(turn.channel.clone()),
                thread_id: thread_anchor.clone(),
                agent_id: Some(agent_id.clone()),
                workspace_id: Some(turn.workspace_id.clone()),
                ..SessionContext::default()
            },
            thread: thread_anchor.clone(),
            prompts,
        };

        // Everything from here runs under the turn's identity so per-user
        // vault paths resolve; `dispatch_turn` is reached via `tokio::spawn`,
        // which does not inherit task-locals. Every way out of this block
        // ends on the card as a terminal status.
        let run = async {
            // Re-read under the lock so we don't race another writer on the UUID.
            let mut session = self.thread_session(&turn.session_key, turn.owner(), &turn.user)?;
            let choice = match &collected {
                Some(collected) => ChatChoice {
                    model: collected.plan.model.clone(),
                    effort: collected.plan.effort.clone(),
                },
                None => chat.effective(&ChatChoice {
                    model: session.claude_chat_model().map(String::from),
                    effort: session.claude_chat_effort().map(String::from),
                }),
            };
            let project = self.project_for(&binding, &agent_id)?;
            let repo_backed = !project.repos.is_empty();

            // Deliberate rotation, capping prefix-replay cost. A workspace the
            // launcher turns out to have lost is handled after provisioning,
            // with its own notice, because only the launcher knows. A collected
            // round keeps the transcript it ran on.
            let rotation = match &collected {
                Some(_) => None,
                None => self.rotation.rotation(
                    session.claude_chat_turn_count(),
                    session.claude_chat_last_reply_at(),
                    chrono::Utc::now(),
                ),
            };
            if rotation.is_some() {
                session.clear_claude_session_uuid();
            }
            let completed_turns = session.claude_chat_turn_count();
            let (mut resumed_transcript, mut claude_uuid) = match &collected {
                Some(collected) => (collected.plan.resumed, collected.plan.uuid),
                None => (
                    session.claude_session_uuid().is_some(),
                    Uuid::parse_str(&session.get_or_init_claude_uuid())
                        .map_err(|e| eyre!("invalid claude_session_uuid on session: {e}"))?,
                ),
            };
            if collected.is_none() && !resumed_transcript {
                self.sessions
                    .save(&session)
                    .map_err(|e| eyre!("session save failed (new uuid): {e}"))?;
            }
            info!(
                target: "cc",
                session = %turn.session_key,
                resumed_transcript,
                claude_uuid = %claude_uuid,
                rotated = ?rotation,
                collected = collected.is_some(),
                "cc UUID state at dispatch"
            );
            if let Some(reason) = rotation {
                self.notify_rotation(kind, &turn, &surface, reason).await;
            }

            // The same per-agent and hub-wide Claude Code budget the other
            // surfaces draw from, held from provisioning through the round.
            let Some(_permits) = crate::cc_budget::acquire(&service, Some(&cancel)).await else {
                return Ok::<_, eyre::Report>(None);
            };
            let tokens = github_tokens_for(
                &project,
                agent_vault.as_deref(),
                self.global_vault.as_deref(),
            );
            let gh_token = round_github_token(
                &project,
                &tokens,
                agent_vault.as_deref(),
                self.global_vault.as_deref(),
            );
            let scope_of =
                |uuid: &Uuid| workspace_scope(repo_backed, &turn.session_key, &uuid.to_string());
            let seating = Seating {
                agent_id: &agent_id,
                project: &project,
                tokens: &tokens,
                turn: &turn,
                deadline,
                cancel: &cancel,
                state: &state,
                surface: &surface,
                turn_message_id: &turn_message_id,
            };

            // Where a collected round resumes: its last recorded job, and the
            // leg that job belongs to. A mark the plan does not know is an
            // outcome nobody can tell.
            let key = collected
                .as_ref()
                .map_or_else(RoundKey::new, |collected| collected.plan.key.clone());
            let resume = match collected.as_ref().map(|c| c.checkpoint.as_deref()) {
                Some(Some(mark)) => {
                    Some(key.resume_at(mark).ok_or_else(|| eyre!(OUTCOME_UNKNOWN))?)
                }
                _ => None,
            };
            let (first_jobs, cold_jobs) = match resume {
                Some(Resume { cold: true, jobs }) => (None, Some(jobs)),
                Some(Resume { cold: false, jobs }) => (Some(jobs), None),
                None => (Some(key.jobs(false)), None),
            };
            let seat_uuid = match &cold_jobs {
                Some(_) => key.cold_uuid(),
                None => claude_uuid,
            };
            let mut sandbox = self.seat(&seating, &scope_of(&seat_uuid)).await?;
            info!(target: "cc", kind = kind.key(), session = %turn.session_key, phase = "seated", elapsed_ms = dispatched_at.elapsed().as_millis() as u64, "cc turn");
            if collected.is_none() {
                if workspace_lost(sandbox.fresh(), resumed_transcript, completed_turns) {
                    warn!(
                        target: "cc",
                        session = %turn.session_key,
                        stale_uuid = %claude_uuid,
                        "cc workspace was reclaimed since the last turn; starting fresh"
                    );
                    let stale_scope = scope_of(&claude_uuid);
                    claude_uuid = self.reset_claude_session_uuid(
                        &turn.session_key,
                        turn.owner(),
                        &turn.user,
                        Uuid::new_v4(),
                    )?;
                    resumed_transcript = false;
                    if repo_backed {
                        self.forget_scope(&agent_id, &project.name, &stale_scope)
                            .await;
                        sandbox = self.seat(&seating, &scope_of(&claude_uuid)).await?;
                    }
                    self.notify_rotation(kind, &turn, &surface, RotationReason::WorkspaceMissing)
                        .await;
                }
                if repo_backed {
                    self.warn_branch_staleness(
                        &project,
                        sandbox.as_ref(),
                        &surface,
                        &turn.channel,
                        thread_anchor.as_deref(),
                    )
                    .await;
                }
            }

            // A worker that keeps its jobs through a restart gets a plan
            // before anything runs, and keyed jobs recorded against it, so
            // a successor can collect the round.
            let kept = sandbox.placement().is_some_and(|placement| placement.jobs);
            if collected.is_some() && !kept {
                return Err(eyre!(OUTCOME_UNKNOWN));
            }
            let call = match &collected {
                Some(collected) => ToolCall::resumed(
                    collected.call.clone(),
                    Some(journal.clone()),
                    serde_json::Value::Null,
                ),
                None => ToolCall::new(Some(journal.clone()), serde_json::Value::Null),
            };
            if kept && collected.is_none() {
                let plan = CcPlan {
                    key: key.clone(),
                    uuid: claude_uuid,
                    resumed: resumed_transcript,
                    workspace_id: turn.workspace_id.clone(),
                    channel: turn.channel.clone(),
                    thread: thread_anchor.clone(),
                    card: turn_message_id.clone(),
                    user: turn.user.clone(),
                    identity: turn.identity_id.clone(),
                    agent_id: agent_id.clone(),
                    text: turn.text.clone(),
                    model: choice.model.clone(),
                    effort: choice.effort.clone(),
                };
                call.pending(PLAN_TOOL, serde_json::to_value(&plan)?)
                    .await
                    .map_err(|e| eyre!("recording the round before starting it: {e}"))?;
            }

            let round =
                |uuid: Uuid, resume: bool, system_prompt: Option<String>, jobs| CodingRound {
                    task: turn.text.clone(),
                    max_turns: Some(CHAT_MAX_TURNS),
                    timeout: Duration::from_secs(CHAT_TURN_TIMEOUT_SECS),
                    model: choice.model.clone(),
                    effort: choice.effort.clone(),
                    tools: self.default_allowed_tools.clone(),
                    system_prompt,
                    continuity: Continuity::Session { id: uuid, resume },
                    // Parity with local Claude Code: the repo's CLAUDE.md carries
                    // the conventions the model needs to code well.
                    memory: MemoryLoad {
                        claude_md: true,
                        auto_memory: true,
                    },
                    gh_token: gh_token.clone(),
                    cache_dir: None,
                    jobs: kept.then_some(jobs),
                };
            let first = match first_jobs {
                Some(jobs) => Some(
                    call.clone()
                        .scope(run_chat_attempt(
                            chat.engine.as_ref(),
                            Arc::clone(&sandbox),
                            &round(claude_uuid, resumed_transcript, None, jobs),
                            &ctx,
                            &attempt,
                            &state,
                            &surface,
                            &turn.channel,
                            &turn_message_id,
                            cancel.clone(),
                        ))
                        .await,
                ),
                None => None,
            };

            // Recover from a wiped transcript: move to the round's cold
            // transcript, tell the user their prior context is gone, and
            // re-run once with a recap.
            let result = match first {
                Some(result) if !transcript_lost(&result) => result,
                first => {
                    let fresh_uuid = key.cold_uuid();
                    if first.is_some() {
                        warn!(
                            session = %turn.session_key,
                            stale_uuid = %claude_uuid,
                            "Claude Code session transcript missing — restarting fresh"
                        );
                        let stale_scope = scope_of(&claude_uuid);
                        self.reset_claude_session_uuid(
                            &turn.session_key,
                            turn.owner(),
                            &turn.user,
                            fresh_uuid,
                        )?;
                        info!(target: "cc", session = %turn.session_key, %fresh_uuid, "cc recovery uuid minted");
                        if repo_backed {
                            self.forget_scope(&agent_id, &project.name, &stale_scope)
                                .await;
                            sandbox = self.seat(&seating, &scope_of(&fresh_uuid)).await?;
                        }
                    }
                    let brief = self
                        .brief_store
                        .load(&turn.session_key)
                        .await
                        .map(|b| b.summary(200));
                    if first.is_some() {
                        let (notice_summary, notice_blocks) =
                            session_lost_blocks(kind.names(), brief.as_deref());
                        if let Err(e) = surface
                            .send_message(&outbound(
                                &turn.channel,
                                thread_anchor.clone(),
                                notice_summary,
                                notice_blocks,
                            ))
                            .await
                        {
                            warn!(target: "cc", session = %turn.session_key, error = %e, "cc session-lost notice send failed; user will not be told prior context was wiped");
                        }
                        {
                            let mut s = state.lock().await;
                            s.status = TurnStatus::Starting;
                            s.tool_calls.clear();
                            s.assistant_text.clear();
                            s.stats = None;
                        }
                        if let Err(e) =
                            edit_card(&surface, &turn.channel, &turn_message_id, &state).await
                        {
                            warn!(target: "cc", session = %turn.session_key, error = %e, "cc fresh-session card edit failed; will retry on next state change");
                        }
                    }
                    // The recap goes in the system prompt, not the user slot:
                    // meta-context in `task` changes how the model treats the
                    // user's actual words.
                    let jobs = cold_jobs.unwrap_or_else(|| key.jobs(true));
                    call.clone()
                        .scope(run_chat_attempt(
                            chat.engine.as_ref(),
                            Arc::clone(&sandbox),
                            &round(
                                fresh_uuid,
                                false,
                                inject_recap(None, brief.as_deref()),
                                jobs,
                            ),
                            &ctx,
                            &attempt,
                            &state,
                            &surface,
                            &turn.channel,
                            &turn_message_id,
                            cancel.clone(),
                        ))
                        .await
                }
            };
            Ok(Some((result, sandbox)))
        };

        let outcome = match turn.identity_id.clone() {
            Some(uid) => {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(uid, run)
                    .await
            }
            None => run.await,
        };
        let (result, sandbox) = match outcome {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                ending.settled = true;
                self.end_before_round(
                    &state,
                    &surface,
                    &turn,
                    &turn_message_id,
                    TurnStatus::Stopped,
                )
                .await;
                return Ok(());
            }
            Err(_) if cancel.is_cancelled() => {
                ending.settled = true;
                self.end_before_round(
                    &state,
                    &surface,
                    &turn,
                    &turn_message_id,
                    TurnStatus::Stopped,
                )
                .await;
                return Ok(());
            }
            Err(e) => {
                let failed = TurnStatus::Failed(truncate_err(&e.to_string()));
                self.end_before_round(&state, &surface, &turn, &turn_message_id, failed)
                    .await;
                return Err(e);
            }
        };

        let duration_ms = started_at.elapsed().as_millis() as u64;
        let cancelled = cancel.is_cancelled();

        // Accumulate this turn's cost into the per-thread ledger before the
        // card is finalised, so the running total lands on the same edit.
        let cost_update = match &result {
            Ok(r) => match self
                .cost_ledger
                .add_turn(&turn.session_key, r.cost_usd)
                .await
            {
                Ok(u) => Some(u),
                Err(e) => {
                    warn!(target: "cc", session = %turn.session_key, error = %e, "cc cost ledger update failed; thread total will be missing on card");
                    None
                }
            },
            Err(_) => None,
        };
        let thread_cost_usd = cost_update.as_ref().map(|u| u.snapshot.total_cost_usd);
        {
            let mut s = state.lock().await;
            info!(
                target: "cc",
                session = %turn.session_key,
                cancelled,
                prior_status = ?s.status,
                result_ok = result.is_ok(),
                thread_cost_usd,
                "cc finalize_state input"
            );
            finalize_state(&mut s, &result, duration_ms, cancelled, thread_cost_usd);
        }

        // Soft cap: one notice when the thread's spend crosses a new band.
        if let Some(band) = cost_update.as_ref().and_then(|u| u.threshold_crossed) {
            let total = cost_update
                .as_ref()
                .map(|u| u.snapshot.total_cost_usd)
                .unwrap_or(band);
            let (sum, blocks) = thread_spend_threshold_blocks(band, total);
            if let Err(e) = surface
                .send_message(&outbound(&turn.channel, thread_anchor.clone(), sum, blocks))
                .await
            {
                warn!(target: "cc", session = %turn.session_key, error = %e, "cc thread-spend notice send failed; user will not see threshold crossing");
            }
        }

        // After a crash, stash uncommitted edits so the model's progress is
        // recoverable and the next turn cannot overwrite it. Best effort.
        let should_stash = matches!(&result, Ok(r) if r.completion == claude_code::Completion::Failed)
            && !cancelled;
        if should_stash {
            let stash_label = format!("WIP: cc subprocess crash {}", &turn.session_key);
            let stashes =
                crate::projects::cc_session::stash_dirty_repos(sandbox.as_ref(), &stash_label)
                    .await;
            if !stashes.is_empty() {
                let mut s = state.lock().await;
                match s.crash_diagnostic.as_mut() {
                    Some(d) => d.wip_stash = stashes,
                    None => warn!(
                        target: "cc",
                        session = %turn.session_key,
                        "cc post-crash stash produced entries but no diagnostic was set; stashes: {}",
                        stashes.iter().map(|s| s.repo.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                }
            }
        }

        // Answer first, card second: the reply is a thread post of its own,
        // so a long turn is never stuck behind a card edit.
        deliver_answer(
            &surface,
            &turn.channel,
            thread_anchor.clone(),
            &turn.user,
            &turn.session_key,
            &state,
        )
        .await;
        ending.settled = true;
        if let Err(e) = edit_card(&surface, &turn.channel, &turn_message_id, &state).await {
            warn!(target: "cc", session = %turn.session_key, error = %e, "cc final card edit failed; answer was already posted as thread reply");
        }
        info!(target: "cc", session = %turn.session_key, "cc active_turn removed");
        self.active_turns.remove(&turn.session_key);

        if matches!(&result, Ok(r) if r.exit_code == 0 && !cancelled) {
            if let Err(e) =
                self.record_chat_turn_completed(&turn.session_key, turn.owner(), &turn.user)
            {
                warn!(target: "cc", session = %turn.session_key, error = %e, "cc turn-count bump failed; rotation cadence may drift");
            }
            // The quality gate lands as a follow-up card after the answer,
            // and only reports: the user asks for the fix in the next turn.
            let reports = crate::projects::cc_quality_gate::run_in(sandbox.as_ref()).await;
            if let Some((sum, blocks)) = crate::slack_blocks::quality_gate_blocks(
                kind.names(),
                &crate::projects::cc_quality_gate::summarise(&reports),
            ) {
                if let Err(e) = surface
                    .send_message(&outbound(&turn.channel, thread_anchor, sum, blocks))
                    .await
                {
                    warn!(target: "cc", session = %turn.session_key, error = %e, "cc quality-gate notice send failed");
                }
            }
        }

        log_outcome(&turn.session_key, &result);
        Ok(())
    }

    /// Put the card in `status` and show it; a failed edit is logged, the
    /// next state change retries.
    async fn show(
        &self,
        state: &Arc<Mutex<TurnState>>,
        surface: &Arc<dyn ChannelSurface>,
        turn: &ChatTurn,
        turn_message_id: &str,
        status: TurnStatus,
    ) {
        state.lock().await.status = status;
        if let Err(e) = edit_card(surface, &turn.channel, turn_message_id, state).await {
            warn!(target: "cc", session = %turn.session_key, error = %e, "cc card edit failed; will retry on next state change");
        }
    }

    /// End a turn that never reached its round: the card gets its terminal
    /// status and the thread is free for the next turn.
    async fn end_before_round(
        &self,
        state: &Arc<Mutex<TurnState>>,
        surface: &Arc<dyn ChannelSurface>,
        turn: &ChatTurn,
        turn_message_id: &str,
        status: TurnStatus,
    ) {
        self.show(state, surface, turn, turn_message_id, status)
            .await;
        self.active_turns.remove(&turn.session_key);
    }

    /// Materialise the workspace at `scope`, showing the queued card while a
    /// worker at capacity is waited out; a Stop while queued ends the wait.
    async fn seat(&self, seating: &Seating<'_>, scope: &str) -> Result<Arc<dyn CodingSandbox>> {
        let Seating {
            turn,
            state,
            surface,
            turn_message_id,
            ..
        } = *seating;
        let spec = WorkspaceSpec {
            agent_id: seating.agent_id,
            project: seating.project,
            scope,
            tokens: seating.tokens,
            home: self.home_for(&turn.session_key),
            primary: None,
        };
        let seated = ensure_with_wait(
            self.launcher.as_ref(),
            &spec,
            seating.deadline,
            Some(seating.cancel),
            || self.show(state, surface, turn, turn_message_id, TurnStatus::Queued),
        )
        .await;
        match seated {
            Ok(sandbox) => {
                if matches!(state.lock().await.status, TurnStatus::Queued) {
                    self.show(state, surface, turn, turn_message_id, TurnStatus::Starting)
                        .await;
                }
                Ok(sandbox)
            }
            Err(e) => Err(eyre!("Failed to materialise the workspace: {e}")),
        }
    }

    /// Drop a scope a rotated transcript no longer needs. Best effort: an
    /// orphan is reclaimed by the sweeper.
    async fn forget_scope(&self, agent_id: &str, project: &str, scope: &str) {
        if let Err(e) = self.launcher.close(agent_id, project, scope).await {
            warn!(target: "cc", %scope, error = %e, "cc stale workspace not released; the sweeper will");
        }
    }

    /// Tell the thread its transcript restarted, with the brief when there
    /// is one.
    async fn notify_rotation(
        &self,
        kind: ChatKind,
        turn: &ChatTurn,
        surface: &Arc<dyn ChannelSurface>,
        reason: RotationReason,
    ) {
        warn!(target: "cc", session = %turn.session_key, ?reason, "cc session restarted with a fresh transcript");
        let brief = self
            .brief_store
            .load(&turn.session_key)
            .await
            .map(|b| b.summary(200));
        let (summary, blocks) = match reason {
            RotationReason::WorkspaceMissing => {
                workspace_reset_blocks(kind.names(), brief.as_deref())
            }
            RotationReason::TurnCount | RotationReason::Idle => {
                session_rotated_blocks(kind.names(), brief.as_deref())
            }
        };
        if let Err(e) = surface
            .send_message(&outbound(
                &turn.channel,
                turn.thread_ts.clone().or_else(|| turn.message_ts.clone()),
                summary,
                blocks,
            ))
            .await
        {
            warn!(target: "cc", session = %turn.session_key, error = %e, "cc rotation notice send failed; user will not be told prior context was reset");
        }
    }

    /// Persist that one chat turn just completed under the current UUID. Drives
    /// the rotation thresholds checked at the start of the next dispatch.
    fn record_chat_turn_completed(
        &self,
        session_key: &str,
        owner: &str,
        legacy_user: &str,
    ) -> Result<()> {
        let mut session = self.thread_session(session_key, owner, legacy_user)?;
        session.record_claude_chat_turn(chrono::Utc::now());
        self.sessions
            .save(&session)
            .map_err(|e| eyre!("session save failed during turn-count bump: {e}"))
    }
}

/// Build an `OutboundMessage` carrying Block Kit blocks and a notification fallback.
fn outbound(
    channel: &str,
    thread_id: Option<String>,
    summary: String,
    blocks: Vec<serde_json::Value>,
) -> OutboundMessage {
    OutboundMessage {
        channel: channel.to_string(),
        text: summary,
        thread_id,
        username: None,
        icon_url: None,
        blocks: Some(blocks),
    }
}

/// Render the current state, post it as a new Slack message, return the new message id.
async fn post_card(
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    thread_id: Option<String>,
    state: &Arc<Mutex<TurnState>>,
) -> Result<String> {
    let (summary, blocks) = {
        let s = state.lock().await;
        render_turn(&s)
    };
    let sent = surface
        .send_message(&outbound(channel, thread_id, summary, blocks))
        .await?;
    Ok(sent.message_id)
}

/// Deliver the model's accumulated response text to the user, with layered fallbacks.
///
/// Primary: post as one or more thread replies via `send_message` in text-only mode,
/// which chunks long text across `chat.postMessage` calls — far more robust than
/// packing the answer into the card's Block Kit payload (the historical bug that
/// froze cards on long turns).
///
/// Fallback A — Slack DM: if the thread post fails (channel archived, permissions
/// changed, transient API error beyond retries), open a DM with the requesting user
/// and post the answer there with a "thread post failed" preamble. The user still
/// gets the answer; it just lands in their DMs instead of the thread.
///
/// Fallback B — log dump: if the DM also fails (token revoked, Slack outage), the
/// answer is durably logged at warn-level so an operator can recover it from
/// structured logs. The user sees nothing in Slack, which is the irreducible
/// outcome when Slack itself is unreachable.
///
/// Returns `()` because the caller has nothing actionable to do with a failure that
/// has already exhausted all delivery paths and produced its own logs.
async fn deliver_answer(
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    thread_id: Option<String>,
    user: &str,
    session_key: &str,
    state: &Arc<Mutex<TurnState>>,
) {
    let joined = {
        let s = state.lock().await;
        s.assistant_text.join("\n\n")
    };
    if joined.trim().is_empty() {
        info!(target: "cc", session = %session_key, "cc deliver_answer: no text to post");
        return;
    }

    let primary_err = match post_text_thread_reply(surface, channel, thread_id, &joined).await {
        Ok(()) => return,
        Err(e) => {
            warn!(
                target: "cc",
                session = %session_key,
                error = %e,
                "cc thread reply failed; attempting DM fallback"
            );
            e
        }
    };

    if let Err(dm_err) = post_text_dm(surface, user, &joined).await {
        warn!(
            target: "cc",
            session = %session_key,
            user = %user,
            thread_error = %primary_err,
            dm_error = %dm_err,
            answer = %joined,
            "cc DM fallback also failed; answer text is in this log line for recovery"
        );
        return;
    }

    info!(
        target: "cc",
        session = %session_key,
        user = %user,
        "cc DM fallback delivered answer to user's DMs (thread post had failed)"
    );
}

/// Build a plain-text (no-blocks) `OutboundMessage`. The Slack surface's
/// text-only path will handle markdown conversion and chunking automatically.
fn text_outbound(channel: String, text: String, thread_id: Option<String>) -> OutboundMessage {
    OutboundMessage {
        channel,
        text,
        thread_id,
        username: None,
        icon_url: None,
        blocks: None,
    }
}

/// Send `text` as a plain (no-blocks) message into the given Slack thread.
async fn post_text_thread_reply(
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    thread_id: Option<String>,
    text: &str,
) -> Result<()> {
    let msg = text_outbound(channel.to_string(), text.to_string(), thread_id);
    surface.send_message(&msg).await.map(|_| ())
}

/// Open a DM with `user` and post `text` there, prefixed with a notice so the user
/// understands why the answer arrived in their DMs rather than the thread.
async fn post_text_dm(surface: &Arc<dyn ChannelSurface>, user: &str, text: &str) -> Result<()> {
    let dm_channel = surface.open_conversation(user).await?;
    let preamble =
        "⚠️ _Posting your answer to the original thread failed — full answer below._\n\n";
    let msg = text_outbound(dm_channel, format!("{preamble}{text}"), None);
    surface.send_message(&msg).await.map(|_| ())
}

/// Render the current state and edit an existing Slack message in place.
///
/// Returns the underlying `update_message` `Result` so callers can decide whether
/// a failure is recoverable (next debounce tick will retry) or terminal (in which
/// case the answer-delivery thread reply is the user-visible source of truth, not
/// the card). Callers should `warn!`-log on failure rather than `?`-propagate, since
/// a failed card edit is never fatal to the dispatcher's flow.
async fn edit_card(
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    message_id: &str,
    state: &Arc<Mutex<TurnState>>,
) -> Result<()> {
    let (summary, blocks) = {
        let s = state.lock().await;
        render_turn(&s)
    };
    surface
        .update_message(
            channel,
            message_id,
            &outbound(channel, None, summary, blocks),
        )
        .await
}

fn finalize_state(
    s: &mut TurnState,
    result: &Result<ClaudeCodeResult>,
    duration_ms: u64,
    cancelled: bool,
    thread_cost_usd: Option<f64>,
) {
    let stats_from = |r: &ClaudeCodeResult| TurnStats {
        input_tokens: r.input_tokens as u32,
        output_tokens: r.output_tokens as u32,
        cache_read_tokens: r.cache_read_tokens as u32,
        cost_usd: r.cost_usd,
        num_turns: r.num_turns,
        duration_ms,
        thread_cost_usd,
        engine: (!r.engine.is_empty()).then(|| r.engine.clone()),
        model: r.model.clone(),
    };
    match result {
        Ok(r) if r.exit_code == 0 => {
            s.status = TurnStatus::Done;
            s.stats = Some(stats_from(r));
            s.crash_diagnostic = None;
        }
        Ok(r) => {
            // Prefer the classified crash label ("killed (signal 9 —
            // likely OOM)") over the raw "exit N" — the user's
            // actionable signal is in the classification, not the code.
            let label = r
                .crash_kind
                .as_ref()
                .map(|k| k.label())
                .unwrap_or_else(|| format!("exit {}", r.exit_code));
            s.status = if cancelled || matches!(s.status, TurnStatus::Stopped) {
                TurnStatus::Stopped
            } else {
                TurnStatus::Failed(label.clone())
            };
            s.stats = Some(stats_from(r));
            // Diagnostic block runs alongside the failure label; only
            // populated when we actually classified a crash AND there's
            // stderr to show or the label adds new info beyond the
            // already-rendered Failed(label).
            s.crash_diagnostic = if cancelled || matches!(s.status, TurnStatus::Stopped) {
                None
            } else {
                r.crash_kind.as_ref().map(|k| CrashDiagnostic {
                    label: k.label(),
                    stderr_tail: r.stderr_tail.clone(),
                    wip_stash: Vec::new(),
                })
            };
        }
        Err(e) => {
            s.status = if cancelled || matches!(s.status, TurnStatus::Stopped) {
                TurnStatus::Stopped
            } else {
                TurnStatus::Failed(truncate_err(&e.to_string()))
            };
            s.crash_diagnostic = None;
        }
    }
}

fn log_outcome(session_key: &str, result: &Result<ClaudeCodeResult>) {
    match result {
        Ok(r) if r.exit_code == 0 => info!(
            session = %session_key,
            turns = r.num_turns,
            cost_usd = format!("{:.4}", r.cost_usd),
            tokens_in = r.input_tokens,
            tokens_out = r.output_tokens,
            cache_creation = r.cache_creation_tokens,
            cache_read = r.cache_read_tokens,
            "Claude chat turn completed"
        ),
        Ok(r) => warn!(
            session = %session_key,
            exit_code = r.exit_code,
            "Claude chat turn ended non-zero"
        ),
        Err(e) => warn!(
            session = %session_key,
            error = %e,
            "Claude chat turn errored"
        ),
    }
}

/// What every attempt of one `/cc` turn runs under: the thread's session, so
/// the round's hub tools act and tokenise in it, and the turn's approval
/// prompts, posted in the thread.
struct AttemptScope {
    session: SessionContext,
    thread: Option<String>,
    prompts: Arc<TurnPrompts>,
}

/// One engine round against a Slack turn card: spawns the progress renderer,
/// runs the round, then drains the renderer. Extracted so `dispatch_turn` can
/// re-run after a session-not-found recovery without duplicating the setup.
/// The round runs in the thread's session with the turn's progress sink and
/// approval ledger scoped, which is where its hub tools are bound from.
#[allow(clippy::too_many_arguments)]
async fn run_chat_attempt(
    engine: &dyn CodingEngine,
    sandbox: Arc<dyn CodingSandbox>,
    round: &CodingRound,
    ctx: &RoundContext<'_>,
    attempt: &AttemptScope,
    state: &Arc<Mutex<TurnState>>,
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    turn_message_id: &str,
    cancel: CancellationToken,
) -> Result<ClaudeCodeResult> {
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel::<ProgressEvent>(128);
    let renderer = tokio::spawn(run_renderer(
        progress_rx,
        Arc::clone(state),
        Arc::clone(surface),
        CardPlace {
            channel: channel.to_string(),
            message_id: turn_message_id.to_string(),
            thread: attempt.thread.clone(),
        },
        Arc::clone(&attempt.prompts),
    ));
    let result = attempt
        .session
        .clone()
        .scope(scope_turn(
            Some(progress_tx.clone()),
            Some(Arc::clone(&attempt.prompts.ledger)),
            engine.run(sandbox, round, ctx, Some(cancel), Some(progress_tx)),
        ))
        .await;
    // Every sender is dropped with the round; the renderer drains its queue
    // and exits.
    let _ = renderer.await;
    result
}

/// `{github_owner -> PAT}` for every repo of `project`, layered per user.
fn github_tokens_for(
    project: &Project,
    agent_vault: Option<&Vault>,
    global_vault: Option<&Vault>,
) -> HashMap<String, String> {
    project
        .repos
        .iter()
        .filter_map(|repo| {
            let RepoSource::Github { owner, .. } = &repo.source;
            crate::user_config::resolve_github_pat(agent_vault, global_vault, owner)
                .map(|token| (owner.clone(), token))
        })
        .collect()
}

/// The one token a round's `gh` runs with: the primary repo owner's, or the
/// bench PAT for a workspace with no repo to key an owner off.
fn round_github_token(
    project: &Project,
    tokens: &HashMap<String, String>,
    agent_vault: Option<&Vault>,
    global_vault: Option<&Vault>,
) -> Option<String> {
    project
        .repos
        .first()
        .and_then(|repo| {
            let RepoSource::Github { owner, .. } = &repo.source;
            tokens.get(owner).cloned()
        })
        .or_else(|| {
            project
                .repos
                .is_empty()
                .then(|| crate::user_config::resolve_bench_github_pat(agent_vault, global_vault))
                .flatten()
        })
}

/// Where a turn's card is, and the thread its approval prompts go to.
struct CardPlace {
    channel: String,
    message_id: String,
    thread: Option<String>,
}

/// Consume progress events, update turn state, edit the turn message at most once per
/// `EDIT_DEBOUNCE_MS` so we stay well inside Slack's message-edit rate limit. Also
/// fires a heartbeat re-edit every `HEARTBEAT_INTERVAL` while the channel is open
/// so the elapsed-seconds counter ticks even when the subprocess is mid-tool-call
/// and producing no events. The select is bounded by `rx.recv() → None`: once the
/// progress channel closes (subprocess returned, `progress_tx` dropped) the loop
/// exits, so the heartbeat can never race with the dispatcher's final card edit.
///
/// An approval a held hub tool call raises is posted in the thread as its own
/// prompt card, which a click resolves; one that settles with nobody having
/// clicked is marked expired, its buttons gone.
async fn run_renderer(
    mut rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
    state: Arc<Mutex<TurnState>>,
    surface: Arc<dyn ChannelSurface>,
    place: CardPlace,
    prompts: Arc<TurnPrompts>,
) {
    let CardPlace {
        channel,
        message_id,
        thread,
    } = place;
    let mut last_edit_at: Option<Instant> = None;
    let mut pending = false;
    let mut posted: HashMap<String, (String, Vec<serde_json::Value>)> = HashMap::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick is immediate; skip it so we don't double-edit at start.
    heartbeat.tick().await;

    loop {
        let debounce = if pending {
            match last_edit_at {
                Some(t) => Duration::from_millis(EDIT_DEBOUNCE_MS)
                    .saturating_sub(Instant::now().saturating_duration_since(t)),
                None => Duration::from_millis(0),
            }
        } else {
            Duration::from_secs(3600)
        };

        tokio::select! {
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(ProgressEvent::ApprovalRequired {
                        request_id,
                        agent_id,
                        tool_name,
                        detail,
                        reason,
                        options,
                        ..
                    }) => {
                        let blocks = permission_prompt_blocks(
                            &agent_id, &tool_name, &detail, &reason, &request_id, &options,
                        );
                        let fallback = format!(
                            ":warning: Approval required: agent `{agent_id}` wants to run `{tool_name}`."
                        );
                        match surface
                            .send_message(&outbound(&channel, thread.clone(), fallback, blocks.clone()))
                            .await
                        {
                            Ok(sent) => {
                                posted.insert(request_id, (sent.message_id, blocks));
                            }
                            Err(e) => warn!(target: "cc", channel = %channel, error = %e, "cc approval prompt post failed; the call waits unanswered"),
                        }
                    }
                    Some(ProgressEvent::ApprovalResolved { request_id }) => {
                        if let Some((prompt_id, blocks)) = posted.remove(&request_id) {
                            if !prompts.was_answered(&request_id) {
                                expire_prompt(&surface, &channel, &prompt_id, blocks).await;
                            }
                        }
                    }
                    Some(event) => {
                        state.lock().await.apply(event);
                        pending = true;
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep(debounce), if pending => {
                if let Err(e) = edit_card(&surface, &channel, &message_id, &state).await {
                    warn!(target: "cc", channel = %channel, message_id = %message_id, error = %e, "cc streaming card edit failed; will retry on next event");
                }
                last_edit_at = Some(Instant::now());
                pending = false;
            }
            _ = heartbeat.tick() => {
                // Only fire when nothing else has edited the card recently —
                // otherwise we'd write the same content the debounce flush
                // just wrote. The render-time elapsed counter ticks on every
                // call regardless of state changes, so this gives users a
                // visible sign of life during multi-minute tool calls.
                let stale = last_edit_at
                    .map(|t| t.elapsed() >= HEARTBEAT_INTERVAL)
                    .unwrap_or(true);
                if stale {
                    if let Err(e) = edit_card(&surface, &channel, &message_id, &state).await {
                        warn!(target: "cc", channel = %channel, message_id = %message_id, error = %e, "cc heartbeat card edit failed; next event will retry");
                    }
                    last_edit_at = Some(Instant::now());
                }
            }
        }
    }

    if pending {
        if let Err(e) = edit_card(&surface, &channel, &message_id, &state).await {
            warn!(target: "cc", channel = %channel, message_id = %message_id, error = %e, "cc renderer-flush card edit failed; final state may not reflect on card");
        }
    }
    // A prompt still open when the round ended can no longer be answered.
    for (request_id, (prompt_id, blocks)) in posted {
        if !prompts.was_answered(&request_id) {
            expire_prompt(&surface, &channel, &prompt_id, blocks).await;
        }
    }
}

/// Mark a prompt nobody answered as expired and take its buttons away.
async fn expire_prompt(
    surface: &Arc<dyn ChannelSurface>,
    channel: &str,
    prompt_id: &str,
    blocks: Vec<serde_json::Value>,
) {
    let ack = "⌛ expired without an answer, so it did not run".to_string();
    let blocks = crate::slack_manager::resolved_permission_blocks(
        Some(&serde_json::Value::Array(blocks)),
        crate::slack_manager::ResolutionOutcome::Expired,
        ack.clone(),
        Some("no one"),
        "",
    );
    if let Err(e) = surface
        .update_message(channel, prompt_id, &outbound(channel, None, ack, blocks))
        .await
    {
        warn!(target: "cc", channel = %channel, error = %e, "cc expired approval prompt edit failed; its buttons stay");
    }
}

impl TurnState {
    fn apply(&mut self, event: ProgressEvent) {
        // RateLimited stamps the wait state without further bookkeeping; any
        // subsequent event (handled below) means the retry has progressed and
        // we transition back into the live working state.
        if let ProgressEvent::RateLimited { wait_secs, .. } = event {
            self.status = TurnStatus::RateLimited { wait_secs };
            return;
        }
        if matches!(
            self.status,
            TurnStatus::Starting | TurnStatus::RateLimited { .. }
        ) {
            self.status = TurnStatus::Running;
        }
        match event {
            ProgressEvent::ToolStart { name, detail, .. } => {
                let tool = name.strip_prefix("claude:").unwrap_or(&name).to_string();
                self.tool_calls.push((tool, detail));
            }
            ProgressEvent::Response { text } => {
                if !text.trim().is_empty() {
                    self.assistant_text.push(text);
                }
            }
            ProgressEvent::StreamDelta { text } => {
                if !text.is_empty() {
                    match self.assistant_text.last_mut() {
                        Some(last) => last.push_str(&text),
                        None => self.assistant_text.push(text),
                    }
                }
            }
            _ => {}
        }
    }
}

/// Pure agent-selection logic — factored out for unit testing.
///
/// See [`ClaudeChatService::pick_agent`] for behavior; this function
/// operates on a snapshot of the registry (ids + default) rather than on
/// live `AgentService`s so it can be exercised without heavy setup.
fn pick_agent_core(
    explicit: Option<&str>,
    hint: Option<&str>,
    agent_ids: &[String],
    default_id: &str,
) -> std::result::Result<String, String> {
    let known = |id: &str| agent_ids.iter().any(|a| a == id);
    let list_agents = || {
        let mut ids = agent_ids.to_vec();
        ids.sort();
        ids.into_iter()
            .map(|a| format!("`@{a}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    if let Some(id) = explicit {
        return if known(id) {
            Ok(id.to_string())
        } else {
            Err(format!(
                "Unknown agent `@{id}`. Available: {}. Usage: `/cc @<agent> <owner/repo> [task]`.",
                list_agents()
            ))
        };
    }

    if let Some(id) = hint {
        if known(id) {
            return Ok(id.to_string());
        }
        // Routing pointed at an id that no longer exists — fall through
        // to the ambiguity check rather than silently using the default.
    }

    if agent_ids.len() <= 1 {
        // Single-agent deployment → never ambiguous. Empty registry is
        // surfaced later when we try to read a vault.
        return Ok(default_id.to_string());
    }

    Err(format!(
        "This channel isn't bound to an agent. Pick one: `/cc @<agent> <owner/repo> [task]`. \
         Available: {}.",
        list_agents()
    ))
}

fn truncate_err(s: &str) -> String {
    const MAX: usize = 400;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// Build the `--append-system-prompt` value for a `session_lost`
/// recovery, preserving any existing prompt the caller had configured
/// and prepending the recap when a brief is available. Returns the
/// caller's existing value unchanged when there's no brief — recap is
/// purely additive, never lossy.
///
/// Recap intentionally goes *before* the existing prompt so the
/// existing rules occupy the highest-priority position closest to the
/// user turn. Only sensible on the lost-recovery path; rotation
/// surfaces the brief in the card without injecting it into the prompt
/// (the user is mid-conversation and may have moved on).
fn inject_recap(existing: Option<String>, brief: Option<&str>) -> Option<String> {
    let Some(brief) = brief else {
        return existing;
    };
    let recap = format!(
        "You are continuing a Slack thread whose prior Claude Code transcript was lost. \
         The user's original ask in this thread was:\n\n> {brief}\n\n\
         Treat the most recent user turn as a continuation of that goal unless they say otherwise."
    );
    Some(match existing {
        Some(prior) if !prior.is_empty() => format!("{recap}\n\n{prior}"),
        _ => recap,
    })
}

fn clone_failure_message(owner: &str, repo: &str, detail: &str) -> String {
    format!("Couldn't clone `{owner}/{repo}`: {detail}")
}

/// Outcome of resolving a user-supplied `/cc <repo>` argument against the
/// on-disk workspace. `NotCloned` carries the split coordinates so the caller
/// can trigger an auto-clone; `Unrecoverable` carries a Slack-ready error for
/// inputs we can't act on (ambiguous bare names, traversal attempts, …).
#[derive(Debug)]
enum ResolveOutcome {
    Ok(String),
    NotCloned { owner: String, repo: String },
    Unrecoverable(String),
}

/// Resolve a user-supplied `/cc <repo>` argument to the on-disk `owner/name`
/// form, whatever case it was typed in. Bare names are accepted when exactly
/// one cloned repo matches by basename.
///
/// Only an `owner/name` checkout can answer: a thread binds to its repo's
/// upstream, and a checkout sitting directly under `repos/` names no owner.
fn resolve_chat_repo(repo_arg: &str, agent_workspace: &Path) -> ResolveOutcome {
    let available = list_available_repos(agent_workspace);
    let owned: Vec<String> = available
        .iter()
        .filter(|full| full.contains('/'))
        .cloned()
        .collect();
    let cloned = repos_named(repo_arg, &owned);

    let Some((owner, name)) = repo_arg.split_once('/') else {
        return match cloned.as_slice() {
            [] => ResolveOutcome::Unrecoverable(not_cloned_message(repo_arg, &available)),
            [one] => ResolveOutcome::Ok((*one).clone()),
            many => ResolveOutcome::Unrecoverable(format!(
                "Repo `{}` is ambiguous — matches {}. Use the full `owner/repo` form.",
                repo_arg,
                many.iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    };

    // Syntactic guard against absolute paths, `..` traversal, and multi-slash
    // forms: what is not two safe segments is never joined to a path, nor
    // handed on to be cloned.
    if !is_safe_path_segment(owner) || !is_safe_path_segment(name) {
        return ResolveOutcome::Unrecoverable(not_cloned_message(repo_arg, &available));
    }

    // The clone on disk decides the spelling. Taken as typed, `Acme/API`
    // beside a cloned `acme/api` would be cloned a second time on a
    // filesystem that tells the two apart.
    match cloned.as_slice() {
        [one] => ResolveOutcome::Ok((*one).clone()),
        [] => ResolveOutcome::NotCloned {
            owner: owner.to_string(),
            repo: name.to_string(),
        },
        many => match many.iter().find(|full| full.as_str() == repo_arg) {
            Some(exact) => ResolveOutcome::Ok((*exact).clone()),
            None => ResolveOutcome::Unrecoverable(format!(
                "Repo `{}` is ambiguous — matches {}. Write it exactly as cloned.",
                repo_arg,
                many.iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && !s.contains('/')
}

fn not_cloned_message(repo_arg: &str, available: &[String]) -> String {
    let hint = clone_hint(repo_arg);
    if available.is_empty() {
        format!("Repo `{repo_arg}` not cloned and no repos are available in the workspace. {hint}")
    } else {
        let listed = available
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Repo `{repo_arg}` not cloned. Available: {listed}. {hint}")
    }
}

/// Self-service remedy appended to "not cloned" errors. Prefills the owner/repo
/// arguments when the user supplied them; otherwise keeps them as placeholders.
fn clone_hint(repo_arg: &str) -> String {
    match repo_arg.split_once('/') {
        Some((owner, repo)) if !owner.is_empty() && !repo.is_empty() => format!(
            "Ask an agent to run `github.clone(owner=\"{owner}\", repo=\"{repo}\")` to add it."
        ),
        _ => "Ask an agent to run `github.clone(owner=…, repo=…)` to add it.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;

    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_common::channels::{ChannelCapabilities, Reaction, SentMessage};

    #[test]
    fn a_cx_turn_runs_at_medium_effort_unless_the_thread_or_the_config_names_one() {
        let engine: Arc<dyn CodingEngine> = Arc::new(crate::coding_engine::CodexEngine::new(
            crate::coding_engine::CodexAuth::Subscription,
            Default::default(),
        ));
        let unpinned = ChatChoice::default();
        let defaulted = ChatEngine::codex(Arc::clone(&engine), None, None);
        assert_eq!(
            defaulted.effective(&unpinned).effort.as_deref(),
            Some(DEFAULT_CX_EFFORT)
        );
        assert_eq!(DEFAULT_CX_EFFORT, "medium");
        let pinned = ChatChoice {
            model: None,
            effort: Some("low".into()),
        };
        assert_eq!(
            defaulted.effective(&pinned).effort.as_deref(),
            Some("low"),
            "an explicit effort: word wins"
        );
        let configured = ChatEngine::codex(engine, None, Some("xhigh".into()));
        assert_eq!(
            configured.effective(&unpinned).effort.as_deref(),
            Some("xhigh"),
            "[codex_chat] effort wins over the default"
        );
    }

    fn offered() -> ModelChoices {
        ModelChoices {
            models: ["astra", "sol", "terra", "luna"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            efforts: &["low", "medium", "high", "xhigh", "max"],
            raw_id_prefixes: &["gpt-"],
        }
    }

    #[test]
    fn model_and_effort_words_are_taken_from_before_the_task_and_nowhere_else() {
        let (choice, rest) = split_chat_options(
            "@allan Model:Astra project:fieldops effort:XHIGH fix the flaky spec",
        )
        .unwrap();
        assert_eq!(choice.model.as_deref(), Some("astra"));
        assert_eq!(choice.effort.as_deref(), Some("xhigh"));
        assert_eq!(rest, "@allan project:fieldops fix the flaky spec");
        let (choice, rest) =
            split_chat_options("effort:low driftwerk/workforce model:sol").unwrap();
        assert_eq!(
            (choice.model.as_deref(), choice.effort.as_deref()),
            (Some("sol"), Some("low"))
        );
        assert_eq!(rest, "driftwerk/workforce");
        let (choice, rest) = split_chat_options(
            "driftwerk/workforce explain why model:foo and effort:max appear here",
        )
        .unwrap();
        assert_eq!(choice, ChatChoice::default());
        assert_eq!(
            rest,
            "driftwerk/workforce explain why model:foo and effort:max appear here"
        );
        let (choice, rest) =
            split_chat_options("model:sol model:astra summarise the module:loader").unwrap();
        assert_eq!(choice.model.as_deref(), Some("astra"));
        assert_eq!(rest, "summarise the module:loader");
        let err = split_chat_options("model: driftwerk/workforce").unwrap_err();
        assert!(err.contains("`model:` needs a value"), "{err}");
    }

    #[test]
    fn a_choice_is_an_alias_or_a_full_model_id_and_a_known_effort() {
        let ok = |model: &str, effort: Option<&str>| {
            validate_chat_choice(
                &ChatChoice {
                    model: Some(model.into()),
                    effort: effort.map(String::from),
                },
                &offered(),
            )
        };
        assert!(ok("astra", Some("xhigh")).is_ok());
        assert!(ok("gpt-5.6-terra", None).is_ok());
        assert!(
            ok("claude-opus-4-8", None).is_err(),
            "a Claude id is not a Codex model"
        );
        let claude = crate::coding_engine::ClaudeCodeEngine.model_choices();
        assert!(validate_chat_choice(
            &ChatChoice {
                model: Some("gpt-5.6-sol".into()),
                effort: None
            },
            &claude
        )
        .is_err());
        assert!(validate_chat_choice(
            &ChatChoice {
                model: Some("claude-opus-4-8".into()),
                effort: None
            },
            &claude
        )
        .is_ok());
        assert!(validate_chat_choice(
            &ChatChoice {
                model: None,
                effort: Some("xhigh".into())
            },
            &claude
        )
        .is_err());
        let err = ok("astro", None).unwrap_err();
        assert!(
            err.contains("`model:astro`") && err.contains("`astra`"),
            "{err}"
        );
        let err = ok("sol", Some("ultra")).unwrap_err();
        assert!(
            err.contains("`effort:ultra`") && err.contains("`xhigh`"),
            "{err}"
        );
        assert!(validate_chat_choice(&ChatChoice::default(), &offered()).is_ok());
    }

    fn fresh_state() -> TurnState {
        TurnState {
            user_text: "hello".into(),
            user_mention: "<@U1>".into(),
            binding: "workforce".into(),
            cli: "Claude Code",
            status: TurnStatus::Starting,
            tool_calls: Vec::new(),
            assistant_text: Vec::new(),
            stats: None,
            stop_value: "slack:ws:C1:t1".into(),
            started_at: Instant::now(),
            crash_diagnostic: None,
        }
    }

    /// Mock chat surface that records every send/open call and can inject failures
    /// on specified call positions. Used to test the answer-delivery fallback chain.
    struct MockChatSurface {
        sends: Mutex<Vec<OutboundMessage>>,
        opens: Mutex<Vec<String>>,
        send_calls: AtomicUsize,
        // Call indices (0-based) that should fail; all others succeed.
        fail_send_at: Vec<usize>,
        fail_open_conversation: bool,
        dm_channel: String,
    }

    impl MockChatSurface {
        fn new() -> Self {
            Self {
                sends: Mutex::new(Vec::new()),
                opens: Mutex::new(Vec::new()),
                send_calls: AtomicUsize::new(0),
                fail_send_at: Vec::new(),
                fail_open_conversation: false,
                dm_channel: "D-DM-CHANNEL".to_string(),
            }
        }

        fn fail_sends_at(mut self, indices: Vec<usize>) -> Self {
            self.fail_send_at = indices;
            self
        }

        fn fail_open_conversation(mut self) -> Self {
            self.fail_open_conversation = true;
            self
        }

        async fn sent(&self) -> Vec<OutboundMessage> {
            self.sends.lock().await.clone()
        }

        async fn opens(&self) -> Vec<String> {
            self.opens.lock().await.clone()
        }
    }

    #[async_trait]
    impl ChannelSurface for MockChatSurface {
        fn platform(&self) -> &str {
            "mock-chat"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                threads: true,
                rich_text: true,
                send_message: true,
                edit_message: true,
                ..Default::default()
            }
        }

        async fn add_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
            let idx = self.send_calls.fetch_add(1, Ordering::SeqCst);
            self.sends.lock().await.push(message.clone());
            if self.fail_send_at.contains(&idx) {
                return Err(eyre!("mock send failure at call {idx}"));
            }
            Ok(SentMessage {
                channel: message.channel.clone(),
                message_id: format!("mock-ts-{idx}"),
                thread_id: message.thread_id.clone(),
                platform: "mock-chat".to_string(),
            })
        }

        async fn open_conversation(&self, user_id: &str) -> Result<String> {
            self.opens.lock().await.push(user_id.to_string());
            if self.fail_open_conversation {
                return Err(eyre!("mock open_conversation failure"));
            }
            Ok(self.dm_channel.clone())
        }
    }

    mod approvals {
        use super::*;
        use crate::coding_engine::{CliRoundResult, ModelChoices};
        use crate::coding_testing::Seats;
        use crate::mcp_turn::testing::Routes;
        use crate::mcp_turn::ClientTimeout;
        use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool};
        use workforce_common::session::InMemorySessionStore;
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};

        /// A channel without thread isolation keys its sessions by channel
        /// alone, while a button click names the thread its card is in.
        const KEY: &str = "slack:T1:C1";

        /// A round whose one hub tool call needs approval, bound as every
        /// engine binds its round's tools; keeps what the call answered.
        #[derive(Default)]
        struct HoldsACall(std::sync::Mutex<Vec<String>>);

        #[async_trait]
        impl CodingEngine for HoldsACall {
            fn label(&self) -> &'static str {
                "claude-code"
            }
            fn preflight(&self, _: &RoundContext<'_>) -> Result<(), crate::coding_engine::Refusal> {
                Ok(())
            }
            fn model_choices(&self) -> ModelChoices {
                ModelChoices {
                    models: Vec::new(),
                    efforts: &[],
                    raw_id_prefixes: &[],
                }
            }
            async fn run(
                &self,
                sandbox: Arc<dyn CodingSandbox>,
                _: &CodingRound,
                ctx: &RoundContext<'_>,
                cancel: Option<CancellationToken>,
                _: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
            ) -> Result<CliRoundResult> {
                let tools = ctx
                    .round_tools(
                        sandbox.as_ref(),
                        cancel.as_ref(),
                        ClientTimeout::CLAUDE_CODE,
                    )
                    .expect("the catalogue is served");
                let reply = tools
                    .endpoint()
                    .handle(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"catalogue_remove_entry","arguments":{"name":"figma"}}}"#,
                    )
                    .await
                    .unwrap_or_default();
                self.0.lock().unwrap().push(reply.clone());
                Ok(CliRoundResult {
                    text: reply,
                    completion: claude_code::Completion::Success,
                    ..Default::default()
                })
            }
        }

        /// The prompt card the renderer posted in the thread: its message id,
        /// its request id and its blocks.
        async fn prompt_card(surface: &MockChatSurface) -> (String, String, serde_json::Value) {
            loop {
                let sent = surface.sent().await;
                let card = sent.iter().enumerate().find_map(|(i, m)| {
                    let blocks = serde_json::Value::Array(m.blocks.clone()?);
                    let request = blocks
                        .as_array()?
                        .iter()
                        .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
                        .find(|e| e["action_id"] == "perm_allow_once")?["value"]
                        .as_str()?
                        .to_string();
                    assert_eq!(
                        m.thread_id.as_deref(),
                        Some("100.1"),
                        "posted in the thread"
                    );
                    Some((format!("mock-ts-{i}"), request, blocks))
                });
                if let Some(card) = card {
                    return card;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// A `/cc` thread whose turn, started by `u1`, holds one call to the
        /// recording tool on its approval card.
        struct Held {
            registry: Arc<AgentRegistry>,
            cc: Arc<ClaudeChatService>,
            surface: Arc<MockChatSurface>,
            service: Arc<crate::serve::AgentService>,
            engine: Arc<HoldsACall>,
            tool: Arc<RecordingTool>,
            running: tokio::task::JoinHandle<Result<()>>,
            _tmp: tempfile::TempDir,
        }

        async fn held(routes: Option<Arc<Routes>>) -> Held {
            let tmp = tempfile::tempdir().unwrap();
            let service = crate::mcp_turn::testing::service().await;
            let tool = Arc::new(RecordingTool::default());
            service.tools.register(Arc::clone(&tool) as Arc<dyn Tool>);
            service.set_permission_gate(Arc::new(AskEveryCall));
            if let Some(routes) = routes {
                service.set_approval_router(routes);
            }
            let agent_id = service.agent_id.to_string();
            let projects: Arc<dyn workforce_projects::ProjectStore> =
                Arc::new(workforce_projects::sqlite::SqliteProjectStore::in_memory().unwrap());
            projects.put(&Project::new(&agent_id, "p1")).unwrap();
            let sessions: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
            let mut session = sessions.get_or_create(KEY, "u1", PSEUDO_AGENT_ID).unwrap();
            session.mark_claude_chat_project("p1", "u1", Some(&agent_id));
            sessions.save(&session).unwrap();
            let registry = Arc::new(AgentRegistry::single(Arc::clone(&service)));
            let engine = Arc::new(HoldsACall::default());
            let cc = Arc::new(ClaudeChatService::new(
                sessions,
                Arc::new(Seats {
                    hub_tools: vec!["catalogue.*".to_string()],
                    ..Default::default()
                }),
                engine.clone(),
                false,
                tmp.path().join("workspace"),
                tmp.path().join("data"),
                projects,
                Arc::clone(&registry),
                None,
            ));
            let surface = Arc::new(MockChatSurface::new());
            let turn = ChatTurn {
                workspace_id: "T1".into(),
                session_key: KEY.into(),
                channel: "C1".into(),
                thread_ts: Some("100.1".into()),
                message_ts: Some("100.2".into()),
                user: "u1".into(),
                text: "remove figma".into(),
                agent_id: Some(agent_id),
                identity_id: Some("u1".into()),
            };
            let running = tokio::spawn(Arc::clone(&cc).dispatch_turn(turn, surface.clone()));
            Held {
                registry,
                cc,
                surface,
                service,
                engine,
                tool,
                running,
                _tmp: tmp,
            }
        }

        impl Held {
            /// `user` clicks `action` on the card.
            async fn click(&self, user: &str, action: &str) {
                let (card, request_id, blocks) = prompt_card(&self.surface).await;
                let surface: Arc<dyn ChannelSurface> = self.surface.clone();
                crate::slack_manager::handle_permission_button(
                    &self.registry,
                    Some(&self.cc),
                    Some(&surface),
                    "T1",
                    "C1",
                    Some("100.1"),
                    Some(&card),
                    user,
                    action,
                    Some(&request_id),
                    Some(&blocks),
                    "",
                )
                .await;
            }

            async fn finished(self) -> Vec<serde_json::Value> {
                self.running.await.unwrap().unwrap();
                self.tool.runs()
            }

            /// Link the Slack user `slack_id` to an identity holding `role`.
            fn identify(&self, slack_id: &str, role: IdentityRole) {
                let store: Arc<dyn IdentityStore> =
                    Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
                let mut identity = Identity::new(slack_id, role);
                identity.link_id("slack", slack_id);
                store.create(&identity).unwrap();
                *self.service.identity_store.write() = Some(store);
            }
        }

        #[tokio::test]
        async fn cc_approval_card_click_resolves_held_call() {
            let held = held(None).await;
            prompt_card(&held.surface).await;
            assert!(held.tool.runs().is_empty(), "held until someone clicks");
            held.click("u1", "perm_allow_once").await;
            assert_eq!(
                held.finished().await,
                [serde_json::json!({ "name": "figma" })]
            );
        }

        #[tokio::test]
        async fn bystander_cannot_answer_anothers_held_approval() {
            let held = held(None).await;
            held.click("U2", "perm_allow_once").await;
            held.click("u1", "perm_deny_once").await;
            assert!(
                held.finished().await.is_empty(),
                "the bystander's allow never reached the call; the requester's deny did"
            );
        }

        #[tokio::test]
        async fn bystander_always_allow_is_refused() {
            let held = held(None).await;
            held.identify("U9", IdentityRole::SystemAdmin);
            let writer = Arc::new(crate::mcp_turn::testing::Writer::default());
            held.service.set_permission_writer(writer.clone());
            held.click("U9", "perm_allow_always").await;
            held.click("u1", "perm_deny_once").await;
            assert!(held.finished().await.is_empty());
            assert!(
                writer.0.lock().is_empty(),
                "no rule was saved for someone else"
            );
        }

        #[tokio::test]
        async fn approver_can_answer() {
            let held = held(None).await;
            held.identify("U9", IdentityRole::SystemAdmin);
            held.click("U9", "perm_allow_once").await;
            assert_eq!(
                held.finished().await,
                [serde_json::json!({ "name": "figma" })]
            );
        }

        #[tokio::test]
        async fn cc_round_routed_approval_is_refused_not_resumed_as_runner_turn() {
            let routes = Arc::new(Routes::default());
            let held = held(Some(Arc::clone(&routes))).await;
            let engine = Arc::clone(&held.engine);
            assert!(held.finished().await.is_empty(), "nothing ran");
            assert!(
                routes.0.lock().is_empty(),
                "no pending approval was written"
            );
            let replies = engine.0.lock().unwrap().clone();
            assert!(
                replies
                    .iter()
                    .any(|reply| reply.contains("This action needs approval from bob")),
                "the round was told why, and whom to ask: {replies:?}"
            );
        }
    }

    mod durable {
        use super::*;
        use crate::coding_testing::{Rounds, Seats};
        use workforce_common::session::{InMemorySessionStore, JournalEntry, JournalStep};

        const KEY: &str = "slack:T1:C1:100.1";

        struct Thread {
            cc: Arc<ClaudeChatService>,
            engine: Arc<Rounds>,
            store: Arc<InMemorySessionStore>,
            sessions: Arc<dyn SessionStore>,
            surface: Arc<MockChatSurface>,
            session: Session,
            agent_id: String,
        }

        async fn thread(unkept: bool) -> Thread {
            let tmp = tempfile::tempdir().unwrap();
            let mut config = workforce_common::settings::WorkforceConfig::default();
            config.hub.data_dir = tmp.path().to_string_lossy().to_string();
            config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
                .join("memory.db")
                .to_string_lossy()
                .into_owned();
            config.llm.api_key = Some("test-key".into());
            let service = crate::serve::AgentService::from_config(config, None)
                .await
                .unwrap();
            let agent_id = service.agent_id.to_string();
            let projects: Arc<dyn workforce_projects::ProjectStore> =
                Arc::new(workforce_projects::sqlite::SqliteProjectStore::in_memory().unwrap());
            projects.put(&Project::new(&agent_id, "p1")).unwrap();
            let store = Arc::new(InMemorySessionStore::new());
            let sessions: Arc<dyn SessionStore> = store.clone();
            let mut session = sessions.get_or_create(KEY, "u1", PSEUDO_AGENT_ID).unwrap();
            session.mark_claude_chat_project("p1", "u1", Some(&agent_id));
            sessions.save(&session).unwrap();
            let engine = Arc::new(Rounds::default());
            let cc = Arc::new(ClaudeChatService::new(
                Arc::clone(&sessions),
                Arc::new(Seats {
                    unkept,
                    ..Default::default()
                }),
                engine.clone(),
                true,
                tmp.path().join("workspace"),
                tmp.path().join("data"),
                projects,
                Arc::new(AgentRegistry::single(service)),
                None,
            ));
            std::mem::forget(tmp);
            Thread {
                cc,
                engine,
                store,
                sessions,
                surface: Arc::new(MockChatSurface::new()),
                session,
                agent_id,
            }
        }

        impl Thread {
            fn turn(&self, text: &str) -> ChatTurn {
                ChatTurn {
                    workspace_id: "T1".into(),
                    session_key: KEY.into(),
                    channel: "C1".into(),
                    thread_ts: Some("100.1".into()),
                    message_ts: Some("100.2".into()),
                    user: "u1".into(),
                    text: text.into(),
                    agent_id: Some(self.agent_id.clone()),
                    identity_id: Some("u1".into()),
                }
            }

            fn surface(&self) -> Arc<dyn ChannelSurface> {
                self.surface.clone()
            }

            async fn dispatch(&self, text: &str) {
                Arc::clone(&self.cc)
                    .dispatch_turn(self.turn(text), self.surface())
                    .await
                    .unwrap();
            }

            fn journal(&self) -> Vec<JournalEntry> {
                self.sessions.journal(&self.session.id).unwrap()
            }

            async fn answers(&self) -> usize {
                self.surface
                    .sent()
                    .await
                    .iter()
                    .filter(|m| {
                        m.text.contains("PR_URL") && m.thread_id.as_deref() == Some("100.1")
                    })
                    .count()
            }

            /// A plan a turn journalled before its hub stopped, with the job
            /// it had started, and the record it left.
            fn interrupted(&self, key: &RoundKey, mark: Option<&str>) -> String {
                let plan = CcPlan {
                    key: key.clone(),
                    uuid: Uuid::new_v4(),
                    resumed: false,
                    workspace_id: "T1".into(),
                    channel: "C1".into(),
                    thread: Some("100.1".into()),
                    card: "card-ts".into(),
                    user: "u1".into(),
                    identity: Some("u1".into()),
                    agent_id: self.agent_id.clone(),
                    text: "fix it".into(),
                    model: None,
                    effort: None,
                };
                let entry = |step| JournalEntry {
                    call: "c1".into(),
                    at: chrono::Utc::now(),
                    step,
                };
                self.sessions
                    .journal_append(
                        &self.session.id,
                        &entry(JournalStep::Pending {
                            tool: PLAN_TOOL.into(),
                            input: serde_json::Value::Null,
                            plan: serde_json::to_value(&plan).unwrap(),
                            actor: None,
                        }),
                    )
                    .unwrap();
                if let Some(mark) = mark {
                    self.sessions
                        .journal_append(
                            &self.session.id,
                            &entry(JournalStep::Checkpoint { mark: mark.into() }),
                        )
                        .unwrap();
                }
                self.sessions
                    .begin_turn(&self.session.id, None, &Default::default())
                    .unwrap()
            }

            fn recorded(&self) -> bool {
                self.store.has_turn_record(&self.session.id)
            }
        }

        #[tokio::test]
        async fn a_turn_journals_its_plan_and_job_and_clears_them_once_answered() {
            let t = thread(false).await;
            let journalled = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = Arc::clone(&journalled);
            let sessions = Arc::clone(&t.sessions);
            let id = t.session.id.clone();
            *t.engine.probe.lock().unwrap() = Some(Box::new(move || {
                *seen.lock().unwrap() = sessions.journal(&id).unwrap()
            }));
            t.dispatch("fix it").await;

            let steps: Vec<_> = journalled
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.step.clone())
                .collect();
            let plan = match &steps[..] {
                [JournalStep::Pending { tool, plan, .. }, JournalStep::Checkpoint { mark }] => {
                    assert_eq!(tool, PLAN_TOOL);
                    let plan: CcPlan = serde_json::from_value(plan.clone()).unwrap();
                    assert_eq!(mark, plan.key.jobs(false).job().as_str());
                    plan
                }
                other => panic!("a round mid-flight journals its plan then its job: {other:?}"),
            };
            assert_eq!(
                (plan.text.as_str(), plan.card.is_empty()),
                ("fix it", false)
            );
            let runs = t.engine.runs();
            assert_eq!(runs.len(), 1);
            assert!(!runs[0].1, "a new round is not pinned");
            assert_eq!(runs[0].2.as_deref(), Some("u1"));
            assert_eq!(t.answers().await, 1);
            assert!(
                t.journal().is_empty(),
                "an answered round leaves no journal"
            );
            assert!(!t.recorded(), "nor a turn record");
        }

        #[tokio::test]
        async fn a_worker_that_does_not_keep_jobs_gets_no_plan() {
            let t = thread(true).await;
            let journalled = Arc::new(std::sync::Mutex::new(None));
            let seen = Arc::clone(&journalled);
            let sessions = Arc::clone(&t.sessions);
            let id = t.session.id.clone();
            *t.engine.probe.lock().unwrap() = Some(Box::new(move || {
                *seen.lock().unwrap() = Some(sessions.journal(&id).unwrap())
            }));
            t.dispatch("fix it").await;
            assert!(t.engine.runs().is_empty(), "its rounds are not keyed");
            assert_eq!(t.engine.seen.lock().unwrap().len(), 1);
            let mid_round = journalled.lock().unwrap().take().expect("the round ran");
            assert!(mid_round.is_empty(), "{mid_round:?}");
        }

        #[tokio::test]
        async fn an_interrupted_round_is_collected_at_its_job_and_answered_once() {
            let t = thread(false).await;
            let key = RoundKey::new();
            let mark = key.jobs(false).job().to_string();
            let token = t.interrupted(&key, Some(&mark));

            t.cc.settle_interrupted(t.session.clone(), token, t.surface())
                .await;

            assert_eq!(t.engine.runs(), vec![(mark, true, Some("u1".into()))]);
            assert_eq!(t.answers().await, 1);
            assert!(t.journal().is_empty());
            assert!(!t.recorded());
        }

        #[tokio::test]
        async fn a_new_message_collects_the_interrupted_round_first() {
            let t = thread(false).await;
            let key = RoundKey::new();
            let mark = key.jobs(false).job().to_string();
            t.interrupted(&key, Some(&mark));

            t.dispatch("and then this").await;

            let runs = t.engine.runs();
            assert_eq!(runs.len(), 2);
            assert_eq!((runs[0].0.as_str(), runs[0].1), (mark.as_str(), true));
            assert!(!runs[1].1, "then the new turn runs");
            assert_eq!(t.answers().await, 2);
            assert!(t.journal().is_empty());
        }

        #[tokio::test]
        async fn a_round_on_a_worker_that_no_longer_keeps_jobs_is_not_run_again() {
            let t = thread(true).await;
            let key = RoundKey::new();
            let token = t.interrupted(&key, Some(&key.jobs(false).job().to_string()));

            t.cc.settle_interrupted(t.session.clone(), token, t.surface())
                .await;

            assert!(t.engine.seen.lock().unwrap().is_empty());
            assert_eq!(t.answers().await, 0);
            assert!(t.journal().is_empty());
            assert!(!t.recorded());
        }

        #[tokio::test]
        async fn a_round_whose_thread_is_busy_is_left_for_whoever_holds_it() {
            let t = thread(false).await;
            let key = RoundKey::new();
            let token = t.interrupted(&key, Some(&key.jobs(false).job().to_string()));
            let lock =
                t.cc.turn_locks
                    .entry(KEY.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
            let _running = lock.lock().await;

            t.cc.settle_interrupted(t.session.clone(), token, t.surface())
                .await;

            assert!(t.engine.seen.lock().unwrap().is_empty());
            assert_eq!(t.journal().len(), 2, "its plan and job are kept");
            assert!(t.recorded(), "and its record, for the next sweep");
        }

        #[tokio::test]
        async fn a_round_that_recorded_no_job_is_driven_from_its_start() {
            let t = thread(false).await;
            let key = RoundKey::new();
            let token = t.interrupted(&key, None);
            t.cc.settle_interrupted(t.session.clone(), token, t.surface())
                .await;
            assert_eq!(
                t.engine.runs(),
                vec![(key.jobs(false).job().to_string(), false, Some("u1".into()))]
            );
        }

        #[tokio::test]
        async fn a_round_already_answered_is_not_collected_again() {
            let t = thread(false).await;
            let key = RoundKey::new();
            t.interrupted(&key, Some(&key.jobs(false).job().to_string()));
            let (turn, collected) = t.cc.interrupted(KEY, "u1", "u1").unwrap().unwrap();
            t.sessions.journal_clear(&t.session.id).unwrap();

            Arc::clone(&t.cc)
                .run_turn(turn, Some(collected), t.surface(), Claim::Take)
                .await
                .unwrap();

            assert!(t.engine.seen.lock().unwrap().is_empty());
            assert_eq!(t.answers().await, 0);
        }
    }

    fn state_with_text(text: &str) -> Arc<Mutex<TurnState>> {
        let mut s = fresh_state();
        s.assistant_text.push(text.to_string());
        Arc::new(Mutex::new(s))
    }

    /// Hold a concrete `Arc<MockChatSurface>` alongside the trait-object view that
    /// `deliver_answer` consumes — `Arc::clone` keeps both pointing at the same
    /// allocation, so assertions on the concrete handle observe what the SUT did.
    fn mock_surface(mock: MockChatSurface) -> (Arc<MockChatSurface>, Arc<dyn ChannelSurface>) {
        let concrete = Arc::new(mock);
        let dynamic: Arc<dyn ChannelSurface> = concrete.clone();
        (concrete, dynamic)
    }

    #[tokio::test]
    async fn deliver_answer_posts_to_thread_when_text_present() {
        let (mock, surface) = mock_surface(MockChatSurface::new());
        let state = state_with_text("the cross-reference report");
        deliver_answer(
            &surface,
            "C-CHAN",
            Some("THREAD-TS".to_string()),
            "U-USER",
            "session:1",
            &state,
        )
        .await;
        let sent = mock.sent().await;
        assert_eq!(sent.len(), 1, "exactly one send to the thread");
        assert_eq!(sent[0].channel, "C-CHAN");
        assert_eq!(sent[0].thread_id, Some("THREAD-TS".to_string()));
        assert_eq!(sent[0].text, "the cross-reference report");
        assert!(sent[0].blocks.is_none(), "text-only mode, no blocks");
        assert!(
            mock.opens().await.is_empty(),
            "no DM fallback when thread succeeds"
        );
    }

    #[tokio::test]
    async fn deliver_answer_skips_when_assistant_text_is_empty() {
        let (mock, surface) = mock_surface(MockChatSurface::new());
        let state = Arc::new(Mutex::new(fresh_state()));
        deliver_answer(
            &surface,
            "C-CHAN",
            Some("THREAD-TS".to_string()),
            "U-USER",
            "session:1",
            &state,
        )
        .await;
        assert!(mock.sent().await.is_empty(), "no posts when text is empty");
        assert!(mock.opens().await.is_empty(), "no DM when text is empty");
    }

    #[tokio::test]
    async fn deliver_answer_falls_back_to_dm_when_thread_post_fails() {
        // Index 0 = the thread post; index 1 = the DM post (after open_conversation).
        let (mock, surface) = mock_surface(MockChatSurface::new().fail_sends_at(vec![0]));
        let state = state_with_text("the answer");
        deliver_answer(
            &surface,
            "C-CHAN",
            Some("THREAD-TS".to_string()),
            "U-USER",
            "session:1",
            &state,
        )
        .await;
        let sent = mock.sent().await;
        assert_eq!(sent.len(), 2, "thread post + DM post recorded");
        assert_eq!(sent[0].thread_id, Some("THREAD-TS".to_string()));
        assert_eq!(sent[1].channel, "D-DM-CHANNEL");
        assert!(sent[1].thread_id.is_none(), "DM is not threaded");
        assert!(
            sent[1].text.contains("the answer"),
            "DM contains the answer text"
        );
        assert!(
            sent[1].text.contains("thread failed"),
            "DM has explanatory preamble: got {}",
            sent[1].text
        );
        assert_eq!(mock.opens().await, vec!["U-USER".to_string()]);
    }

    #[tokio::test]
    async fn deliver_answer_logs_text_when_both_paths_fail() {
        // Both sends fail; open_conversation succeeds so DM send is attempted-and-fails.
        let (mock, surface) = mock_surface(MockChatSurface::new().fail_sends_at(vec![0, 1]));
        let state = state_with_text("irrecoverable answer");
        deliver_answer(
            &surface,
            "C-CHAN",
            Some("THREAD-TS".to_string()),
            "U-USER",
            "session:1",
            &state,
        )
        .await;
        let sent = mock.sent().await;
        assert_eq!(sent.len(), 2, "two attempts before giving up");
        assert_eq!(mock.opens().await.len(), 1, "DM fallback was attempted");
    }

    #[tokio::test]
    async fn deliver_answer_logs_when_dm_open_fails_after_thread_failure() {
        let (mock, surface) = mock_surface(
            MockChatSurface::new()
                .fail_sends_at(vec![0])
                .fail_open_conversation(),
        );
        let state = state_with_text("doubly lost");
        deliver_answer(
            &surface,
            "C-CHAN",
            Some("THREAD-TS".to_string()),
            "U-USER",
            "session:1",
            &state,
        )
        .await;
        let sent = mock.sent().await;
        assert_eq!(
            sent.len(),
            1,
            "only the thread attempt; no DM send happened"
        );
        assert_eq!(mock.opens().await.len(), 1, "open_conversation was tried");
    }

    fn result_ok(exit_code: i32, cache: u64) -> Result<ClaudeCodeResult> {
        Ok(ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: "reply".into(),
            exit_code,
            duration: Duration::from_secs(1),
            input_tokens: 100,
            output_tokens: 42,
            cache_creation_tokens: 0,
            cache_read_tokens: cache,
            cost_usd: 0.01,
            num_turns: 1,
            completion: claude_code::Completion::Success,
            crash_kind: None,
            stderr_tail: String::new(),
            handed_over: Vec::new(),
            ..Default::default()
        })
    }

    #[test]
    fn inject_recap_returns_existing_unchanged_when_no_brief() {
        // Recap is purely additive; with no brief there's nothing to
        // prepend and we must not mutate the caller's prompt.
        let existing = Some("be concise".to_string());
        assert_eq!(
            inject_recap(existing.clone(), None),
            existing,
            "no brief → existing prompt must pass through unchanged"
        );
        assert_eq!(inject_recap(None, None), None);
    }

    #[test]
    fn inject_recap_preserves_existing_prompt_when_brief_is_set() {
        let result = inject_recap(Some("be concise".into()), Some("ship cost-tracking"));
        let prompt = result.expect("recap injection produces Some");
        assert!(prompt.contains("ship cost-tracking"));
        assert!(prompt.contains("be concise"));
        // Recap goes before the existing prompt so the existing rules
        // remain at the most recent (highest-priority) position.
        let recap_idx = prompt.find("ship cost-tracking").unwrap();
        let existing_idx = prompt.find("be concise").unwrap();
        assert!(recap_idx < existing_idx);
    }

    #[test]
    fn inject_recap_uses_brief_alone_when_no_existing_prompt() {
        let prompt = inject_recap(None, Some("ship cost-tracking")).unwrap();
        assert!(prompt.contains("ship cost-tracking"));
        assert!(prompt.contains("Treat the most recent user turn"));
    }

    #[test]
    fn a_repo_thread_is_a_single_repo_project_with_a_safe_name() {
        let project = repo_project("allan", "HarborLabs/HarborRails").unwrap();
        assert_eq!(project.agent_id, "allan");
        assert_eq!(project.name, "repo-harborlabs-harborrails");
        assert_eq!(project.repos.len(), 1);
        assert_eq!(project.repos[0].alias, "HarborRails");
        assert_eq!(
            project.repos[0].source,
            RepoSource::Github {
                owner: "HarborLabs".into(),
                repo: "HarborRails".into(),
            }
        );
        assert_eq!(repo_project_name("a b", "c/d"), "repo-a-b-c-d");
        for bad in ["nope", "/x", "x/"] {
            assert!(repo_project("allan", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_thread_keys_one_home_for_its_lifetime() {
        let key = home_key("slack:T1:C1:1699999999.1");
        assert_eq!(key, home_key("slack:T1:C1:1699999999.1"));
        assert_ne!(key, home_key("slack:T1:C1:1699999999.2"));
        assert!(key.starts_with("cc-dm-"), "{key}");
        assert!(key.len() <= 64);
        assert!(key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    }

    #[test]
    fn apply_tool_start_flips_starting_to_running_and_records_call() {
        let mut s = fresh_state();
        s.apply(ProgressEvent::ToolStart {
            name: "claude:Read".into(),
            round: 1,
            detail: "foo.rs".into(),
        });
        assert!(matches!(s.status, TurnStatus::Running));
        assert_eq!(
            s.tool_calls,
            vec![("Read".to_string(), "foo.rs".to_string())]
        );
    }

    #[test]
    fn apply_response_appends_non_empty_text() {
        let mut s = fresh_state();
        s.apply(ProgressEvent::Response {
            text: "first".into(),
        });
        s.apply(ProgressEvent::Response { text: "  ".into() });
        s.apply(ProgressEvent::Response {
            text: "second".into(),
        });
        assert_eq!(
            s.assistant_text,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn apply_stream_delta_concatenates_into_last_segment() {
        let mut s = fresh_state();
        s.apply(ProgressEvent::StreamDelta { text: "hel".into() });
        s.apply(ProgressEvent::StreamDelta { text: "lo".into() });
        assert_eq!(s.assistant_text, vec!["hello".to_string()]);
    }

    #[test]
    fn apply_rate_limited_stamps_wait_state() {
        let mut s = fresh_state();
        s.status = TurnStatus::Running;
        s.apply(ProgressEvent::RateLimited {
            wait_secs: 240,
            attempt: 1,
        });
        match s.status {
            TurnStatus::RateLimited { wait_secs } => assert_eq!(wait_secs, 240),
            other => panic!("expected RateLimited; got {other:?}"),
        }
    }

    #[test]
    fn apply_recovers_to_running_on_next_event_after_rate_limit() {
        // After the runner resumes, the very next event (a tool call here)
        // should flip the card back to the live "working" state.
        let mut s = fresh_state();
        s.status = TurnStatus::RateLimited { wait_secs: 240 };
        s.apply(ProgressEvent::ToolStart {
            name: "claude:Read".into(),
            round: 1,
            detail: "foo.rs".into(),
        });
        assert!(matches!(s.status, TurnStatus::Running));
        assert_eq!(s.tool_calls.len(), 1);
    }

    #[test]
    fn apply_preserves_terminal_status() {
        let mut s = fresh_state();
        s.status = TurnStatus::Stopped;
        s.apply(ProgressEvent::ToolStart {
            name: "claude:Read".into(),
            round: 1,
            detail: "".into(),
        });
        assert!(matches!(s.status, TurnStatus::Stopped));
    }

    #[test]
    fn finalize_success_sets_done_with_stats() {
        let mut s = fresh_state();
        finalize_state(&mut s, &result_ok(0, 12), 5000, false, None);
        assert!(matches!(s.status, TurnStatus::Done));
        let stats = s.stats.as_ref().expect("stats recorded on success");
        assert_eq!(stats.cache_read_tokens, 12);
        assert_eq!(stats.duration_ms, 5000);
    }

    #[test]
    fn finalize_nonzero_exit_sets_failed_unless_cancelled() {
        let mut s = fresh_state();
        finalize_state(&mut s, &result_ok(1, 0), 3000, false, None);
        assert!(matches!(s.status, TurnStatus::Failed(_)));

        let mut s = fresh_state();
        finalize_state(&mut s, &result_ok(1, 0), 3000, true, None);
        assert!(matches!(s.status, TurnStatus::Stopped));
    }

    #[test]
    fn finalize_error_respects_existing_stopped_status() {
        let mut s = fresh_state();
        s.status = TurnStatus::Stopped;
        finalize_state(&mut s, &Err(eyre!("boom")), 3000, false, None);
        assert!(matches!(s.status, TurnStatus::Stopped));
        assert!(s.stats.is_none());
    }

    fn seed_repo(workspace: &Path, owner: &str, repo: &str) {
        let dot_git = workspace.join("repos").join(owner).join(repo).join(".git");
        std::fs::create_dir_all(&dot_git).unwrap();
    }

    fn expect_ok(outcome: ResolveOutcome) -> String {
        match outcome {
            ResolveOutcome::Ok(s) => s,
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    fn expect_not_cloned(outcome: ResolveOutcome) -> (String, String) {
        match outcome {
            ResolveOutcome::NotCloned { owner, repo } => (owner, repo),
            other => panic!("expected NotCloned, got {other:?}"),
        }
    }

    fn expect_unrecoverable(outcome: ResolveOutcome) -> String {
        match outcome {
            ResolveOutcome::Unrecoverable(s) => s,
            other => panic!("expected Unrecoverable, got {other:?}"),
        }
    }

    #[test]
    fn resolve_bare_name_with_exactly_one_match_expands_to_owner_slash_name() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");

        let resolved = expect_ok(resolve_chat_repo("workforce", temp.path()));
        assert_eq!(resolved, "driftwerk/workforce");
    }

    #[test]
    fn a_checkout_with_no_owner_is_not_bound_by_its_bare_name() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("repos/mono/.git")).unwrap();

        let err = expect_unrecoverable(resolve_chat_repo("mono", temp.path()));
        assert!(err.contains("not cloned"), "err: {err}");
    }

    #[test]
    fn an_ownerless_checkout_does_not_hide_the_owned_repo_of_the_same_name() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("repos/mono/.git")).unwrap();
        seed_repo(temp.path(), "driftwerk", "mono");

        let resolved = expect_ok(resolve_chat_repo("mono", temp.path()));
        assert_eq!(resolved, "driftwerk/mono");
    }

    #[test]
    fn a_repo_typed_in_another_case_binds_the_clone_as_it_is_spelled_on_disk() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");

        for typed in ["Driftwerk/Workforce", "WORKFORCE"] {
            let resolved = expect_ok(resolve_chat_repo(typed, temp.path()));
            assert_eq!(resolved, "driftwerk/workforce", "{typed}");
        }
    }

    #[test]
    fn resolve_bare_name_with_no_match_is_unrecoverable_and_lists_available() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "other");

        let err = expect_unrecoverable(resolve_chat_repo("workforce", temp.path()));
        assert!(err.contains("workforce"), "err: {err}");
        assert!(err.contains("not cloned"), "err: {err}");
        assert!(err.contains("driftwerk/other"), "err: {err}");
    }

    #[test]
    fn resolve_bare_name_with_no_match_and_empty_workspace_mentions_github_clone() {
        let temp = tempfile::TempDir::new().unwrap();

        let err = expect_unrecoverable(resolve_chat_repo("workforce", temp.path()));
        assert!(err.contains("no repos are available"), "err: {err}");
        assert!(err.contains("Ask an agent"), "err: {err}");
        assert!(err.contains("github.clone"), "err: {err}");
    }

    #[test]
    fn not_cloned_message_for_qualified_name_prefills_owner_and_repo() {
        let msg = not_cloned_message("driftwerk/workforce", &[]);
        assert!(msg.contains("owner=\"driftwerk\""), "msg: {msg}");
        assert!(msg.contains("repo=\"workforce\""), "msg: {msg}");
    }

    #[test]
    fn not_cloned_message_for_bare_name_keeps_placeholders() {
        let msg = not_cloned_message("workforce", &[]);
        assert!(msg.contains("owner=…"), "msg: {msg}");
        assert!(msg.contains("repo=…"), "msg: {msg}");
    }

    #[test]
    fn resolve_bare_name_ambiguous_is_unrecoverable_with_candidates() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");
        seed_repo(temp.path(), "other-org", "workforce");

        let err = expect_unrecoverable(resolve_chat_repo("workforce", temp.path()));
        assert!(err.contains("ambiguous"), "err: {err}");
        assert!(err.contains("driftwerk/workforce"), "err: {err}");
        assert!(err.contains("other-org/workforce"), "err: {err}");
    }

    #[test]
    fn resolve_owner_slash_name_with_clone_present_succeeds_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");

        let resolved = expect_ok(resolve_chat_repo("driftwerk/workforce", temp.path()));
        assert_eq!(resolved, "driftwerk/workforce");
    }

    #[test]
    fn resolve_qualified_missing_source_is_autoclonable() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "other");

        let (owner, repo) =
            expect_not_cloned(resolve_chat_repo("driftwerk/workforce", temp.path()));
        assert_eq!(owner, "driftwerk");
        assert_eq!(repo, "workforce");
    }

    #[test]
    fn resolve_qualified_missing_source_is_autoclonable_even_in_empty_workspace() {
        let temp = tempfile::TempDir::new().unwrap();

        let (owner, repo) =
            expect_not_cloned(resolve_chat_repo("driftwerk/workforce", temp.path()));
        assert_eq!(owner, "driftwerk");
        assert_eq!(repo, "workforce");
    }

    #[test]
    fn resolve_rejects_parent_traversal_as_unrecoverable() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");
        let escape = temp.path().parent().unwrap().join("escape");
        std::fs::create_dir_all(escape.join(".git")).unwrap();

        let err = expect_unrecoverable(resolve_chat_repo("../../escape", temp.path()));
        assert!(err.contains("not cloned"), "err: {err}");
    }

    #[test]
    fn resolve_rejects_absolute_paths_as_unrecoverable() {
        let temp = tempfile::TempDir::new().unwrap();
        seed_repo(temp.path(), "driftwerk", "workforce");

        let err = expect_unrecoverable(resolve_chat_repo("/etc/passwd", temp.path()));
        assert!(err.contains("not cloned"), "err: {err}");
    }

    #[test]
    fn resolve_rejects_empty_owner_or_name_as_unrecoverable() {
        let temp = tempfile::TempDir::new().unwrap();

        expect_unrecoverable(resolve_chat_repo("driftwerk/", temp.path()));
        expect_unrecoverable(resolve_chat_repo("/workforce", temp.path()));
    }

    #[test]
    fn resolve_rejects_dot_prefixed_segments_as_unrecoverable() {
        let temp = tempfile::TempDir::new().unwrap();

        expect_unrecoverable(resolve_chat_repo(".hidden/workforce", temp.path()));
        expect_unrecoverable(resolve_chat_repo("driftwerk/.hidden", temp.path()));
    }

    #[test]
    fn resolve_rejects_three_segment_paths_as_unrecoverable() {
        let temp = tempfile::TempDir::new().unwrap();

        expect_unrecoverable(resolve_chat_repo("a/b/c", temp.path()));
    }

    #[test]
    fn clone_failure_message_formats_owner_repo_and_detail() {
        let msg = clone_failure_message("driftwerk", "workforce", "boom");
        assert_eq!(msg, "Couldn't clone `driftwerk/workforce`: boom");
    }

    // ── pick_agent_core ──

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_agent_explicit_wins_when_known() {
        let agents = ids(&["allan", "architect"]);
        let out = pick_agent_core(Some("architect"), Some("allan"), &agents, "allan").unwrap();
        assert_eq!(out, "architect");
    }

    #[test]
    fn pick_agent_explicit_unknown_errors_with_listing() {
        let agents = ids(&["allan", "architect"]);
        let err = pick_agent_core(Some("ghost"), None, &agents, "allan").unwrap_err();
        assert!(err.contains("Unknown agent `@ghost`"), "err: {err}");
        assert!(err.contains("`@allan`"), "err: {err}");
        assert!(err.contains("`@architect`"), "err: {err}");
    }

    #[test]
    fn pick_agent_uses_routing_hint_when_known() {
        let agents = ids(&["allan", "architect"]);
        let out = pick_agent_core(None, Some("allan"), &agents, "architect").unwrap();
        assert_eq!(out, "allan");
    }

    #[test]
    fn pick_agent_ignores_stale_hint_and_falls_through_to_ambiguity() {
        let agents = ids(&["allan", "architect"]);
        // Routing pointed at a since-removed agent — treat as ambiguous rather
        // than silently defaulting.
        let err = pick_agent_core(None, Some("ghost"), &agents, "architect").unwrap_err();
        assert!(err.contains("isn't bound to an agent"), "err: {err}");
    }

    #[test]
    fn pick_agent_single_agent_is_never_ambiguous() {
        let agents = ids(&["only"]);
        let out = pick_agent_core(None, None, &agents, "only").unwrap();
        assert_eq!(out, "only");
    }

    #[test]
    fn pick_agent_multiple_agents_no_signal_errors() {
        let agents = ids(&["allan", "architect"]);
        let err = pick_agent_core(None, None, &agents, "allan").unwrap_err();
        assert!(err.contains("isn't bound to an agent"), "err: {err}");
        assert!(err.contains("/cc @<agent>"), "err: {err}");
        // Listing is stable-sorted so users see a consistent order.
        let allan_pos = err.find("`@allan`").unwrap();
        let arch_pos = err.find("`@architect`").unwrap();
        assert!(allan_pos < arch_pos, "err: {err}");
    }

    #[test]
    fn pick_agent_empty_registry_returns_default_id() {
        // Empty registry is surfaced later when vault resolution fails; this
        // function just shouldn't treat "no agents" as ambiguous.
        let agents: Vec<String> = Vec::new();
        let out = pick_agent_core(None, None, &agents, "default").unwrap();
        assert_eq!(out, "default");
    }

    const THREAD: &str = "slack:atlas:D0:1780980299.321109";
    const CANON: &str = "69116b82-6a40-4f24-8809-8c01b69a4d52";
    const RAW: &str = "U0FAKEUSER3";

    fn cc_session(store: &dyn SessionStore, owner: &str) -> Session {
        let mut s = store.get_or_create(THREAD, owner, PSEUDO_AGENT_ID).unwrap();
        s.mark_claude_chat("acme/widgets", owner, Some("atlas"));
        s.get_or_init_claude_uuid();
        store.save(&s).unwrap();
        s
    }

    #[test]
    fn probe_creates_nothing_when_thread_has_no_session() {
        let store = workforce_common::session::InMemorySessionStore::new();
        assert!(find_thread_session(&store, THREAD, CANON, RAW).is_none());
        assert!(
            store.list(None, 100).unwrap().is_empty(),
            "a read-only probe must not materialise a session"
        );
    }

    #[test]
    fn probe_resolves_canonical_then_legacy_binding() {
        let canonical = workforce_common::session::InMemorySessionStore::new();
        cc_session(&canonical, CANON);
        assert!(
            find_thread_session(&canonical, THREAD, CANON, RAW).is_some_and(|s| s.is_claude_chat())
        );

        let legacy = workforce_common::session::InMemorySessionStore::new();
        cc_session(&legacy, RAW);
        assert!(
            find_thread_session(&legacy, THREAD, CANON, RAW).is_some_and(|s| s.is_claude_chat()),
            "a not-yet-adopted raw-id binding is still routable"
        );
    }

    #[test]
    fn adopt_rekeys_legacy_binding_preserving_id_and_uuid_idempotently() {
        let store = workforce_common::session::InMemorySessionStore::new();
        let legacy = cc_session(&store, RAW);
        let uuid = legacy.claude_session_uuid().unwrap().to_string();

        // A plain message in the legacy thread: the read-only probe routes via
        // the raw-id binding without materialising anything.
        assert!(find_thread_session(&store, THREAD, CANON, RAW).is_some_and(|s| s.is_claude_chat()));
        assert_eq!(
            store.list(None, 100).unwrap().len(),
            1,
            "probe created nothing"
        );

        let adopted = adopt_thread_session(&store, THREAD, CANON, RAW).unwrap();
        assert_eq!(adopted.id, legacy.id, "same session, not a new one");
        assert_eq!(adopted.owner, CANON, "owner re-keyed to canonical");
        assert_eq!(
            adopted.label.as_deref(),
            Some(format!("{THREAD}:{CANON}").as_str())
        );
        assert_eq!(
            adopted.claude_session_uuid(),
            Some(uuid.as_str()),
            "resume handle kept"
        );
        assert!(adopted.is_claude_chat());
        assert_eq!(
            adopted.claude_chat_repo(),
            Some("acme/widgets"),
            "binding kept"
        );
        assert_eq!(
            adopted.claude_chat_agent_id(),
            Some("atlas"),
            "agent pin kept"
        );

        assert!(
            store.find(THREAD, RAW).unwrap().is_none(),
            "raw-id binding is gone"
        );
        assert_eq!(
            store.find(THREAD, CANON).unwrap().map(|s| s.id),
            Some(legacy.id.clone())
        );

        let again = adopt_thread_session(&store, THREAD, CANON, RAW).unwrap();
        assert_eq!(again.id, legacy.id, "second adoption is a no-op");
        assert_eq!(
            store.list(None, 100).unwrap().len(),
            1,
            "no twin ever created"
        );
    }

    #[test]
    fn adopt_creates_under_canonical_owner_when_no_binding() {
        let store = workforce_common::session::InMemorySessionStore::new();
        let created = adopt_thread_session(&store, THREAD, CANON, RAW).unwrap();
        assert_eq!(created.owner, CANON);
        assert!(store.find(THREAD, RAW).unwrap().is_none(), "no raw-id twin");
        assert_eq!(store.list(None, 100).unwrap().len(), 1);
    }
}

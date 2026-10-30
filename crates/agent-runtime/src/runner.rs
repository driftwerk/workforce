use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eyre::Result;
use tracing::{debug, info, warn};
use workforce_core::policy::{PolicyContext, PolicyRequest};
use workforce_core::types::AgentId;
pub use workforce_core::types::StreamDiscardReason;
use workforce_policy::engine::PolicyEngine;

use workforce_common::secret::{fixed_key, ApiKeySource};
use workforce_common::session::{estimate_tokens, CompactionConfig, ReservedContext};
use workforce_common::settings::ToolLoadingMode;
use workforce_common::subscription::Served;

use crate::compaction::CompactionEngine;
use crate::compile_gate::{with_notice, GateTurn, Verdict};
use crate::llm::anthropic::CACHE_BREAK_MARKER;
use crate::llm::failover::classify_error;
use crate::llm::prefix::PrefixFingerprint;
use crate::llm::provider::{with_cached_prefix, with_tool_deferral, LlmProvider};
use crate::llm::streaming::StreamEvent;
use crate::llm::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Loaded, Role, StopReason, TokenUsage,
};
use crate::tools::access::{self, ToolAccess, ToolAccessSnapshot};
use crate::tools::approval::{Decision, GatedCall, OpenPrompt, Refusal, TurnApprovals};
pub use crate::tools::dispatch::{apply_capability_gate, apply_cascade_gate, ToolGates};
use crate::tools::dispatch::{
    execute_approved, execute_gated, gate_tool_call, would_execute, GateOutcome, ToolExecution,
};
use crate::tools::loading::{PlanContext, ToolPlan};
use crate::tools::registry::ToolRegistry;
use crate::tools::ToolResult;
use crate::turn_journal::ToolCall;

const MAX_TOOL_HALLUCINATION_RETRIES: u32 = 2;

/// Bytes of round-opening text held back before any delta is forwarded — just
/// enough to see whether the round opens as tool-call-shaped JSON without
/// perceptible latency.
const STREAM_GATE_HOLD_BYTES: usize = 200;
/// Cadence (accumulated bytes) at which a suppressed suspect stream is
/// re-classified, keeping detection cost per-kilobyte rather than per-delta.
const STREAM_GATE_RECHECK_BYTES: usize = 1_024;
/// Ceiling on suppressed suspect text: past this a still-call-shaped suspect
/// is cut off rather than letting a runaway text tool call stream for
/// minutes; anything else is released in full — a late benign verdict may
/// delay an answer, never destroy it.
const STREAM_GATE_ABORT_BYTES: usize = 8_192;

/// Keys under which a text-form tool call may carry the tool name.
const TOOL_CALL_NAME_KEYS: [&str; 3] = ["name", "tool", "tool_name"];
/// Keys under which a text-form tool call may carry the input object.
const TOOL_CALL_INPUT_KEYS: [&str; 3] = ["input", "arguments", "params"];

/// Same-namespace alternatives offered alongside a "tool not found" error.
const TOOL_NOT_FOUND_SUGGESTIONS: usize = 5;

// ── Task-local session context ──────────────────────────────────────────────
// Propagated through JoinSet::spawn so that tools can resolve the current
// session without changing the `Tool::execute` trait signature.
tokio::task_local! {
    /// Set only on an approval-resume turn: the exact tool+args a human just
    /// approved. The hub's resume launcher scopes this around `process_message`
    /// so the runner pre-executes it before the first LLM round.
    pub static CURRENT_PREEXECUTE: PreExecutedTool;
    /// Current session key (e.g. "slack:workspace:C123:thread_ts").
    pub static CURRENT_SESSION_KEY: String;
    /// Current session ID (UUID).
    pub static CURRENT_SESSION_ID: String;
    /// Current user ID for per-user memory isolation (DW-147).
    pub static CURRENT_USER_ID: String;
    /// Identity charged for this turn's LLM spend when it differs from
    /// [`CURRENT_USER_ID`]. A scheduled job can run AS a human (credential
    /// and memory access) while BILLING a system identity, so background
    /// automation doesn't drain the human's personal budget allowance.
    /// Unset → spend bills [`CURRENT_USER_ID`] as before.
    pub static CURRENT_BILLING_IDENTITY: String;
    /// TTL to stamp on tables staged during this turn. Unset → the stager's
    /// default (short, reasoning-scratch); the edge scopes it to its configured
    /// artifact TTL so a browsable server-backed table lives as long as an export.
    pub static CURRENT_ARTIFACT_TTL: std::time::Duration;
    /// The store artifacts staged during this turn go to, when the turn's
    /// owner keeps one apart from the process-wide store: the edge scopes
    /// each tenant's store around its turns, so a spill or an export lands
    /// in that tenant's root. Unset → [`crate::artifacts::global_store`]'s
    /// process-wide store.
    pub static CURRENT_ARTIFACT_STORE: Arc<dyn crate::artifacts::ArtifactStore>;
    /// Scope an artifact staged during this turn belongs to, overriding the
    /// session id. A workflow sets it to its instance so a later step can read
    /// what an earlier one staged; every other caller leaves it unset and stays
    /// scoped to its own session.
    pub static CURRENT_ARTIFACT_SCOPE: String;
    /// The requesting user's PLATFORM-native id (e.g. their Slack `U…` id),
    /// resolved from the identity's `linked_ids` at turn setup. Distinct from
    /// [`CURRENT_USER_ID`], which is the internal identity UUID. Used to verify
    /// the requesting human's access to a channel (not just the bot's) before a
    /// cross-channel read. Empty/absent when unknown (web/non-Slack/unresolved),
    /// which callers MUST treat as fail-closed.
    pub static CURRENT_PLATFORM_USER_ID: String;
    /// The voice-bridge channel this turn was SPOKEN on (e.g. "voice-pe"), for
    /// turns that came from a satellite. Empty for every other surface,
    /// including a spoken iOS turn — that is a screen the reply is already
    /// delivered to.
    ///
    /// Carried per turn rather than read back off the session: a WebSocket
    /// connection anchors on a session shared by every surface the same person
    /// uses with the same agent, so the session's latched voice flag answers
    /// "was one of my turns spoken", not "was THIS turn spoken".
    pub static CURRENT_VOICE_CHANNEL: String;
    /// The automated source (`heartbeat`, `monitor`, `scheduler`,
    /// `workflow`, …) whose turn the current work descends from, where its
    /// session no longer says so: a sub-agent such a turn spawned runs in a
    /// session of its own. Unset for work a person started.
    pub static CURRENT_AUTOMATED_ORIGIN: String;
    /// Current Slack channel ID (e.g. "C123" or "D456") — set during agent runs
    /// so tools can resolve channel context without needing a scheduler.
    pub static CURRENT_CHANNEL_ID: String;
    /// Current Slack thread timestamp (e.g. "1234567890.123456") — set during
    /// agent runs so tools can resolve thread context without needing a scheduler.
    pub static CURRENT_THREAD_ID: String;
    /// The Desk conversation the current run answers to: a live Desk turn's,
    /// attended, or the one a job, workflow or heartbeat check made from Desk
    /// was stamped with, unattended. Unset everywhere else, Slack included.
    pub static CURRENT_DESK: workforce_common::channels::desk::DeskScope;
    /// Current agent ID — set during agent runs so tools (e.g., WASM skill
    /// tools) can identify which agent invoked them.
    pub static CURRENT_AGENT_ID: String;
    /// Current Slack workspace_id — set during agent runs so messaging tools
    /// can route to the correct bot identity without the agent needing to
    /// supply it.
    pub static CURRENT_WORKSPACE_ID: String;
    /// Live progress sender for the current turn, re-applied around tool
    /// execution (see the JoinSet spawn in `run_with_progress_and_history`) so a
    /// long-running tool — the advisor, `coding.session` — can surface its
    /// nested activity on the turn's status line via [`ProgressEvent::SubActivity`].
    /// Absent in tests, background runs, and surfaces without a progress sink;
    /// resolve it with [`current_progress_tx`].
    pub static CURRENT_PROGRESS_TX: tokio::sync::mpsc::Sender<ProgressEvent>;
    /// The self-serve approval ledger of the turn a tool runs in, scoped
    /// wherever [`CURRENT_PROGRESS_TX`] is, so a nested surface — a coding
    /// round's MCP calls — puts its prompts on the same turn's card and hears
    /// the same clicks. Absent where nobody can answer a prompt.
    pub static CURRENT_TURN_APPROVALS: Arc<TurnApprovals>;
    /// When the human last heard something durable this turn, shared between the
    /// round loop and [`execute_with_deadline`] so the two cannot both post a
    /// check-in for the same silence. The loop owns the `Arc` and hands it to
    /// each spawned tool task via [`SessionContext::with_user_comm`]; absent
    /// outside a progress-bearing turn, where the heartbeat is inert.
    ///
    /// A `tokio` instant, not a `std` one: it is compared against waits driven by
    /// `tokio::time::sleep`, and only tokio's clock keeps the two agreeing when a
    /// test pauses time.
    pub static LAST_USER_COMM: Arc<Mutex<tokio::time::Instant>>;
    /// Set when the run's tool registry is already curated to a deliberate
    /// subset (e.g. a research-worker sub-agent). Such a run offers every tool
    /// in full: a catalogue would only add a load step to a list someone
    /// already chose. See [`tools_precurated`].
    pub static PRECURATED_TOOLSET: bool;
    /// Set when the caller — not the agent — owns outbound delivery for this
    /// turn (e.g. a monitor auto-delivers the agent's final text to a fixed
    /// channel). Strips the agent's outbound delivery tools from the turn's
    /// registry so it can't self-post a duplicate, or a chatty "Done — posted
    /// to …" confirmation, alongside the caller-delivered reply. See
    /// [`delivery_tools_suppressed`].
    pub static SUPPRESS_DELIVERY_TOOLS: bool;
    /// The tools an automated turn declared it needs (a heartbeat check's or
    /// a scheduled job's `tools`). The turn's registry keeps only those, by
    /// name or namespace, plus retrieval. See [`tool_whitelist`].
    pub static TOOL_WHITELIST: ToolWhitelist;
    /// Per-session active environment overrides for gated MCP workspaces,
    /// keyed `workspace -> env`. Populated each turn from the session's
    /// persisted `mcp_active_envs` (set by a human `cmd: use <ws> <env>`),
    /// and read by the tool filter + the MCP call guard to decide which
    /// workspace member is reachable. Deliberately NOT scoped inside
    /// `run_isolated`, so a spawned sub-agent / research worker does not
    /// inherit a human's session override — it falls back to the workspace
    /// default (or dormant), never silently carrying staging into a
    /// sub-investigation. See [`current_active_mcp_envs`].
    pub static ACTIVE_MCP_ENVS: std::collections::HashMap<String, String>;
    /// Set by the games surface so a normal work agent can play a turn fully
    /// sandboxed — no tools, and no passive-memory write — without needing a
    /// separate locked-down agent config. See [`game_sandboxed`].
    pub static GAME_SANDBOX: bool;
    /// Name of the active environment for this turn (`[environments]`), or empty
    /// when none is active. Populated each turn from the session's override
    /// (`cmd: env use`) or the global default env; reset to the default env for
    /// spawned sub-agents and background runs. Read by [`current_active_environment`]
    /// — its presence is what engages the skill-instance lock. See
    /// [`crate::llm::environment::resolve_instance`].
    pub static ACTIVE_ENVIRONMENT: String;
    /// The active environment's skill bindings (`skill -> instances`, first is
    /// the default), empty when no environment is active. Read by
    /// [`crate::llm::environment::resolve_instance`] to hold a bound skill to its
    /// set. Threaded separately from the `scope_layers` array like
    /// [`ACTIVE_MCP_ENVS`]. See [`current_active_skill_instances`].
    pub static ACTIVE_SKILL_INSTANCES: crate::llm::environment::SkillPins;
    /// Tools this conversation used or loaded in its recent turns, which a
    /// turn that loads on demand keeps resident. Derived from the stored
    /// transcript by the hub each turn; absent scope (sub-agents, workflow
    /// steps, tests) reads as empty. See [`session_tools`].
    pub static SESSION_TOOLS: Arc<BTreeSet<String>>;
    /// What paid for the LLM calls in this scope. See [`ServedLog`].
    pub static CURRENT_SERVED: ServedLog;
}

/// The subscription that paid for the last LLM call in one scope (a turn or
/// one helper call), carried in [`SessionContext`] so it crosses the
/// streaming spawn.
#[derive(Clone, Debug, Default)]
pub struct ServedLog(Arc<parking_lot::Mutex<Option<Served>>>);

impl ServedLog {
    pub fn note(&self, served: Served) {
        self.set(Some(served));
    }

    fn set(&self, served: Option<Served>) {
        *self.0.lock() = served;
    }

    pub fn served(&self) -> Option<Served> {
        self.0.lock().clone()
    }

    /// Run `fut` with this log as the current one.
    pub async fn scope<F: std::future::Future>(self, fut: F) -> F::Output {
        CURRENT_SERVED.scope(self, fut).await
    }
}

/// Note a subscription-served call in the current scope's log. Outside any
/// log there is no cost record to tell, so nothing is kept.
pub fn note_served(served: Served) {
    let _ = CURRENT_SERVED.try_with(|log| log.note(served));
}

/// Run `fut` under a fresh [`ServedLog`] and return what it noted, so a cost
/// record reads only its own call's route and not an enclosing turn's.
pub async fn with_served_log<F: std::future::Future>(fut: F) -> (F::Output, Option<Served>) {
    let log = ServedLog::default();
    let output = log.clone().scope(fut).await;
    (output, log.served())
}

/// Run one call under its own log and, when it succeeds, make what it noted
/// the enclosing scope's — nothing noted leaves the scope metered.
async fn as_last_call<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let (output, served) = with_served_log(fut).await;
    if output.is_ok() {
        let _ = CURRENT_SERVED.try_with(|log| log.set(served));
    }
    output
}

/// Makes the last successful call decide what the scope is billed as, so a
/// turn mixing providers is billed by its final call.
struct LastCallServed {
    inner: Arc<dyn LlmProvider>,
}

impl LastCallServed {
    fn wrap(inner: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
        Arc::new(Self { inner })
    }
}

#[async_trait::async_trait]
impl LlmProvider for LastCallServed {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        as_last_call(self.inner.create_message(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
        ))
        .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        as_last_call(self.inner.create_message_stream(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
            tx,
        ))
        .await
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.inner.context_floor(model).await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.inner.verify_credentials().await
    }
}

/// The tools this conversation used or loaded recently. Absent scope reads as
/// empty. See [`SESSION_TOOLS`].
pub fn session_tools() -> Arc<BTreeSet<String>> {
    SESSION_TOOLS.try_with(Arc::clone).unwrap_or_default()
}

/// The current turn's gated-MCP-workspace env overrides (`workspace -> env`).
/// Absent scope — ordinary runs with no `cmd: use` active, and every spawned
/// sub-agent — reads as empty, so only the workspace `default` (if any) is
/// auto-active. See [`ACTIVE_MCP_ENVS`].
pub fn current_active_mcp_envs() -> std::collections::HashMap<String, String> {
    ACTIVE_MCP_ENVS.try_with(|m| m.clone()).unwrap_or_default()
}

/// The active environment name for this turn, or `None` when none is active
/// (the skill-instance lock is disengaged). Absent scope and an empty value both
/// read as `None`. See [`ACTIVE_ENVIRONMENT`].
pub fn current_active_environment() -> Option<String> {
    ACTIVE_ENVIRONMENT
        .try_with(|s| s.clone())
        .ok()
        .filter(|s| !s.is_empty())
}

/// The active environment's skill bindings for this turn. Absent scope —
/// ordinary runs with no environment active — reads as empty. See
/// [`ACTIVE_SKILL_INSTANCES`].
pub fn current_active_skill_instances() -> crate::llm::environment::SkillPins {
    ACTIVE_SKILL_INSTANCES
        .try_with(|m| m.clone())
        .unwrap_or_default()
}

/// Whether the current run's tool registry is pre-curated and must bypass
/// context-window catalogue filtering (see [`PRECURATED_TOOLSET`]). Absent
/// scope — ordinary interactive turns — reads as `false`.
pub fn tools_precurated() -> bool {
    PRECURATED_TOOLSET.try_with(|&v| v).unwrap_or(false)
}

/// Whether the current run must route all outbound delivery through the caller
/// rather than the agent's own messaging tools (see [`SUPPRESS_DELIVERY_TOOLS`]).
/// Absent scope — ordinary interactive turns — reads as `false`.
pub fn delivery_tools_suppressed() -> bool {
    SUPPRESS_DELIVERY_TOOLS.try_with(|&v| v).unwrap_or(false)
}

/// The tools an automated turn declared, and whether the session it runs in
/// was minted for that run alone.
#[derive(Debug, Clone)]
pub struct ToolWhitelist {
    pub tools: Arc<[String]>,
    /// The session is the run's own, so a turn resumed on it later keeps
    /// these tools. A shared or pinned session never does.
    pub owns_session: bool,
}

impl ToolWhitelist {
    /// `tools` for a turn in a session the run shares with other work.
    pub fn for_turn(tools: &[String]) -> Self {
        Self {
            tools: tools.into(),
            owns_session: false,
        }
    }

    /// `tools` for a turn in a session minted for this run alone.
    pub fn for_own_session(tools: &[String]) -> Self {
        Self {
            tools: tools.into(),
            owns_session: true,
        }
    }
}

/// The tool whitelist the current turn declared, if any (see
/// [`TOOL_WHITELIST`]). Absent scope reads as `None`: the full surface.
pub fn tool_whitelist() -> Option<ToolWhitelist> {
    TOOL_WHITELIST.try_with(ToolWhitelist::clone).ok()
}

/// Run `fut` with `whitelist` as the turn's declared tools, or unchanged
/// when there is none.
pub async fn with_tool_whitelist<F: std::future::Future>(
    whitelist: Option<ToolWhitelist>,
    fut: F,
) -> F::Output {
    match whitelist {
        Some(tools) => TOOL_WHITELIST.scope(tools, fut).await,
        None => fut.await,
    }
}

/// Whether the current turn is a sandboxed game turn — it runs with no tools and
/// writes no passive memory, letting a normal work agent play without a separate
/// locked-down config. Absent scope reads as `false`. See [`GAME_SANDBOX`].
pub fn game_sandboxed() -> bool {
    GAME_SANDBOX.try_with(|&v| v).unwrap_or(false)
}

/// Tool names that deliver something outbound to a human — the channels through
/// which an agent could post a duplicate when a caller already owns delivery.
/// Read-only messaging tools (`messaging.read`, `messaging.member_info`, …) are
/// deliberately excluded: a monitor turn still needs them to resolve names and
/// gather context. Used to filter the registry under [`SUPPRESS_DELIVERY_TOOLS`].
pub fn is_outbound_delivery_tool(name: &str) -> bool {
    matches!(
        name,
        "messaging.send"
            | "messaging.send_file"
            | "messaging.upload_file"
            | "email.send"
            | "email.forward"
            | "postmark.email_send"
            | "postmark.email_send_batch"
            | "postmark.email_send_template"
            | "sendgrid.email_send"
            | "sendgrid.email_send_batch"
            | "browser.upload_file"
    )
}

/// Return a copy of `registry` with every outbound delivery tool removed (see
/// [`is_outbound_delivery_tool`]). Used for turns where the caller owns delivery
/// and the agent must not self-post (see [`SUPPRESS_DELIVERY_TOOLS`]). The shared
/// registry is left untouched — `clone_all` shallow-clones the tool `Arc`s.
pub fn without_delivery_tools(
    registry: &crate::tools::registry::ToolRegistry,
) -> crate::tools::registry::ToolRegistry {
    let filtered = registry.clone_all();
    for name in filtered.names() {
        if is_outbound_delivery_tool(&name) {
            filtered.unregister(&name);
        }
    }
    filtered
}

/// Clone of the current turn's progress sender, if one is in scope. Tools call
/// this from inside `execute()` to stream nested sub-activity (see
/// [`ProgressEvent::SubActivity`]). Returns `None` outside an agent turn (tests,
/// background work) or on surfaces with no progress rendering — callers must
/// treat the no-op case as normal.
pub fn current_progress_tx() -> Option<tokio::sync::mpsc::Sender<ProgressEvent>> {
    CURRENT_PROGRESS_TX.try_with(|t| t.clone()).ok()
}

/// The Desk conversation the current run answers to, if it answers to one.
pub fn current_desk() -> Option<workforce_common::channels::desk::DeskScope> {
    CURRENT_DESK.try_with(Clone::clone).ok()
}

/// Run `fut` answering to `desk`, or as it is when there is none.
pub async fn with_desk<F: std::future::Future>(
    desk: Option<workforce_common::channels::desk::DeskScope>,
    fut: F,
) -> F::Output {
    match desk {
        Some(desk) => CURRENT_DESK.scope(desk, fut).await,
        None => fut.await,
    }
}

/// The approval ledger of the turn a tool runs in; `None` where nobody can
/// answer a prompt.
pub fn current_turn_approvals() -> Option<Arc<TurnApprovals>> {
    CURRENT_TURN_APPROVALS.try_with(Arc::clone).ok()
}

/// Run `fut` as a tool of the turn whose progress sink and approval ledger
/// these are, so nested activity reaches that turn's card and its prompts
/// that turn's clicks. Either may be absent.
pub async fn scope_turn<F: std::future::Future>(
    progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    approvals: Option<Arc<TurnApprovals>>,
    fut: F,
) -> F::Output {
    let fut = async move {
        match approvals {
            Some(ledger) => CURRENT_TURN_APPROVALS.scope(ledger, fut).await,
            None => fut.await,
        }
    };
    match progress {
        Some(tx) => CURRENT_PROGRESS_TX.scope(tx, fut).await,
        None => fut.await,
    }
}

/// Execute a tool while enforcing its optional [`crate::tools::Tool::soft_deadline`].
/// On overrun the in-flight future is dropped — which kills any `kill_on_drop`
/// child process the tool spawned — and a structured failure is returned to the
/// model so control returns to the loop instead of the turn freezing on a single
/// runaway call (the 700s `code.search` incident). Tools that declare no deadline
/// run unbounded, exactly as before.
pub(crate) async fn execute_with_deadline(
    tool: &Arc<dyn crate::tools::Tool>,
    input: serde_json::Value,
) -> eyre::Result<crate::tools::ToolResult> {
    let run = async {
        match tool.soft_deadline() {
            Some(deadline) => match tokio::time::timeout(deadline, tool.execute(input)).await {
                Ok(res) => res,
                Err(_) => Ok(crate::tools::ToolResult::failure(
                    format!(
                        "Tool `{}` exceeded its {}s time budget and was cancelled. The request \
                         was too broad to finish in one pass — narrow it (a more specific pattern, \
                         path, or filter) and try again, or run it as a background job so it doesn't \
                         block the conversation.",
                        tool.name(),
                        deadline.as_secs()
                    ),
                    deadline.as_millis() as u64,
                )),
            },
            None => tool.execute(input).await,
        }
    };
    tokio::pin!(run);
    let started = tokio::time::Instant::now();
    loop {
        tokio::select! {
            result = &mut run => return result,
            _ = tokio::time::sleep(heartbeat_due_in(started)) => {
                // A round runs its tools concurrently against one shared clock,
                // so several can wake for the same silence; whoever gets here
                // first resets it and the rest fall through. Re-reading is what
                // keeps it to one card per interval rather than one per tool.
                if heartbeat_due_in(started) > Duration::ZERO {
                    continue;
                }
                // Advance BEFORE trying to speak, and whether or not speaking
                // works. A tick that left the clock untouched — no progress
                // sink, or a consumer that has already gone — makes the next
                // `check_in_due_in()` return zero again and spins this loop hot
                // for the rest of the tool call.
                mark_user_comm();
                emit_tool_check_in(tool.name(), started.elapsed()).await;
            }
        }
    }
}

/// Far enough out to never fire, without handing `sleep` a value near the end of
/// time. Used when there is no turn in scope, so nothing is waiting on it.
const NEVER: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// How long until the turn's silence budget is next exhausted. Reads the shared
/// clock so a check-in the round loop just posted delays this one too; without a
/// turn in scope it never comes due.
fn check_in_due_in() -> Duration {
    LAST_USER_COMM
        .try_with(|c| {
            let elapsed = lock_instant(c).elapsed();
            CHECK_IN_INTERVAL.saturating_sub(elapsed)
        })
        .unwrap_or(NEVER)
}

/// When this call may next announce itself: not before the turn's silence
/// budget is spent, and not before THIS call has itself been running that long.
///
/// The second half matters. Without it a 1-second tool that happens to start
/// just as the budget expires posts "still working… for 1s" and — because it
/// resets the shared clock — takes the slot from the round-boundary check-in,
/// which carries the round tally the human actually wants.
fn heartbeat_due_in(started: tokio::time::Instant) -> Duration {
    let own = CHECK_IN_INTERVAL.saturating_sub(started.elapsed());
    check_in_due_in().max(own)
}

/// Record that the human has just been spoken to. No-op outside a turn.
fn mark_user_comm() {
    LAST_USER_COMM
        .try_with(|c| *lock_instant(c) = tokio::time::Instant::now())
        .ok();
}

fn lock_instant(
    cell: &Mutex<tokio::time::Instant>,
) -> std::sync::MutexGuard<'_, tokio::time::Instant> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Tell the human a single tool call is still running.
///
/// The round loop's check-in cannot cover this: one `coding.session` is one
/// round, so a 20-minute call sits inside a single iteration and the loop never
/// gets to look. Emitting the same [`ProgressEvent::CheckIn`] from here is what
/// puts a Pause button on screen during that window — previously the only way to
/// stop such a turn was to send another message, which discarded its work.
async fn emit_tool_check_in(tool: &str, elapsed: Duration) {
    let Some(tx) = current_progress_tx() else {
        return;
    };
    let note = format!(
        "⏳ *Still working* — `{}` has been running for {}. \
         Tap *Pause* (or reply here) if you want me to stop.",
        tool,
        humanize_elapsed(elapsed)
    );
    let _ = tx.send(ProgressEvent::CheckIn { note }).await;
}

/// `45s` / `3m 05s` / `1h 30m`. Matches the hub's `human_dur`, and is used by
/// every elapsed the runner renders, so one turn never shows the human three
/// different spellings of the same duration.
fn humanize_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Maximum time a turn may run without saying anything durable to the human.
///
/// Two emitters share it. The round loop posts a [`ProgressEvent::CheckIn`] with
/// the round tally at a round boundary; a single tool call that outlasts the
/// interval posts a thinner one from [`execute_with_deadline`], which is what
/// reaches the human during a call that never returns to the loop. Both reset
/// the same clock, so silence never exceeds roughly this interval plus one
/// in-flight LLM call.
///
/// Must stay above the leaf-tool [`crate::tools::Tool::soft_deadline`]: that is
/// what guarantees a deadlined tool can never outlive one interval, and so why
/// the heartbeat needs no special case for it.
pub const CHECK_IN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(180);

/// A concrete, zero-cost progress note for a periodic check-in: how long we've
/// been at it, the round, and a tally of the tools used so far. Deterministic on
/// purpose — it reports what actually happened rather than a model's gloss, and
/// never adds latency or token cost to a turn that's already running long. (A
/// richer model-authored "here's the picture so far" note is a later upgrade to
/// this one function.)
fn build_checkin_note(
    rounds: u32,
    elapsed: std::time::Duration,
    invocations: &[ToolInvocation],
) -> String {
    let when = humanize_elapsed(elapsed);
    let mut tally: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for inv in invocations {
        *tally.entry(inv.tool_name.as_str()).or_default() += 1;
    }
    let tools = if tally.is_empty() {
        "still thinking it through — no tools yet".to_string()
    } else {
        tally
            .iter()
            .map(|(name, count)| {
                if *count > 1 {
                    format!("`{name}` ×{count}")
                } else {
                    format!("`{name}`")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "⏳ *Still working* — {when} in, round {rounds}.\nSo far: {tools}.\n\
         I'll keep going and report back. Tap *Pause* (or reply here) if you want me to stop."
    )
}

/// Rewrite a live check-in note (see [`build_checkin_note`]) into a settled
/// record for after the run is over. The elapsed/round/tools are retained — so
/// the human can still see roughly how long it took — but the "Still working"
/// framing becomes a neutral, factual time stamp and the now-defunct Pause
/// invite is dropped (the surface strips the Pause button alongside this). The
/// label is deliberately outcome-agnostic: settling happens on natural
/// completion, a steer-stop, and an error alike, so it must not claim success.
/// The elapsed figure is the last check-in's, i.e. a lower bound on the total.
pub fn settle_checkin_note(note: &str) -> String {
    note.lines()
        .filter(|line| !line.starts_with("I'll keep going"))
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("⏳ *Still working* —", "⏱️ *Last check-in* —", 1)
}

/// Ground a check-in note in the last thing the human could have seen the
/// long-running tool do, so a card saying a call has run for twenty minutes
/// also says what it was last observed doing. `None` leaves the note as is.
pub fn checkin_note_with_activity(note: &str, last_activity: Option<&str>) -> String {
    match last_activity.map(str::trim).filter(|a| !a.is_empty()) {
        Some(activity) => format!("{note}\nLast seen: {activity}"),
        None => note.to_string(),
    }
}

/// Captured session context for propagation into spawned tasks.
#[derive(Clone, Default)]
pub struct SessionContext {
    pub key: Option<String>,
    pub id: Option<String>,
    pub user_id: Option<String>,
    /// The explicit billing identity ([`CURRENT_BILLING_IDENTITY`]) only —
    /// absent, spend falls back to [`Self::user_id`] inside the scope.
    pub billing_identity: Option<String>,
    pub platform_user_id: Option<String>,
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
    pub agent_id: Option<String>,
    pub workspace_id: Option<String>,
    /// The voice-bridge channel this turn was spoken on. Carried across the
    /// tool-exec spawn like the other `CURRENT_*` locals — the scheduler and
    /// reminder tools run in that spawned task, and stamping a job's delivery
    /// target is exactly what they need it for.
    pub voice_channel: Option<String>,
    /// [`CURRENT_AUTOMATED_ORIGIN`], carried so a job the scheduler tool
    /// creates in the spawned tool task records the automation behind it.
    pub automated_origin: Option<String>,
    /// Active gated-MCP-workspace env overrides (`workspace -> env`). Carried
    /// across the tool-exec `JoinSet::spawn` alongside the `CURRENT_*` locals so
    /// the MCP call guard and `mcp.workspace_*` tools — which run in that
    /// spawned task — see the session's `cmd: use` activation. A `HashMap`, not
    /// a `String`, so it's threaded separately from the `scope_layers` array.
    pub active_mcp_envs: std::collections::HashMap<String, String>,
    /// Active environment name (`[environments]`), carried across the tool-exec
    /// spawn so the skill `resolve_instance` lock — which runs in that task —
    /// sees the turn's environment. Rides the `scope_layers` array (a `String`).
    pub active_environment: Option<String>,
    /// The active environment's skill bindings, carried alongside
    /// [`Self::active_mcp_envs`] so `resolve_instance` can read them inside the
    /// spawned tool task. A `HashMap`, threaded separately from `scope_layers`.
    pub active_skill_instances: crate::llm::environment::SkillPins,
    /// The turn's staged-table TTL, carried across the spawn like the HashMap
    /// locals (it is a `Duration`, not a `String`, so it rides outside
    /// `scope_layers`) so a `data.query` in a spawned tool task stages its tables
    /// for the same duration. Absent → the stager's scratch default.
    pub artifact_ttl: Option<std::time::Duration>,
    /// The turn's own artifact store ([`CURRENT_ARTIFACT_STORE`]), carried
    /// across the spawn so a tool task stages into the same root.
    pub artifact_store: Option<Arc<dyn crate::artifacts::ArtifactStore>>,
    pub artifact_scope: Option<String>,
    /// The turn's silence clock, carried across the spawn like the other
    /// non-`String` locals so a long tool call can post its own check-in — and
    /// therefore a Pause button — from inside the spawned task.
    pub last_user_comm: Option<Arc<Mutex<tokio::time::Instant>>>,
    /// The workflow run's secret policy, carried across the tool-exec spawn
    /// because the sinks that resolve `[[vault:<scope>:<key>]]` markers —
    /// `browser.fill`, `http.request` — run in that spawned task. Without it
    /// `resolve_secret_markers` sees no policy, reads that as "not a workflow
    /// run", and hands the marker to the legacy per-skill resolver, which
    /// rejects the scope-qualified key as malformed. `None` outside a workflow
    /// run, which is the interactive case the legacy resolver is for.
    pub secret_policy: Option<workforce_common::secret_policy::SecretPolicy>,
    /// The enclosing scope's [`ServedLog`], so a call made in the spawned
    /// task is billed on the record that scope writes.
    pub served: Option<ServedLog>,
    /// [`CURRENT_DESK`], carried so a tool in the spawned task delivers to,
    /// and is gated by, the Desk conversation the run answers to.
    pub desk: Option<workforce_common::channels::desk::DeskScope>,
}

fn task_local_value(
    key: &'static tokio::task::LocalKey<String>,
    keep_empty: bool,
) -> Option<String> {
    let value = key.try_with(|s| s.clone()).ok();
    if keep_empty {
        value
    } else {
        value.filter(|s| !s.is_empty())
    }
}

impl SessionContext {
    /// Attach the turn's silence clock. The round loop owns it directly, so it
    /// is handed over rather than read from a task-local.
    pub fn with_user_comm(mut self, clock: &Arc<Mutex<tokio::time::Instant>>) -> Self {
        self.last_user_comm = Some(Arc::clone(clock));
        self
    }

    /// Capture the current task-local session context.
    /// Must be called in the parent task (before `JoinSet::spawn`).
    pub fn capture() -> Self {
        Self {
            key: task_local_value(&CURRENT_SESSION_KEY, true),
            id: task_local_value(&CURRENT_SESSION_ID, true),
            user_id: task_local_value(&CURRENT_USER_ID, true),
            billing_identity: task_local_value(&CURRENT_BILLING_IDENTITY, false),
            platform_user_id: task_local_value(&CURRENT_PLATFORM_USER_ID, true),
            channel_id: task_local_value(&CURRENT_CHANNEL_ID, false),
            thread_id: task_local_value(&CURRENT_THREAD_ID, false),
            agent_id: task_local_value(&CURRENT_AGENT_ID, false),
            workspace_id: task_local_value(&CURRENT_WORKSPACE_ID, false),
            voice_channel: task_local_value(&CURRENT_VOICE_CHANNEL, false),
            automated_origin: task_local_value(&CURRENT_AUTOMATED_ORIGIN, false),
            active_mcp_envs: current_active_mcp_envs(),
            active_environment: current_active_environment(),
            active_skill_instances: current_active_skill_instances(),
            artifact_ttl: CURRENT_ARTIFACT_TTL.try_with(|ttl| *ttl).ok(),
            artifact_store: CURRENT_ARTIFACT_STORE.try_with(Arc::clone).ok(),
            artifact_scope: task_local_value(&CURRENT_ARTIFACT_SCOPE, false),
            // Picked up when a nested capture happens inside a tool task; the
            // round loop hands its own in with `with_user_comm`.
            last_user_comm: LAST_USER_COMM.try_with(Arc::clone).ok(),
            secret_policy: workforce_common::secret_policy::capture(),
            served: CURRENT_SERVED.try_with(ServedLog::clone).ok(),
            desk: current_desk(),
        }
    }

    /// `tokio::spawn` `fut` with this context applied.
    pub fn spawn<F>(self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(self.scope(fut))
    }

    /// Run a future with the captured session context re-applied as task-locals.
    pub async fn scope<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T> + Send,
        T: Send,
    {
        let active_mcp_envs = self.active_mcp_envs;
        let active_skill_instances = self.active_skill_instances;
        let artifact_ttl = self.artifact_ttl;
        let artifact_store = self.artifact_store;
        let last_user_comm = self.last_user_comm;
        let secret_policy = self.secret_policy;
        let served = self.served;
        let desk = self.desk;
        let layers: [(&'static tokio::task::LocalKey<String>, Option<String>, bool); 13] = [
            (&CURRENT_ARTIFACT_SCOPE, self.artifact_scope, false),
            (&CURRENT_AGENT_ID, self.agent_id, true),
            (&CURRENT_CHANNEL_ID, self.channel_id, true),
            (&CURRENT_THREAD_ID, self.thread_id, true),
            (&CURRENT_WORKSPACE_ID, self.workspace_id, true),
            (&CURRENT_SESSION_KEY, self.key, false),
            (&CURRENT_SESSION_ID, self.id, false),
            (&CURRENT_USER_ID, self.user_id, false),
            (&CURRENT_BILLING_IDENTITY, self.billing_identity, false),
            (&CURRENT_PLATFORM_USER_ID, self.platform_user_id, false),
            (&ACTIVE_ENVIRONMENT, self.active_environment, true),
            (&CURRENT_VOICE_CHANNEL, self.voice_channel, false),
            (&CURRENT_AUTOMATED_ORIGIN, self.automated_origin, false),
        ];
        // The HashMap- and Duration-typed locals ride outside the String-typed
        // `scope_layers` array. The maps are always scoped (empty = default); the
        // TTL only when captured (absent = the stager's scratch default).
        let inner = ACTIVE_MCP_ENVS.scope(
            active_mcp_envs,
            ACTIVE_SKILL_INSTANCES.scope(
                active_skill_instances,
                scope_layers(fut, layers.into_iter()),
            ),
        );
        let inner = async {
            match artifact_ttl {
                Some(ttl) => CURRENT_ARTIFACT_TTL.scope(ttl, inner).await,
                None => inner.await,
            }
        };
        // Boxed here: each layer below nests the whole future once more, and
        // the unboxed stack of them overflowed a worker thread in the edge's
        // tool-round tests.
        let inner: std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + '_>> =
            Box::pin(async {
                match artifact_store {
                    Some(store) => CURRENT_ARTIFACT_STORE.scope(store, inner).await,
                    None => inner.await,
                }
            });
        // Matched rather than run through `scope_if_present`, whose boxing would
        // impose a `'static` bound this future does not carry.
        let inner = async {
            match secret_policy {
                Some(policy) => {
                    workforce_common::secret_policy::SECRET_POLICY
                        .scope(policy, inner)
                        .await
                }
                None => inner.await,
            }
        };
        let inner = async {
            match served {
                Some(log) => log.scope(inner).await,
                None => inner.await,
            }
        };
        let inner = with_desk(desk, inner);
        match last_user_comm {
            Some(clock) => LAST_USER_COMM.scope(clock, inner).await,
            None => inner.await,
        }
    }
}

async fn scope_layers<F, T, I>(fut: F, layers: I) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
    I: IntoIterator<Item = (&'static tokio::task::LocalKey<String>, Option<String>, bool)>,
{
    let mut boxed: std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>> = Box::pin(fut);
    for (key, value, with_sentinel) in layers {
        boxed = match value {
            Some(v) => Box::pin(key.scope(v, boxed)),
            None if with_sentinel => Box::pin(key.scope(String::new(), boxed)),
            None => boxed,
        };
    }
    boxed.await
}

impl SessionContext {
    /// Run a closure with the captured context re-applied via `sync_scope`.
    /// Task-locals do not cross `spawn_blocking` — wrap blocking bodies that
    /// stage artifacts or resolve credentials in this (bulk-xlsx design §1).
    pub fn sync_scope<T>(self, f: impl FnOnce() -> T) -> T {
        let active_mcp_envs = self.active_mcp_envs;
        let active_skill_instances = self.active_skill_instances;
        let artifact_ttl = self.artifact_ttl;
        let artifact_store = self.artifact_store;
        let secret_policy = self.secret_policy;
        let served = self.served;
        let desk = self.desk;
        let layers: [(&'static tokio::task::LocalKey<String>, Option<String>); 10] = [
            (&CURRENT_AGENT_ID, self.agent_id),
            (&CURRENT_CHANNEL_ID, self.channel_id),
            (&CURRENT_THREAD_ID, self.thread_id),
            (&CURRENT_WORKSPACE_ID, self.workspace_id),
            (&CURRENT_SESSION_KEY, self.key),
            (&CURRENT_SESSION_ID, self.id),
            (&CURRENT_USER_ID, self.user_id),
            (&CURRENT_BILLING_IDENTITY, self.billing_identity),
            (&CURRENT_PLATFORM_USER_ID, self.platform_user_id),
            (&ACTIVE_ENVIRONMENT, self.active_environment),
        ];
        let mut run: Box<dyn FnOnce() -> T> = Box::new(f);
        for (key, value) in layers {
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                let inner = run;
                run = Box::new(move || key.sync_scope(v, inner));
            }
        }
        // Carry the HashMap-typed locals (always; empty = default), then the TTL
        // (only when captured; absent = the stager's scratch default).
        let outer = move || {
            ACTIVE_MCP_ENVS.sync_scope(active_mcp_envs, move || {
                ACTIVE_SKILL_INSTANCES.sync_scope(active_skill_instances, run)
            })
        };
        let outer =
            move || workforce_common::secret_policy::sync_scope_if_present(secret_policy, outer);
        let outer = move || match served {
            Some(log) => CURRENT_SERVED.sync_scope(log, outer),
            None => outer(),
        };
        let outer = move || match desk {
            Some(desk) => CURRENT_DESK.sync_scope(desk, outer),
            None => outer(),
        };
        let outer = move || match artifact_store {
            Some(store) => CURRENT_ARTIFACT_STORE.sync_scope(store, outer),
            None => outer(),
        };
        match artifact_ttl {
            Some(ttl) => CURRENT_ARTIFACT_TTL.sync_scope(ttl, outer),
            None => outer(),
        }
    }
}

/// Process-wide count of AgentRunner executions currently in flight —
/// every surface (Slack, gateway, Teams, workflow steps, background work)
/// funnels through the runner, so this is the one gauge a graceful
/// shutdown can drain against.
static INFLIGHT_TURNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn inflight_turns() -> usize {
    INFLIGHT_TURNS.load(std::sync::atomic::Ordering::SeqCst)
}

/// RAII increment of [`inflight_turns`] for the duration of a runner
/// execution.
struct TurnGuard;

impl TurnGuard {
    fn begin() -> Self {
        INFLIGHT_TURNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        INFLIGHT_TURNS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Synthetic identity for system-initiated work (NATS event handlers,
/// scheduled jobs, batch sweeps). Per-user vault paths reject `None`
/// identity by design — callers that genuinely have no human user must
/// opt in to this synthetic UID rather than silently fall through.
pub const SYSTEM_USER_ID: &str = "system";

/// Run `fut` with `CURRENT_USER_ID` scoped to [`SYSTEM_USER_ID`]. Use this
/// at every system-job entrypoint (NATS subscribers, schedulers, knowledge
/// sweeps, oauth callbacks) so vault and skill code sees a non-None
/// identity. Audit logs and per-user vault paths land under
/// `users/system/...`, never silently shared with whoever ran last.
pub async fn with_system_identity<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_USER_ID.scope(SYSTEM_USER_ID.to_string(), fut).await
}

/// `tokio::spawn` that carries the caller's session task-locals into the
/// new task. Plain `tokio::spawn` drops them — use this for fire-and-forget
/// work that depends on `CURRENT_USER_ID` etc. (passive memory extraction,
/// lesson extraction). Outside an agent run the capture is empty and this
/// is equivalent to a bare `tokio::spawn`.
pub fn spawn_in_session_context<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    SessionContext::capture().spawn(fut)
}

// Round warning thresholds removed in DW-419 — replaced by proportional
// halfway warnings that adapt to the per-step max_rounds budget.

/// Record of a model switch during an agent run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelSwitchRecord {
    pub round: u32,
    pub from_model: String,
    pub to_model: String,
    pub reason: String,
}

/// Configuration for an agent runner.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Agent identifier.
    pub agent_id: AgentId,
    /// System prompt for the LLM.
    pub system_prompt: String,
    /// LLM model to use.
    pub model: Option<String>,
    /// Maximum tokens per response.
    pub max_tokens: Option<u32>,
    /// Maximum tool execution rounds before stopping.
    pub max_rounds: u32,
    /// Team ID for policy evaluation context.
    pub team_id: Option<String>,
    /// Agent role for policy evaluation (e.g. "developer", "reviewer").
    pub role: Option<String>,
    /// Compaction / context window management configuration.
    pub compaction: CompactionConfig,
    /// Auto-spill of large tabular tool outputs.
    pub spill: workforce_common::session::SpillConfig,
    /// Adaptive model routing configuration (DW-252).
    pub routing: crate::model_router::RoutingConfig,
    /// Model tier equivalence registry for provider-agnostic routing.
    pub tier_registry: crate::llm::tiers::TierRegistry,
    /// Per-model max_tokens overrides.
    pub model_max_tokens: std::collections::HashMap<String, u32>,
    /// Rounds between automatic context eviction checks (workflow steps only).
    /// When set, stale tool outputs are replaced with compact markers every N rounds.
    pub context_eviction_interval: Option<u32>,
    /// Number of recent tool outputs to preserve per tool type during eviction.
    pub context_eviction_keep_last: u32,
    /// Optional trace writer for capturing LLM fine-tuning data.
    pub trace_writer: Option<Arc<dyn crate::trace::TraceWriter>>,
    /// Where this run came from, for its trace header.
    pub trace_tags: crate::trace::TraceTags,
    /// Optional sink for runtime Always-* persistence. When set, the
    /// runner persists `AllowAlways` / `DenyAlways` choices into the
    /// permission store before resolving the approval. When `None`,
    /// the runner still honours the choice for the in-flight call but
    /// never saves it (legacy behaviour).
    pub permission_writer: Option<Arc<dyn PermissionWriter>>,
    /// Cascade gate consulted *after* the PolicyEngine. Can only further
    /// restrict the PolicyEngine decision — never relax it. When `None`
    /// (the `[permissions] enabled = false` case) the cascade arm is a
    /// no-op and tool dispatch is governed entirely by the YAML policy
    /// engine, preserving pre-cascade behaviour.
    pub permission_gate: Option<Arc<dyn PermissionGate>>,
    /// How tool calls' arguments are checked against their tools' schemas.
    pub arguments: crate::tools::arguments::ArgumentValidation,
    /// Which tools a turn offers in full and which it catalogues for loading.
    pub tool_loading: crate::tools::loading::ToolLoading,
    /// What the agent's Sift programs may call; `None` when it runs none.
    pub programs: Option<Arc<crate::tools::program::Programs>>,
    /// Constitutional admin gate consulted *before* the cascade and any
    /// approval prompt. `Some(reason)` from [`CapabilityGate::deny_reason`]
    /// hard-denies the call pre-dispatch (the role→capability matrix forbids
    /// it). Always consulted when wired — unlike `permission_gate`, it is not
    /// gated on `[permissions] enabled`; admin authority is constitutional.
    pub capability_gate: Option<Arc<dyn CapabilityGate>>,
    /// Routes `RequireApproval` decisions to designated approvers (Slack DMs)
    /// and persists a durable pending row. `None` ⇒ every approval is the
    /// existing in-channel self-serve prompt.
    pub approval_router: Option<Arc<dyn ApprovalRouter>>,
    /// Set only on an approval-resume turn: the exact tool+args a human just
    /// approved, executed deterministically before the first LLM round.
    pub preexecute: Option<PreExecutedTool>,
    /// Where the turn records each tool call as it returns, and where a tool
    /// records work it leaves running; `None` keeps no journal.
    pub journal: Option<Arc<dyn crate::turn_journal::TurnJournal>>,
    /// Work an interrupted turn left running, collected before the first
    /// round and answered as the calls that started it.
    pub collect: Vec<crate::turn_journal::PendingCall>,
    /// The calls this turn already made before its first round, which the
    /// run takes as they are instead of making `preexecute` and `collect`
    /// itself.
    pub seeds: Option<Seeds>,
    /// Whether this run answers a human waiting in a conversation (as opposed
    /// to a scheduler/monitor/worker turn). Interactive wind-downs invite the
    /// user to say "continue" — a follow-up resumes with full session context.
    pub interactive: bool,
    /// Whether the turn's surface can answer a self-serve approval prompt.
    pub self_serve: crate::tools::approval::SelfServe,
    /// Checks that code the turn edited builds before it answers; `None`
    /// checks nothing.
    pub compile_gate: Option<crate::compile_gate::CompileGateSettings>,
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("agent_id", &self.agent_id)
            .field("model", &self.model)
            .field("max_rounds", &self.max_rounds)
            .field("trace_writer", &self.trace_writer.as_ref().map(|_| "..."))
            .field("trace_tags", &self.trace_tags)
            .finish_non_exhaustive()
    }
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            agent_id: AgentId::new("default"),
            system_prompt: "You are a helpful AI assistant.".to_string(),
            model: None,
            max_tokens: None,
            max_rounds: 50,
            team_id: None,
            role: None,
            compaction: CompactionConfig::default(),
            spill: workforce_common::session::SpillConfig::default(),
            routing: crate::model_router::RoutingConfig::default(),
            tier_registry: crate::llm::tiers::TierRegistry::new(None),
            model_max_tokens: std::collections::HashMap::new(),
            context_eviction_interval: None,
            context_eviction_keep_last: 3,
            trace_writer: None,
            trace_tags: Default::default(),
            permission_writer: None,
            permission_gate: None,
            arguments: crate::tools::arguments::ArgumentValidation::default(),
            tool_loading: crate::tools::loading::ToolLoading::default(),
            programs: None,
            capability_gate: None,
            approval_router: None,
            preexecute: None,
            journal: None,
            collect: Vec::new(),
            seeds: None,
            interactive: false,
            self_serve: crate::tools::approval::SelfServe::Answerable,
            compile_gate: None,
        }
    }
}

/// The calls a turn made before its first round, as the call and result
/// pairs that follow its task, and what each did.
#[derive(Debug, Clone, Default)]
pub struct Seeds {
    pub messages: Vec<LlmMessage>,
    pub invocations: Vec<ToolInvocation>,
}

impl RunnerConfig {
    /// Resolve the effective max_tokens for a given model, checking per-model
    /// overrides before falling back to the global max_tokens, then clamping
    /// to the model's known output ceiling.
    pub fn effective_max_tokens(&self, model: &str) -> Option<u32> {
        let fallback = self.max_tokens.unwrap_or(8192);
        let configured = crate::llm::model_limits::effective_max_tokens_for_model(
            model,
            &self.model_max_tokens,
            fallback,
        );
        // Clamp to the model's known output ceiling: an over-large `max_tokens`
        // is rejected by the provider and over-reserves the context window.
        Some(
            crate::llm::model_limits::model_limits(model).map_or(configured, |limits| {
                configured.min(limits.max_output_tokens)
            }),
        )
    }
}

/// Result of running an agent on a task.
#[derive(Debug)]
pub struct RunResult {
    /// The final text response from the agent.
    pub response: String,
    /// Total token usage across all LLM calls.
    pub total_usage: TokenUsage,
    /// Number of tool execution rounds.
    pub rounds: u32,
    /// Tool invocations that were made.
    pub tool_invocations: Vec<ToolInvocation>,
    /// Per-round telemetry details.
    pub round_details: Vec<RoundDetail>,
    /// Total wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Model used for this run.
    pub model: String,
    /// Model switches that occurred during the run (DW-267).
    pub model_switches: Vec<ModelSwitchRecord>,
    /// Whether the run was terminated because the round budget was exhausted.
    pub budget_capped: bool,
    /// Whether the run reached its round cap at all — including the graceful
    /// path where the wind-down warning landed and the agent wrapped up
    /// cleanly, which ends `end_turn` with `budget_capped` FALSE.
    ///
    /// Both are "the turn stopped because it ran out of rounds, not because
    /// the work was done", and a reader needs the same offer to continue. The
    /// distinction was invisible in the UI and produced the worst version of
    /// it: the wind-down prompt tells the agent to say it can continue on
    /// request, so users got a reply ending "say continue" with nothing to
    /// click, on the turns most likely to be unfinished.
    pub hit_round_cap: bool,
    /// Follow-up user messages absorbed mid-run via injection, in arrival
    /// order. The hub persists these into the session transcript — they exist
    /// nowhere else once the run ends.
    pub injected_user_messages: Vec<InjectedUserMessage>,
    /// What the compile gate appended to `response` (a failing or unverified
    /// build): the reader's to see, but not the agent's words.
    pub gate_notice: Option<String>,
}

impl RunResult {
    /// `response` as the agent wrote it, without the compile gate's notice:
    /// what a review of the answer reads.
    pub fn agent_response(&self) -> std::borrow::Cow<'_, str> {
        match &self.gate_notice {
            Some(notice) => crate::compile_gate::without_notice(&self.response, notice),
            None => std::borrow::Cow::Borrowed(&self.response),
        }
    }
}

/// Per-round telemetry detail for a single LLM call.
#[derive(Debug, Clone)]
pub struct RoundDetail {
    pub round_number: u32,
    pub usage: TokenUsage,
    pub latency_ms: u64,
    pub prompt_size_bytes: usize,
    pub response_size_bytes: usize,
    pub tool_count: u32,
    pub has_tool_use: bool,
    pub stop_reason: String,
}

/// Record of a single tool invocation.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub tool_name: String,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    pub success: bool,
    pub policy_decision: String,
    pub duration_ms: u64,
    /// Structured denial reason from the policy engine (DW-187).
    pub denial_reason: Option<String>,
    /// Error message when `success == false`. Carries the `ToolResult.error`
    /// text so consumers can see *why* the tool failed instead of only
    /// `output: null`.
    pub error: Option<String>,
}

/// Progress events emitted during an agent run.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Agent is calling the LLM (round number).
    LlmCall { round: u32 },
    /// A tool execution is starting.
    ToolStart {
        name: String,
        round: u32,
        /// Summary of what the tool is doing (e.g. file path, command).
        detail: String,
    },
    /// A tool execution finished.
    ToolEnd {
        name: String,
        success: bool,
        duration_ms: u64,
        round: u32,
    },
    /// Beginning of a streamed LLM response.
    StreamStart,
    /// A chunk of text from the LLM (token-by-token delta).
    StreamDelta { text: String },
    /// End of the streamed LLM response.
    StreamEnd,
    /// The text just streamed for this round is being discarded: it never
    /// reaches `result.response`, so a consumer that renders the accumulated
    /// stream should drop what it buffered for this round. The `reason` says
    /// whether the text was garbage or a real draft answer — the two demand
    /// opposite treatment from a consumer mirroring the stream to a human.
    StreamDiscard { reason: StreamDiscardReason },
    /// The agent's answer is complete. Anything streamed after this is a
    /// post-turn annotation — a grounding block, a review note — produced by a
    /// pass that runs once the answer is already written.
    ///
    /// A consumer mirroring the stream to a human should settle the answer here:
    /// drop any "still writing" affordance and stop treating it as growing. The
    /// annotations that follow belong beside the answer, not inside it, and the
    /// reader should not be made to wait for them.
    ///
    /// Deliberately carries NO "a check is running" signal: no consumer may
    /// announce a review, because review failures are silent (log-only) — and
    /// silence after an announced check reads as verification that never
    /// happened. Silence must stay uninformative.
    AnswerSettled,
    /// Final text response is available (sent for non-streaming or as a summary).
    Response { text: String },
    /// Agent run is complete with stats.
    Done {
        rounds: u32,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        tool_count: usize,
    },
    /// Session compaction is starting.
    CompactStart,
    /// Session compaction finished.
    CompactEnd,
    /// Context window usage update (sent after session save).
    ContextUpdate {
        estimated_tokens: u32,
        context_max: u32,
    },
    /// Mid-run message(s) were absorbed from the user.
    InjectedMessage { count: u32 },
    /// A tool call requires human approval before execution.
    ApprovalRequired {
        request_id: String,
        agent_id: String,
        tool_name: String,
        detail: String,
        reason: String,
        round: u32,
        /// Which prompt options the surface should render. Carries the
        /// `ask` vs `always_ask` distinction so phase-4 typed buttons
        /// can hide Always-Allow when the parent state forbids it.
        /// Pre-cascade surfaces ignore this and fall back to the
        /// existing approve/reject text flow.
        options: workforce_core::policy::PermissionPromptOptions,
        /// `Some` only for routed (designated-approver) approvals: tells the
        /// surface to DM these approvers instead of prompting in-channel.
        routing: Option<ApprovalRouting>,
    },
    /// A self-serve approval prompt settled — answered, or left unanswered
    /// until it timed out — and can no longer be answered.
    ApprovalResolved { request_id: String },
    /// The model call was rate-limited and the runner is sleeping before retrying.
    /// Emitted so the UI can show the wait state instead of appearing stuck.
    RateLimited { wait_secs: u64, attempt: u32 },
    /// A model produced no first token within the liveness watchdog window
    /// and the failover chain is switching to the next model. Emitted so
    /// the UI can explain the switch instead of appearing stuck.
    SlowModelFailover { model: String, elapsed_secs: u64 },
    /// A secret marker was substituted to cleartext at a secret-sink
    /// tool boundary. Carries metadata only — never cleartext. Used by
    /// the surface layer to post a "stored, please delete the original
    /// message" follow-up to the user.
    SecretConsumed {
        tool: String,
        consumed: Vec<workforce_common::secret_marker::ConsumedSecret>,
    },
    /// Nested activity from a long-running tool's own work — e.g. the advisor's
    /// or `coding.session`'s `claude -p` subprocess. Carries a human-readable
    /// `source` and a one-line `detail` so the surface can show
    /// "advisor ▸ reading foo.rs" on the live status line without conflating it
    /// with the agent's own tool calls (keeps `ToolStart`/`ToolEnd` accounting
    /// clean). Emitted via a bridge that translates a nested `ProgressEvent`
    /// stream; never carries cleartext.
    SubActivity { source: String, detail: String },
    /// Periodic progress check-in: a long turn has gone quiet, so post a
    /// durable note to the human (with a Pause affordance) and carry on. Unlike
    /// the ephemeral status line this is a normal message that survives in the
    /// thread, guaranteeing the human hears something at least every few
    /// minutes. The surface owns the Pause button; the runner only supplies the
    /// note. Non-blocking — the run continues immediately after emitting this.
    CheckIn { note: String },
    /// The turn needs the person it runs for to connect a subscription
    /// login first; a surface offers them the way to. Never carries a
    /// sign-in code.
    ConnectRequired {
        prompt: workforce_common::subscription::ConnectPrompt,
    },
    /// A live Desk turn as it stands, emitted by the turn's own relay between
    /// two of its events, so every consumer carries it in order with the
    /// deltas around it.
    Snapshot(workforce_common::channels::desk::TurnSnapshot),
}

/// A follow-up user message absorbed into a run, reported on [`RunResult`]
/// so the hub can persist it into the session transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedUserMessage {
    pub user: String,
    pub text: String,
    pub external_id: Option<String>,
}

/// A message injected into a running agent mid-execution.
#[derive(Debug, Clone)]
pub enum InjectedMessage {
    /// A follow-up user message absorbed into the current run.
    UserMessage {
        text: String,
        user: String,
        external_id: Option<String>,
    },
    /// A steering command that tells the agent to stop.
    SteerStop { text: String, user: String },
    /// Human response to an approval request.
    ///
    /// Surfaces upgraded to the cascade prompt UI (phase 4) populate
    /// `choice` with the typed 4-option selection. Legacy surfaces
    /// (free-text `approve <id>` / `reject <id>`) set `approved`; the
    /// approval router upgrades that to `AllowOnce`/`DenyOnce` via
    /// [`PermissionChoice::from_legacy_bool`].
    ApprovalResponse {
        request_id: String,
        approved: bool,
        user: String,
        choice: Option<workforce_core::policy::PermissionChoice>,
    },
}

/// Callback the runner uses to persist `Always-Allow` / `Always-Deny`
/// runtime choices as User-scope rules in the permission store. The
/// runner stays decoupled from `workforce-permissions` — the hub
/// supplies an adapter that owns the `PermissionEvaluator`.
#[async_trait::async_trait]
pub trait PermissionWriter: Send + Sync {
    /// Persist a runtime choice. Implementations write a User-scope
    /// row keyed on `(user_id, tool_name)`; subsequent cascade
    /// evaluations honour it (or refuse to if the parent state caps
    /// permissiveness — phase 2's write-time guard).
    async fn persist_user_choice(
        &self,
        user_id: &str,
        tool_name: &str,
        choice: workforce_core::policy::PermissionChoice,
    ) -> eyre::Result<()>;
}

/// Delivery hint carried on [`ProgressEvent::ApprovalRequired`] for routed
/// (designated-approver) approvals. The authoritative copy is the row written
/// by [`ApprovalRouter::defer`]; this only tells the surface whom to DM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRouting {
    pub approver_user_ids: Vec<String>,
    pub requester_user_id: String,
}

/// Outcome of routing one `RequireApproval` decision. A three-way enum so the
/// four-eyes "nobody reachable" case is a distinct state, never an ambiguous
/// empty list.
pub enum ApprovalDecision {
    /// No routing rule applies → today's in-channel self-serve prompt.
    SelfServe,
    /// Deliver to these approvers (non-empty, all Slack-reachable).
    Routed(ApprovalRoute),
    /// A rule requires another approver (`allow_self_approval = false`) but
    /// nobody eligible is reachable → deny now rather than strand the request.
    DenyUnreachable,
}

/// Concrete, already-resolved routing. Roles are expanded to user ids by the
/// adapter at decision time (membership is live).
pub struct ApprovalRoute {
    pub approver_user_ids: Vec<String>,
    pub allow_self_approval: bool,
    pub ttl: std::time::Duration,
}

pub struct ApprovalRouteCtx<'a> {
    pub user_id: &'a str,
    pub agent_id: &'a str,
    pub tool_name: &'a str,
}

/// Everything persisted at deferral so the approval can be delivered, resolved,
/// and resumed later — independent of any live runner.
#[derive(Debug, Clone)]
pub struct PendingApprovalCore {
    pub request_id: String,
    pub agent_id: String,
    pub session_key: String,
    pub channel_id: String,
    pub requester_user_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub reason: String,
    pub allow_self_approval: bool,
    pub approver_user_ids: Vec<String>,
    pub ttl: std::time::Duration,
}

/// One tool the resume turn executes deterministically before the first LLM
/// round — the exact action a human approved.
#[derive(Debug, Clone)]
pub struct PreExecutedTool {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
}

/// Routes a `RequireApproval` decision to designated approvers and persists the
/// durable pending row. Hub-supplied so the runner stays decoupled from
/// `workforce-permissions` / `workforce-identity`, mirroring [`PermissionGate`].
pub trait ApprovalRouter: Send + Sync {
    fn route(&self, ctx: &ApprovalRouteCtx<'_>) -> ApprovalDecision;
    /// Synchronously persist the core row before the runner returns the
    /// synthetic "pending" tool result. Idempotent on `request_id`.
    fn defer(&self, record: &PendingApprovalCore) -> eyre::Result<()>;
}

/// Resolution of the routing consult for a `RequireApproval` decision.
#[derive(Debug)]
pub enum RouteResolution {
    SelfServe,
    DenyUnreachable,
    Routed {
        request_id: String,
        approver_user_ids: Vec<String>,
        /// How long the durable row stays answerable.
        ttl: std::time::Duration,
    },
}

/// How long a self-serve approval prompt stays answerable before the runner
/// refuses the call as unanswered.
pub const SELF_SERVE_APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The synthetic tool result injected when an approval is routed to a
/// designated approver — shared by both run loops so the wording can't drift.
pub const ROUTED_APPROVAL_TOOL_RESULT: &str =
    "⏳ This action requires approval from a designated approver and has been routed to \
     them. You will be notified when they decide. Do not retry this tool.";

/// What a call answers when a rule routes its approval and nobody who could
/// give it can be reached.
pub const UNREACHABLE_APPROVER: &str =
    "No eligible approver is reachable to authorise this action.";

/// What a call decided earlier answers when policy now denies it.
const NO_LONGER_PERMITTED: &str =
    "This action is no longer permitted by policy and was not performed.";

/// A call a run makes before its first round.
#[derive(Clone, Copy)]
enum Seeded<'a> {
    /// The exact call a human just approved.
    Approved(&'a PreExecutedTool),
    /// Work an interrupted turn left running.
    Collected(&'a crate::turn_journal::PendingCall),
}

impl Seeded<'_> {
    fn call(&self) -> (&str, &serde_json::Value) {
        match self {
            Self::Approved(pre) => (&pre.tool_name, &pre.tool_input),
            Self::Collected(pending) => (&pending.tool, &pending.input),
        }
    }

    fn decision(&self) -> &'static str {
        match self {
            Self::Approved(_) => "preexec",
            Self::Collected(_) => "collected",
        }
    }
}

/// One tool call as a streaming round collects it: the call, what the model
/// reads back, and the record the transcript keeps.
type ToolOutcome = (
    String,
    String,
    serde_json::Value,
    String,
    serde_json::Value,
    bool,
    u64,
    String,
    bool,
    Option<String>,
    Vec<String>,
);

/// Report a settled call's `ToolEnd` and shape its outcome.
async fn report(
    ptx: &tokio::sync::mpsc::Sender<ProgressEvent>,
    round: u32,
    tool_use_id: String,
    tool_name: String,
    input: serde_json::Value,
    exec: ToolExecution,
) -> ToolOutcome {
    let decision = exec.decision();
    conclude(ptx, round, tool_use_id, tool_name, input, exec, decision).await
}

/// Report a call that needed approval and did not run.
async fn refuse(
    ptx: &tokio::sync::mpsc::Sender<ProgressEvent>,
    round: u32,
    tool_use_id: String,
    tool_name: String,
    input: serde_json::Value,
    refusal: Refusal,
    reason: String,
) -> ToolOutcome {
    let exec = ToolExecution::blocked(refusal.tool_result().to_string(), reason);
    conclude(
        ptx,
        round,
        tool_use_id,
        tool_name,
        input,
        exec,
        "require_approval",
    )
    .await
}

async fn conclude(
    ptx: &tokio::sync::mpsc::Sender<ProgressEvent>,
    round: u32,
    tool_use_id: String,
    tool_name: String,
    input: serde_json::Value,
    exec: ToolExecution,
    decision: &str,
) -> ToolOutcome {
    let _ = ptx
        .send(ProgressEvent::ToolEnd {
            name: tool_name.clone(),
            success: exec.success,
            duration_ms: exec.duration_ms,
            round,
        })
        .await;
    (
        tool_use_id,
        tool_name,
        input,
        exec.output_text,
        exec.output,
        exec.success,
        exec.duration_ms,
        decision.to_string(),
        exec.is_error,
        exec.deny_reason,
        exec.loads,
    )
}

/// The outcome of a call that ran, for the loop that reports no progress.
fn executed_outcome(
    tool_use_id: String,
    tool_name: String,
    input: serde_json::Value,
    exec: ToolExecution,
) -> ToolOutcome {
    let decision = exec.decision().to_string();
    (
        tool_use_id,
        tool_name,
        input,
        exec.output_text,
        exec.output,
        exec.success,
        exec.duration_ms,
        decision,
        exec.is_error,
        exec.deny_reason,
        exec.loads,
    )
}

/// The outcome of a call the tool plan answered in place of running a tool.
fn planned_outcome(
    tool_use_id: String,
    input: serde_json::Value,
    answer: crate::tools::loading::PlanAnswer,
) -> ToolOutcome {
    let tool = answer.tool.clone();
    let exec = answer.execution();
    (
        tool_use_id,
        tool,
        input,
        exec.output_text,
        exec.output,
        exec.success,
        exec.duration_ms,
        "load".to_string(),
        exec.is_error,
        exec.deny_reason,
        exec.loads,
    )
}

/// What a `tool_call` naming no registered tool gets back.
pub(crate) fn unknown_call_message(tool: &str) -> String {
    format!(
        "`{}` is not a tool. Load it with `{}` using its exact name from the tool catalogue.",
        to_llm_name(tool),
        to_llm_name(crate::tools::loading::LOAD_TOOL)
    )
}

/// The outcome of a `tool_call` the runner could not unwrap.
fn unwrap_failure(tool_use_id: String, input: serde_json::Value, message: String) -> ToolOutcome {
    (
        tool_use_id,
        crate::tools::loading::CALL_TOOL.to_string(),
        input,
        message.clone(),
        serde_json::Value::Null,
        false,
        0,
        "allow".to_string(),
        true,
        None,
        Vec::new(),
    )
}

/// A result that loaded `loads` (internal names): the text the model reads —
/// the result followed by the loaded tools' definitions — and the record a
/// native wire renders instead.
pub fn loaded_result(registry: &ToolRegistry, note: String, loads: &[String]) -> (String, Loaded) {
    let definitions: Vec<crate::tools::ToolDefinition> = loads
        .iter()
        .filter_map(|name| registry.get(name).map(|tool| tool.definition()))
        .collect();
    (
        crate::tools::loading::with_definitions(&note, &definitions),
        Loaded {
            tools: loads.iter().map(|name| to_llm_name(name)).collect(),
            note,
        },
    )
}

/// The call's tool and input with any `tool_call` unwrapped, and whether it
/// was one; `Err` is the answer to a `tool_call` that could not be.
pub(crate) fn unwrapped<'a>(
    llm_name: &'a str,
    input: &'a serde_json::Value,
) -> Result<(Cow<'a, str>, Cow<'a, serde_json::Value>, bool), String> {
    match crate::tools::loading::unwrap_call(llm_name, input) {
        None => Ok((Cow::Borrowed(llm_name), Cow::Borrowed(input), false)),
        Some(call) => call.map(|call| (Cow::Owned(call.tool), Cow::Owned(call.arguments), true)),
    }
}

/// `message`'s tool results, each marked with what its call loaded.
pub(crate) fn with_loaded(message: LlmMessage, loads: &HashMap<String, Loaded>) -> LlmMessage {
    LlmMessage {
        content: message
            .content
            .into_iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    loaded,
                } => ContentBlock::ToolResult {
                    loaded: loads.get(&tool_use_id).cloned().or(loaded),
                    tool_use_id,
                    content,
                    is_error,
                },
                other => other,
            })
            .collect(),
        ..message
    }
}

/// Who asks and in which session, when the current scope can route an
/// approval at all: a call with neither is always self-serve.
fn routing_scope() -> Option<(String, String)> {
    Some((
        task_local_value(&CURRENT_USER_ID, false)?,
        task_local_value(&CURRENT_SESSION_KEY, false)?,
    ))
}

/// What the router would decide for `tool_name` in the current scope, with
/// nothing deferred: for a caller that cannot resume a routed call and must
/// refuse it before any row is written.
pub fn approval_route(
    router: Option<&dyn ApprovalRouter>,
    agent_id: &str,
    tool_name: &str,
) -> ApprovalDecision {
    match (router, routing_scope()) {
        (Some(router), Some((requester, _))) => router.route(&ApprovalRouteCtx {
            user_id: &requester,
            agent_id,
            tool_name,
        }),
        _ => ApprovalDecision::SelfServe,
    }
}

/// Consult `router` for a `RequireApproval` decision on `agent_id`'s call. On
/// a routed outcome the durable row is persisted here, before the synthetic
/// "pending" result is returned. A missing router, no requester, no session
/// linkage, or a persist failure all fall back to the self-serve prompt so a
/// human still sees the request. Reads the requester, session and channel
/// from the scoped task-locals.
pub fn route_approval(
    router: Option<&dyn ApprovalRouter>,
    agent_id: &str,
    tool_name: &str,
    reason: &str,
    input: &serde_json::Value,
) -> RouteResolution {
    let (Some(router), Some((requester, session_key))) = (router, routing_scope()) else {
        return RouteResolution::SelfServe;
    };
    let channel_id = task_local_value(&CURRENT_CHANNEL_ID, false).unwrap_or_default();
    match approval_route(Some(router), agent_id, tool_name) {
        ApprovalDecision::SelfServe => RouteResolution::SelfServe,
        ApprovalDecision::DenyUnreachable => RouteResolution::DenyUnreachable,
        ApprovalDecision::Routed(route) => {
            let request_id = uuid::Uuid::new_v4().to_string();
            let core = PendingApprovalCore {
                request_id: request_id.clone(),
                agent_id: agent_id.to_string(),
                session_key,
                channel_id,
                requester_user_id: requester,
                tool_name: tool_name.to_string(),
                tool_input: input.clone(),
                reason: reason.to_string(),
                allow_self_approval: route.allow_self_approval,
                approver_user_ids: route.approver_user_ids.clone(),
                ttl: route.ttl,
            };
            match router.defer(&core) {
                Ok(()) => RouteResolution::Routed {
                    request_id,
                    approver_user_ids: route.approver_user_ids,
                    ttl: route.ttl,
                },
                Err(e) => {
                    warn!(error = %e, tool = %tool_name, "approval defer persist failed; falling back to self-serve prompt");
                    RouteResolution::SelfServe
                }
            }
        }
    }
}

/// A self-serve prompt about to be put to the person: what the card shows,
/// where it goes, and whose Always-* rule an answer writes.
pub struct ApprovalAsk<'a> {
    pub progress: Option<&'a tokio::sync::mpsc::Sender<ProgressEvent>>,
    pub agent_id: &'a str,
    pub tool: &'a str,
    pub detail: &'a str,
    pub reason: &'a str,
    pub round: u32,
    pub options: workforce_core::policy::PermissionPromptOptions,
    pub writer: Option<&'a dyn PermissionWriter>,
    pub requester: Option<&'a str>,
}

/// Put `prompt` to the person and wait up to `patience` for their answer:
/// the card goes out as [`ProgressEvent::ApprovalRequired`], is withdrawn with
/// [`ProgressEvent::ApprovalResolved`] however the prompt settles, and an
/// Always-* answer is persisted before the call it answered may run.
pub async fn await_approval(
    ask: ApprovalAsk<'_>,
    ledger: &TurnApprovals,
    prompt: OpenPrompt,
    patience: Duration,
) -> Decision {
    let request_id = prompt.request_id.clone();
    if let Some(progress) = ask.progress {
        let _ = progress
            .send(ProgressEvent::ApprovalRequired {
                request_id: request_id.clone(),
                agent_id: ask.agent_id.to_string(),
                tool_name: ask.tool.to_string(),
                detail: ask.detail.to_string(),
                reason: ask.reason.to_string(),
                round: ask.round,
                options: ask.options,
                routing: None,
            })
            .await;
    }
    let mut withdrawn = Withdraws {
        progress: ask.progress,
        request_id: Some(request_id),
    };
    let decision = ledger.resolve(prompt, patience).await;
    if let (Some(progress), Some(request_id)) = (ask.progress, withdrawn.request_id.take()) {
        let _ = progress
            .send(ProgressEvent::ApprovalResolved { request_id })
            .await;
    }
    persist_choice(ask.writer, ask.requester, ask.tool, decision.choice).await;
    decision
}

/// Withdraws a prompt's card when the wait for its answer is dropped before
/// it settles, as a round that ends first drops it.
struct Withdraws<'a> {
    progress: Option<&'a tokio::sync::mpsc::Sender<ProgressEvent>>,
    request_id: Option<String>,
}

impl Drop for Withdraws<'_> {
    fn drop(&mut self) {
        if let (Some(progress), Some(request_id)) = (self.progress, self.request_id.take()) {
            let _ = progress.try_send(ProgressEvent::ApprovalResolved { request_id });
        }
    }
}

/// Deliver the media `invocations` generated to the surface `ctx` names. A
/// failed delivery is logged, never fatal to the turn.
pub async fn deliver_generated(
    sink: &dyn crate::tools::image_gen::ImageSink,
    ctx: &SessionContext,
    invocations: &[ToolInvocation],
) {
    for img in invocations
        .iter()
        .filter(|inv| inv.success)
        .flat_map(|inv| crate::tools::image_gen::parse_generated_images(&inv.output))
    {
        if let Err(e) = sink
            .deliver(
                ctx,
                &img.artifact_id,
                &img.media_type,
                img.filename.as_deref(),
                img.caption.as_deref(),
            )
            .await
        {
            warn!(
                error = %e,
                artifact_id = img.artifact_id.as_str(),
                "failed to deliver generated image to channel surface"
            );
        }
    }
}

/// Wall time one step's viewed files may take to render together; a file
/// started within it is finished.
const VIEW_RENDER_BUDGET: Duration = Duration::from_secs(300);

/// The content blocks the files `invocations` staged for viewing render to,
/// for `model`. A failed render is logged and contributes nothing; a file
/// the step's [`VIEW_RENDER_BUDGET`] ran out before is said to be unrendered.
pub async fn render_viewed(
    sink: &dyn crate::tools::image_view::ImageViewSink,
    ctx: &SessionContext,
    invocations: &[ToolInvocation],
    model: &str,
) -> Vec<ContentBlock> {
    let deadline = tokio::time::Instant::now() + VIEW_RENDER_BUDGET;
    let mut blocks = Vec::new();
    for img in invocations
        .iter()
        .filter(|inv| inv.success)
        .flat_map(|inv| crate::tools::image_view::parse_viewed_images(&inv.output))
    {
        if tokio::time::Instant::now() >= deadline {
            blocks.push(ContentBlock::Text {
                text: format!(
                    "`{}` was not rendered: this step's time limit for viewing files was \
                     reached. View it again, fewer files at a time, to see it.",
                    img.filename
                ),
            });
            continue;
        }
        match sink.render(ctx, &img, model).await {
            Ok(rendered) => blocks.extend(rendered),
            Err(e) => warn!(
                error = %e,
                artifact_id = img.artifact_id.as_str(),
                "failed to render viewed image into conversation"
            ),
        }
    }
    blocks
}

/// Persist an Always-* choice as the requester's own rule before the call it
/// answered runs, so the cascade re-evaluating that call already sees it.
async fn persist_choice(
    writer: Option<&dyn PermissionWriter>,
    user: Option<&str>,
    tool: &str,
    choice: Option<workforce_core::policy::PermissionChoice>,
) {
    let (Some(writer), Some(user), Some(choice)) =
        (writer, user, choice.filter(|choice| choice.persists()))
    else {
        return;
    };
    if let Err(e) = writer.persist_user_choice(user, tool, choice).await {
        warn!(
            error = %e,
            tool = %tool,
            user = %user,
            "Failed to persist runtime permission choice"
        );
    }
}

/// Local mirror of `workforce_permissions::types::PermissionState`. The
/// runner crate stays decoupled from `workforce-permissions`; the hub's
/// adapter translates between the two enums in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeState {
    Allow,
    Ask,
    AlwaysAsk,
    Deny,
}

impl CascadeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::AlwaysAsk => "always_ask",
            Self::Deny => "deny",
        }
    }
}

/// Cascade gate the runner consults *after* the PolicyEngine returns
/// its decision. The cascade can only further restrict the decision;
/// it can never relax a `Deny` to an `Allow`. Implementations are
/// expected to be cheap (the hub adapter is an LRU-backed evaluator
/// with a generation-counter invariant) — the runner calls this on the
/// tool-dispatch hot path.
pub trait PermissionGate: Send + Sync {
    fn evaluate(&self, user_id: &str, agent_id: &str, tool_name: &str) -> CascadeState;
}

/// Constitutional admin gate the runner consults *before* the cascade and
/// *before* any approval prompt is emitted. A `Some(reason)` is a hard,
/// pre-dispatch [`workforce_core::policy::PolicyDecision::Deny`] — the role→capability matrix forbids
/// this caller from this tool, so no prompt may fire (a member can't
/// "Always-Allow" their way past a constitutional Deny). `None` passes the
/// call through to the existing policy/cascade/`@destructive` flow, which owns
/// every `Ask`/`Allow`. Unlike [`PermissionGate`], this is **always** consulted
/// when wired — it is not gated on `[permissions] enabled`; admin authority is
/// constitutional, not opt-in. Implementations must be cheap (the hub adapter
/// is a pure registry lookup + identity-store read) — the runner calls this on
/// the tool-dispatch hot path.
pub trait CapabilityGate: Send + Sync {
    fn deny_reason(
        &self,
        user_id: &str,
        agent_id: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<String>;

    /// Whether the call needs a capability `user_id` held under an earlier
    /// role and lacks now, asked after a run (why it was refused something).
    /// Recording the refusal stays [`Self::deny_reason`]'s. A gate that keeps
    /// no history of roles knows of no capability lost.
    fn lost(&self, _user_id: &str, _tool: &str, _input: &serde_json::Value) -> bool {
        false
    }
}

/// Combine the PolicyEngine decision with a cascade state, returning
/// the more restrictive of the two. Pure function so the gate hot path
/// is testable without spinning up an evaluator.
pub fn combine_with_cascade(
    response: workforce_core::policy::PolicyResponse,
    cascade: CascadeState,
) -> workforce_core::policy::PolicyResponse {
    use workforce_core::policy::PolicyDecision as D;
    use CascadeState as C;
    let workforce_core::policy::PolicyResponse {
        decision,
        matched_rule,
        evaluation_time_us,
    } = response;
    let decision = match (decision, cascade) {
        // PolicyEngine deny always wins.
        (d @ D::Deny { .. }, _) => d,
        // Cascade deny upgrades any PolicyEngine pass.
        (_, C::Deny) => D::Deny {
            reason: "Denied by permission cascade".to_string(),
        },
        // PolicyEngine allow + cascade ask/always_ask → upgrade to prompt.
        (D::Allow, C::Ask) => D::RequireApproval {
            reason: "Required by permission cascade".to_string(),
            allow_save_allow: true,
        },
        (D::Allow, C::AlwaysAsk) => D::RequireApproval {
            reason: "Required by permission cascade (always-ask)".to_string(),
            allow_save_allow: false,
        },
        (D::Allow, C::Allow) => D::Allow,
        // PolicyEngine already prompts: cascade can only tighten the
        // `allow_save_allow` bit (the `always_ask` distinction). Allow
        // stays a soft pass-through; the prompt the user sees is the
        // PolicyEngine one with the cascade's Always-Allow cap applied.
        (
            D::RequireApproval {
                reason,
                allow_save_allow,
            },
            cascade,
        ) => {
            let allow_save_allow = match cascade {
                C::AlwaysAsk => false,
                _ => allow_save_allow,
            };
            D::RequireApproval {
                reason,
                allow_save_allow,
            }
        }
    };
    workforce_core::policy::PolicyResponse {
        decision,
        matched_rule,
        evaluation_time_us,
    }
}

/// Upper bound on the detail carried in a `ToolStart` progress event. The
/// Slack and Claude-Code renderers re-truncate to their own display width
/// (`truncate_detail`, 80 chars); this only bounds the NATS payload so a giant
/// heredoc command can't bloat the stream.
const DETAIL_CAP: usize = 200;

/// Extract a short, human-readable summary from a tool call's input JSON. Feeds
/// the live status line — `_Running jira.api:_ `GET /rest/api/3/issue/PROJ-1``.
///
/// Driven by which fields are *present*, not by the tool name. Tool names are
/// an unreliable signal here: `jira.list_projects` contains "list" but takes no
/// path, `code.read` contains "read" but `metabase.card_query` contains neither
/// yet is the case we most want to summarise. Matching on `name.contains(...)`
/// mis-bucketed the whole `*.list` / `*_list` family (rendering a bare ".") and
/// left HTTP-style tools showing just their verb — the alphabetically-first
/// `method` value ("GET") was all that surfaced. Field shape is the honest
/// signal, and the descriptive-key fallback keeps MCP/adapter calls readable.
fn tool_detail(_tool_name: &str, input: &serde_json::Value) -> String {
    pick_tool_detail(input).chars().take(DETAIL_CAP).collect()
}

/// The approval-prompt detail for a call: a tool's own [`Tool::approval_summary`]
/// when it offers one (so an approver sees the call's true scope), else the
/// generic shape-based [`tool_detail`]. Both are capped at [`DETAIL_CAP`].
pub fn approval_detail(tools: &ToolRegistry, tool_name: &str, input: &serde_json::Value) -> String {
    tools
        .get(tool_name)
        .and_then(|tool| tool.approval_summary(input))
        .map_or_else(
            || tool_detail(tool_name, input),
            |summary| summary.chars().take(DETAIL_CAP).collect(),
        )
}

fn pick_tool_detail(input: &serde_json::Value) -> String {
    let Some(obj) = input.as_object() else {
        return input.as_str().unwrap_or_default().to_string();
    };
    // Non-empty string value for a key.
    let s = |k: &str| {
        obj.get(k)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
    };

    // HTTP-style API tools — `{ method, path|url|endpoint, ... }`. Compose so
    // the line reads "GET /rest/api/3/issue/PROJ-1" rather than the bare verb.
    if let Some(method) = s("method") {
        if let Some(target) = s("path").or_else(|| s("url")).or_else(|| s("endpoint")) {
            return format!("{method} {target}");
        }
    }
    // Shell / exec tools.
    if let Some(cmd) = s("command").or_else(|| s("cmd")) {
        return cmd.to_string();
    }
    // Filesystem-ish tools.
    if let Some(path) = s("path")
        .or_else(|| s("file_path"))
        .or_else(|| s("directory"))
    {
        return path.to_string();
    }
    // Otherwise surface the most descriptive scalar, preferring well-known
    // query/identifier keys over whatever happens to sort first in the
    // (alphabetical) input map.
    const PRIORITY: &[&str] = &[
        "query",
        "q",
        "url",
        "endpoint",
        "sql",
        "question",
        "prompt",
        "text",
        "message",
        "content",
        "body",
        "title",
        "summary",
        "name",
        "issue_key",
        "key",
        "id",
    ];
    for k in PRIORITY {
        if let Some(v) = s(k) {
            return v.to_string();
        }
    }
    // Numeric / bool identifiers (e.g. `{ "card_id": 5 }`) — keep the key for
    // context, since the bare value alone says nothing.
    for (k, v) in obj {
        if v.is_number() || v.is_boolean() {
            return format!("{k}={v}");
        }
    }
    // Last resort: first non-empty string value (the original behaviour).
    obj.values()
        .find_map(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Tokenise tool-result content so any credential captured by a non-web
/// tool (e.g., `code.search` over a workspace containing `vault.enc`,
/// `.env`, `*-credentials.json`) is replaced with a marker before it
/// enters the prompt. Pairs with `wrap_external_content` for web tools.
/// Idempotent — already-tokenised content is a cheap no-op. See incident
/// 2026-05-18 — `sentinel.audit` caught a cleartext credential at the
/// LLM-egress boundary; this layer stops it at the tool-result boundary.
fn tokenize_tool_outputs(raw: Vec<(String, String, String, bool)>) -> Vec<(String, String, bool)> {
    raw.into_iter()
        .map(|(id, tool_name, content, is_error)| {
            // Phase 0 of tool-output compression:
            // record per-tool output size so we can see which tools dominate context
            // before building any compression. This is the single chokepoint both run
            // loops funnel tool results through, and `tool_name` is still present here
            // (the returned tuple drops it). Tool names are a bounded set, so they are
            // a safe metric label. No-op until the hub installs the Prometheus recorder.
            let est = estimate_tokens(&content);
            metrics::histogram!("workforce.tool_output.est_tokens", "tool" => tool_name.clone())
                .record(est as f64);
            metrics::counter!("workforce.tool_output.count", "tool" => tool_name.clone())
                .increment(1);

            let (tokenized, _captured) =
                crate::security::external_content::tokenize_with_session_context(
                    &content, &tool_name,
                );
            (id, tokenized, is_error)
        })
        .collect()
}

/// Build a trace record for a tool invocation with cleartext credentials
/// redacted out of its input and output. The execution trace is persisted to
/// disk, so it must not store secrets at rest — the egress sanitizer only
/// protects the path *to the model*, not the trace file (a tool like `exec.run`
/// that read a credential would otherwise persist it verbatim here).
fn redacted_tool_result_trace(inv: &ToolInvocation) -> crate::trace::ToolResultTrace {
    let mut input = inv.input.clone();
    let mut output = inv.output.clone();
    workforce_common::redact::redact_json_strings(&mut input, 32);
    workforce_common::redact::redact_json_strings(&mut output, 32);
    crate::trace::ToolResultTrace {
        tool_use_id: String::new(),
        tool_name: inv.tool_name.clone(),
        input,
        output,
        success: inv.success,
        duration_ms: inv.duration_ms,
    }
}

/// Why a context overflow happened. Drives the wording of the system note
/// injected after compaction so the agent reacts appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowCause {
    /// The latest turn's tool result(s) dominate the prompt — earlier
    /// conversation is intact and the agent should retry the tool with
    /// narrower scope or pagination.
    ToolSpam,
    /// Tokens are distributed across many turns — old conversation context
    /// is being dropped to make room.
    HistoryGrowth,
}

impl OverflowCause {
    fn emergency_note(self) -> &'static str {
        match self {
            Self::ToolSpam => {
                "[SYSTEM] The previous tool call returned more data than fits in the \
                 context window. Older tool results were dropped to make room. Your \
                 earlier conversation is intact — if you still need data from that \
                 tool, retry with a narrower scope (specific path, smaller `limit`, \
                 or pagination via `offset`)."
            }
            Self::HistoryGrowth => {
                "[SYSTEM] Earlier conversation context was compacted to fit the \
                 context window. Recent context is preserved."
            }
        }
    }

    fn nuclear_user_note(self) -> &'static str {
        match self {
            Self::ToolSpam => {
                "[SYSTEM] The previous tool call's result was too large to fit in the \
                 context window even after dropping older messages. That tool result \
                 has been discarded; your earlier conversation is intact. Retry the \
                 tool with a narrower scope (specific path, smaller `limit`, or \
                 pagination via `offset`)."
            }
            Self::HistoryGrowth => {
                "[SYSTEM] SEVERE CONTEXT LOSS: The conversation far exceeded the \
                 context window. Only the most recent message has been preserved. \
                 Earlier context has been lost."
            }
        }
    }

    fn nuclear_assistant_ack(self) -> &'static str {
        match self {
            Self::ToolSpam => {
                "Understood — that tool returned too much data. I'll retry with a \
                 more focused query."
            }
            Self::HistoryGrowth => {
                "Understood. I've lost earlier context due to context window limits. \
                 I'll do my best with the remaining information."
            }
        }
    }
}

/// How a context-limit integer is positioned relative to a keyword in a
/// provider error string.
enum LimitExtractor {
    /// The number follows the keyword, e.g. "maximum context length is 163840".
    After(&'static str),
    /// The number immediately precedes the keyword, e.g. "200000 maximum".
    Before(&'static str),
}

/// Tried in order; the first match wins. The specific OpenRouter phrasing
/// (`After("maximum context length is")`) must precede the generic
/// `Before("maximum")` fallback so it isn't shadowed by the same keyword.
const CONTEXT_LIMIT_EXTRACTORS: &[LimitExtractor] = &[
    LimitExtractor::After("maximum context length is"),
    LimitExtractor::After("context length of"),
    LimitExtractor::After("limit is"),
    LimitExtractor::Before("maximum"),
];

fn extract_limit(text: &str, extractor: &LimitExtractor) -> Option<u32> {
    match extractor {
        LimitExtractor::After(kw) => text
            .find(kw)
            .and_then(|i| first_integer(&text[i + kw.len()..])),
        LimitExtractor::Before(kw) => text
            .find(kw)
            .and_then(|i| last_integer(text[..i].trim_end())),
    }
}

fn first_integer(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

fn last_integer(s: &str) -> Option<u32> {
    let end = s.rfind(|c: char| c.is_ascii_digit())? + 1;
    let start = s[..end]
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    s[start..end].parse().ok()
}

fn estimate_block_tokens(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text { text } => estimate_tokens(text),
        ContentBlock::Thinking { thinking, .. } => estimate_tokens(thinking),
        ContentBlock::ToolUse { input, .. } => estimate_tokens(&input.to_string()),
        ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => 1000,
    }
}

fn estimate_messages_tokens(messages: &[LlmMessage]) -> u32 {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(estimate_block_tokens)
        .sum()
}

/// Estimated tokens of `ToolResult` content currently in the prompt — the
/// tool-output footprint a single LLM call is billed for (prior rounds' outputs
/// are resent and re-billed every call, and this reflects any pruning already
/// applied). Summed at each call and paired with the turn's cumulative
/// `tokens_in`, it gives the true share of paid context that is tool output —
/// the lever tool-output compression pulls.
fn tool_result_tokens_in_prompt(messages: &[LlmMessage]) -> u32 {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(estimate_tokens(content)),
            _ => None,
        })
        .sum()
}

/// The provider-reported prompt size of the round loop's most recent LLM call,
/// pinned to the conversation length that produced it so later rounds can
/// price only what was appended since. Feeds the watchdog the REAL context
/// size — the len/4 estimate runs ~2× behind on dense content, which let long
/// tool turns grow into provider overflows the watchdog never predicted.
#[derive(Debug, Clone, Copy)]
struct ObservedRound {
    input_tokens: u32,
    message_len_at_observation: usize,
}

impl ObservedRound {
    /// Capture a round's real prompt size (`input + cache read + cache
    /// creation`) against the message count that produced it. `None` when the
    /// provider reported nothing (degraded/stub responses) — the previous
    /// observation, if any, stays in force.
    fn capture(usage: &TokenUsage, message_len: usize) -> Option<Self> {
        let input_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        (input_tokens > 0).then_some(Self {
            input_tokens,
            message_len_at_observation: message_len,
        })
    }

    /// A copy of this observation with `freed` tokens subtracted from the
    /// baseline — applied after in-flight eviction shrinks history the sample
    /// was measured against, so the stale baseline can't overstate pressure.
    fn discounted(self, freed: u32) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(freed),
            ..self
        }
    }

    /// The observation-based prediction for the next call: the real prompt
    /// size plus the estimate of messages appended since. `None` when the
    /// history has shrunk below the observation point (a compaction ran) —
    /// the sample described a prompt that no longer exists.
    fn predicted_tokens(&self, messages: &[LlmMessage]) -> Option<u32> {
        messages
            .get(self.message_len_at_observation..)
            .map(|appended| {
                self.input_tokens
                    .saturating_add(estimate_messages_tokens(appended))
            })
    }
}

/// Predict whether the next LLM call would breach the urgent context fraction,
/// returning the predicted prompt size when it would. Lets the round loop
/// compact mid-turn instead of waiting for a provider overflow it can't always
/// recover from. The prediction is the larger of the pure estimate and the
/// last observed real prompt size extended by what was appended since;
/// estimate-only when no observation exists.
fn watchdog_should_compact(
    messages: &[LlmMessage],
    reserved: ReservedContext,
    context_limit: u32,
    urgent_ratio: f32,
    last_observed: Option<ObservedRound>,
) -> Option<u32> {
    let estimate_based = estimate_messages_tokens(messages).saturating_add(reserved.total());
    let observed_based = last_observed
        .and_then(|o| o.predicted_tokens(messages))
        .unwrap_or(0);
    let predicted = estimate_based.max(observed_based);
    let limit = (context_limit as f32 * urgent_ratio) as u32;
    (predicted > limit).then_some(predicted)
}

/// The final-round wind-down injection. Interactive turns additionally have
/// the agent hand the user a resume path — a follow-up "continue" message
/// picks the work back up with full session context, no extra machinery.
/// The final interactive round is an answer round: tool calls in it are not
/// executed (the wind-down said so). When the model calls tools anyway, keep
/// its text — often the answer with a reflexive extra lookup attached — and
/// drop the calls; with no text at all there is nothing to salvage and the
/// caller lets the round-cap wrap-up produce the answer instead.
fn strip_final_round_tool_calls(response: &LlmResponse) -> Option<LlmResponse> {
    let text_blocks: Vec<ContentBlock> = response
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Text { .. }))
        .cloned()
        .collect();
    let has_text = text_blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if !text.trim().is_empty()));
    has_text.then(|| LlmResponse {
        content: text_blocks,
        stop_reason: StopReason::EndTurn,
        usage: response.usage.clone(),
        model: response.model.clone(),
    })
}

fn wind_down_text(interactive: bool) -> String {
    if interactive {
        // The old text ended "This is your last chance to use tools" — on a
        // model already tool-looping that reads as an invitation to one more
        // call, which is unanswerable within budget: the runner then lands in
        // the forced wrap-up instead of the answer the model plausibly had.
        // The final interactive round is an ANSWER round; tool calls in it are
        // not executed (see the answer-only enforcement in both loops).
        "[SYSTEM] This is your final round. Answer the user's question NOW, \
         directly and completely, from everything you have learned above. Do \
         not call tools — tool calls in this round will not be executed. Note \
         explicitly anything you could not verify. If work remains, end by \
         telling the user: \"If you want me to keep going, say 'continue'.\""
            .to_string()
    } else {
        "[SYSTEM] You have reached your final execution round. \
         After this response, you will not be able to use any \
         more tools. Please: (1) finish or abandon your current \
         tool operations, (2) summarize what you accomplished and \
         what remains, (3) save any important state or artifacts. \
         This is your last chance to use tools."
            .to_string()
    }
}

/// The one notice a discarded text-form tool call earns.
///
/// Marked `[SYSTEM]` like every other injected directive and explicit that the
/// discarded response never reached anyone: an unmarked correction reads as the
/// human speaking and gets rebutted out loud, and a model that believes its
/// draft was delivered goes on to recap promises the reader never saw.
fn text_tool_call_correction() -> LlmMessage {
    LlmMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "[SYSTEM] Automated notice, not a message from the user: your previous \
                   response was a tool call written out as text rather than executed, so it \
                   was discarded and never delivered — nothing in it has been seen. Invoke \
                   tools through the tool-calling mechanism instead of describing them, and \
                   answer without referring to this notice."
                .to_string(),
        }],
    }
}

/// The forced wrap-up prompt once the round budget is exhausted. For an
/// interactive turn the reader asked a QUESTION — a "summary of what was
/// accomplished" is not an answer to it, so the wrap-up demands the answer
/// itself, honestly bounded by what was actually verified. Automated origins
/// keep the resumable work summary. The banner joins as an uncached third
/// segment so the stable prompt's cached segments are still read.
fn round_cap_wrapup_prompt(system_prompt: &str, max_rounds: u32, interactive: bool) -> String {
    if interactive {
        format!(
            "{system_prompt}{CACHE_BREAK_MARKER}[SYSTEM — ROUND LIMIT REACHED]\n\
             You have used all {max_rounds} execution rounds and cannot use any more tools.\n\
             Using everything you learned above, ANSWER THE USER'S QUESTION now, as \
             completely as the evidence allows. State plainly anything you could not \
             verify. If work remains, end by telling the user: \"If you want me to \
             keep going, say 'continue'.\""
        )
    } else {
        format!(
            "{system_prompt}{CACHE_BREAK_MARKER}[SYSTEM — ROUND LIMIT REACHED]\n\
             You have used all {max_rounds} execution rounds. You cannot use any more tools.\n\
             Provide a concise summary of:\n\
             1. What was accomplished\n\
             2. What remains unfinished\n\
             3. Any artifacts or state saved\n\
             4. Exactly where you stopped so work can be resumed"
        )
    }
}

/// Round 1 of a turn with history caches up to the end of the replayed prefix
/// — the only boundary the next turn sends again. Later rounds, and turns
/// without history, cache the last message as before.
fn cached_prefix_for_round(round: u32, prior_len: usize) -> Option<usize> {
    (round == 1 && prior_len > 0).then_some(prior_len)
}

/// True when the non-compactable floor (system prompt + tool schemas +
/// response reservation) alone meets or exceeds the window: even zero messages
/// overflow, so `emergency_compact_messages` (which targets `limit − reserved`)
/// can never make room and the watchdog would loop forever.
fn compaction_floor_exceeds_window(reserved: ReservedContext, context_limit: u32) -> bool {
    reserved.total() >= context_limit
}

/// The non-compactable request floor (system prompt + tool schemas + response
/// reservation) meets or exceeds the model's context window, so message
/// compaction can never make the request fit. The turn ends with guidance to
/// shrink the floor instead of looping the watchdog to an OOM (2026-06-11).
struct CompactionFutile {
    reserved: ReservedContext,
    context_limit: u32,
}

impl CompactionFutile {
    fn degraded_response(&self, model: &str) -> LlmResponse {
        degraded_overflow_response(model, self.reserved, self.context_limit)
    }
}

/// A terminal `EndTurn` response carrying [`degraded_overflow_message`]. Built
/// wherever a request can't fit the window and compaction can't recover — the
/// proactive breaker and both reactive overflow fallbacks.
fn degraded_overflow_response(
    model: &str,
    reserved: ReservedContext,
    context_limit: u32,
) -> LlmResponse {
    LlmResponse {
        content: vec![ContentBlock::Text {
            text: degraded_overflow_message(reserved, context_limit),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
        model: model.to_string(),
    }
}

/// Deliver a synthetic (non-LLM-streamed) response's text as one stream
/// segment. Consumers gate the terminal `Response` on having streamed, so a
/// degraded message produced after the turn already streamed would otherwise
/// be suppressed; streaming it here renders it and lets the trailing
/// `Response` dedupe, exactly like a real streamed answer.
async fn stream_text_segment(progress_tx: &tokio::sync::mpsc::Sender<ProgressEvent>, text: &str) {
    let _ = progress_tx.send(ProgressEvent::StreamStart).await;
    let _ = progress_tx
        .send(ProgressEvent::StreamDelta {
            text: text.to_string(),
        })
        .await;
    let _ = progress_tx.send(ProgressEvent::StreamEnd).await;
}

/// Operator-facing explanation when a request can't fit the context window and
/// compaction can't help, naming the dominant contributor so the fix is
/// actionable. A fitting overhead means the message history is the cause;
/// otherwise the tool schemas, response reservation, or system prompt dominate.
fn degraded_overflow_message(reserved: ReservedContext, context_limit: u32) -> String {
    let ReservedContext {
        system_prompt_tokens,
        tool_schema_tokens,
        response_reservation_tokens,
    } = reserved;
    let overhead = reserved.total();
    if overhead < context_limit {
        return "This conversation exceeded the context window and could not be compacted \
                further. Please start a new session with /new."
            .to_string();
    }
    if tool_schema_tokens >= system_prompt_tokens.max(response_reservation_tokens) {
        format!(
            "I can't continue this turn: the attached tool definitions are ~{tool_schema_tokens} \
             tokens and the fixed request overhead (~{overhead}) meets or exceeds this model's \
             {context_limit}-token context window, so trimming the conversation can't make room. \
             Reduce the attached tools — curate the MCP server's `tools_allow` or detach a large \
             MCP server — or switch to a larger-context model, then try again."
        )
    } else if response_reservation_tokens >= system_prompt_tokens {
        format!(
            "I can't continue this turn: the reserved response budget (~{response_reservation_tokens} \
             tokens) plus the rest of the fixed overhead (~{overhead}) meets or exceeds this model's \
             {context_limit}-token context window. Lower `max_tokens` or switch to a larger-context \
             model, then try again."
        )
    } else {
        format!(
            "I can't continue this turn: the system prompt and fixed overhead (~{overhead} tokens) \
             meet or exceed this model's {context_limit}-token context window. Switch to a \
             larger-context model, then try again."
        )
    }
}

/// The agent runner — executes the plan→policy→tool→feedback loop.
pub struct AgentRunner {
    config: RunnerConfig,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    tool_access: Arc<dyn ToolAccess>,
    image_sink: Option<Arc<dyn crate::tools::image_gen::ImageSink>>,
    image_view_sink: Option<Arc<dyn crate::tools::image_view::ImageViewSink>>,
}

impl AgentRunner {
    pub fn new(
        config: RunnerConfig,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        // Wrap the provider so every LLM request — streaming or not,
        // direct or via compaction — passes through the egress
        // sanitizer that catches any cleartext credential the ingress
        // tokenizer missed. No per-call-site wiring required.
        let llm = LastCallServed::wrap(crate::security::secret_egress::SanitizingProvider::wrap(
            llm,
        ));
        Self {
            config,
            llm,
            tools,
            policy,
            tool_access: access::noop(),
            image_sink: None,
            image_view_sink: None,
        }
    }

    /// Install a per-user tool-visibility policy. The default is a
    /// permissive no-op; the hub wires the MCP shadow / built-in
    /// fallback policy here.
    pub fn with_tool_access(mut self, access: Arc<dyn ToolAccess>) -> Self {
        self.tool_access = access;
        self
    }

    /// Install the image-delivery sink. When set, the runner delivers any
    /// images a `generate_image` tool stages — inline, as soon as the tool
    /// result lands — to the surface the turn originates from (Slack file
    /// upload, gateway attachment). The default is no sink, in which case a
    /// generated image stays in the artifact store undelivered.
    pub fn with_image_sink(mut self, sink: Arc<dyn crate::tools::image_gen::ImageSink>) -> Self {
        self.image_sink = Some(sink);
        self
    }

    /// Install the image-view sink. When set, the runner renders any file a
    /// `view_attachment` tool stages into the conversation — appended to the
    /// tool-result turn so the model sees it on its next step. The default is
    /// no sink, in which case a staged file is never rendered.
    pub fn with_image_view_sink(
        mut self,
        sink: Arc<dyn crate::tools::image_view::ImageViewSink>,
    ) -> Self {
        self.image_view_sink = Some(sink);
        self
    }

    /// Deliver any images staged by the given round's tool invocations to the
    /// active channel surface. Called from *both* run loops (non-streaming and
    /// streaming) so a generated image reaches the user regardless of which
    /// path served the turn — delivered inline (before the agent's text) so the
    /// image shows first.
    async fn deliver_round_images(&self, round_invocations: &[ToolInvocation]) {
        if let Some(sink) = &self.image_sink {
            deliver_generated(sink.as_ref(), &SessionContext::capture(), round_invocations).await;
        }
    }

    /// Render any images staged by the given round's tools into the
    /// conversation, appending the resulting content blocks to the most recent
    /// message (the tool-result turn). Called from *both* run loops with the
    /// model that will consume the appended blocks, so vision capability is
    /// resolved against the right model.
    async fn append_viewed_images(
        &self,
        messages: &mut [LlmMessage],
        round_invocations: &[ToolInvocation],
        active_model: &str,
    ) {
        let Some(sink) = &self.image_view_sink else {
            return;
        };
        let Some(target) = messages.last_mut() else {
            return;
        };
        debug_assert!(
            target.role == Role::User
                && target
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
            "append_viewed_images expects the last message to be the tool-result turn"
        );
        target.content.extend(
            render_viewed(
                sink.as_ref(),
                &SessionContext::capture(),
                round_invocations,
                active_model,
            )
            .await,
        );
    }

    /// Estimate the fixed per-request overhead — system prompt, tool schemas,
    /// and response reservation — for the given model. Used to make compaction
    /// thresholds reflect true context usage, not message tokens alone.
    pub fn reserved_context_for(&self, model: &str) -> ReservedContext {
        self.reserved_context_for_with(model, &self.offered_for(model))
    }

    /// Like [`Self::reserved_context_for`] but over what a turn actually
    /// offers — its catalogue and the tools it has loaded — so the mid-turn
    /// watchdog measures the real per-request footprint.
    fn reserved_context_for_with(&self, model: &str, offered: &OfferedTools) -> ReservedContext {
        ReservedContext {
            system_prompt_tokens: estimate_tokens(&offered.prompt(&self.config.system_prompt)),
            tool_schema_tokens: offered
                .defs
                .iter()
                .map(crate::tools::loading::definition_tokens)
                .sum(),
            response_reservation_tokens: self.config.effective_max_tokens(model).unwrap_or(8192),
        }
    }

    /// Resolve the model's context limit, preferring the limit the provider
    /// reported in `error`, then the static registry, then the configured
    /// ceiling. The provider value wins because it reflects the endpoint that
    /// just rejected the request, which may differ from the registry.
    fn resolve_context_limit(&self, model: &str, error: &str) -> u32 {
        Self::parse_context_limit(error)
            .or_else(|| crate::llm::model_limits::context_for(model))
            .unwrap_or(self.config.compaction.max_context_tokens)
    }

    /// Compact mid-turn if the next call is predicted to breach the urgent
    /// context fraction, so a long tool-using turn can't grow unbounded into a
    /// provider overflow. `last_observed` feeds the prediction the previous
    /// round's real prompt size; estimate-only when `None`.
    fn run_context_watchdog(
        &self,
        messages: &mut Vec<LlmMessage>,
        offered: &OfferedTools,
        model: &str,
        round: u32,
        last_observed: Option<ObservedRound>,
    ) -> Result<(), CompactionFutile> {
        let context_limit =
            crate::llm::model_limits::effective_context_for_model(&self.config.compaction, model);
        let reserved = self.reserved_context_for_with(model, offered);
        let urgent_ratio = self.config.compaction.urgent_compaction_threshold;

        // Soft stage: over the eviction threshold, relieve pressure by
        // shrinking stale in-flight tool outputs. Batched here — under
        // pressure — rather than every round, so below the threshold the
        // message-cache prefix is never touched (under *sustained* pressure
        // newly-aged messages still shrink each round, matching the old
        // per-round prune only in that regime). Clamped below the urgent
        // ratio so a misconfigured soft ≥ urgent still lets eviction run
        // before summary compaction.
        let soft_ratio = self
            .config
            .compaction
            .soft_eviction_threshold
            .min(urgent_ratio);
        let mut last_observed = last_observed;
        if watchdog_should_compact(messages, reserved, context_limit, soft_ratio, last_observed)
            .is_some()
        {
            let before = estimate_messages_tokens(messages);
            let keep_last = self.config.context_eviction_keep_last as usize;
            let (evicted, _) = Self::evict_stale_tool_outputs(messages, keep_last);
            Self::prune_consumed_tool_blocks(messages, 4);
            let freed = before.saturating_sub(estimate_messages_tokens(messages));
            if freed > 0 {
                info!(
                    round,
                    evicted,
                    freed_tokens_est = freed,
                    "Context watchdog — evicted stale tool outputs under soft pressure"
                );
                // The observed baseline was measured against the un-evicted
                // history; discount what eviction freed so a stale observation
                // can't force a premature urgent compaction. `freed` is
                // estimate-denominated (chars/4) while the baseline is
                // provider-real, so on dense content the discount understates
                // the real relief and urgent compaction can still fire — the
                // safe direction. The opposite error (over-discount via the
                // appended-since-observation slice) is caught by the reactive
                // post-overflow path in both loops.
                last_observed = last_observed.map(|o| o.discounted(freed));
            }
        }

        let Some(predicted) = watchdog_should_compact(
            messages,
            reserved,
            context_limit,
            urgent_ratio,
            last_observed,
        ) else {
            return Ok(());
        };
        // The compactor's trim target is estimate-denominated. When the
        // observed prediction runs ahead of the estimate (the len/4 drift),
        // deflate the window handed to the compactor by the same ratio — an
        // unscaled target reads the drifted history as already small enough
        // and trims nothing, and the next call still overflows.
        let estimated_messages = estimate_messages_tokens(messages);
        let predicted_messages = predicted.saturating_sub(reserved.total()).max(1);
        let deflation = (estimated_messages as f32 / predicted_messages as f32).min(1.0);
        let available = context_limit.saturating_sub(reserved.total()) as f32;
        let trim_limit = reserved
            .total()
            .saturating_add((available * deflation) as u32);
        // Only log when a trim actually happened — once a near-limit tail is
        // already at target, the predicate keeps firing but compaction is a
        // no-op, and an unconditional warn would spam every round.
        if Self::emergency_compact_messages(messages, trim_limit, reserved) {
            warn!(
                round,
                predicted_tokens = predicted,
                context_limit,
                "Mid-turn context watchdog — compacted before next call"
            );
        }
        // The non-compactable floor: when it alone meets the window, no
        // compaction can make room — abort the turn instead of looping
        // (2026-06-11 OOM).
        let floor = reserved;
        if compaction_floor_exceeds_window(floor, context_limit) {
            return Err(CompactionFutile {
                reserved: floor,
                context_limit,
            });
        }
        Ok(())
    }

    /// Create a compaction engine using this runner's LLM and config.
    /// The compaction config with `max_context_tokens` narrowed to what this
    /// agent's model can actually hold.
    ///
    /// The engine re-checks `needs_compaction` against its own config, so
    /// handing it the raw configured ceiling makes it disagree with the
    /// model-aware check the caller already ran: on a model whose real window
    /// is smaller than the configured ceiling, the caller says "compact" and
    /// the engine says "no need" and returns early — the session then never
    /// compacts and overflows for real. Narrow it once, here, so every engine
    /// the runner hands out measures against the true window.
    fn model_aware_compaction(&self) -> workforce_common::session::CompactionConfig {
        let mut compaction = self.config.compaction.clone();
        // No model in scope ⇒ nothing to narrow against; `model_limits` returns
        // `None` for the empty name and the configured ceiling stands.
        let model = self.config.model.as_deref().unwrap_or_default();
        compaction.max_context_tokens =
            crate::llm::model_limits::effective_context_limit(model, compaction.max_context_tokens);
        compaction
    }

    pub fn compaction_engine(&self) -> CompactionEngine {
        CompactionEngine::new(
            self.model_aware_compaction(),
            Arc::clone(&self.llm),
            self.config.model.clone(),
        )
    }

    /// Create a compaction engine with a memory writer for persisting flush output.
    pub fn compaction_engine_with_writer(
        &self,
        memory_writer: Arc<dyn crate::compaction::MemoryWriter>,
    ) -> CompactionEngine {
        CompactionEngine::with_memory_writer(
            self.model_aware_compaction(),
            Arc::clone(&self.llm),
            self.config.model.clone(),
            memory_writer,
        )
    }

    /// Classify why the prompt overflowed. Determines whether the agent
    /// gets a "your tool returned too much data" note or a "we dropped
    /// older messages" note. See incident 2026-05-18 — a single oversize
    /// `code.search` result triggered the historical "SEVERE CONTEXT
    /// LOSS" wording, which misled the agent into apologising for a
    /// conversation that was never lost.
    fn classify_overflow(messages: &[LlmMessage], msg_tokens: &[u32]) -> OverflowCause {
        let total: u32 = msg_tokens.iter().sum();
        if total == 0 {
            return OverflowCause::HistoryGrowth;
        }
        let last_user_tool_tokens: u32 = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { content, .. } => Some(estimate_tokens(content)),
                        _ => None,
                    })
                    .sum()
            })
            .unwrap_or(0);
        if last_user_tool_tokens.saturating_mul(2) > total {
            OverflowCause::ToolSpam
        } else {
            OverflowCause::HistoryGrowth
        }
    }

    /// Parse the context-window limit out of a provider overflow error.
    fn parse_context_limit(error: &str) -> Option<u32> {
        let lower = error.to_lowercase();
        CONTEXT_LIMIT_EXTRACTORS
            .iter()
            .find_map(|extractor| extract_limit(&lower, extractor))
    }

    /// Emergency compaction: drop old messages to fit within context limits.
    ///
    /// Keeps the first 2 messages (task context) + last N messages (recent
    /// context), drops everything in between, and inserts a system note.
    /// The target is computed against the space left for messages after the
    /// fixed `reserved` overhead (system prompt, tool schemas, response
    /// reservation) is subtracted from `context_limit` — message tokens alone
    /// would otherwise be measured against a budget the overhead already spent.
    /// Returns `true` if messages were actually trimmed.
    fn emergency_compact_messages(
        messages: &mut Vec<LlmMessage>,
        context_limit: u32,
        reserved: ReservedContext,
    ) -> bool {
        const SAFETY_MARGIN: f32 = 0.85;
        let available_for_messages = context_limit.saturating_sub(reserved.total());
        let target_tokens = (available_for_messages as f32 * SAFETY_MARGIN) as u32;

        // Estimate tokens per message (floored at 1 so empty messages still count).
        let msg_tokens: Vec<u32> = messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(estimate_block_tokens)
                    .sum::<u32>()
                    .max(1)
            })
            .collect();

        let total_tokens: u32 = msg_tokens.iter().sum();
        if total_tokens <= target_tokens {
            return false; // Already under target
        }
        if messages.len() <= 4 {
            return Self::truncate_large_blocks(messages, target_tokens);
        }

        // If ONE message is responsible for the overflow, shrink it instead of
        // evicting history. The keep-front/keep-back split below is blind to
        // size, so a huge *recent* message (a 100K-char attachment dump, a
        // giant tool result) survives as part of the "recent" tail and eats the
        // whole budget — `kept_back` collapses to 1-2 and the turn's actual
        // findings are dropped to make room for the very thing that overflowed.
        //
        // The gate is deliberately narrow: everything else must already fit
        // without it. When the conversation is *collectively* too big, dropping
        // the middle really is the right move and this must not pre-empt it.
        // Index 0 is the task message and is never shrunk (it anchors the
        // turn), matching `truncate_large_blocks`.
        let hog = msg_tokens
            .iter()
            .enumerate()
            .skip(1)
            .max_by_key(|(_, t)| **t)
            .map(|(i, t)| (i, *t));
        if let Some((hog_idx, hog_tokens)) = hog {
            let without_hog = total_tokens.saturating_sub(hog_tokens);
            if without_hog <= target_tokens {
                // Shrink THIS message, not "whatever is biggest scanning from
                // the tail" — the generic sweep would happily clip the freshest
                // tool results to spare a mid-conversation hog. Aim 10% under
                // the target so the estimate-based clip lands clear of it
                // rather than flush against it.
                let hog_target = target_tokens.saturating_sub(target_tokens / 10);
                let excess = total_tokens.saturating_sub(hog_target);
                let saved = Self::shrink_message_blocks(&mut messages[hog_idx], excess);
                let after = estimate_messages_tokens(messages);
                if saved > 0 && after <= target_tokens {
                    warn!(
                        hog_index = hog_idx,
                        largest_message_tokens = hog_tokens,
                        tokens_saved = saved,
                        tokens_after = after,
                        target_tokens = target_tokens,
                        "Emergency compaction: truncated one oversized message instead of dropping history"
                    );
                    return true;
                }
            }
        }

        // Re-measure: the hog pass above may have changed message sizes.
        let msg_tokens: Vec<u32> = messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(estimate_block_tokens)
                    .sum::<u32>()
                    .max(1)
            })
            .collect();

        let cause = Self::classify_overflow(messages, &msg_tokens);
        let note = || LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: cause.emergency_note().to_string(),
            }],
        };

        let keep_front = 2.min(messages.len());
        let keep_back = 10.min(messages.len().saturating_sub(keep_front));
        let front_tokens: u32 = msg_tokens[..keep_front].iter().sum();
        let back_start = messages.len() - keep_back;
        let back_tokens: u32 = msg_tokens[back_start..].iter().sum();

        if front_tokens + back_tokens > target_tokens {
            let budget = target_tokens.saturating_sub(front_tokens);
            let mut kept_back = 0usize;
            let mut running = 0u32;
            for i in (back_start..messages.len()).rev() {
                running += msg_tokens[i];
                if running > budget {
                    break;
                }
                kept_back += 1;
            }
            if kept_back == 0 {
                return Self::nuclear_compact(messages, &msg_tokens, target_tokens, cause);
            }
            let actual_back_start = messages.len() - kept_back;
            let mut compacted = Vec::with_capacity(keep_front + 1 + kept_back);
            compacted.extend_from_slice(&messages[..keep_front]);
            compacted.push(note());
            compacted.extend_from_slice(&messages[actual_back_start..]);
            let old_len = messages.len();
            *messages = compacted;
            Self::strip_orphaned_tool_blocks(messages);
            warn!(
                old_messages = old_len,
                new_messages = messages.len(),
                context_limit = context_limit,
                cause = ?cause,
                "Emergency compaction: dropped middle messages"
            );
            return true;
        }

        let mut compacted = Vec::with_capacity(keep_front + 1 + keep_back);
        compacted.extend_from_slice(&messages[..keep_front]);
        compacted.push(note());
        compacted.extend_from_slice(&messages[back_start..]);
        let old_len = messages.len();
        *messages = compacted;
        Self::strip_orphaned_tool_blocks(messages);
        warn!(
            old_messages = old_len,
            new_messages = messages.len(),
            context_limit = context_limit,
            cause = ?cause,
            "Emergency compaction: dropped middle messages"
        );
        true
    }

    /// Truncate content blocks in a message to fit within a character budget.
    fn truncate_message(message: &mut LlmMessage, max_chars: usize) {
        for block in &mut message.content {
            match block {
                ContentBlock::Text { text } => {
                    if text.len() > max_chars {
                        // Find a valid char boundary
                        let mut end = max_chars;
                        while !text.is_char_boundary(end) && end > 0 {
                            end -= 1;
                        }
                        text.truncate(end);
                        text.push_str("... [truncated]");
                    }
                }
                ContentBlock::Image { .. } => {
                    *block = ContentBlock::Text {
                        text: "[image removed to fit context window]".to_string(),
                    };
                }
                ContentBlock::Document { .. } => {
                    *block = ContentBlock::Text {
                        text: "[document removed to fit context window]".to_string(),
                    };
                }
                ContentBlock::ToolUse { input, .. } => {
                    let s = input.to_string();
                    if s.len() > max_chars {
                        *input = serde_json::json!({"truncated": true});
                    }
                }
                ContentBlock::ToolResult { content, .. } => {
                    if content.len() > max_chars {
                        let mut end = max_chars;
                        while !content.is_char_boundary(end) && end > 0 {
                            end -= 1;
                        }
                        content.truncate(end);
                        content.push_str("... [truncated]");
                    }
                }
                // Thinking blocks must be preserved as-is for API echo-back
                ContentBlock::Thinking { .. } => {}
            }
        }
    }

    /// Last-resort compaction: keep only the last user message (truncated)
    /// and a system note. Always returns `true`. `cause` drives the wording
    /// of the system note so the agent reacts to the right failure mode.
    fn nuclear_compact(
        messages: &mut Vec<LlmMessage>,
        _msg_tokens: &[u32],
        target_tokens: u32,
        cause: OverflowCause,
    ) -> bool {
        let max_chars = (target_tokens as usize) * 4;

        let last_user_idx = messages.iter().rposition(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
        });

        let mut compacted = Vec::with_capacity(3);

        compacted.push(LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: cause.nuclear_user_note().to_string(),
            }],
        });
        compacted.push(LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: cause.nuclear_assistant_ack().to_string(),
            }],
        });

        if let Some(idx) = last_user_idx {
            let mut last_msg = messages[idx].clone();
            Self::truncate_message(&mut last_msg, max_chars);
            compacted.push(last_msg);
        } else {
            compacted.push(LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "[SYSTEM] No user message could be preserved. Please ask the user \
                           to repeat their request."
                        .to_string(),
                }],
            });
        }

        warn!(
            old_messages = messages.len(),
            new_messages = compacted.len(),
            cause = ?cause,
            "Nuclear compaction: last user message preserved"
        );
        *messages = compacted;
        Self::strip_orphaned_tool_blocks(messages);
        true
    }

    /// Collect every `tool_use_id` referenced by a `ToolResult` block. Used
    /// both to detect consumed rounds (the `ToolUse` whose `id` appears here
    /// has been answered) and to detect orphan-stripping candidates.
    fn collect_consumed_tool_use_ids(messages: &[LlmMessage]) -> std::collections::HashSet<String> {
        messages
            .iter()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Shrink consumed tool-use blocks to bound context growth.
    ///
    /// Keeps the last `keep_recent` messages at full fidelity. For older
    /// messages, truncates `ToolResult.content` to 500 chars and shrinks
    /// `ToolUse.input` for blocks whose `id` already appears in a later
    /// `ToolResult` (i.e. the round has been consumed). Idempotent.
    fn prune_consumed_tool_blocks(messages: &mut [LlmMessage], keep_recent: usize) {
        use workforce_common::tool_input::shrink_tool_input;

        const TOOL_USE_INPUT_FIELD_MAX: usize = 256;
        const TOOL_USE_INPUT_MAX: usize = 1024;
        const TOOL_RESULT_MAX: usize = 500;

        let consumed = Self::collect_consumed_tool_use_ids(messages);
        let boundary = messages.len().saturating_sub(keep_recent);

        for msg in &mut messages[..boundary] {
            for block in &mut msg.content {
                match block {
                    ContentBlock::ToolResult {
                        content,
                        loaded: None,
                        ..
                    } if content.len() > TOOL_RESULT_MAX => {
                        let mut end = TOOL_RESULT_MAX;
                        while !content.is_char_boundary(end) && end > 0 {
                            end -= 1;
                        }
                        content.truncate(end);
                        content.push_str("...[output truncated — already consumed by agent]");
                    }
                    ContentBlock::ToolUse { id, input, .. } if consumed.contains(id) => {
                        shrink_tool_input(input, TOOL_USE_INPUT_FIELD_MAX, TOOL_USE_INPUT_MAX);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Truncate the largest content blocks to fit within the target token budget.
    ///
    /// Used when there are too few messages to drop (≤4) but content is too large.
    /// Iterates blocks in reverse order (most recent first), truncating text and
    /// tool results, removing images/documents, until the budget is met.
    fn truncate_large_blocks(messages: &mut [LlmMessage], target_tokens: u32) -> bool {
        let total = estimate_messages_tokens(messages);
        let mut excess = total.saturating_sub(target_tokens);
        if excess == 0 {
            return false;
        }

        // Truncate largest blocks first (skip first message = task context)
        for msg in messages.iter_mut().skip(1).rev() {
            if excess == 0 {
                break;
            }
            excess = excess.saturating_sub(Self::shrink_message_blocks(msg, excess));
        }
        warn!(remaining_excess = excess, "truncate_large_blocks completed");
        true
    }

    /// Shrink the oversized blocks of ONE message, reclaiming at most `excess`
    /// tokens. Returns how many tokens were actually reclaimed.
    ///
    /// Split out so the emergency-compaction pre-pass can target a specific
    /// offending message instead of scanning from the tail — clipping whatever
    /// happens to be most recent is how you destroy a turn's freshest tool
    /// output while the actual hog sails through untouched.
    fn shrink_message_blocks(msg: &mut LlmMessage, excess: u32) -> u32 {
        let mut remaining = excess;
        let mut saved_total = 0u32;
        for block in msg.content.iter_mut() {
            if remaining == 0 {
                break;
            }
            let saved = match block {
                ContentBlock::Text { text } if estimate_tokens(text) > 500 => {
                    Self::clip_text(text, remaining)
                }
                ContentBlock::ToolResult { content, .. } if estimate_tokens(content) > 500 => {
                    Self::clip_text(content, remaining)
                }
                ContentBlock::ToolUse { input, .. }
                    if estimate_tokens(&input.to_string()) > 500 =>
                {
                    let outcome = workforce_common::tool_input::shrink_tool_input(input, 128, 512);
                    if outcome.shrunk {
                        ((outcome.bytes_before - outcome.bytes_after) / 4) as u32
                    } else {
                        0
                    }
                }
                ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                    *block = ContentBlock::Text {
                        text: "[attachment removed to fit context window]".to_string(),
                    };
                    1000
                }
                _ => 0,
            };
            saved_total += saved;
            remaining = remaining.saturating_sub(saved);
        }
        saved_total
    }

    /// Clip `text` down by up to `budget` tokens, on a char boundary, leaving a
    /// truncation marker. Returns the tokens reclaimed.
    fn clip_text(text: &mut String, budget: u32) -> u32 {
        let current = estimate_tokens(text);
        let keep_tokens = current.saturating_sub(budget);
        let max_chars = (keep_tokens as usize * 4).max(200);
        if text.len() <= max_chars {
            return 0;
        }
        let mut end = max_chars;
        while !text.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        let saved = current.saturating_sub(estimate_tokens(&text[..end]));
        text.truncate(end);
        text.push_str("\n[truncated to fit context window]");
        saved
    }

    /// Remove orphaned tool blocks after compaction, enforcing the two API
    /// invariants: every `tool_use` is answered by a `tool_result` in the
    /// **immediately following** user message, and every `tool_result`
    /// references a `tool_use` that still exists.
    ///
    /// Adjacency is the subtle one: `emergency_compact_messages`/`nuclear_compact`
    /// can *insert* a system note between an assistant `tool_use` turn and its
    /// `tool_result` turn. The result still exists further down, so a
    /// presence-anywhere check would keep the now-unanswered `tool_use` and the
    /// API would reject the request.
    fn strip_orphaned_tool_blocks(messages: &mut Vec<LlmMessage>) {
        // `tool_use` ids that are answered by the immediately following user
        // turn. This is exactly the set that survives invariant #2, and (since
        // a surviving `tool_use` is what a `tool_result` must reference) the set
        // that satisfies invariant #1 too.
        let answered: std::collections::HashSet<String> = messages
            .windows(2)
            .filter(|w| w[0].role == Role::Assistant && w[1].role == Role::User)
            .flat_map(|w| {
                let results: std::collections::HashSet<&str> = w[1]
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                        _ => None,
                    })
                    .collect();
                w[0].content.iter().filter_map(move |b| match b {
                    ContentBlock::ToolUse { id, .. } if results.contains(id.as_str()) => {
                        Some(id.clone())
                    }
                    _ => None,
                })
            })
            .collect();

        for msg in messages.iter_mut() {
            msg.content.retain(|block| match block {
                ContentBlock::ToolUse { id, .. } => answered.contains(id),
                ContentBlock::ToolResult { tool_use_id, .. } => answered.contains(tool_use_id),
                _ => true,
            });
        }

        messages.retain(|m| !m.content.is_empty());
    }

    /// Run the agent on a task and return the result.
    pub async fn run(&self, task: &str) -> Result<RunResult> {
        self.run_with_history(task, Vec::new()).await
    }

    /// Run the agent with conversation history from previous turns.
    pub async fn run_with_history(
        &self,
        task: &str,
        prior_messages: Vec<LlmMessage>,
    ) -> Result<RunResult> {
        let content = vec![ContentBlock::Text {
            text: task.to_string(),
        }];
        self.run_with_history_content(content, prior_messages).await
    }

    /// Run the agent with conversation history, accepting rich content blocks
    /// (text + images) as the user message.
    pub async fn run_with_history_content(
        &self,
        content: Vec<ContentBlock>,
        prior_messages: Vec<LlmMessage>,
    ) -> Result<RunResult> {
        let _turn = TurnGuard::begin();
        let mut tel = crate::turn_event::TurnTelemetry::start(&self.config);
        tel.set_task(&content);
        let result =
            Box::pin(self.run_with_history_content_inner(content, prior_messages, &mut tel)).await;
        tel.emit(&result);
        result
    }

    /// The tools a turn offers before its plan is worked out: the runner's own
    /// `program_run` when this agent runs programs.
    fn offered_tools(&self) -> OfferedTools {
        OfferedTools::with_intrinsics(
            self.config
                .programs
                .iter()
                .map(|_| {
                    let def = crate::tools::program::program_tool_definition();
                    LlmToolDef {
                        name: to_llm_name(&def.name),
                        description: def.description,
                        input_schema: def.input_schema,
                    }
                })
                .collect(),
        )
    }

    /// The program context a `program_run` call in this turn runs with.
    fn program_context(
        &self,
        offered: &OfferedTools,
        access: &Arc<ToolAccessSnapshot>,
        messages: &[LlmMessage],
    ) -> Option<crate::tools::program::ProgramContext> {
        self.config
            .programs
            .as_ref()
            .map(|programs| crate::tools::program::ProgramContext {
                gates: ToolGates::from(&self.config),
                policy: Arc::clone(&self.policy),
                tools: Arc::clone(&self.tools),
                access: Arc::clone(access),
                plan: offered.plan.clone(),
                programs: Arc::clone(programs),
                earlier: crate::tools::program::earlier_programs(messages),
            })
    }

    /// The journalled call a round's tool use runs as, minted the first time
    /// the round asks for it with the input the transcript keeps.
    fn call_for(
        &self,
        calls: &mut HashMap<String, ToolCall>,
        tool_use_id: &str,
        input: &serde_json::Value,
    ) -> ToolCall {
        calls
            .entry(tool_use_id.to_string())
            .or_insert_with(|| ToolCall::new(self.config.journal.clone(), input.clone()))
            .clone()
    }

    /// Whether policy, as it stands now, denies `tool_name` with `input`: the
    /// check for a call decided earlier and run only now.
    fn denied_now(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        let request = PolicyRequest {
            agent_id: self.config.agent_id.clone(),
            tool_name: tool_name.to_string(),
            parameters: input.clone(),
            context: PolicyContext {
                node_id: None,
                team_id: self.config.team_id.clone(),
                task_id: None,
                role: self.config.role.clone(),
            },
        };
        apply_cascade_gate(
            &ToolGates::from(&self.config),
            self.policy.evaluate(&request),
            tool_name,
        )
        .decision
        .is_denied()
    }

    /// Seed the calls this run starts with after its task: the ones
    /// [`RunnerConfig::seeds`] already made, else [`Self::seed`]'s.
    async fn seed_calls(
        &self,
        messages: &mut Vec<LlmMessage>,
        invocations: &mut Vec<ToolInvocation>,
        progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) {
        let seeds = match &self.config.seeds {
            Some(seeds) => seeds.clone(),
            None => self.seed(progress, cancel).await,
        };
        messages.extend(seeds.messages);
        invocations.extend(seeds.invocations);
    }

    /// Make the calls a turn starts with, before its first round: the one a
    /// human just approved, then each an interrupted turn left running. Each
    /// becomes a call and result pair, through the same output handling as
    /// any call, and journalled. Returns early, leaving the rest, once the
    /// turn is cancelled.
    pub async fn seed(
        &self,
        progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Seeds {
        let mut seeds = Seeds::default();
        let approved = self.config.preexecute.iter().map(Seeded::Approved);
        let collected = self.config.collect.iter().map(Seeded::Collected);
        for seeded in approved.chain(collected) {
            let run = self.seed_call(
                &mut seeds.messages,
                &mut seeds.invocations,
                seeded,
                progress,
            );
            match cancel {
                Some(cancel) => tokio::select! {
                    biased;
                    () = cancel.cancelled() => return seeds,
                    () = run => {}
                },
                None => run.await,
            }
        }
        seeds
    }

    async fn seed_call(
        &self,
        messages: &mut Vec<LlmMessage>,
        invocations: &mut Vec<ToolInvocation>,
        seeded: Seeded<'_>,
        progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) {
        let (tool_name, input) = seeded.call();
        let journal = self.config.journal.clone();
        let call = match seeded {
            Seeded::Approved(_) => ToolCall::new(journal, input.clone()),
            Seeded::Collected(pending) => {
                ToolCall::resumed(pending.call.clone(), journal, input.clone())
            }
        };
        if let Some(progress) = progress {
            let _ = progress
                .send(ProgressEvent::ToolStart {
                    name: tool_name.to_string(),
                    round: 0,
                    detail: approval_detail(&self.tools, tool_name, input),
                })
                .await;
        }
        let started = std::time::Instant::now();
        let tool = self.tools.get(tool_name);
        let result = match &tool {
            _ if self.denied_now(tool_name, input) => {
                if let (Some(tool), Seeded::Collected(pending)) = (&tool, &seeded) {
                    tool.abandon(pending.plan.clone(), pending.checkpoint.clone())
                        .await;
                }
                ToolResult::failure(NO_LONGER_PERMITTED, 0)
            }
            None => ToolResult::failure(tool_not_found_message(tool_name, &self.tools), 0),
            Some(tool) => {
                // Sink calls need marker → cleartext substitution at execution
                // time; the transcript keeps `input`, markers intact.
                let sub = crate::security::secret_sinks::substitute_secret_markers(
                    tool_name,
                    input.clone(),
                    &CURRENT_SESSION_KEY
                        .try_with(|k| k.clone())
                        .unwrap_or_default(),
                );
                if !sub.unknown.is_empty() {
                    ToolResult::failure(format!("BLOCKED: {}", sub.unknown_reason()), 0)
                } else {
                    let run = async {
                        match &seeded {
                            Seeded::Approved(_) => execute_with_deadline(tool, sub.input).await,
                            Seeded::Collected(pending) => {
                                tool.collect(
                                    sub.input,
                                    pending.plan.clone(),
                                    pending.checkpoint.clone(),
                                )
                                .await
                            }
                        }
                    };
                    SessionContext::capture()
                        .scope(call.clone().scope(run))
                        .await
                        .unwrap_or_else(|e| ToolResult::failure(format!("Tool error: {e}"), 0))
                }
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let output_str = match result.success {
            true => serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string()),
            false => result.error.unwrap_or_else(|| "Tool failed".to_string()),
        };
        let (output_str, output) = crate::spill::spill_tool_output(
            tool_name,
            output_str,
            result.output,
            !result.success,
            &self.config.spill,
            crate::spill::RetrievalSurface::from_registry(&self.tools),
        );
        let invocation = ToolInvocation {
            tool_name: tool_name.to_string(),
            input: input.clone(),
            output,
            success: result.success,
            policy_decision: seeded.decision().to_string(),
            duration_ms,
            denial_reason: None,
            error: (!result.success).then(|| output_str.clone()),
        };
        call.finished(&invocation).await;
        if let Some(progress) = progress {
            let _ = progress
                .send(ProgressEvent::ToolEnd {
                    name: tool_name.to_string(),
                    success: result.success,
                    duration_ms,
                    round: 0,
                })
                .await;
        }
        // Named as the model names its tools: a provider refuses a call
        // under a name no tool it was offered has.
        let tool_use_id = format!("{}-{}", seeded.decision(), uuid::Uuid::new_v4());
        messages.push(LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                tool_use_id.clone(),
                to_llm_name(tool_name),
                input.clone(),
            )],
        });
        messages.push(LlmMessage::tool_results(tokenize_tool_outputs(vec![(
            tool_use_id,
            tool_name.to_string(),
            output_str,
            !result.success,
        )])));
        invocations.push(invocation);
    }

    async fn run_with_history_content_inner(
        &self,
        content: Vec<ContentBlock>,
        prior_messages: Vec<LlmMessage>,
        tel: &mut crate::turn_event::TurnTelemetry,
    ) -> Result<RunResult> {
        let prior_len = prior_messages.len();
        let mut messages = prior_messages;
        messages.push(LlmMessage {
            role: Role::User,
            content,
        });
        let mut total_usage = TokenUsage::default();
        let mut tool_invocations: Vec<ToolInvocation> = Vec::new();
        self.seed_calls(&mut messages, &mut tool_invocations, None, None)
            .await;
        let mut compile_gate = self.gate_turn(&tool_invocations);
        let mut rounds = 0;
        let mut model_switches: Vec<ModelSwitchRecord> = Vec::new();
        let start_time = std::time::Instant::now();
        let mut round_details: Vec<RoundDetail> = Vec::new();
        let run_model = self.config.model.clone().unwrap_or_default();
        let mut actual_model = run_model.clone();
        let mut halfway_warned = false;
        let mut tool_hallucination_retries: u32 = 0;
        let mut last_observed: Option<ObservedRound> = None;
        let mut stop_after_next_round = false;

        // Initialize adaptive model router (DW-252) with tier registry for
        // provider-agnostic routing decisions.
        let mut model_router = crate::model_router::ModelRouter::with_tiers(
            self.config.routing.clone(),
            self.config.tier_registry.clone(),
            None, // task_type not available here yet - will enhance in follow-up
            run_model.clone(),
        );
        let mut current_model = run_model.clone();

        // ── Trace capture: write header ──
        let trace_start = chrono::Utc::now();
        if let Some(ref tw) = self.config.trace_writer {
            let header = self.trace_header(tel.trace_id(), &current_model, trace_start);
            if let Err(e) = tw.write_header(&header) {
                warn!(error = %e, "Failed to write trace header");
            }
        }

        info!(
            agent = %self.config.agent_id,
            history = messages.len(),
            tools = self.tools.len(),
            model = %current_model,
            "Starting agent run"
        );

        let session = session_tools();
        let mut offered = self.offered_tools();
        let mut program_nudge =
            crate::tools::program::ProgramNudge::new(self.config.programs.as_ref());
        let mut prefix: Option<PrefixFingerprint> = None;

        loop {
            rounds += 1;
            tel.set_rounds(rounds);

            let context_floor = self.llm.context_floor(&current_model).await;
            let tools_rebuilt = offered.refresh(
                &self.tools,
                self.tool_access.as_ref(),
                &self.plan_context(&session, context_floor),
            );
            if rounds == 1 {
                offered.log_plan(self.config.agent_id.as_str());
            }
            let access_snapshot = Arc::clone(&offered.access);

            // Proportional halfway warning (DW-419)
            let halfway = self.config.max_rounds / 2;
            if !halfway_warned
                && halfway > 0
                && rounds >= halfway
                && rounds < self.config.max_rounds
            {
                halfway_warned = true;
                info!(
                    agent = %self.config.agent_id,
                    round = rounds,
                    max_rounds = self.config.max_rounds,
                    "Halfway round budget warning",
                );
                messages.push(LlmMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[SYSTEM: ⚠️ You've used {} of {} rounds. \
                             Shift from exploration to execution — start producing output now.]",
                            rounds, self.config.max_rounds
                        ),
                    }],
                });
            }

            if rounds > self.config.max_rounds {
                // Log tool usage breakdown for diagnostics
                let mut tool_counts: HashMap<&str, usize> = HashMap::new();
                for inv in &tool_invocations {
                    *tool_counts.entry(&inv.tool_name).or_insert(0) += 1;
                }
                warn!(
                    agent = %self.config.agent_id,
                    rounds = rounds,
                    tool_summary = ?tool_counts,
                    "Max rounds exceeded — tool usage breakdown"
                );
                let notice = match self
                    .gate_answer(
                        &mut compile_gate,
                        &access_snapshot,
                        rounds,
                        false,
                        None,
                        None,
                    )
                    .await
                {
                    Verdict::Finish(notice) => notice,
                    Verdict::Fix(_) | Verdict::Cancelled => None,
                };

                // Ask LLM for the wrap-up without tools so the user gets useful
                // output — for an interactive turn, that wrap-up IS the answer.
                let summary_prompt = round_cap_wrapup_prompt(
                    &self.config.system_prompt,
                    self.config.max_rounds,
                    self.config.interactive,
                );
                match self
                    .llm
                    .create_message(
                        Some(&summary_prompt),
                        &messages,
                        &[],
                        Some(&current_model),
                        self.config.effective_max_tokens(&current_model),
                        true, // enable cache for summary call (multi-round task)
                    )
                    .await
                {
                    Ok(resp) => {
                        let text = resp.text().unwrap_or_default();
                        total_usage.input_tokens += resp.usage.input_tokens;
                        total_usage.output_tokens += resp.usage.output_tokens;
                        total_usage.cache_creation_input_tokens +=
                            resp.usage.cache_creation_input_tokens;
                        total_usage.cache_read_input_tokens += resp.usage.cache_read_input_tokens;
                        let duration_ms = start_time.elapsed().as_millis() as u64;
                        // Trace footer: max rounds exceeded
                        if let Some(ref tw) = self.config.trace_writer {
                            if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                                outcome: crate::trace::TraceOutcome::MaxRoundsExceeded,
                                completed_at: chrono::Utc::now(),
                                total_rounds: rounds,
                                total_input_tokens: total_usage.input_tokens,
                                total_output_tokens: total_usage.output_tokens,
                                total_duration_ms: duration_ms,
                            }) {
                                warn!(error = %e, "Failed to write trace footer");
                            }
                        }
                        let text = if text.is_empty() {
                            "Agent reached maximum number of tool execution rounds.".to_string()
                        } else {
                            text
                        };
                        return Ok(RunResult {
                            response: with_notice(text, notice.as_deref()).0,
                            total_usage,
                            rounds,
                            tool_invocations,
                            round_details,
                            duration_ms,
                            model: actual_model,
                            model_switches,
                            budget_capped: true,
                            hit_round_cap: true,
                            injected_user_messages: Vec::new(),
                            gate_notice: notice,
                        });
                    }
                    Err(e) => {
                        warn!("Failed to get summary after max rounds: {}", e);
                        let duration_ms = start_time.elapsed().as_millis() as u64;
                        // Trace footer: max rounds exceeded (summary failed)
                        if let Some(ref tw) = self.config.trace_writer {
                            if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                                outcome: crate::trace::TraceOutcome::MaxRoundsExceeded,
                                completed_at: chrono::Utc::now(),
                                total_rounds: rounds,
                                total_input_tokens: total_usage.input_tokens,
                                total_output_tokens: total_usage.output_tokens,
                                total_duration_ms: duration_ms,
                            }) {
                                warn!(error = %e, "Failed to write trace footer");
                            }
                        }
                        return Ok(RunResult {
                            response: with_notice(
                                "Agent reached maximum number of tool execution rounds."
                                    .to_string(),
                                notice.as_deref(),
                            )
                            .0,
                            total_usage,
                            rounds,
                            tool_invocations,
                            round_details,
                            duration_ms,
                            model: actual_model,
                            model_switches,
                            budget_capped: true,
                            hit_round_cap: true,
                            injected_user_messages: Vec::new(),
                            gate_notice: notice,
                        });
                    }
                }
            }

            // Inject wind-down warning on the final round so the agent can wrap up
            if rounds == self.config.max_rounds {
                messages.push(LlmMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: wind_down_text(self.config.interactive),
                    }],
                });
            }

            self.observe_prefix(
                &mut prefix,
                PrefixFingerprint::of(&offered.prompt(&self.config.system_prompt), &offered.defs),
                rounds,
                tools_rebuilt,
            );
            let watchdog = self.run_context_watchdog(
                &mut messages,
                &offered,
                &current_model,
                rounds,
                last_observed,
            );
            let round_start = std::time::Instant::now();
            let cached_prefix = cached_prefix_for_round(rounds, prior_len);
            let response = match watchdog {
                Err(futile) => futile.degraded_response(&current_model),
                Ok(()) => match self
                    .create_round_message(&messages, &offered, &current_model, cached_prefix)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let error_str = format!("{e}");
                        if error_str.contains("empty response") {
                            warn!("LLM returned empty response — retrying once");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            self.create_round_message(
                                &messages,
                                &offered,
                                &current_model,
                                cached_prefix,
                            )
                            .await?
                        } else if classify_error(&error_str).needs_compaction() {
                            warn!("Context overflow — attempting emergency compaction");
                            let context_limit =
                                self.resolve_context_limit(&current_model, &error_str);
                            let reserved = self.reserved_context_for_with(&current_model, &offered);
                            if Self::emergency_compact_messages(
                                &mut messages,
                                context_limit,
                                reserved,
                            ) {
                                // The rewritten history recurs nowhere, so the
                                // retry caches on the last message.
                                self.create_round_message(&messages, &offered, &current_model, None)
                                    .await?
                            } else {
                                warn!("Emergency compaction failed entirely — returning degraded response");
                                degraded_overflow_response(&current_model, reserved, context_limit)
                            }
                        } else {
                            return Err(e);
                        }
                    }
                },
            };

            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;
            total_usage.cache_creation_input_tokens += response.usage.cache_creation_input_tokens;
            total_usage.cache_read_input_tokens += response.usage.cache_read_input_tokens;
            actual_model = response.model.clone();
            tel.observe_llm(&response.usage, &actual_model);
            // Phase 0: the tool-output footprint actually billed on this call — the
            // ToolResult blocks in the prompt we just sent. Cumulative across rounds
            // it pairs with `tokens_in` to size the share of paid context that is tool
            // output.
            tel.observe_tool_output_tokens(tool_result_tokens_in_prompt(&messages));

            // Mechanism B — tool-call repair (see the streaming loop for the
            // rationale): synthesize a native call from text-form tool-call
            // output so it executes through the normal per-round path without
            // burning a retry. Never after a stop request.
            // The final interactive round answers; nothing in it may execute
            // tools (budgets of 1 keep the old behavior — there was never a
            // round to research in).
            let answer_only_round = self.config.interactive
                && self.config.max_rounds >= 2
                && rounds == self.config.max_rounds;
            let response = (!stop_after_next_round && !answer_only_round)
                .then(|| self.repaired_text_tool_call(&response, rounds))
                .flatten()
                .unwrap_or(response);
            let response = if answer_only_round && response.has_tool_use() {
                match strip_final_round_tool_calls(&response) {
                    Some(answer) => {
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round emitted tool calls — discarded; taking its text as the answer"
                        );
                        answer
                    }
                    None => {
                        // No salvageable text: don't execute, don't push the
                        // dangling tool_use into history (a wrap-up call over
                        // unresponded tool_use blocks is a provider error) —
                        // let the round cap produce the answer.
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round emitted only tool calls — proceeding to the round-cap answer"
                        );
                        continue;
                    }
                }
            } else {
                response
            };

            let round_tool_count = if response.has_tool_use() {
                response.tool_uses().len() as u32
            } else {
                0
            };
            round_details.push(RoundDetail {
                round_number: rounds,
                usage: response.usage.clone(),
                latency_ms: round_start.elapsed().as_millis() as u64,
                prompt_size_bytes: response.usage.input_tokens as usize * 4,
                response_size_bytes: response.usage.output_tokens as usize * 4,
                tool_count: round_tool_count,
                has_tool_use: response.has_tool_use(),
                stop_reason: format!("{:?}", response.stop_reason),
            });
            last_observed =
                ObservedRound::capture(&response.usage, messages.len()).or(last_observed);

            if !response.has_tool_use() {
                let text = response.text().unwrap_or_default();

                // A tool call the model wrote out instead of making is by
                // definition NOT an answer — delivering it verbatim would hand
                // the reader tool-call JSON. On the answer-only round there is
                // no retry left to spend (it would loop past the cap): the
                // round-cap wrap-up answers instead.
                if !matches!(
                    classify_text_tool_call(&text, &self.tools),
                    TextCallVerdict::Prose
                ) && (answer_only_round
                    || tool_hallucination_retries < MAX_TOOL_HALLUCINATION_RETRIES)
                {
                    if answer_only_round {
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round produced a prose tool call — proceeding to the round-cap answer"
                        );
                    } else {
                        tool_hallucination_retries += 1;
                        warn!(
                            agent = %self.config.agent_id,
                            retry = tool_hallucination_retries,
                            "Detected tool call in text output — injecting correction and retrying"
                        );
                        messages.push(text_tool_call_correction());
                    }
                    continue;
                }

                let (text, gate_notice) = match self
                    .gate_answer(
                        &mut compile_gate,
                        &access_snapshot,
                        rounds,
                        stop_after_next_round,
                        None,
                        None,
                    )
                    .await
                {
                    Verdict::Fix(note) => {
                        if !text.trim().is_empty() {
                            messages.push(LlmMessage {
                                role: Role::Assistant,
                                content: vec![ContentBlock::Text { text }],
                            });
                        }
                        messages.push(LlmMessage::user(note));
                        continue;
                    }
                    Verdict::Finish(notice) => (with_notice(text, notice.as_deref()).0, notice),
                    Verdict::Cancelled => (text, None),
                };

                let duration_ms = start_time.elapsed().as_millis() as u64;
                debug!(
                    agent = %self.config.agent_id,
                    rounds = rounds,
                    total_input_tokens = total_usage.input_tokens,
                    total_output_tokens = total_usage.output_tokens,
                    cache_created = total_usage.cache_creation_input_tokens,
                    cache_read = total_usage.cache_read_input_tokens,
                    "Agent run complete"
                );
                // Trace: final round (no tools) + footer
                if let Some(ref tw) = self.config.trace_writer {
                    let input_content = messages
                        .last()
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    if let Err(e) = tw.write_round(&crate::trace::TraceRound {
                        round: rounds,
                        input: input_content,
                        response: response.content.clone(),
                        tool_results: Vec::new(),
                        model: response.model.clone(),
                        stop_reason: response.stop_reason.clone(),
                        input_tokens: response.usage.input_tokens,
                        output_tokens: response.usage.output_tokens,
                        cache_read_tokens: response.usage.cache_read_input_tokens,
                        cache_creation_tokens: response.usage.cache_creation_input_tokens,
                    }) {
                        warn!(error = %e, "Failed to write trace round");
                    }
                    if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                        outcome: crate::trace::TraceOutcome::Success,
                        completed_at: chrono::Utc::now(),
                        total_rounds: rounds,
                        total_input_tokens: total_usage.input_tokens,
                        total_output_tokens: total_usage.output_tokens,
                        total_duration_ms: duration_ms,
                    }) {
                        warn!(error = %e, "Failed to write trace footer");
                    }
                }
                return Ok(RunResult {
                    response: text,
                    total_usage,
                    rounds,
                    tool_invocations,
                    round_details,
                    duration_ms,
                    model: actual_model,
                    model_switches,
                    budget_capped: false,
                    // Natural completion — but if the wind-down landed, the agent
                    // wrapped up because it ran out of rounds, not because it finished.
                    hit_round_cap: rounds >= self.config.max_rounds,
                    injected_user_messages: Vec::new(),
                    gate_notice,
                });
            }

            // A routed approval stopped this run: the wrap-up round has been
            // consumed, so don't execute any more tools.
            if stop_after_next_round {
                let text = response.text().unwrap_or_default();
                let duration_ms = start_time.elapsed().as_millis() as u64;
                if let Some(ref tw) = self.config.trace_writer {
                    if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                        outcome: crate::trace::TraceOutcome::Cancelled,
                        completed_at: chrono::Utc::now(),
                        total_rounds: rounds,
                        total_input_tokens: total_usage.input_tokens,
                        total_output_tokens: total_usage.output_tokens,
                        total_duration_ms: duration_ms,
                    }) {
                        warn!(error = %e, "Failed to write trace footer");
                    }
                }
                return Ok(RunResult {
                    response: text,
                    total_usage,
                    rounds,
                    tool_invocations,
                    round_details,
                    duration_ms,
                    model: actual_model,
                    model_switches,
                    budget_capped: false,
                    hit_round_cap: false,
                    injected_user_messages: Vec::new(),
                    gate_notice: None,
                });
            }

            // Process tool uses
            let tool_uses = response.tool_uses();
            // (tool_use_id, tool_name, output_str, is_error) — tool_name lets
            // `tokenize_tool_outputs` tag captures with the originating tool.
            let mut tool_results: Vec<(String, String, String, bool)> = Vec::new();
            let mut round_loads: HashMap<String, Loaded> = HashMap::new();
            let mut round_calls: HashMap<String, ToolCall> = HashMap::new();

            // Add the assistant response to conversation
            messages.push(LlmMessage {
                role: crate::llm::types::Role::Assistant,
                content: response.content.clone(),
            });

            // Execute tools in parallel using JoinSet
            let mut join_set = tokio::task::JoinSet::new();
            let round_first = tool_invocations.len();
            let round_nested: Arc<parking_lot::Mutex<Vec<ToolInvocation>>> = Arc::default();

            for (tool_use_id, llm_tool_name, input) in &tool_uses {
                let (llm_tool_name, input, via_call) = match unwrapped(llm_tool_name, input) {
                    Ok(call) => call,
                    Err(message) => {
                        let outcome =
                            unwrap_failure(tool_use_id.to_string(), (*input).clone(), message);
                        join_set.spawn(async move { outcome });
                        continue;
                    }
                };
                let resolved = resolve_tool_call_logged(
                    &llm_tool_name,
                    &input,
                    &self.tools,
                    ToolCallOrigin {
                        via: "runner",
                        agent: Some(self.config.agent_id.as_str()),
                        model: Some(&current_model),
                        round: Some(rounds),
                    },
                );
                let tool_name = resolved.name;
                let input = &*resolved.input;

                offered.note_shadow_call(self.config.agent_id.as_str(), &tool_name);
                if let Some(answer) = offered.plan.answer(
                    self.config.agent_id.as_str(),
                    rounds,
                    &llm_tool_name,
                    &tool_name,
                    input,
                ) {
                    let outcome = planned_outcome(tool_use_id.to_string(), input.clone(), answer);
                    join_set.spawn(async move { outcome });
                    continue;
                }
                if crate::tools::program::is_program_call(&tool_name) {
                    if let Some(program) =
                        self.program_context(&offered, &access_snapshot, &messages)
                    {
                        let ctx = SessionContext::capture();
                        let call = self.call_for(&mut round_calls, tool_use_id, input);
                        let nested = Arc::clone(&round_nested);
                        let (tool_use_id, input) = (tool_use_id.to_string(), input.clone());
                        join_set.spawn(async move {
                            let fut = async move {
                                let run = crate::tools::program::run_program(program, &input).await;
                                nested.lock().extend(run.nested);
                                executed_outcome(
                                    tool_use_id,
                                    crate::tools::program::PROGRAM_TOOL.to_string(),
                                    input,
                                    run.execution,
                                )
                            };
                            ctx.scope(call.scope(fut)).await
                        });
                        continue;
                    }
                }
                if via_call && self.tools.get(&tool_name).is_none() {
                    let outcome = unwrap_failure(
                        tool_use_id.to_string(),
                        input.clone(),
                        unknown_call_message(&tool_name),
                    );
                    join_set.spawn(async move { outcome });
                    continue;
                }

                let detail = approval_detail(&self.tools, &tool_name, input);

                debug!(
                    agent = %self.config.agent_id,
                    tool = %tool_name,
                    detail = %detail,
                    round = rounds,
                    model = %current_model,
                    "Executing tool"
                );

                let gates = ToolGates {
                    arguments: match via_call {
                        true => crate::tools::arguments::ArgumentValidation::Enforce,
                        false => self.config.arguments,
                    },
                    ..ToolGates::from(&self.config)
                };
                let outcome = gate_tool_call(&gates, &self.policy, &self.tools, &tool_name, input);
                let tool_use_id = tool_use_id.to_string();
                let input = (*input).clone();
                let tools = Arc::clone(&self.tools);

                match outcome {
                    GateOutcome::Execute { exec_input, .. } => {
                        let tn = tool_name.clone();
                        let ctx = SessionContext::capture();
                        let access = Arc::clone(&access_snapshot);
                        let call = self.call_for(&mut round_calls, &tool_use_id, &input);
                        join_set.spawn(async move {
                            let fut = async move {
                                let exec =
                                    execute_gated(&tools, &access, &tn, &input, exec_input).await;
                                executed_outcome(tool_use_id, tn, input, exec)
                            };
                            ctx.scope(call.scope(fut)).await
                        });
                    }
                    GateOutcome::Blocked {
                        message, reason, ..
                    } => {
                        let tn = tool_name.clone();
                        join_set.spawn(async move {
                            (
                                tool_use_id,
                                tn,
                                input,
                                message,
                                serde_json::Value::Null,
                                false,
                                0u64,
                                "deny".to_string(),
                                true,
                                Some(reason),
                                Vec::new(),
                            )
                        });
                    }
                    GateOutcome::RequireApproval { reason, .. } => {
                        let tn = tool_name.clone();
                        let r = reason.clone();
                        match route_approval(
                            self.config.approval_router.as_deref(),
                            self.config.agent_id.as_str(),
                            &tn,
                            &r,
                            &input,
                        ) {
                            RouteResolution::Routed {
                                request_id,
                                approver_user_ids,
                                ..
                            } => {
                                // Routed to a designated approver: arm the stop
                                // so the next round is the wrap-up, and carry the
                                // delivery coordinates out in the invocation
                                // record — the pending row does not persist
                                // approvers, and no progress consumer exists on
                                // this path to DM them.
                                stop_after_next_round = true;
                                let payload = serde_json::json!({
                                    "approval_request_id": request_id,
                                    "approver_user_ids": approver_user_ids,
                                    "detail": detail,
                                });
                                join_set.spawn(async move {
                                    (
                                        tool_use_id,
                                        tn,
                                        input,
                                        ROUTED_APPROVAL_TOOL_RESULT.to_string(),
                                        payload,
                                        false,
                                        0u64,
                                        "require_approval".to_string(),
                                        false,
                                        None,
                                        Vec::new(),
                                    )
                                });
                            }
                            RouteResolution::DenyUnreachable => {
                                join_set.spawn(async move {
                                    let msg = UNREACHABLE_APPROVER.to_string();
                                    (
                                        tool_use_id,
                                        tn,
                                        input,
                                        msg.clone(),
                                        serde_json::Value::Null,
                                        false,
                                        0u64,
                                        "deny".to_string(),
                                        true,
                                        Some(msg),
                                        Vec::new(),
                                    )
                                });
                            }
                            // No routing rule (or no requester/session/persist):
                            // no human is watching a non-progress turn, so the
                            // instant error stands.
                            RouteResolution::SelfServe => {
                                join_set.spawn(async move {
                                    (
                                        tool_use_id,
                                        tn,
                                        input,
                                        format!("Approval required: {}", r),
                                        serde_json::Value::Null,
                                        false,
                                        0u64,
                                        "require_approval".to_string(),
                                        true,
                                        Some(r.clone()),
                                        Vec::new(),
                                    )
                                });
                            }
                        }
                    }
                }
            }

            while let Some(result) = join_set.join_next().await {
                let (
                    tool_use_id,
                    tool_name,
                    input,
                    output_str,
                    output,
                    success,
                    duration_ms,
                    policy_decision,
                    is_error,
                    denial_reason,
                    loads,
                ) = result.map_err(|e| eyre::eyre!("Tool task panicked: {}", e))?;
                if let Some(gate) = compile_gate.as_mut() {
                    gate.observe(&tool_name, &input, &output, success);
                }
                let (output_str, output) = crate::spill::spill_tool_output(
                    &tool_name,
                    output_str,
                    output,
                    is_error,
                    &self.config.spill,
                    crate::spill::RetrievalSurface::from_registry(&self.tools),
                );
                let error = (!success).then(|| output_str.clone());
                let output_str = match loads.is_empty() {
                    true => output_str,
                    false => {
                        offered.queue_loads(&loads);
                        let (content, loaded) = loaded_result(&self.tools, output_str, &loads);
                        round_loads.insert(tool_use_id.clone(), loaded);
                        content
                    }
                };
                let tool_name_for_capture = tool_name.clone();
                let invocation = ToolInvocation {
                    tool_name,
                    input,
                    output,
                    success,
                    policy_decision,
                    duration_ms,
                    denial_reason,
                    error,
                };
                round_calls
                    .remove(&tool_use_id)
                    .unwrap_or_else(|| {
                        ToolCall::new(self.config.journal.clone(), invocation.input.clone())
                    })
                    .finished(&invocation)
                    .await;
                tool_invocations.push(invocation);
                tool_results.push((tool_use_id, tool_name_for_capture, output_str, is_error));
            }
            if let Some(nudge) = program_nudge.as_mut() {
                nudge.observe(&mut tool_results);
            }

            // What this round's programs called, recorded like direct calls.
            tool_invocations.extend(std::mem::take(&mut *round_nested.lock()));

            // Deliver any images this round's tools staged (both run loops).
            self.deliver_round_images(&tool_invocations[round_first..])
                .await;
            tel.observe_tools(&tool_invocations[round_first..]);

            // ── Trace capture: write round with tool results ──
            if let Some(ref tw) = self.config.trace_writer {
                let input_content = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let trace_tool_results: Vec<crate::trace::ToolResultTrace> = tool_invocations
                    [round_first..]
                    .iter()
                    .map(redacted_tool_result_trace)
                    .collect();
                if let Err(e) = tw.write_round(&crate::trace::TraceRound {
                    round: rounds,
                    input: input_content,
                    response: response.content.clone(),
                    tool_results: trace_tool_results,
                    model: response.model.clone(),
                    stop_reason: response.stop_reason.clone(),
                    input_tokens: response.usage.input_tokens,
                    output_tokens: response.usage.output_tokens,
                    cache_read_tokens: response.usage.cache_read_input_tokens,
                    cache_creation_tokens: response.usage.cache_creation_input_tokens,
                }) {
                    warn!(error = %e, "Failed to write trace round");
                }
            }

            messages.push(with_loaded(
                LlmMessage::tool_results(tokenize_tool_outputs(tool_results)),
                &round_loads,
            ));

            // Adaptive model routing (DW-252): decide if we should switch models
            if self.config.routing.enable_adaptive_routing {
                // Collect round metrics for routing decision
                let round_tools: Vec<String> = tool_invocations[round_first..]
                    .iter()
                    .map(|inv| inv.tool_name.clone())
                    .collect();

                let write_tools = [
                    "code_edit",
                    "filesystem_write",
                    "filesystem_edit",
                    "github_pr_create",
                    "github_issue_create",
                    "linear_issue_create",
                    "messaging_send",
                ];
                let has_write_tools = round_tools
                    .iter()
                    .any(|t| write_tools.iter().any(|wt| t.contains(wt)));

                let round_duration_ms = round_details.last().map(|rd| rd.latency_ms).unwrap_or(0);

                // Check if there were any errors in this round (for escalation detection)
                let has_error_recovery = tool_invocations[round_first..]
                    .iter()
                    .any(|inv| !inv.success);

                let metrics = crate::model_router::RoundMetrics {
                    round_number: rounds,
                    duration_ms: round_duration_ms,
                    tools_used: round_tools,
                    has_write_tools,
                    has_error_recovery,
                };

                model_router.record_round(metrics);
                let decision = model_router.decide();

                if let Some(new_model) = decision.target_model {
                    let from_model = current_model.clone();

                    info!(
                        agent = %self.config.agent_id,
                        from_model = %from_model,
                        to_model = %new_model,
                        round = rounds,
                        reason = %decision.reason,
                        confidence = decision.confidence,
                        "Adaptive routing: switching models"
                    );

                    current_model = new_model.clone();
                    model_router.apply_switch(current_model.clone());
                    model_switches.push(ModelSwitchRecord {
                        round: rounds,
                        from_model,
                        to_model: new_model,
                        reason: decision.reason.clone(),
                    });
                }
            }

            // Render any images staged by this round's tools into the
            // conversation, using the (possibly just-switched) model that will
            // consume the appended blocks.
            self.append_viewed_images(
                &mut messages,
                &tool_invocations[round_first..],
                &current_model,
            )
            .await;
        }
    }

    /// Run the agent on a task, sending progress events to the given channel.
    ///
    /// This is the same as `run()` but emits `ProgressEvent`s so callers
    /// (e.g. the gateway) can stream status updates to connected clients.
    pub async fn run_with_progress(
        &self,
        task: &str,
        progress_tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    ) -> Result<RunResult> {
        let task_content = vec![ContentBlock::Text {
            text: task.to_string(),
        }];
        self.run_with_progress_and_history(task_content, Vec::new(), progress_tx, None, None)
            .await
    }

    /// The trace header of a run starting now on `model`: what whoever
    /// started the run said of its origin, and the session it runs in.
    fn trace_header(
        &self,
        trace_id: &str,
        model: &str,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> crate::trace::TraceHeader {
        self.config.trace_tags.header(crate::trace::TraceRun {
            trace_id: trace_id.to_string(),
            agent_id: self.config.agent_id.to_string(),
            session_id: SessionContext::capture().id.unwrap_or_default(),
            model: model.to_string(),
            system_prompt: self.config.system_prompt.clone(),
            tools: self.tools.definitions(),
            started_at,
        })
    }

    /// Run the agent with conversation history and progress events.
    ///
    /// `prior_messages` contains the conversation history (user/assistant pairs)
    /// from previous turns. `task_content` is appended as the latest user message
    /// and may contain text, images, or both.
    ///
    /// This variant uses LLM streaming to emit [`ProgressEvent::StreamDelta`] events
    /// in real time as the model generates text, bracketed by `StreamStart` / `StreamEnd`.
    pub async fn run_with_progress_and_history(
        &self,
        task_content: Vec<ContentBlock>,
        prior_messages: Vec<LlmMessage>,
        progress_tx: tokio::sync::mpsc::Sender<ProgressEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        inject_rx: Option<tokio::sync::mpsc::Receiver<InjectedMessage>>,
    ) -> Result<RunResult> {
        let _turn = TurnGuard::begin();
        let mut tel = crate::turn_event::TurnTelemetry::start(&self.config);
        tel.set_task(&task_content);
        let result = Box::pin(self.run_with_progress_and_history_inner(
            task_content,
            prior_messages,
            progress_tx,
            cancel_token,
            inject_rx,
            &mut tel,
        ))
        .await;
        tel.emit(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_with_progress_and_history_inner(
        &self,
        task_content: Vec<ContentBlock>,
        prior_messages: Vec<LlmMessage>,
        progress_tx: tokio::sync::mpsc::Sender<ProgressEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        inject_rx: Option<tokio::sync::mpsc::Receiver<InjectedMessage>>,
        tel: &mut crate::turn_event::TurnTelemetry,
    ) -> Result<RunResult> {
        let prior_len = prior_messages.len();
        let mut messages = prior_messages;
        messages.push(LlmMessage {
            role: Role::User,
            content: task_content,
        });
        let mut total_usage = TokenUsage::default();
        let mut tool_invocations: Vec<ToolInvocation> = Vec::new();
        self.seed_calls(
            &mut messages,
            &mut tool_invocations,
            Some(&progress_tx),
            cancel_token.as_ref(),
        )
        .await;
        let mut compile_gate = self.gate_turn(&tool_invocations);
        let mut rounds = 0;
        let model_switches: Vec<ModelSwitchRecord> = Vec::new();
        let start_time = std::time::Instant::now();
        let mut round_details: Vec<RoundDetail> = Vec::new();
        let run_model = self.config.model.clone().unwrap_or_default();
        let mut actual_model = run_model.clone();
        let mut halfway_warned = false;
        let mut tool_hallucination_retries: u32 = 0;
        let mut last_observed: Option<ObservedRound> = None;
        // Last time we said something durable to the human. Drives the periodic
        // check-in so a long, quiet turn never leaves the user guessing.
        // Shared with `execute_with_deadline` so a heartbeat posted from inside
        // a long tool call counts as the human having heard from us, and the
        // loop does not immediately post a second card when the tool returns.
        let last_user_comm = Arc::new(Mutex::new(tokio::time::Instant::now()));
        // Owned by the loop alone — see the nudge below for why it is not the
        // silence clock.
        let mut last_nudge = tokio::time::Instant::now();

        info!(
            agent = %self.config.agent_id,
            tools = self.tools.len(),
            model = %run_model,
            "Starting agent run (with streaming progress)"
        );

        // ── Trace capture: write header (streaming path) ──
        let trace_start = chrono::Utc::now();
        if let Some(ref tw) = self.config.trace_writer {
            let header = self.trace_header(tel.trace_id(), &run_model, trace_start);
            if let Err(e) = tw.write_header(&header) {
                warn!(error = %e, "Failed to write trace header");
            }
        }

        let mut stop_after_next_round = false;

        // Split inject_rx into the turn's approval ledger, which settles
        // self-serve prompts, and the regular injected messages (user
        // follow-ups, steer-stop, etc.). A turn without an inject channel, or
        // whose surface renders no prompt, has no ledger: nobody could answer
        // its prompts. The ledger ends with the
        // turn, so a call a detached task still holds never runs on a late
        // answer.
        let (approvals, mut fwd_inject_rx) =
            TurnApprovals::split(inject_rx, self.config.self_serve);
        let _ledger_end = approvals.clone().map(TurnApprovals::end_on_drop);

        let mut absorbed_followups: Vec<InjectedUserMessage> = Vec::new();
        let session = session_tools();
        let mut offered = self.offered_tools();
        let mut program_nudge =
            crate::tools::program::ProgramNudge::new(self.config.programs.as_ref());
        let mut prefix: Option<PrefixFingerprint> = None;

        loop {
            rounds += 1;
            tel.set_rounds(rounds);

            // ── Mid-run message injection ──
            // Drain follow-ups at the top of every round so a message injected
            // before the run started (a coalesced Slack follow-up) is already
            // in round 1's prompt. (ApprovalResponse messages are handled by
            // the router task above; only regular messages arrive here.)
            self.absorb_injected_followups(
                &mut fwd_inject_rx,
                &mut messages,
                &mut absorbed_followups,
                &mut stop_after_next_round,
                &progress_tx,
                rounds,
            )
            .await;

            let context_floor = self.llm.context_floor(&run_model).await;
            let tools_rebuilt = offered.refresh(
                &self.tools,
                self.tool_access.as_ref(),
                &self.plan_context(&session, context_floor),
            );
            if rounds == 1 {
                offered.log_plan(self.config.agent_id.as_str());
            }
            let access_snapshot = Arc::clone(&offered.access);

            // Proportional halfway warning (DW-419)
            let halfway = self.config.max_rounds / 2;
            if !halfway_warned
                && halfway > 0
                && rounds >= halfway
                && rounds < self.config.max_rounds
            {
                halfway_warned = true;
                info!(
                    agent = %self.config.agent_id,
                    round = rounds,
                    max_rounds = self.config.max_rounds,
                    "Halfway round budget warning",
                );
                messages.push(LlmMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[SYSTEM: ⚠️ You've used {} of {} rounds. \
                             Shift from exploration to execution — start producing output now.]",
                            rounds, self.config.max_rounds
                        ),
                    }],
                });
            }

            if rounds > self.config.max_rounds {
                // Log tool usage breakdown for diagnostics
                let mut tool_counts: HashMap<&str, usize> = HashMap::new();
                for inv in &tool_invocations {
                    *tool_counts.entry(&inv.tool_name).or_insert(0) += 1;
                }
                warn!(
                    agent = %self.config.agent_id,
                    rounds = rounds,
                    tool_summary = ?tool_counts,
                    "Max rounds exceeded — tool usage breakdown"
                );
                let notice = match self
                    .gate_answer(
                        &mut compile_gate,
                        &access_snapshot,
                        rounds,
                        false,
                        Some(&progress_tx),
                        cancel_token.as_ref(),
                    )
                    .await
                {
                    Verdict::Finish(notice) => notice,
                    Verdict::Fix(_) => None,
                    Verdict::Cancelled => {
                        tel.mark_cancelled();
                        break;
                    }
                };

                // Ask LLM for the wrap-up without tools so the user gets useful
                // output — for an interactive turn, that wrap-up IS the answer.
                let summary_prompt = round_cap_wrapup_prompt(
                    &self.config.system_prompt,
                    self.config.max_rounds,
                    self.config.interactive,
                );
                let summary_text = match self
                    .llm
                    .create_message(
                        Some(&summary_prompt),
                        &messages,
                        &[],
                        self.config.model.as_deref(),
                        self.config
                            .model
                            .as_deref()
                            .map(|m| self.config.effective_max_tokens(m))
                            .unwrap_or(self.config.max_tokens),
                        true, // enable cache (this is a long non-agent turn)
                    )
                    .await
                {
                    Ok(resp) => {
                        let text = resp.text().unwrap_or_default();
                        total_usage.input_tokens += resp.usage.input_tokens;
                        total_usage.output_tokens += resp.usage.output_tokens;
                        if text.is_empty() {
                            "Agent reached maximum number of tool execution rounds.".to_string()
                        } else {
                            text
                        }
                    }
                    Err(e) => {
                        warn!("Failed to get summary after max rounds: {}", e);
                        "Agent reached maximum number of tool execution rounds.".to_string()
                    }
                };
                let summary_text = with_notice(summary_text, notice.as_deref()).0;

                let _ = progress_tx
                    .send(ProgressEvent::Response {
                        text: summary_text.clone(),
                    })
                    .await;
                let _ = progress_tx
                    .send(ProgressEvent::Done {
                        rounds,
                        input_tokens: total_usage.input_tokens,
                        output_tokens: total_usage.output_tokens,
                        cache_read_tokens: total_usage.cache_read_input_tokens,
                        tool_count: tool_invocations.len(),
                    })
                    .await;

                let duration_ms_streaming = start_time.elapsed().as_millis() as u64;
                // Trace footer: max rounds exceeded (streaming path)
                if let Some(ref tw) = self.config.trace_writer {
                    if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                        outcome: crate::trace::TraceOutcome::MaxRoundsExceeded,
                        completed_at: chrono::Utc::now(),
                        total_rounds: rounds,
                        total_input_tokens: total_usage.input_tokens,
                        total_output_tokens: total_usage.output_tokens,
                        total_duration_ms: duration_ms_streaming,
                    }) {
                        warn!(error = %e, "Failed to write trace footer");
                    }
                }

                return Ok(RunResult {
                    response: summary_text,
                    total_usage,
                    rounds,
                    tool_invocations,
                    round_details,
                    duration_ms: duration_ms_streaming,
                    model: actual_model,
                    model_switches,
                    budget_capped: true,
                    hit_round_cap: true,
                    injected_user_messages: std::mem::take(&mut absorbed_followups),
                    gate_notice: notice,
                });
            }

            // Inject wind-down warning on the final round so the agent can wrap up
            if rounds == self.config.max_rounds {
                messages.push(LlmMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: wind_down_text(self.config.interactive),
                    }],
                });
            }

            if let Some(ref token) = cancel_token {
                if token.is_cancelled() {
                    tel.mark_cancelled();
                    info!(agent = %self.config.agent_id, rounds, "Cancelled by user");
                    break;
                }
            }

            let watchdog = self.run_context_watchdog(
                &mut messages,
                &offered,
                &run_model,
                rounds,
                last_observed,
            );

            // ── Periodic check-in (DW: long-running research) ──
            // Guarantee the human hears something at least every
            // CHECK_IN_INTERVAL. This fires at a round boundary and carries the
            // round tally; a single tool call that outlasts the interval posts
            // its own, thinner heartbeat from `execute_with_deadline` (which is
            // what puts a Pause button on screen mid-call). Both share the
            // silence clock, so they cannot double-post.
            if lock_instant(&last_user_comm).elapsed() >= CHECK_IN_INTERVAL {
                let note = build_checkin_note(rounds, start_time.elapsed(), &tool_invocations);
                let _ = progress_tx.send(ProgressEvent::CheckIn { note }).await;
                *lock_instant(&last_user_comm) = tokio::time::Instant::now();
            }

            // The convergence nudge is about the agent, not the human, so it
            // keeps its own clock: a mid-call heartbeat resets the silence
            // budget, and hanging the nudge off that would let a turn of
            // just-over-interval tool calls defer it for ever.
            if last_nudge.elapsed() >= CHECK_IN_INTERVAL {
                // A long quiet stretch usually means it's deep in exploration,
                // so bias it toward delivering with what it has. Re-fires each
                // interval, so the pressure builds the longer a turn runs.
                messages.push(LlmMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[SYSTEM: This turn has run {}s across {} rounds. Prioritise wrapping \
                             up — give your best answer with what you already have rather than \
                             starting new investigation, unless something essential is missing.]",
                            start_time.elapsed().as_secs(),
                            rounds
                        ),
                    }],
                });
                last_nudge = tokio::time::Instant::now();
            }

            self.observe_prefix(
                &mut prefix,
                PrefixFingerprint::of(&offered.prompt(&self.config.system_prompt), &offered.defs),
                rounds,
                tools_rebuilt,
            );

            // Use streaming LLM call so we can forward text deltas in real time
            // (with emergency compaction recovery on context overflow). The
            // degraded response is delivered by the canonical final-response
            // emit below, like the non-streaming loop.
            let round_start = std::time::Instant::now();
            let cached_prefix = cached_prefix_for_round(rounds, prior_len);
            let (response, gate_aborted) = match watchdog {
                Err(futile) => {
                    let degraded = futile.degraded_response(&run_model);
                    if let Some(text) = degraded.text() {
                        stream_text_segment(&progress_tx, &text).await;
                    }
                    (degraded, false)
                }
                Ok(()) => {
                    let _ = progress_tx
                        .send(ProgressEvent::LlmCall { round: rounds })
                        .await;
                    match self
                        .call_llm_streaming(&messages, &offered, &progress_tx, tel, cached_prefix)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let error_str = format!("{e}");
                            if error_str.contains("empty response") {
                                warn!("LLM returned empty response — retrying once");
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                self.call_llm_streaming(
                                    &messages,
                                    &offered,
                                    &progress_tx,
                                    tel,
                                    cached_prefix,
                                )
                                .await?
                            } else if classify_error(&error_str).needs_compaction() {
                                warn!("Context overflow — attempting emergency compaction");
                                let _ = progress_tx.send(ProgressEvent::CompactStart).await;
                                let context_limit =
                                    self.resolve_context_limit(&run_model, &error_str);
                                let reserved = self.reserved_context_for_with(&run_model, &offered);
                                if Self::emergency_compact_messages(
                                    &mut messages,
                                    context_limit,
                                    reserved,
                                ) {
                                    let _ = progress_tx.send(ProgressEvent::CompactEnd).await;
                                    // The rewritten history recurs nowhere, so
                                    // the retry caches on the last message.
                                    self.call_llm_streaming(
                                        &messages,
                                        &offered,
                                        &progress_tx,
                                        tel,
                                        None,
                                    )
                                    .await?
                                } else {
                                    let _ = progress_tx.send(ProgressEvent::CompactEnd).await;
                                    warn!("Emergency compaction failed entirely — returning degraded response");
                                    let degraded = degraded_overflow_response(
                                        &run_model,
                                        reserved,
                                        context_limit,
                                    );
                                    if let Some(text) = degraded.text() {
                                        stream_text_segment(&progress_tx, &text).await;
                                    }
                                    (degraded, false)
                                }
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
            };

            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;
            total_usage.cache_creation_input_tokens += response.usage.cache_creation_input_tokens;
            total_usage.cache_read_input_tokens += response.usage.cache_read_input_tokens;
            actual_model = response.model.clone();
            tel.observe_llm(&response.usage, &actual_model);
            // Phase 0: the tool-output footprint actually billed on this call — the
            // ToolResult blocks in the prompt we just sent. Cumulative across rounds
            // it pairs with `tokens_in` to size the share of paid context that is tool
            // output.
            tel.observe_tool_output_tokens(tool_result_tokens_in_prompt(&messages));

            // Mechanism B — tool-call repair: a weaker model sometimes emits
            // its tool call as text. When the round's text parses as a call
            // to a registered tool, synthesize the call so it executes
            // through the normal per-round path (policy, gates, sentinel,
            // approval) without burning a retry. Never after a stop request —
            // no new work once the user has asked to wrap up.
            // See the non-streaming twin: the final interactive round answers.
            let answer_only_round = self.config.interactive
                && self.config.max_rounds >= 2
                && rounds == self.config.max_rounds;
            let response = match (!stop_after_next_round && !answer_only_round)
                .then(|| self.repaired_text_tool_call(&response, rounds))
                .flatten()
            {
                Some(repaired) => {
                    if !gate_aborted {
                        // The bogus JSON already streamed; wipe it from the
                        // preview (an aborted stream discarded at the abort
                        // site).
                        let _ = progress_tx
                            .send(ProgressEvent::StreamDiscard {
                                reason: StreamDiscardReason::Retry,
                            })
                            .await;
                    }
                    repaired
                }
                None => response,
            };
            let response = if answer_only_round && response.has_tool_use() {
                match strip_final_round_tool_calls(&response) {
                    Some(answer) => {
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round emitted tool calls — discarded; taking its text as the answer"
                        );
                        answer
                    }
                    None => {
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round emitted only tool calls — proceeding to the round-cap answer"
                        );
                        // A tool-only round streamed nothing, so there is
                        // nothing to discard — a Retry here would consume the
                        // preview's fallback for an older, real round.
                        continue;
                    }
                }
            } else {
                response
            };

            let round_tool_count = if response.has_tool_use() {
                response.tool_uses().len() as u32
            } else {
                0
            };
            round_details.push(RoundDetail {
                round_number: rounds,
                usage: response.usage.clone(),
                latency_ms: round_start.elapsed().as_millis() as u64,
                prompt_size_bytes: response.usage.input_tokens as usize * 4,
                response_size_bytes: response.usage.output_tokens as usize * 4,
                tool_count: round_tool_count,
                has_tool_use: response.has_tool_use(),
                stop_reason: format!("{:?}", response.stop_reason),
            });
            last_observed =
                ObservedRound::capture(&response.usage, messages.len()).or(last_observed);

            if let Some(ref token) = cancel_token {
                if token.is_cancelled() {
                    tel.mark_cancelled();
                    info!(agent = %self.config.agent_id, rounds, "Cancelled after LLM call");
                    break;
                }
            }

            if !response.has_tool_use() {
                let text = response.text().unwrap_or_default();

                // See the non-streaming twin. A stream the gate cut off is
                // never an answer whatever the text says — it judged the whole
                // round, while `text` is only its first block. The round's
                // text already streamed, so tell consumers to rewind it rather
                // than keep it as answer text — unless the gate already
                // discarded that preview at the abort site.
                if (gate_aborted
                    || !matches!(
                        classify_text_tool_call(&text, &self.tools),
                        TextCallVerdict::Prose
                    ))
                    && (answer_only_round
                        || tool_hallucination_retries < MAX_TOOL_HALLUCINATION_RETRIES)
                {
                    if !gate_aborted {
                        let _ = progress_tx
                            .send(ProgressEvent::StreamDiscard {
                                reason: StreamDiscardReason::Retry,
                            })
                            .await;
                    }
                    if answer_only_round {
                        warn!(
                            agent = %self.config.agent_id,
                            rounds,
                            "final interactive round produced a prose tool call — proceeding to the round-cap answer"
                        );
                    } else {
                        tool_hallucination_retries += 1;
                        warn!(
                            agent = %self.config.agent_id,
                            retry = tool_hallucination_retries,
                            "Detected tool call in text output — injecting correction and retrying"
                        );
                        messages.push(text_tool_call_correction());
                    }
                    continue;
                }

                // A follow-up may have landed during the final LLM call —
                // absorb it with one more round instead of dropping it on
                // return. The draft answer goes into the conversation first so
                // the follow-up reads as a reply to it; it is superseded by the
                // next round's response.
                messages.push(LlmMessage {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                });
                if self
                    .absorb_injected_followups(
                        &mut fwd_inject_rx,
                        &mut messages,
                        &mut absorbed_followups,
                        &mut stop_after_next_round,
                        &progress_tx,
                        rounds,
                    )
                    .await
                    > 0
                {
                    // The draft answer streamed but is superseded by the next
                    // round (which addresses the absorbed follow-up too), so it
                    // is dropped from the result — discard it downstream.
                    let _ = progress_tx
                        .send(ProgressEvent::StreamDiscard {
                            reason: StreamDiscardReason::Superseded,
                        })
                        .await;
                    continue;
                }

                // The draft is already in the conversation, so a fix round
                // only adds the errors. It also streamed: rewind it, since it
                // presents as done code that does not build.
                let (text, gate_notice) = match self
                    .gate_answer(
                        &mut compile_gate,
                        &access_snapshot,
                        rounds,
                        stop_after_next_round,
                        Some(&progress_tx),
                        cancel_token.as_ref(),
                    )
                    .await
                {
                    Verdict::Fix(note) => {
                        let _ = progress_tx
                            .send(ProgressEvent::StreamDiscard {
                                reason: StreamDiscardReason::Retry,
                            })
                            .await;
                        if text.trim().is_empty() {
                            messages.pop();
                        }
                        messages.push(LlmMessage::user(note));
                        continue;
                    }
                    Verdict::Cancelled => {
                        tel.mark_cancelled();
                        info!(agent = %self.config.agent_id, rounds, "Cancelled during the build check");
                        break;
                    }
                    Verdict::Finish(notice) => {
                        let (answer, added) = with_notice(text, notice.as_deref());
                        if let Some(added) = added {
                            stream_text_segment(&progress_tx, &added).await;
                        }
                        (answer, notice)
                    }
                };

                let duration_ms = start_time.elapsed().as_millis() as u64;

                // Trace: final round (no tools) + footer (streaming path)
                if let Some(ref tw) = self.config.trace_writer {
                    let input_content = messages
                        .last()
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    if let Err(e) = tw.write_round(&crate::trace::TraceRound {
                        round: rounds,
                        input: input_content,
                        response: response.content.clone(),
                        tool_results: Vec::new(),
                        model: response.model.clone(),
                        stop_reason: response.stop_reason.clone(),
                        input_tokens: response.usage.input_tokens,
                        output_tokens: response.usage.output_tokens,
                        cache_read_tokens: response.usage.cache_read_input_tokens,
                        cache_creation_tokens: response.usage.cache_creation_input_tokens,
                    }) {
                        warn!(error = %e, "Failed to write trace round");
                    }
                    if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                        outcome: crate::trace::TraceOutcome::Success,
                        completed_at: chrono::Utc::now(),
                        total_rounds: rounds,
                        total_input_tokens: total_usage.input_tokens,
                        total_output_tokens: total_usage.output_tokens,
                        total_duration_ms: duration_ms,
                    }) {
                        warn!(error = %e, "Failed to write trace footer");
                    }
                }

                // Also send the full Response event (for non-streaming consumers
                // and session recording)
                let _ = progress_tx
                    .send(ProgressEvent::Response { text: text.clone() })
                    .await;
                let _ = progress_tx
                    .send(ProgressEvent::Done {
                        rounds,
                        input_tokens: total_usage.input_tokens,
                        output_tokens: total_usage.output_tokens,
                        cache_read_tokens: total_usage.cache_read_input_tokens,
                        tool_count: tool_invocations.len(),
                    })
                    .await;

                return Ok(RunResult {
                    response: text,
                    total_usage,
                    rounds,
                    tool_invocations,
                    round_details,
                    duration_ms,
                    model: actual_model,
                    model_switches,
                    budget_capped: false,
                    // Natural completion — but if the wind-down landed, the agent
                    // wrapped up because it ran out of rounds, not because it finished.
                    hit_round_cap: rounds >= self.config.max_rounds,
                    injected_user_messages: std::mem::take(&mut absorbed_followups),
                    gate_notice,
                });
            }

            // If the user requested a stop, don't execute any more tools
            if stop_after_next_round {
                let text = response.text().unwrap_or_default();
                let duration_ms = start_time.elapsed().as_millis() as u64;
                // Trace footer: user-requested stop
                if let Some(ref tw) = self.config.trace_writer {
                    if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                        outcome: crate::trace::TraceOutcome::Cancelled,
                        completed_at: chrono::Utc::now(),
                        total_rounds: rounds,
                        total_input_tokens: total_usage.input_tokens,
                        total_output_tokens: total_usage.output_tokens,
                        total_duration_ms: duration_ms,
                    }) {
                        warn!(error = %e, "Failed to write trace footer");
                    }
                }
                let _ = progress_tx
                    .send(ProgressEvent::Response { text: text.clone() })
                    .await;
                let _ = progress_tx
                    .send(ProgressEvent::Done {
                        rounds,
                        input_tokens: total_usage.input_tokens,
                        output_tokens: total_usage.output_tokens,
                        cache_read_tokens: total_usage.cache_read_input_tokens,
                        tool_count: tool_invocations.len(),
                    })
                    .await;
                return Ok(RunResult {
                    response: text,
                    total_usage,
                    rounds,
                    tool_invocations,
                    round_details,
                    duration_ms,
                    model: actual_model,
                    model_switches,
                    budget_capped: false,
                    hit_round_cap: false,
                    injected_user_messages: std::mem::take(&mut absorbed_followups),
                    gate_notice: None,
                });
            }

            let tool_uses = response.tool_uses();
            let mut tool_results: Vec<(String, String, String, bool)> = Vec::new();
            let mut round_loads: HashMap<String, Loaded> = HashMap::new();
            let mut round_calls: HashMap<String, ToolCall> = HashMap::new();

            messages.push(LlmMessage {
                role: crate::llm::types::Role::Assistant,
                content: response.content.clone(),
            });

            // Execute tools in parallel using JoinSet
            let mut join_set = tokio::task::JoinSet::new();
            let round_first = tool_invocations.len();
            let round_nested: Arc<parking_lot::Mutex<Vec<ToolInvocation>>> = Arc::default();

            for (tool_use_id, llm_tool_name, input) in &tool_uses {
                let (llm_tool_name, input, via_call) = match unwrapped(llm_tool_name, input) {
                    Ok(call) => call,
                    Err(message) => {
                        let outcome =
                            unwrap_failure(tool_use_id.to_string(), (*input).clone(), message);
                        join_set.spawn(async move { outcome });
                        continue;
                    }
                };
                let resolved = resolve_tool_call_logged(
                    &llm_tool_name,
                    &input,
                    &self.tools,
                    ToolCallOrigin {
                        via: "runner",
                        agent: Some(self.config.agent_id.as_str()),
                        model: Some(&run_model),
                        round: Some(rounds),
                    },
                );
                let tool_name = resolved.name;
                let input = &*resolved.input;

                offered.note_shadow_call(self.config.agent_id.as_str(), &tool_name);
                if let Some(answer) = offered.plan.answer(
                    self.config.agent_id.as_str(),
                    rounds,
                    &llm_tool_name,
                    &tool_name,
                    input,
                ) {
                    let outcome = planned_outcome(tool_use_id.to_string(), input.clone(), answer);
                    join_set.spawn(async move { outcome });
                    continue;
                }
                if crate::tools::program::is_program_call(&tool_name) {
                    if let Some(program) =
                        self.program_context(&offered, &access_snapshot, &messages)
                    {
                        let _ = progress_tx
                            .send(ProgressEvent::ToolStart {
                                name: crate::tools::program::PROGRAM_TOOL.to_string(),
                                round: rounds,
                                detail: String::new(),
                            })
                            .await;
                        let ctx = SessionContext::capture().with_user_comm(&last_user_comm);
                        let call = self.call_for(&mut round_calls, tool_use_id, input);
                        let nested = Arc::clone(&round_nested);
                        let ptx = progress_tx.clone();
                        let (tool_use_id, input) = (tool_use_id.to_string(), input.clone());
                        let ledger = approvals.clone();
                        join_set.spawn(async move {
                            let run = scope_turn(
                                Some(ptx.clone()),
                                ledger,
                                ctx.scope(
                                    call.scope(crate::tools::program::run_program(program, &input)),
                                ),
                            )
                            .await;
                            nested.lock().extend(run.nested);
                            report(
                                &ptx,
                                rounds,
                                tool_use_id,
                                crate::tools::program::PROGRAM_TOOL.to_string(),
                                input,
                                run.execution,
                            )
                            .await
                        });
                        continue;
                    }
                }
                if via_call && self.tools.get(&tool_name).is_none() {
                    let outcome = unwrap_failure(
                        tool_use_id.to_string(),
                        input.clone(),
                        unknown_call_message(&tool_name),
                    );
                    join_set.spawn(async move { outcome });
                    continue;
                }
                let detail = approval_detail(&self.tools, &tool_name, input);

                debug!(
                    agent = %self.config.agent_id,
                    tool = %tool_name,
                    detail = %detail,
                    round = rounds,
                    model = %run_model,
                    "Executing tool"
                );

                let _ = progress_tx
                    .send(ProgressEvent::ToolStart {
                        name: tool_name.clone(),
                        round: rounds,
                        detail: detail.clone(),
                    })
                    .await;

                let gates = ToolGates {
                    arguments: match via_call {
                        true => crate::tools::arguments::ArgumentValidation::Enforce,
                        false => self.config.arguments,
                    },
                    ..ToolGates::from(&self.config)
                };
                let outcome = gate_tool_call(&gates, &self.policy, &self.tools, &tool_name, input);
                let tool_use_id = tool_use_id.to_string();
                let input = (*input).clone();
                let ptx = progress_tx.clone();
                let tools = Arc::clone(&self.tools);

                let consumed = match &outcome {
                    GateOutcome::Execute { consumed, .. }
                    | GateOutcome::Blocked { consumed, .. } => consumed.clone(),
                    GateOutcome::RequireApproval { .. } => Vec::new(),
                };
                if !consumed.is_empty() {
                    let _ = ptx
                        .send(ProgressEvent::SecretConsumed {
                            tool: tool_name.clone(),
                            consumed,
                        })
                        .await;
                }

                match outcome {
                    GateOutcome::Execute { exec_input, .. } => {
                        let ctx = SessionContext::capture().with_user_comm(&last_user_comm);
                        let access = Arc::clone(&access_snapshot);
                        let call = self.call_for(&mut round_calls, &tool_use_id, &input);
                        let ledger = approvals.clone();
                        join_set.spawn(async move {
                            // Re-apply the turn's progress sender and approval
                            // ledger as task-locals inside the spawned tool
                            // task so a long-running tool can stream
                            // SubActivity to the status line and put its own
                            // prompts on the turn's card. The silence clock
                            // rides along so a long call can post its own
                            // check-in — and thus a Pause button.
                            let exec = scope_turn(
                                Some(ptx.clone()),
                                ledger,
                                call.scope(ctx.scope(execute_gated(
                                    &tools, &access, &tool_name, &input, exec_input,
                                ))),
                            )
                            .await;
                            report(&ptx, rounds, tool_use_id, tool_name, input, exec).await
                        });
                    }
                    GateOutcome::Blocked {
                        message, reason, ..
                    } => {
                        join_set.spawn(async move {
                            let exec = ToolExecution::blocked(message, reason);
                            report(&ptx, rounds, tool_use_id, tool_name, input, exec).await
                        });
                    }
                    GateOutcome::RequireApproval {
                        reason,
                        allow_save_allow,
                    } => {
                        let prompt_options =
                            workforce_core::policy::PermissionPromptOptions::ask(allow_save_allow);
                        let agent_id_str = self.config.agent_id.to_string();
                        let acting_user_id = CURRENT_USER_ID
                            .try_with(|u| u.clone())
                            .ok()
                            .filter(|u| !u.is_empty());

                        match route_approval(
                            self.config.approval_router.as_deref(),
                            self.config.agent_id.as_str(),
                            &tool_name,
                            &reason,
                            &input,
                        ) {
                            RouteResolution::Routed {
                                request_id,
                                approver_user_ids,
                                ..
                            } => {
                                stop_after_next_round = true;
                                let routing = ApprovalRouting {
                                    approver_user_ids,
                                    requester_user_id: acting_user_id.unwrap_or_default(),
                                };
                                join_set.spawn(async move {
                                    let _ = ptx
                                        .send(ProgressEvent::ApprovalRequired {
                                            request_id,
                                            agent_id: agent_id_str,
                                            tool_name: tool_name.clone(),
                                            detail,
                                            reason,
                                            round: rounds,
                                            options: prompt_options,
                                            routing: Some(routing),
                                        })
                                        .await;
                                    let _ = ptx
                                        .send(ProgressEvent::ToolEnd {
                                            name: tool_name.clone(),
                                            success: false,
                                            duration_ms: 0,
                                            round: rounds,
                                        })
                                        .await;
                                    (
                                        tool_use_id,
                                        tool_name,
                                        input,
                                        ROUTED_APPROVAL_TOOL_RESULT.to_string(),
                                        serde_json::Value::Null,
                                        false,
                                        0u64,
                                        "require_approval".to_string(),
                                        false,
                                        None,
                                        Vec::new(),
                                    )
                                });
                            }
                            RouteResolution::DenyUnreachable => {
                                join_set.spawn(async move {
                                    let msg = UNREACHABLE_APPROVER.to_string();
                                    let exec = ToolExecution::blocked(msg.clone(), msg);
                                    report(&ptx, rounds, tool_use_id, tool_name, input, exec).await
                                });
                            }
                            RouteResolution::SelfServe => {
                                let opened = approvals
                                    .as_ref()
                                    .ok_or(Refusal::Unattended)
                                    .and_then(|ledger| {
                                        let call = GatedCall {
                                            tool: tool_name.clone(),
                                            input: input.clone(),
                                        };
                                        ledger.open(call).map(|prompt| (Arc::clone(ledger), prompt))
                                    });
                                match opened {
                                    Err(refusal) => {
                                        join_set.spawn(async move {
                                            refuse(
                                                &ptx,
                                                rounds,
                                                tool_use_id,
                                                tool_name,
                                                input,
                                                refusal,
                                                reason,
                                            )
                                            .await
                                        });
                                    }
                                    Ok((ledger, prompt)) => {
                                        // The approved call runs in the spawned
                                        // task with the requester's captured
                                        // task-locals, so the gate sees their
                                        // identity and session.
                                        let approval_ctx = SessionContext::capture()
                                            .with_user_comm(&last_user_comm);
                                        let call =
                                            self.call_for(&mut round_calls, &tool_use_id, &input);
                                        let permission_writer =
                                            self.config.permission_writer.clone();
                                        let policy = Arc::clone(&self.policy);
                                        let access = Arc::clone(&access_snapshot);
                                        join_set.spawn(async move {
                                            let decision = await_approval(
                                                ApprovalAsk {
                                                    progress: Some(&ptx),
                                                    agent_id: &agent_id_str,
                                                    tool: &tool_name,
                                                    detail: &detail,
                                                    reason: &reason,
                                                    round: rounds,
                                                    options: prompt_options,
                                                    writer: permission_writer.as_deref(),
                                                    requester: acting_user_id.as_deref(),
                                                },
                                                &ledger,
                                                prompt,
                                                SELF_SERVE_APPROVAL_TIMEOUT,
                                            )
                                            .await;
                                            match decision.verdict() {
                                                Err(refusal) => {
                                                    refuse(
                                                        &ptx,
                                                        rounds,
                                                        tool_use_id,
                                                        tool_name,
                                                        input,
                                                        refusal,
                                                        reason,
                                                    )
                                                    .await
                                                }
                                                Ok(()) => {
                                                    let exec = approval_ctx
                                                        .scope(call.scope(scope_turn(
                                                            Some(ptx.clone()),
                                                            Some(Arc::clone(&ledger)),
                                                            execute_approved(
                                                                &gates, &policy, &tools, &access,
                                                                &tool_name, &input,
                                                            ),
                                                        )))
                                                        .await;
                                                    if exec.is_error {
                                                        ledger
                                                            .close(decision.call, Refusal::Failed);
                                                    }
                                                    report(
                                                        &ptx,
                                                        rounds,
                                                        tool_use_id,
                                                        tool_name,
                                                        input,
                                                        exec,
                                                    )
                                                    .await
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Collect results (order doesn't matter for tool_results, LLM matches by ID)
            while let Some(result) = join_set.join_next().await {
                if let Some(ref token) = cancel_token {
                    if token.is_cancelled() {
                        tel.mark_cancelled();
                        info!(agent = %self.config.agent_id, rounds, "Cancelled during tool execution");
                        join_set.abort_all();
                        break;
                    }
                }
                let (
                    tool_use_id,
                    tool_name,
                    input,
                    output_str,
                    output,
                    success,
                    duration_ms,
                    policy_decision,
                    is_error,
                    denial_reason,
                    loads,
                ) = result.map_err(|e| eyre::eyre!("Tool task panicked: {}", e))?;
                if let Some(gate) = compile_gate.as_mut() {
                    gate.observe(&tool_name, &input, &output, success);
                }
                let (output_str, output) = crate::spill::spill_tool_output(
                    &tool_name,
                    output_str,
                    output,
                    is_error,
                    &self.config.spill,
                    crate::spill::RetrievalSurface::from_registry(&self.tools),
                );
                let error = (!success).then(|| output_str.clone());
                let output_str = match loads.is_empty() {
                    true => output_str,
                    false => {
                        offered.queue_loads(&loads);
                        let (content, loaded) = loaded_result(&self.tools, output_str, &loads);
                        round_loads.insert(tool_use_id.clone(), loaded);
                        content
                    }
                };
                let tool_name_for_capture = tool_name.clone();
                let invocation = ToolInvocation {
                    tool_name,
                    input,
                    output,
                    success,
                    policy_decision,
                    duration_ms,
                    denial_reason,
                    error,
                };
                round_calls
                    .remove(&tool_use_id)
                    .unwrap_or_else(|| {
                        ToolCall::new(self.config.journal.clone(), invocation.input.clone())
                    })
                    .finished(&invocation)
                    .await;
                tool_invocations.push(invocation);
                tool_results.push((tool_use_id, tool_name_for_capture, output_str, is_error));
            }
            if let Some(nudge) = program_nudge.as_mut() {
                nudge.observe(&mut tool_results);
            }

            // What this round's programs called, recorded like direct calls.
            tool_invocations.extend(std::mem::take(&mut *round_nested.lock()));

            // Deliver any images this round's tools staged (streaming path).
            self.deliver_round_images(&tool_invocations[round_first..])
                .await;

            // ── Trace capture: write round with tool results (streaming path) ──
            if let Some(ref tw) = self.config.trace_writer {
                let input_content = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let trace_tool_results: Vec<crate::trace::ToolResultTrace> = tool_invocations
                    [round_first..]
                    .iter()
                    .map(redacted_tool_result_trace)
                    .collect();
                if let Err(e) = tw.write_round(&crate::trace::TraceRound {
                    round: rounds,
                    input: input_content,
                    response: response.content.clone(),
                    tool_results: trace_tool_results,
                    model: response.model.clone(),
                    stop_reason: response.stop_reason.clone(),
                    input_tokens: response.usage.input_tokens,
                    output_tokens: response.usage.output_tokens,
                    cache_read_tokens: response.usage.cache_read_input_tokens,
                    cache_creation_tokens: response.usage.cache_creation_input_tokens,
                }) {
                    warn!(error = %e, "Failed to write trace round");
                }
            }

            messages.push(with_loaded(
                LlmMessage::tool_results(tokenize_tool_outputs(tool_results)),
                &round_loads,
            ));

            // Render any images staged by this round's tools into the
            // conversation so the model sees them on its next step.
            self.append_viewed_images(&mut messages, &tool_invocations[round_first..], &run_model)
                .await;

            // ── Automatic context eviction (DW-418) ──
            // Periodically evict stale tool outputs to reduce token usage in long steps.
            if let Some(interval) = self.config.context_eviction_interval {
                if interval > 0 && rounds % interval == 0 {
                    let (evicted_count, freed_chars) = Self::evict_stale_tool_outputs(
                        &mut messages,
                        self.config.context_eviction_keep_last as usize,
                    );
                    if evicted_count > 0 {
                        info!(
                            agent = %self.config.agent_id,
                            round = rounds,
                            evicted = evicted_count,
                            freed_chars = freed_chars,
                            freed_tokens_est = freed_chars / 4,
                            "Evicted stale tool outputs from context"
                        );
                    }
                }
            }
        }

        // The only remaining break path is cancellation
        // Trace footer: cancelled
        if let Some(ref tw) = self.config.trace_writer {
            if let Err(e) = tw.write_footer(&crate::trace::TraceFooter {
                outcome: crate::trace::TraceOutcome::Cancelled,
                completed_at: chrono::Utc::now(),
                total_rounds: rounds,
                total_input_tokens: total_usage.input_tokens,
                total_output_tokens: total_usage.output_tokens,
                total_duration_ms: start_time.elapsed().as_millis() as u64,
            }) {
                warn!(error = %e, "Failed to write trace footer");
            }
        }
        Err(eyre::eyre!("Cancelled by user"))
    }

    /// This turn's compile gate, having seen the calls the turn made before
    /// its first round; `None` when the agent runs none.
    fn gate_turn(&self, seeded: &[ToolInvocation]) -> Option<GateTurn> {
        let mut gate = GateTurn::new(
            self.config.compile_gate.as_ref(),
            self.config.agent_id.as_str(),
        )?;
        for call in seeded {
            gate.observe(&call.tool_name, &call.input, &call.output, call.success);
        }
        Some(gate)
    }

    /// What a turn about to answer in round `rounds` does about the code it
    /// edited. A turn asked to stop checks nothing more; a fix round is only
    /// worth taking when the round after it can still call tools.
    async fn gate_answer(
        &self,
        gate: &mut Option<GateTurn>,
        access: &ToolAccessSnapshot,
        rounds: u32,
        stopping: bool,
        progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Verdict {
        let Some(gate) = gate.as_mut() else {
            return Verdict::Finish(None);
        };
        if stopping {
            return gate.standing();
        }
        let gates = ToolGates::from(&self.config);
        let authorise = |tool: &str, input: &serde_json::Value| {
            would_execute(&gates, &self.policy, &self.tools, access, tool, input)
        };
        gate.verdict(
            &authorise,
            rounds + 1 < self.config.max_rounds,
            progress,
            cancel,
        )
        .await
    }

    /// One round's non-streaming LLM call, with the recurring message prefix
    /// (if any) declared for the providers' breakpoint placement and the
    /// catalogued tools in scope for a wire that defers them natively.
    async fn create_round_message(
        &self,
        messages: &[LlmMessage],
        offered: &OfferedTools,
        model: &str,
        cached_prefix: Option<usize>,
    ) -> Result<LlmResponse> {
        with_cached_prefix(
            cached_prefix,
            with_tool_deferral(
                offered.plan.deferral(),
                self.llm.create_message(
                    Some(&offered.prompt(&self.config.system_prompt)),
                    messages,
                    &offered.defs,
                    Some(model),
                    self.config.effective_max_tokens(model),
                    true,
                ),
            ),
        )
        .await
    }

    /// Call the LLM with streaming, forwarding text deltas to the progress channel.
    ///
    /// Returns the fully-assembled `LlmResponse` once the stream completes,
    /// plus whether the stream gate aborted the round mid-stream (Mechanism A).
    /// Text deltas are emitted as `StreamStart` → `StreamDelta`* → `StreamEnd`
    /// progress events during generation. `cached_prefix` is the recurring
    /// message prefix declared for this round's breakpoint placement.
    async fn call_llm_streaming(
        &self,
        messages: &[LlmMessage],
        offered: &OfferedTools,
        progress_tx: &tokio::sync::mpsc::Sender<ProgressEvent>,
        tel: &mut crate::turn_event::TurnTelemetry,
        cached_prefix: Option<usize>,
    ) -> Result<(LlmResponse, bool)> {
        let (stream_tx, stream_rx) = tokio::sync::mpsc::channel::<StreamEvent>(128);

        // Spawn the streaming LLM call. `self.llm` is already wrapped
        // in [`SanitizingProvider`] at construction, so the egress
        // credential sweep runs inside the provider call.
        let llm = Arc::clone(&self.llm);
        let system_prompt = offered.prompt(&self.config.system_prompt).into_owned();
        let messages_owned: Vec<LlmMessage> = messages.to_vec();
        let tool_defs_owned: Vec<LlmToolDef> = offered.defs.clone();
        let deferral = offered.plan.deferral();
        let model = self.config.model.clone();
        let max_tokens = model
            .as_deref()
            .map(|m| self.config.effective_max_tokens(m))
            .unwrap_or(self.config.max_tokens);

        // Task-locals don't cross `tokio::spawn`: the session context (user,
        // billing identity, served log) rides `spawn_in_session_context`, and
        // the per-turn thinking budget, the recurring prefix and the
        // catalogued tools are carried over by hand — otherwise the override
        // set for a voice turn would reach every non-streaming call and
        // silently miss the streaming one, which is the path voice actually
        // takes.
        let thinking_override = crate::llm::provider::thinking_budget_override();
        let effort = crate::llm::provider::effort_override();
        let stream_handle = spawn_in_session_context(async move {
            with_cached_prefix(
                cached_prefix,
                with_tool_deferral(
                    deferral,
                    crate::llm::provider::with_thinking_budget_override(
                        thinking_override,
                        crate::llm::provider::with_effort_override(
                            effort,
                            llm.create_message_stream(
                                Some(&system_prompt),
                                &messages_owned,
                                &tool_defs_owned,
                                model.as_deref(),
                                max_tokens,
                                true,
                                stream_tx,
                            ),
                        ),
                    ),
                ),
            )
            .await
        });

        // Process stream events: forward text deltas to progress_tx and collect the response
        let response = self
            .process_stream_events(stream_rx, progress_tx, tel)
            .await;

        if matches!(response, Ok((_, true))) {
            // Stream-gate abort: some providers ignore the dropped receiver
            // and keep draining the HTTP stream (the model keeps generating)
            // — cancel the producer so the connection closes now.
            stream_handle.abort();
            return response;
        }

        // Wait for the stream producer to finish
        let producer_result = stream_handle
            .await
            .map_err(|e| eyre::eyre!("LLM stream task panicked: {}", e))?;

        // The producer's error is authoritative: it names the cause where the
        // consumer's is a symptom ("empty response"), and a stream that failed
        // after a block closed leaves a collected response that is truncated.
        producer_result.and(response)
    }

    /// Open a stream segment if needed and forward one delta to the preview.
    /// `forwarded_any` records that this LLM call put text on screen — the
    /// signal that makes a later `StreamDiscard` meaningful.
    async fn forward_stream_delta(
        progress_tx: &tokio::sync::mpsc::Sender<ProgressEvent>,
        streaming_text: &mut bool,
        forwarded_any: &mut bool,
        text: String,
    ) {
        if !*streaming_text {
            *streaming_text = true;
            let _ = progress_tx.send(ProgressEvent::StreamStart).await;
        }
        *forwarded_any = true;
        let _ = progress_tx.send(ProgressEvent::StreamDelta { text }).await;
    }

    /// Process a stream of LLM events, forwarding text deltas as progress
    /// events through the [`StreamGate`] and assembling the final response.
    ///
    /// Returns the response plus whether the gate aborted the stream — in
    /// which case the response carries the suppressed text and no token usage
    /// (providers report usage only at stream end, so an aborted round
    /// records none).
    async fn process_stream_events(
        &self,
        mut stream_rx: tokio::sync::mpsc::Receiver<StreamEvent>,
        progress_tx: &tokio::sync::mpsc::Sender<ProgressEvent>,
        tel: &mut crate::turn_event::TurnTelemetry,
    ) -> Result<(LlmResponse, bool)> {
        use crate::llm::streaming::StreamCollector;

        let mut collector = StreamCollector::new();
        let mut streaming_text = false;
        let mut forwarded_any = false;
        let mut gate = StreamGate::new(&self.tools);
        let mut aborted_at: Option<usize> = None;

        while let Some(event) = stream_rx.recv().await {
            // UI-only signals (pacing waits, failover notices, errors) are
            // not model output — stamping ttft from them would fake a fast
            // first token in exactly the delayed turns the metric exists
            // to expose.
            if !matches!(
                event,
                StreamEvent::RateLimitWait { .. }
                    | StreamEvent::SlowModelFailover { .. }
                    | StreamEvent::KeepAlive { .. }
                    | StreamEvent::Restart
                    | StreamEvent::Error { .. }
            ) {
                tel.observe_first_stream_event();
            }
            // Layer real-time UI feedback on top of the shared accumulator —
            // text deltas open a stream segment, tool calls and errors close it.
            match &event {
                StreamEvent::TextDelta { index, text } => match gate.on_text(text, &self.tools) {
                    GateVerdict::Forward(chunk) => {
                        Self::forward_stream_delta(
                            progress_tx,
                            &mut streaming_text,
                            &mut forwarded_any,
                            chunk,
                        )
                        .await;
                    }
                    GateVerdict::Held => {}
                    GateVerdict::Abort => aborted_at = Some(*index),
                },
                StreamEvent::ToolUseStart { .. } => {
                    // A native call closes the text segment — release the
                    // hold first so text the model paired with a real call is
                    // never lost from the preview.
                    if let Some(held) = gate.release(&self.tools) {
                        Self::forward_stream_delta(
                            progress_tx,
                            &mut streaming_text,
                            &mut forwarded_any,
                            held,
                        )
                        .await;
                    }
                    if streaming_text {
                        streaming_text = false;
                        let _ = progress_tx.send(ProgressEvent::StreamEnd).await;
                    }
                }
                StreamEvent::Error { .. } if streaming_text => {
                    streaming_text = false;
                    let _ = progress_tx.send(ProgressEvent::StreamEnd).await;
                }
                // The call starts over: void what it previewed, and leave the
                // segment unclosed so the replacement opens a fresh one — a
                // `StreamEnd` here would read as the answer ending.
                StreamEvent::Restart => {
                    if forwarded_any {
                        let _ = progress_tx
                            .send(ProgressEvent::StreamDiscard {
                                reason: StreamDiscardReason::Interrupted,
                            })
                            .await;
                    }
                    gate = StreamGate::new(&self.tools);
                    forwarded_any = false;
                    streaming_text = false;
                    tel.observe_stream_restart();
                }
                StreamEvent::RateLimitWait { wait_secs, attempt } => {
                    let _ = progress_tx
                        .send(ProgressEvent::RateLimited {
                            wait_secs: *wait_secs,
                            attempt: *attempt,
                        })
                        .await;
                }
                StreamEvent::SlowModelFailover {
                    model,
                    elapsed_secs,
                } => {
                    let _ = progress_tx
                        .send(ProgressEvent::SlowModelFailover {
                            model: model.clone(),
                            elapsed_secs: *elapsed_secs,
                        })
                        .await;
                }
                _ => {}
            }
            collector.process(event)?;
            if let Some(index) = aborted_at {
                // Mechanism A abort: stop consuming (dropping the receiver
                // ends the producer), flush the open text block into the
                // collector so the suppressed text still reaches round
                // history, and tell streaming consumers to drop whatever this
                // round previewed.
                collector.process(StreamEvent::ContentStop { index })?;
                // Only a round that previewed text has anything to discard —
                // an all-held abort must not consume the preview's retry
                // fallback for an older, real round.
                if forwarded_any {
                    let _ = progress_tx
                        .send(ProgressEvent::StreamDiscard {
                            reason: StreamDiscardReason::Retry,
                        })
                        .await;
                }
                break;
            }
        }

        if aborted_at.is_none() {
            // Stream ended with text still held (short round, or a suspect
            // that never matched): the hold is lossless.
            if let Some(held) = gate.release(&self.tools) {
                Self::forward_stream_delta(
                    progress_tx,
                    &mut streaming_text,
                    &mut forwarded_any,
                    held,
                )
                .await;
            }
        }
        if streaming_text {
            let _ = progress_tx.send(ProgressEvent::StreamEnd).await;
        }

        let (blocks, stop_reason, usage) = collector.finish();

        // A zero-token, no-content response means the stream failed silently
        // (e.g. all providers rate-limited). Fail loudly so the runner doesn't
        // hand back an empty green-tick reply.
        if blocks.is_empty() && usage.input_tokens == 0 && usage.output_tokens == 0 {
            return Err(eyre::eyre!(
                "LLM returned empty response (0 tokens, no content)"
            ));
        }

        Ok((
            LlmResponse {
                content: blocks,
                stop_reason,
                usage,
                model: self
                    .config
                    .model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            },
            aborted_at.is_some(),
        ))
    }

    /// Mechanism B — tool-call repair. When a round produced no native
    /// tool_use but its text is a hallucinated tool call that
    /// [`parse_text_tool_call`] admits, return a replacement response whose
    /// sole content is the synthesized `ToolUse` block. The caller lets it flow
    /// through the identical per-round execution path as a native call — policy
    /// evaluation, capability/cascade gates, sentinel scan, approval routing —
    /// and the round's text is superseded by the call.
    fn repaired_text_tool_call(&self, response: &LlmResponse, round: u32) -> Option<LlmResponse> {
        if response.has_tool_use() {
            return None;
        }
        let (name, input) = parse_text_tool_call(&response.all_text()?, &self.tools)?;
        info!(tool = %name, round, "repaired hallucinated text tool call");
        // UUID-suffixed like `preexec-{uuid}`: session history spans turns, so
        // a bare round number would collide across turns in one conversation.
        Some(LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: format!("repaired-{round}-{}", uuid::Uuid::new_v4()),
                name,
                input,
                signature: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: response.usage.clone(),
            model: response.model.clone(),
        })
    }

    /// Build LLM tool definitions from the tool registry.
    ///
    /// Classify injected messages into user follow-ups and steering commands.
    ///
    /// Returns `(follow_ups, should_stop)`.
    fn process_injected_messages(messages: &[InjectedMessage]) -> (Vec<InjectedUserMessage>, bool) {
        let stop_phrases = [
            "stop",
            "hold on",
            "wait",
            "cancel",
            "nevermind",
            "never mind",
        ];
        let mut follow_ups = Vec::new();
        let mut should_stop = false;

        for msg in messages {
            match msg {
                InjectedMessage::UserMessage {
                    text,
                    user,
                    external_id,
                } => {
                    let lower = text.to_lowercase();
                    let trimmed = lower.trim();
                    if stop_phrases
                        .iter()
                        .any(|p| trimmed == *p || trimmed.starts_with(&format!("{p} ")))
                    {
                        should_stop = true;
                    } else {
                        follow_ups.push(InjectedUserMessage {
                            user: user.clone(),
                            text: text.clone(),
                            external_id: external_id.clone(),
                        });
                    }
                }
                InjectedMessage::SteerStop { .. } => {
                    should_stop = true;
                }
                InjectedMessage::ApprovalResponse { .. } => {
                    // Handled by the approval router task, not here.
                }
            }
        }

        (follow_ups, should_stop)
    }

    /// Drain pending injected messages and absorb them into the conversation:
    /// follow-up texts become a user message (and are recorded for session
    /// persistence), a steer-stop arms `stop_after_next_round`. Returns the
    /// number of follow-up texts absorbed so callers can decide whether a
    /// would-be-final round needs one more pass.
    #[allow(clippy::too_many_arguments)]
    async fn absorb_injected_followups(
        &self,
        fwd_inject_rx: &mut Option<tokio::sync::mpsc::Receiver<InjectedMessage>>,
        messages: &mut Vec<LlmMessage>,
        absorbed: &mut Vec<InjectedUserMessage>,
        stop_after_next_round: &mut bool,
        progress_tx: &tokio::sync::mpsc::Sender<ProgressEvent>,
        rounds: u32,
    ) -> usize {
        let injected: Vec<InjectedMessage> = fwd_inject_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        if injected.is_empty() {
            return 0;
        }
        let (follow_ups, should_stop) = Self::process_injected_messages(&injected);
        if !follow_ups.is_empty() {
            let combined = follow_ups
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.push(LlmMessage::user(format!(
                "[FOLLOW-UP from the user — incorporate this into your current task]\n\n{}",
                combined
            )));
            info!(
                agent = %self.config.agent_id,
                count = injected.len(),
                round = rounds,
                "Injected mid-run user messages"
            );
        }
        if should_stop {
            messages.push(LlmMessage::user(
                "[SYSTEM] The user asked you to stop. Wrap up your current operation, \
                 summarize what you've done so far, and acknowledge the user's request. \
                 Do NOT start any new tool calls."
                    .to_string(),
            ));
            *stop_after_next_round = true;
        }
        let _ = progress_tx
            .send(ProgressEvent::InjectedMessage {
                count: injected.len() as u32,
            })
            .await;
        let absorbed_count = follow_ups.len();
        absorbed.extend(follow_ups);
        absorbed_count
    }

    /// Tool names are sanitised for the LLM API (dots → underscores)
    /// because providers like Anthropic only allow `[a-zA-Z0-9_-]`.
    /// Evict stale tool outputs from the message history to reduce context size.
    ///
    /// Groups all `ToolResult` content blocks by the tool the preceding
    /// `ToolUse` block called (through `tool_call` or directly). For each tool,
    /// preserves the `keep_last` most recent results and replaces older large
    /// results (≥500 chars) with compact markers; results that loaded tools
    /// keep their definitions. Returns `(evicted_count, freed_chars)`.
    fn evict_stale_tool_outputs(messages: &mut [LlmMessage], keep_last: usize) -> (usize, usize) {
        // Phase 1: Build an index of all ToolResult locations with their tool names.
        // We need the tool name from the corresponding ToolUse block.
        // Build a map from tool_use_id → tool_name by scanning ToolUse blocks.
        let mut tool_use_names: HashMap<String, String> = HashMap::new();
        for msg in messages.iter() {
            for block in &msg.content {
                if let ContentBlock::ToolUse {
                    id, name, input, ..
                } = block
                {
                    let tool = match crate::tools::loading::unwrap_call(name, input) {
                        Some(Ok(call)) => call.tool,
                        _ => name.clone(),
                    };
                    tool_use_names.insert(id.clone(), tool);
                }
            }
        }

        // Phase 2: Collect all ToolResult positions with their tool names and content sizes.
        // Each entry: (message_index, block_index, tool_name, content_len)
        let mut tool_results: Vec<(usize, usize, String, usize)> = Vec::new();
        for (msg_idx, msg) in messages.iter().enumerate() {
            for (block_idx, block) in msg.content.iter().enumerate() {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    loaded: None,
                    ..
                } = block
                {
                    let tool_name = tool_use_names
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    tool_results.push((msg_idx, block_idx, tool_name, content.len()));
                }
            }
        }

        // Phase 3: Group by tool name and determine which to evict.
        // Within each group, results are already in chronological order (by position).
        let mut groups: HashMap<String, Vec<(usize, usize, usize)>> = HashMap::new();
        for (msg_idx, block_idx, tool_name, content_len) in tool_results {
            groups
                .entry(tool_name)
                .or_default()
                .push((msg_idx, block_idx, content_len));
        }

        let mut evicted_count = 0usize;
        let mut freed_chars = 0usize;

        // Phase 4: For each tool group, evict all but the last `keep_last` results.
        for entries in groups.values() {
            if entries.len() <= keep_last {
                continue;
            }
            let evict_count = entries.len() - keep_last;
            for &(msg_idx, block_idx, content_len) in entries.iter().take(evict_count) {
                // Only evict if content is large enough to matter
                if content_len < 500 {
                    continue;
                }
                // Replace content with a compact marker
                if let Some(msg) = messages.get_mut(msg_idx) {
                    if let Some(ContentBlock::ToolResult { content, .. }) =
                        msg.content.get_mut(block_idx)
                    {
                        let marker = format!("[evicted — {} chars freed]", content_len);
                        freed_chars += content_len - marker.len();
                        *content = marker;
                        evicted_count += 1;
                    }
                }
            }
        }

        (evicted_count, freed_chars)
    }

    /// What a turn plans from, given the smallest context window the request
    /// can reach.
    fn plan_context<'a>(
        &'a self,
        session: &'a BTreeSet<String>,
        context_floor: Option<u32>,
    ) -> PlanContext<'a> {
        PlanContext {
            loading: &self.config.tool_loading,
            session,
            context_floor,
            precurated: tools_precurated(),
        }
    }

    /// The tools a request for `model` would be offered now, before any load.
    fn offered_for(&self, model: &str) -> OfferedTools {
        let session = session_tools();
        let mut offered = self.offered_tools();
        offered.refresh(
            &self.tools,
            self.tool_access.as_ref(),
            &self.plan_context(&session, crate::llm::model_limits::context_for(model)),
        );
        offered
    }

    /// Compare this round's cached prefix with the previous round's and say
    /// so when it moved: expected when the registry or access changed or a
    /// tool inlined on demand, a warning otherwise.
    fn observe_prefix(
        &self,
        previous: &mut Option<PrefixFingerprint>,
        next: PrefixFingerprint,
        round: u32,
        tools_rebuilt: bool,
    ) {
        if let Some(drift) = previous.as_ref().and_then(|p| p.drift(&next)) {
            match tools_rebuilt || drift.is_growth() {
                true => info!(
                    agent = %self.config.agent_id,
                    round,
                    %drift,
                    "Prompt prefix changed mid-turn (expected)"
                ),
                false => warn!(
                    agent = %self.config.agent_id,
                    round,
                    %drift,
                    "Prompt prefix changed mid-turn"
                ),
            }
        }
        *previous = Some(next);
    }
}

/// The registry's tools a caller with `access` may use: neither blocked nor a
/// dormant MCP workspace member.
fn permitted_definitions(
    registry: &ToolRegistry,
    access: &ToolAccessSnapshot,
) -> Vec<crate::tools::ToolDefinition> {
    registry
        .definitions()
        .into_iter()
        .filter(|def| {
            !access.is_blocked(&def.name)
                && crate::llm::mcp_workspace::is_tool_visible(&to_llm_name(&def.name))
        })
        .collect()
}

/// The tools a turn offers the model: its plan and the definitions it
/// declares. The plan is rebuilt only when the registry, the caller's access
/// or the request's context floor changes, so a definition that renders
/// differently between rounds cannot move the cached prefix on its own; a
/// load lands on the next round (DW-277).
#[derive(Default)]
pub(crate) struct OfferedTools {
    key: Option<(u64, Option<u32>)>,
    pub(crate) access: Arc<ToolAccessSnapshot>,
    pub(crate) plan: ToolPlan,
    pub(crate) defs: Vec<LlmToolDef>,
    shadow: Option<ToolPlan>,
    pending: Vec<String>,
    /// The runner's own tools declared besides the plan's, whatever it loads.
    intrinsics: Vec<LlmToolDef>,
}

impl OfferedTools {
    /// Offered tools that always declare `intrinsics` besides the plan's.
    pub(crate) fn with_intrinsics(intrinsics: Vec<LlmToolDef>) -> Self {
        Self {
            intrinsics,
            ..Self::default()
        }
    }

    /// Bring the plan up to date and apply the loads queued since the last
    /// round; `true` when the plan was rebuilt.
    pub(crate) fn refresh(
        &mut self,
        registry: &ToolRegistry,
        access: &dyn ToolAccess,
        ctx: &PlanContext<'_>,
    ) -> bool {
        let key = (registry.generation(), ctx.context_floor);
        let snapshot = access.snapshot();
        let rebuilt = self.key != Some(key) || *self.access != snapshot;
        if rebuilt {
            let definitions = permitted_definitions(registry, &snapshot);
            self.shadow = (ctx.loading.mode == ToolLoadingMode::Shadow)
                .then(|| ToolPlan::would_defer(definitions.clone(), ctx));
            self.plan = ToolPlan::build(definitions, ctx).keep_loads(&self.plan);
            self.access = Arc::new(snapshot);
            self.key = Some(key);
        }
        for name in std::mem::take(&mut self.pending) {
            self.plan.load(&[name]);
        }
        if rebuilt {
            self.defs = self.plan.declared();
            self.defs.extend(self.intrinsics.iter().cloned());
        }
        rebuilt
    }

    /// Queue tools a call made callable, for the next round.
    pub(crate) fn queue_loads(&mut self, loads: &[String]) {
        self.pending.extend_from_slice(loads);
    }

    /// The system prompt a round sends: the agent's own, with the catalogue.
    pub(crate) fn prompt<'a>(&self, system_prompt: &'a str) -> std::borrow::Cow<'a, str> {
        self.plan.prompt(system_prompt)
    }

    /// Say so, in shadow, when `tool` is a call loading on demand would have
    /// had to load first.
    pub(crate) fn note_shadow_call(&self, agent: &str, tool: &str) {
        if self
            .shadow
            .as_ref()
            .is_some_and(|shadow| shadow.is_unloaded(tool))
        {
            info!(agent, tool, "shadow: call would have needed a tool load");
        }
    }

    /// Log what this turn catalogues, and what shadowing would.
    pub(crate) fn log_plan(&self, agent: &str) {
        let (catalogued, tokens_saved) = self.plan.deferred_summary();
        if catalogued > 0 {
            info!(
                agent,
                catalogued, tokens_saved, "Tools catalogued for loading"
            );
        }
        if let Some(shadow) = &self.shadow {
            let (catalogued, tokens_saved) = shadow.deferred_summary();
            info!(
                agent,
                catalogued, tokens_saved, "shadow: tools that would be catalogued"
            );
        }
    }
}

/// Convert an internal tool name to an LLM-safe name (dots → underscores).
pub fn to_llm_name(name: &str) -> String {
    name.replace('.', "_")
}

/// Convert an LLM tool name back to the internal name (underscores → dots).
///
/// Underscore-to-dot is ambiguous for names with multiple segments (e.g.
/// `google_sheets_batch_update` could be `google.sheets.batch_update`,
/// `google.sheets_batch.update`, or `google_sheets.batch.update`). The only
/// unambiguous reverse is to ask the registry: scan every registered name
/// and find the one whose `to_llm_name(...)` equals `llm_name`.
///
/// Cost is O(N) in the number of registered tools per call; with ~100 tools
/// this is dominated by string allocation and is negligible compared to the
/// LLM round-trip that triggered the call.
pub fn from_llm_name(llm_name: &str, registry: &ToolRegistry) -> String {
    // Direct match (e.g. names that contain no dots) is the fast path.
    if registry.get(llm_name).is_some() {
        return llm_name.to_string();
    }

    // Authoritative scan: a registered name with N dots becomes a name with
    // N more underscores under `to_llm_name`. Find the unique inverse.
    for name in registry.names() {
        if to_llm_name(&name) == llm_name {
            return name;
        }
    }

    // No match — return the LLM-side name so the caller's lookup fails with
    // a clear "tool not found" error rather than silently misrouting.
    llm_name.to_string()
}

/// A tool call resolved from its LLM-side form to a registered call.
#[derive(Debug)]
pub struct ResolvedToolCall<'a> {
    /// Registry name to look up and to gate under.
    pub name: String,
    /// Input to call it with — borrowed unless an action had to be injected.
    pub input: Cow<'a, serde_json::Value>,
    /// `Some(action)` when the model flattened an action-dispatch tool into a
    /// single `tool_action` name and the call was rewritten. Callers log it so
    /// the per-model mis-naming rate stays queryable.
    pub repaired_action: Option<String>,
}

/// Resolve an LLM-emitted tool name — and, where needed, its input — to a
/// registered call.
///
/// Beyond [`from_llm_name`]'s underscore↔dot inverse, this repairs the one
/// shape that inverse cannot express: an *action-dispatch* tool (one registered
/// tool taking an `action` enum, e.g. `notes`) called under the `tool_action`
/// name that every namespaced tool uses (`jira.search` → `jira_search`). Such a
/// call would otherwise fail with "tool not found" and silently drop the work.
///
/// The rewrite is admitted only when the tool's own schema proves it: exactly
/// one split of the name must yield a registered tool whose declared `action`
/// enum contains the full remainder, with no conflicting `action` already in
/// the input. Zero or several such splits reject — an ambiguous name is a
/// clear error, never a guess.
pub fn resolve_llm_tool_call<'a>(
    llm_name: &str,
    input: &'a serde_json::Value,
    registry: &ToolRegistry,
) -> ResolvedToolCall<'a> {
    let name = from_llm_name(llm_name, registry);
    if registry.has(&name) {
        return ResolvedToolCall {
            name,
            input: Cow::Borrowed(input),
            repaired_action: None,
        };
    }
    match action_dispatch_split(llm_name, input, registry) {
        Some((tool, action)) => {
            let mut object = input.as_object().cloned().unwrap_or_default();
            object.insert(
                "action".to_string(),
                serde_json::Value::String(action.clone()),
            );
            ResolvedToolCall {
                name: tool,
                input: Cow::Owned(serde_json::Value::Object(object)),
                repaired_action: Some(action),
            }
        }
        None => ResolvedToolCall {
            name,
            input: Cow::Borrowed(input),
            repaired_action: None,
        },
    }
}

/// Where a tool call entered the system, so one query over the repair warning
/// returns the same columns for every dispatch path.
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolCallOrigin<'a> {
    /// Dispatch path: `runner`, `chat`, `mcp`, `advisor`.
    pub via: &'static str,
    pub agent: Option<&'a str>,
    pub model: Option<&'a str>,
    pub round: Option<u32>,
}

impl ToolCallOrigin<'_> {
    const UNSET: &'static str = "-";
}

/// [`resolve_llm_tool_call`] plus the one log line every dispatch loop owes a
/// repair. Every path that turns a model-emitted name into an execution goes
/// through here, so no loop can skip either half and no two emit different
/// fields for the same event.
pub fn resolve_tool_call_logged<'a>(
    llm_name: &str,
    input: &'a serde_json::Value,
    registry: &ToolRegistry,
    origin: ToolCallOrigin<'_>,
) -> ResolvedToolCall<'a> {
    let resolved = resolve_llm_tool_call(llm_name, input, registry);
    if let Some(action) = &resolved.repaired_action {
        warn!(
            via = %origin.via,
            agent = %origin.agent.unwrap_or(ToolCallOrigin::UNSET),
            model = %origin.model.unwrap_or(ToolCallOrigin::UNSET),
            round = origin.round,
            llm_tool = %llm_name,
            tool = %resolved.name,
            action = %action,
            "repaired flattened action-dispatch tool call"
        );
    }
    resolved
}

/// The unique `(registry tool, action)` a flattened name splits into, or `None`
/// when the name is not an action-dispatch call or is ambiguous.
fn action_dispatch_split(
    llm_name: &str,
    input: &serde_json::Value,
    registry: &ToolRegistry,
) -> Option<(String, String)> {
    // Models emit both `notes_write_human` and `notes.write_human`; canonicalise
    // to the underscore form so one split rule covers both (mirrors the
    // dotted/underscored reconciliation in `mcp_servers::shadows`).
    let canonical = to_llm_name(llm_name);
    if !matches!(
        input,
        serde_json::Value::Object(_) | serde_json::Value::Null
    ) {
        return None;
    }
    let requested_action = input.get("action").and_then(serde_json::Value::as_str);
    let mut candidates = canonical.match_indices('_').filter_map(|(at, _)| {
        let action = &canonical[at + 1..];
        // A conflicting explicit action makes the intent ambiguous: the name
        // says one thing, the argument another. Never pick a winner.
        if requested_action.is_some_and(|existing| existing != action) {
            return None;
        }
        let tool = registry.get(&from_llm_name(&canonical[..at], registry))?;
        declares_action(&tool.input_schema(), action)
            .then(|| (tool.name().to_string(), action.to_string()))
    });
    let only = candidates.next()?;
    candidates.next().is_none().then_some(only)
}

/// Whether an input schema declares an `action` enum listing `action`. A tool's
/// own schema is the sole authority for whether it can serve the rewritten call.
fn declares_action(schema: &serde_json::Value, action: &str) -> bool {
    declared_actions(schema).is_some_and(|actions| actions.iter().any(|value| value == action))
}

/// The error handed back for an unresolvable tool name. Names registered under
/// the same first segment are offered so the model's next round can self-correct
/// instead of retrying the same miss.
pub fn tool_not_found_message(name: &str, registry: &ToolRegistry) -> String {
    let canonical = to_llm_name(name);
    let namespace = canonical.split('_').next().unwrap_or_default();
    if namespace.is_empty() {
        return format!("Tool '{name}' not found");
    }
    // A miss whose namespace IS an action-dispatch tool is the flattened-name
    // mistake that got past the repair (an action the tool doesn't declare).
    // Naming the tool alone would leave the model guessing again — name the
    // actions it actually serves.
    if let Some(actions) = registry.get(namespace).and_then(|tool| {
        declared_actions(&tool.input_schema()).filter(|actions| !actions.is_empty())
    }) {
        return format!(
            "Tool '{name}' not found. `{namespace}` takes an `action`: {}",
            actions.join(", ")
        );
    }
    let mut suggestions: Vec<String> = registry
        .names()
        .into_iter()
        .map(|registered| to_llm_name(&registered))
        .filter(|registered| registered.split('_').next() == Some(namespace))
        .collect();
    suggestions.sort();
    suggestions.truncate(TOOL_NOT_FOUND_SUGGESTIONS);
    match suggestions.is_empty() {
        true => format!("Tool '{name}' not found"),
        false => format!(
            "Tool '{name}' not found. Did you mean: {}?",
            suggestions.join(", ")
        ),
    }
}

/// The `action` enum an input schema declares, if it is an action-dispatch tool.
fn declared_actions(schema: &serde_json::Value) -> Option<Vec<String>> {
    Some(
        schema
            .get("properties")?
            .get("action")?
            .get("enum")?
            .as_array()?
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
    )
}

/// What the built-in tools need beyond the workspace.
#[derive(Clone, Default)]
pub struct ToolSecrets {
    /// Where `web.search` finds its Brave Search key on each call; the tool
    /// is registered only with a source.
    pub brave_api_key: Option<ApiKeySource>,
    /// The privilege-separated runner daemon (audit F7). When `Some`,
    /// `exec.run` dispatches to the daemon (fail-closed if unreachable)
    /// instead of executing commands in-process. `None` = in-process (dev).
    pub runner: Option<crate::tools::exec_runner::RunnerEndpoint>,
}

impl ToolSecrets {
    /// Resolve tool secrets from environment variables only (no vault).
    pub fn from_env() -> Self {
        Self {
            brave_api_key: std::env::var("BRAVE_API_KEY").ok().map(fixed_key),
            runner: None,
        }
    }
}

/// Builder for creating a default set of tools.
pub fn default_tools(allowed_paths: Vec<std::path::PathBuf>, secrets: ToolSecrets) -> ToolRegistry {
    use crate::tools::exec::ExecCommand;
    use crate::tools::filesystem::{EditFile, ListDir, ReadFile, WriteFile};
    use crate::tools::security::SecurityAuditTool;
    use crate::tools::web::{WebFetchTool, WebSearchTool};

    let registry = ToolRegistry::new();
    registry.register(Arc::new(ReadFile::new(allowed_paths.clone())));
    registry.register(Arc::new(WriteFile::new(allowed_paths.clone())));
    registry.register(Arc::new(EditFile::new(allowed_paths.clone())));
    registry.register(Arc::new(ListDir::new(allowed_paths.clone())));

    let mut exec = if let Some(workspace) = allowed_paths.first() {
        ExecCommand::default().with_workspace(workspace.clone())
    } else {
        ExecCommand::default()
    };
    // Add any additional allowed paths (e.g. worktrees dir) so exec.run
    // can set workdir to workflow worktree directories.
    for path in allowed_paths.iter().skip(1) {
        exec = exec.with_allowed_workdir(path.clone());
    }
    let exec = exec.with_runner(secrets.runner.clone());
    registry.register(Arc::new(exec));

    // Security audit tool
    registry.register(Arc::new(SecurityAuditTool::new(allowed_paths.clone())));

    // Web tools
    registry.register(Arc::new(WebFetchTool::new()));
    if let Some(api_key) = secrets.brave_api_key {
        registry.register(Arc::new(WebSearchTool::new(api_key)));
    }

    // Wall-clock tools (time.sleep) — bounded waits for polling loops.
    for tool in crate::tools::time::time_tools() {
        registry.register(tool);
    }

    registry
}

/// Whether `text` names a tool this agent actually holds, dotted or
/// underscored.
fn mentions_registered_tool(text: &str, tool_names: &[String]) -> bool {
    let lower = text.to_lowercase();
    tool_names
        .iter()
        .any(|name| lower.contains(&name.replace('.', "_")) || lower.contains(name))
}

/// Remove markdown code-fence marker lines so fenced JSON is judged by its
/// content, not its wrapping.
fn strip_code_fences(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Locate the outermost JSON object in `text`: the byte span from the first
/// `{` to its balanced close, honouring string literals and escapes (a brace
/// inside a string does not count). Returns the start offset and, when the
/// object closed, the end offset (exclusive); `None` end = still open
/// (truncated or mid-stream).
fn outer_json_object_span(text: &str) -> Option<(usize, Option<usize>)> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in text[start..].char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, Some(start + idx + ch.len_utf8())));
                }
            }
            _ => {}
        }
    }
    Some((start, None))
}

/// What a round of pure text actually is: a call to execute, a call the model
/// wrote out instead of making, or the answer.
///
/// One verdict serves all three readers of that text — the streaming preview
/// gate, the repair path and both round handlers — so none of them can decide
/// differently about the same round.
enum TextCallVerdict {
    /// A complete call to a registered tool: synthesize and execute it.
    Executable {
        name: String,
        input: serde_json::Value,
    },
    /// Call-shaped but not callable — truncated, or naming a tool this agent
    /// holds under a payload that is not a call. Never an answer.
    Malformed,
    /// The model answered. Deliver it verbatim.
    Prose,
}

/// Classify a round's text.
///
/// The outermost JSON object must dominate the fence-stripped text (≥60% of
/// its trimmed length) before the text is a call at all: prose that quotes a
/// tool name or embeds an example is an answer, and discarding an answer costs
/// far more than delivering the rare call that carries a long preamble. Past
/// that gate a payload we can execute is [`TextCallVerdict::Executable`]; one
/// whose top-level keys are pure call vocabulary is
/// [`TextCallVerdict::Malformed`] when it never closed, or when the text names
/// a tool this agent holds — a closed object naming an unknown tool is data the
/// reader asked for.
fn classify_text_tool_call(text: &str, tools: &ToolRegistry) -> TextCallVerdict {
    let stripped = strip_code_fences(text);
    let trimmed = stripped.trim();
    let Some((start, end)) = outer_json_object_span(trimmed) else {
        return TextCallVerdict::Prose;
    };
    if (end.unwrap_or(trimmed.len()) - start) * 5 < trimmed.len() * 3 {
        return TextCallVerdict::Prose;
    }
    if let Some((name, input)) = end.and_then(|end| executable_call(&trimmed[start..end], tools)) {
        return TextCallVerdict::Executable { name, input };
    }
    if first_object_call_shaped(trimmed)
        && (end.is_none() || mentions_registered_tool(trimmed, &tools.names()))
    {
        TextCallVerdict::Malformed
    } else {
        TextCallVerdict::Prose
    }
}

/// The balanced JSON object `span` read as a call: it must name a registered
/// tool under a [`TOOL_CALL_NAME_KEYS`] key and carry an object input under a
/// [`TOOL_CALL_INPUT_KEYS`] key. Returns the resolved registry name and the
/// input to call it with.
fn executable_call(span: &str, tools: &ToolRegistry) -> Option<(String, serde_json::Value)> {
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(span).ok()?;
    let name = TOOL_CALL_NAME_KEYS
        .iter()
        .find_map(|key| object.get(*key)?.as_str())?;
    let input = match TOOL_CALL_INPUT_KEYS.iter().find_map(|key| object.get(*key)) {
        Some(value) => value.is_object().then(|| value.clone())?,
        // No input key: admit an implicit `{}` only when the name key is the
        // object's sole key — anything else is not unambiguously a call.
        None => (object.len() == 1).then(|| serde_json::json!({}))?,
    };
    let resolved = resolve_llm_tool_call(name, &input, tools);
    tools
        .has(&resolved.name)
        .then(|| (resolved.name, resolved.input.into_owned()))
}

/// The executable projection of [`classify_text_tool_call`] — the strict
/// admission for repairing a tool call the model emitted as text.
fn parse_text_tool_call(text: &str, tools: &ToolRegistry) -> Option<(String, serde_json::Value)> {
    match classify_text_tool_call(text, tools) {
        TextCallVerdict::Executable { name, input } => Some((name, input)),
        _ => None,
    }
}

/// Top-level keys of the first JSON object in `text`, as far as the text
/// (possibly a truncated stream prefix) reveals them. A key counts only once
/// its string has closed and its `:` has been seen, so array-element strings
/// and truncated tails are never mistaken for keys.
fn first_object_top_level_keys(text: &str) -> Vec<String> {
    let Some(start) = text.find('{') else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut expecting_key = false;
    let mut candidate: Option<String> = None;
    for ch in text[start..].chars() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' if in_string => in_string = false,
            '"' => {
                in_string = true;
                if depth == 1 && expecting_key {
                    candidate = Some(String::new());
                }
            }
            c if in_string => {
                if let Some(key) = candidate.as_mut() {
                    key.push(c);
                }
            }
            '{' => {
                depth += 1;
                expecting_key = depth == 1;
            }
            '}' if depth == 1 => break,
            '}' => depth -= 1,
            ':' if depth == 1 => {
                keys.extend(candidate.take());
                expecting_key = false;
            }
            ',' if depth == 1 => expecting_key = true,
            _ => {}
        }
    }
    keys
}

/// Whether the first JSON object's visible top-level keys have the vocabulary
/// of a tool call: at least one [`TOOL_CALL_NAME_KEYS`] key and nothing
/// outside the call vocabulary. Data objects (`{"name": "Alice", "age": 3}`,
/// `{"results": [...]}`) carry other keys and are never call-shaped, however
/// many nested `"name"` fields they contain.
fn first_object_call_shaped(text: &str) -> bool {
    let keys = first_object_top_level_keys(text);
    keys.iter()
        .any(|key| TOOL_CALL_NAME_KEYS.contains(&key.as_str()))
        && keys.iter().all(|key| {
            TOOL_CALL_NAME_KEYS.contains(&key.as_str())
                || TOOL_CALL_INPUT_KEYS.contains(&key.as_str())
                || matches!(key.as_str(), "id" | "type")
        })
}

/// Mechanism A classification: does a held stream prefix look like a tool
/// call being emitted as text? Fence-stripped and trimmed, it must open as
/// JSON and either be call-shaped or quote a registered tool name (dotted or
/// underscored).
fn stream_prefix_suspect(prefix: &str, tool_names: &[String]) -> bool {
    let stripped = strip_code_fences(prefix);
    let trimmed = stripped.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return false;
    }
    let quoted = |name: &str| format!("\"{name}\"");
    tool_names.iter().any(|name| {
        trimmed.contains(&quoted(name)) || trimmed.contains(&quoted(&name.replace('.', "_")))
    }) || first_object_call_shaped(trimmed)
}

/// Mechanism A — early-abort gate for tool calls streamed as text.
///
/// Weaker models sometimes stream an entire tool call as plain text (tens of
/// kilobytes over minutes). The gate holds the first
/// [`STREAM_GATE_HOLD_BYTES`] of round text, classifies the prefix once, and
/// either opens the stream for good (benign — zero further gating) or
/// suppresses the suspect text while re-classifying every
/// [`STREAM_GATE_RECHECK_BYTES`] — aborting the round when the text parses as
/// a call to a registered tool, and at [`STREAM_GATE_ABORT_BYTES`] either
/// cutting off a still-call-shaped suspect (a runaway text call) or releasing
/// anything else in full. Suppressed text still reaches the round's history
/// via the collector; only the live preview is gated.
enum StreamGate {
    /// Accumulating the initial hold; nothing forwarded yet.
    Holding { buf: String },
    /// Prefix classified suspect: suppress and re-check as text accumulates.
    Suppressing { buf: String, next_check: usize },
    /// Classified benign — forward everything for the rest of the round.
    Open,
}

/// What [`StreamGate::on_text`] decided for the delta just absorbed.
enum GateVerdict {
    /// Forward this text to the live preview now (may include released hold).
    Forward(String),
    /// Keep holding — nothing reaches the preview yet.
    Held,
    /// The suppressed text is a tool call (or an unbounded call-shaped
    /// suspect): cut the round off.
    Abort,
}

impl StreamGate {
    fn new(tools: &ToolRegistry) -> Self {
        // A toolless agent cannot call anything — pure JSON output is data.
        if tools.is_empty() {
            Self::Open
        } else {
            Self::Holding { buf: String::new() }
        }
    }

    fn on_text(&mut self, delta: &str, tools: &ToolRegistry) -> GateVerdict {
        match self {
            Self::Open => GateVerdict::Forward(delta.to_string()),
            Self::Holding { buf } => {
                buf.push_str(delta);
                if buf.len() < STREAM_GATE_HOLD_BYTES {
                    return GateVerdict::Held;
                }
                let held = std::mem::take(buf);
                if stream_prefix_suspect(&held, &tools.names()) {
                    *self = Self::Suppressing {
                        next_check: STREAM_GATE_HOLD_BYTES + STREAM_GATE_RECHECK_BYTES,
                        buf: held,
                    };
                    // A single oversized delta may already be past a checkpoint.
                    self.recheck(tools)
                } else {
                    *self = Self::Open;
                    GateVerdict::Forward(held)
                }
            }
            Self::Suppressing { buf, .. } => {
                buf.push_str(delta);
                self.recheck(tools)
            }
        }
    }

    /// Re-classify a suppressed buffer once it crosses the next checkpoint.
    fn recheck(&mut self, tools: &ToolRegistry) -> GateVerdict {
        let Self::Suppressing { buf, next_check } = self else {
            return GateVerdict::Held;
        };
        if buf.len() < *next_check && buf.len() < STREAM_GATE_ABORT_BYTES {
            return GateVerdict::Held;
        }
        match classify_text_tool_call(buf, tools) {
            // A confirmed call — the round-end handler repairs it into a real
            // execution.
            TextCallVerdict::Executable { .. } => GateVerdict::Abort,
            // A runaway text call: cut it off rather than let it stream for
            // minutes, but only once it outgrows the cap.
            TextCallVerdict::Malformed if buf.len() >= STREAM_GATE_ABORT_BYTES => {
                GateVerdict::Abort
            }
            TextCallVerdict::Malformed => {
                *next_check = buf.len().saturating_add(STREAM_GATE_RECHECK_BYTES);
                GateVerdict::Held
            }
            // Data the reader asked for — release it losslessly and stop gating.
            TextCallVerdict::Prose => {
                let released = std::mem::take(buf);
                *self = Self::Open;
                GateVerdict::Forward(released)
            }
        }
    }

    /// A native tool call arrived or the stream ended: hand back any held
    /// text so the hold is lossless — unless it parses as a tool call the
    /// round-end handler is about to repair (its text is superseded by the
    /// execution and never previewed; it still lands in round history via the
    /// collector).
    fn release(&mut self, tools: &ToolRegistry) -> Option<String> {
        match std::mem::replace(self, Self::Open) {
            Self::Holding { buf } | Self::Suppressing { buf, .. } => {
                (!buf.is_empty() && parse_text_tool_call(&buf, tools).is_none()).then_some(buf)
            }
            Self::Open => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::streaming::StreamEvent;
    use crate::llm::types::{ContentBlock, StopReason, TokenUsage};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use workforce_common::session::CompactionConfig;

    fn response_allow() -> workforce_core::policy::PolicyResponse {
        workforce_core::policy::PolicyResponse {
            decision: workforce_core::policy::PolicyDecision::Allow,
            matched_rule: None,
            evaluation_time_us: 0,
        }
    }

    /// Tools run in a spawned task, and task-locals do not cross a spawn —
    /// they survive only because [`SessionContext`] carries them by hand. A
    /// local missing from that list is silently empty inside every tool, which
    /// is how voice-targeted reminders shipped delivering to Slack: the
    /// scheduler tool could not see the channel the turn was spoken on.
    #[tokio::test]
    async fn the_session_context_carries_every_task_local_a_tool_can_read() {
        let captured = CURRENT_VOICE_CHANNEL
            .scope("voice-pe".to_string(), async {
                CURRENT_SESSION_KEY
                    .scope("voice-pe".to_string(), async {
                        CURRENT_USER_ID
                            .scope("uid-1".to_string(), async {
                                // Exactly what the runner does around a tool call.
                                spawn_in_session_context(async {
                                    (
                                        CURRENT_VOICE_CHANNEL.try_with(|c| c.clone()).ok(),
                                        CURRENT_USER_ID.try_with(|u| u.clone()).ok(),
                                    )
                                })
                                .await
                                .expect("the tool task should not panic")
                            })
                            .await
                    })
                    .await
            })
            .await;

        assert_eq!(
            captured.0.as_deref(),
            Some("voice-pe"),
            "a tool must see the channel the turn was spoken on"
        );
        assert_eq!(captured.1.as_deref(), Some("uid-1"));
    }

    /// The other half: a typed turn must not leak a voice target into a tool.
    #[tokio::test]
    async fn a_turn_with_no_voice_channel_carries_none_into_tools() {
        let seen = CURRENT_USER_ID
            .scope("uid-1".to_string(), async {
                spawn_in_session_context(async {
                    CURRENT_VOICE_CHANNEL
                        .try_with(|c| c.clone())
                        .ok()
                        .filter(|c| !c.is_empty())
                })
                .await
                .expect("the tool task should not panic")
            })
            .await;
        assert_eq!(seen, None);
    }

    fn checkin_inv(tool: &str) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool.into(),
            input: serde_json::json!({}),
            output: serde_json::json!({}),
            success: true,
            policy_decision: "allow".into(),
            duration_ms: 1,
            denial_reason: None,
            error: None,
        }
    }

    /// The settled note keeps the timing/round/tools but drops the "Still
    /// working" framing and the Pause invite, with a neutral label that makes no
    /// success claim (settling also runs on errors/steer-stops). Built from the
    /// real `build_checkin_note` output so wording drift there is caught here.
    #[test]
    fn checkin_note_with_activity_appends_the_last_observation_and_survives_settling() {
        let note = "⏳ *Still working* — `coding.session` has been running for 4m 00s. \
                    Tap *Pause* (or reply here) if you want me to stop.";
        assert_eq!(checkin_note_with_activity(note, None), note);
        assert_eq!(checkin_note_with_activity(note, Some("  ")), note);

        let grounded = checkin_note_with_activity(note, Some("_coding ▸_ `pnpm install`"));
        assert!(grounded.ends_with("\nLast seen: _coding ▸_ `pnpm install`"));

        let settled = settle_checkin_note(&grounded);
        assert!(settled.starts_with("⏱️ *Last check-in* —"));
        assert!(settled.contains("Last seen: _coding ▸_ `pnpm install`"));
    }

    #[test]
    fn settle_checkin_note_neutralizes_framing_and_drops_pause_invite() {
        let live = build_checkin_note(
            3,
            std::time::Duration::from_secs(278),
            &[checkin_inv("advisor"), checkin_inv("linear.issue_get")],
        );
        // Sanity: the live note is what we expect to transform.
        assert!(live.starts_with("⏳ *Still working* — 4m 38s in, round 3."));
        assert!(live.contains("Tap *Pause*"));

        let settled = settle_checkin_note(&live);
        assert_eq!(
            settled,
            "⏱️ *Last check-in* — 4m 38s in, round 3.\nSo far: `advisor`, `linear.issue_get`."
        );
        // No ongoing-work framing, no defunct Pause invite, no success claim.
        assert!(!settled.contains("Still working"));
        assert!(!settled.contains("Pause"));
        assert!(!settled.contains("I'll keep going"));
        assert!(!settled.contains("Done"));
    }

    fn response_deny() -> workforce_core::policy::PolicyResponse {
        workforce_core::policy::PolicyResponse {
            decision: workforce_core::policy::PolicyDecision::Deny {
                reason: "policy".into(),
            },
            matched_rule: None,
            evaluation_time_us: 0,
        }
    }

    fn response_approval(allow_save_allow: bool) -> workforce_core::policy::PolicyResponse {
        workforce_core::policy::PolicyResponse {
            decision: workforce_core::policy::PolicyDecision::RequireApproval {
                reason: "policy approval".into(),
                allow_save_allow,
            },
            matched_rule: None,
            evaluation_time_us: 0,
        }
    }

    #[test]
    fn cascade_deny_overrides_policy_allow() {
        let combined = combine_with_cascade(response_allow(), CascadeState::Deny);
        assert!(matches!(
            combined.decision,
            workforce_core::policy::PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn cascade_allow_keeps_policy_allow() {
        let combined = combine_with_cascade(response_allow(), CascadeState::Allow);
        assert_eq!(
            combined.decision,
            workforce_core::policy::PolicyDecision::Allow
        );
    }

    #[test]
    fn cascade_ask_upgrades_policy_allow_to_prompt() {
        let combined = combine_with_cascade(response_allow(), CascadeState::Ask);
        match combined.decision {
            workforce_core::policy::PolicyDecision::RequireApproval {
                allow_save_allow, ..
            } => assert!(allow_save_allow, "Ask preserves Allow-save"),
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn cascade_always_ask_caps_allow_save_allow() {
        let combined = combine_with_cascade(response_allow(), CascadeState::AlwaysAsk);
        match combined.decision {
            workforce_core::policy::PolicyDecision::RequireApproval {
                allow_save_allow, ..
            } => assert!(!allow_save_allow, "AlwaysAsk hides Always-Allow"),
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn policy_deny_is_unchanged_by_any_cascade() {
        for cascade in [
            CascadeState::Allow,
            CascadeState::Ask,
            CascadeState::AlwaysAsk,
            CascadeState::Deny,
        ] {
            let combined = combine_with_cascade(response_deny(), cascade);
            assert!(
                matches!(
                    combined.decision,
                    workforce_core::policy::PolicyDecision::Deny { .. }
                ),
                "policy Deny + cascade {cascade:?} stayed Deny",
            );
        }
    }

    #[test]
    fn cascade_always_ask_tightens_policy_require_approval() {
        // PolicyEngine already prompts with allow_save_allow=true, but cascade
        // says AlwaysAsk → the resulting prompt must hide Always-Allow.
        let combined = combine_with_cascade(response_approval(true), CascadeState::AlwaysAsk);
        match combined.decision {
            workforce_core::policy::PolicyDecision::RequireApproval {
                allow_save_allow, ..
            } => assert!(!allow_save_allow),
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    struct FixedGate(CascadeState);
    impl PermissionGate for FixedGate {
        fn evaluate(&self, _u: &str, _a: &str, _t: &str) -> CascadeState {
            self.0
        }
    }

    fn config_with_gate(gate: Option<Arc<dyn PermissionGate>>) -> RunnerConfig {
        RunnerConfig {
            permission_gate: gate,
            ..RunnerConfig::default()
        }
    }

    #[tokio::test]
    async fn apply_cascade_gate_unwired_is_a_noop() {
        // No permission_gate set — response passes through verbatim
        // regardless of the user-id task-local.
        let config = config_with_gate(None);
        let out = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                apply_cascade_gate(&ToolGates::from(&config), response_allow(), "vault.read")
            })
            .await;
        assert_eq!(out.decision, workforce_core::policy::PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn apply_cascade_gate_without_user_is_a_noop() {
        // Gate wired but no CURRENT_USER_ID in scope (system jobs / CLI):
        // even a stubbed Deny gate must not affect the response.
        let config = config_with_gate(Some(Arc::new(FixedGate(CascadeState::Deny))));
        let out = apply_cascade_gate(&ToolGates::from(&config), response_allow(), "vault.read");
        assert_eq!(out.decision, workforce_core::policy::PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn apply_cascade_gate_with_user_tightens_decision() {
        let config = config_with_gate(Some(Arc::new(FixedGate(CascadeState::Deny))));
        let out = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                apply_cascade_gate(&ToolGates::from(&config), response_allow(), "vault.read")
            })
            .await;
        assert!(matches!(
            out.decision,
            workforce_core::policy::PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn cascade_deny_upgrades_policy_require_approval_to_deny() {
        let combined = combine_with_cascade(response_approval(true), CascadeState::Deny);
        assert!(matches!(
            combined.decision,
            workforce_core::policy::PolicyDecision::Deny { .. }
        ));
    }

    /// `tool_detail` summarises by input shape, not tool name. These use the
    /// real dotted internal names `from_llm_name` produces (`jira.api`, not the
    /// `mcp__..__jira_api` MCP-registry form) and realistic adapter inputs.
    #[test]
    fn tool_detail_composes_http_method_and_path() {
        // Regression: the lone `method` ("GET") used to sort first and be all
        // the status line showed for `*.api` tools.
        let d = tool_detail(
            "jira.api",
            &serde_json::json!({"method": "GET", "path": "/rest/api/3/issue/PROJ-1"}),
        );
        assert_eq!(d, "GET /rest/api/3/issue/PROJ-1");
    }

    #[test]
    fn tool_detail_prefers_descriptive_keys_over_first_sorted() {
        // `jira.get_issue` { issue_key } — surface the key, not nothing.
        assert_eq!(
            tool_detail(
                "jira.get_issue",
                &serde_json::json!({"issue_key": "PROJ-1"})
            ),
            "PROJ-1"
        );
        // `metabase.search` { query } wins over an alphabetically-earlier field.
        assert_eq!(
            tool_detail(
                "metabase.search",
                &serde_json::json!({"archived": false, "query": "revenue"})
            ),
            "revenue"
        );
    }

    #[test]
    fn tool_detail_filesystem_and_exec_by_shape() {
        assert_eq!(
            tool_detail("filesystem.read", &serde_json::json!({"path": "/a/b.rs"})),
            "/a/b.rs"
        );
        assert_eq!(
            tool_detail("exec.run", &serde_json::json!({"command": "cargo build"})),
            "cargo build"
        );
    }

    #[test]
    fn tool_detail_numeric_identifier_keeps_key() {
        // `*.list` tools no longer mis-render "."; numeric ids carry their key.
        assert_eq!(
            tool_detail("metabase.card_query", &serde_json::json!({"card_id": 5})),
            "card_id=5"
        );
        assert_eq!(
            tool_detail("jira.list_projects", &serde_json::json!({})),
            ""
        );
    }

    #[test]
    fn approval_detail_prefers_tool_summary_then_falls_back_to_shape() {
        use crate::tools::{Tool, ToolResult};
        use async_trait::async_trait;

        struct Scoped;
        #[async_trait]
        impl Tool for Scoped {
            fn name(&self) -> &str {
                "demo.scoped"
            }
            fn description(&self) -> &str {
                ""
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _i: serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::success(serde_json::Value::Null, 0))
            }
            fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
                input
                    .get("n")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| format!("affects {n} items"))
            }
        }

        let registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(Scoped));

        // The tool's own scope summary wins when it offers one.
        assert_eq!(
            approval_detail(&registry, "demo.scoped", &serde_json::json!({"n": 5})),
            "affects 5 items"
        );
        // `approval_summary` → None falls back to the generic shape summary.
        assert_eq!(
            approval_detail(&registry, "demo.scoped", &serde_json::json!({"path": "/x"})),
            "/x"
        );
        // An unregistered tool also falls back to the shape summary.
        assert_eq!(
            approval_detail(
                &registry,
                "demo.unknown",
                &serde_json::json!({"command": "ls"})
            ),
            "ls"
        );
    }

    /// Regression for the `bedrock:zai` (Candy) misconfig where
    /// `max_tokens = 65536` exceeded GLM's 40k output cap.
    #[test]
    fn effective_max_tokens_clamps_to_model_output_ceiling() {
        let mut config = RunnerConfig {
            max_tokens: Some(65_536),
            ..Default::default()
        };
        assert_eq!(
            config.effective_max_tokens("zai.glm-5"),
            Some(40_000),
            "max_tokens above GLM's 40k output cap must clamp down"
        );
        assert_eq!(
            config.effective_max_tokens("some-unknown-model"),
            Some(65_536),
            "unknown model must pass through unchanged"
        );
        config.max_tokens = Some(8_192);
        assert_eq!(
            config.effective_max_tokens("zai.glm-5"),
            Some(8_192),
            "a value at or below the cap is left untouched"
        );
    }

    /// Pins the contract that motivated `spawn_in_session_context`. A bare
    /// `tokio::spawn` drops every task-local — passive memory extraction
    /// silently no-ops because `CURRENT_USER_ID` reads `None` inside the
    /// new task, so the `extract_and_store` gate skips the write.
    #[tokio::test]
    async fn bare_tokio_spawn_loses_current_user_id() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c = Arc::clone(&captured);
        CURRENT_USER_ID
            .scope("test-uid".to_string(), async move {
                tokio::spawn(async move {
                    *c.lock().unwrap() = CURRENT_USER_ID.try_with(|u| u.clone()).ok();
                })
                .await
                .unwrap();
            })
            .await;
        assert_eq!(*captured.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn spawn_in_session_context_propagates_current_user_id() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c = Arc::clone(&captured);
        CURRENT_USER_ID
            .scope("test-uid".to_string(), async move {
                spawn_in_session_context(async move {
                    *c.lock().unwrap() = CURRENT_USER_ID.try_with(|u| u.clone()).ok();
                })
                .await
                .unwrap();
            })
            .await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some("test-uid"));
    }

    #[tokio::test]
    async fn spawn_in_session_context_propagates_full_context() {
        let captured = Arc::new(std::sync::Mutex::new((None, None, None)));
        let c = Arc::clone(&captured);
        CURRENT_USER_ID
            .scope(
                "uid".to_string(),
                CURRENT_SESSION_ID.scope(
                    "sid".to_string(),
                    CURRENT_AGENT_ID.scope("agent".to_string(), async move {
                        spawn_in_session_context(async move {
                            *c.lock().unwrap() = (
                                CURRENT_USER_ID.try_with(|u| u.clone()).ok(),
                                CURRENT_SESSION_ID.try_with(|s| s.clone()).ok(),
                                CURRENT_AGENT_ID.try_with(|a| a.clone()).ok(),
                            );
                        })
                        .await
                        .unwrap();
                    }),
                ),
            )
            .await;
        let (u, s, a) = captured.lock().unwrap().clone();
        assert_eq!(u.as_deref(), Some("uid"));
        assert_eq!(s.as_deref(), Some("sid"));
        assert_eq!(a.as_deref(), Some("agent"));
    }

    #[tokio::test]
    async fn session_scope_workspace_id_visible_inside_scope() {
        let ctx = SessionContext {
            workspace_id: Some("candy".into()),
            ..SessionContext::default()
        };
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c = Arc::clone(&captured);
        ctx.scope(async move {
            *c.lock().unwrap() = CURRENT_WORKSPACE_ID.try_with(|w| w.clone()).ok();
        })
        .await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some("candy"));
    }

    #[tokio::test]
    async fn session_scope_empty_workspace_id_yields_empty_sentinel() {
        let ctx = SessionContext::default();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c = Arc::clone(&captured);
        ctx.scope(async move {
            *c.lock().unwrap() = CURRENT_WORKSPACE_ID.try_with(|w| w.clone()).ok();
        })
        .await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some(""));
    }

    fn served_by(payer: &str) -> Served {
        Served {
            route: workforce_common::subscription::SubscriptionRoute::ChatGpt,
            tier: workforce_common::subscription::CredentialTier::AgentUser,
            payer: payer.to_string(),
        }
    }

    #[tokio::test]
    async fn session_context_carries_billing_identity() {
        let captured = CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                CURRENT_BILLING_IDENTITY.scope("system:indexing".to_string(), async {
                    SessionContext::capture()
                }),
            )
            .await;
        assert_eq!(
            captured.billing_identity.as_deref(),
            Some("system:indexing")
        );

        let seen = captured
            .clone()
            .spawn(async { CURRENT_BILLING_IDENTITY.try_with(String::clone).ok() })
            .await
            .unwrap();
        assert_eq!(seen.as_deref(), Some("system:indexing"));

        let seen_blocking = tokio::task::spawn_blocking(move || {
            captured.sync_scope(|| CURRENT_BILLING_IDENTITY.try_with(String::clone).ok())
        })
        .await
        .unwrap();
        assert_eq!(seen_blocking.as_deref(), Some("system:indexing"));

        let unset = CURRENT_USER_ID
            .scope("alice".to_string(), async { SessionContext::capture() })
            .await;
        assert_eq!(
            unset.billing_identity, None,
            "no explicit billing identity: spend falls back to the scoped user"
        );
    }

    #[tokio::test]
    async fn with_served_log_isolates_a_call_from_its_turn() {
        let turn = ServedLog::default();
        let (_, inner) = turn
            .clone()
            .scope(async {
                note_served(served_by("turn"));
                with_served_log(async { note_served(served_by("helper")) }).await
            })
            .await;
        assert_eq!(inner, Some(served_by("helper")));
        assert_eq!(turn.served(), Some(served_by("turn")));
        note_served(served_by("nobody"));
    }

    /// Round one is served on a subscription (it notes the call) and asks for
    /// a tool; round two comes back from a metered provider, as after a
    /// mid-turn failover, and notes nothing.
    #[derive(Default)]
    struct FailsOverToMeteredProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for FailsOverToMeteredProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let content = if first {
                note_served(served_by("alice"));
                vec![ContentBlock::tool_use(
                    "tu_1".to_string(),
                    "noop".to_string(),
                    serde_json::json!({}),
                )]
            } else {
                vec![ContentBlock::Text {
                    text: "done".to_string(),
                }]
            };
            Ok(LlmResponse {
                stop_reason: if first {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                content,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn metered_failover_after_chatgpt_records_metered_row() {
        struct NoopTool;

        #[async_trait]
        impl Tool for NoopTool {
            fn name(&self) -> &str {
                "noop"
            }
            fn description(&self) -> &str {
                "does nothing"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }

        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(NoopTool));
        let runner = make_runner_with_tools(Arc::new(FailsOverToMeteredProvider::default()), tools);
        let turn_log = ServedLog::default();

        let result = turn_log.clone().scope(runner.run("go")).await.unwrap();

        assert_eq!(result.response, "done");
        assert_eq!(
            turn_log.served(),
            None,
            "the metered call that answered decides the turn's billing"
        );
    }

    #[tokio::test]
    async fn session_scope_skips_session_keys_when_none() {
        let ctx = SessionContext::default();
        let captured = Arc::new(std::sync::Mutex::new((None, None, None)));
        let c = Arc::clone(&captured);
        ctx.scope(async move {
            *c.lock().unwrap() = (
                CURRENT_SESSION_KEY.try_with(|s| s.clone()).ok(),
                CURRENT_SESSION_ID.try_with(|s| s.clone()).ok(),
                CURRENT_USER_ID.try_with(|s| s.clone()).ok(),
            );
        })
        .await;
        let (k, i, u) = captured.lock().unwrap().clone();
        assert!(k.is_none() && i.is_none() && u.is_none());
    }

    #[tokio::test]
    async fn session_scope_round_trip_preserves_all_fields() {
        let original = SessionContext {
            key: Some("sk".into()),
            id: Some("sid".into()),
            user_id: Some("uid".into()),
            billing_identity: Some("system:indexing".into()),
            platform_user_id: Some("Uplat".into()),
            channel_id: Some("C1".into()),
            thread_id: Some("123.456".into()),
            agent_id: Some("aid".into()),
            workspace_id: Some("candy".into()),
            // The scheduler and reminder tools read this inside the spawned
            // tool task to stamp a job's delivery target; dropping it here is
            // what made voice-requested reminders arrive in Slack.
            voice_channel: Some("voice-pe".into()),
            automated_origin: Some("heartbeat".into()),
            // The MCP-workspace override must survive capture→scope→capture too:
            // tools run inside the JoinSet::spawn this propagation feeds, so if
            // it's dropped here the gate sees no `cmd: use` activation. The same
            // holds for the environment lock — the skill `resolve_instance` guard
            // runs in that spawned task.
            active_mcp_envs: [("harbor".to_string(), "staging".to_string())].into(),
            active_environment: Some("harbor-staging".into()),
            active_skill_instances: [("metabase".to_string(), vec!["staging".to_string()])].into(),
            // A staged-table in a spawned tool task must inherit the turn's TTL,
            // else a `data.query`-backed table would age out on the scratch default.
            artifact_ttl: Some(std::time::Duration::from_secs(86_400)),
            artifact_store: None,
            // A workflow step's scope must survive the spawn too, or the table it
            // stages is stamped for its own session and the next step can't read it.
            artifact_scope: Some("workflow:wf-1".into()),
            last_user_comm: None,
            // The secret sinks (`browser.fill`, `http.request`) resolve
            // `[[vault:<scope>:<key>]]` inside the spawned tool task. If the
            // policy is dropped here they see "not a workflow run" and hand the
            // marker to the legacy resolver, which rejects the scoped key.
            secret_policy: Some(workforce_common::secret_policy::SecretPolicy {
                per_user_identity: Some("owner".into()),
                shared_secret_prefix: None,
                declared: [(
                    ("agent".to_string(), "smoke/customer_a_email".to_string()),
                    workforce_common::secret_policy::DeclaredSecret {
                        allowed_hosts: vec!["auth.example.test".into()],
                    },
                )]
                .into(),
            }),
            served: Some(ServedLog::default()),
            // A reminder or a file made in the spawned tool task goes to the
            // Desk conversation the turn answers to.
            desk: Some(workforce_common::channels::desk::DeskScope {
                target: workforce_common::channels::desk::DeskTarget {
                    place: workforce_common::channels::desk::DeskPlace::Dm,
                    channel: "desk".into(),
                    device: None,
                    spoken: false,
                },
                attended: true,
            }),
        };
        let captured = Arc::new(std::sync::Mutex::new(SessionContext::default()));
        let c = Arc::clone(&captured);
        original
            .clone()
            .scope(async move {
                *c.lock().unwrap() = SessionContext::capture();
            })
            .await;
        let after = captured.lock().unwrap().clone();
        assert_eq!(after.artifact_scope, original.artifact_scope);
        assert_eq!(after.key, original.key);
        assert_eq!(after.id, original.id);
        assert_eq!(after.user_id, original.user_id);
        assert_eq!(after.channel_id, original.channel_id);
        assert_eq!(after.thread_id, original.thread_id);
        assert_eq!(after.agent_id, original.agent_id);
        assert_eq!(after.workspace_id, original.workspace_id);
        assert_eq!(after.active_mcp_envs, original.active_mcp_envs);
        assert_eq!(after.active_environment, original.active_environment);
        assert_eq!(
            after.active_skill_instances,
            original.active_skill_instances
        );
        assert_eq!(after.artifact_ttl, original.artifact_ttl);
        assert_eq!(after.voice_channel, original.voice_channel);
        assert_eq!(after.automated_origin, original.automated_origin);
        assert_eq!(after.billing_identity, original.billing_identity);
        assert_eq!(after.desk, original.desk);
        after
            .served
            .expect("served log survives the spawn")
            .note(served_by("alice"));
        assert_eq!(
            original.served.as_ref().and_then(ServedLog::served),
            Some(served_by("alice")),
            "the captured log is the same log, not a copy"
        );
        let (after_policy, original_policy) = (
            after.secret_policy.expect("policy survives the spawn"),
            original.secret_policy.expect("policy set on the original"),
        );
        assert_eq!(
            after_policy.per_user_identity,
            original_policy.per_user_identity
        );
        assert_eq!(
            after_policy.shared_secret_prefix,
            original_policy.shared_secret_prefix
        );
        let key = ("agent".to_string(), "smoke/customer_a_email".to_string());
        assert_eq!(
            after_policy.declared[&key].allowed_hosts,
            original_policy.declared[&key].allowed_hosts
        );
    }

    #[tokio::test]
    async fn scope_installs_the_secret_policy_for_the_spawned_tool_task() {
        // The discriminating check for the marker-resolution bug: inside the
        // re-applied scope a sink must see a policy, so `resolve_secret_markers`
        // takes the scoped path rather than falling through to the legacy one.
        use workforce_common::secret_policy::{
            in_secret_policy_scope, SecretPolicy, SECRET_POLICY,
        };
        let seen = SECRET_POLICY
            .scope(SecretPolicy::default(), async {
                SessionContext::capture()
                    .scope(async { in_secret_policy_scope() })
                    .await
            })
            .await;
        assert!(seen, "policy must survive capture → scope");
        let bare = SessionContext::capture()
            .scope(async { in_secret_policy_scope() })
            .await;
        assert!(!bare, "no policy outside a workflow run");
    }

    #[test]
    fn sync_scope_carries_the_secret_policy_across_the_blocking_boundary() {
        use workforce_common::secret_policy::{
            in_secret_policy_scope, SecretPolicy, SECRET_POLICY,
        };
        let restored = SECRET_POLICY.sync_scope(SecretPolicy::default(), || {
            SessionContext::capture().sync_scope(in_secret_policy_scope)
        });
        assert!(restored);
        let bare = SessionContext::capture().sync_scope(in_secret_policy_scope);
        assert!(!bare);
    }

    #[test]
    fn sync_scope_carries_artifact_ttl_across_the_blocking_boundary() {
        // The `data.query` path re-applies the captured TTL through
        // `SessionContext::sync_scope` (across `spawn_blocking`), so cover that
        // branch: a capture under a scoped TTL restores it; a bare one stays unset.
        let ttl = std::time::Duration::from_secs(86_400);
        let restored = CURRENT_ARTIFACT_TTL.sync_scope(ttl, || {
            SessionContext::capture().sync_scope(|| CURRENT_ARTIFACT_TTL.try_with(|t| *t).ok())
        });
        assert_eq!(restored, Some(ttl));
        let bare =
            SessionContext::capture().sync_scope(|| CURRENT_ARTIFACT_TTL.try_with(|t| *t).ok());
        assert_eq!(bare, None);
    }

    #[tokio::test]
    async fn spawn_in_session_context_no_panic_outside_runner_scope() {
        // Tests / hub startup paths call this without any task-locals set.
        // Capture is empty; the inner future runs without identity gates.
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let o = Arc::clone(&ok);
        spawn_in_session_context(async move {
            o.store(
                CURRENT_USER_ID.try_with(|_| ()).is_err(),
                std::sync::atomic::Ordering::SeqCst,
            );
        })
        .await
        .unwrap();
        assert!(ok.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A mock LLM provider that simulates streaming responses token-by-token.
    struct MockStreamingProvider {
        /// The text chunks to stream.
        chunks: Vec<String>,
    }

    impl MockStreamingProvider {
        fn new(chunks: Vec<&str>) -> Self {
            Self {
                chunks: chunks.into_iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockStreamingProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let full_text: String = self.chunks.iter().cloned().collect();
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text: full_text }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Default::default()
                },
                model: "mock".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let _ = tx.send(StreamEvent::ContentStart { index: 0 }).await;
            for chunk in &self.chunks {
                let _ = tx
                    .send(StreamEvent::TextDelta {
                        index: 0,
                        text: chunk.clone(),
                    })
                    .await;
            }
            let _ = tx.send(StreamEvent::ContentStop { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 20,
                        ..Default::default()
                    },
                })
                .await;
            Ok(())
        }
    }

    /// Streams one scripted response text per LLM call — unlike
    /// [`MockStreamingProvider`] (same chunks every call) it lets a test drive
    /// multi-round paths such as the hallucinated-tool-call retry.
    struct ScriptedStreamingProvider {
        rounds: std::sync::Mutex<std::collections::VecDeque<String>>,
        prompts: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl ScriptedStreamingProvider {
        fn new(rounds: Vec<&str>) -> Self {
            Self {
                rounds: std::sync::Mutex::new(rounds.into_iter().map(str::to_string).collect()),
                prompts: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn next_text(&self, messages: &[LlmMessage]) -> String {
            self.prompts.lock().unwrap().push(messages.to_vec());
            self.rounds
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted rounds exhausted")
        }

        /// The conversation as it was handed to the given round, 0-indexed.
        fn prompt(&self, round: usize) -> Vec<LlmMessage> {
            self.prompts.lock().unwrap()[round].clone()
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedStreamingProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.next_text(messages),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Default::default()
                },
                model: "mock".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let text = self.next_text(messages);
            let _ = tx.send(StreamEvent::ContentStart { index: 0 }).await;
            // Stream in small chunks so gated-stream tests exercise real
            // delta-by-delta accumulation rather than one giant delta.
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(64) {
                let _ = tx
                    .send(StreamEvent::TextDelta {
                        index: 0,
                        text: chunk.iter().collect(),
                    })
                    .await;
            }
            let _ = tx.send(StreamEvent::ContentStop { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 20,
                        ..Default::default()
                    },
                })
                .await;
            Ok(())
        }
    }

    /// A mock LLM that returns text on the first call and triggers no tool use.
    struct MockSimpleProvider {
        response_text: String,
    }

    impl MockSimpleProvider {
        fn new(text: &str) -> Self {
            Self {
                response_text: text.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockSimpleProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.response_text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 15,
                    ..Default::default()
                },
                model: "mock-simple".to_string(),
            })
        }
        // Uses the default (non-streaming fallback) for create_message_stream
    }

    fn make_runner_with_tools_and_policy(
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        policy_yaml: &str,
    ) -> AgentRunner {
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            system_prompt: "You are a test agent.".to_string(),
            model: Some("mock".to_string()),
            max_tokens: Some(1024),
            max_rounds: 5,
            team_id: None,
            role: None,
            compaction: CompactionConfig::default(),
            spill: workforce_common::session::SpillConfig::default(),
            routing: crate::model_router::RoutingConfig::default(),
            tier_registry: crate::llm::tiers::TierRegistry::new(None),
            model_max_tokens: std::collections::HashMap::new(),
            context_eviction_interval: None,
            context_eviction_keep_last: 3,
            trace_writer: None,
            trace_tags: Default::default(),
            permission_writer: None,
            permission_gate: None,
            arguments: crate::tools::arguments::ArgumentValidation::default(),
            tool_loading: crate::tools::loading::ToolLoading::default(),
            programs: None,
            capability_gate: None,
            approval_router: None,
            preexecute: None,
            journal: None,
            collect: Vec::new(),
            seeds: None,
            interactive: false,
            self_serve: crate::tools::approval::SelfServe::Answerable,
            compile_gate: None,
        };
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(policy_yaml).expect("policy parse"),
        );
        AgentRunner::new(config, llm, tools, policy)
    }

    fn make_runner_with_tools(llm: Arc<dyn LlmProvider>, tools: Arc<ToolRegistry>) -> AgentRunner {
        make_runner_with_tools_and_policy(
            llm,
            tools,
            "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
        )
    }

    fn make_runner(llm: Arc<dyn LlmProvider>) -> AgentRunner {
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            system_prompt: "You are a test agent.".to_string(),
            model: Some("mock".to_string()),
            max_tokens: Some(1024),
            max_rounds: 5,
            team_id: None,
            role: None,
            compaction: CompactionConfig::default(),
            spill: workforce_common::session::SpillConfig::default(),
            routing: crate::model_router::RoutingConfig::default(),
            tier_registry: crate::llm::tiers::TierRegistry::new(None),
            model_max_tokens: std::collections::HashMap::new(),
            context_eviction_interval: None,
            context_eviction_keep_last: 3,
            trace_writer: None,
            trace_tags: Default::default(),
            permission_writer: None,
            permission_gate: None,
            arguments: crate::tools::arguments::ArgumentValidation::default(),
            tool_loading: crate::tools::loading::ToolLoading::default(),
            programs: None,
            capability_gate: None,
            approval_router: None,
            preexecute: None,
            journal: None,
            collect: Vec::new(),
            seeds: None,
            interactive: false,
            self_serve: crate::tools::approval::SelfServe::Answerable,
            compile_gate: None,
        };
        let tools = Arc::new(ToolRegistry::new());
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
            )
            .expect("policy parse"),
        );
        AgentRunner::new(config, llm, tools, policy)
    }

    /// Callers wrap a run in several task-local scopes and spawn it; an
    /// unboxed run loop is copied into every one of those frames and
    /// overflows a 2 MiB worker stack.
    #[test]
    fn run_entry_futures_keep_the_loop_on_the_heap() {
        const BOUND: usize = 4 * 1024;
        let runner = make_runner(Arc::new(MockSimpleProvider::new("ok")));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let streamed = runner.run_with_progress_and_history(Vec::new(), Vec::new(), tx, None, None);
        let blocking = runner.run_with_history_content(Vec::new(), Vec::new());
        assert!(std::mem::size_of_val(&streamed) < BOUND);
        assert!(std::mem::size_of_val(&blocking) < BOUND);
    }

    enum StubDecision {
        DenyUnreachable,
        Routed(Vec<String>, bool),
    }

    struct StubRouter {
        decision: StubDecision,
        deferred: Arc<std::sync::Mutex<Vec<PendingApprovalCore>>>,
    }

    impl ApprovalRouter for StubRouter {
        fn route(&self, _ctx: &ApprovalRouteCtx<'_>) -> ApprovalDecision {
            match &self.decision {
                StubDecision::DenyUnreachable => ApprovalDecision::DenyUnreachable,
                StubDecision::Routed(ids, self_ok) => ApprovalDecision::Routed(ApprovalRoute {
                    approver_user_ids: ids.clone(),
                    allow_self_approval: *self_ok,
                    ttl: std::time::Duration::from_secs(3600),
                }),
            }
        }
        fn defer(&self, record: &PendingApprovalCore) -> eyre::Result<()> {
            self.deferred.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    fn runner_with_router(router: Arc<dyn ApprovalRouter>) -> AgentRunner {
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            approval_router: Some(router),
            ..Default::default()
        };
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
            )
            .expect("policy parse"),
        );
        AgentRunner::new(
            config,
            Arc::new(MockSimpleProvider::new("ok")),
            Arc::new(ToolRegistry::new()),
            policy,
        )
    }

    fn route_for(
        runner: &AgentRunner,
        tool: &str,
        reason: &str,
        input: &serde_json::Value,
    ) -> RouteResolution {
        route_approval(
            runner.config.approval_router.as_deref(),
            runner.config.agent_id.as_str(),
            tool,
            reason,
            input,
        )
    }

    async fn route_in_scope(runner: &AgentRunner) -> RouteResolution {
        CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                CURRENT_SESSION_KEY.scope("slack:ws:C1:T1".to_string(), async {
                    route_for(
                        runner,
                        "vault.delete",
                        "sensitive",
                        &serde_json::json!({"id": 1}),
                    )
                }),
            )
            .await
    }

    #[tokio::test]
    async fn unrouted_approval_is_self_serve() {
        let runner = make_runner(Arc::new(MockSimpleProvider::new("ok")));
        assert!(matches!(
            route_in_scope(&runner).await,
            RouteResolution::SelfServe
        ));
    }

    #[tokio::test]
    async fn routed_approval_defers_with_exact_record() {
        let deferred = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = runner_with_router(Arc::new(StubRouter {
            decision: StubDecision::Routed(vec!["bob".to_string()], false),
            deferred: Arc::clone(&deferred),
        }));
        match route_in_scope(&runner).await {
            RouteResolution::Routed {
                approver_user_ids, ..
            } => assert_eq!(approver_user_ids, vec!["bob".to_string()]),
            other => panic!("expected Routed, got {other:?}"),
        }
        let rows = deferred.lock().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one durable row persisted at deferral"
        );
        assert_eq!(rows[0].tool_name, "vault.delete");
        assert_eq!(rows[0].requester_user_id, "alice");
        assert_eq!(rows[0].session_key, "slack:ws:C1:T1");
        assert_eq!(rows[0].approver_user_ids, vec!["bob".to_string()]);
        assert!(!rows[0].allow_self_approval);
        assert_eq!(rows[0].tool_input, serde_json::json!({"id": 1}));
    }

    #[tokio::test]
    async fn routed_without_session_key_falls_back_to_self_serve() {
        let runner = runner_with_router(Arc::new(StubRouter {
            decision: StubDecision::Routed(vec!["bob".to_string()], false),
            deferred: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                route_for(&runner, "vault.delete", "r", &serde_json::json!({}))
            })
            .await;
        assert!(matches!(res, RouteResolution::SelfServe));
    }

    #[tokio::test]
    async fn deny_unreachable_passes_through() {
        let runner = runner_with_router(Arc::new(StubRouter {
            decision: StubDecision::DenyUnreachable,
            deferred: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        assert!(matches!(
            route_in_scope(&runner).await,
            RouteResolution::DenyUnreachable
        ));
    }

    #[tokio::test]
    async fn routed_defer_emits_event_persists_and_terminates() {
        let deferred = Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            model: Some("mock".to_string()),
            max_rounds: 5,
            approval_router: Some(Arc::new(StubRouter {
                decision: StubDecision::Routed(vec!["bob".to_string()], false),
                deferred: Arc::clone(&deferred),
            })),
            ..Default::default()
        };
        // `ask` forces RequireApproval so the streaming routing arm is reached.
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: ask-all\n    scope: global\n    tool: \"*\"\n    action: ask\n",
            )
            .expect("policy parse"),
        );
        let runner = AgentRunner::new(
            config,
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
            policy,
        );
        let (tx, mut rx) = mpsc::channel(64);
        let result = CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                CURRENT_SESSION_KEY.scope(
                    "slack:ws:C1:T1".to_string(),
                    runner.run_with_progress("delete it", tx),
                ),
            )
            .await
            .expect("routed approval terminates the turn");
        assert!(result.rounds >= 1);

        let prompts: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                ProgressEvent::ApprovalRequired { routing, .. } => Some(routing),
                _ => None,
            })
            .collect();
        assert_eq!(prompts.len(), 1, "exactly one approval prompt emitted");
        let routing = prompts[0].clone().expect("routed approval carries routing");
        assert_eq!(routing.approver_user_ids, vec!["bob".to_string()]);
        assert_eq!(routing.requester_user_id, "alice");

        let rows = deferred.lock().unwrap();
        assert_eq!(rows.len(), 1, "defer persisted exactly once");
        assert_eq!(rows[0].approver_user_ids, vec!["bob".to_string()]);
    }

    fn ask_all_policy() -> Arc<workforce_policy::engine::PolicyEngine> {
        Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: ask-all\n    scope: global\n    tool: \"*\"\n    action: ask\n",
            )
            .expect("policy parse"),
        )
    }

    fn allow_all_policy() -> Arc<workforce_policy::engine::PolicyEngine> {
        Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
            )
            .expect("policy parse"),
        )
    }

    /// A skill tool that holds itself behind a human decision and renders its
    /// own scope sentence, as a manifest with `requires_approval` produces.
    struct GatedSkillTool(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl crate::tools::Tool for GatedSkillTool {
        fn name(&self) -> &str {
            "record-keeper.apply_change"
        }
        fn description(&self) -> &str {
            "apply a change"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn approval_requirement(&self) -> Option<String> {
            Some("Required by the skill's own approval gate".to_string())
        }
        fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
            Some(format!(
                "apply `{}` to {} recipient(s) in `{}`",
                input["action"].as_str()?,
                input["recipients"].as_array()?.len(),
                input["environment"].as_str()?
            ))
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::tools::ToolResult::success(
                serde_json::Value::Null,
                0,
            ))
        }
    }

    const GATED_CALL: &str =
        r#"{"action":"reset","recipients":["a@x.com","b@x.com"],"environment":"production"}"#;

    /// Emits the same gated call twice in one round, then wraps up.
    struct TwiceGatedProvider(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl LlmProvider for TwiceGatedProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let input: serde_json::Value = serde_json::from_str(GATED_CALL).unwrap();
            Ok(LlmResponse {
                content: if first {
                    vec![
                        ContentBlock::tool_use(
                            "tu_1".to_string(),
                            "record-keeper.apply_change".to_string(),
                            input.clone(),
                        ),
                        ContentBlock::tool_use(
                            "tu_2".to_string(),
                            "record-keeper.apply_change".to_string(),
                            input,
                        ),
                    ]
                } else {
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }]
                },
                stop_reason: if first {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    /// Policy allows the call outright; the gate comes from the skill alone,
    /// and two identical calls are two decisions — collapsing them would let
    /// one Approve authorise an action nobody read.
    #[tokio::test]
    async fn a_skill_gate_routes_every_call_and_executes_none() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = ToolRegistry::new();
        tools.register(Arc::new(GatedSkillTool(Arc::clone(&ran))));
        let (runner, deferred) = routed_runner(
            Arc::new(TwiceGatedProvider(std::sync::atomic::AtomicUsize::new(0))),
            Arc::new(tools),
            StubDecision::Routed(vec!["bob".to_string()], false),
            allow_all_policy(),
        );

        let result = run_non_progress_in_scope(&runner, "apply it").await;
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a gated tool must not execute before a human approves"
        );
        assert_eq!(result.tool_invocations.len(), 2);
        for inv in &result.tool_invocations {
            assert_eq!(inv.policy_decision, "require_approval");
        }

        let rows = deferred.lock().unwrap();
        assert_eq!(rows.len(), 2, "two payloads are two decisions");
        for row in rows.iter() {
            assert_eq!(row.tool_name, "record-keeper.apply_change");
            assert_eq!(row.reason, "Required by the skill's own approval gate");
            assert_eq!(
                row.tool_input,
                serde_json::from_str::<serde_json::Value>(GATED_CALL).unwrap(),
                "the pinned payload is the record form the model emitted"
            );
        }
        assert_ne!(rows[0].request_id, rows[1].request_id);
    }

    /// The deficiency the manifest template fixes: without it the generic
    /// shape summary names one field and says nothing about the action or
    /// the size of the batch.
    #[test]
    fn a_skill_summary_replaces_a_shape_summary_that_says_nothing() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry.register(Arc::new(GatedSkillTool(ran)));
        let input: serde_json::Value = serde_json::from_str(GATED_CALL).unwrap();
        assert_eq!(
            approval_detail(&registry, "record-keeper.apply_change", &input),
            "apply `reset` to 2 recipient(s) in `production`"
        );
        assert_eq!(
            approval_detail(&registry, "unregistered.apply_change", &input),
            "reset",
            "the shape summary shows one field and neither the size of the \
             batch nor where it lands"
        );
    }

    fn routed_runner(
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        decision: StubDecision,
        policy: Arc<workforce_policy::engine::PolicyEngine>,
    ) -> (AgentRunner, Arc<std::sync::Mutex<Vec<PendingApprovalCore>>>) {
        let deferred = Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            model: Some("mock".to_string()),
            max_rounds: 5,
            approval_router: Some(Arc::new(StubRouter {
                decision,
                deferred: Arc::clone(&deferred),
            })),
            ..Default::default()
        };
        (AgentRunner::new(config, llm, tools, policy), deferred)
    }

    async fn run_non_progress_in_scope(runner: &AgentRunner, task: &str) -> RunResult {
        CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                CURRENT_SESSION_KEY.scope("slack:ws:C1:T1".to_string(), runner.run(task)),
            )
            .await
            .expect("run completes")
    }

    /// Non-progress routed approval: the invocation record carries the
    /// request id / approvers / detail (the hub's delivery coordinates), the
    /// run returns after exactly one wrap-up round with the model's text, and
    /// the accumulated telemetry is carried out — not empty vectors.
    #[tokio::test]
    async fn non_progress_routed_approval_records_coordinates_and_wraps_up() {
        let (runner, deferred) = routed_runner(
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
            StubDecision::Routed(vec!["bob".to_string()], false),
            ask_all_policy(),
        );
        let result = run_non_progress_in_scope(&runner, "delete it").await;

        assert_eq!(result.response, "done", "round N+1 wrap-up text delivered");
        assert_eq!(result.rounds, 2);
        assert!(!result.budget_capped);
        assert_eq!(result.round_details.len(), 2, "round telemetry carried out");
        assert_eq!(result.total_usage.input_tokens, 20, "usage accumulated");

        assert_eq!(result.tool_invocations.len(), 1);
        let inv = &result.tool_invocations[0];
        assert_eq!(inv.policy_decision, "require_approval");
        assert!(!inv.success);
        assert!(inv.denial_reason.is_none(), "routed is pending, not denied");
        assert_eq!(inv.error.as_deref(), Some(ROUTED_APPROVAL_TOOL_RESULT));

        let rows = deferred.lock().unwrap();
        assert_eq!(rows.len(), 1, "durable row persisted at deferral");
        assert_eq!(
            inv.output["approval_request_id"].as_str().unwrap(),
            rows[0].request_id,
            "invocation carries the persisted request id"
        );
        assert_eq!(
            inv.output["approver_user_ids"],
            serde_json::json!(["bob"]),
            "invocation carries the approvers (the row does not persist them)"
        );
        assert!(inv.output["detail"].is_string());
    }

    /// A provider that requests a tool call EVERY round — drives the stop
    /// check: after a routed approval the run must return without executing
    /// the wrap-up round's tool requests.
    /// Scripted provider for the final-round-answers tests: emits per-call
    /// responses by index, and marks the round-cap wrap-up call (empty tool
    /// defs) distinctly so a test can prove which path produced the answer.
    struct FinalRoundScriptProvider {
        calls: std::sync::atomic::AtomicUsize,
        /// Whether the FINAL budgeted round's response carries text alongside
        /// its tool calls.
        final_round_has_text: bool,
    }

    #[async_trait]
    impl LlmProvider for FinalRoundScriptProvider {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The runner's round-cap wrap-up is the only call whose system
            // prompt carries the ROUND LIMIT banner (the registry is empty in
            // this harness, so tool-def emptiness can't distinguish it).
            if system_prompt.is_some_and(|s| s.contains("ROUND LIMIT REACHED")) {
                return Ok(LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "wrap-up answer".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    model: "mock".to_string(),
                });
            }
            let mut content = Vec::new();
            if call >= 1 && self.final_round_has_text {
                content.push(ContentBlock::Text {
                    text: "the answer from evidence".to_string(),
                });
            }
            content.push(ContentBlock::tool_use(
                format!("tu_{call}"),
                "some_tool".to_string(),
                serde_json::json!({}),
            ));
            Ok(LlmResponse {
                content,
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    fn final_round_runner(interactive: bool, final_round_has_text: bool) -> AgentRunner {
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            model: Some("mock".to_string()),
            max_rounds: 2,
            interactive,
            ..Default::default()
        };
        AgentRunner::new(
            config,
            Arc::new(FinalRoundScriptProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                final_round_has_text,
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(
                workforce_policy::engine::PolicyEngine::from_yaml(
                    "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                )
                .expect("policy parse"),
            ),
        )
    }

    /// THE thrash fix, pinned. A model that reaches its final interactive round
    /// still calling tools used to overshoot into a "summary of what was
    /// accomplished" — the observed production failure was 17 rounds, 134s and
    /// a truncated non-answer with the relevant source already injected. The
    /// final interactive round is now an ANSWER round: its text is the answer
    /// and its tool calls are discarded, not executed.
    #[tokio::test]
    async fn final_interactive_round_takes_its_text_and_discards_tool_calls() {
        let runner = final_round_runner(true, true);
        let result = run_non_progress_in_scope(&runner, "where is it enforced?").await;

        assert_eq!(result.response, "the answer from evidence");
        assert_eq!(result.rounds, 2, "ends ON the budget, no overshoot");
        assert_eq!(
            result.tool_invocations.len(),
            1,
            "only round 1's tool ran — the final round's calls were discarded"
        );
        assert!(result.hit_round_cap, "Continue affordance stays reachable");
    }

    /// A final interactive round with ONLY tool calls has nothing to salvage:
    /// the runner proceeds to the round-cap wrap-up — reworded to answer the
    /// question — rather than executing past the budget.
    #[tokio::test]
    async fn final_interactive_round_with_only_tool_calls_falls_to_the_wrapup_answer() {
        let runner = final_round_runner(true, false);
        let result = run_non_progress_in_scope(&runner, "where is it enforced?").await;

        assert_eq!(result.response, "wrap-up answer");
        assert_eq!(
            result.tool_invocations.len(),
            1,
            "the final round's tool calls were not executed"
        );
        assert!(result.budget_capped);
    }

    /// Automated origins (scheduler/monitor/heartbeat) keep the old contract:
    /// a job's last round may legitimately be an action, so its tools execute.
    #[tokio::test]
    async fn automated_final_round_still_executes_its_tools() {
        let runner = final_round_runner(false, true);
        let result = run_non_progress_in_scope(&runner, "do the job").await;

        assert_eq!(
            result.tool_invocations.len(),
            2,
            "both rounds' tools executed for a non-interactive run"
        );
        assert_eq!(result.response, "wrap-up answer", "then the cap wraps up");
    }

    struct MockAlwaysToolUseProvider;

    #[async_trait]
    impl LlmProvider for MockAlwaysToolUseProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::tool_use(
                    format!("tu_{}", uuid::Uuid::new_v4()),
                    "gated_tool".to_string(),
                    serde_json::json!({}),
                )],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 10,
                    ..Default::default()
                },
                model: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn non_progress_routed_approval_stops_before_executing_more_tools() {
        let (runner, deferred) = routed_runner(
            Arc::new(MockAlwaysToolUseProvider),
            Arc::new(ToolRegistry::new()),
            StubDecision::Routed(vec!["bob".to_string()], false),
            ask_all_policy(),
        );
        let result = run_non_progress_in_scope(&runner, "delete it").await;

        assert_eq!(result.rounds, 2, "returns after the wrap-up round");
        assert_eq!(
            result.tool_invocations.len(),
            1,
            "round 2's tool request was NOT executed (or routed again)"
        );
        assert_eq!(deferred.lock().unwrap().len(), 1, "deferred exactly once");
        assert_eq!(result.round_details.len(), 2);
    }

    #[tokio::test]
    async fn non_progress_deny_unreachable_denies_and_run_continues() {
        let (runner, deferred) = routed_runner(
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
            StubDecision::DenyUnreachable,
            ask_all_policy(),
        );
        let result = run_non_progress_in_scope(&runner, "delete it").await;

        assert_eq!(result.response, "done");
        assert_eq!(result.rounds, 2);
        let inv = &result.tool_invocations[0];
        assert_eq!(inv.policy_decision, "deny");
        assert!(!inv.success);
        assert_eq!(inv.denial_reason.as_deref(), Some(UNREACHABLE_APPROVER));
        assert!(deferred.lock().unwrap().is_empty(), "nothing persisted");
    }

    #[tokio::test]
    async fn non_progress_self_serve_keeps_the_legacy_instant_error() {
        // No approval router configured → SelfServe → today's instant error.
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            model: Some("mock".to_string()),
            max_rounds: 5,
            ..Default::default()
        };
        let runner = AgentRunner::new(
            config,
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
            ask_all_policy(),
        );
        let result = run_non_progress_in_scope(&runner, "delete it").await;

        assert_eq!(result.response, "done");
        let inv = &result.tool_invocations[0];
        assert_eq!(inv.policy_decision, "require_approval");
        assert!(!inv.success);
        assert!(inv.denial_reason.is_some());
        assert!(inv
            .error
            .as_deref()
            .unwrap()
            .starts_with("Approval required:"));
        assert!(inv.output.is_null(), "legacy arm carries no coordinates");
    }

    /// Two tool calls in the routed round: the gated one routes, its sibling
    /// still executes.
    struct MockGatedPlusSiblingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for MockGatedPlusSiblingProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Ok(if first {
                LlmResponse {
                    content: vec![
                        ContentBlock::tool_use(
                            "tu_gated".to_string(),
                            "gated_tool".to_string(),
                            serde_json::json!({}),
                        ),
                        ContentBlock::tool_use(
                            "tu_echo".to_string(),
                            "echo".to_string(),
                            serde_json::json!({"say": "hi"}),
                        ),
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 10,
                        ..Default::default()
                    },
                    model: "mock".to_string(),
                }
            } else {
                LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    },
                    model: "mock".to_string(),
                }
            })
        }
    }

    struct SiblingEchoTool;

    #[async_trait]
    impl Tool for SiblingEchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult::success(input, 1))
        }
    }

    #[tokio::test]
    async fn non_progress_routed_sibling_tools_still_execute() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(SiblingEchoTool));
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: ask-gated\n    scope: global\n    tool: \"gated_tool\"\n    action: ask\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
            )
            .expect("policy parse"),
        );
        let (runner, deferred) = routed_runner(
            Arc::new(MockGatedPlusSiblingProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            tools,
            StubDecision::Routed(vec!["bob".to_string()], false),
            policy,
        );
        let result = run_non_progress_in_scope(&runner, "do both").await;

        assert_eq!(result.response, "done");
        assert_eq!(result.rounds, 2);
        assert_eq!(result.tool_invocations.len(), 2);
        let echo = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name == "echo")
            .expect("sibling executed");
        assert!(echo.success);
        assert_eq!(echo.output, serde_json::json!({"say": "hi"}));
        let gated = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name == "gated_tool")
            .expect("gated recorded");
        assert_eq!(gated.policy_decision, "require_approval");
        assert!(gated.output["approval_request_id"].is_string());
        assert_eq!(deferred.lock().unwrap().len(), 1);
    }

    /// A `CapabilityGate` that hard-denies every call — drives the
    /// pre-dispatch short-circuit in the runner test below.
    struct DenyAllCapabilityGate;

    impl CapabilityGate for DenyAllCapabilityGate {
        fn deny_reason(
            &self,
            _user_id: &str,
            _agent_id: &str,
            _tool: &str,
            _input: &serde_json::Value,
        ) -> Option<String> {
            Some("denied by capability gate".to_string())
        }
    }

    #[tokio::test]
    async fn capability_gate_deny_short_circuits_before_any_prompt() {
        // An `ask-all` policy would normally emit an ApprovalRequired prompt for
        // the tool call; the capability gate's pre-dispatch Deny must beat it so
        // NO prompt is ever emitted (a member can't "Always-Allow" past a
        // constitutional Deny). This is the prompt-then-deny guard.
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            model: Some("mock".to_string()),
            max_rounds: 5,
            capability_gate: Some(Arc::new(DenyAllCapabilityGate)),
            ..Default::default()
        };
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: ask-all\n    scope: global\n    tool: \"*\"\n    action: ask\n",
            )
            .expect("policy parse"),
        );
        let runner = AgentRunner::new(
            config,
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
            policy,
        );
        let (tx, mut rx) = mpsc::channel(64);
        // A user must be in scope for the gate to consult (system contexts pass
        // through); the gate then denies the tool.
        CURRENT_USER_ID
            .scope("alice".to_string(), runner.run_with_progress("go", tx))
            .await
            .expect("turn completes");

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProgressEvent::ApprovalRequired { .. })),
            "capability-gate Deny must short-circuit before any approval prompt"
        );
        // The tool was attempted (ToolStart) but denied — never an approval.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProgressEvent::ToolStart { .. })),
            "the denied tool was still dispatched (and then hard-denied)"
        );
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "vault.delete"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            input: serde_json::Value,
        ) -> eyre::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({"deleted": input}),
                1,
            ))
        }
    }

    #[tokio::test]
    async fn preexecute_runs_tool_and_seeds_history() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let runner = make_runner_with_tools(Arc::new(MockSimpleProvider::new("ok")), tools);
        let mut messages = Vec::new();
        runner
            .seed_call(
                &mut messages,
                &mut Vec::new(),
                Seeded::Approved(&PreExecutedTool {
                    tool_name: "vault.delete".to_string(),
                    tool_input: serde_json::json!({"id": 7}),
                }),
                None,
            )
            .await;
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0].role, Role::Assistant));
        match &messages[0].content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "vault_delete");
                assert_eq!(input, &serde_json::json!({"id": 7}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &messages[1].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(*is_error, None);
                assert!(content.contains("deleted"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_approved_call_runs_even_when_its_tool_is_catalogued() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let runner = with_loading(
            make_runner_with_tools(Arc::new(MockSimpleProvider::new("ok")), tools),
            &[],
        );
        let mut messages = Vec::new();
        runner
            .seed_call(
                &mut messages,
                &mut Vec::new(),
                Seeded::Approved(&PreExecutedTool {
                    tool_name: "vault.delete".to_string(),
                    tool_input: serde_json::json!({"id": 7}),
                }),
                None,
            )
            .await;
        match &messages[1].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(*is_error, None);
                assert!(content.contains("deleted"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn preexecute_skips_when_policy_denies() {
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: deny-all\n    scope: global\n    tool: \"*\"\n    action: deny\n",
            )
            .expect("policy parse"),
        );
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let runner = AgentRunner::new(
            RunnerConfig {
                agent_id: workforce_core::types::AgentId::new("test"),
                ..Default::default()
            },
            Arc::new(MockSimpleProvider::new("ok")),
            tools,
            policy,
        );
        let mut messages = Vec::new();
        runner
            .seed_call(
                &mut messages,
                &mut Vec::new(),
                Seeded::Approved(&PreExecutedTool {
                    tool_name: "vault.delete".to_string(),
                    tool_input: serde_json::json!({"id": 7}),
                }),
                None,
            )
            .await;
        match &messages[1].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(*is_error, Some(true));
                assert!(content.contains("no longer permitted"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// Records the `tools_precurated()` flag observed inside `create_message` —
    /// the exact call site where real providers run `filter_tools`. Proves a
    /// scope set around the run reaches that site (i.e. the LLM call is awaited
    /// inline, not spawned).
    struct RecordingPrecuratedProvider {
        seen: Arc<std::sync::Mutex<Option<bool>>>,
    }

    #[async_trait]
    impl LlmProvider for RecordingPrecuratedProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            *self.seen.lock().unwrap() = Some(crate::runner::tools_precurated());
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                model: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn precurated_scope_reaches_provider_create_message() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let runner = make_runner(Arc::new(RecordingPrecuratedProvider {
            seen: Arc::clone(&seen),
        }));
        PRECURATED_TOOLSET
            .scope(true, runner.run_with_history("hello", vec![]))
            .await
            .expect("run");
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "the worker's pre-curated scope must reach create_message"
        );
    }

    #[tokio::test]
    async fn ordinary_run_is_not_precurated() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let runner = make_runner(Arc::new(RecordingPrecuratedProvider {
            seen: Arc::clone(&seen),
        }));
        runner.run_with_history("hello", vec![]).await.expect("run");
        assert_eq!(*seen.lock().unwrap(), Some(false));
    }

    /// Emits one ToolUse on the first call, then ends the turn. Lets tests
    /// drive a real tool round through the runner's join sites.
    #[derive(Default)]
    struct MockToolUseProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for MockToolUseProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Ok(if first {
                LlmResponse {
                    content: vec![ContentBlock::tool_use(
                        "tu_1".to_string(),
                        "bulk_rows".to_string(),
                        serde_json::json!({}),
                    )],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 10,
                        ..Default::default()
                    },
                    model: "mock".to_string(),
                }
            } else {
                LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    },
                    model: "mock".to_string(),
                }
            })
        }
    }

    #[derive(Default)]
    struct RecordingJournal {
        pending: Mutex<Vec<(String, String, serde_json::Value)>>,
        checkpoints: Mutex<Vec<(String, String)>>,
        finished: Mutex<Vec<(String, ToolInvocation)>>,
    }

    #[async_trait]
    impl crate::turn_journal::TurnJournal for RecordingJournal {
        async fn pending(
            &self,
            call: &str,
            tool: &str,
            _input: &serde_json::Value,
            plan: serde_json::Value,
        ) -> Result<()> {
            self.pending
                .lock()
                .unwrap()
                .push((call.to_string(), tool.to_string(), plan));
            Ok(())
        }

        async fn checkpoint(&self, call: &str, mark: &str) -> Result<()> {
            self.checkpoints
                .lock()
                .unwrap()
                .push((call.to_string(), mark.to_string()));
            Ok(())
        }

        async fn finished(&self, call: &str, invocation: &ToolInvocation) {
            self.finished
                .lock()
                .unwrap()
                .push((call.to_string(), invocation.clone()));
        }
    }

    /// A tool that leaves work running: it journals a plan and a checkpoint
    /// under the call it runs in, and collects by echoing what it was given.
    struct LeavesWorkTool;

    #[async_trait]
    impl Tool for LeavesWorkTool {
        fn name(&self) -> &str {
            "bulk_rows"
        }
        fn description(&self) -> &str {
            "leaves work running"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            let call = ToolCall::current().expect("a tool runs inside its call");
            call.pending("bulk_rows", serde_json::json!({"round": call.id()}))
                .await?;
            call.checkpoint("job-1").await?;
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({"call": call.id()}),
                1,
            ))
        }
        async fn collect(
            &self,
            input: serde_json::Value,
            plan: serde_json::Value,
            checkpoint: Option<String>,
        ) -> Result<crate::tools::ToolResult> {
            let call = ToolCall::current().expect("a collect runs inside its call");
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({
                    "collected": plan,
                    "call": call.id(),
                    "input": input,
                    "checkpoint": checkpoint,
                }),
                1,
            ))
        }
    }

    fn journalled_runner(
        journal: &Arc<RecordingJournal>,
        collect: Vec<crate::turn_journal::PendingCall>,
    ) -> AgentRunner {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(LeavesWorkTool));
        tools.register(Arc::new(EchoTool));
        let mut runner = make_runner_with_tools(
            Arc::new(MockToolUseProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            tools,
        );
        let journal: Arc<dyn crate::turn_journal::TurnJournal> = journal.clone();
        runner.config.journal = Some(journal);
        runner.config.collect = collect;
        runner
    }

    fn assert_journalled_as_its_own_call(journal: &RecordingJournal) {
        let pending = journal.pending.lock().unwrap().clone();
        let finished = journal.finished.lock().unwrap().clone();
        assert_eq!(pending.len(), 1);
        assert_eq!(finished.len(), 1);
        let (call, tool, plan) = &pending[0];
        assert_eq!(tool, "bulk_rows");
        assert_eq!(plan, &serde_json::json!({"round": call}));
        assert_eq!(&finished[0].0, call, "one id for the call throughout");
        assert_eq!(finished[0].1.output, serde_json::json!({"call": call}));
        assert_eq!(
            journal.checkpoints.lock().unwrap().clone(),
            vec![(call.clone(), "job-1".to_string())]
        );
    }

    #[tokio::test]
    async fn each_call_is_journalled_under_the_id_its_tool_runs_in() {
        let journal = Arc::new(RecordingJournal::default());
        journalled_runner(&journal, Vec::new())
            .run_with_history("go", Vec::new())
            .await
            .unwrap();
        assert_journalled_as_its_own_call(&journal);
    }

    #[tokio::test]
    async fn each_streamed_call_is_journalled_under_the_id_its_tool_runs_in() {
        let journal = Arc::new(RecordingJournal::default());
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        journalled_runner(&journal, Vec::new())
            .run_with_progress_and_history(
                vec![ContentBlock::Text { text: "go".into() }],
                Vec::new(),
                tx,
                None,
                None,
            )
            .await
            .unwrap();
        assert_journalled_as_its_own_call(&journal);
    }

    #[tokio::test]
    async fn work_a_turn_left_running_is_collected_as_its_call_before_the_first_round() {
        let journal = Arc::new(RecordingJournal::default());
        let pending = crate::turn_journal::PendingCall {
            call: "c9".into(),
            tool: "bulk_rows".into(),
            input: serde_json::json!({"rows": 3}),
            plan: serde_json::json!({"round": "r9"}),
            checkpoint: Some("job-4".into()),
        };
        let unknown = crate::turn_journal::PendingCall {
            call: "c10".into(),
            tool: "vault.delete".into(),
            input: serde_json::json!({}),
            plan: serde_json::json!({}),
            checkpoint: None,
        };
        let runner = journalled_runner(&journal, vec![pending, unknown]);
        let (mut messages, mut invocations) = (Vec::new(), Vec::new());
        runner
            .seed_calls(&mut messages, &mut invocations, None, None)
            .await;

        assert_eq!(messages.len(), 4, "a call and its result for each");
        let ContentBlock::ToolUse { name, input, .. } = &messages[0].content[0] else {
            panic!("expected the call first");
        };
        assert_eq!(
            (name.as_str(), input),
            ("bulk_rows", &serde_json::json!({"rows": 3}))
        );
        let ContentBlock::ToolResult { content, .. } = &messages[1].content[0] else {
            panic!("expected its result next");
        };
        assert!(
            ["r9", "c9", "job-4", "rows"]
                .iter()
                .all(|seen| content.contains(seen)),
            "{content}"
        );
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &messages[3].content[0]
        else {
            panic!("expected the second result");
        };
        assert_eq!(content, crate::turn_journal::OUTCOME_UNKNOWN);
        assert_eq!(*is_error, Some(true));
        let ContentBlock::ToolUse { name, .. } = &messages[2].content[0] else {
            panic!("expected the second call");
        };
        assert_eq!(name, "vault_delete", "named as the model names its tools");

        let finished: Vec<String> = journal
            .finished
            .lock()
            .unwrap()
            .iter()
            .map(|(call, _)| call.clone())
            .collect();
        assert_eq!(
            finished,
            vec!["c9", "c10"],
            "collected once, under the original calls"
        );
        assert_eq!(invocations.len(), 2);
        assert!(invocations[0].success && !invocations[1].success);
    }

    #[tokio::test]
    async fn work_policy_now_forbids_is_ended_rather_than_collected() {
        #[derive(Default)]
        struct Abandons(Mutex<Vec<(serde_json::Value, Option<String>)>>);

        #[async_trait]
        impl Tool for Abandons {
            fn name(&self) -> &str {
                "bulk_rows"
            }
            fn description(&self) -> &str {
                "leaves work running"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(serde_json::json!({}), 1))
            }
            async fn abandon(&self, plan: serde_json::Value, checkpoint: Option<String>) {
                self.0.lock().unwrap().push((plan, checkpoint));
            }
        }

        let tool = Arc::new(Abandons::default());
        let tools = Arc::new(ToolRegistry::new());
        tools.register(tool.clone());
        let mut runner = make_runner_with_tools_and_policy(
            Arc::new(MockSimpleProvider::new("ok")),
            tools,
            "rules:\n  - name: deny-all\n    scope: global\n    tool: \"*\"\n    action: deny\n",
        );
        runner.config.collect = vec![crate::turn_journal::PendingCall {
            call: "c9".into(),
            tool: "bulk_rows".into(),
            input: serde_json::json!({}),
            plan: serde_json::json!({"round": "r9"}),
            checkpoint: Some("job-4".into()),
        }];
        let (mut messages, mut invocations) = (Vec::new(), Vec::new());
        runner
            .seed_calls(&mut messages, &mut invocations, None, None)
            .await;
        assert_eq!(
            tool.0.lock().unwrap().clone(),
            vec![(
                serde_json::json!({"round": "r9"}),
                Some("job-4".to_string())
            )]
        );
        assert_eq!(invocations[0].error.as_deref(), Some(NO_LONGER_PERMITTED));
    }

    #[tokio::test]
    async fn a_cancelled_run_stops_collecting() {
        let journal = Arc::new(RecordingJournal::default());
        let pending = crate::turn_journal::PendingCall {
            call: "c9".into(),
            tool: "bulk_rows".into(),
            input: serde_json::json!({}),
            plan: serde_json::json!({}),
            checkpoint: None,
        };
        let runner = journalled_runner(&journal, vec![pending]);
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let (mut messages, mut invocations) = (Vec::new(), Vec::new());
        runner
            .seed_calls(&mut messages, &mut invocations, None, Some(&cancel))
            .await;
        assert!(messages.is_empty() && invocations.is_empty());
        assert!(
            journal.finished.lock().unwrap().is_empty(),
            "left to settle"
        );
    }

    #[tokio::test]
    async fn large_tabular_tool_output_spills_canonically() {
        use crate::artifacts::store::install_test_global_store;
        use crate::artifacts::types::{ArtifactBody, ArtifactId};

        struct BulkRowsTool;

        #[async_trait]
        impl Tool for BulkRowsTool {
            fn name(&self) -> &str {
                "bulk_rows"
            }
            fn description(&self) -> &str {
                "returns a big table"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                let rows: Vec<serde_json::Value> = (0..2000)
                    .map(|i| serde_json::json!({"id": i, "name": format!("row-{i}-padding")}))
                    .collect();
                Ok(crate::tools::ToolResult::success(
                    serde_json::Value::Array(rows),
                    0,
                ))
            }
        }

        let store = install_test_global_store();
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(BulkRowsTool));
        // Spilling is contract-bound: an output is only staged in a form this
        // surface can read back. Registering the table readers is what makes a
        // tabular artifact legitimate here — without them the runner would
        // (correctly) leave the rows inline.
        tools.register(Arc::new(crate::tools::data::DataQueryTool));
        tools.register(Arc::new(crate::tools::data::DataInspectTool));
        let runner = make_runner_with_tools(Arc::new(MockToolUseProvider::default()), tools);

        let result = CURRENT_AGENT_ID
            .scope(
                "spill-int-agent".to_string(),
                CURRENT_SESSION_ID.scope(
                    "spill-int-session".to_string(),
                    runner.run("fetch all the rows"),
                ),
            )
            .await
            .unwrap();

        let inv = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name.contains("bulk_rows"))
            .expect("tool ran");
        assert_eq!(
            inv.output["kind"], "tabular_artifact",
            "the persisted invocation record must hold the digest, not rows"
        );
        assert_eq!(inv.output["rows"], 2000);

        let id = ArtifactId::parse(inv.output["artifact_id"].as_str().unwrap()).unwrap();
        let descriptor = store.metadata("spill-int-agent", &id).unwrap().unwrap();
        assert_eq!(descriptor.session_id.as_deref(), Some("spill-int-session"));
        assert_eq!(descriptor.producer, "bulk_rows");
        let ArtifactBody::Json(serde_json::Value::Array(stored)) =
            store.read("spill-int-agent", &id).unwrap()
        else {
            panic!("expected canonical rows");
        };
        assert_eq!(stored.len(), 2000);
    }

    #[tokio::test]
    async fn deliver_round_images_delivers_only_successful_image_results() {
        use crate::artifacts::types::ArtifactId;
        use crate::tools::image_gen::ImageSink;

        #[derive(Default)]
        struct RecordingSink {
            calls: std::sync::Mutex<Vec<(String, String)>>,
        }
        #[async_trait]
        impl ImageSink for RecordingSink {
            async fn deliver(
                &self,
                _ctx: &SessionContext,
                artifact_id: &ArtifactId,
                media_type: &str,
                _filename: Option<&str>,
                _caption: Option<&str>,
            ) -> Result<()> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((artifact_id.as_str().to_string(), media_type.to_string()));
                Ok(())
            }
        }

        let sink = Arc::new(RecordingSink::default());
        let runner =
            make_runner(Arc::new(MockSimpleProvider::new("ok"))).with_image_sink(sink.clone());

        let id = ArtifactId::fresh();
        let image = |success: bool, art: &ArtifactId| ToolInvocation {
            tool_name: "generate_image".into(),
            input: serde_json::json!({}),
            output: serde_json::json!({
                "generated_images": [{ "artifact_id": art.as_str(), "media_type": "image/jpeg" }]
            }),
            success,
            policy_decision: "allow".into(),
            duration_ms: 1,
            denial_reason: None,
            error: None,
        };
        let other = ToolInvocation {
            tool_name: "filesystem_read".into(),
            input: serde_json::json!({}),
            output: serde_json::json!({ "content": "x" }),
            success: true,
            policy_decision: "allow".into(),
            duration_ms: 1,
            denial_reason: None,
            error: None,
        };
        // Successful image → delivered; failed image → skipped; non-image → ignored.
        runner
            .deliver_round_images(&[image(true, &id), image(false, &ArtifactId::fresh()), other])
            .await;

        let recorded = sink.calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "only the successful image should deliver"
        );
        assert_eq!(recorded[0].0, id.as_str());
        assert_eq!(recorded[0].1, "image/jpeg");
    }

    #[tokio::test(start_paused = true)]
    async fn views_past_the_steps_render_budget_are_said_to_be_unrendered() {
        use crate::tools::image_view::{ImageViewSink, ViewedImageRef, VIEWED_IMAGES_KEY};

        struct SlowViewSink;
        #[async_trait]
        impl ImageViewSink for SlowViewSink {
            async fn render(
                &self,
                _ctx: &SessionContext,
                viewed: &ViewedImageRef,
                _active_model: &str,
            ) -> Result<Vec<ContentBlock>> {
                tokio::time::sleep(Duration::from_secs(200)).await;
                Ok(vec![ContentBlock::Text {
                    text: format!("rendered {}", viewed.filename),
                }])
            }
        }

        let views: Vec<ToolInvocation> = (1..=4)
            .map(|i| ToolInvocation {
                tool_name: "view_attachment".into(),
                input: serde_json::json!({}),
                output: serde_json::json!({
                    VIEWED_IMAGES_KEY: [{
                        "artifact_id": crate::artifacts::types::ArtifactId::fresh().as_str(),
                        "media_type": "video/mp4",
                        "filename": format!("{i}.mp4")
                    }]
                }),
                success: true,
                policy_decision: "allow".into(),
                duration_ms: 1,
                denial_reason: None,
                error: None,
            })
            .collect();
        let blocks = render_viewed(&SlowViewSink, &SessionContext::default(), &views, "mock").await;
        let texts: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.as_str(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect();
        assert_eq!(texts[..2], ["rendered 1.mp4", "rendered 2.mp4"]);
        assert!(
            texts[2].starts_with("`3.mp4` was not rendered: this step's time limit"),
            "{}",
            texts[2]
        );
        assert!(texts[3].starts_with("`4.mp4` was not rendered"));
    }

    #[tokio::test]
    async fn append_viewed_images_appends_blocks_after_tool_results() {
        use crate::artifacts::types::ArtifactId;
        use crate::tools::image_view::{ImageViewSink, ViewedImageRef};

        struct FakeViewSink;
        #[async_trait]
        impl ImageViewSink for FakeViewSink {
            async fn render(
                &self,
                _ctx: &SessionContext,
                viewed: &ViewedImageRef,
                _active_model: &str,
            ) -> Result<Vec<ContentBlock>> {
                Ok(vec![ContentBlock::image_base64(
                    &viewed.media_type,
                    "Zm9v".into(),
                )])
            }
        }

        let runner = make_runner(Arc::new(MockSimpleProvider::new("ok")))
            .with_image_view_sink(Arc::new(FakeViewSink));

        let id = ArtifactId::fresh();
        let viewed = |success: bool| ToolInvocation {
            tool_name: "view_attachment".into(),
            input: serde_json::json!({}),
            output: serde_json::json!({
                "viewed_images": [{
                    "artifact_id": id.as_str(),
                    "media_type": "image/png",
                    "filename": "generated.png"
                }]
            }),
            success,
            policy_decision: "allow".into(),
            duration_ms: 1,
            denial_reason: None,
            error: None,
        };

        let mut messages = vec![LlmMessage::tool_results(vec![(
            "call_1".into(),
            "{}".into(),
            false,
        )])];
        // A failed invocation is skipped; only the successful one renders.
        runner
            .append_viewed_images(&mut messages, &[viewed(false), viewed(true)], "mock")
            .await;

        let content = &messages.last().unwrap().content;
        let kinds: Vec<&str> = content
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult { .. } => "tool_result",
                ContentBlock::Image { .. } => "image",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["tool_result", "image"],
            "image is appended after the tool result, exactly once"
        );
    }

    #[tokio::test]
    async fn test_streaming_progress_emits_deltas() {
        let provider = Arc::new(MockStreamingProvider::new(vec![
            "Hello", ", ", "world", "!",
        ]));
        let runner = make_runner(provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Say hello", tx).await.unwrap();
        assert_eq!(result.response, "Hello, world!");
        assert_eq!(result.rounds, 1);
        assert!(!result.budget_capped);

        // Collect all progress events
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        // Verify event sequence: LlmCall → StreamStart → StreamDelta* → StreamEnd → Response → Done
        let event_types: Vec<&str> = events
            .iter()
            .map(|e| match e {
                ProgressEvent::LlmCall { .. } => "llm_call",
                ProgressEvent::StreamStart => "stream_start",
                ProgressEvent::StreamDelta { .. } => "stream_delta",
                ProgressEvent::StreamEnd => "stream_end",
                ProgressEvent::StreamDiscard { .. } => "stream_discard",
                ProgressEvent::AnswerSettled => "answer_settled",
                ProgressEvent::Response { .. } => "response",
                ProgressEvent::Done { .. } => "done",
                ProgressEvent::ToolStart { .. } => "tool_start",
                ProgressEvent::ToolEnd { .. } => "tool_end",
                ProgressEvent::CompactStart => "compact_start",
                ProgressEvent::CompactEnd => "compact_end",
                ProgressEvent::ContextUpdate { .. } => "context_update",
                ProgressEvent::InjectedMessage { .. } => "injected_message",
                ProgressEvent::ApprovalRequired { .. } => "approval_required",
                ProgressEvent::ApprovalResolved { .. } => "approval_resolved",
                ProgressEvent::RateLimited { .. } => "rate_limited",
                ProgressEvent::SlowModelFailover { .. } => "slow_model_failover",
                ProgressEvent::SecretConsumed { .. } => "secret_consumed",
                ProgressEvent::SubActivity { .. } => "sub_activity",
                ProgressEvent::CheckIn { .. } => "check_in",
                ProgressEvent::ConnectRequired { .. } => "connect_required",
                ProgressEvent::Snapshot(_) => "snapshot",
            })
            .collect();

        assert_eq!(event_types[0], "llm_call");
        assert_eq!(event_types[1], "stream_start");
        // Middle events are stream_delta
        let delta_count = event_types.iter().filter(|&&t| t == "stream_delta").count();
        assert_eq!(delta_count, 4); // "Hello", ", ", "world", "!"
        assert!(event_types.contains(&"stream_end"));
        assert!(event_types.contains(&"response"));
        assert!(event_types.contains(&"done"));

        // Verify delta content
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProgressEvent::StreamDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hello", ", ", "world", "!"]);
    }

    /// Replays one scripted event sequence per LLM call, then its outcome —
    /// the raw stream a failover layer hands the runner.
    struct EventScriptProvider {
        calls: std::sync::Mutex<std::collections::VecDeque<(Vec<StreamEvent>, Result<()>)>>,
    }

    impl EventScriptProvider {
        fn new(calls: Vec<(Vec<StreamEvent>, Result<()>)>) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(calls.into()),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for EventScriptProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            unreachable!("streamed only")
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let (events, outcome) = self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .expect("streamed past the script");
            for event in events {
                let _ = tx.send(event).await;
            }
            outcome
        }
    }

    fn text_block(index: usize, text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ContentStart { index },
            StreamEvent::TextDelta {
                index,
                text: text.to_string(),
            },
            StreamEvent::ContentStop { index },
        ]
    }

    fn message_done() -> StreamEvent {
        StreamEvent::MessageDone {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
        }
    }

    /// The previewed draft, a restart, then the real answer.
    fn restarted_answer() -> Vec<StreamEvent> {
        [
            vec![
                StreamEvent::ContentStart { index: 0 },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "The abandoned draft".to_string(),
                },
                StreamEvent::Restart,
            ],
            text_block(0, "The real answer."),
            vec![message_done()],
        ]
        .concat()
    }

    fn drain(rx: &mut mpsc::Receiver<ProgressEvent>) -> Vec<ProgressEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[tokio::test]
    async fn a_restart_discards_the_preview_as_interrupted() {
        let runner = make_runner(EventScriptProvider::new(vec![(restarted_answer(), Ok(()))]));
        let (tx, mut rx) = mpsc::channel(256);

        let result = runner.run_with_progress("Go", tx).await.unwrap();

        assert_eq!(result.response, "The real answer.");
        let reasons: Vec<StreamDiscardReason> = drain(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                ProgressEvent::StreamDiscard { reason } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, [StreamDiscardReason::Interrupted]);
    }

    #[tokio::test]
    async fn a_restart_reopens_the_stream_without_a_stream_end() {
        let runner = make_runner(EventScriptProvider::new(vec![(restarted_answer(), Ok(()))]));
        let (tx, mut rx) = mpsc::channel(256);

        runner.run_with_progress("Go", tx).await.unwrap();

        let stream: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                ProgressEvent::StreamStart => Some("start".to_string()),
                ProgressEvent::StreamDelta { text } => Some(text),
                ProgressEvent::StreamEnd => Some("end".to_string()),
                ProgressEvent::StreamDiscard { .. } => Some("discard".to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            stream[..6],
            [
                "start",
                "The abandoned draft",
                "discard",
                "start",
                "The real answer.",
                "end"
            ]
        );
    }

    #[tokio::test]
    async fn a_restart_with_nothing_previewed_sends_no_discard() {
        let events = [
            vec![
                StreamEvent::ContentStart { index: 0 },
                StreamEvent::ThinkingDelta {
                    index: 0,
                    thinking: "considering".to_string(),
                },
                StreamEvent::Restart,
            ],
            text_block(0, "The real answer."),
            vec![message_done()],
        ]
        .concat();
        let runner = make_runner(EventScriptProvider::new(vec![(events, Ok(()))]));
        let (tx, mut rx) = mpsc::channel(256);

        let result = runner.run_with_progress("Go", tx).await.unwrap();

        assert_eq!(result.response, "The real answer.");
        assert!(!drain(&mut rx)
            .iter()
            .any(|event| matches!(event, ProgressEvent::StreamDiscard { .. })));
    }

    #[tokio::test]
    async fn a_tool_call_streamed_by_an_abandoned_attempt_never_runs() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let events = [
            text_block(0, "Deleting it now."),
            vec![
                StreamEvent::ToolUseStart {
                    index: 1,
                    id: "toolu_abandoned".to_string(),
                    name: "vault.delete".to_string(),
                    signature: None,
                },
                StreamEvent::InputJsonDelta {
                    index: 1,
                    partial: r#"{"id": 7}"#.to_string(),
                },
                StreamEvent::ContentStop { index: 1 },
                StreamEvent::Restart,
            ],
            text_block(0, "The real answer."),
            vec![message_done()],
        ]
        .concat();
        let runner =
            make_runner_with_tools(EventScriptProvider::new(vec![(events, Ok(()))]), tools);
        let (tx, mut rx) = mpsc::channel(256);

        let result = runner.run_with_progress("Go", tx).await.unwrap();

        assert_eq!(result.response, "The real answer.");
        assert!(result.tool_invocations.is_empty());
        assert!(drain(&mut rx)
            .iter()
            .all(|event| !matches!(event, ProgressEvent::ToolStart { .. })));
    }

    #[tokio::test]
    async fn a_stream_failing_after_a_closed_block_is_an_error_not_an_answer() {
        let events = [
            text_block(0, "Let me look that up."),
            vec![StreamEvent::ToolUseStart {
                index: 1,
                id: "toolu_cut".to_string(),
                name: "vault.delete".to_string(),
                signature: None,
            }],
        ]
        .concat();
        let runner = make_runner(EventScriptProvider::new(vec![(
            events,
            Err(eyre::eyre!("Bedrock streaming error: Overloaded")),
        )]));
        let (tx, _rx) = mpsc::channel(256);

        let err = runner.run_with_progress("Go", tx).await.unwrap_err();

        assert!(err.to_string().contains("Overloaded"));
    }

    #[tokio::test]
    async fn test_hallucinated_tool_call_discards_the_stream_as_a_retry() {
        // Round 1 streams a tool call as text (garbage), round 2 the real
        // answer. The retry path must discard the streamed garbage with the
        // Retry reason so consumers wipe it rather than retain it.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            r#"Sure — {"tool": "vault.delete", "arguments": {"id": 7}}"#,
            "The real answer.",
        ]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "The real answer.");

        let mut reasons = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ProgressEvent::StreamDiscard { reason } = event {
                reasons.push(reason);
            }
        }
        assert_eq!(reasons, vec![StreamDiscardReason::Retry]);
    }

    /// STREAMING-loop pin for the final-round contract (implementation review
    /// F2/F5): a prose tool call emitted ON the final interactive round must
    /// never be delivered as the answer — it is discarded like the mid-turn
    /// retry case, and the round-cap wrap-up produces the real reply. The
    /// design-level tests drive the non-streaming loop; this one exists because
    /// the production failure lives on the streaming path.
    #[tokio::test]
    async fn streaming_final_round_prose_tool_call_is_discarded_not_delivered() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        // Round 1: prose tool call → normal retry. Round 2 (final): prose tool
        // call again → the answer-only branch discards and proceeds to the
        // wrap-up, which pops the third script entry.
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            r#"Sure — {"tool": "vault.delete", "arguments": {"id": 7}}"#,
            r#"{"tool": "vault.delete", "arguments": {"id": 8}}"#,
            "the wrap-up answer",
        ]));
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            system_prompt: "You are a test agent.".to_string(),
            model: Some("mock".to_string()),
            max_rounds: 2,
            interactive: true,
            ..Default::default()
        };
        let runner = AgentRunner::new(
            config,
            provider,
            tools,
            Arc::new(
                workforce_policy::engine::PolicyEngine::from_yaml(
                    "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                )
                .expect("policy parse"),
            ),
        );
        let (tx, mut rx) = mpsc::channel(256);
        let result = runner
            .run_with_progress("where is it enforced?", tx)
            .await
            .unwrap();

        assert_eq!(
            result.response, "the wrap-up answer",
            "prose tool-call junk must never become the delivered answer"
        );
        assert!(result.budget_capped);
        let mut retries = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                ProgressEvent::StreamDiscard {
                    reason: StreamDiscardReason::Retry
                }
            ) {
                retries += 1;
            }
        }
        assert_eq!(
            retries, 2,
            "both junk rounds discarded from the reader's preview"
        );
    }

    #[tokio::test]
    async fn test_non_streaming_fallback_emits_deltas() {
        // MockSimpleProvider uses the default create_message_stream which
        // synthesises stream events from the complete response
        let provider = Arc::new(MockSimpleProvider::new("Fallback response"));
        let runner = make_runner(provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Test fallback", tx).await.unwrap();
        assert_eq!(result.response, "Fallback response");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        // Should still get StreamStart → StreamDelta → StreamEnd
        let has_stream_start = events
            .iter()
            .any(|e| matches!(e, ProgressEvent::StreamStart));
        let has_stream_end = events.iter().any(|e| matches!(e, ProgressEvent::StreamEnd));
        let has_response = events
            .iter()
            .any(|e| matches!(e, ProgressEvent::Response { .. }));

        assert!(has_stream_start, "Missing StreamStart event");
        assert!(has_stream_end, "Missing StreamEnd event");
        assert!(has_response, "Missing Response event");

        // The fallback sends the whole text as a single delta
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProgressEvent::StreamDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Fallback response"]);
    }

    #[tokio::test]
    async fn test_non_progress_run_still_works() {
        // Ensure the non-progress run() method still works (uses create_message directly)
        let provider = Arc::new(MockSimpleProvider::new("Direct response"));
        let runner = make_runner(provider);

        let result = runner.run("Test direct").await.unwrap();
        assert_eq!(result.response, "Direct response");
        assert_eq!(result.rounds, 1);
        assert_eq!(result.total_usage.input_tokens, 5);
        assert_eq!(result.total_usage.output_tokens, 15);
        assert!(!result.budget_capped);
    }

    /// Records the ambient thinking-budget override observed *inside*
    /// `create_message_stream` — i.e. on the far side of the runner's
    /// `tokio::spawn`. A plain task-local does not cross that spawn, so this
    /// is the only check that distinguishes a working override from one that
    /// silently evaporates on the streaming path.
    struct BudgetProbeProvider {
        seen: std::sync::Mutex<Option<Option<u32>>>,
        effort: std::sync::Mutex<Option<crate::llm::anthropic::Effort>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for BudgetProbeProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "probe".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            *self.seen.lock().unwrap() = Some(crate::llm::provider::thinking_budget_override());
            *self.effort.lock().unwrap() = crate::llm::provider::effort_override();
            let _ = tx.send(StreamEvent::ContentStart { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::TextDelta {
                    index: 0,
                    text: "ok".to_string(),
                })
                .await;
            let _ = tx.send(StreamEvent::ContentStop { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
                .await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn thinking_budget_override_survives_the_streaming_spawn() {
        let provider = Arc::new(BudgetProbeProvider {
            seen: std::sync::Mutex::new(None),
            effort: std::sync::Mutex::new(None),
        });
        let runner = make_runner(provider.clone());
        let (tx, _rx) = mpsc::channel(64);

        crate::llm::provider::with_thinking_budget_override(
            Some(0),
            runner.run_with_progress("Hi", tx),
        )
        .await
        .unwrap();

        assert_eq!(
            *provider.seen.lock().unwrap(),
            Some(Some(0)),
            "the override must reach the spawned streaming call"
        );
    }

    #[tokio::test]
    async fn the_turns_effort_survives_the_streaming_spawn() {
        use crate::llm::anthropic::Effort;
        let provider = Arc::new(BudgetProbeProvider {
            seen: std::sync::Mutex::new(None),
            effort: std::sync::Mutex::new(None),
        });
        let runner = make_runner(provider.clone());
        let (tx, _rx) = mpsc::channel(64);

        crate::llm::provider::with_effort_override(
            Some(Effort::Xhigh),
            runner.run_with_progress("Hi", tx),
        )
        .await
        .unwrap();

        assert_eq!(*provider.effort.lock().unwrap(), Some(Effort::Xhigh));
    }

    #[tokio::test]
    async fn no_thinking_budget_override_leaves_the_stream_call_untouched() {
        let provider = Arc::new(BudgetProbeProvider {
            seen: std::sync::Mutex::new(None),
            effort: std::sync::Mutex::new(None),
        });
        let runner = make_runner(provider.clone());
        let (tx, _rx) = mpsc::channel(64);

        crate::llm::provider::with_thinking_budget_override(
            None,
            runner.run_with_progress("Hi", tx),
        )
        .await
        .unwrap();

        assert_eq!(*provider.seen.lock().unwrap(), Some(None));
    }

    #[test]
    fn cached_prefix_for_round_marks_only_round_one_with_history() {
        assert_eq!(cached_prefix_for_round(1, 3), Some(3));
        assert_eq!(cached_prefix_for_round(2, 3), None);
        assert_eq!(cached_prefix_for_round(1, 0), None);
    }

    #[test]
    fn round_cap_wrapup_keeps_the_banner_in_its_own_segment() {
        let stable = format!("static{CACHE_BREAK_MARKER}semi");
        for interactive in [true, false] {
            let prompt = round_cap_wrapup_prompt(&stable, 5, interactive);
            let segments = crate::llm::anthropic::cache_segments(&prompt);
            assert_eq!(segments.len(), 3);
            assert_eq!(&segments[..2], ["static", "semi"]);
            assert!(segments[2].contains("ROUND LIMIT REACHED"));
        }
    }

    /// A tool that does nothing, so a scripted `tool_use` round can complete.
    struct NoopTool;

    #[async_trait]
    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "does nothing"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({"ok": true}),
                0,
            ))
        }
    }

    /// The context block leading every task message in the prefix tests.
    const TASK_CONTEXT: &str = "[SYSTEM CONTEXT] turn context";

    /// Records `(system_prompt, messages, cached_prefix_len())` per call and
    /// answers with a `tool_use` when the last message is a turn's task
    /// (led by [`TASK_CONTEXT`]), text on every later round — one tool round
    /// per turn.
    #[derive(Default)]
    struct PrefixRecordingProvider {
        calls: std::sync::Mutex<Vec<(String, Vec<LlmMessage>, Option<usize>)>>,
    }

    #[async_trait]
    impl LlmProvider for PrefixRecordingProvider {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.calls.lock().unwrap().push((
                system_prompt.unwrap_or_default().to_string(),
                messages.to_vec(),
                crate::llm::provider::cached_prefix_len(),
            ));
            let is_task = messages.last().is_some_and(|m| {
                matches!(m.content.first(), Some(ContentBlock::Text { text }) if text == TASK_CONTEXT)
            });
            let (content, stop_reason) = if is_task {
                (
                    vec![ContentBlock::tool_use(
                        format!("tu_{}", messages.len()),
                        "noop".to_string(),
                        serde_json::json!({}),
                    )],
                    StopReason::ToolUse,
                )
            } else {
                (
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    StopReason::EndTurn,
                )
            };
            Ok(LlmResponse {
                content,
                stop_reason,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    fn prefix_recording_runner() -> (Arc<PrefixRecordingProvider>, AgentRunner) {
        let provider = Arc::new(PrefixRecordingProvider::default());
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(NoopTool));
        let runner = make_runner_with_tools(provider.clone(), tools);
        (provider, runner)
    }

    fn task_with_context(text: &str) -> Vec<ContentBlock> {
        vec![
            ContentBlock::Text {
                text: TASK_CONTEXT.to_string(),
            },
            ContentBlock::Text {
                text: text.to_string(),
            },
        ]
    }

    fn as_wire(messages: &[LlmMessage]) -> serde_json::Value {
        serde_json::to_value(messages).unwrap()
    }

    #[tokio::test]
    async fn second_turn_reads_the_first_turns_prefix_and_writes_only_its_delta() {
        let (provider, runner) = prefix_recording_runner();

        runner
            .run_with_history_content(task_with_context("q1"), Vec::new())
            .await
            .unwrap();
        let prior = vec![LlmMessage::user("q1"), LlmMessage::assistant("a1")];
        runner
            .run_with_history_content(task_with_context("q2"), prior.clone())
            .await
            .unwrap();

        let calls = provider.calls.lock().unwrap();
        let declared: Vec<Option<usize>> = calls.iter().map(|c| c.2).collect();
        assert_eq!(
            declared,
            vec![None, None, Some(2), None],
            "only round 1 of the turn with history declares the replayed prefix"
        );
        assert_eq!(
            as_wire(&calls[2].1[..2]),
            as_wire(&prior),
            "the declared prefix is exactly the replayed history"
        );
        assert!(
            calls.iter().all(|c| c.0 == runner.config.system_prompt),
            "the system prompt is byte-identical on every call"
        );
    }

    #[tokio::test]
    async fn streaming_round_one_declares_the_prior_as_cached_prefix() {
        let (provider, runner) = prefix_recording_runner();
        let (tx, _rx) = mpsc::channel(64);
        let prior = vec![LlmMessage::user("q1"), LlmMessage::assistant("a1")];

        runner
            .run_with_progress_and_history(task_with_context("q2"), prior.clone(), tx, None, None)
            .await
            .unwrap();

        let calls = provider.calls.lock().unwrap();
        let declared: Vec<Option<usize>> = calls.iter().map(|c| c.2).collect();
        assert_eq!(
            declared,
            vec![Some(2), None],
            "the declaration must survive the streaming spawn and clear on round 2"
        );
        assert_eq!(as_wire(&calls[0].1[..2]), as_wire(&prior));
    }

    /// Rejects its first call as a context overflow and records the prefix
    /// declared on every call.
    #[derive(Default)]
    struct OverflowOnceProvider {
        declared: std::sync::Mutex<Vec<Option<usize>>>,
    }

    #[async_trait]
    impl LlmProvider for OverflowOnceProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let mut declared = self.declared.lock().unwrap();
            declared.push(crate::llm::provider::cached_prefix_len());
            if declared.len() == 1 {
                return Err(eyre::eyre!(
                    "context length exceeded: maximum context length is 1000 tokens"
                ));
            }
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn retry_after_emergency_compaction_caches_on_the_last_message() {
        let provider = Arc::new(OverflowOnceProvider::default());
        let runner = make_runner(provider.clone());
        let prior = vec![LlmMessage::user("q1"), LlmMessage::assistant("a1")];

        runner
            .run_with_history_content(task_with_context("q2"), prior)
            .await
            .unwrap();

        assert_eq!(
            *provider.declared.lock().unwrap(),
            vec![Some(2), None],
            "a compacted history is no longer the replayed prefix"
        );
    }

    /// Streams like [`MockStreamingProvider`] and, as a subscription provider
    /// does, resolves its payer from the task-locals and notes the call as
    /// served.
    struct PayerProbeProvider {
        inner: MockStreamingProvider,
        seen: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    #[async_trait]
    impl LlmProvider for PayerProbeProvider {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            messages: &[LlmMessage],
            tools: &[LlmToolDef],
            model: Option<&str>,
            max_tokens: Option<u32>,
            enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.inner
                .create_message(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                )
                .await
        }

        async fn create_message_stream(
            &self,
            system_prompt: Option<&str>,
            messages: &[LlmMessage],
            tools: &[LlmToolDef],
            model: Option<&str>,
            max_tokens: Option<u32>,
            enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let user = CURRENT_USER_ID.try_with(String::clone).ok();
            let billing = CURRENT_BILLING_IDENTITY.try_with(String::clone).ok();
            self.seen.lock().unwrap().push((user.clone(), billing));
            if let Some(payer) = user {
                note_served(served_by(&payer));
            }
            self.inner
                .create_message_stream(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                    tx,
                )
                .await
        }
    }

    #[tokio::test]
    async fn stream_spawn_carries_user_and_billing_identity() {
        let provider = Arc::new(PayerProbeProvider {
            inner: MockStreamingProvider::new(vec!["Hello"]),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let runner = make_runner(provider.clone());
        let (tx, _rx) = mpsc::channel(64);
        let turn_log = ServedLog::default();

        let result = CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                CURRENT_BILLING_IDENTITY.scope(
                    "system:indexing".to_string(),
                    turn_log.clone().scope(runner.run_with_progress("Hi", tx)),
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.response, "Hello");
        assert_eq!(
            *provider.seen.lock().unwrap(),
            [(
                Some("alice".to_string()),
                Some("system:indexing".to_string())
            )]
        );
        assert_eq!(
            turn_log.served(),
            Some(served_by("alice")),
            "the streamed call's served record reaches the turn's log"
        );
    }

    #[tokio::test]
    async fn test_streaming_token_usage() {
        let provider = Arc::new(MockStreamingProvider::new(vec!["Hello"]));
        let runner = make_runner(provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Hi", tx).await.unwrap();
        assert_eq!(result.total_usage.input_tokens, 10);
        assert_eq!(result.total_usage.output_tokens, 20);

        // Verify Done event has correct stats
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let done_event = events
            .iter()
            .find(|e| matches!(e, ProgressEvent::Done { .. }));
        assert!(done_event.is_some());
        if let Some(ProgressEvent::Done {
            rounds,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            tool_count,
        }) = done_event
        {
            assert_eq!(*rounds, 1);
            assert_eq!(*input_tokens, 10);
            assert_eq!(*output_tokens, 20);
            assert_eq!(*cache_read_tokens, 0);
            assert_eq!(*tool_count, 0);
        }
    }

    #[tokio::test]
    async fn streaming_loop_delivers_degraded_text_as_a_stream_segment() {
        // Tiny window → the watchdog aborts futile on round 1 (the LLM is never
        // called). The degraded message must reach the user as a stream segment
        // so the consumer's streamed-Response dedup can't swallow it.
        let provider = Arc::new(MockStreamingProvider::new(vec!["unused"]));
        let mut runner = make_runner(provider);
        runner.config.compaction.max_context_tokens = 100;
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("do the thing", tx).await.unwrap();
        assert!(result.response.contains("context window"));

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        let streamed: String = events
            .iter()
            .filter_map(|e| match e {
                ProgressEvent::StreamDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            streamed.contains("context window"),
            "degraded text must be streamed; got: {streamed:?}"
        );
    }

    // ── Canonical agent-turn event (wide log) ──────────────────────────────

    struct MockErrorProvider;

    #[async_trait]
    impl LlmProvider for MockErrorProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("simulated provider failure"))
        }
    }

    type CapturedEvent = std::collections::HashMap<String, String>;

    thread_local! {
        /// Events recorded on this thread while a capture is armed. `None`
        /// means "not capturing" — the layer is installed process-wide, so
        /// every other test in the binary passes through it and must be
        /// ignored cheaply.
        static CAPTURED_EVENTS: std::cell::RefCell<Option<Vec<CapturedEvent>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Holds the capturing subscriber's level hint at `OFF` until it is the
    /// global default; see [`install_capture_subscriber`].
    static CAPTURE_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    struct CaptureLayer;

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            CAPTURED_EVENTS.with(|slot| {
                if let Some(buf) = slot.borrow_mut().as_mut() {
                    let mut grab = FieldGrab(std::collections::HashMap::new());
                    event.record(&mut grab);
                    buf.push(grab.0);
                }
            });
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(
                match CAPTURE_LIVE.load(std::sync::atomic::Ordering::Acquire) {
                    true => tracing::level_filters::LevelFilter::TRACE,
                    false => tracing::level_filters::LevelFilter::OFF,
                },
            )
        }
    }

    /// Install the capturing subscriber once, process-wide, for this test
    /// binary, routing events to a per-thread buffer.
    ///
    /// `set_global_default` registers the dispatcher, which rebuilds every
    /// callsite's cached interest and raises the global max level, *before*
    /// it publishes the dispatcher as the global default. A callsite another
    /// test hits for the first time in that gap asks the not-yet-published
    /// global, gets `NoSubscriber`'s `Interest::never`, and caches it for the
    /// life of the process: every later `agent_turn` is then dropped before
    /// it reaches any subscriber (DW-572). So the layer's level hint stays
    /// `OFF` while it is installed, which keeps every `info!` from reaching
    /// callsite registration at all, and is raised only once the global is
    /// published, with a rebuild that computes each interest against it.
    fn install_capture_subscriber() {
        use tracing_subscriber::layer::SubscriberExt;
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(CaptureLayer),
            )
            .expect("nothing else in this test binary sets a global subscriber");
            CAPTURE_LIVE.store(true, std::sync::atomic::Ordering::Release);
            tracing::callsite::rebuild_interest_cache();
        });
    }

    struct FieldGrab(std::collections::HashMap<String, String>);
    impl tracing::field::Visit for FieldGrab {
        fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
        fn record_i64(&mut self, f: &tracing::field::Field, v: i64) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            self.0.insert(f.name().to_string(), format!("{v:?}"));
        }
    }

    /// Run `body` under the capturing subscriber and return the `agent_turn`
    /// events it emitted on this thread.
    ///
    /// `body` runs on a current-thread runtime, so everything the turn spawns
    /// stays on this thread and lands in this thread's buffer — including the
    /// runner's spawned streaming call.
    fn capture_turn_events<F: std::future::Future<Output = ()>>(body: F) -> Vec<CapturedEvent> {
        install_capture_subscriber();
        CAPTURED_EVENTS.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
        CAPTURED_EVENTS
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.get("event").is_some_and(|v| v == "agent_turn"))
            .collect()
    }

    #[test]
    fn streaming_turn_emits_one_canonical_event() {
        let turns = capture_turn_events(async {
            let runner = make_runner(Arc::new(MockSimpleProvider::new("All done here")));
            let (tx, _rx) = mpsc::channel(64);
            runner
                .run_with_progress("Summarise the report", tx)
                .await
                .unwrap();
        });
        assert_eq!(turns.len(), 1, "exactly one agent_turn event per turn");
        let e = &turns[0];
        assert_eq!(e.get("outcome").map(String::as_str), Some("ok"));
        assert_eq!(e.get("rounds").map(String::as_str), Some("1"));
        assert_eq!(e.get("agent").map(String::as_str), Some("test"));
        assert_eq!(e.get("tool_count").map(String::as_str), Some("0"));
        assert_eq!(
            e.get("task").map(String::as_str),
            Some("Summarise the report")
        );
        assert_eq!(e.get("response").map(String::as_str), Some("All done here"));
        assert!(e.contains_key("tokens_in"), "carries token counts");
        assert!(e.contains_key("duration_ms"));
        assert!(e.contains_key("trace_id"));
    }

    #[test]
    fn errored_turn_still_emits_canonical_event_with_context() {
        let turns = capture_turn_events(async {
            let runner = make_runner(Arc::new(MockErrorProvider));
            let (tx, _rx) = mpsc::channel(64);
            let result = runner.run_with_progress("Hi", tx).await;
            assert!(result.is_err(), "provider failure must surface as Err");
        });
        assert_eq!(turns.len(), 1, "a failed turn must still emit its event");
        let e = &turns[0];
        assert_eq!(e.get("outcome").map(String::as_str), Some("error"));
        assert_eq!(e.get("rounds").map(String::as_str), Some("1"));
        assert!(
            e.get("error").map(|s| !s.is_empty()).unwrap_or(false),
            "error message is captured"
        );
    }

    #[test]
    fn cancelled_turn_emits_cancelled_outcome() {
        let turns = capture_turn_events(async {
            let runner = make_runner(Arc::new(MockSimpleProvider::new("Done")));
            let (tx, _rx) = mpsc::channel(64);
            let token = tokio_util::sync::CancellationToken::new();
            token.cancel();
            let result = runner
                .run_with_progress_and_history(
                    vec![ContentBlock::Text {
                        text: "Hi".to_string(),
                    }],
                    Vec::new(),
                    tx,
                    Some(token),
                    None,
                )
                .await;
            assert!(result.is_err(), "a cancelled turn returns Err");
        });
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].get("outcome").map(String::as_str),
            Some("cancelled")
        );
    }

    #[test]
    fn test_progress_event_debug() {
        // Ensure all variants are Debug-printable
        let events = vec![
            ProgressEvent::LlmCall { round: 1 },
            ProgressEvent::StreamStart,
            ProgressEvent::StreamDelta {
                text: "hi".to_string(),
            },
            ProgressEvent::StreamEnd,
            ProgressEvent::Response {
                text: "hello".to_string(),
            },
            ProgressEvent::Done {
                rounds: 1,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                tool_count: 0,
            },
        ];
        for event in &events {
            let _ = format!("{:?}", event);
        }
    }

    // ── from_llm_name / to_llm_name tests ──────────────────────

    /// Helper: create a mock tool with a given name.
    fn mock_tool(name: &str) -> Arc<dyn Tool> {
        struct NamedTool(String);

        #[async_trait]
        impl Tool for NamedTool {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }

        Arc::new(NamedTool(name.to_string()))
    }

    fn registry_with(names: &[&str]) -> ToolRegistry {
        let reg = ToolRegistry::new();
        for name in names {
            reg.register(mock_tool(name));
        }
        reg
    }

    #[test]
    fn outbound_delivery_tools_are_classified() {
        // The channels through which an agent could self-post a duplicate.
        for name in [
            "messaging.send",
            "messaging.send_file",
            "messaging.upload_file",
            "email.send",
            "email.forward",
            "postmark.email_send",
            "postmark.email_send_batch",
            "postmark.email_send_template",
            "sendgrid.email_send",
            "sendgrid.email_send_batch",
            "browser.upload_file",
        ] {
            assert!(is_outbound_delivery_tool(name), "{name} should be outbound");
        }
        // Read/context tools a monitor turn still needs (e.g. resolving the
        // poster's name) must be kept.
        for name in [
            "messaging.read",
            "messaging.read_thread",
            "messaging.member_info",
            "messaging.channel_info",
            "email.inbox",
            "browser.snapshot",
            "data.query",
            "memory",
        ] {
            assert!(!is_outbound_delivery_tool(name), "{name} should be kept");
        }
    }

    #[test]
    fn without_delivery_tools_strips_only_outbound() {
        let reg = registry_with(&[
            "messaging.send",
            "messaging.send_file",
            "messaging.read",
            "messaging.member_info",
            "data.query",
        ]);
        let before = reg.len();
        let filtered = without_delivery_tools(&reg);
        // Outbound delivery gone; read/context tools survive.
        assert!(!filtered.has("messaging.send"));
        assert!(!filtered.has("messaging.send_file"));
        assert!(filtered.has("messaging.read"));
        assert!(filtered.has("messaging.member_info"));
        assert!(filtered.has("data.query"));
        // The shared source registry is untouched — clone_all is non-destructive.
        assert_eq!(reg.len(), before);
        assert!(reg.has("messaging.send"));
    }

    #[tokio::test]
    async fn delivery_tools_suppressed_tracks_scope() {
        // Default (no scope) is the everyday interactive turn: nothing stripped.
        assert!(!delivery_tools_suppressed());
        // Inside the scope a monitor sets, suppression is active.
        SUPPRESS_DELIVERY_TOOLS
            .scope(true, async {
                assert!(delivery_tools_suppressed());
            })
            .await;
        // …and it falls back to false once the scope ends.
        assert!(!delivery_tools_suppressed());
    }

    #[tokio::test]
    async fn soft_deadline_times_out_runaway_tool() {
        // A tool that overruns its soft deadline must not freeze the turn: the
        // helper returns a structured failure (not an error) so the loop carries
        // on and the model is told to narrow the request.
        struct SlowTool;
        #[async_trait]
        impl Tool for SlowTool {
            fn name(&self) -> &str {
                "slow.tool"
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn soft_deadline(&self) -> Option<std::time::Duration> {
                Some(std::time::Duration::from_millis(20))
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                // Far longer than the deadline; dropped at timeout, so the test
                // completes in ~20ms rather than waiting this out.
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("late"),
                    0,
                ))
            }
        }

        let tool: Arc<dyn Tool> = Arc::new(SlowTool);
        let result = execute_with_deadline(&tool, serde_json::json!({}))
            .await
            .expect("helper returns Ok even on timeout");
        assert!(!result.success, "a timed-out tool must report failure");
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("time budget"),
            "timeout error should explain the budget: {err}"
        );
    }

    #[tokio::test]
    async fn no_deadline_tool_runs_to_completion() {
        // The default (None) deadline leaves a tool's behaviour untouched.
        let tool = mock_tool("plain.tool");
        let result = execute_with_deadline(&tool, serde_json::json!({}))
            .await
            .expect("ok");
        assert!(result.success);
    }

    /// A tool that sleeps past the check-in interval, as `coding.session` does
    /// for twenty minutes.
    struct UnboundedTool(std::time::Duration);
    #[async_trait]
    impl Tool for UnboundedTool {
        fn name(&self) -> &str {
            "coding.session"
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            tokio::time::sleep(self.0).await;
            Ok(crate::tools::ToolResult::success(
                serde_json::json!("ok"),
                0,
            ))
        }
    }

    /// Collect whatever the runner emitted while `fut` ran, with the turn-scoped
    /// task-locals in place — the same scoping the streaming loop applies around
    /// a spawned tool call.
    async fn events_during<F, T>(fut: F) -> (T, Vec<ProgressEvent>)
    where
        F: std::future::Future<Output = T>,
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let clock = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let out = LAST_USER_COMM
            .scope(clock, CURRENT_PROGRESS_TX.scope(tx, fut))
            .await;
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        (out, events)
    }

    fn check_in_notes(events: &[ProgressEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ProgressEvent::CheckIn { note } => Some(note.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The defect this exists to fix: one `coding.session` is one round, so the
    /// loop's round-boundary check-in never fires and no Pause button reaches
    /// the human. The heartbeat has to come from inside the tool wrapper — and
    /// it only works if the task-local is actually in scope at the call site,
    /// which is what this asserts.
    #[tokio::test(start_paused = true)]
    async fn a_long_tool_call_emits_its_own_check_in() {
        let tool: Arc<dyn Tool> = Arc::new(UnboundedTool(
            CHECK_IN_INTERVAL * 2 + Duration::from_secs(5),
        ));
        let (result, events) =
            events_during(execute_with_deadline(&tool, serde_json::json!({}))).await;

        assert!(result.expect("tool completes").success);
        let notes = check_in_notes(&events);
        assert_eq!(notes.len(), 2, "one per elapsed interval: {notes:?}");
        assert!(notes[0].contains("coding.session"), "{}", notes[0]);
        assert!(
            notes[0].contains("Pause"),
            "the point is the Pause affordance: {}",
            notes[0]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_quick_tool_call_says_nothing() {
        let tool: Arc<dyn Tool> = Arc::new(UnboundedTool(Duration::from_secs(1)));
        let (_, events) = events_during(execute_with_deadline(&tool, serde_json::json!({}))).await;
        assert!(check_in_notes(&events).is_empty());
    }

    /// A deadlined tool cannot outlive one interval — `Tool::soft_deadline`'s
    /// contract requires the deadline to be shorter — so the two mechanisms
    /// compose instead of shadowing: the structured failure still comes back.
    #[tokio::test(start_paused = true)]
    async fn a_deadlined_tool_still_reports_its_overrun() {
        let tool: Arc<dyn Tool> = Arc::new(SlowDeadlinedTool);
        let (result, events) =
            events_during(execute_with_deadline(&tool, serde_json::json!({}))).await;
        let result = result.expect("helper returns Ok even on timeout");
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("time budget"));
        assert!(
            check_in_notes(&events).is_empty(),
            "a deadline shorter than the interval can never reach a heartbeat"
        );
    }

    struct SlowDeadlinedTool;
    #[async_trait]
    impl Tool for SlowDeadlinedTool {
        fn name(&self) -> &str {
            "slow.deadlined"
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn soft_deadline(&self) -> Option<std::time::Duration> {
            Some(CHECK_IN_INTERVAL / 2)
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            tokio::time::sleep(CHECK_IN_INTERVAL * 4).await;
            Ok(crate::tools::ToolResult::success(
                serde_json::json!("late"),
                0,
            ))
        }
    }

    /// Without a turn in scope — the non-streaming loop, which has no progress
    /// sink at all — the wrapper must be inert rather than panicking on the
    /// missing task-local.
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_is_inert_without_a_turn_in_scope() {
        let tool: Arc<dyn Tool> = Arc::new(UnboundedTool(CHECK_IN_INTERVAL * 2));
        let result = execute_with_deadline(&tool, serde_json::json!({}))
            .await
            .expect("tool completes");
        assert!(result.success);
    }

    /// The shared clock is what stops the loop double-posting the moment a tool
    /// that just heartbeated returns.
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_defers_the_next_check_in() {
        let clock = Arc::new(Mutex::new(
            tokio::time::Instant::now() - CHECK_IN_INTERVAL * 2,
        ));
        LAST_USER_COMM
            .scope(Arc::clone(&clock), async {
                assert_eq!(check_in_due_in(), Duration::ZERO, "overdue before");
                mark_user_comm();
                assert!(
                    check_in_due_in() > Duration::from_secs(1),
                    "the clock must have moved, or the loop posts a second card"
                );
            })
            .await;
    }

    /// If a tick left the clock untouched the next one would be due
    /// immediately, spinning this loop hot for the rest of the tool call. That
    /// is reachable in production: the progress consumer can exit — closing the
    /// channel — while a twenty-minute `coding.session` is still running.
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_that_cannot_be_delivered_still_yields() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx); // the consumer has gone; every send now fails
        let clock = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let tool: Arc<dyn Tool> = Arc::new(UnboundedTool(CHECK_IN_INTERVAL * 3));

        let spun = LAST_USER_COMM
            .scope(
                Arc::clone(&clock),
                CURRENT_PROGRESS_TX.scope(tx, async {
                    let started = tokio::time::Instant::now();
                    let result = execute_with_deadline(&tool, serde_json::json!({})).await;
                    assert!(result.expect("tool completes").success);
                    started.elapsed()
                }),
            )
            .await;

        // Paused-clock time only advances when every task is idle, so a busy
        // loop would hang the test rather than reach here; the elapsed check
        // pins that the sleeps were real intervals, not zero-length spins.
        assert!(spun >= CHECK_IN_INTERVAL * 3, "elapsed {spun:?}");
    }

    #[test]
    fn to_llm_name_converts_dots() {
        assert_eq!(to_llm_name("messaging.send"), "messaging_send");
        assert_eq!(
            to_llm_name("messaging.upload_file"),
            "messaging_upload_file"
        );
    }

    #[test]
    fn from_llm_name_simple() {
        let reg = registry_with(&["messaging.send", "messaging.read"]);
        assert_eq!(from_llm_name("messaging_send", &reg), "messaging.send");
        assert_eq!(from_llm_name("messaging_read", &reg), "messaging.read");
    }

    #[test]
    fn from_llm_name_with_underscores_in_original() {
        let reg = registry_with(&[
            "messaging.upload_file",
            "messaging.list_pins",
            "messaging.read_thread",
            "messaging.member_info",
            "messaging.emoji_list",
        ]);
        assert_eq!(
            from_llm_name("messaging_upload_file", &reg),
            "messaging.upload_file"
        );
        assert_eq!(
            from_llm_name("messaging_list_pins", &reg),
            "messaging.list_pins"
        );
        assert_eq!(
            from_llm_name("messaging_read_thread", &reg),
            "messaging.read_thread"
        );
        assert_eq!(
            from_llm_name("messaging_member_info", &reg),
            "messaging.member_info"
        );
        assert_eq!(
            from_llm_name("messaging_emoji_list", &reg),
            "messaging.emoji_list"
        );
    }

    #[test]
    fn from_llm_name_direct_match() {
        let reg = registry_with(&["plain_tool"]);
        // If the name exists directly in the registry, return it as-is
        assert_eq!(from_llm_name("plain_tool", &reg), "plain_tool");
    }

    #[test]
    fn from_llm_name_unknown_returns_original() {
        let reg = registry_with(&["messaging.send"]);
        assert_eq!(from_llm_name("unknown_tool", &reg), "unknown_tool");
    }

    /// Regression for ENG-102 sub-issue 2: `google.sheets.batch_update` is a
    /// 3-segment dotted name; its LLM form `google_sheets_batch_update` was
    /// not recoverable by the old single-position substitution heuristic.
    #[test]
    fn from_llm_name_three_segment_name() {
        let reg = registry_with(&[
            "google.sheets.batch_update",
            "google.sheets.update",
            "google.sheets.append",
        ]);
        assert_eq!(
            from_llm_name("google_sheets_batch_update", &reg),
            "google.sheets.batch_update"
        );
        assert_eq!(
            from_llm_name("google_sheets_update", &reg),
            "google.sheets.update"
        );
        assert_eq!(
            from_llm_name("google_sheets_append", &reg),
            "google.sheets.append"
        );
    }

    /// Property: every registered tool name must round-trip through the
    /// LLM-name encoding and back unchanged.
    #[test]
    fn from_llm_name_round_trips_for_typical_names() {
        let reg = registry_with(&[
            "messaging.send",
            "messaging.upload_file",
            "google.sheets.batch_update",
            "google.sheets.update",
            "google.drive.list_files",
            "plain_tool",
        ]);
        for name in reg.names() {
            let llm = to_llm_name(&name);
            assert_eq!(
                from_llm_name(&llm, &reg),
                name,
                "round-trip failed for `{name}` (llm form: `{llm}`)"
            );
        }
    }

    // ── action-dispatch name repair ────────────────────────────

    /// A tool that dispatches on an `action` enum — the shape (`notes`,
    /// `memory`) a model flattens into a single `tool_action` name.
    fn action_tool(name: &str, actions: &[&str]) -> Arc<dyn Tool> {
        struct ActionTool(String, Vec<String>);

        #[async_trait]
        impl Tool for ActionTool {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                "action-dispatch mock"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "action": { "type": "string", "enum": self.1 } }
                })
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }

        Arc::new(ActionTool(
            name.to_string(),
            actions.iter().map(|a| a.to_string()).collect(),
        ))
    }

    fn notes_registry() -> ToolRegistry {
        let reg = ToolRegistry::new();
        reg.register(action_tool(
            "notes",
            &["write_human", "write_daily", "list_all_tasks"],
        ));
        reg
    }

    /// The production failure: `notes_write_human` executed as
    /// `notes(action="write_human")` instead of dropping the write.
    #[test]
    fn flattened_action_name_is_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!({ "content": "a moment worth keeping" });
        let resolved = resolve_llm_tool_call("notes_write_human", &input, &reg);
        assert_eq!(resolved.name, "notes");
        assert_eq!(resolved.repaired_action.as_deref(), Some("write_human"));
        assert_eq!(resolved.input["action"], "write_human");
        assert_eq!(resolved.input["content"], "a moment worth keeping");
    }

    /// The action is the FULL remainder, underscores and all.
    #[test]
    fn flattened_action_keeps_multi_word_actions_whole() {
        let reg = notes_registry();
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("notes_list_all_tasks", &input, &reg);
        assert_eq!(resolved.name, "notes");
        assert_eq!(resolved.repaired_action.as_deref(), Some("list_all_tasks"));
    }

    /// Models also emit the dotted form; one rule covers both.
    #[test]
    fn dotted_flattened_action_name_is_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("notes.write_human", &input, &reg);
        assert_eq!(resolved.name, "notes");
        assert_eq!(resolved.repaired_action.as_deref(), Some("write_human"));
    }

    /// A missing input object is still a valid call — the action alone suffices.
    #[test]
    fn flattened_action_repairs_null_input() {
        let reg = notes_registry();
        let input = serde_json::Value::Null;
        let resolved = resolve_llm_tool_call("notes_write_daily", &input, &reg);
        assert_eq!(resolved.name, "notes");
        assert_eq!(resolved.input["action"], "write_daily");
    }

    /// A registered tool always wins; the rewrite only ever fires on a miss.
    #[test]
    fn registered_name_is_never_rewritten() {
        let reg = notes_registry();
        reg.register(mock_tool("notes.write_human"));
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("notes_write_human", &input, &reg);
        assert_eq!(resolved.name, "notes.write_human");
        assert!(resolved.repaired_action.is_none());
        assert!(resolved.input.get("action").is_none());
    }

    /// The tool's own schema is the authority: an action it doesn't declare is
    /// not a call it can serve.
    #[test]
    fn undeclared_action_is_not_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("notes_delete_everything", &input, &reg);
        assert_eq!(resolved.name, "notes_delete_everything");
        assert!(resolved.repaired_action.is_none());
    }

    /// A tool with no `action` enum is not an action-dispatch tool.
    #[test]
    fn non_action_tool_is_not_repaired() {
        let reg = registry_with(&["messaging.send"]);
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("messaging_send_urgently", &input, &reg);
        assert_eq!(resolved.name, "messaging_send_urgently");
        assert!(resolved.repaired_action.is_none());
    }

    /// Name says one action, argument says another: ambiguous, so no guess.
    #[test]
    fn conflicting_explicit_action_is_not_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!({ "action": "write_daily" });
        let resolved = resolve_llm_tool_call("notes_write_human", &input, &reg);
        assert_eq!(resolved.name, "notes_write_human");
        assert!(resolved.repaired_action.is_none());
    }

    /// A redundant but agreeing action is not a conflict.
    #[test]
    fn agreeing_explicit_action_is_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!({ "action": "write_human" });
        let resolved = resolve_llm_tool_call("notes_write_human", &input, &reg);
        assert_eq!(resolved.name, "notes");
        assert_eq!(resolved.repaired_action.as_deref(), Some("write_human"));
    }

    /// A dotted registry name (`code.git`) is the common real shape after
    /// `notes`/`memory`: the prefix must resolve through the registry scan, not
    /// just by direct lookup.
    #[test]
    fn dotted_registry_prefix_is_repaired() {
        let reg = ToolRegistry::new();
        reg.register(action_tool("code.git", &["status", "commit"]));
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("code_git_status", &input, &reg);
        assert_eq!(resolved.name, "code.git");
        assert_eq!(resolved.repaired_action.as_deref(), Some("status"));
        assert_eq!(resolved.input["action"], "status");
    }

    /// Two tools could claim the same name — fail safe rather than pick one.
    #[test]
    fn ambiguous_split_is_not_repaired() {
        let reg = ToolRegistry::new();
        reg.register(action_tool("a", &["b_c"]));
        reg.register(action_tool("a.b", &["c"]));
        let input = serde_json::json!({});
        let resolved = resolve_llm_tool_call("a_b_c", &input, &reg);
        assert_eq!(resolved.name, "a_b_c");
        assert!(resolved.repaired_action.is_none());
    }

    /// A non-object input can't carry an action, so it can't be repaired.
    #[test]
    fn non_object_input_is_not_repaired() {
        let reg = notes_registry();
        let input = serde_json::json!("just a string");
        let resolved = resolve_llm_tool_call("notes_write_human", &input, &reg);
        assert_eq!(resolved.name, "notes_write_human");
        assert!(resolved.repaired_action.is_none());
    }

    /// An unrepairable name still resolves to itself, and borrows its input —
    /// the common path allocates nothing.
    #[test]
    fn unknown_name_passes_through_borrowed() {
        let reg = notes_registry();
        let input = serde_json::json!({ "x": 1 });
        let resolved = resolve_llm_tool_call("invented_tool", &input, &reg);
        assert_eq!(resolved.name, "invented_tool");
        assert!(matches!(resolved.input, Cow::Borrowed(_)));
    }

    // ── tool-not-found suggestions ─────────────────────────────

    /// Emits one native tool call under `llm_tool_name`, then finishes.
    struct SingleToolCallProvider {
        llm_tool_name: String,
        input: serde_json::Value,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl SingleToolCallProvider {
        fn new(llm_tool_name: &str, input: serde_json::Value) -> Self {
            Self {
                llm_tool_name: llm_tool_name.to_string(),
                input,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for SingleToolCallProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let (content, stop_reason) = match first {
                true => (
                    vec![ContentBlock::tool_use(
                        "tu_1".to_string(),
                        self.llm_tool_name.clone(),
                        self.input.clone(),
                    )],
                    StopReason::ToolUse,
                ),
                false => (
                    vec![ContentBlock::Text {
                        text: "written".to_string(),
                    }],
                    StopReason::EndTurn,
                ),
            };
            Ok(LlmResponse {
                content,
                stop_reason,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    /// Records the input each call receives, so a test can prove the injected
    /// action reached the tool itself.
    struct RecordingNotesTool(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);

    #[async_trait]
    impl Tool for RecordingNotesTool {
        fn name(&self) -> &str {
            "notes"
        }
        fn description(&self) -> &str {
            "notes"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "action": { "type": "string", "enum": ["write_human"] } }
            })
        }
        async fn execute(&self, input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            self.0.lock().unwrap().push(input);
            Ok(crate::tools::ToolResult::success(
                serde_json::json!("stored"),
                0,
            ))
        }
    }

    /// End-to-end: the repair must land BEFORE execution, and the turn must be
    /// recorded under the real tool name — not the flattened one.
    #[tokio::test]
    async fn flattened_action_call_executes_under_the_real_tool_name() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingNotesTool(Arc::clone(&seen))));
        let runner = make_runner_with_tools(
            Arc::new(SingleToolCallProvider::new(
                "notes_write_human",
                serde_json::json!({ "content": "a moment worth keeping" }),
            )),
            tools,
        );

        let result = runner.run("Remember this").await.unwrap();

        assert_eq!(result.response, "written");
        assert_eq!(result.tool_invocations.len(), 1);
        let invocation = &result.tool_invocations[0];
        assert_eq!(invocation.tool_name, "notes");
        assert!(invocation.success);
        assert_eq!(
            invocation.input["action"], "write_human",
            "the RECORDED input must carry the action — the approval record and \
             the session transcript are both built from it"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the write must actually have happened");
        assert_eq!(seen[0]["action"], "write_human");
        assert_eq!(seen[0]["content"], "a moment worth keeping");
    }

    /// The deferred approval must capture the REPAIRED input. A human approves
    /// `notes(action="write_human")`; if the record kept the pre-repair input
    /// the resume would re-execute `notes` with no action — a silent no-op
    /// after an explicit human approval.
    #[tokio::test]
    async fn routed_approval_records_the_repaired_input() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingNotesTool(Arc::clone(&seen))));
        let (runner, deferred) = routed_runner(
            Arc::new(SingleToolCallProvider::new(
                "notes_write_human",
                serde_json::json!({ "content": "a moment worth keeping" }),
            )),
            tools,
            StubDecision::Routed(vec!["bob".to_string()], false),
            ask_all_policy(),
        );

        run_non_progress_in_scope(&runner, "Remember this").await;

        let deferred = deferred.lock().unwrap();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].tool_name, "notes");
        assert_eq!(deferred[0].tool_input["action"], "write_human");
        assert_eq!(deferred[0].tool_input["content"], "a moment worth keeping");
        assert!(
            seen.lock().unwrap().is_empty(),
            "a routed call must not execute before approval"
        );
    }

    /// The repaired call is gated as the tool it resolves to — a flattened name
    /// can never be used to slip past a policy that denies the real tool.
    #[tokio::test]
    async fn flattened_action_call_is_denied_under_the_real_tool_name() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingNotesTool(Arc::clone(&seen))));
        let runner = make_runner_with_tools_and_policy(
            Arc::new(SingleToolCallProvider::new(
                "notes_write_human",
                serde_json::json!({}),
            )),
            tools,
            "rules:\n  - name: deny-notes\n    scope: global\n    tool: \"notes\"\n    action: deny\n",
        );

        let result = runner.run("Remember this").await.unwrap();

        assert_eq!(result.tool_invocations.len(), 1);
        let invocation = &result.tool_invocations[0];
        assert_eq!(invocation.tool_name, "notes");
        assert_eq!(invocation.policy_decision, "deny");
        assert!(
            seen.lock().unwrap().is_empty(),
            "a denied call must never execute"
        );
    }

    #[test]
    fn not_found_offers_same_namespace_names() {
        let reg = registry_with(&["messaging.send", "messaging.read", "vault.get"]);
        let message = tool_not_found_message("messaging_broadcast", &reg);
        assert!(
            message.contains("messaging_read, messaging_send"),
            "{message}"
        );
        assert!(!message.contains("vault"), "{message}");
    }

    /// After a failed repair (an action the tool does not declare) the model
    /// needs the action list, not the bare tool name it already guessed.
    #[test]
    fn not_found_on_an_action_tool_lists_its_actions() {
        let reg = notes_registry();
        let message = tool_not_found_message("notes_delete_everything", &reg);
        assert!(message.contains("`notes` takes an `action`"), "{message}");
        assert!(message.contains("write_human"), "{message}");
        assert!(message.contains("list_all_tasks"), "{message}");
    }

    #[test]
    fn not_found_without_relatives_stays_bare() {
        let reg = registry_with(&["vault.get"]);
        let message = tool_not_found_message("invented_tool", &reg);
        assert_eq!(message, "Tool 'invented_tool' not found");
    }

    #[test]
    fn not_found_suggestions_are_capped() {
        let reg = registry_with(&[
            "jira.a", "jira.b", "jira.c", "jira.d", "jira.e", "jira.f", "jira.g",
        ]);
        let message = tool_not_found_message("jira_zzz", &reg);
        assert_eq!(
            message.matches("jira_").count(),
            TOOL_NOT_FOUND_SUGGESTIONS + 1,
            "{message}"
        );
    }

    /// Records every call's prompt and can hold the first call open until the
    /// test releases it, so injection timing is deterministic.
    struct RecordingProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        seen: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<&str>, gated_first_call: bool) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into_iter().map(String::from).collect()),
                seen: std::sync::Mutex::new(Vec::new()),
                started: Arc::new(tokio::sync::Semaphore::new(0)),
                release: Arc::new(tokio::sync::Semaphore::new(if gated_first_call {
                    0
                } else {
                    usize::MAX >> 4
                })),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.started.add_permits(1);
            let permit = self.release.acquire().await.expect("release semaphore");
            permit.forget();
            let text = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than responses provided");
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 15,
                    ..Default::default()
                },
                model: "mock-recording".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn pre_run_injected_followup_absorbed_in_round_one() {
        let provider = Arc::new(RecordingProvider::new(vec!["final answer"], false));
        let runner = make_runner(provider.clone());
        let (progress_tx, _progress_rx) = mpsc::channel(64);
        let (inject_tx, inject_rx) = mpsc::channel(8);

        // Injected before the run starts — the coalesced-Slack-follow-up case.
        inject_tx
            .send(InjectedMessage::UserMessage {
                text: "also check the logs".into(),
                user: "u1".into(),
                external_id: Some("167.001".into()),
            })
            .await
            .unwrap();

        let result = runner
            .run_with_progress_and_history(
                vec![ContentBlock::Text {
                    text: "do the thing".into(),
                }],
                Vec::new(),
                progress_tx,
                None,
                Some(inject_rx),
            )
            .await
            .unwrap();

        assert_eq!(result.rounds, 1, "follow-up must land in round 1's prompt");
        assert_eq!(
            result.injected_user_messages,
            vec![InjectedUserMessage {
                user: "u1".into(),
                text: "also check the logs".into(),
                external_id: Some("167.001".into()),
            }]
        );
        let seen = provider.seen.lock().unwrap();
        let round1_texts: Vec<String> = seen[0]
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            round1_texts
                .iter()
                .any(|t| t.contains("[FOLLOW-UP") && t.contains("also check the logs")),
            "round 1 prompt must already contain the follow-up: {round1_texts:?}"
        );
    }

    #[tokio::test]
    async fn followup_during_final_round_gets_one_more_round() {
        let provider = Arc::new(RecordingProvider::new(
            vec!["draft answer", "revised answer"],
            true,
        ));
        let runner = make_runner(provider.clone());
        let (progress_tx, _progress_rx) = mpsc::channel(64);
        let (inject_tx, inject_rx) = mpsc::channel(8);

        let started = Arc::clone(&provider.started);
        let release = Arc::clone(&provider.release);
        let run = tokio::spawn({
            let runner = runner;
            async move {
                runner
                    .run_with_progress_and_history(
                        vec![ContentBlock::Text {
                            text: "do the thing".into(),
                        }],
                        Vec::new(),
                        progress_tx,
                        None,
                        Some(inject_rx),
                    )
                    .await
            }
        });

        // Wait until round 1's LLM call is in flight, inject, then let the
        // call complete — the follow-up arrives too late for round 1's drain.
        started.acquire().await.unwrap().forget();
        inject_tx
            .send(InjectedMessage::UserMessage {
                text: "one more thing".into(),
                user: "u1".into(),
                external_id: None,
            })
            .await
            .unwrap();
        release.add_permits(2);

        let result = run.await.unwrap().unwrap();
        assert_eq!(
            result.rounds, 2,
            "a follow-up landing during the final LLM call earns one more round"
        );
        assert_eq!(result.response, "revised answer");
        assert_eq!(
            result
                .injected_user_messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["one more thing"]
        );
        // Round 2's prompt carries the draft answer then the follow-up, in order.
        let seen = provider.seen.lock().unwrap();
        let round2_texts: Vec<(Role, String)> = seen[1]
            .iter()
            .flat_map(|m| m.content.iter().map(move |b| (m.role.clone(), b)))
            .filter_map(|(role, b)| match b {
                ContentBlock::Text { text } => Some((role, text.clone())),
                _ => None,
            })
            .collect();
        let draft_pos = round2_texts
            .iter()
            .position(|(role, t)| *role == Role::Assistant && t == "draft answer")
            .expect("draft answer must be in round 2's prompt");
        let follow_pos = round2_texts
            .iter()
            .position(|(role, t)| *role == Role::User && t.contains("one more thing"))
            .expect("follow-up must be in round 2's prompt");
        assert!(
            draft_pos < follow_pos,
            "follow-up must read as a reply to the draft"
        );
    }

    #[test]
    fn injected_message_stop_phrases() {
        let msgs = vec![InjectedMessage::UserMessage {
            text: "stop".into(),
            user: "u1".into(),
            external_id: None,
        }];
        let (texts, should_stop) = AgentRunner::process_injected_messages(&msgs);
        assert!(texts.is_empty());
        assert!(should_stop);
    }

    #[test]
    fn injected_message_hold_on() {
        let msgs = vec![InjectedMessage::UserMessage {
            text: "Hold on a sec".into(),
            user: "u1".into(),
            external_id: None,
        }];
        let (texts, should_stop) = AgentRunner::process_injected_messages(&msgs);
        assert!(texts.is_empty());
        assert!(should_stop);
    }

    #[test]
    fn injected_message_regular_followup() {
        let msgs = vec![InjectedMessage::UserMessage {
            text: "Also consider performance".into(),
            user: "u1".into(),
            external_id: None,
        }];
        let (follow_ups, should_stop) = AgentRunner::process_injected_messages(&msgs);
        assert_eq!(
            follow_ups,
            vec![InjectedUserMessage {
                user: "u1".into(),
                text: "Also consider performance".into(),
                external_id: None,
            }]
        );
        assert!(!should_stop);
    }

    #[test]
    fn injected_message_mixed() {
        let msgs = vec![
            InjectedMessage::UserMessage {
                text: "focus on tests too".into(),
                user: "u1".into(),
                external_id: None,
            },
            InjectedMessage::SteerStop {
                text: "stop".into(),
                user: "u1".into(),
            },
        ];
        let (follow_ups, should_stop) = AgentRunner::process_injected_messages(&msgs);
        assert_eq!(
            follow_ups
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["focus on tests too"]
        );
        assert!(should_stop);
    }

    #[test]
    fn nuclear_compact_preserves_last_user_message() {
        let mut messages = vec![
            LlmMessage::user("First message"),
            LlmMessage::assistant("Response 1"),
            LlmMessage::user("Second message"),
            LlmMessage::assistant("Response 2"),
            LlmMessage::user("Last important message"),
        ];
        let tokens = vec![100; 5];
        let result = AgentRunner::nuclear_compact(
            &mut messages,
            &tokens,
            1000,
            OverflowCause::HistoryGrowth,
        );
        assert!(result);
        assert_eq!(messages.len(), 3);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("SEVERE CONTEXT LOSS"))
        );
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);
        if let ContentBlock::Text { text } = &messages[2].content[0] {
            assert!(text.contains("Last important message"));
        }
    }

    #[test]
    fn nuclear_compact_tool_spam_wording_does_not_claim_lost_conversation() {
        let mut messages = vec![
            LlmMessage::user("Morning"),
            LlmMessage::assistant("Morning!"),
            LlmMessage::user("Can you check the status?"),
        ];
        let tokens = vec![10, 10, 10];
        AgentRunner::nuclear_compact(&mut messages, &tokens, 1000, OverflowCause::ToolSpam);
        let note = match &messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text note"),
        };
        assert!(
            !note.contains("SEVERE CONTEXT LOSS"),
            "tool-spam must not parrot history-loss wording"
        );
        assert!(
            !note.contains("Earlier context has been lost"),
            "tool-spam must not claim conversation was lost"
        );
        assert!(
            note.contains("tool"),
            "tool-spam wording must mention the tool"
        );
    }

    #[test]
    fn nuclear_compact_no_user_messages() {
        let mut messages = vec![LlmMessage::assistant("Response only")];
        let tokens = vec![100];
        let result = AgentRunner::nuclear_compact(
            &mut messages,
            &tokens,
            1000,
            OverflowCause::HistoryGrowth,
        );
        assert!(result);
        assert_eq!(messages.len(), 3);
        if let ContentBlock::Text { text } = &messages[2].content[0] {
            assert!(text.contains("repeat their request"));
        }
    }

    #[test]
    fn tokenize_tool_outputs_preserves_shape_and_order() {
        // No session key is active in test scope, so tokenize_with_session_context
        // returns its input unchanged. We're verifying the helper's tuple-shape
        // mapping and ordering — the actual credential redaction is exercised
        // by `workforce_common::redact` tests.
        let raw = vec![
            (
                "id-1".to_string(),
                "code.search".to_string(),
                "hello world".to_string(),
                false,
            ),
            (
                "id-2".to_string(),
                "settings".to_string(),
                "config dump".to_string(),
                true,
            ),
        ];
        let out = tokenize_tool_outputs(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ("id-1".to_string(), "hello world".to_string(), false)
        );
        assert_eq!(
            out[1],
            ("id-2".to_string(), "config dump".to_string(), true)
        );
    }

    #[test]
    fn classify_overflow_tool_spam_when_last_tool_result_dominates() {
        let small_text = "x".repeat(40);
        let huge_tool = "y".repeat(40_000);
        let messages = vec![
            LlmMessage::user(&small_text),
            LlmMessage::assistant(&small_text),
            LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: huge_tool.clone(),
                    is_error: None,
                    loaded: None,
                }],
            },
        ];
        let tokens: Vec<u32> = messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => estimate_tokens(text),
                        ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
                        _ => 0,
                    })
                    .sum()
            })
            .collect();
        assert_eq!(
            AgentRunner::classify_overflow(&messages, &tokens),
            OverflowCause::ToolSpam
        );
    }

    #[test]
    fn classify_overflow_history_growth_when_tokens_are_distributed() {
        let chunk = "x".repeat(40_000);
        let messages: Vec<_> = (0..6)
            .map(|i| {
                if i % 2 == 0 {
                    LlmMessage::user(&chunk)
                } else {
                    LlmMessage::assistant(&chunk)
                }
            })
            .collect();
        let tokens: Vec<u32> = messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => estimate_tokens(text),
                        _ => 0,
                    })
                    .sum()
            })
            .collect();
        assert_eq!(
            AgentRunner::classify_overflow(&messages, &tokens),
            OverflowCause::HistoryGrowth
        );
    }

    #[test]
    fn truncate_message_respects_char_boundary() {
        let mut msg = LlmMessage::user("Hello \u{1f30d} world! This is a longer message.");
        // Truncate to 10 chars — "\u{1f30d}" is 4 bytes, so boundary matters
        AgentRunner::truncate_message(&mut msg, 10);
        if let ContentBlock::Text { text } = &msg.content[0] {
            assert!(text.len() <= 30); // 10 + "... [truncated]"
            assert!(text.ends_with("... [truncated]"));
        }
    }

    #[test]
    fn truncate_message_replaces_images() {
        let mut msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "Look at this:".to_string(),
                },
                ContentBlock::Image {
                    source: crate::llm::types::ImageSource {
                        source_type: "base64".to_string(),
                        media_type: "image/png".to_string(),
                        data: "iVBORw0KGgo=".to_string(),
                    },
                },
            ],
        };
        AgentRunner::truncate_message(&mut msg, 100);
        assert!(
            matches!(&msg.content[1], ContentBlock::Text { text } if text.contains("image removed"))
        );
    }

    #[test]
    fn emergency_compact_never_returns_false() {
        // Create messages where front 2 are huge and back messages are huge
        let huge_text = "x".repeat(100_000);
        let mut messages = vec![
            LlmMessage::user(&huge_text),
            LlmMessage::assistant(&huge_text),
            LlmMessage::user(&huge_text),
            LlmMessage::assistant(&huge_text),
            LlmMessage::user("Small last message"),
        ];
        let result = AgentRunner::emergency_compact_messages(
            &mut messages,
            1000,
            ReservedContext::default(),
        );
        assert!(result);
        // Messages should have been compacted
        assert!(messages.len() < 5);
    }

    /// The ENG-101 failure mode. One oversized recent message (a 100K-char
    /// attachment dump) must be shrunk rather than allowed to survive as part
    /// of the "recent" tail while the turn's actual findings are evicted to
    /// make room for it. Losing the findings is what sent the agent back to
    /// re-discovering the same file, round after round.
    #[test]
    fn emergency_compact_shrinks_the_hog_instead_of_dropping_findings() {
        let mut messages = vec![LlmMessage::user("task: diff the yml and open a PR")];
        // 12 small messages standing in for the turn's accumulated findings.
        for i in 0..12 {
            messages.push(LlmMessage::assistant(format!(
                "finding {i}: the file lives at violations_and_warnings.yml"
            )));
        }
        // …then one enormous recent message, as an attachment dump would be.
        messages.push(LlmMessage::user("x".repeat(400_000)));
        let before = messages.len();

        assert!(AgentRunner::emergency_compact_messages(
            &mut messages,
            50_000,
            ReservedContext::default()
        ));

        // The hog is cut down…
        let total = estimate_messages_tokens(&messages);
        assert!(total <= 50_000, "still over budget: {total}");
        // …and the findings are still there, which is the whole point.
        assert_eq!(
            messages.len(),
            before,
            "no history was dropped — the oversized message was truncated instead"
        );
        let kept = messages
            .iter()
            .filter(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.starts_with("finding ")),
                )
            })
            .count();
        assert_eq!(kept, 12, "every finding survived");
    }

    /// The discriminating case for the hog pre-pass: the oversized message is
    /// in the MIDDLE, with a large tool result after it. A tail-first sweep
    /// would consume its whole budget clipping the recent tool result and leave
    /// the hog untouched — destroying the freshest evidence to spare the thing
    /// that actually overflowed.
    #[test]
    fn emergency_compact_shrinks_the_hog_not_the_freshest_tool_result() {
        let mut messages = vec![LlmMessage::user("task")];
        for i in 0..4 {
            messages.push(LlmMessage::assistant(format!("finding {i}")));
        }
        // The hog, mid-conversation: ~50K tokens, on its own over the ~42.5K
        // target. Everything else fits comfortably, so the gate must open.
        messages.push(LlmMessage::user("H".repeat(200_000)));
        // Recent, still substantial, must survive intact: ~5K tokens.
        let fresh = "F".repeat(20_000);
        messages.push(LlmMessage::assistant(fresh.clone()));

        assert!(AgentRunner::emergency_compact_messages(
            &mut messages,
            50_000,
            ReservedContext::default()
        ));

        let text_of = |m: &LlmMessage| match &m.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        };
        let hog = text_of(&messages[5]);
        assert!(
            hog.len() < 200_000 && hog.contains("[truncated to fit context window]"),
            "the hog itself was shrunk (len {})",
            hog.len()
        );
        assert_eq!(
            text_of(&messages[6]),
            fresh,
            "the freshest tool output was left byte-intact"
        );
        assert!(
            estimate_messages_tokens(&messages) <= 42_500,
            "under target"
        );
    }

    #[test]
    fn emergency_compact_subtracts_reserved_overhead() {
        // ~30K message tokens, 131K limit. Without overhead accounting the
        // target (~111K) leaves messages untouched. With ~113K reserved, the
        // available message budget collapses and trimming must occur — this is
        // the 2026-05-22 failure mode.
        let chunk = "x".repeat(40_000); // ~10K tokens each
        let mut messages = vec![
            LlmMessage::user("task"),
            LlmMessage::assistant(&chunk),
            LlmMessage::user(&chunk),
            LlmMessage::assistant(&chunk),
            LlmMessage::user("latest"),
        ];
        let reserved = ReservedContext {
            system_prompt_tokens: 31_000,
            tool_schema_tokens: 16_000,
            response_reservation_tokens: 65_536,
        };
        // Without overhead the same messages stay untouched.
        let mut untouched = messages.clone();
        assert!(!AgentRunner::emergency_compact_messages(
            &mut untouched,
            131_072,
            ReservedContext::default()
        ));
        let before = messages.len();
        assert!(AgentRunner::emergency_compact_messages(
            &mut messages,
            131_072,
            reserved
        ));
        assert!(messages.len() < before);
    }

    fn msg_of_tokens(tokens: u32) -> LlmMessage {
        LlmMessage::user("x".repeat(tokens as usize * 4))
    }

    /// Assert the API invariant Bedrock/Anthropic enforce: every `tool_use`
    /// must be answered by a `tool_result` in the *immediately following*
    /// message. Panics with the offending id otherwise.
    fn assert_no_orphan_tool_use(messages: &[LlmMessage]) {
        for (i, msg) in messages.iter().enumerate() {
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    let answered = messages
                        .get(i + 1)
                        .map(|next| {
                            next.role == Role::User
                                && next.content.iter().any(|b| {
                                    matches!(
                                        b,
                                        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                                    )
                                })
                        })
                        .unwrap_or(false);
                    assert!(
                        answered,
                        "orphan tool_use {id} at message {i} — no tool_result in the next message"
                    );
                }
            }
        }
    }

    fn assistant_tool_use(id: &str) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id.to_string(),
                "view_attachment".to_string(),
                serde_json::json!({"name": "file.pdf"}),
            )],
        }
    }

    fn tool_result_for(id: &str) -> LlmMessage {
        LlmMessage::tool_results(vec![(id.to_string(), "result body".to_string(), false)])
    }

    #[test]
    fn strip_orphaned_tool_blocks_drops_tool_use_split_from_its_result() {
        // Reproduces the emergency-compaction split: a system note is inserted
        // between an assistant `tool_use` turn and its `tool_result` turn. The
        // result still exists, so a presence-only check would keep the
        // `tool_use` and produce the Bedrock 400 ("`tool_use` ids were found
        // without `tool_result` blocks immediately after").
        let mut messages = vec![
            LlmMessage::user("read this attachment"),
            assistant_tool_use("toolu_X"),
            LlmMessage::user("[older messages dropped to fit context]"),
            tool_result_for("toolu_X"),
            LlmMessage::assistant("here is what the file says"),
        ];

        AgentRunner::strip_orphaned_tool_blocks(&mut messages);

        assert_no_orphan_tool_use(&messages);
        // The split round is dropped entirely — both the orphaned use and its
        // now-dangling result.
        assert!(!messages.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
        ))));
    }

    #[test]
    fn strip_orphaned_tool_blocks_keeps_adjacent_pairs() {
        let mut messages = vec![
            LlmMessage::user("task"),
            assistant_tool_use("toolu_Y"),
            tool_result_for("toolu_Y"),
            LlmMessage::assistant("done"),
        ];
        let before = messages.clone();
        AgentRunner::strip_orphaned_tool_blocks(&mut messages);
        assert_no_orphan_tool_use(&messages);
        assert_eq!(messages.len(), before.len());
    }

    #[test]
    fn emergency_compact_never_orphans_a_tool_use() {
        // End-to-end: a token-heavy attachment turn whose first round is a
        // `tool_use` whose result lands in the kept back-window. Emergency
        // compaction inserts its note between them; the output must still be a
        // valid request.
        let huge = "x".repeat(80_000); // ~20K tokens
        let mut messages = vec![
            LlmMessage::user("read this attachment"),
            assistant_tool_use("toolu_Z"),
            LlmMessage::tool_results(vec![("toolu_Z".to_string(), huge.clone(), false)]),
            LlmMessage::assistant(&huge),
            LlmMessage::user("and now summarise"),
            LlmMessage::assistant(&huge),
            LlmMessage::user("latest"),
        ];

        let trimmed = AgentRunner::emergency_compact_messages(
            &mut messages,
            8_000,
            ReservedContext::default(),
        );
        assert!(trimmed);
        assert_no_orphan_tool_use(&messages);
    }

    #[test]
    fn watchdog_triggers_above_urgent_threshold() {
        let messages = vec![msg_of_tokens(100_000)];
        let reserved = ReservedContext {
            system_prompt_tokens: 31_000,
            tool_schema_tokens: 16_000,
            response_reservation_tokens: 0,
        };
        assert_eq!(
            watchdog_should_compact(&messages, reserved, 131_072, 0.8, None),
            Some(147_000)
        );
    }

    #[test]
    fn watchdog_holds_fire_below_threshold() {
        let messages = vec![msg_of_tokens(50_000)];
        assert_eq!(
            watchdog_should_compact(&messages, ReservedContext::default(), 131_072, 0.8, None),
            None
        );
    }

    #[test]
    fn watchdog_uses_response_reservation() {
        let messages = vec![msg_of_tokens(90_000)];
        let small = ReservedContext {
            response_reservation_tokens: 8_192,
            ..ReservedContext::default()
        };
        let large = ReservedContext {
            response_reservation_tokens: 65_536,
            ..ReservedContext::default()
        };
        assert!(watchdog_should_compact(&messages, small, 131_072, 0.8, None).is_none());
        assert!(watchdog_should_compact(&messages, large, 131_072, 0.8, None).is_some());
    }

    #[test]
    fn observed_round_discount_subtracts_from_baseline_and_saturates() {
        let observed = ObservedRound {
            input_tokens: 110_000,
            message_len_at_observation: 1,
        };
        assert_eq!(observed.discounted(10_000).input_tokens, 100_000);
        assert_eq!(observed.discounted(200_000).input_tokens, 0);
        // A discounted observation no longer overrides a low estimate: after
        // in-flight eviction freed most of the history, the stale baseline
        // must not force an urgent compaction on its own.
        let messages = vec![msg_of_tokens(20_000)];
        assert!(watchdog_should_compact(
            &messages,
            ReservedContext::default(),
            131_072,
            0.8,
            Some(observed.discounted(90_000)),
        )
        .is_none());
    }

    #[test]
    fn watchdog_fires_on_observed_usage_the_estimate_cannot_see() {
        // A 20k-token estimate is nowhere near the 104,857 urgent limit, but
        // the provider just reported a 110k prompt for those same messages —
        // the observation must drive the prediction.
        let messages = vec![msg_of_tokens(20_000)];
        let observed = ObservedRound {
            input_tokens: 110_000,
            message_len_at_observation: 1,
        };
        assert_eq!(
            watchdog_should_compact(
                &messages,
                ReservedContext::default(),
                131_072,
                0.8,
                Some(observed),
            ),
            Some(110_000)
        );
        // Estimate-only (no observation) keeps the old behaviour.
        assert!(
            watchdog_should_compact(&messages, ReservedContext::default(), 131_072, 0.8, None)
                .is_none()
        );
    }

    #[test]
    fn observed_round_prices_only_messages_appended_since() {
        let messages = vec![
            msg_of_tokens(10_000),
            msg_of_tokens(10_000),
            msg_of_tokens(5_000),
        ];
        let observed = ObservedRound {
            input_tokens: 60_000,
            message_len_at_observation: 2,
        };
        // 60k real + the 5k appended since the observation.
        assert_eq!(observed.predicted_tokens(&messages), Some(65_000));
        // Nothing appended → the raw observation.
        let current = ObservedRound {
            input_tokens: 60_000,
            message_len_at_observation: 3,
        };
        assert_eq!(current.predicted_tokens(&messages), Some(60_000));
        // History shrank below the observation point (compaction) → stale.
        let stale = ObservedRound {
            input_tokens: 60_000,
            message_len_at_observation: 4,
        };
        assert_eq!(stale.predicted_tokens(&messages), None);
    }

    #[test]
    fn observed_round_capture_sums_cache_components_and_skips_empty() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            cache_read_input_tokens: 40_000,
            cache_creation_input_tokens: 2_000,
            ..TokenUsage::default()
        };
        let observed = ObservedRound::capture(&usage, 7).expect("non-zero usage captures");
        assert_eq!(observed.input_tokens, 43_000);
        assert_eq!(observed.message_len_at_observation, 7);
        assert!(
            ObservedRound::capture(&TokenUsage::default(), 7).is_none(),
            "a zero-usage (degraded) response must not overwrite a real sample"
        );
    }

    #[test]
    fn stale_observation_falls_back_to_estimate_only() {
        // Post-compaction history is shorter than the observation point: the
        // sample must be ignored, not treated as a live 120k prompt.
        let messages = vec![msg_of_tokens(1_000)];
        let stale = ObservedRound {
            input_tokens: 120_000,
            message_len_at_observation: 5,
        };
        assert!(watchdog_should_compact(
            &messages,
            ReservedContext::default(),
            131_072,
            0.8,
            Some(stale),
        )
        .is_none());
    }

    #[test]
    fn wind_down_text_offers_continue_only_interactively() {
        let automated = wind_down_text(false);
        let interactive = wind_down_text(true);
        assert!(!automated.contains("continue"));
        assert!(interactive.contains("If you want me to keep going, say 'continue'."));
        // The two forks now diverge on purpose: the interactive wind-down
        // demands the ANSWER and forbids tool calls (they are not executed on
        // the final interactive round), while the automated one keeps "last
        // chance to use tools" — a job's final round may legitimately act.
        assert!(
            interactive.contains("Do not call tools"),
            "interactive wind-down must forbid tools: {interactive}"
        );
        assert!(
            !interactive.contains("last chance to use tools"),
            "the tool invitation caused guaranteed overshoot on capped turns"
        );
        assert!(automated.contains("last chance to use tools"));
    }

    #[test]
    fn compaction_floor_breaker_trips_only_when_overhead_fills_window() {
        let limit = 200_000;
        let fits = ReservedContext {
            tool_schema_tokens: 150_000,
            ..ReservedContext::default()
        };
        assert!(!compaction_floor_exceeds_window(fits, limit));
        let floods = ReservedContext {
            tool_schema_tokens: 700_000,
            ..ReservedContext::default()
        };
        assert!(compaction_floor_exceeds_window(floods, limit));
        // The boundary is inclusive: exactly the window leaves no message room.
        let exact = ReservedContext {
            tool_schema_tokens: limit,
            ..ReservedContext::default()
        };
        assert!(compaction_floor_exceeds_window(exact, limit));
    }

    #[test]
    fn degraded_overflow_message_names_the_dominant_contributor() {
        let limit = 200_000;
        // Overhead fits → it's the message history → /new.
        let fits = ReservedContext {
            tool_schema_tokens: 100_000,
            ..ReservedContext::default()
        };
        assert!(degraded_overflow_message(fits, limit).contains("/new"));
        // Tools dominate the over-window overhead.
        let tools = ReservedContext {
            tool_schema_tokens: 700_000,
            system_prompt_tokens: 5_000,
            response_reservation_tokens: 8_192,
        };
        assert!(degraded_overflow_message(tools, limit).contains("tool"));
        // A large response reservation, not tools, fills a small window.
        let response = ReservedContext {
            tool_schema_tokens: 10_000,
            system_prompt_tokens: 5_000,
            response_reservation_tokens: 200_000,
        };
        let msg = degraded_overflow_message(response, limit);
        assert!(msg.contains("max_tokens"), "got: {msg}");
    }

    #[test]
    fn watchdog_aborts_turn_when_overhead_alone_fills_window() {
        let mut runner = make_runner_with_tools(
            Arc::new(MockSimpleProvider::new("ok")),
            Arc::new(ToolRegistry::new()),
        );
        // A window smaller than the fixed response reservation: no amount of
        // message compaction can free room, so the breaker must fire.
        runner.config.compaction.max_context_tokens = 100;
        let mut messages = vec![msg_of_tokens(50)];
        assert!(matches!(
            runner.run_context_watchdog(&mut messages, &OfferedTools::default(), "mock", 1, None),
            Err(CompactionFutile { .. })
        ));
    }

    #[test]
    fn watchdog_proceeds_when_overhead_fits() {
        let runner = make_runner_with_tools(
            Arc::new(MockSimpleProvider::new("ok")),
            Arc::new(ToolRegistry::new()),
        );
        // Default large window, small conversation → no compaction, no abort.
        let mut messages = vec![msg_of_tokens(50)];
        assert!(runner
            .run_context_watchdog(&mut messages, &OfferedTools::default(), "mock", 1, None)
            .is_ok());
    }

    #[test]
    fn watchdog_deflates_trim_target_when_observation_outruns_estimate() {
        let mut runner = make_runner_with_tools(
            Arc::new(MockSimpleProvider::new("ok")),
            Arc::new(ToolRegistry::new()),
        );
        runner.config.compaction.max_context_tokens = 131_072;
        // Ten messages estimating 40k — under both the 104,857 urgent limit
        // and the estimate-denominated trim target — while the provider just
        // reported a 126k prompt for them. Without deflating the target by
        // the observed/estimate ratio the compactor reads the history as
        // already small enough, trims nothing, and the next call overflows.
        let mut messages: Vec<LlmMessage> = (0..10).map(|_| msg_of_tokens(4_000)).collect();
        let observed = ObservedRound {
            input_tokens: 126_000,
            message_len_at_observation: 10,
        };
        assert!(runner
            .run_context_watchdog(
                &mut messages,
                &OfferedTools::default(),
                "mock",
                3,
                Some(observed)
            )
            .is_ok());
        let after = estimate_messages_tokens(&messages);
        assert!(
            after < 40_000,
            "observed drift must actually shrink the history: {after} tokens across {} messages",
            messages.len()
        );
    }

    #[test]
    fn estimate_block_tokens_covers_all_variants() {
        assert_eq!(
            estimate_block_tokens(&ContentBlock::Text {
                text: "x".repeat(40)
            }),
            10
        );
        assert_eq!(
            estimate_block_tokens(&ContentBlock::Thinking {
                thinking: "x".repeat(40),
                signature: None,
            }),
            10
        );
        assert_eq!(
            estimate_block_tokens(&ContentBlock::image_base64("image/png", "abc".to_string())),
            1000
        );
    }

    #[test]
    fn emergency_compact_reserved_exceeds_limit_falls_through() {
        // Degenerate: reserved overhead alone exceeds the context limit (stale
        // model registry). target_tokens saturates to 0; nuclear compaction
        // must still succeed rather than panic.
        let chunk = "x".repeat(5_000);
        let mut messages = vec![
            LlmMessage::user("task"),
            LlmMessage::assistant(&chunk),
            LlmMessage::user(&chunk),
            LlmMessage::assistant(&chunk),
            LlmMessage::user("latest request"),
        ];
        let reserved = ReservedContext {
            system_prompt_tokens: 200_000,
            tool_schema_tokens: 0,
            response_reservation_tokens: 0,
        };
        assert!(AgentRunner::emergency_compact_messages(
            &mut messages,
            131_072,
            reserved
        ));
    }

    #[test]
    fn parse_context_limit_cerebras_format() {
        // Cerebras style: "limit is 131072"
        let limit = AgentRunner::parse_context_limit(
            "Please reduce the length of the messages or completion. Current length is 138950 while limit is 131072"
        );
        assert_eq!(limit, Some(131072));

        // Anthropic/OpenAI style: "200000 maximum"
        let limit = AgentRunner::parse_context_limit("512993 tokens > 200000 maximum");
        assert_eq!(limit, Some(200000));
    }

    #[test]
    fn parse_context_limit_openrouter_format() {
        // The exact 2026-05-22 incident string.
        let limit = AgentRunner::parse_context_limit(
            "This endpoint's maximum context length is 163840 tokens. However, you \
             requested about 171273 tokens (89861 of text input, 15876 of tool input, \
             65536 in the output). Please reduce the length of either one.",
        );
        assert_eq!(limit, Some(163840));
    }

    #[test]
    fn parse_context_limit_openrouter_context_length_of_variant() {
        let limit = AgentRunner::parse_context_limit(
            "Model has a maximum context length of 200000 tokens but 210000 were supplied",
        );
        assert_eq!(limit, Some(200000));
    }

    #[test]
    fn parse_context_limit_prefers_specific_over_fallback() {
        // Contains both the specific "context length is X" and a distinct
        // trailing "Y maximum"; the specific extractor must win (163840, not 999999).
        let limit = AgentRunner::parse_context_limit(
            "maximum context length is 163840 tokens; request exceeds the 999999 maximum",
        );
        assert_eq!(limit, Some(163840));
    }

    #[test]
    fn parse_context_limit_garbage_returns_none() {
        assert_eq!(
            AgentRunner::parse_context_limit("internal server error, please retry"),
            None
        );
        assert_eq!(
            AgentRunner::parse_context_limit("the maximum was exceeded"),
            None
        );
    }

    // ── Context eviction tests (DW-418) ──────────────────────────────────

    fn make_tool_use_msg(id: &str, name: &str) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
                signature: None,
            }],
        }
    }

    fn make_tool_result_msg(tool_use_id: &str, content: &str) -> LlmMessage {
        LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error: None,
                loaded: None,
            }],
        }
    }

    #[test]
    fn eviction_preserves_keep_last() {
        let large = "x".repeat(1000);
        let mut messages = vec![
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &large),
            make_tool_use_msg("t2", "code.read"),
            make_tool_result_msg("t2", &large),
            make_tool_use_msg("t3", "code.read"),
            make_tool_result_msg("t3", &large),
            make_tool_use_msg("t4", "code.read"),
            make_tool_result_msg("t4", &large),
        ];

        let (evicted, freed) = AgentRunner::evict_stale_tool_outputs(&mut messages, 2);

        // Should evict the 2 oldest (t1, t2), keep the 2 most recent (t3, t4)
        assert_eq!(evicted, 2);
        assert!(freed > 0);

        // Verify t1 and t2 are evicted (contain marker)
        if let ContentBlock::ToolResult { content, .. } = &messages[1].content[0] {
            assert!(content.contains("[evicted"), "t1 should be evicted");
        }
        if let ContentBlock::ToolResult { content, .. } = &messages[3].content[0] {
            assert!(content.contains("[evicted"), "t2 should be evicted");
        }
        // Verify t3 and t4 are preserved
        if let ContentBlock::ToolResult { content, .. } = &messages[5].content[0] {
            assert_eq!(content.len(), 1000, "t3 should be preserved");
        }
        if let ContentBlock::ToolResult { content, .. } = &messages[7].content[0] {
            assert_eq!(content.len(), 1000, "t4 should be preserved");
        }
    }

    #[test]
    fn eviction_skips_small_outputs() {
        let small = "x".repeat(499); // below 500 char threshold
        let large = "x".repeat(1000);
        let mut messages = vec![
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &small), // small — should NOT be evicted
            make_tool_use_msg("t2", "code.read"),
            make_tool_result_msg("t2", &large),
            make_tool_use_msg("t3", "code.read"),
            make_tool_result_msg("t3", &large),
        ];

        let (evicted, _freed) = AgentRunner::evict_stale_tool_outputs(&mut messages, 1);

        // Only t2 should be evicted (t1 is too small, t3 is the most recent)
        assert_eq!(evicted, 1);
        // t1 preserved (too small)
        if let ContentBlock::ToolResult { content, .. } = &messages[1].content[0] {
            assert_eq!(content.len(), 499, "small output should be preserved");
        }
    }

    #[test]
    fn eviction_groups_by_tool_name() {
        let large = "x".repeat(1000);
        let mut messages = vec![
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &large),
            make_tool_use_msg("t2", "code.search"),
            make_tool_result_msg("t2", &large),
            make_tool_use_msg("t3", "code.read"),
            make_tool_result_msg("t3", &large),
            make_tool_use_msg("t4", "code.search"),
            make_tool_result_msg("t4", &large),
        ];

        // keep_last = 1 per tool type
        let (evicted, _freed) = AgentRunner::evict_stale_tool_outputs(&mut messages, 1);

        // Should evict t1 (older code.read) and t2 (older code.search)
        assert_eq!(evicted, 2);

        // t1 (code.read oldest) evicted
        if let ContentBlock::ToolResult { content, .. } = &messages[1].content[0] {
            assert!(content.contains("[evicted"));
        }
        // t2 (code.search oldest) evicted
        if let ContentBlock::ToolResult { content, .. } = &messages[3].content[0] {
            assert!(content.contains("[evicted"));
        }
        // t3 (code.read newest) preserved
        if let ContentBlock::ToolResult { content, .. } = &messages[5].content[0] {
            assert_eq!(content.len(), 1000);
        }
        // t4 (code.search newest) preserved
        if let ContentBlock::ToolResult { content, .. } = &messages[7].content[0] {
            assert_eq!(content.len(), 1000);
        }
    }

    #[test]
    fn eviction_returns_correct_counts() {
        let large = "x".repeat(2000);
        let mut messages = vec![
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &large),
            make_tool_use_msg("t2", "code.read"),
            make_tool_result_msg("t2", &large),
        ];

        let (evicted, freed) = AgentRunner::evict_stale_tool_outputs(&mut messages, 1);
        assert_eq!(evicted, 1);
        // freed should be original content len minus the marker len
        assert!(
            freed > 1900,
            "freed chars should be close to 2000, got {}",
            freed
        );
    }

    #[test]
    fn eviction_no_op_when_within_keep_last() {
        let large = "x".repeat(1000);
        let mut messages = vec![
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &large),
            make_tool_use_msg("t2", "code.read"),
            make_tool_result_msg("t2", &large),
        ];

        // keep_last = 3, but we only have 2 results — nothing to evict
        let (evicted, freed) = AgentRunner::evict_stale_tool_outputs(&mut messages, 3);
        assert_eq!(evicted, 0);
        assert_eq!(freed, 0);
    }

    #[test]
    fn eviction_preserves_non_tool_messages() {
        let large = "x".repeat(1000);
        let mut messages = vec![
            LlmMessage::user("Hello"),
            LlmMessage::assistant("Hi there"),
            make_tool_use_msg("t1", "code.read"),
            make_tool_result_msg("t1", &large),
            LlmMessage::user("Follow up question"),
            make_tool_use_msg("t2", "code.read"),
            make_tool_result_msg("t2", &large),
        ];

        let (evicted, _) = AgentRunner::evict_stale_tool_outputs(&mut messages, 1);
        assert_eq!(evicted, 1);

        // User and assistant messages should be untouched
        if let ContentBlock::Text { text } = &messages[0].content[0] {
            assert_eq!(text, "Hello");
        }
        if let ContentBlock::Text { text } = &messages[1].content[0] {
            assert_eq!(text, "Hi there");
        }
        if let ContentBlock::Text { text } = &messages[4].content[0] {
            assert_eq!(text, "Follow up question");
        }
    }

    // ── classify_text_tool_call (the one verdict) ────────────────────────

    fn verdict(text: &str, names: &[&str]) -> TextCallVerdict {
        classify_text_tool_call(text, &registry_with(names))
    }

    #[test]
    fn analysis_prose_citing_tools_is_an_answer() {
        // The shape that made agents bin finished answers and then rebut the
        // correction out loud: a long write-up naming the tools it ran, with
        // braces somewhere in the evidence.
        let text = r#"The real issue is an N+1 query in `Worker#agency_organisations`
(app/models/worker.rb:282-291), hit via `/keycloak_user/data`. I found it with `repo.grep`
and confirmed the timeout pattern with `datadog.logs_search` — the token mapper catches the
failure and omits the `permissions` claim, so login still succeeds. The offending block
builds `{ agency_id: id }` per registration rather than eager-loading. Shall I open an
issue?"#;
        assert!(matches!(
            verdict(text, &["repo.grep", "datadog.logs_search"]),
            TextCallVerdict::Prose
        ));
    }

    #[test]
    fn prose_describing_a_call_is_an_answer() {
        assert!(matches!(
            verdict(
                r#"Let me call messaging_send with {"channel": "test"}"#,
                &["messaging.send"]
            ),
            TextCallVerdict::Prose
        ));
    }

    #[test]
    fn plain_text_and_empty_text_are_answers() {
        assert!(matches!(
            verdict(
                "Sure, I'll send you a message right away.",
                &["messaging.send"]
            ),
            TextCallVerdict::Prose
        ));
        assert!(matches!(verdict("", &["exec.run"]), TextCallVerdict::Prose));
    }

    #[test]
    fn json_dominant_call_to_a_registered_tool_is_executable() {
        assert!(matches!(
            verdict(
                r#"I'll send that now. {"tool": "messaging.send", "input": {"channel": "D0"}}"#,
                &["messaging.send"]
            ),
            TextCallVerdict::Executable { .. }
        ));
    }

    #[test]
    fn truncated_call_shaped_json_is_malformed() {
        // Never closes, so nothing can execute it — and it is not an answer.
        assert!(matches!(
            verdict(
                r#"{"name": "invented.tool", "arguments": {"blob": "aaaaaaaaaaaaaaaaaaaa"#,
                &["vault.delete"]
            ),
            TextCallVerdict::Malformed
        ));
    }

    #[test]
    fn truncated_data_answer_is_still_an_answer() {
        // Cut off by the token cap, but its keys are not call vocabulary.
        assert!(matches!(
            verdict(
                r#"{"results": [{"id": 1, "name": "row-1"}, {"id": 2, "name": "row-2"#,
                &["vault.delete"]
            ),
            TextCallVerdict::Prose
        ));
    }

    #[test]
    fn closed_call_shaped_json_naming_an_unknown_tool_is_data() {
        // The reader asked for JSON; a name we do not hold is not a call.
        assert!(matches!(
            verdict(
                r#"{"name": "Alice", "params": {"age": 3, "note": "profile"}}"#,
                &["vault.delete"]
            ),
            TextCallVerdict::Prose
        ));
    }

    #[test]
    fn closed_call_naming_a_registered_tool_but_uncallable_is_malformed() {
        for text in [
            r#"{"name": "vault.delete", "input": "id=7"}"#,
            r#"{"tool": "vault.delete", "id": 7}"#,
        ] {
            assert!(
                matches!(verdict(text, &["vault.delete"]), TextCallVerdict::Malformed),
                "{text}"
            );
        }
    }

    // ── parse_text_tool_call (Mechanism B admission) ─────────────────────

    #[test]
    fn parse_admits_fenced_json_call() {
        let tools = registry_with(&["vault.delete"]);
        let text = "```json\n{\"name\": \"vault.delete\", \"input\": {\"id\": 7}}\n```";
        let (name, input) = parse_text_tool_call(text, &tools).expect("admitted");
        assert_eq!(name, "vault.delete");
        assert_eq!(input, serde_json::json!({"id": 7}));
    }

    #[test]
    fn parse_admits_all_name_and_input_key_combos() {
        let tools = registry_with(&["vault.delete"]);
        for (name_key, input_key) in [
            ("name", "input"),
            ("tool", "arguments"),
            ("tool_name", "params"),
        ] {
            let text = format!(r#"{{"{name_key}": "vault.delete", "{input_key}": {{"id": 1}}}}"#);
            let (name, input) = parse_text_tool_call(&text, &tools)
                .unwrap_or_else(|| panic!("{name_key}/{input_key} should be admitted"));
            assert_eq!(name, "vault.delete");
            assert_eq!(input, serde_json::json!({"id": 1}));
        }
    }

    #[test]
    fn parse_resolves_llm_underscore_name_to_registry_name() {
        let tools = registry_with(&["vault.delete"]);
        let text = r#"{"name": "vault_delete", "arguments": {"id": 2}}"#;
        let (name, _) = parse_text_tool_call(text, &tools).expect("admitted");
        assert_eq!(
            name, "vault.delete",
            "LLM-side name resolves to registry name"
        );
    }

    #[test]
    fn parse_rejects_unregistered_tool() {
        let tools = registry_with(&["vault.delete"]);
        let text = r#"{"name": "made.up", "input": {"id": 7}}"#;
        assert!(parse_text_tool_call(text, &tools).is_none());
    }

    #[test]
    fn parse_rejects_prose_dominant_embedded_example() {
        let tools = registry_with(&["vault.delete"]);
        let text = format!(
            "To delete a vault entry you would normally invoke the tool like \
             this: {} — but that is only an example for documentation purposes, \
             so nothing should actually run here. The surrounding explanation \
             deliberately dominates the JSON span.",
            r#"{"name": "vault.delete", "input": {"id": 7}}"#
        );
        assert!(
            parse_text_tool_call(&text, &tools).is_none(),
            "prose-dominant text must not be executed"
        );
    }

    #[test]
    fn parse_scanner_handles_nested_objects_and_braces_in_strings() {
        let tools = registry_with(&["vault.delete"]);
        let text = r#"{"name": "vault.delete", "input": {"query": "a { \" } b", "nested": {"x": {"y": 1}}}}"#;
        let (name, input) = parse_text_tool_call(text, &tools).expect("admitted");
        assert_eq!(name, "vault.delete");
        assert_eq!(input["query"], "a { \" } b");
        assert_eq!(input["nested"]["x"]["y"], 1);
    }

    #[test]
    fn parse_missing_input_key_defaults_only_for_sole_name_key() {
        let tools = registry_with(&["vault.delete"]);
        let (name, input) =
            parse_text_tool_call(r#"{"tool": "vault.delete"}"#, &tools).expect("admitted");
        assert_eq!(name, "vault.delete");
        assert_eq!(input, serde_json::json!({}));
        // Extra keys without an input key are not unambiguously a call.
        assert!(parse_text_tool_call(r#"{"tool": "vault.delete", "id": 7}"#, &tools).is_none());
    }

    #[test]
    fn parse_rejects_non_object_input() {
        let tools = registry_with(&["vault.delete"]);
        assert!(
            parse_text_tool_call(r#"{"name": "vault.delete", "input": "id=7"}"#, &tools).is_none()
        );
    }

    #[test]
    fn parse_rejects_truncated_json() {
        let tools = registry_with(&["vault.delete"]);
        assert!(
            parse_text_tool_call(r#"{"name": "vault.delete", "input": {"id": 7"#, &tools).is_none()
        );
    }

    #[test]
    fn stream_prefix_suspect_classification() {
        let names = vec!["vault.delete".to_string()];
        // JSON opener + tool-call key.
        assert!(stream_prefix_suspect(r#"{"name": "somethi"#, &names));
        // Fenced JSON opener + registered tool name (underscored form).
        assert!(stream_prefix_suspect("```json\n{\"vault_delete\"", &names));
        // Array opener + tool-call key.
        assert!(stream_prefix_suspect(r#"[{"tool_name": "#, &names));
        // The Anthropic tool_use block shape, hallucinated as text.
        assert!(stream_prefix_suspect(
            r#"{"type": "tool_use", "name": "vault_delete", "input": {"#,
            &names
        ));
        // Prose is never suspect, even when it mentions JSON keys.
        assert!(!stream_prefix_suspect(r#"The "name" field is set"#, &names));
        // JSON without tool-call shape is not suspect.
        assert!(!stream_prefix_suspect(
            r#"{"results": [{"id": 1}]}"#,
            &names
        ));
        // Data objects are not suspect, however many "name" fields they
        // carry — only call-shaped top-level vocabulary is.
        assert!(!stream_prefix_suspect(
            r#"{"results": [{"name": "Alice"}, {"name": "Bob"#,
            &names
        ));
        assert!(!stream_prefix_suspect(
            r#"{"name": "Alice", "age": 3, "notes": ["#,
            &names
        ));
    }

    // ── Mechanism B repair (end-to-end, both loops) ──────────────────────

    fn scripted_call(input: serde_json::Value) -> String {
        serde_json::json!({"name": "vault.delete", "input": input}).to_string()
    }

    #[tokio::test]
    async fn streaming_repair_executes_text_tool_call_without_burning_retries() {
        // Three consecutive text-form tool calls all repair and execute —
        // more than MAX_TOOL_HALLUCINATION_RETRIES, proving repair does not
        // consume the retry budget.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            &scripted_call(serde_json::json!({"n": 1})),
            &scripted_call(serde_json::json!({"n": 2})),
            &scripted_call(serde_json::json!({"n": 3})),
            "All done.",
        ]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "All done.");
        assert_eq!(result.rounds, 4);
        assert_eq!(result.tool_invocations.len(), 3);
        for (i, inv) in result.tool_invocations.iter().enumerate() {
            assert_eq!(inv.tool_name, "vault.delete");
            assert!(inv.success);
            assert_eq!(inv.input, serde_json::json!({"n": i + 1}));
            assert_eq!(inv.output, serde_json::json!({"deleted": {"n": i + 1}}));
        }

        let mut reasons = Vec::new();
        let mut deltas = String::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ProgressEvent::StreamDiscard { reason } => reasons.push(reason),
                ProgressEvent::StreamDelta { text } => deltas.push_str(&text),
                _ => {}
            }
        }
        assert_eq!(
            reasons,
            vec![
                StreamDiscardReason::Retry,
                StreamDiscardReason::Retry,
                StreamDiscardReason::Retry
            ],
            "each repaired round discards its superseded text"
        );
        assert_eq!(
            deltas, "All done.",
            "tool-call JSON never reaches the preview"
        );
    }

    #[tokio::test]
    async fn non_progress_repair_executes_text_tool_call_without_burning_retries() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            &scripted_call(serde_json::json!({"n": 1})),
            &scripted_call(serde_json::json!({"n": 2})),
            &scripted_call(serde_json::json!({"n": 3})),
            "All done.",
        ]));
        let runner = make_runner_with_tools(provider, tools);

        let result = runner.run("Go").await.unwrap();
        assert_eq!(result.response, "All done.");
        assert_eq!(result.rounds, 4);
        assert_eq!(result.tool_invocations.len(), 3);
        assert!(result.tool_invocations.iter().all(|inv| inv.success));
    }

    /// Call-shaped, opens as JSON, never closes — nothing can execute it and
    /// it is not an answer.
    const TRUNCATED_CALL: &str = r#"{"name": "vault.delete", "arguments": {"blob": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"#;

    const PROSE_NAMING_A_TOOL: &str = r#"I would use the vault.delete tool with {"id": 7} here, but I am only describing the approach."#;

    #[tokio::test]
    async fn streaming_prose_naming_a_tool_is_delivered() {
        // Prose that quotes a tool name and a JSON fragment is the answer:
        // discarding it strands the reader and leaves the model recapping a
        // reply nobody received.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![PROSE_NAMING_A_TOOL]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, PROSE_NAMING_A_TOOL);
        assert_eq!(result.rounds, 1);
        assert!(result.tool_invocations.is_empty());

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty(), "the answer must not be rewound");
        assert_eq!(deltas, PROSE_NAMING_A_TOOL);
    }

    #[tokio::test]
    async fn non_progress_prose_naming_a_tool_is_delivered() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![PROSE_NAMING_A_TOOL]));
        let runner = make_runner_with_tools(provider, tools);

        let result = runner.run("Go").await.unwrap();
        assert_eq!(result.response, PROSE_NAMING_A_TOOL);
        assert_eq!(result.rounds, 1);
        assert!(result.tool_invocations.is_empty());
    }

    #[tokio::test]
    async fn streaming_malformed_call_retries_without_a_delivered_draft() {
        // A truncated call is not an answer: the round is discarded, and the
        // retry must not carry it as an assistant turn — history that claims a
        // reply was delivered is what makes the next round recap it.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            TRUNCATED_CALL,
            "The real answer.",
        ]));
        let runner = make_runner_with_tools(provider.clone(), tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "The real answer.");
        assert_eq!(result.rounds, 2);
        assert!(result.tool_invocations.is_empty(), "nothing must execute");

        let (_, reasons, _) = drain_stream_events(&mut rx);
        assert_eq!(reasons, vec![StreamDiscardReason::Retry]);

        assert_retry_prompt_is_clean(&provider.prompt(1));
    }

    /// Nothing the runner injects may read as the human speaking, and the
    /// discarded round may not appear at all — history that claims a reply was
    /// delivered is what makes the next round recap it.
    fn assert_retry_prompt_is_clean(prompt: &[LlmMessage]) {
        assert!(
            prompt.iter().all(|m| m.role != Role::Assistant),
            "the discarded round must not enter history as a delivered turn"
        );
        let texts: Vec<&str> = prompt
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().all(|text| !text.contains(TRUNCATED_CALL)),
            "the discarded call must not be echoed back"
        );
        assert_eq!(texts.first(), Some(&"Go"), "the task leads the prompt");
        assert!(
            texts[1..]
                .iter()
                .all(|text| text.starts_with(crate::turn_event::RUNNER_NOTICE_PREFIX)),
            "every injected directive must be marked: {:?}",
            texts
        );
    }

    #[tokio::test]
    async fn non_progress_malformed_call_retries_without_a_delivered_draft() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            TRUNCATED_CALL,
            "The real answer.",
        ]));
        let runner = make_runner_with_tools(provider.clone(), tools);

        let result = runner.run("Go").await.unwrap();
        assert_eq!(result.response, "The real answer.");
        assert_eq!(result.rounds, 2);
        assert!(result.tool_invocations.is_empty());
        assert_retry_prompt_is_clean(&provider.prompt(1));
    }

    #[tokio::test]
    async fn malformed_calls_stop_retrying_once_the_budget_is_spent() {
        // A model that writes the same call as text three times exhausts the
        // budget and its last attempt is delivered verbatim — long-standing
        // behaviour, pinned here because tightening the verdict is what makes
        // this path reachable only by a genuinely malformed round.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            TRUNCATED_CALL,
            TRUNCATED_CALL,
            TRUNCATED_CALL,
        ]));
        let runner = make_runner_with_tools(provider, tools);

        let result = runner.run("Go").await.unwrap();
        assert_eq!(
            result.rounds,
            MAX_TOOL_HALLUCINATION_RETRIES + 1,
            "the retry budget bounds the loop"
        );
        assert_eq!(result.response, TRUNCATED_CALL);
    }

    #[tokio::test]
    async fn repaired_call_respects_policy_denial() {
        // A tool call synthesized from text must flow through the same policy
        // evaluation as a native call: a denied tool yields a denial result,
        // not an execution.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            &scripted_call(serde_json::json!({"id": 7})),
            "I was denied.",
        ]));
        let runner = make_runner_with_tools_and_policy(
            provider,
            tools,
            "rules:\n  - name: deny-all\n    scope: global\n    tool: \"*\"\n    action: deny\n",
        );
        let (tx, _rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "I was denied.");
        assert_eq!(result.tool_invocations.len(), 1);
        let inv = &result.tool_invocations[0];
        assert_eq!(inv.tool_name, "vault.delete");
        assert_eq!(inv.policy_decision, "deny");
        assert!(!inv.success);
        assert_eq!(
            inv.output,
            serde_json::Value::Null,
            "the tool must never have executed"
        );
    }

    // ── Mechanism A stream gate ──────────────────────────────────────────

    fn drain_stream_events(
        rx: &mut mpsc::Receiver<ProgressEvent>,
    ) -> (String, Vec<StreamDiscardReason>, usize) {
        let mut deltas = String::new();
        let mut reasons = Vec::new();
        let mut delta_count = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                ProgressEvent::StreamDelta { text } => {
                    delta_count += 1;
                    deltas.push_str(&text);
                }
                ProgressEvent::StreamDiscard { reason } => reasons.push(reason),
                _ => {}
            }
        }
        (deltas, reasons, delta_count)
    }

    #[tokio::test]
    async fn stream_gate_aborts_runaway_suspect_json_at_the_cap() {
        // A never-closing JSON blob with tool-call shape but no registered
        // tool name survives every recheck — the 8KB cap must cut it off
        // instead of letting it stream indefinitely.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let runaway = format!(
            r#"{{"name": "invented.tool", "arguments": {{"blob": "{}"#,
            "x".repeat(9_000)
        );
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&runaway, "Recovered."]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "Recovered.");
        assert_eq!(result.rounds, 2);
        assert!(result.tool_invocations.is_empty());

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert_eq!(
            reasons,
            vec![],
            "a round that previewed nothing has nothing to discard — the event \
             would consume the preview's retry fallback for an older real round"
        );
        assert_eq!(
            deltas, "Recovered.",
            "the runaway JSON never reaches the preview"
        );
    }

    #[tokio::test]
    async fn stream_gate_abort_repairs_a_completed_text_tool_call() {
        // The call completes mid-stream, a recheck then matches the
        // registered tool name and aborts — round-end handling repairs the
        // accumulated text into a real execution.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        // Literal JSON so the "name" key stays in the suspect prefix
        // (serde_json::json! would sort keys alphabetically).
        let call = format!(
            "{{\"name\": \"vault.delete\", \"arguments\": {{\"filler\": \"{}\"}}}}{}",
            "x".repeat(800),
            "\n".repeat(600)
        );
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![
            &call,
            "Done after repair.",
        ]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Go", tx).await.unwrap();
        assert_eq!(result.response, "Done after repair.");
        assert_eq!(result.rounds, 2);
        assert_eq!(result.tool_invocations.len(), 1);
        let inv = &result.tool_invocations[0];
        assert_eq!(inv.tool_name, "vault.delete");
        assert!(inv.success);
        assert_eq!(inv.output["deleted"]["filler"], "x".repeat(800));

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert_eq!(
            reasons,
            vec![],
            "nothing was previewed, so neither the abort site nor the repair \
             emits a discard"
        );
        assert_eq!(deltas, "Done after repair.");
    }

    #[tokio::test]
    async fn stream_gate_streams_json_data_without_tool_call_shape() {
        // A JSON data answer with no tool-call keys is classified benign at
        // the hold and streams in full.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let rows: Vec<serde_json::Value> = (0..40)
            .map(|i| serde_json::json!({"id": i, "value": format!("row-{i}")}))
            .collect();
        let data = serde_json::json!({"results": rows}).to_string();
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&data]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner
            .run_with_progress("Give me the data", tx)
            .await
            .unwrap();
        assert_eq!(result.response, data);
        assert_eq!(result.rounds, 1);

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty(), "no discard for a benign answer");
        assert_eq!(deltas, data, "the full JSON answer streams");
    }

    #[tokio::test]
    async fn stream_gate_resumes_after_suspect_json_proves_benign() {
        // Opens call-shaped (name + input keys — suspect), but the object
        // closes without matching a tool call — the recheck must release the
        // hold losslessly and resume normal streaming for the prose tail.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let text = format!(
            "{} That record belongs to the customer profile. {}",
            r#"{"name": "Alice", "input": {"age": 3}}"#,
            "The remaining explanation continues at length without any braces. ".repeat(25)
        );
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&text]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Explain", tx).await.unwrap();
        assert_eq!(result.response, text);
        assert_eq!(result.rounds, 1);

        let (deltas, reasons, delta_count) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty());
        assert_eq!(deltas, text, "the hold is lossless");
        assert!(delta_count >= 2, "streaming resumes after the release");
    }

    #[tokio::test]
    async fn stream_gate_releases_suspect_benign_short_json_at_stream_end() {
        // A short call-shaped suspect that never reaches a recheck and never
        // matches a tool call is released in full when the stream ends.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let text = format!(
            "{{\"name\": \"Alice\", \"params\": {{\"items\": {:?}, \"note\": \"{}\"}}}}",
            (0..20).collect::<Vec<u32>>(),
            "d".repeat(300)
        );
        assert!(text.len() > STREAM_GATE_HOLD_BYTES && text.len() < STREAM_GATE_RECHECK_BYTES);
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&text]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Data please", tx).await.unwrap();
        assert_eq!(result.response, text);

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty());
        assert_eq!(deltas, text);
    }

    #[tokio::test]
    async fn stream_gate_forwards_prose_after_the_hold() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let prose = "This is an ordinary prose answer that keeps going. ".repeat(10);
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&prose]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(64);

        let result = runner.run_with_progress("Chat", tx).await.unwrap();
        assert_eq!(result.response, prose);

        let (deltas, reasons, delta_count) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty());
        assert_eq!(deltas, prose, "everything is forwarded");
        assert!(
            delta_count >= 2,
            "only the initial hold coalesces; the rest streams delta-by-delta"
        );
    }

    #[tokio::test]
    async fn stream_gate_streams_large_json_data_with_nested_name_keys() {
        // A big single-object data answer full of nested "name" fields (a
        // chart/table payload) must never be suppressed or cut off at the
        // cap: its top-level keys are not call vocabulary, so it is benign
        // at the hold.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let rows: Vec<serde_json::Value> = (0..400)
            .map(|i| serde_json::json!({"id": i, "name": format!("row-{i}")}))
            .collect();
        let data = serde_json::json!({"results": rows}).to_string();
        assert!(data.len() > STREAM_GATE_ABORT_BYTES);
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&data]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(1024);

        let result = runner
            .run_with_progress("Chart the data", tx)
            .await
            .unwrap();
        assert_eq!(result.response, data);
        assert_eq!(result.rounds, 1, "no retry is burned on a data answer");

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty(), "a data answer is never discarded");
        assert_eq!(deltas, data, "the full payload streams");
    }

    #[tokio::test]
    async fn stream_gate_releases_tool_name_mention_data_at_the_cap() {
        // Data that quotes a registered tool name early stays suspect through
        // every recheck (the object never closes mid-stream) but is not
        // call-shaped — the cap must release it to the preview, not cut the
        // round off, and the round handler must deliver it rather than bin an
        // answer over a name quoted in a log line.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(EchoTool));
        let entries: Vec<String> = (0..900).map(|i| format!("\"entry-{i}\"")).collect();
        let data = format!(
            "{{\"log\": \"ran \\\"vault_delete\\\" before this export\", \"entries\": [{}]}}",
            entries.join(", ")
        );
        assert!(data.len() > STREAM_GATE_ABORT_BYTES);
        let provider = Arc::new(ScriptedStreamingProvider::new(vec![&data]));
        let runner = make_runner_with_tools(provider, tools);
        let (tx, mut rx) = mpsc::channel(1024);

        let result = runner
            .run_with_progress("Export the log", tx)
            .await
            .unwrap();
        assert_eq!(result.response, data);
        assert_eq!(result.rounds, 1);
        assert!(result.tool_invocations.is_empty());

        let (deltas, reasons, _) = drain_stream_events(&mut rx);
        assert!(reasons.is_empty(), "the answer must not be rewound");
        assert!(
            deltas.contains(&data),
            "the cap released the full payload instead of cutting it off"
        );
    }

    // ── ToolUse input pruning (context-overflow guard) ───────────────────

    fn make_tool_use_with_input(id: &str, name: &str, input: serde_json::Value) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
                signature: None,
            }],
        }
    }

    fn big_values(n: usize) -> serde_json::Value {
        let rows: Vec<serde_json::Value> = (0..n).map(|i| serde_json::json!([i, i + 1])).collect();
        serde_json::json!({
            "sheet_id": "abc",
            "range": "Sheet1!A1",
            "values": rows,
        })
    }

    #[test]
    fn collect_consumed_tool_use_ids_returns_only_ids_with_results() {
        let messages = vec![
            make_tool_use_msg("t1", "x"),
            make_tool_use_msg("t2", "x"),
            make_tool_use_msg("t3", "x"),
            make_tool_result_msg("t1", "ok"),
            make_tool_result_msg("t2", "ok"),
        ];
        let consumed = AgentRunner::collect_consumed_tool_use_ids(&messages);
        assert!(consumed.contains("t1"));
        assert!(consumed.contains("t2"));
        assert!(!consumed.contains("t3"));
        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn prune_consumed_tool_blocks_shrinks_consumed_tool_use_input() {
        let mut messages = vec![
            make_tool_use_with_input("t1", "sheets.append", big_values(200)),
            make_tool_result_msg("t1", "ok"),
            make_tool_use_with_input("t2", "sheets.append", big_values(200)),
            make_tool_result_msg("t2", "ok"),
            make_tool_use_with_input("t3", "sheets.append", big_values(200)),
            make_tool_result_msg("t3", "ok"),
        ];

        AgentRunner::prune_consumed_tool_blocks(&mut messages, 2);

        if let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] {
            assert!(
                input["values"].is_string(),
                "older consumed ToolUse should be shrunk"
            );
        } else {
            panic!("expected ToolUse");
        }
        if let ContentBlock::ToolUse { input, .. } = &messages[4].content[0] {
            assert!(
                input["values"].is_array(),
                "recent ToolUse should be untouched"
            );
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn prune_consumed_tool_blocks_preserves_signature_id_and_name() {
        let mut messages = vec![
            LlmMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "sheets.append".to_string(),
                    input: big_values(200),
                    signature: Some("gemini-signature-1".to_string()),
                }],
            },
            make_tool_result_msg("t1", "ok"),
            make_tool_use_msg("dummy_recent", "x"),
        ];

        AgentRunner::prune_consumed_tool_blocks(&mut messages, 1);

        if let ContentBlock::ToolUse {
            id,
            name,
            signature,
            ..
        } = &messages[0].content[0]
        {
            assert_eq!(id, "t1");
            assert_eq!(name, "sheets.append");
            assert_eq!(signature.as_deref(), Some("gemini-signature-1"));
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn prune_consumed_tool_blocks_skips_unconsumed_blocks() {
        let mut messages = vec![
            make_tool_use_with_input("orphan", "sheets.append", big_values(200)),
            make_tool_use_msg("dummy", "x"),
            make_tool_result_msg("dummy", "ok"),
        ];

        AgentRunner::prune_consumed_tool_blocks(&mut messages, 0);

        if let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] {
            assert!(
                input["values"].is_array(),
                "unconsumed ToolUse must stay full-fidelity"
            );
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn a_result_that_loaded_tools_keeps_its_definitions_through_pruning_and_eviction() {
        let definitions = "x".repeat(2_000);
        let loaded = |id: &str| LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: definitions.clone(),
                is_error: None,
                loaded: Some(Loaded {
                    tools: vec!["vault_set".into()],
                    note: "Loaded: vault_set.".into(),
                }),
            }],
        };
        let call = |id: &str| LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id.to_string(),
                "tool_load".to_string(),
                serde_json::json!({ "names": ["vault_set"] }),
            )],
        };
        let mut messages: Vec<LlmMessage> = (0..3)
            .flat_map(|i| [call(&format!("t{i}")), loaded(&format!("t{i}"))])
            .collect();
        AgentRunner::prune_consumed_tool_blocks(&mut messages, 1);
        assert_eq!(
            AgentRunner::evict_stale_tool_outputs(&mut messages, 1),
            (0, 0)
        );
        assert!(messages
            .iter()
            .flat_map(|m| &m.content)
            .all(|block| match block {
                ContentBlock::ToolResult { content, .. } => content == &definitions,
                _ => true,
            }));
    }

    #[test]
    fn eviction_groups_tool_call_results_by_the_tool_they_called() {
        let result =
            |id: &str| LlmMessage::tool_results(vec![(id.to_string(), "y".repeat(600), false)]);
        let call = |id: &str, tool: &str| LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id.to_string(),
                "tool_call".to_string(),
                serde_json::json!({ "name": tool, "arguments": {} }),
            )],
        };
        let mut messages = vec![
            call("a", "jira_search"),
            result("a"),
            call("b", "metabase_query"),
            result("b"),
            call("c", "metabase_query"),
            result("c"),
        ];
        let (evicted, _) = AgentRunner::evict_stale_tool_outputs(&mut messages, 1);
        assert_eq!(evicted, 1, "only the older metabase result goes");
        let content = |i: usize| match &messages[i].content[0] {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert_eq!(content(1).len(), 600, "jira's only result stays");
        assert!(content(3).starts_with("[evicted"));
    }

    #[test]
    fn prune_consumed_tool_blocks_is_idempotent() {
        let mut messages = vec![
            make_tool_use_with_input("t1", "sheets.append", big_values(200)),
            make_tool_result_msg("t1", "ok"),
            make_tool_use_msg("dummy_recent", "x"),
        ];
        AgentRunner::prune_consumed_tool_blocks(&mut messages, 1);
        let snapshot = messages.clone();
        AgentRunner::prune_consumed_tool_blocks(&mut messages, 1);
        // Idempotence: structurally equivalent inputs serialise identically.
        let before: Vec<String> = snapshot
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        let after: Vec<String> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn truncate_large_blocks_shrinks_tool_use_input() {
        let mut messages = vec![
            LlmMessage::user("kick off task"),
            make_tool_use_with_input("t1", "sheets.append", big_values(500)),
        ];
        let before_tokens: u32 = messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::ToolUse { input, .. } => estimate_tokens(&input.to_string()),
                ContentBlock::Text { text } => estimate_tokens(text),
                _ => 0,
            })
            .sum();

        let mutated = AgentRunner::truncate_large_blocks(&mut messages, 100);

        assert!(mutated);
        let after_tokens: u32 = messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::ToolUse { input, .. } => estimate_tokens(&input.to_string()),
                ContentBlock::Text { text } => estimate_tokens(text),
                _ => 0,
            })
            .sum();
        assert!(after_tokens < before_tokens);
    }

    // ── tool_access filter wiring ───────────────────────────────

    fn runner_with_tools(names: &[&str], access: Arc<dyn ToolAccess>) -> AgentRunner {
        let config = RunnerConfig {
            agent_id: workforce_core::types::AgentId::new("test"),
            system_prompt: "x".to_string(),
            model: Some("mock".to_string()),
            max_tokens: Some(1),
            max_rounds: 1,
            team_id: None,
            role: None,
            compaction: CompactionConfig::default(),
            spill: workforce_common::session::SpillConfig::default(),
            routing: crate::model_router::RoutingConfig::default(),
            tier_registry: crate::llm::tiers::TierRegistry::new(None),
            model_max_tokens: HashMap::new(),
            context_eviction_interval: None,
            context_eviction_keep_last: 3,
            trace_writer: None,
            trace_tags: Default::default(),
            permission_writer: None,
            permission_gate: None,
            arguments: crate::tools::arguments::ArgumentValidation::default(),
            tool_loading: crate::tools::loading::ToolLoading::default(),
            programs: None,
            capability_gate: None,
            approval_router: None,
            preexecute: None,
            journal: None,
            collect: Vec::new(),
            seeds: None,
            interactive: false,
            self_serve: crate::tools::approval::SelfServe::Answerable,
            compile_gate: None,
        };
        let tools = Arc::new(registry_with(names));
        let policy = Arc::new(
            workforce_policy::engine::PolicyEngine::from_yaml(
                "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
            )
            .expect("policy parse"),
        );
        let llm = Arc::new(MockSimpleProvider::new("ok")) as Arc<dyn LlmProvider>;
        AgentRunner::new(config, llm, tools, policy).with_tool_access(access)
    }

    struct StaticAccess(HashMap<String, String>);
    impl ToolAccess for StaticAccess {
        fn snapshot(&self) -> ToolAccessSnapshot {
            ToolAccessSnapshot::new(self.0.clone())
        }
    }

    fn plan_context<'a>(
        loading: &'a crate::tools::loading::ToolLoading,
        session: &'a BTreeSet<String>,
    ) -> PlanContext<'a> {
        PlanContext {
            loading,
            session,
            context_floor: Some(1_000_000),
            precurated: false,
        }
    }

    fn loading_on(core: &[&str]) -> crate::tools::loading::ToolLoading {
        crate::tools::loading::ToolLoading {
            mode: ToolLoadingMode::On,
            core: Some(Arc::new(core.iter().map(|t| t.to_string()).collect())),
            always: Arc::default(),
        }
    }

    fn with_loading(mut runner: AgentRunner, core: &[&str]) -> AgentRunner {
        runner.config.tool_loading = loading_on(core);
        runner
    }

    /// `names` and enough other tools that the list is not offered whole.
    fn padded(names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|name| name.to_string())
            .chain((0..40).map(|i| format!("zz_filler{i}")))
            .collect()
    }

    fn padded_registry(names: &[&str]) -> ToolRegistry {
        let names = padded(names);
        registry_with(&names.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn offered_names(offered: &OfferedTools) -> Vec<&str> {
        offered.defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn blocked_and_dormant_tools_are_never_offered() {
        let _g = crate::llm::mcp_workspace::lock_and_clear_for_test();
        crate::llm::mcp_workspace::set_member("crm-prod", "crm", "prod", true);
        crate::llm::mcp_workspace::set_member("crm-staging", "crm", "staging", false);
        let registry = registry_with(&[
            "filesystem.read",
            "linear.issue_create",
            "mcp.crm-prod.search",
            "mcp.crm-staging.search",
        ]);
        let blocked = StaticAccess(HashMap::from([(
            "linear.issue_create".to_string(),
            "shadowed".to_string(),
        )]));
        let loading = crate::tools::loading::ToolLoading::default();
        let session = BTreeSet::new();
        let mut offered = OfferedTools::default();
        offered.refresh(&registry, &blocked, &plan_context(&loading, &session));
        assert_eq!(
            offered_names(&offered),
            ["filesystem_read", "mcp_crm-prod_search"]
        );
    }

    #[tokio::test]
    async fn session_tools_read_empty_outside_a_scope() {
        assert!(session_tools().is_empty());
        let inside = SESSION_TOOLS
            .scope(
                Arc::new(BTreeSet::from(["linear.issue_create".to_string()])),
                async { session_tools() },
            )
            .await;
        assert!(inside.contains("linear.issue_create"));
    }

    #[test]
    fn reserved_context_matches_between_the_pre_turn_and_watchdog_paths() {
        let runner = runner_with_tools(
            &["filesystem_read", "linear_issue_create"],
            Arc::new(access::NoopToolAccess),
        );
        let session = BTreeSet::new();
        let mut offered = OfferedTools::default();
        offered.refresh(
            &runner.tools,
            runner.tool_access.as_ref(),
            &runner.plan_context(&session, crate::llm::model_limits::context_for("mock")),
        );
        assert_eq!(
            runner.reserved_context_for_with("mock", &offered),
            runner.reserved_context_for("mock")
        );
        assert_eq!(
            runner
                .reserved_context_for_with("mock", &OfferedTools::default())
                .tool_schema_tokens,
            0
        );
    }

    #[test]
    fn the_reserved_context_counts_the_catalogue_and_only_declared_tools() {
        let names = padded(&["memory"]);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let runner = || runner_with_tools(&names, Arc::new(access::NoopToolAccess));
        let catalogued = with_loading(runner(), &[]).reserved_context_for("mock");
        let everything = runner().reserved_context_for("mock");
        assert!(catalogued.tool_schema_tokens < everything.tool_schema_tokens);
        assert!(catalogued.system_prompt_tokens > everything.system_prompt_tokens);
    }

    #[test]
    fn offered_tools_hold_still_until_the_registry_access_or_a_load_changes_them() {
        let registry = registry_with(&["a", "b"]);
        let loading = crate::tools::loading::ToolLoading::default();
        let session = BTreeSet::new();
        let ctx = plan_context(&loading, &session);
        let mut offered = OfferedTools::default();
        assert!(offered.refresh(&registry, &access::NoopToolAccess, &ctx));
        assert_eq!(offered.defs.len(), 2);
        assert!(
            !offered.refresh(&registry, &access::NoopToolAccess, &ctx),
            "nothing moved, nothing rebuilt"
        );

        registry.register(mock_tool("c"));
        assert!(offered.refresh(&registry, &access::NoopToolAccess, &ctx));
        assert_eq!(offered.defs.len(), 3);

        let blocked = StaticAccess(HashMap::from([("b".to_string(), "hidden".to_string())]));
        assert!(offered.refresh(&registry, &blocked, &ctx));
        assert_eq!(offered.defs.len(), 2);
        assert!(offered.access.is_blocked("b"));

        let registry = padded_registry(&["a", "c"]);
        let on = loading_on(&["a"]);
        let mut loading_offered = OfferedTools::default();
        loading_offered.refresh(
            &registry,
            &access::NoopToolAccess,
            &plan_context(&on, &session),
        );
        assert_eq!(
            offered_names(&loading_offered),
            ["a", "tool_load", "tool_call"]
        );
        assert!(loading_offered.plan.is_unloaded("c"));
        loading_offered.queue_loads(&["c".to_string()]);
        assert!(
            !loading_offered.refresh(
                &registry,
                &access::NoopToolAccess,
                &plan_context(&on, &session)
            ),
            "a load changes no declared tool"
        );
        assert_eq!(
            offered_names(&loading_offered),
            ["a", "tool_load", "tool_call"]
        );
        assert!(!loading_offered.plan.is_unloaded("c"));
    }

    #[test]
    fn offered_tools_come_out_in_name_order_whatever_the_registration_order() {
        let registry = registry_with(&["zeta", "alpha", "mid", "beta"]);
        let blocked = StaticAccess(HashMap::from([("mid".to_string(), "hidden".to_string())]));
        let loading = crate::tools::loading::ToolLoading::default();
        let session = BTreeSet::new();
        let mut offered = OfferedTools::default();
        offered.refresh(&registry, &blocked, &plan_context(&loading, &session));
        assert_eq!(offered_names(&offered), ["alpha", "beta", "zeta"]);
    }

    /// Answers each call with the next scripted round of tool calls, then
    /// ends the turn, recording the system prompt and tools of every call.
    struct ScriptedLoadingProvider {
        script: Vec<Vec<(&'static str, serde_json::Value)>>,
        seen: std::sync::Mutex<Vec<(String, Vec<String>, Vec<LlmMessage>)>>,
    }

    impl ScriptedLoadingProvider {
        fn new(script: Vec<Vec<(&'static str, serde_json::Value)>>) -> Arc<Self> {
            Arc::new(Self {
                script,
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedLoadingProvider {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            messages: &[LlmMessage],
            tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let mut seen = self.seen.lock().unwrap();
            seen.push((
                system_prompt.unwrap_or_default().to_string(),
                tools.iter().map(|t| t.name.clone()).collect(),
                messages.to_vec(),
            ));
            let round = seen.len();
            Ok(match self.script.get(round - 1) {
                Some(calls) => LlmResponse {
                    content: calls
                        .iter()
                        .enumerate()
                        .map(|(i, (tool, input))| {
                            ContentBlock::tool_use(
                                format!("tu_{round}_{i}"),
                                tool.to_string(),
                                input.clone(),
                            )
                        })
                        .collect(),
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                    model: "mock".to_string(),
                },
                None => LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    model: "mock".to_string(),
                },
            })
        }
    }

    fn loading_runner(llm: Arc<ScriptedLoadingProvider>, core: &[&str]) -> AgentRunner {
        loading_runner_with(
            llm,
            core,
            padded_registry(&["memory", "vault.set", "scheduler"]),
        )
    }

    fn loading_runner_with(
        llm: Arc<ScriptedLoadingProvider>,
        core: &[&str],
        registry: ToolRegistry,
    ) -> AgentRunner {
        with_loading(
            make_runner_with_tools(llm as Arc<dyn LlmProvider>, Arc::new(registry)),
            core,
        )
    }

    fn records(result: &RunResult) -> Vec<(&str, &str, bool)> {
        let mut records: Vec<(&str, &str, bool)> = result
            .tool_invocations
            .iter()
            .map(|i| (i.tool_name.as_str(), i.policy_decision.as_str(), i.success))
            .collect();
        records.sort();
        records
    }

    fn result_of<'a>(messages: &'a [LlmMessage], id: &str) -> (&'a str, Option<&'a Loaded>) {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    loaded,
                    ..
                } if tool_use_id == id => Some((content.as_str(), loaded.as_ref())),
                _ => None,
            })
            .expect("the call has a result")
    }

    async fn loads_then_calls(streaming: bool) {
        let llm = ScriptedLoadingProvider::new(vec![
            vec![("tool_load", serde_json::json!({ "names": ["vault_set"] }))],
            vec![(
                "tool_call",
                serde_json::json!({ "name": "vault_set", "arguments": {} }),
            )],
        ]);
        let runner = loading_runner(Arc::clone(&llm), &[]);
        let result = run_either(&runner, streaming).await;

        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter()
                .all(|(_, tools, _)| tools == &["memory", "tool_load", "tool_call"]),
            "the declared tools are the same in every round"
        );
        assert!(seen[0].0.contains("## Tool catalogue"));
        assert!(seen[0].0.contains("- vault_set: mock"));
        assert_eq!(seen[1].0, seen[0].0, "the catalogue holds for the turn");
        let (content, loaded) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.contains("\"name\":\"vault_set\""), "{content}");
        let loaded = loaded.expect("the load is marked");
        assert_eq!(loaded.tools, ["vault_set"]);
        assert!(loaded.note.starts_with("Loaded: vault_set."));

        assert_eq!(
            records(&result),
            [("tool.load", "load", true), ("vault.set", "allow", true)]
        );
    }

    #[tokio::test]
    async fn a_loaded_tool_is_called_through_tool_call() {
        loads_then_calls(false).await;
    }

    #[tokio::test]
    async fn a_loaded_tool_is_called_through_tool_call_when_streaming() {
        loads_then_calls(true).await;
    }

    #[tokio::test]
    async fn a_loaded_tool_also_runs_when_called_directly() {
        let llm = ScriptedLoadingProvider::new(vec![
            vec![("tool_load", serde_json::json!({ "names": ["vault_set"] }))],
            vec![("vault_set", serde_json::json!({}))],
        ]);
        let result = run_either(&loading_runner(Arc::clone(&llm), &[]), false).await;
        assert_eq!(
            records(&result),
            [("tool.load", "load", true), ("vault.set", "allow", true)]
        );
    }

    async fn run_either(runner: &AgentRunner, streaming: bool) -> RunResult {
        match streaming {
            false => runner.run("go").await,
            true => {
                let (tx, _rx) = tokio::sync::mpsc::channel(256);
                runner.run_with_progress("go", tx).await
            }
        }
        .expect("run")
    }

    async fn unloaded_calls_are_loaded_not_run(streaming: bool) {
        let llm = ScriptedLoadingProvider::new(vec![vec![
            ("tool_load", serde_json::json!({ "names": ["scheduler"] })),
            ("scheduler", serde_json::json!({})),
            (
                "tool_call",
                serde_json::json!({ "name": "vault_set", "arguments": {} }),
            ),
        ]]);
        let runner = loading_runner(Arc::clone(&llm), &["memory"]);
        let result = run_either(&runner, streaming).await;

        assert_eq!(
            records(&result),
            [
                ("scheduler", "load", false),
                ("tool.load", "load", true),
                ("vault.set", "load", false)
            ],
            "a load and a call in the same round: the call is still loaded, not run"
        );
        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen[1].1, ["memory", "tool_load", "tool_call"]);
        let (skipped, loaded) = result_of(&seen[1].2, "tu_1_1");
        assert!(skipped.starts_with("`scheduler` was not loaded"));
        assert!(skipped.contains("\"name\":\"scheduler\""));
        assert_eq!(loaded.unwrap().tools, ["scheduler"]);
    }

    #[tokio::test]
    async fn a_call_to_an_unloaded_tool_loads_it_instead_of_running() {
        unloaded_calls_are_loaded_not_run(false).await;
    }

    #[tokio::test]
    async fn a_call_to_an_unloaded_tool_loads_it_instead_of_running_when_streaming() {
        unloaded_calls_are_loaded_not_run(true).await;
    }

    fn with_programs(mut runner: AgentRunner, read_tools: &[&str]) -> AgentRunner {
        runner.config.programs = Some(Arc::new(crate::tools::program::Programs::from_config(
            &workforce_common::settings::ProgramsConfig {
                read_tools: read_tools.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            },
        )));
        runner
    }

    fn program(source: &str) -> (&'static str, serde_json::Value) {
        ("program_run", serde_json::json!({ "source": source }))
    }

    async fn a_program_runs_its_calls_as_direct_calls(streaming: bool) {
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "let a = memory({})\nlet b = vault_set({})\nreturn { a, b }",
        )]]);
        let runner = with_programs(
            make_runner_with_tools(
                Arc::clone(&llm) as Arc<dyn LlmProvider>,
                Arc::new(registry_with(&["memory", "vault.set"])),
            ),
            &["memory", "vault"],
        );
        let result = run_either(&runner, streaming).await;

        assert_eq!(
            records(&result),
            [
                ("memory", "allow", true),
                ("program.run", "allow", true),
                ("vault.set", "allow", true)
            ],
            "each call the program made is recorded under its own tool"
        );
        let seen = llm.seen.lock().unwrap();
        assert!(seen[0].1.contains(&"program_run".to_string()));
        let (content, _) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.contains("EXTERNAL_UNTRUSTED_CONTENT"), "{content}");
        assert!(content.contains("\"a\": \"ok\""), "{content}");
    }

    #[tokio::test]
    async fn a_program_runs_its_calls_as_direct_calls_in_the_plain_loop() {
        a_program_runs_its_calls_as_direct_calls(false).await;
    }

    #[tokio::test]
    async fn a_program_runs_its_calls_as_direct_calls_when_streaming() {
        a_program_runs_its_calls_as_direct_calls(true).await;
    }

    #[tokio::test]
    async fn a_program_may_call_only_tools_listed_as_reads() {
        let llm =
            ScriptedLoadingProvider::new(vec![vec![program("let a = vault_set({})\nreturn a")]]);
        let runner = with_programs(
            make_runner_with_tools(
                Arc::clone(&llm) as Arc<dyn LlmProvider>,
                Arc::new(registry_with(&["memory", "vault.set"])),
            ),
            &["memory"],
        );
        let result = run_either(&runner, false).await;
        assert_eq!(records(&result), [("program.run", "allow", false)]);
        let seen = llm.seen.lock().unwrap();
        let (content, _) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.contains("not on this install's list"), "{content}");
    }

    #[tokio::test]
    async fn a_program_calling_a_catalogued_tool_loads_it_for_the_next_round() {
        let llm =
            ScriptedLoadingProvider::new(vec![vec![program("let a = vault_set({})\nreturn a")]]);
        let runner = with_programs(loading_runner(Arc::clone(&llm), &["memory"]), &["vault"]);
        let result = run_either(&runner, false).await;
        assert_eq!(records(&result), [("program.run", "allow", false)]);
        let seen = llm.seen.lock().unwrap();
        let (content, loaded) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.contains("was not loaded"), "{content}");
        assert_eq!(loaded.expect("the load is marked").tools, ["vault_set"]);
    }

    #[tokio::test]
    async fn a_program_cannot_run_a_call_that_needs_an_approval() {
        struct Guarded;
        #[async_trait]
        impl Tool for Guarded {
            fn name(&self) -> &str {
                "vault.get"
            }
            fn description(&self) -> &str {
                "Read a secret."
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn approval_requirement(&self) -> Option<String> {
                Some("reads a secret".to_string())
            }
            async fn execute(&self, _: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("secret"),
                    0,
                ))
            }
        }
        let registry = registry_with(&["memory"]);
        registry.register(Arc::new(Guarded));
        let llm =
            ScriptedLoadingProvider::new(vec![vec![program("let a = vault_get({})\nreturn a")]]);
        let runner = with_programs(
            make_runner_with_tools(Arc::clone(&llm) as Arc<dyn LlmProvider>, Arc::new(registry)),
            &["vault"],
        );
        let result = run_either(&runner, false).await;
        assert_eq!(
            records(&result),
            [
                ("program.run", "allow", false),
                ("vault.get", "deny", false)
            ]
        );
        let seen = llm.seen.lock().unwrap();
        let (content, _) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.contains("needs an approval"), "{content}");
    }

    /// A native SQL tool that records each query and answers one row.
    struct NativeQuery(std::sync::Mutex<Vec<serde_json::Value>>);
    #[async_trait]
    impl Tool for NativeQuery {
        fn name(&self) -> &str {
            "metabase.dataset_query_native"
        }
        fn description(&self) -> &str {
            "Run SQL."
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            self.0.lock().unwrap().push(input);
            let rows = crate::security::external_content::wrap_external_content(
                "[[\"2026-09\", 4]]",
                "metabase_native_query",
            )
            .wrapped_content;
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({
                    "data": { "cols": [{ "name": "month" }, { "name": "n" }], "rows_wrapped": rows }
                }),
                0,
            ))
        }
    }

    fn native_query() -> Arc<NativeQuery> {
        Arc::new(NativeQuery(std::sync::Mutex::new(Vec::new())))
    }

    #[tokio::test]
    async fn a_program_sql_block_binds_its_values_as_template_tags() {
        let native = native_query();
        let registry = registry_with(&["memory"]);
        registry.register(Arc::clone(&native) as Arc<dyn Tool>);
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "let rows = sql(7) {\n  SELECT month, n FROM counts WHERE region = ${\"north\"} AND n > ${2}\n}\nreturn rows",
        )]]);
        let runner = with_programs(
            make_runner_with_tools(Arc::clone(&llm) as Arc<dyn LlmProvider>, Arc::new(registry)),
            &["metabase"],
        );
        let result = run_either(&runner, false).await;

        let calls = native.0.lock().unwrap();
        let call = &calls[0];
        assert_eq!(call["database_id"], 7);
        assert!(
            call["query"]
                .as_str()
                .unwrap()
                .contains("region = {{p1}} AND n > {{p2}}"),
            "{call}"
        );
        assert_eq!(call["template_tags"]["p1"]["type"], "text");
        assert_eq!(call["template_tags"]["p2"]["type"], "number");
        assert_eq!(
            call["parameters"],
            serde_json::json!([
                { "type": "category", "target": ["variable", ["template-tag", "p1"]], "value": "north" },
                { "type": "number/=", "target": ["variable", ["template-tag", "p2"]], "value": 2 }
            ])
        );
        let program_record = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name == crate::tools::program::PROGRAM_TOOL)
            .expect("the program is recorded");
        assert_eq!(
            program_record.output,
            serde_json::json!({ "returned": [{ "month": "2026-09", "n": 4 }] }),
            "the value is recorded under `returned`, never as the runner's own markers"
        );
    }

    #[tokio::test]
    async fn a_program_sql_block_runs_while_the_sql_tool_is_unloaded() {
        let native = native_query();
        let registry = padded_registry(&["memory"]);
        registry.register(Arc::clone(&native) as Arc<dyn Tool>);
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "return sql(7) { SELECT month, n FROM counts }",
        )]]);
        let runner = with_programs(
            loading_runner_with(Arc::clone(&llm), &["memory"], registry),
            &["metabase"],
        );
        let result = run_either(&runner, false).await;
        assert_eq!(
            records(&result),
            [
                ("metabase.dataset_query_native", "allow", true),
                ("program.run", "allow", true)
            ],
            "the model writes the query, not the tool's arguments, so no load is needed"
        );
        assert_eq!(native.0.lock().unwrap().len(), 1);
        let seen = llm.seen.lock().unwrap();
        assert!(
            !seen[0]
                .1
                .contains(&"metabase_dataset_query_native".to_string()),
            "the SQL tool was not offered, so it was unloaded: {:?}",
            seen[0].1
        );
    }

    async fn sql_refusal(read_tools: &[&str], with_native: bool, source: &str) -> String {
        let registry = registry_with(&["memory"]);
        if with_native {
            registry.register(native_query() as Arc<dyn Tool>);
        }
        let llm = ScriptedLoadingProvider::new(vec![vec![program(source)]]);
        let runner = with_programs(
            make_runner_with_tools(Arc::clone(&llm) as Arc<dyn LlmProvider>, Arc::new(registry)),
            read_tools,
        );
        let result = run_either(&runner, false).await;
        assert_eq!(records(&result), [("program.run", "allow", false)]);
        let seen = llm.seen.lock().unwrap();
        result_of(&seen[1].2, "tu_1_0").0.to_string()
    }

    #[tokio::test]
    async fn a_program_sql_block_says_why_it_cannot_run() {
        let query = "return sql(7) { SELECT 1 }";
        let missing = sql_refusal(&["metabase"], false, query).await;
        assert!(
            missing.contains("which this install does not have"),
            "{missing}"
        );
        let not_read = sql_refusal(&["memory"], true, query).await;
        assert!(
            not_read.contains("not on this install's list"),
            "{not_read}"
        );
        let named = sql_refusal(
            &["metabase"],
            true,
            "return sql(\"warehouse\") { SELECT 1 }",
        )
        .await;
        assert!(named.contains("must be a numeric database id"), "{named}");
        assert!(named.contains("\"warehouse\""), "{named}");
    }

    /// A native SQL tool that stages every result as a table artifact, as the
    /// real one does once a result is too large to return inline.
    struct StagingNativeQuery(std::sync::Mutex<Vec<serde_json::Value>>);

    #[async_trait]
    impl Tool for StagingNativeQuery {
        fn name(&self) -> &str {
            "metabase.dataset_query_native"
        }
        fn description(&self) -> &str {
            "Run SQL."
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, input: serde_json::Value) -> Result<crate::tools::ToolResult> {
            self.0.lock().unwrap().push(input);
            let store = crate::artifacts::store::install_test_global_store();
            let agent = crate::runner::CURRENT_AGENT_ID.with(|a| a.clone());
            let rows: Vec<serde_json::Value> = (0..3000)
                .map(|i| serde_json::json!({ "id": i, "grp": i % 3 }))
                .collect();
            let staged = crate::artifacts::tabular::stage_table(
                &*store,
                &agent,
                "metabase_native_query",
                Some(vec!["id".into(), "grp".into()]),
                rows,
                1,
            )
            .expect("staged");
            let crate::artifacts::tabular::StagedTable::Staged { envelope, .. } = staged else {
                panic!("expected a staged table");
            };
            Ok(crate::tools::ToolResult::success(
                serde_json::json!({ "data": envelope }),
                0,
            ))
        }
    }

    fn staging_runner(
        llm: &Arc<ScriptedLoadingProvider>,
    ) -> (AgentRunner, Arc<StagingNativeQuery>) {
        let native = Arc::new(StagingNativeQuery(std::sync::Mutex::new(Vec::new())));
        let registry = registry_with(&["memory"]);
        registry.register(Arc::clone(&native) as Arc<dyn Tool>);
        let runner = with_programs(
            make_runner_with_tools(Arc::clone(llm) as Arc<dyn LlmProvider>, Arc::new(registry)),
            &["metabase"],
        );
        (runner, native)
    }

    /// Run `fut` as the hub does a turn: with its agent in scope, which the
    /// artifact store files everything under.
    async fn as_agent<F: std::future::Future>(fut: F) -> F::Output {
        crate::runner::CURRENT_AGENT_ID
            .scope("sift-test".to_string(), fut)
            .await
    }

    #[tokio::test]
    async fn a_program_reads_a_sql_result_too_large_to_return_inline() {
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "let rows = sql(7) { SELECT id, grp FROM t }\nreturn { n: count(rows), twos: count(rows.filter(r => r.grp == 2)) }",
        )]]);
        let (runner, native) = staging_runner(&llm);
        let result = as_agent(run_either(&runner, false)).await;
        let record = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name == crate::tools::program::PROGRAM_TOOL)
            .expect("the program is recorded");
        assert_eq!(
            record.output["returned"],
            serde_json::json!({ "n": 3000, "twos": 1000 }),
            "all 3,000 staged rows reach the program"
        );
        assert_eq!(native.0.lock().unwrap()[0]["limit"], 50_000);
    }

    #[tokio::test]
    async fn a_program_sql_block_writes_booleans_and_null_as_keywords() {
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "return sql(7) { SELECT 1 FROM t WHERE a = ${true} AND b IS ${null} AND c = ${\"x\"} }",
        )]]);
        let (runner, native) = staging_runner(&llm);
        as_agent(run_either(&runner, false)).await;
        let calls = native.0.lock().unwrap();
        let query = calls[0]["query"].as_str().unwrap();
        assert!(
            query.contains("a = TRUE AND b IS NULL AND c = {{p3}}"),
            "{query}"
        );
        assert_eq!(
            calls[0]["template_tags"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["p3"]
        );
    }

    #[tokio::test]
    async fn a_program_hands_the_user_a_file_without_the_rows_reaching_the_model() {
        crate::artifacts::store::install_test_global_store();
        let llm = ScriptedLoadingProvider::new(vec![vec![program(
            "let file = artifact_write(\"orders.xlsx\", [{ id: 1, total: 9.5 }, { id: 2, total: 3 }])\nreturn { file: file.filename, rows: file.rows }",
        )]]);
        let (runner, _) = staging_runner(&llm);
        let result = as_agent(run_either(&runner, false)).await;
        let record = result
            .tool_invocations
            .iter()
            .find(|i| i.tool_name == crate::tools::program::PROGRAM_TOOL)
            .expect("the program is recorded");
        assert!(record.success, "{:?}", record.output);
        assert_eq!(record.output["returned"]["rows"], 2);
        let delivered = crate::tools::image_gen::parse_generated_images(&record.output);
        assert_eq!(
            delivered.len(),
            1,
            "one file is delivered: {:?}",
            record.output
        );
        assert_eq!(delivered[0].filename.as_deref(), Some("orders.xlsx"));
    }

    async fn the_model_is_reminded_of_programs_after_three_lookups(streaming: bool) {
        let llm = ScriptedLoadingProvider::new(vec![
            vec![("memory", serde_json::json!({}))],
            vec![("memory", serde_json::json!({}))],
            vec![("memory", serde_json::json!({}))],
            vec![("memory", serde_json::json!({}))],
        ]);
        let runner = with_programs(
            make_runner_with_tools(
                Arc::clone(&llm) as Arc<dyn LlmProvider>,
                Arc::new(registry_with(&["memory"])),
            ),
            &["memory"],
        );
        run_either(&runner, streaming).await;
        let seen = llm.seen.lock().unwrap();
        let reminder = |round: usize, id: &str| result_of(&seen[round].2, id).0.contains("[Sift]");
        assert!(
            !reminder(1, "tu_1_0") && !reminder(2, "tu_2_0"),
            "not before the third"
        );
        assert!(
            reminder(3, "tu_3_0"),
            "the third lookup's result carries the reminder"
        );
        assert!(!reminder(4, "tu_4_0"), "and only once");
    }

    #[tokio::test]
    async fn the_model_is_reminded_of_programs_after_three_lookups_in_the_plain_loop() {
        the_model_is_reminded_of_programs_after_three_lookups(false).await;
    }

    #[tokio::test]
    async fn the_model_is_reminded_of_programs_after_three_lookups_when_streaming() {
        the_model_is_reminded_of_programs_after_three_lookups(true).await;
    }

    #[tokio::test]
    async fn a_program_runs_again_with_edits_from_the_conversation() {
        let first = "let a = memory({ who: \"Tim Barlow\" })\nreturn a";
        let llm = ScriptedLoadingProvider::new(vec![
            vec![program(first)],
            vec![(
                "program_run",
                serde_json::json!({
                    "program": crate::tools::program::program_id(first),
                    "edits": [{ "find": "Tim Barlow", "replace": "Felipe Dellinger" }]
                }),
            )],
        ]);
        let runner = with_programs(
            make_runner_with_tools(
                Arc::clone(&llm) as Arc<dyn LlmProvider>,
                Arc::new(registry_with(&["memory"])),
            ),
            &["memory"],
        );
        let result = run_either(&runner, false).await;
        let inputs: Vec<_> = result
            .tool_invocations
            .iter()
            .filter(|i| i.tool_name == "memory")
            .map(|i| i.input["who"].clone())
            .collect();
        assert_eq!(
            inputs,
            ["Tim Barlow", "Felipe Dellinger"],
            "the re-run runs the edited program"
        );
        let seen = llm.seen.lock().unwrap();
        let (content, _) = result_of(&seen[1].2, "tu_1_0");
        assert!(
            content.contains(&crate::tools::program::program_id(first)),
            "the result names the program: {content}"
        );
    }

    #[tokio::test]
    async fn a_streamed_program_starts_and_ends_under_one_name() {
        let llm = ScriptedLoadingProvider::new(vec![vec![program("return 1")]]);
        let runner = with_programs(
            make_runner_with_tools(
                Arc::clone(&llm) as Arc<dyn LlmProvider>,
                Arc::new(registry_with(&["memory"])),
            ),
            &["memory"],
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        runner.run_with_progress("go", tx).await.expect("run");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ProgressEvent::ToolStart { name, .. } => events.push(("start", name)),
                ProgressEvent::ToolEnd { name, .. } => events.push(("end", name)),
                _ => {}
            }
        }
        assert_eq!(
            events,
            [
                ("start", "program.run".to_string()),
                ("end", "program.run".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn without_programs_there_is_no_program_tool() {
        let llm = ScriptedLoadingProvider::new(vec![vec![program("return 1")]]);
        let runner = make_runner_with_tools(
            Arc::clone(&llm) as Arc<dyn LlmProvider>,
            Arc::new(registry_with(&["memory"])),
        );
        let result = run_either(&runner, false).await;
        assert_eq!(records(&result), [("program_run", "allow", false)]);
        let seen = llm.seen.lock().unwrap();
        assert!(!seen[0].1.contains(&"program_run".to_string()));
    }

    #[tokio::test]
    async fn a_tool_call_is_checked_against_its_schema_while_validation_shadows() {
        struct Ledger;
        #[async_trait]
        impl Tool for Ledger {
            fn name(&self) -> &str {
                "ledger.add"
            }
            fn description(&self) -> &str {
                "Add to the ledger."
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "count": { "type": "integer" } },
                    "required": ["count"]
                })
            }
            async fn execute(&self, _: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }
        let registry = padded_registry(&["memory"]);
        registry.register(Arc::new(Ledger));
        let llm = ScriptedLoadingProvider::new(vec![vec![
            (
                "tool_call",
                serde_json::json!({ "name": "memory", "arguments": { "count": "x" } }),
            ),
            (
                "tool_call",
                serde_json::json!({ "name": "ledger_add", "arguments": {} }),
            ),
            ("tool_call", serde_json::json!({ "arguments": {} })),
            (
                "tool_call",
                serde_json::json!({ "name": "vault_sett", "arguments": {} }),
            ),
        ]]);
        let mut runner = loading_runner_with(Arc::clone(&llm), &["memory", "ledger.add"], registry);
        runner.config.arguments = crate::tools::arguments::ArgumentValidation::Shadow;
        let result = run_either(&runner, false).await;

        assert_eq!(
            records(&result),
            [
                ("ledger.add", "deny", false),
                ("memory", "allow", true),
                ("tool.call", "allow", false),
                ("tool.call", "allow", false)
            ]
        );
        let seen = llm.seen.lock().unwrap();
        let (rejected, _) = result_of(&seen[1].2, "tu_1_1");
        assert!(
            rejected.starts_with("'ledger.add' did not run"),
            "{rejected}"
        );
        assert!(rejected.contains("Its full input schema:") && rejected.contains("\"count\""));
        let (unnamed, _) = result_of(&seen[1].2, "tu_1_2");
        assert!(unnamed.contains("`name`"));
        let (unknown, _) = result_of(&seen[1].2, "tu_1_3");
        assert!(
            unknown.starts_with("`vault_sett` is not a tool."),
            "{unknown}"
        );
    }

    #[tokio::test]
    async fn a_result_that_loads_tools_carries_their_definitions() {
        struct Enable;
        #[async_trait]
        impl Tool for Enable {
            fn name(&self) -> &str {
                "system.enable_skill"
            }
            fn description(&self) -> &str {
                "Enable a skill."
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(&self, _: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(
                    crate::tools::ToolResult::success(serde_json::json!({ "enabled": true }), 0)
                        .loading(vec!["vault.set".to_string()]),
                )
            }
        }
        let registry = padded_registry(&["memory", "vault.set"]);
        registry.register(Arc::new(Enable));
        let llm = ScriptedLoadingProvider::new(vec![
            vec![("system_enable_skill", serde_json::json!({}))],
            vec![(
                "tool_call",
                serde_json::json!({ "name": "vault_set", "arguments": {} }),
            )],
        ]);
        let runner = loading_runner_with(Arc::clone(&llm), &["system.enable_skill"], registry);
        let result = run_either(&runner, false).await;

        let seen = llm.seen.lock().unwrap();
        let (content, loaded) = result_of(&seen[1].2, "tu_1_0");
        assert!(content.starts_with("{\"enabled\":true}"));
        assert!(content.contains("\"name\":\"vault_set\""));
        assert_eq!(loaded.unwrap().note, "{\"enabled\":true}");
        assert!(records(&result).contains(&("vault.set", "allow", true)));
    }

    #[tokio::test]
    async fn the_round_cap_wrap_up_carries_no_catalogue() {
        let llm = ScriptedLoadingProvider::new(vec![
            vec![("memory", serde_json::json!({}))],
            vec![("memory", serde_json::json!({}))],
        ]);
        let mut runner = loading_runner(Arc::clone(&llm), &["memory"]);
        runner.config.max_rounds = 1;
        runner.run("go").await.expect("run");
        let seen = llm.seen.lock().unwrap();
        let (wrap_up, tools, _) = seen.last().unwrap();
        assert!(tools.is_empty());
        assert!(!wrap_up.contains("## Tool catalogue"));
    }

    /// Records the tools array of every call; answers ToolUse on `tool` for
    /// the first `tool_use_rounds` calls, then ends the turn.
    struct RecordingToolUseProvider {
        tool: &'static str,
        tool_use_rounds: usize,
        calls: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl LlmProvider for RecordingToolUseProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(serde_json::to_vec(tools).expect("tools serialise"));
            let call = calls.len();
            let usage = TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            };
            Ok(match call <= self.tool_use_rounds {
                true => LlmResponse {
                    content: vec![ContentBlock::tool_use(
                        format!("tu_{call}"),
                        self.tool.to_string(),
                        serde_json::json!({}),
                    )],
                    stop_reason: StopReason::ToolUse,
                    usage,
                    model: "mock".to_string(),
                },
                false => LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage,
                    model: "mock".to_string(),
                },
            })
        }
    }

    #[tokio::test]
    async fn a_tool_that_mutates_its_schema_on_execute_does_not_move_the_tools_array() {
        struct CountingSchemaTool {
            executions: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl Tool for CountingSchemaTool {
            fn name(&self) -> &str {
                "counting_tool"
            }
            fn description(&self) -> &str {
                "renders its execution count into its schema"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "executions": self.executions.load(std::sync::atomic::Ordering::SeqCst)
                })
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                self.executions
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }

        let tool = Arc::new(CountingSchemaTool {
            executions: std::sync::atomic::AtomicUsize::new(0),
        });
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::clone(&tool) as Arc<dyn Tool>);
        let llm = Arc::new(RecordingToolUseProvider {
            tool: "counting_tool",
            tool_use_rounds: 2,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let runner = make_runner_with_tools(Arc::clone(&llm) as Arc<dyn LlmProvider>, registry);

        runner.run("go").await.expect("run");

        let calls = llm.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(
            calls.iter().all(|c| *c == calls[0]),
            "the tools array must be byte-identical across the rounds of a turn"
        );
        assert!(String::from_utf8_lossy(&calls[0]).contains("\"executions\":0"));
        assert_eq!(
            tool.input_schema()["executions"],
            serde_json::json!(2),
            "the live schema really did move; only the offered one held still"
        );
    }

    #[tokio::test]
    async fn a_tool_registered_mid_turn_is_offered_on_the_next_round() {
        struct LateRegisteringTool(std::sync::Weak<ToolRegistry>);

        #[async_trait]
        impl Tool for LateRegisteringTool {
            fn name(&self) -> &str {
                "late_registering"
            }
            fn description(&self) -> &str {
                "registers another tool while it runs"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                if let Some(registry) = self.0.upgrade() {
                    registry.register(mock_tool("late.tool"));
                }
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!("ok"),
                    0,
                ))
            }
        }

        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(LateRegisteringTool(Arc::downgrade(&registry))));
        let llm = Arc::new(RecordingToolUseProvider {
            tool: "late_registering",
            tool_use_rounds: 1,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let runner = make_runner_with_tools(Arc::clone(&llm) as Arc<dyn LlmProvider>, registry);

        runner.run("go").await.expect("run");

        let calls = llm.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let offered = |call: &[u8]| String::from_utf8_lossy(call).contains("\"late_tool\"");
        assert!(!offered(&calls[0]), "not there before it was registered");
        assert!(
            offered(&calls[1]),
            "a mid-turn registration lands next round (DW-277)"
        );
    }

    // ── self-serve approval prompts ─────────────────────────────

    mod self_serve {
        use super::*;
        use crate::tools::approval::testing::{
            as_member, gated_runner, RecordingTool, ScriptedModel, MEMBER,
        };
        use crate::tools::approval::Refusal;
        use workforce_core::policy::PermissionChoice;

        struct GatedRun {
            result: RunResult,
            prompts: Vec<(String, String)>,
            elapsed: std::time::Duration,
        }

        impl GatedRun {
            fn details(&self) -> Vec<&str> {
                self.prompts
                    .iter()
                    .map(|(_, detail)| detail.as_str())
                    .collect()
            }

            /// What the model read back for each call that did not succeed.
            fn failures(&self) -> Vec<&str> {
                self.result
                    .tool_invocations
                    .iter()
                    .filter_map(|call| call.error.as_deref())
                    .collect()
            }

            fn refused_as(&self, refusal: Refusal) -> usize {
                self.failures()
                    .iter()
                    .filter(|failure| **failure == refusal.tool_result())
                    .count()
            }
        }

        fn entry(name: &str) -> serde_json::Value {
            serde_json::json!({ "name": name })
        }

        fn every_prompt(
            choice: PermissionChoice,
        ) -> impl Fn(&str) -> Vec<(String, PermissionChoice)> {
            move |request_id| vec![(request_id.to_string(), choice)]
        }

        /// Run a gated turn to completion, answering each prompt it raises
        /// with whatever `answer` returns. A turn run `attended = false` has
        /// no channel for answers at all.
        async fn run_gated(
            model: ScriptedModel,
            tool: &Arc<RecordingTool>,
            attended: bool,
            answer: impl Fn(&str) -> Vec<(String, PermissionChoice)>,
        ) -> GatedRun {
            let runner = gated_runner(model, Arc::clone(tool));
            let (progress_tx, mut progress_rx) = mpsc::channel(64);
            let (inject_tx, inject_rx) = mpsc::channel(8);
            let started = tokio::time::Instant::now();
            let run = tokio::spawn(as_member(async move {
                runner
                    .run_with_progress_and_history(
                        vec![ContentBlock::Text {
                            text: "remove the entry".into(),
                        }],
                        Vec::new(),
                        progress_tx,
                        None,
                        attended.then_some(inject_rx),
                    )
                    .await
            }));
            let mut prompts = Vec::new();
            while let Some(event) = progress_rx.recv().await {
                if let ProgressEvent::ApprovalRequired {
                    request_id, detail, ..
                } = event
                {
                    for (id, choice) in answer(&request_id) {
                        let _ = inject_tx
                            .send(InjectedMessage::ApprovalResponse {
                                request_id: id,
                                approved: choice.is_allow(),
                                user: MEMBER.into(),
                                choice: Some(choice),
                            })
                            .await;
                    }
                    prompts.push((request_id, detail));
                }
            }
            GatedRun {
                result: run.await.expect("run task").expect("run completes"),
                prompts,
                elapsed: started.elapsed(),
            }
        }

        #[tokio::test]
        async fn an_allowed_prompt_runs_the_call_once_with_the_input_shown() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let run = run_gated(
                model,
                &tool,
                true,
                every_prompt(PermissionChoice::AllowOnce),
            )
            .await;
            assert_eq!(run.details(), ["figma"]);
            assert_eq!(tool.runs(), [entry("figma")]);
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test]
        async fn a_call_arriving_without_its_required_input_is_corrected_not_prompted() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([serde_json::json!({})], entry("figma"), 4);
            let run = run_gated(
                model,
                &tool,
                true,
                every_prompt(PermissionChoice::AllowOnce),
            )
            .await;
            assert_eq!(
                run.details(),
                ["figma"],
                "only the complete call is put to the user"
            );
            assert_eq!(
                run.failures(),
                [
                    "'catalogue.remove_entry' was called without required input `name`, so it did \
                  not run and no approval was requested. Call it again with every required field."
                ]
            );
            assert_eq!(tool.runs(), [entry("figma")]);
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test]
        async fn a_denied_call_is_refused_again_without_a_new_prompt() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let run = run_gated(model, &tool, true, every_prompt(PermissionChoice::DenyOnce)).await;
            assert_eq!(
                run.prompts.len(),
                1,
                "a denial is not put to the user again"
            );
            assert_eq!(run.refused_as(Refusal::Denied), 4, "{:?}", run.failures());
            assert!(run
                .result
                .tool_invocations
                .iter()
                .all(|call| call.policy_decision == "require_approval"));
            assert!(tool.runs().is_empty());
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test]
        async fn an_approved_call_that_fails_is_not_prompted_again() {
            let tool = Arc::new(RecordingTool::failing());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let run = run_gated(
                model,
                &tool,
                true,
                every_prompt(PermissionChoice::AllowOnce),
            )
            .await;
            assert_eq!(run.prompts.len(), 1);
            assert_eq!(tool.runs(), [entry("figma")], "allow_once covers one run");
            assert_eq!(run.failures()[0], "the catalogue is unavailable");
            assert_eq!(run.refused_as(Refusal::Failed), 3, "{:?}", run.failures());
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test]
        async fn two_calls_to_one_tool_in_a_round_are_two_decisions() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma"), entry("probe")], entry("figma"), 4);
            let run = run_gated(
                model,
                &tool,
                true,
                every_prompt(PermissionChoice::AllowOnce),
            )
            .await;
            assert_eq!(run.prompts.len(), 2);
            assert_ne!(
                run.prompts[0].0, run.prompts[1].0,
                "each call has its own prompt"
            );
            let mut runs = tool.runs();
            runs.sort_by_key(|input| input["name"].as_str().map(str::to_string));
            assert_eq!(runs, [entry("figma"), entry("probe")]);
        }

        #[tokio::test]
        async fn an_unknown_answer_resolves_nothing() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let run = run_gated(model, &tool, true, |request_id| {
                vec![
                    ("not-a-request".to_string(), PermissionChoice::AllowOnce),
                    (request_id.to_string(), PermissionChoice::DenyOnce),
                ]
            })
            .await;
            assert!(
                tool.runs().is_empty(),
                "only the prompt's own answer counts"
            );
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test(start_paused = true)]
        async fn an_unanswered_prompt_is_refused_and_not_raised_again() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 3);
            let run = run_gated(model, &tool, true, |_| Vec::new()).await;
            assert_eq!(run.prompts.len(), 1);
            assert_eq!(
                run.elapsed, SELF_SERVE_APPROVAL_TIMEOUT,
                "one prompt waited"
            );
            assert_eq!(
                run.refused_as(Refusal::Unanswered),
                3,
                "{:?}",
                run.failures()
            );
            assert!(tool.runs().is_empty());
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test(start_paused = true)]
        async fn a_prompt_nobody_can_answer_is_refused_at_once() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 3);
            let run = run_gated(model, &tool, false, |_| Vec::new()).await;
            assert!(run.prompts.is_empty(), "nobody could answer a prompt");
            assert_eq!(
                run.refused_as(Refusal::Unattended),
                3,
                "{:?}",
                run.failures()
            );
            assert!(tool.runs().is_empty());
            assert_eq!(run.elapsed, std::time::Duration::ZERO, "nothing waited");
            assert_eq!(run.result.response, "done");
        }

        #[tokio::test]
        async fn an_approved_call_is_scanned_like_any_other() {
            let tool = Arc::new(RecordingTool::default());
            let smuggled = entry("${CATALOGUE_TOKEN}");
            let model = ScriptedModel::new([smuggled.clone()], smuggled, 4);
            let run = run_gated(
                model,
                &tool,
                true,
                every_prompt(PermissionChoice::AllowOnce),
            )
            .await;
            assert!(
                tool.runs().is_empty(),
                "the sentinel still blocks an approved call"
            );
            assert!(
                run.failures()[0].starts_with("BLOCKED by security policy"),
                "{:?}",
                run.failures()
            );
            assert_eq!(run.result.response, "done");
        }
    }

    mod compile_gate {
        use super::*;
        use crate::compile_gate::testing::{failed, settings, ScriptedGate, BUILD};
        use crate::compile_gate::CheckReport;

        enum Step {
            Edit(&'static str),
            Answer(&'static str),
        }

        /// Plays `steps` one per model call, recording each call's messages.
        struct Script {
            steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
            seen: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
        }

        impl Script {
            fn new(steps: Vec<Step>) -> Arc<Self> {
                Arc::new(Self {
                    steps: std::sync::Mutex::new(steps.into()),
                    seen: std::sync::Mutex::default(),
                })
            }

            fn prompt_text(&self, call: usize) -> String {
                self.seen.lock().unwrap()[call]
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }

        #[async_trait]
        impl LlmProvider for Script {
            async fn create_message(
                &self,
                _system_prompt: Option<&str>,
                messages: &[LlmMessage],
                _tools: &[LlmToolDef],
                _model: Option<&str>,
                _max_tokens: Option<u32>,
                _enable_cache: bool,
            ) -> Result<LlmResponse> {
                let call = {
                    let mut seen = self.seen.lock().unwrap();
                    seen.push(messages.to_vec());
                    seen.len()
                };
                let step = self.steps.lock().unwrap().pop_front();
                let (content, stop_reason) = match step.expect("model called past the script") {
                    Step::Edit(path) => (
                        vec![ContentBlock::tool_use(
                            format!("tu_{call}"),
                            "edit".to_string(),
                            serde_json::json!({ "path": path }),
                        )],
                        StopReason::ToolUse,
                    ),
                    Step::Answer(text) => (
                        vec![ContentBlock::Text {
                            text: text.to_string(),
                        }],
                        StopReason::EndTurn,
                    ),
                };
                Ok(LlmResponse {
                    content,
                    stop_reason,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                    model: "mock".to_string(),
                })
            }
        }

        struct Named(&'static str);

        #[async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "test tool"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _input: serde_json::Value) -> Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult::success(
                    serde_json::json!({"ok": true}),
                    0,
                ))
            }
        }

        fn runner(
            script: &Arc<Script>,
            gate: &Arc<ScriptedGate>,
            max_fix_rounds: u32,
            may_build: bool,
        ) -> AgentRunner {
            let tools = Arc::new(ToolRegistry::new());
            tools.register(Arc::new(Named("edit")));
            if may_build {
                tools.register(Arc::new(Named(BUILD)));
            }
            let config = RunnerConfig {
                agent_id: workforce_core::types::AgentId::new("test"),
                model: Some("mock".to_string()),
                max_rounds: 10,
                compile_gate: Some(settings(gate, max_fix_rounds)),
                ..Default::default()
            };
            AgentRunner::new(
                config,
                Arc::clone(script) as Arc<dyn LlmProvider>,
                tools,
                Arc::new(
                    workforce_policy::engine::PolicyEngine::from_yaml(
                        "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                    )
                    .expect("policy parse"),
                ),
            )
        }

        #[tokio::test]
        async fn a_failing_build_goes_back_to_the_agent_and_a_fixed_one_answers_plainly() {
            let script = Script::new(vec![
                Step::Edit("src/lib.rs"),
                Step::Answer("done, first try"),
                Step::Edit("src/lib.rs"),
                Step::Answer("done, fixed"),
            ]);
            let gate =
                ScriptedGate::new(vec![failed("E0425 cannot find value"), CheckReport::Passed]);
            let result = runner(&script, &gate, 2, true).run("fix it").await.unwrap();

            assert_eq!(result.response, "done, fixed");
            assert_eq!(gate.checks(), 2);
            let fix_prompt = script.prompt_text(2);
            assert!(
                fix_prompt.contains("E0425 cannot find value"),
                "{fix_prompt}"
            );
            assert!(
                fix_prompt.contains("done, first try"),
                "the draft stays in the conversation"
            );
        }

        #[tokio::test]
        async fn fix_rounds_are_bounded_and_the_answer_says_the_build_fails() {
            let script = Script::new(vec![
                Step::Edit("src/lib.rs"),
                Step::Answer("v1"),
                Step::Edit("src/lib.rs"),
                Step::Answer("v2"),
            ]);
            let gate = ScriptedGate::new(vec![failed("E1"), failed("E2")]);
            let result = runner(&script, &gate, 1, true).run("fix it").await.unwrap();

            assert!(result.response.starts_with("v2"), "{}", result.response);
            assert!(result.response.contains("The build is failing"));
            assert!(result.response.contains("E2"));
            assert_eq!(gate.checks(), 2);
            assert_eq!(
                script.seen.lock().unwrap().len(),
                4,
                "no round past the bound"
            );
        }

        #[tokio::test]
        async fn the_streaming_loop_rewinds_the_draft_and_streams_the_failure() {
            let script = Script::new(vec![
                Step::Edit("src/lib.rs"),
                Step::Answer("v1"),
                Step::Answer("v2"),
            ]);
            let gate = ScriptedGate::new(vec![failed("E1")]);
            let (tx, mut rx) = mpsc::channel(1024);
            let result = runner(&script, &gate, 2, true)
                .run_with_progress("fix it", tx)
                .await
                .unwrap();

            assert!(result.response.starts_with("v2"), "{}", result.response);
            assert!(
                result.response.contains("E1"),
                "a fix round that edits nothing keeps the failure"
            );
            assert_eq!(gate.checks(), 1);
            let mut rewound = 0;
            let mut streamed = String::new();
            while let Ok(event) = rx.try_recv() {
                match event {
                    ProgressEvent::StreamDiscard {
                        reason: StreamDiscardReason::Retry,
                    } => {
                        rewound += 1;
                        streamed.clear();
                    }
                    ProgressEvent::StreamDelta { text } => streamed.push_str(&text),
                    _ => {}
                }
            }
            assert_eq!(rewound, 1);
            assert!(
                script.prompt_text(2).contains("v1"),
                "the draft stays in the conversation"
            );
            assert_eq!(
                streamed, result.response,
                "what stays on screen is the answer, notice once"
            );
            let notice = result.gate_notice.as_deref().expect("the failure");
            assert!(notice.starts_with("**The build is failing.**"));
            assert_eq!(result.agent_response(), "v2");
        }

        #[tokio::test]
        async fn an_empty_draft_never_reaches_the_model_as_an_empty_block() {
            for streaming in [false, true] {
                let script = Script::new(vec![
                    Step::Edit("src/lib.rs"),
                    Step::Answer(""),
                    Step::Answer("fixed"),
                ]);
                let gate = ScriptedGate::new(vec![failed("E1")]);
                let runner = runner(&script, &gate, 2, true);
                let result = match streaming {
                    true => {
                        let (tx, _rx) = mpsc::channel(1024);
                        runner.run_with_progress("fix it", tx).await.unwrap()
                    }
                    false => runner.run("fix it").await.unwrap(),
                };
                assert!(result.response.starts_with("fixed"), "{}", result.response);
                let fix_round = script.seen.lock().unwrap()[2].clone();
                assert!(
                    fix_round.iter().flat_map(|m| m.content.iter()).all(
                        |block| !matches!(block, ContentBlock::Text { text } if text.trim().is_empty())
                    ),
                    "streaming: {streaming}"
                );
                assert!(script.prompt_text(2).contains("E1"));
            }
        }

        #[tokio::test]
        async fn a_turn_out_of_rounds_still_reports_a_failing_build() {
            let script = Script::new(vec![
                Step::Edit("src/lib.rs"),
                Step::Edit("src/lib.rs"),
                Step::Answer("wrap-up"),
            ]);
            let gate = ScriptedGate::new(vec![failed("E1")]);
            let mut runner = runner(&script, &gate, 2, true);
            runner.config.max_rounds = 2;
            let result = runner.run("fix it").await.unwrap();

            assert!(result.hit_round_cap);
            assert!(
                result.response.starts_with("wrap-up"),
                "{}",
                result.response
            );
            assert!(result.response.contains("The build is failing"));
            assert_eq!(gate.checks(), 1);
        }

        #[tokio::test]
        async fn the_non_streamed_answer_carries_the_notice_apart_from_the_agents_words() {
            let script = Script::new(vec![Step::Edit("src/lib.rs"), Step::Answer("done")]);
            let gate = ScriptedGate::new(vec![CheckReport::Inconclusive {
                reason: "`cargo check` timed out after 600s".into(),
            }]);
            let result = runner(&script, &gate, 2, true).run("fix it").await.unwrap();

            assert!(result
                .response
                .starts_with("done\n\n**The build was not verified"));
            assert!(result.gate_notice.is_some());
            assert_eq!(result.agent_response(), "done");
        }

        #[tokio::test]
        async fn a_turn_that_may_not_build_answers_unchecked() {
            let script = Script::new(vec![Step::Edit("src/lib.rs"), Step::Answer("done")]);
            let gate = ScriptedGate::new(vec![]);
            let result = runner(&script, &gate, 2, false)
                .run("fix it")
                .await
                .unwrap();

            assert_eq!(result.response, "done");
            assert_eq!(gate.checks(), 0);
        }
    }
}

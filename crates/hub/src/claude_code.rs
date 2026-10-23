//! Claude Code CLI integration — subprocess spawning and API key resolution.
//!
//! Shared by both the `claude-code` workflow step engine and the `coding.claude`
//! ad-hoc tool. Spawns `claude -p` as a subprocess and captures the result.
//!
//! ## Cost Safety
//!
//! Errors NEVER fall through to the normal agent loop or API providers.
//! `run_with_retry` retries once on transient failures then hard-stops, and all
//! attempts share one wall clock ([`ClaudeCodeConfig::timeout`]).
//!
//! ## Budget exhaustion is not failure
//!
//! Running out of turns or wall clock ([`Completion::TurnCap`] /
//! [`Completion::Timeout`]) yields whatever the round produced rather than an
//! error, and is never retried. A capped round has typically done real work —
//! possibly an already-pushed branch and PR — so discarding it loses the work
//! and re-running it risks duplicating what landed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;
use workforce_agent::llm::types::TokenUsage;
use workforce_agent::runner::ProgressEvent;
use workforce_agent::tools::exec_runner::{
    CodexMcpConfig, ExecRequest, ExecSummary, GroupChild, JobId, McpBinding, McpResponder,
    RunnerEndpoint, ScopeId,
};

use crate::mcp::{McpEndpoint, MCP_SERVER_KEY};
use crate::subscription::limits::{is_rate_limited, is_usage_capped, looks_like_auth_failure};
use crate::vault::Vault;
pub use workforce_agent::llm::bedrock_credentials::BedrockCredentials;

/// Stream output past which a round is worth a warning. It is read to the end
/// all the same: Claude Code's result and Codex's `turn.completed` come last.
pub(crate) const LARGE_OUTPUT_BYTES: usize = 1024 * 1024;

/// Whether a line of `len` bytes, after `before` bytes, carries a stream past
/// [`LARGE_OUTPUT_BYTES`].
pub(crate) fn passes_large_output(before: usize, len: usize) -> bool {
    before <= LARGE_OUTPUT_BYTES && before + len > LARGE_OUTPUT_BYTES
}

/// `before` plus a line of `len` bytes from `program`'s stream, warning on the
/// one line that carries the stream past [`LARGE_OUTPUT_BYTES`].
pub(crate) fn count_output(program: &str, before: usize, len: usize) -> usize {
    let total_bytes = before + len;
    if passes_large_output(before, len) {
        warn!(
            target: "cc",
            program,
            total_bytes,
            "CLI output passed 1 MiB; reading on to the end of the stream"
        );
    }
    total_bytes
}

const DEFAULT_RETRY_DELAY_SECS: u64 = 60;

/// Backoff schedule for rate-limit (429) retries, in seconds. Sized to the typical
/// Max OAuth bucket-refill cadence: at 30k input TPM and ~100k cache writes from a
/// big resumed-session prefix, the bucket needs ~3-4 minutes to recover. A short
/// initial wait would hit 429 again immediately; the schedule front-loads enough to
/// give the bucket a fighting chance.
const RATE_LIMIT_BACKOFF_SECS: &[u64] = &[180, 240];

#[derive(Clone)]
pub struct ClaudeCodeConfig {
    pub task: String,
    pub workdir: PathBuf,
    pub max_turns: Option<u32>,
    pub allowed_tools: Vec<String>,
    pub append_system_prompt: Option<String>,
    /// Wall clock for the whole [`run_with_retry`] call, shared across its
    /// attempts — not a fresh window per attempt.
    pub timeout: Duration,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The hub tools the round is served over MCP, bound to whoever it runs
    /// for; `None` gives it none.
    pub mcp: Option<crate::mcp_turn::RoundTools>,
    /// UUID for the Claude Code session. `None` = one-shot.
    /// `Some(..)` paired with `resume = false` creates a new session at this UUID;
    /// `Some(..)` paired with `resume = true` continues an existing one.
    pub session_id: Option<Uuid>,
    pub resume: bool,
    /// Keep the session transcript on disk so a later invocation can `--resume`.
    /// `false` = workflow-step / ad-hoc behaviour (`--no-session-persistence`).
    pub persist_session: bool,
    /// Auto-discover and load CLAUDE.md files from the workdir tree.
    /// `false` sets `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`.
    pub load_claude_md: bool,
    /// Load auto-memory (`MEMORY.md` index and referenced files) the way the
    /// interactive CLI does. `false` sets `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`.
    pub load_auto_memory: bool,
    /// GitHub token injected as `GH_TOKEN`/`GITHUB_TOKEN` for the subprocess so
    /// `gh` and `git push` authenticate. Repo-aware callers resolve this
    /// per-owner via [`crate::user_config::resolve_github_pat`]; `None` lets
    /// [`run`] fall back to the flat `skills/github/token` (then env).
    pub gh_token: Option<String>,
    /// Persistent toolchain-cache root (`[coding_session] cache_dir`), exported
    /// to the subprocess as the cache locations in [`toolchain_cache_env`].
    pub cache_dir: Option<PathBuf>,
    /// Run in a prepared scope on the remote worker instead of on this host.
    /// `workdir` is then a path inside that scope.
    pub remote: Option<RemotePlacement>,
    /// Whose account the round bills: the user's subscription, or Bedrock on
    /// credentials minted for this round.
    pub auth: ClaudeAuth,
    /// The durable jobs of a keyed round; `None` for a round nothing will
    /// collect after a restart.
    pub jobs: Option<RoundJobs>,
    /// Runs the round contained; `None` runs it with the CLI's own tools,
    /// settings and system prompt.
    pub contained: Option<Contained>,
}

impl ClaudeCodeConfig {
    /// A contained round in `workdir`, paid by `login`, on `model`, for at
    /// most `timeout`: nothing loaded, handed over or kept beyond what
    /// `contained` gives it.
    pub fn contained(
        workdir: PathBuf,
        login: crate::subscription::Login,
        contained: Contained,
        model: String,
        timeout: Duration,
    ) -> Self {
        Self {
            task: String::new(),
            workdir,
            max_turns: None,
            allowed_tools: Vec::new(),
            append_system_prompt: None,
            timeout,
            model: Some(model),
            effort: None,
            mcp: None,
            session_id: None,
            resume: false,
            persist_session: false,
            load_claude_md: false,
            load_auto_memory: false,
            gh_token: None,
            cache_dir: None,
            remote: None,
            auth: ClaudeAuth::Resolved(login),
            jobs: None,
            contained: Some(contained),
        }
    }
}

/// Where a keyed round's durable jobs get their ids. Every attempt of every
/// engine in the chain is named from the round, and each id is journalled as
/// the round's checkpoint before its job starts. Collecting the round after a
/// restart resumes at the last checkpoint — starting that job if it never
/// did, attaching to it otherwise — and decides nothing past it, so no job is
/// ever started twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundJobs {
    pub round: String,
    /// When the round first started: its budget is spent from then, however
    /// often it is driven.
    pub started_at: chrono::DateTime<chrono::Utc>,
    engine: usize,
    attempt: u32,
    /// Whether the round runs only this job, as when it is collected.
    pinned: bool,
}

impl RoundJobs {
    pub fn new(round: String, started_at: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            round,
            started_at,
            engine: 0,
            attempt: 0,
            pinned: false,
        }
    }

    /// The job `mark` names, pinned: the round resumes there and nowhere
    /// else. `None` when `mark` is not one of `round`'s jobs.
    pub fn resuming(
        round: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        mark: &str,
    ) -> Option<Self> {
        let (engine, attempt) = mark
            .strip_prefix(round)?
            .strip_prefix('.')?
            .split_once('.')?;
        Some(Self {
            engine: engine.parse().ok()?,
            attempt: attempt.parse().ok()?,
            pinned: true,
            ..Self::new(round.to_string(), started_at)
        })
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// The position in the chain of the engine this job runs on.
    pub fn engine(&self) -> usize {
        self.engine
    }

    /// The jobs of the engine at `index` in the round's chain.
    pub fn for_engine(&self, index: usize) -> Self {
        Self {
            engine: index,
            ..self.clone()
        }
    }

    /// The job of the engine's `attempt`th attempt, counting from zero; a
    /// pinned round keeps its one job.
    fn for_attempt(&self, attempt: u32) -> Self {
        match self.pinned {
            true => self.clone(),
            false => Self {
                attempt,
                ..self.clone()
            },
        }
    }

    pub fn job(&self) -> JobId {
        JobId::new(format!("{}.{}.{}", self.round, self.engine, self.attempt))
    }

    /// How much of the round's wall clock has gone.
    fn spent(&self) -> Duration {
        (chrono::Utc::now() - self.started_at)
            .to_std()
            .unwrap_or_default()
    }
}

/// A keyed round: the legs it runs in, its first and the cold restart that
/// replaces it once its driver transcript turns out lost, and the jobs each
/// leg's attempts are named by.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoundKey {
    pub round: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Where a collected round picks up: the leg its last recorded job belongs
/// to, and that job.
pub struct Resume {
    pub cold: bool,
    pub jobs: RoundJobs,
}

impl RoundKey {
    pub fn new() -> Self {
        Self {
            round: Uuid::new_v4(),
            started_at: chrono::Utc::now(),
        }
    }

    fn leg(&self, cold: bool) -> String {
        match cold {
            false => self.round.to_string(),
            true => format!("{}-cold", self.round),
        }
    }

    /// The jobs of the first leg, or of the cold restart.
    pub fn jobs(&self, cold: bool) -> RoundJobs {
        RoundJobs::new(self.leg(cold), self.started_at)
    }

    /// Where the round resumes from `mark`, its last recorded job; `None`
    /// when `mark` is not one of its jobs.
    pub fn resume_at(&self, mark: &str) -> Option<Resume> {
        [false, true].into_iter().find_map(|cold| {
            RoundJobs::resuming(&self.leg(cold), self.started_at, mark)
                .map(|jobs| Resume { cold, jobs })
        })
    }

    /// The driver session a cold restart moves to: the same however often the
    /// round is driven.
    pub fn cold_uuid(&self) -> Uuid {
        Uuid::new_v5(&self.round, b"cold")
    }
}

impl Default for RoundKey {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a round found its driver transcript gone.
pub(crate) fn transcript_lost(result: &Result<ClaudeCodeResult>) -> bool {
    matches!(result, Ok(r) if r.completion == Completion::SessionNotFound)
}

/// How the `claude` subprocess authenticates.
#[derive(Clone, Debug)]
pub enum ClaudeAuth {
    /// A Claude login the subscription resolver already chose for the payer.
    Resolved(crate::subscription::Login),
    /// Bedrock, with credentials that outlive nothing but the round.
    Bedrock(BedrockSession),
}

/// A round with nothing but what it is given: the built-in `tools` it keeps,
/// no MCP server or setting beyond its own, its system prompt from a file,
/// and its prompt as one stream-json user message.
#[derive(Clone, Debug)]
pub struct Contained {
    pub tools: Vec<String>,
    pub system_prompt_file: PathBuf,
    /// The user message's content blocks.
    pub input: Vec<serde_json::Value>,
    /// How long the CLI waits on one MCP call; `None` leaves its default.
    pub tool_timeout: Option<Duration>,
    /// Moves the per-machine sections out of the system prompt, as an agent
    /// turn's does to keep its prompt cached across turns.
    pub exclude_dynamic: bool,
}

impl Contained {
    /// The stdin a contained round reads: its one user message, as a JSON
    /// line.
    pub(crate) fn user_line(&self) -> String {
        let message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": self.input },
        });
        format!("{message}\n")
    }
}

/// Everything a `claude` subprocess needs to talk to Bedrock instead of
/// Anthropic's API. The credentials print redacted.
#[derive(Clone, Debug)]
pub struct BedrockSession {
    pub region: String,
    pub credentials: BedrockCredentials,
    /// The inference-profile ids the CLI's `opus`, `sonnet` and `haiku`
    /// aliases resolve to, so its background tasks never pick a model the
    /// account lacks.
    pub pins: ModelPins,
}

/// Inference-profile ids for the CLI's model aliases, most to least capable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPins {
    pub opus: String,
    pub sonnet: String,
    pub haiku: String,
}

/// A prepared scope on the remote worker.
#[derive(Clone)]
pub struct RemotePlacement {
    pub endpoint: RunnerEndpoint,
    pub scope: ScopeId,
    /// Whether the scope runs rounds as durable jobs, which outlive a lost
    /// connection and a restart of the worker.
    pub jobs: bool,
}

/// The event vocabulary of a CLI's JSON stream: what to keep from each event,
/// the progress it implies, and when the stream has said all it will.
pub trait StreamGrammar: Default + Send + 'static {
    fn record(&mut self, event: &serde_json::Value) -> Vec<ProgressEvent>;

    /// The stream has delivered its result; a process that lingers past it
    /// is ended after a grace.
    fn finished(&self) -> bool;

    /// What the model calls the stream has reported so far used.
    fn spent(&self) -> TokenUsage {
        TokenUsage::default()
    }

    /// The thread the stream started, for a CLI that names it only once it
    /// runs.
    fn thread(&self) -> Option<String> {
        None
    }
}

/// A CLI run the transport carries: the command, its stdin, its wall clock,
/// and where it runs.
pub struct CliInvocation {
    pub command: Command,
    pub stdin: String,
    pub timeout: Duration,
    /// A prepared scope on the remote worker; `None` runs on this host, in
    /// process or on the hub-local runner.
    pub remote: Option<RemotePlacement>,
    /// Answers a remote run's tunnelled MCP calls; a run on this host reaches
    /// its tools over the loopback listener its config names instead.
    pub mcp: Option<McpEndpoint>,
    /// Asks the worker to write its MCP endpoint into a file inside the
    /// remote scope in Codex's config grammar as well, beside the
    /// Claude-format JSON it always writes.
    pub codex_mcp_config: Option<CodexMcpConfig>,
    /// The durable job this run is; `None` mints a fresh one.
    pub job: Option<JobId>,
}

/// The hub's own home, path and module path, handed to a coding round and its
/// workspace setup alike so both resolve the same toolchain.
pub fn inherited_toolchain_env() -> Vec<(String, String)> {
    ["HOME", "PATH", "NODE_PATH"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok().map(|val| (var.to_string(), val)))
        .collect()
}

/// The environment that points every toolchain a coding round may run at one
/// persistent cache root, so installs and builds stay warm across rounds and
/// restarts. Shared by the round itself and the workspace setup command, so
/// what setup installs is what the round finds.
pub fn toolchain_cache_env(cache_dir: &Path) -> Vec<(String, String)> {
    [
        ("XDG_DATA_HOME", "share"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("NPM_CONFIG_CACHE", "npm"),
        ("COREPACK_HOME", "corepack"),
        ("CARGO_HOME", "cargo"),
        ("GRADLE_USER_HOME", "gradle"),
        ("BUNDLE_PATH", "bundle"),
    ]
    .into_iter()
    .map(|(var, sub)| (var.to_string(), cache_dir.join(sub).display().to_string()))
    .collect()
}

#[derive(Default)]
pub struct ClaudeCodeResult {
    pub text: String,
    /// The engine that ran the round (see `coding_engine`); empty for a
    /// round run outside one.
    pub engine: String,
    /// The model the round asked for.
    pub model: Option<String>,
    pub exit_code: i32,
    pub duration: Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
    pub num_turns: u32,
    /// Why the round ended. Every retry / classification / surfacing decision
    /// switches on this rather than re-deriving it from [`Self::exit_code`],
    /// which keeps its raw subprocess meaning for callers that predate it.
    pub completion: Completion,
    /// Best-effort classification of *why* the subprocess exited non-zero.
    /// `None` on success or when no signal is meaningful (rate-limit retries
    /// already have their own surfacing). Surfaced into the Slack failure
    /// card so the user can see "killed (likely OOM)" instead of an opaque
    /// "exit 1" — the failure mode that ate two of last night's runs.
    pub crash_kind: Option<CrashKind>,
    /// Last lines of stderr captured from the subprocess. Truncated to
    /// `STDERR_TAIL_BYTES` so a runaway logger doesn't blow up the card.
    /// Empty when stderr was empty or the process exited cleanly.
    pub stderr_tail: String,
    /// What the CLI itself reported of a failed round — its own error, never
    /// the model's words or another process's stderr: all a login's refusal
    /// may be read from.
    pub notice: String,
    /// The earlier attempts a fallback chain moved on from before this round
    /// served, oldest first; their usage is not in this result's own counts.
    pub handed_over: Vec<workforce_orchestrator::workflow::instance::EngineAttempt>,
    /// What the round reported about its login's usage limit.
    pub limit: Option<crate::subscription::LimitSignal>,
    /// The durable job the round ran as; `None` for a round nothing will
    /// collect after a restart. Keys its cost row, so a job collected again
    /// is recorded once.
    pub job: Option<JobId>,
}

impl ClaudeCodeResult {
    /// What the round's own model calls used, as a provider reports usage.
    pub fn usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: saturating_tokens(self.input_tokens),
            output_tokens: saturating_tokens(self.output_tokens),
            cache_creation_input_tokens: saturating_tokens(self.cache_creation_tokens),
            cache_read_input_tokens: saturating_tokens(self.cache_read_tokens),
        }
    }

    /// The last non-blank line of a failed round's account of itself (the
    /// refusal, the API error, or where its narration stopped), else how the
    /// process ended: the text a retry or a hand-over was decided on.
    pub fn failure_line(&self) -> String {
        self.text
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty() && *line != "…")
            .map(clipped)
            .or_else(|| self.crash_kind.as_ref().map(CrashKind::label))
            .unwrap_or_else(|| format!("exit {}", self.exit_code))
    }

    /// What this round says about the login it ran on.
    pub fn round_end(&self) -> crate::hub_core::logins::resolve::RoundEnd<'_> {
        crate::hub_core::logins::resolve::RoundEnd {
            limit: self.limit.clone(),
            failed: self.completion == Completion::Failed,
            notice: &self.notice,
            stderr_tail: &self.stderr_tail,
            failure_line: self.failure_line(),
        }
    }
}

const HEADLINE_CHARS: usize = 300;

/// The first non-blank line of an error's text, redacted and clipped, as logs
/// and records carry it.
pub(crate) fn headline(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(clipped)
}

/// `line` with its secrets redacted and cut to [`HEADLINE_CHARS`]. Redacted
/// before the cut, so no part of a secret outlives a pattern the cut broke.
fn clipped(line: &str) -> String {
    let redacted = crate::redact::redact_secrets(line);
    match redacted.char_indices().nth(HEADLINE_CHARS) {
        Some((cut, _)) => format!("{}…", &redacted[..cut]),
        None => redacted,
    }
}

/// Whether an error is the caller's own cancellation rather than a failure.
pub(crate) fn is_cancelled(error: &eyre::Report) -> bool {
    error.to_string().contains("cancelled")
}

/// A round its caller cancelled, what its model calls had used by then, and
/// the thread it had started, which a later round resumes.
#[derive(Debug, thiserror::Error)]
#[error("{program} cancelled")]
pub struct Interrupted {
    program: String,
    pub spent: TokenUsage,
    pub thread: Option<String>,
}

impl Interrupted {
    fn of(program: String, capture: &impl StreamGrammar) -> Self {
        Self {
            program,
            spent: capture.spent(),
            thread: capture.thread(),
        }
    }
}

/// A stream's token count as a provider reports one, capped rather than
/// wrapped.
fn saturating_tokens(tokens: u64) -> u32 {
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

/// Why a Claude Code round ended.
///
/// Budget exhaustion ([`Self::TurnCap`], [`Self::Timeout`]) is not failure: the
/// subprocess did real work — quite possibly a pushed branch and an open PR —
/// and re-running it would spend the budget again to redo it, risking a
/// duplicate branch or PR. Both therefore yield their partial output and are
/// never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Completion {
    #[default]
    Success,
    /// The CLI hit `--max-turns`; its last assistant message is in `text`.
    TurnCap,
    /// The call's wall-clock budget ran out; `text` holds what streamed before
    /// the kill.
    Timeout,
    /// `--resume <uuid>` found no transcript (e.g. `~/.claude/projects/...` was
    /// wiped by a container redeploy). The caller clears the persisted UUID and
    /// restarts cold — retrying the same `--resume` would fail identically.
    SessionNotFound,
    Failed,
}

impl Completion {
    /// The subprocess produced work worth handing back to the agent.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Success | Self::TurnCap | Self::Timeout)
    }

    /// Whether re-running the same instruction could plausibly do better.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Failed)
    }

    /// Work landed but the round stopped short of its own conclusion, so the
    /// caller should continue rather than restart.
    pub fn is_truncated(self) -> bool {
        matches!(self, Self::TurnCap | Self::Timeout)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::TurnCap => "turn_cap",
            Self::Timeout => "timeout",
            Self::SessionNotFound => "session_not_found",
            Self::Failed => "failed",
        }
    }
}

/// What we could infer about a non-zero subprocess exit. Conservative —
/// a missing classification ("just exit-N, here's the stderr") is better
/// than a misleading one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashKind {
    /// Exit code 137 (= 128 + SIGKILL). On Linux container deployments
    /// the OOM killer is by far the most common cause; on macOS the same
    /// code shows up for `kill -9`. Either way the user's actionable
    /// signal is "increase the memory budget or shorten the turn".
    KilledLikelyOom,
    /// Exit code 139 (= 128 + SIGSEGV) — a segfault, almost certainly
    /// inside the `claude` CLI itself. Worth surfacing distinctly so the
    /// operator knows to bump the CLI version.
    Segfault,
    /// Ran out of wall clock and we killed it. Distinct from
    /// [`Self::KilledLikelyOom`] because *we* sent the signal: without this the
    /// self-inflicted SIGKILL reads as 137 and every caller reports a plain
    /// timeout as an out-of-memory kill.
    TimedOut { after_secs: u64 },
    /// Subprocess returned a non-zero exit code that we can't classify
    /// further. The stderr_tail is the user's only signal.
    Unknown { exit_code: i32 },
}

impl CrashKind {
    /// Map an exit code to a CrashKind; `None` on a clean exit. Callers decide
    /// *whether* a crash card is warranted — `run` only consults this for
    /// [`Completion::Failed`], since every other outcome has richer handling.
    pub fn from_exit_code(code: i32) -> Option<Self> {
        match code {
            0 => None,
            137 => Some(Self::KilledLikelyOom),
            139 => Some(Self::Segfault),
            other => Some(Self::Unknown { exit_code: other }),
        }
    }

    /// One-line label for the Slack card.
    pub fn label(&self) -> String {
        match self {
            Self::KilledLikelyOom => "killed (signal 9 — likely OOM)".to_string(),
            Self::Segfault => "segfault (signal 11)".to_string(),
            Self::TimedOut { after_secs } => format!("timed out after {after_secs}s"),
            Self::Unknown { exit_code } => format!("exit {exit_code}"),
        }
    }
}

/// Resolve a subprocess exit to the shell's `128 + signal` convention
/// when the process was terminated by a signal — the convention
/// `CrashKind::from_exit_code` expects. `ExitStatus::code()` returns
/// `None` on Unix signal kills (SIGKILL/SIGSEGV), so without this
/// translation an OOM-killed claude would show up as exit `-1` instead
/// of `137`, hiding the most common production failure mode behind a
/// generic "exit -1" label.
fn exit_code_from_status(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    -1
}

/// Cap stderr capture at this many bytes — a runaway logger could
/// otherwise blow up the per-turn record + the Slack card. Sized to fit
/// well within Slack's per-section limit while still showing the last
/// dozen-ish meaningful lines.
const STDERR_TAIL_BYTES: usize = 4_000;

/// Cap on text salvaged from a round that ran out of budget. Generous next to
/// the stderr tail because this is the agent's only account of what the round
/// actually did.
const SALVAGE_TAIL_BYTES: usize = 16 * 1024;

pub(crate) use workforce_agent::tools::exec_runner::tail_utf8;

/// True when Claude Code's stderr signals that `--resume <uuid>` referred to a
/// transcript file that no longer exists. Matching the exact phrase keeps the
/// matcher tight; if the wording ever changes the recovery just stops firing
/// and we fall back to the existing fail-the-turn behaviour.
fn stderr_indicates_session_not_found(stderr: &str) -> bool {
    stderr.contains("No conversation found with session ID")
}

/// Everything worth keeping from the CLI's stdout event stream.
///
/// `assistant_text` exists for the timeout case: the terminal `result` event
/// never arrives when we kill the process, so without accumulating as we go
/// there is nothing left to hand back — the whole round's work vanishes.
#[derive(Default)]
struct StreamCapture {
    shape: ProgressShape,
    /// The model call the last assistant event belonged to: the CLI splits
    /// one call's content blocks into an event each.
    last_call: Option<String>,
    result: Option<serde_json::Value>,
    subtype: Option<String>,
    /// The CLI reports an API failure (a 429, an auth error) as a `success`
    /// result flagged `is_error`, with the error text as the answer.
    is_error: bool,
    assistant_text: String,
    assistant_turns: u32,
    progress_round: u32,
    /// The last `rate_limit_event`'s report, if it reported a limit.
    limit: Option<crate::subscription::LimitSignal>,
    /// Each model call's usage as its assistant events reported it, in
    /// order: the CLI repeats one call's usage on each of its content blocks.
    calls: Vec<(Option<String>, TokenUsage)>,
}

/// How a round's stream reads as progress.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProgressShape {
    /// A coding round: the CLI's own tool calls, its narration as responses,
    /// and its result as the run's end.
    #[default]
    Coding,
    /// A contained round, as an agent turn reports itself: each model call,
    /// then its text as a closed stream segment. Its tool calls are the hub's
    /// to report, and its answer and end the turn's.
    Turn,
}

impl StreamGrammar for StreamCapture {
    fn record(&mut self, event: &serde_json::Value) -> Vec<ProgressEvent> {
        log_stream_event(event);
        self.keep(event);
        match self.shape {
            ProgressShape::Coding => progress_events_of(event, &mut self.progress_round),
            ProgressShape::Turn => {
                turn_progress_of(event, &mut self.progress_round, &mut self.last_call)
            }
        }
    }

    fn finished(&self) -> bool {
        self.result.is_some()
    }

    fn spent(&self) -> TokenUsage {
        self.calls
            .iter()
            .map(|(_, usage)| usage.clone())
            .fold(TokenUsage::default(), std::ops::Add::add)
    }
}

impl StreamCapture {
    fn keep(&mut self, event: &serde_json::Value) {
        match event.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                self.assistant_turns += 1;
                self.note_call(event);
                let texts = event
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .filter(|t| !t.is_empty());
                for text in texts {
                    if !self.assistant_text.is_empty() {
                        self.assistant_text.push('\n');
                    }
                    self.assistant_text.push_str(text);
                }
                keep_tail(&mut self.assistant_text, 2 * SALVAGE_TAIL_BYTES);
            }
            Some("result") => {
                self.subtype = event
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                self.is_error = event
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
                self.result = Some(event.clone());
            }
            Some("rate_limit_event") => {
                self.limit = crate::subscription::limits::claude_rate_limit_event(event);
            }
            _ => {}
        }
    }

    /// The CLI's own turn count when it reported one, else what we observed.
    /// The two are not interchangeable — the CLI counts its internal turns,
    /// which is the number `--max-turns` is compared against.
    fn num_turns(&self) -> u32 {
        self.result
            .as_ref()
            .and_then(|v| v.get("num_turns"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32)
            .unwrap_or(self.assistant_turns)
    }

    /// The CLI's final answer, or — when it never got to produce one — the
    /// narration it streamed along the way.
    fn text(&self) -> String {
        let final_text = self
            .result
            .as_ref()
            .and_then(|v| v.get("result"))
            .and_then(|r| r.as_str())
            .filter(|t| !t.is_empty());
        match final_text {
            Some(t) => t.to_string(),
            None => tail_utf8(&self.assistant_text, SALVAGE_TAIL_BYTES),
        }
    }

    /// Keep the usage an assistant event reports for its model call, once
    /// per call however many content blocks repeat it.
    fn note_call(&mut self, event: &serde_json::Value) {
        let Some(usage) = event.pointer("/message/usage") else {
            return;
        };
        let call = event
            .pointer("/message/id")
            .and_then(|id| id.as_str())
            .map(str::to_string);
        let tokens =
            |field: &str| saturating_tokens(usage.get(field).and_then(|n| n.as_u64()).unwrap_or(0));
        let usage = TokenUsage {
            input_tokens: tokens("input_tokens"),
            output_tokens: tokens("output_tokens"),
            cache_creation_input_tokens: tokens("cache_creation_input_tokens"),
            cache_read_input_tokens: tokens("cache_read_input_tokens"),
        };
        match self.calls.last_mut() {
            Some((last, kept)) if call.is_some() && *last == call => *kept = usage,
            _ => self.calls.push((call, usage)),
        }
    }

    /// The text of a result the CLI flagged `is_error`: its own report of
    /// why the round failed.
    fn error_text(&self) -> Option<&str> {
        self.is_error
            .then(|| self.result.as_ref()?.get("result")?.as_str())
            .flatten()
    }

    /// Nothing was captured — the process produced no parseable output at all.
    fn is_empty(&self) -> bool {
        self.result.is_none() && self.assistant_text.is_empty()
    }

    fn usage(&self, pointer: &str) -> u64 {
        self.result
            .as_ref()
            .and_then(|v| v.pointer(pointer))
            .and_then(|t| t.as_u64())
            .unwrap_or(0)
    }
}

/// Cut `text` to its last `keep` bytes, at a char boundary, once it has grown
/// past twice that. The cut is amortised, and a tail shorter than what is kept
/// reads exactly as it would from the whole text. Not [`tail_utf8`], whose
/// marker and newline snap would stack on every cut.
fn keep_tail(text: &mut String, keep: usize) {
    if text.len() > 2 * keep {
        let start = (text.len() - keep..=text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(text.len());
        text.drain(..start);
    }
}

/// Derive why a round ended, once, from every signal available.
fn completion_of(
    exit_code: i32,
    timed_out: bool,
    session_not_found: bool,
    capture: &StreamCapture,
    max_turns: Option<u32>,
) -> Completion {
    // The CLI's own verdict outranks how the process ended: a round that
    // reported its result and then lingered, or exited non-zero on shutdown,
    // still did the work its result describes.
    match capture.subtype.as_deref() {
        Some("success") if !capture.is_error => return Completion::Success,
        Some("error_max_turns") => return Completion::TurnCap,
        _ => {}
    }
    if timed_out {
        // Nothing salvaged means nothing to hand back; reporting that as a
        // usable round is the "green tick, no answer" failure.
        return if capture.is_empty() {
            Completion::Failed
        } else {
            Completion::Timeout
        };
    }
    if session_not_found {
        return Completion::SessionNotFound;
    }
    // The count stands in for the subtype if the CLI ever renames it: it
    // cannot reach the cap unless the cap is what stopped it. A `success`
    // result flagged as an error counts its failed turn too, so it never does.
    if capture.subtype.as_deref() != Some("success")
        && max_turns.is_some_and(|m| capture.num_turns() >= m)
    {
        return Completion::TurnCap;
    }
    if exit_code == 0 && capture.result.is_none() {
        Completion::Success
    } else {
        Completion::Failed
    }
}

/// Below this, a retry cannot plausibly reach a useful stopping point, so
/// spending what remains of the call's budget on one only delays the answer.
/// Shares its value with the `[coding_session] timeout_secs` floor, which exists
/// precisely so a configured budget always affords at least one viable attempt.
const MIN_ATTEMPT_BUDGET: Duration =
    Duration::from_secs(workforce_common::settings::MIN_DRIVER_TIMEOUT_SECS);

/// The wall clock for one `run_with_retry` call, shared across its attempts.
///
/// `ClaudeCodeConfig::timeout` is the budget for the *call*, not for each
/// attempt: a fresh full window per attempt is how a 20-minute bound became a
/// 31-minute turn in production.
struct CallBudget {
    total: Duration,
    started: Instant,
}

impl CallBudget {
    /// A budget of `total`, `spent` of it already gone.
    fn new(total: Duration, spent: Duration) -> Self {
        Self {
            total: total.saturating_sub(spent),
            started: Instant::now(),
        }
    }

    fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.started.elapsed())
    }

    /// The budget for another attempt after waiting `delay`, or `None` when too
    /// little would be left for it to be worth starting.
    fn next_attempt(&self, delay: Duration) -> Option<Duration> {
        let left = self.remaining().checked_sub(delay)?;
        (left >= MIN_ATTEMPT_BUDGET).then_some(left)
    }
}

/// Read an env var, treating empty as unset.
pub(crate) fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Turn a fully-configured `claude` command into a runner request.
///
/// Reading the program, arguments, working directory and environment back off
/// the `Command` means the dispatched and in-process paths are built by exactly
/// the same code — there is no second place for the OAuth token, the tool
/// arguments or the workspace to be assembled differently.
///
/// `get_envs` yields `None` for a variable the caller removed. Those are
/// dropped rather than forwarded: the runner rebuilds the environment from its
/// allowlist, so a variable to be removed is simply one that is never added.
/// That is what keeps `ANTHROPIC_API_KEY` out of a dispatched Claude Code.
pub fn exec_request_for(
    cmd: &Command,
    stdin_payload: String,
    timeout: std::time::Duration,
) -> ExecRequest {
    let std_cmd = cmd.as_std();
    let lossy = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
    ExecRequest {
        program: lossy(std_cmd.get_program()),
        args: std_cmd.get_args().map(lossy).collect(),
        workdir: std_cmd.get_current_dir().map(|d| d.display().to_string()),
        timeout_secs: timeout.as_secs(),
        allow_retry: false,
        env: std_cmd
            .get_envs()
            .filter_map(|(key, value)| value.map(|v| (lossy(key), lossy(v))))
            .collect(),
        stdin: Some(stdin_payload),
        scope: None,
        mcp: None,
    }
}

/// Git configuration for a round in a remote scope: a credential helper that
/// answers with the round's `GH_TOKEN`, so `git push` and `gh` share one token
/// and nothing is written into the scope.
pub(crate) fn remote_git_credential_env() -> [(&'static str, &'static str); 3] {
    [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "credential.helper"),
        (
            "GIT_CONFIG_VALUE_0",
            "!f() { echo username=x-access-token; echo \"password=$GH_TOKEN\"; }; f",
        ),
    ]
}

/// Point a `claude` subprocess at the user's Max/Pro **subscription** OAuth
/// token, scrubbing the higher-precedence Anthropic API-key env vars it would
/// otherwise inherit from the hub process.
///
/// Claude Code's auth precedence is `ANTHROPIC_AUTH_TOKEN` → `ANTHROPIC_API_KEY`
/// → `apiKeyHelper` → `CLAUDE_CODE_OAUTH_TOKEN` → keychain, and in headless
/// `-p` mode it uses whichever is present **silently, with no prompt**. The hub
/// container injects `ANTHROPIC_API_KEY` (a task-def secret read by the
/// Anthropic LLM / vision / knowledge provider paths). Since we spawn `claude`
/// without clearing the environment, the child inherits that key, which
/// outranks the OAuth token we set — so CC bills the first-party API instead of
/// the subscription and dies with "Credit balance is too low" once the key runs
/// dry. Removing the two higher-precedence vars **on the child only** forces the
/// subscription path while leaving the hub's own use of the key untouched.
pub fn use_subscription_oauth(cmd: &mut Command, oauth_token: &str) {
    use_subscription_oauth_via(cmd, oauth_token, privacy_route().as_deref());
}

/// [`use_subscription_oauth`] with the privacy route given explicitly. When a
/// route is set the child talks to the gateway instead of the API directly:
/// only the base URL and the tenant token change, so the subscription login
/// still applies (setting `ANTHROPIC_AUTH_TOKEN` would replace it).
pub fn use_subscription_oauth_via(
    cmd: &mut Command,
    oauth_token: &str,
    route: Option<&PrivacyRoute>,
) {
    cmd.env("CLAUDE_CODE_OAUTH_TOKEN", oauth_token);
    for var in SUBPROCESS_SECRET_VARS {
        cmd.env_remove(var);
    }
    match route {
        Some(route) => {
            cmd.env("ANTHROPIC_BASE_URL", &route.gateway_url);
            cmd.env(
                "ANTHROPIC_CUSTOM_HEADERS",
                format!("{PRIVACY_TOKEN_HEADER}: {}", route.token),
            );
        }
        None => {
            for var in PRIVACY_ROUTE_VARS {
                cmd.env_remove(var);
            }
        }
    }
}

pub(crate) const SUBPROCESS_SECRET_VARS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// Header the privacy gateway reads the tenant token from.
const PRIVACY_TOKEN_HEADER: &str = "X-Privacy-Token";

/// What a routed child carries and an unrouted one must not inherit.
const PRIVACY_ROUTE_VARS: &[&str] = &["ANTHROPIC_BASE_URL", "ANTHROPIC_CUSTOM_HEADERS"];

/// Where subscription rounds are sent instead of `api.anthropic.com`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyRoute {
    pub gateway_url: String,
    pub token: String,
}

impl PrivacyRoute {
    /// The route `[privacy]` describes, or `None` when routing is off. Enabled
    /// but incomplete is an error: a round must never fall back to the direct
    /// API because a URL was mistyped.
    pub fn from_config(config: &workforce_common::settings::PrivacyConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let gateway_url = config
            .gateway_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| eyre!("[privacy] enabled but gateway_url is not set"))?;
        if !gateway_url.starts_with("http://") && !gateway_url.starts_with("https://") {
            return Err(eyre!(
                "[privacy] gateway_url must start with http:// or https://"
            ));
        }
        let token = config
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| eyre!("[privacy] enabled but token is not set"))?;
        if token.contains(['\r', '\n']) {
            return Err(eyre!("[privacy] token must be a single line"));
        }
        Ok(Some(Self {
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }))
    }
}

static PRIVACY_ROUTE: std::sync::OnceLock<parking_lot::RwLock<Option<Arc<PrivacyRoute>>>> =
    std::sync::OnceLock::new();

fn privacy_route_slot() -> &'static parking_lot::RwLock<Option<Arc<PrivacyRoute>>> {
    PRIVACY_ROUTE.get_or_init(|| parking_lot::RwLock::new(None))
}

/// Install the process-wide route every subscription round follows. Set once
/// at boot from `[privacy]`; `None` sends rounds to the API directly.
pub fn set_privacy_route(route: Option<PrivacyRoute>) {
    *privacy_route_slot().write() = route.map(Arc::new);
}

pub fn privacy_route() -> Option<Arc<PrivacyRoute>> {
    privacy_route_slot().read().clone()
}

/// Point a `claude` subprocess at Bedrock on the round's own credentials,
/// scrubbing every other way it could authenticate: the subscription token
/// and the Anthropic keys (the mirror image of [`use_subscription_oauth`]),
/// and the hub's ambient AWS identity, so the round runs as the invoke role
/// and nothing else.
pub fn use_bedrock(cmd: &mut Command, session: &BedrockSession) {
    cmd.env("CLAUDE_CODE_USE_BEDROCK", "1");
    cmd.env("AWS_REGION", &session.region);
    cmd.env("AWS_ACCESS_KEY_ID", &session.credentials.access_key_id);
    cmd.env(
        "AWS_SECRET_ACCESS_KEY",
        &session.credentials.secret_access_key,
    );
    match &session.credentials.session_token {
        Some(token) => {
            cmd.env("AWS_SESSION_TOKEN", token);
        }
        None => {
            cmd.env_remove("AWS_SESSION_TOKEN");
        }
    }
    cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", &session.pins.opus);
    cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", &session.pins.sonnet);
    cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &session.pins.haiku);
    for var in SUBPROCESS_SECRET_VARS
        .iter()
        .chain(BEDROCK_DISPLACED_VARS)
        .chain(PRIVACY_ROUTE_VARS)
    {
        cmd.env_remove(var);
    }
}

/// What a Bedrock round must not inherit: the subscription token, and every
/// AWS credential source that would let the SDK answer as the hub's task role.
const BEDROCK_DISPLACED_VARS: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_PROFILE",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
];

/// The auth a round runs with, resolved once before the subprocess is built.
pub(crate) enum ResolvedAuth {
    Subscription(String),
    Bedrock(BedrockSession),
}

impl ResolvedAuth {
    pub(crate) fn resolve(auth: &ClaudeAuth) -> Result<Self> {
        match auth {
            ClaudeAuth::Resolved(login) => match &login.credential {
                crate::subscription::Credential::Token(token) => {
                    Ok(Self::Subscription(token.clone()))
                }
                crate::subscription::Credential::ChatGpt(_) => Err(eyre!(
                    "Claude Code runs on a Claude login, not a {} one",
                    login.kind.label()
                )),
            },
            ClaudeAuth::Bedrock(session) => Ok(Self::Bedrock(session.clone())),
        }
    }

    pub(crate) fn apply(&self, cmd: &mut Command) {
        match self {
            Self::Subscription(token) => use_subscription_oauth(cmd, token),
            Self::Bedrock(session) => use_bedrock(cmd, session),
        }
    }
}

/// The GitHub token a round will actually run with: the caller's per-owner or
/// bench token, else the flat vault entry, else the hub's own environment.
///
/// Callers that *describe* the workspace to Claude Code resolve it through here
/// too — telling it "no GitHub token is configured" while the subprocess was
/// handed one from the environment would cost a round to a refusal it need not
/// have made.
pub fn effective_gh_token(
    explicit: Option<String>,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
) -> Option<String> {
    explicit
        .or_else(|| crate::user_config::get_layered(vault, global_vault, "skills/github/token"))
        .or_else(|| non_empty_env("GH_TOKEN"))
        .or_else(|| non_empty_env("GITHUB_TOKEN"))
}

/// Why a Claude Code invocation could not produce a usable result, as a typed,
/// human-actionable reason. Lets the `/cc` and `coding.session` callers surface
/// "no OAuth token — run `claude setup-token`" *immediately* instead of letting
/// a no-token run fall into [`run_with_retry`]'s 60s-sleep-then-retry path and
/// then dumping a raw error string.
///
/// Deliberately has **no failover variants**: Claude plan OAuth is CLI-only
/// (Messages-API failover would violate Anthropic TOS — see
/// `feedback_messages_api_forbidden.md`), and these paths never fall back to a
/// metered API. The reason is for the *human*, relayed verbatim. This mirrors
/// the advisor's `AdvisorFailure` but without the cost-guard/failover machinery
/// the advisor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CcUnavailable {
    /// The `claude` binary isn't installed / on PATH in this hub image.
    CliMissing { reason: String },
    /// Anthropic rate-limited the Max plan (TPM bucket drained).
    RateLimited,
    /// The OAuth token resolved but Anthropic rejected it (invalid/expired).
    AuthFailed,
    /// The subprocess ran but exited non-zero for some other reason.
    Crashed { label: String, stderr_tail: String },
    /// The subprocess could not be run at all (spawn error, timeout, etc.).
    Errored { detail: String },
}

impl CcUnavailable {
    /// Human-readable, actionable message. Callers relay this verbatim — it
    /// never says "falling back to a model" because these paths don't.
    pub fn human_reason(&self) -> String {
        match self {
            Self::CliMissing { reason } => format!(
                "Claude Code CLI isn't available in this hub image ({reason}). Rebuild the image \
                 with `@anthropic-ai/claude-code` on PATH."
            ),
            Self::RateLimited => "Claude Code is rate-limited (the Max plan's per-minute token \
                bucket is drained). Wait a few minutes and try again — it won't fall back to a \
                paid API."
                .to_string(),
            Self::AuthFailed => format!(
                "Claude Code's OAuth token was rejected (invalid or expired): {}.",
                crate::claude_code_setup_token_tool::CONNECT_CLAUDE_OR_PASTE
            ),
            Self::Crashed { label, stderr_tail } => {
                if stderr_tail.is_empty() {
                    format!("Claude Code failed ({label}). It can't fall back to an API model.")
                } else {
                    format!(
                        "Claude Code failed ({label}). It can't fall back to an API model.\n\n{stderr_tail}"
                    )
                }
            }
            Self::Errored { detail } => {
                format!("Claude Code couldn't run: {detail}. It can't fall back to an API model.")
            }
        }
    }
}

/// Classify a finished Claude Code outcome into a typed [`CcUnavailable`], or
/// `None` when the round produced usable work — a clean exit, or budget
/// exhaustion that still carries what it did. `SessionNotFound` is
/// intentionally *not* classified here — callers own that recovery path
/// (clear the stale UUID and retry fresh) before falling back to this.
pub fn classify_failure(result: &Result<ClaudeCodeResult>) -> Option<CcUnavailable> {
    match result {
        Ok(r) if r.completion.is_usable() => None,
        Ok(r) => {
            if is_rate_limited(&r.text) {
                return Some(CcUnavailable::RateLimited);
            }
            if looks_like_auth_failure(&r.text) || looks_like_auth_failure(&r.stderr_tail) {
                return Some(CcUnavailable::AuthFailed);
            }
            Some(CcUnavailable::Crashed {
                label: r
                    .crash_kind
                    .as_ref()
                    .map(|k| k.label())
                    .unwrap_or_else(|| format!("exit {}", r.exit_code)),
                stderr_tail: r.stderr_tail.clone(),
            })
        }
        Err(e) => {
            // `run` returns these exact eyre messages for the absent-token and
            // missing-binary cases; match them so callers without a pre-check
            // still get the actionable reason rather than a raw dump.
            let s = e.to_string();
            if s.contains("Failed to spawn claude CLI") {
                return Some(CcUnavailable::CliMissing { reason: s });
            }
            if is_rate_limited(&s) {
                return Some(CcUnavailable::RateLimited);
            }
            if looks_like_auth_failure(&s) {
                return Some(CcUnavailable::AuthFailed);
            }
            Some(CcUnavailable::Errored { detail: s })
        }
    }
}

/// Result of probing whether the `claude` CLI is installed and runnable
/// in the current environment. Captured once at hub boot and refreshed on
/// a slow timer so a /cc dispatch into a hub image without the CLI fails
/// fast with a clear "rebuild required" message instead of the opaque
/// `os error 2` users hit on a production install.
#[derive(Debug, Clone)]
pub enum CliHealth {
    /// `claude --version` succeeded; the trimmed stdout (e.g. "1.0.32 (Claude Code)") is preserved for logging.
    Available { version: String },
    /// Either spawn failed (binary not on PATH) or the binary returned non-zero.
    Missing { reason: String },
}

impl CliHealth {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Verify Claude Code can persist session transcripts: `$HOME/.claude`
/// must be creatable/writable by *this* process's uid. When it isn't
/// (e.g. a root-owned bind mount under a uid-1000 hub), every turn's
/// transcript write silently fails and the next `--resume` reports
/// "No conversation found" — /cc loses all context on every single
/// message while looking otherwise healthy. That exact failure ran
/// undiagnosed on driftwerk-prod for three weeks (May 15 – Jun 5,
/// 2026), so it is a hard health failure, not a warning.
fn probe_session_store_writable() -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let dir = std::path::Path::new(&home).join(".claude");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(".wf-write-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Run `claude --version` once and classify the outcome. Bounded by a
/// short timeout (the binary either prints and exits in <100ms or it's
/// hung on something unrelated to availability — the latter is *also* a
/// failure for our purposes, just a slower one). Also probes that the
/// session transcript store (`$HOME/.claude`) is writable — an
/// unwritable store means per-turn amnesia, which is as broken as a
/// missing binary.
pub async fn probe_cli() -> CliHealth {
    let probe = async {
        let mut cmd = Command::new("claude");
        cmd.arg("--version");
        cmd.kill_on_drop(true);
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                if let Err(reason) = probe_session_store_writable() {
                    return CliHealth::Missing {
                        reason: format!(
                            "session store unwritable — every /cc turn would lose context: {reason}. \
                             Fix the $HOME/.claude mount ownership (chown to the hub uid)."
                        ),
                    };
                }
                CliHealth::Available {
                    version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                }
            }
            Ok(out) => CliHealth::Missing {
                reason: format!(
                    "exit {}: {}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            },
            Err(e) => CliHealth::Missing {
                reason: format!("spawn failed ({}): {e}", e.kind()),
            },
        }
    };
    match tokio::time::timeout(Duration::from_secs(5), probe).await {
        Ok(h) => h,
        Err(_) => CliHealth::Missing {
            reason: "claude --version timed out after 5s".to_string(),
        },
    }
}

/// Spawn `claude -p` with `--output-format stream-json` and capture output.
///
/// Streams stdout line-by-line, logging progress events as they arrive, and
/// takes cost/usage from the final `result` event. Running out of wall clock is
/// not an error: the round is killed but still returns what it streamed, with
/// [`Completion::Timeout`] saying so.
/// The MCP config a round is pointed at, and the listener behind it when the
/// round runs on this host: dropping the listener ends it, removes its config
/// and ends every call it still holds.
async fn round_mcp(
    config: &ClaudeCodeConfig,
) -> Result<(Option<crate::mcp_turn::LocalMcp>, Option<PathBuf>)> {
    let local = match (&config.mcp, &config.remote) {
        (Some(tools), None) => Some(tools.serve_local().await?),
        _ => None,
    };
    let path = match (&config.mcp, &local) {
        (_, Some(local)) => Some(local.path().to_path_buf()),
        (Some(_), None) => Some(PathBuf::from(
            workforce_agent::tools::exec_runner::SCOPE_MCP_CONFIG_PATH,
        )),
        (None, None) => None,
    };
    Ok((local, path))
}

pub async fn run(
    config: &ClaudeCodeConfig,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
) -> Result<ClaudeCodeResult> {
    let auth = ResolvedAuth::resolve(&config.auth)?;

    let start = Instant::now();

    // Held for the turn, so no route out of this function can leave its
    // listener serving.
    let (local, mcp_config) = round_mcp(config).await?;
    let cmd = command(config, &auth, mcp_config.as_deref(), vault, global_vault);
    let stdin = config
        .contained
        .as_ref()
        .map_or_else(|| config.task.clone(), Contained::user_line);

    let run = run_cli(
        CliInvocation {
            command: cmd,
            stdin,
            timeout: config.timeout,
            remote: config.remote.clone(),
            mcp: config.mcp.as_ref().map(|tools| tools.endpoint().clone()),
            codex_mcp_config: None,
            job: config.jobs.as_ref().map(RoundJobs::job),
        },
        StreamCapture {
            shape: match config.contained {
                Some(_) => ProgressShape::Turn,
                None => ProgressShape::Coding,
            },
            ..StreamCapture::default()
        },
        cancel,
        progress_tx,
    )
    .await?;
    drop(local);

    Ok(assemble_result(run, config, start.elapsed()))
}

/// The `claude` program a round runs.
#[cfg(not(test))]
pub(crate) fn claude_program() -> PathBuf {
    PathBuf::from("claude")
}

/// The scripted `claude` a test scopes in; outside such a scope, a path
/// that never resolves, so no test reaches the real CLI.
#[cfg(test)]
pub(crate) fn claude_program() -> PathBuf {
    tests::CLAUDE
        .try_with(PathBuf::clone)
        .unwrap_or_else(|_| PathBuf::from("/nonexistent/claude"))
}

/// The `claude` invocation for `config`, authenticated by `auth` and pointed
/// at `mcp_config`. Only an uncontained round is handed a GitHub token; a
/// contained one inherits none from the hub either.
pub(crate) fn command(
    config: &ClaudeCodeConfig,
    auth: &ResolvedAuth,
    mcp_config: Option<&Path>,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
) -> Command {
    let mut cmd = Command::new(claude_program());
    cmd.arg("-p")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json");

    if !config.persist_session {
        cmd.arg("--no-session-persistence");
    }

    if let Some(contained) = &config.contained {
        // An empty source list reads no settings file — the user's, the
        // project's or the local one — so none of their hooks runs.
        cmd.arg("--tools")
            .arg(contained.tools.join(","))
            .arg("--strict-mcp-config")
            .arg("--setting-sources")
            .arg("")
            .arg("--system-prompt-file")
            .arg(&contained.system_prompt_file)
            .arg("--input-format")
            .arg("stream-json");
    }
    // Relocates per-machine sections (cwd, env, git status) into the first
    // user message — no feature loss, and improves cross-call prompt-cache
    // reuse.
    if config
        .contained
        .as_ref()
        .is_none_or(|contained| contained.exclude_dynamic)
    {
        cmd.arg("--exclude-dynamic-system-prompt-sections");
    }

    if let Some(session_id) = config.session_id {
        if config.resume {
            cmd.arg("--resume").arg(session_id.to_string());
        } else {
            cmd.arg("--session-id").arg(session_id.to_string());
        }
    }

    if let Some(max_turns) = config.max_turns {
        cmd.arg("--max-turns").arg(max_turns.to_string());
    }

    if !config.allowed_tools.is_empty() {
        cmd.arg("--allowedTools")
            .arg(config.allowed_tools.join(","));
    }

    if let Some(ref prompt) = config.append_system_prompt {
        cmd.arg("--append-system-prompt").arg(prompt);
    }

    if let Some(ref model) = config.model {
        cmd.arg("--model").arg(model);
    }

    if let Some(ref effort) = config.effort {
        cmd.arg("--effort").arg(effort);
    }

    if let Some(mcp_path) = mcp_config {
        cmd.arg("--mcp-config").arg(mcp_path);
        // Claude Code 2.1.x defers ALL MCP tool schemas behind ToolSearch
        // (verified on 2.1.162, 2026-06-05): with ToolSearch disallowed the
        // model can never load a single mcp__workforce__* schema and answers
        // "I don't have that skill". A/B-tested on driftwerk-prod with the
        // hub's own mcp_config: blocked → TOOLS-INVISIBLE, unblocked → tool
        // call succeeds. ToolSearch therefore stays ON by default; set
        // CLAUDE_CODE_BLOCK_TOOL_SEARCH=1 only on installs pinning a
        // pre-deferral CLI where eager schemas made search a turn-waster.
        let block_tool_search = tool_search_blocked(config.contained.is_some());
        if block_tool_search {
            cmd.arg("--disallowedTools").arg("ToolSearch");
        }
        info!(
            target: "cc",
            block_tool_search,
            "claude code mcp config attached"
        );
    }

    cmd.current_dir(&config.workdir);
    auth.apply(&mut cmd);

    let gh_token = match config.contained {
        Some(_) => {
            cmd.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
            None
        }
        None => effective_gh_token(config.gh_token.clone(), vault, global_vault),
    };
    if let Some(gh_token) = gh_token {
        cmd.env("GH_TOKEN", &gh_token);
        cmd.env("GITHUB_TOKEN", &gh_token);
        // The hub's worktrees carry a credential helper in their config; a
        // remote scope's checkouts do not, so git there reads the round's token.
        if config.remote.is_some() {
            for (var, value) in remote_git_credential_env() {
                cmd.env(var, value);
            }
        }
    }

    if let Some(timeout) = config.contained.as_ref().and_then(|c| c.tool_timeout) {
        cmd.env("MCP_TOOL_TIMEOUT", timeout.as_millis().to_string());
    }
    if !config.load_auto_memory {
        cmd.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
    }
    if !config.load_claude_md {
        cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1");
    }
    cmd.env("CLAUDE_ENABLE_STREAM_WATCHDOG", "1");

    // A remote scope brings its own toolchain and caches; only a round on this
    // host inherits the hub's.
    if config.remote.is_none() {
        let cache_env = config
            .cache_dir
            .iter()
            .flat_map(|dir| toolchain_cache_env(dir));
        for (var, val) in inherited_toolchain_env().into_iter().chain(cache_env) {
            cmd.env(var, val);
        }
    }
    cmd
}

/// Whether this install keeps ToolSearch from a round's model, as
/// `CLAUDE_CODE_BLOCK_TOOL_SEARCH` asks.
pub(crate) fn tool_search_blocked(contained: bool) -> bool {
    blocks_tool_search(
        std::env::var("CLAUDE_CODE_BLOCK_TOOL_SEARCH")
            .ok()
            .as_deref(),
        contained,
    )
}

/// Whether a round keeps ToolSearch from the model, as
/// `CLAUDE_CODE_BLOCK_TOOL_SEARCH` asks: never a contained round, for which
/// it is the only way to load a tool.
fn blocks_tool_search(setting: Option<&str>, contained: bool) -> bool {
    !contained && setting.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Turn a finished subprocess into a [`ClaudeCodeResult`]: decide why it ended,
/// and what to tell callers that only read `exit_code` / `crash_kind`.
fn assemble_result(
    run: CapturedRun<StreamCapture>,
    config: &ClaudeCodeConfig,
    duration: Duration,
) -> ClaudeCodeResult {
    let raw_exit = run.exit_code.unwrap_or(-1);
    let stderr_str = String::from_utf8_lossy(&run.stderr);
    let session_not_found = stderr_indicates_session_not_found(&stderr_str);
    let completion = completion_of(
        raw_exit,
        run.ended.timed_out(),
        session_not_found,
        &run.capture,
        config.max_turns,
    );
    let exit_code = exit_code_for(completion, run.ended, raw_exit);
    info!(
        target: "cc",
        exit_code,
        completion = completion.as_str(),
        ended = run.ended.as_str(),
        duration_ms = duration.as_millis() as u64,
        stderr_head = %stderr_str.chars().take(500).collect::<String>(),
        "claude subprocess complete"
    );
    let crash_kind = crash_kind_for(completion, run.ended, exit_code, config.timeout);
    let stderr_tail = stderr_tail_for(completion, &stderr_str);
    let text = run.capture.text();
    let error_text = run.capture.error_text().unwrap_or_default();
    let limit = limit_of(
        run.capture.limit.clone(),
        completion,
        &[error_text, &stderr_str].join("\n"),
    );
    let notice = error_text.to_string();

    ClaudeCodeResult {
        text,
        engine: String::new(),
        model: config.model.clone(),
        exit_code,
        duration,
        input_tokens: run.capture.usage("/usage/input_tokens"),
        output_tokens: run.capture.usage("/usage/output_tokens"),
        cache_creation_tokens: run.capture.usage("/usage/cache_creation_input_tokens"),
        cache_read_tokens: run.capture.usage("/usage/cache_read_input_tokens"),
        cost_usd: run
            .capture
            .result
            .as_ref()
            .and_then(|v| v.get("total_cost_usd"))
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0),
        num_turns: run.capture.num_turns().max(1),
        completion,
        crash_kind,
        stderr_tail,
        notice,
        handed_over: Vec::new(),
        limit,
        job: config.jobs.as_ref().map(RoundJobs::job),
    }
}

/// A round's limit report: a refusal its stream reported, else the notice a
/// failed round's CLI printed (`notice`: its error result and stderr, never
/// the round's own narration), else the stream's warning.
fn limit_of(
    streamed: Option<crate::subscription::LimitSignal>,
    completion: Completion,
    notice: &str,
) -> Option<crate::subscription::LimitSignal> {
    use crate::subscription::LimitStatus;
    match streamed {
        Some(refused) if refused.status == LimitStatus::Rejected => Some(refused),
        warning => (completion != Completion::Success)
            .then(|| crate::subscription::limits::claude_limit_text(notice, chrono::Utc::now()))
            .flatten()
            .or(warning),
    }
}

/// The exit code a round reports. We sent the SIGKILL that ends a round we
/// cut short, so its 128+9 is our own doing — publishing it would report every
/// such round as an OOM kill (137 is exactly what `CrashKind::KilledLikelyOom`
/// identifies). Derived from `completion` so a round the CLI reported as
/// finished still reads as clean to the callers that gate on this.
pub(crate) fn exit_code_for(completion: Completion, ended: Ended, raw_exit: i32) -> i32 {
    match completion {
        Completion::Success => 0,
        _ if ended.hub_killed() => -1,
        _ => raw_exit,
    }
}

/// Callers that predate `Completion` read the label to describe a failure, so
/// a timeout has to say so here or it renders as a bare "exit -1". Otherwise
/// only a genuine crash gets a card: success, the session-lost recovery, a
/// turn cap and a round cut after its own result all have richer handling.
pub(crate) fn crash_kind_for(
    completion: Completion,
    ended: Ended,
    exit_code: i32,
    timeout: Duration,
) -> Option<CrashKind> {
    match (completion, ended) {
        (Completion::Success | Completion::TurnCap | Completion::SessionNotFound, _) => None,
        (_, Ended::WallClock) => Some(CrashKind::TimedOut {
            after_secs: timeout.as_secs(),
        }),
        (_, Ended::AfterResult) => None,
        (_, Ended::Exited) => CrashKind::from_exit_code(exit_code),
    }
}

/// Keep stderr for a timeout — when a round hangs it is the only evidence of
/// why — but not for the clean exits, where it is noise.
pub(crate) fn stderr_tail_for(completion: Completion, stderr: &str) -> String {
    match completion {
        Completion::Success | Completion::TurnCap => String::new(),
        _ => tail_utf8(stderr, STDERR_TAIL_BYTES),
    }
}

/// How a subprocess run came to an end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The leader exited on its own.
    Exited,
    /// The wall clock ran out and the hub killed the group.
    WallClock,
    /// The leader lingered past [`RESULT_EXIT_GRACE`] after its result and the
    /// hub killed the group; the round itself was over.
    AfterResult,
}

impl Ended {
    pub(crate) fn timed_out(self) -> bool {
        self == Self::WallClock
    }

    /// The hub sent the signal, so the exit status says nothing about the
    /// round.
    pub(crate) fn hub_killed(self) -> bool {
        self != Self::Exited
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::WallClock => "wall_clock",
            Self::AfterResult => "after_result",
        }
    }
}

/// A finished subprocess run: how it ended, and everything it emitted.
pub(crate) struct CapturedRun<G> {
    /// `None` only when the process could not be reaped after a kill.
    pub(crate) exit_code: Option<i32>,
    pub(crate) ended: Ended,
    pub(crate) capture: G,
    pub(crate) stderr: Vec<u8>,
}

const STDERR_HEAD_BYTES: usize = 64 * 1024;
const STDERR_KEEP_BYTES: usize = 64 * 1024;

/// What a CLI wrote to stderr, bounded: the opening, where a refused start
/// explains itself, and the end, where a crash does.
#[derive(Debug, Default)]
pub(crate) struct StderrCapture {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

impl StderrCapture {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        let room = STDERR_HEAD_BYTES
            .saturating_sub(self.head.len())
            .min(chunk.len());
        self.head.extend_from_slice(&chunk[..room]);
        self.tail.extend_from_slice(&chunk[room..]);
        if self.tail.len() > 2 * STDERR_KEEP_BYTES {
            let dropped = self.tail.len() - STDERR_KEEP_BYTES;
            self.tail.drain(..dropped);
            self.omitted += dropped;
        }
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let gap = (self.omitted > 0)
            .then(|| format!("\n[{} bytes of stderr omitted]\n", self.omitted).into_bytes());
        [self.head, gap.unwrap_or_default(), self.tail].concat()
    }
}

/// Hand every chunk `reader` yields to `sink` until it ends or fails.
pub(crate) async fn read_chunks(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    mut sink: impl FnMut(&[u8]),
) {
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    while let Ok(n @ 1..) = reader.read(&mut chunk).await {
        sink(&chunk[..n]);
    }
}

/// The next line `lines` yields, decoded lossily: a stray invalid byte costs a
/// character, not the rest of the stream.
pub(crate) async fn next_line_lossy(
    lines: &mut tokio::io::Split<impl tokio::io::AsyncBufRead + Unpin>,
) -> Option<String> {
    lines
        .next_segment()
        .await
        .ok()
        .flatten()
        .map(|line| String::from_utf8_lossy(&line).into_owned())
}

/// Run `invocation` wherever it belongs — a remote scope, the hub-local
/// runner, or this process — reading its stream with `grammar`. One transport
/// for every CLI engine: the routes, the wall clock, the grace after the
/// result and the MCP tunnel are decided here and nowhere else.
pub(crate) async fn run_cli<G: StreamGrammar>(
    invocation: CliInvocation,
    grammar: G,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
) -> Result<CapturedRun<G>> {
    let CliInvocation {
        command,
        stdin,
        timeout,
        remote,
        mcp,
        codex_mcp_config,
        job,
    } = invocation;
    match (&remote, sandbox_runner()) {
        (Some(placement), _) => {
            dispatch_and_capture(
                command,
                stdin,
                timeout,
                cancel,
                progress_tx,
                &DispatchTarget::remote(placement, mcp.as_ref(), codex_mcp_config, job),
                grammar,
            )
            .await
        }
        (None, Some(endpoint)) => {
            dispatch_and_capture(
                command,
                stdin,
                timeout,
                cancel,
                progress_tx,
                &DispatchTarget::local(endpoint),
                grammar,
            )
            .await
        }
        (None, None) => {
            spawn_and_capture(command, stdin, timeout, cancel, progress_tx, grammar).await
        }
    }
}

/// The name a spawned CLI is reported under in errors.
fn program_name(cmd: &Command) -> String {
    cmd.as_std().get_program().to_string_lossy().into_owned()
}

/// Run `cmd` to completion under `timeout`, streaming its stdout as JSON events.
///
/// Separated from [`run`] so that building the `claude` invocation and running a
/// streaming subprocess under a deadline are independently testable — the
/// timeout path in particular, which used to discard the round's entire output.
/// On timeout the child is killed and then *reaped*, so whatever it streamed
/// before the deadline is still returned; only a spawn failure and cancellation
/// yield `Err`.
async fn spawn_and_capture<G: StreamGrammar>(
    mut cmd: Command,
    stdin_payload: String,
    timeout: Duration,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    grammar: G,
) -> Result<CapturedRun<G>> {
    let program = program_name(&cmd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Phrased for `classify_failure`, which matches this text to tell "no CLI in
    // the image" apart from every other error out of this function.
    let mut child = GroupChild::spawn(&mut cmd)
        .map_err(|e| eyre!("Failed to spawn {program} CLI (is it installed in the image?): {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = stdin.write_all(stdin_payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // Both readers publish into shared state as they go, so whatever arrived
    // is still readable when a drain has to be abandoned (see `drain_or_abort`).
    let capture = Arc::new(Mutex::new(grammar));
    let stderr = Arc::new(Mutex::new(StderrCapture::default()));

    let stderr_task = tokio::spawn({
        let stderr = Arc::clone(&stderr);
        async move {
            if let Some(reader) = stderr_handle {
                read_chunks(reader, |chunk| lock(&stderr).push(chunk)).await;
            }
        }
    });

    let (line_tx, line_rx) = tokio::sync::mpsc::channel::<String>(256);
    let feed_task = tokio::spawn(async move {
        let Some(stdout) = stdout_handle else { return };
        let mut lines = BufReader::new(stdout).split(b'\n');
        while let Some(line) = next_line_lossy(&mut lines).await {
            if line_tx.send(line).await.is_err() {
                return;
            }
        }
    });
    let result_seen = CancellationToken::new();
    let stream_task = tokio::spawn(consume_stream_lines(
        program.clone(),
        line_rx,
        Arc::clone(&capture),
        progress_tx,
        result_seen.clone(),
    ));

    let wait = await_exit(&mut child, timeout, &result_seen);
    let finished = match cancel {
        Some(token) => tokio::select! {
            outcome = wait => Some(outcome),
            _ = token.cancelled() => None,
        },
        None => Some(wait.await),
    };

    // Cancellation discards the round, so there is nothing to drain for — but
    // the readers must still be stopped, or they outlive the call holding a
    // pipe. `child` is killed on drop.
    let Some((status, ended)) = finished else {
        warn!(target: "cc", program, "subprocess cancelled via token");
        stream_task.abort();
        stderr_task.abort();
        return Err(Interrupted::of(program, &*lock(&capture)).into());
    };

    settle_pipes(&mut child, stream_task, stderr_task).await;
    feed_task.abort();

    let capture = std::mem::take(&mut *lock(&capture));
    let stderr = std::mem::take(&mut *lock(&stderr)).into_bytes();
    Ok(CapturedRun {
        exit_code: status.as_ref().map(exit_code_from_status),
        ended,
        capture,
        stderr,
    })
}

/// How long a `claude` that has already emitted its result may linger before
/// the round is ended for it. The CLI normally exits within a second of its
/// result; one that does not is being held open by a background child, and a
/// round is over when Claude Code has replied.
const RESULT_EXIT_GRACE: Duration = Duration::from_secs(10);
/// How long a round asked to end is given to report that it has.
const END_GRACE: Duration = Duration::from_secs(20);
/// Beyond a durable round's own budget, how long to keep reaching for it: the
/// worker enforces the budget and then reports how the round ended.
const DURABLE_ROUND_GRACE: Duration = Duration::from_secs(60);

/// Wait for the leader to exit within `timeout`, cutting the wait short once the
/// result event has arrived: after [`RESULT_EXIT_GRACE`] the group is killed.
async fn await_exit(
    child: &mut GroupChild,
    timeout: Duration,
    result_seen: &CancellationToken,
) -> (Option<std::process::ExitStatus>, Ended) {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    tokio::select! {
        status = child.wait() => return (status.ok(), Ended::Exited),
        _ = &mut deadline => return kill_and_reap(child, Ended::WallClock).await,
        _ = result_seen.cancelled() => {}
    }
    tokio::select! {
        status = child.wait() => (status.ok(), Ended::Exited),
        _ = &mut deadline => kill_and_reap(child, Ended::WallClock).await,
        _ = tokio::time::sleep(RESULT_EXIT_GRACE) => kill_and_reap(child, Ended::AfterResult).await,
    }
}

async fn kill_and_reap(
    child: &mut GroupChild,
    ended: Ended,
) -> (Option<std::process::ExitStatus>, Ended) {
    match ended {
        Ended::AfterResult => warn!(
            target: "cc",
            grace_secs = RESULT_EXIT_GRACE.as_secs(),
            "claude replied but did not exit — killing its process group"
        ),
        _ => warn!(
            target: "cc",
            "claude subprocess timed out — killing its process group and salvaging output"
        ),
    }
    child.kill_group();
    // Even SIGKILL leaves a process in uninterruptible sleep unreaped — a
    // wedged NFS mount has hung this host before — so give up on the status
    // rather than the whole round.
    let reaped = tokio::time::timeout(STREAM_DRAIN_DEADLINE, child.wait()).await;
    (reaped.ok().and_then(Result::ok), ended)
}

/// Finish reading once the leader is gone; readers that cannot be drained even
/// after the group is killed are abandoned.
async fn settle_pipes(
    child: &mut GroupChild,
    mut stream_task: tokio::task::JoinHandle<()>,
    mut stderr_task: tokio::task::JoinHandle<()>,
) {
    if child
        .drain_readers(STREAM_DRAIN_DEADLINE, &mut stream_task, &mut stderr_task)
        .await
        .is_none()
    {
        warn!(target: "cc", "claude pipes still open after its process group was killed — abandoning the drain");
        stream_task.abort();
        stderr_task.abort();
    }
}

/// Where to dispatch Claude Code, or `None` to spawn it in this process.
///
/// Process-wide rather than per-config: there are six places that build a
/// Claude Code invocation, and a new one must not be able to opt out of the
/// sandbox by forgetting a field. Unset — every surface that never calls
/// [`set_sandbox_runner`] — keeps the in-process path.
static SANDBOX_RUNNER: std::sync::OnceLock<Option<RunnerEndpoint>> = std::sync::OnceLock::new();

/// Record where Claude Code runs. Called once from the serve path with the
/// runner only when `[exec] sandbox_claude_code` is on.
pub fn set_sandbox_runner(runner: Option<RunnerEndpoint>) {
    let _ = SANDBOX_RUNNER.set(runner);
}

pub(crate) fn sandbox_runner() -> Option<&'static RunnerEndpoint> {
    SANDBOX_RUNNER.get().and_then(|r| r.as_ref())
}

/// What a durable job's tool call is told while the hub drains.
const DRAINING_REFUSAL: &str = "The hub is restarting; call this tool again.";

/// Where a dispatched round runs and how its MCP calls are answered: the
/// hub-local runner sharing this filesystem, or a prepared scope on the remote
/// worker whose tool calls tunnel back over the same connection.
struct DispatchTarget<'a> {
    endpoint: &'a RunnerEndpoint,
    scope: Option<ScopeId>,
    mcp: Option<&'a McpEndpoint>,
    codex_mcp_config: Option<CodexMcpConfig>,
    jobs: bool,
    job: Option<JobId>,
    draining: fn() -> bool,
}

impl<'a> DispatchTarget<'a> {
    fn local(endpoint: &'a RunnerEndpoint) -> Self {
        Self {
            endpoint,
            scope: None,
            mcp: None,
            codex_mcp_config: None,
            jobs: false,
            job: None,
            draining: crate::shutdown::in_progress,
        }
    }

    fn remote(
        placement: &'a RemotePlacement,
        mcp: Option<&'a McpEndpoint>,
        codex_mcp_config: Option<CodexMcpConfig>,
        job: Option<JobId>,
    ) -> Self {
        Self {
            endpoint: &placement.endpoint,
            scope: Some(placement.scope.clone()),
            mcp,
            codex_mcp_config,
            jobs: placement.jobs,
            job,
            draining: crate::shutdown::in_progress,
        }
    }

    fn request(&self, cmd: &Command, stdin_payload: String, timeout: Duration) -> ExecRequest {
        ExecRequest {
            scope: self.scope.clone(),
            mcp: self.mcp.map(|endpoint| McpBinding {
                server_name: MCP_SERVER_KEY.to_string(),
                instructions: endpoint.instructions().map(str::to_string),
                codex_config: self.codex_mcp_config.clone(),
            }),
            ..exec_request_for(cmd, stdin_payload, timeout)
        }
    }

    /// Run `request` to its summary: as a durable job the worker keeps
    /// through a lost link, when the scope can run one, and as a plain
    /// command otherwise. A round that ends without the job's summary, by a
    /// cancel, an error or being dropped, ends the job too, unless the hub is
    /// draining and leaves it for its successor.
    async fn run(
        &self,
        request: ExecRequest,
        sink: tokio::sync::mpsc::Sender<workforce_agent::tools::exec_runner::ServerFrame>,
        cancel: CancellationToken,
    ) -> Result<ExecSummary> {
        use workforce_agent::tools::exec_runner::{
            dispatch_exec, dispatch_job, forget_job, DurableJob,
        };
        let Some(scope) = self.scope.clone().filter(|_| self.jobs) else {
            return dispatch_exec(self.endpoint, request, sink, cancel, self.responder()).await;
        };
        let job = match &self.job {
            Some(job) => {
                if let Some(call) = workforce_agent::turn_journal::ToolCall::current() {
                    call.checkpoint(job.as_str()).await.map_err(|e| {
                        e.wrap_err("recording the round's next job before starting it")
                    })?;
                }
                job.clone()
            }
            None => JobId::generate(),
        };
        let timeout = Duration::from_secs(request.timeout_secs) + DURABLE_ROUND_GRACE;
        let held = HeldJob {
            endpoint: self.endpoint.clone(),
            job: Some((job.clone(), scope.clone())),
            draining: self.draining,
        };
        let durable = DurableJob {
            job: job.clone(),
            scope,
            exec: Some(request),
        };
        let summary = tokio::select! {
            summary = dispatch_job(
                self.endpoint,
                durable,
                sink,
                CancellationToken::new(),
                self.responder(),
                timeout,
            ) => summary?,
            _ = cancel.cancelled() => return Err(eyre!("secure command runner: the job was cancelled")),
        };
        held.finished();
        if self.job.is_none() {
            forget_job(self.endpoint, &job).await;
        }
        Ok(summary)
    }

    /// Answers the worker's tunnelled MCP requests from the endpoint; a
    /// notification is answered with an empty body, which the worker's bridge
    /// turns into the 202 the protocol expects.
    ///
    /// A durable job's tool call is refused unrun while the hub drains: a
    /// reply lost with the link after the tool acted would leave the job's
    /// model to call it again on the successor. The rest of the protocol is
    /// still answered, and a round that is not a durable job keeps running
    /// its calls through the drain, which is how it finishes.
    fn responder(&self) -> Option<McpResponder> {
        let durable = self.jobs && self.scope.is_some();
        let draining = self.draining;
        self.mcp.cloned().map(|endpoint| -> McpResponder {
            Arc::new(move |body: String| {
                let endpoint = endpoint.clone();
                Box::pin(async move {
                    match durable && draining() && McpEndpoint::is_tool_call(&body) {
                        true => McpEndpoint::refusal(&body, DRAINING_REFUSAL),
                        false => endpoint.handle(&body).await.unwrap_or_default(),
                    }
                })
            })
        })
    }
}

/// A durable job its round still answers for. Dropped before the job's
/// summary arrived, it ends the job on the worker, since nothing else would
/// before its deadline; a draining hub leaves it running for a successor to
/// collect.
struct HeldJob {
    endpoint: RunnerEndpoint,
    job: Option<(JobId, ScopeId)>,
    draining: fn() -> bool,
}

impl HeldJob {
    fn finished(mut self) {
        self.job = None;
    }
}

impl Drop for HeldJob {
    fn drop(&mut self) {
        let Some((job, scope)) = self.job.take().filter(|_| !(self.draining)()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let endpoint = self.endpoint.clone();
        runtime.spawn(async move {
            workforce_agent::tools::exec_runner::cancel_job(&endpoint, &job, &scope).await
        });
    }
}

/// Run `cmd` on a runner daemon instead of in this process.
///
/// Produces the same [`CapturedRun`] as the local path and shares the stream
/// parser with it; only the framing differs. Fail-closed: a runner error is
/// returned rather than retried in-process, which would put Claude Code back in
/// the hub user with the vault in reach.
async fn dispatch_and_capture<G: StreamGrammar>(
    cmd: Command,
    stdin_payload: String,
    timeout: Duration,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    target: &DispatchTarget<'_>,
    grammar: G,
) -> Result<CapturedRun<G>> {
    use workforce_agent::tools::exec_runner::{LineReassembler, ServerFrame};

    let program = program_name(&cmd);
    let req = target.request(&cmd, stdin_payload, timeout);
    let capture = Arc::new(Mutex::new(grammar));
    let stderr = Arc::new(Mutex::new(StderrCapture::default()));

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ServerFrame>(256);
    let (line_tx, line_rx) = tokio::sync::mpsc::channel::<String>(256);

    let result_seen = CancellationToken::new();
    let stream_task = tokio::spawn(consume_stream_lines(
        program.clone(),
        line_rx,
        Arc::clone(&capture),
        progress_tx,
        result_seen.clone(),
    ));

    // Chunk boundaries fall wherever the pipe split, so stdout is reassembled
    // into the lines the parser expects; the tail is flushed at end of stream
    // because Claude Code's last line is the result of the turn.
    let relay_task = tokio::spawn({
        let stderr = Arc::clone(&stderr);
        async move {
            let mut reassembler = LineReassembler::new();
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerFrame::Stdout { chunk } => {
                        for line in reassembler.push(chunk.as_bytes()) {
                            if line_tx.send(line).await.is_err() {
                                return;
                            }
                        }
                    }
                    ServerFrame::Stderr { chunk } => lock(&stderr).push(chunk.as_bytes()),
                    _ => {}
                }
            }
            if let Some(tail) = reassembler.finish() {
                let _ = line_tx.send(tail).await;
            }
        }
    });

    let cancel = cancel.unwrap_or_default();
    let end = cancel.child_token();
    let settled = settle_dispatch(target.run(req, event_tx, end.clone()), &result_seen, &end).await;

    if settled.is_err() && cancel.is_cancelled() {
        warn!(target: "cc", program, "subprocess cancelled via token");
        relay_task.abort();
        stream_task.abort();
        return Err(Interrupted::of(program, &*lock(&capture)).into());
    }
    let (summary, ended) = settled?;

    drain_or_abort(relay_task, "relay").await;
    drain_or_abort(stream_task, "stdout").await;

    let capture = std::mem::take(&mut *lock(&capture));
    let stderr = std::mem::take(&mut *lock(&stderr)).into_bytes();
    Ok(CapturedRun {
        exit_code: summary.exit_code,
        ended,
        capture,
        stderr,
    })
}

/// Drive a dispatch to its summary, or to the result event plus
/// [`RESULT_EXIT_GRACE`], whichever comes first. Past the grace `end` is
/// fired, which has the daemon end the round's process group, and the round
/// carries a synthetic summary; its completion comes from the result it
/// already reported.
async fn settle_dispatch(
    run: impl std::future::Future<Output = Result<ExecSummary>>,
    result_seen: &CancellationToken,
    end: &CancellationToken,
) -> Result<(ExecSummary, Ended)> {
    let ended_by_daemon = |summary: ExecSummary| {
        let ended = if summary.timed_out {
            Ended::WallClock
        } else {
            Ended::Exited
        };
        (summary, ended)
    };
    tokio::pin!(run);
    tokio::select! {
        summary = &mut run => return summary.map(ended_by_daemon),
        _ = result_seen.cancelled() => {}
    }
    match tokio::time::timeout(RESULT_EXIT_GRACE, &mut run).await {
        Ok(summary) => summary.map(ended_by_daemon),
        Err(_) => {
            warn!(
                target: "cc",
                grace_secs = RESULT_EXIT_GRACE.as_secs(),
                "claude replied but did not exit — ending its process group"
            );
            end.cancel();
            let _ = tokio::time::timeout(END_GRACE, &mut run).await;
            Ok((
                ExecSummary {
                    exit_code: None,
                    attempts: 1,
                    truncated: false,
                    timed_out: false,
                },
                Ended::AfterResult,
            ))
        }
    }
}

/// Parse Claude Code's `stream-json` output, one object per line, recording
/// events and emitting progress.
///
/// Takes lines rather than a reader so the locally-spawned and dispatched paths
/// share it: the framing differs between them, the meaning does not.
///
/// Every line is read, however much came before it: the event that ends the
/// round comes last, and a reader that stops early leaves the CLI blocked on a
/// full pipe or its runner connection dropped.
async fn consume_stream_lines<G: StreamGrammar>(
    program: String,
    mut lines: tokio::sync::mpsc::Receiver<String>,
    capture: Arc<Mutex<G>>,
    progress_tx: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    result_seen: CancellationToken,
) {
    let mut total_bytes = 0usize;
    while let Some(line) = lines.recv().await {
        total_bytes = count_output(&program, total_bytes, line.len());
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        // Record before the (awaiting) progress emit, so an aborted drain
        // never loses an event it had already parsed.
        let progress = {
            let mut capture = lock(&capture);
            let progress = capture.record(&event);
            if capture.finished() {
                result_seen.cancel();
            }
            progress
        };
        if let Some(tx) = &progress_tx {
            for event in progress {
                let _ = tx.send(event).await;
            }
        }
    }
}

/// Recovering from poisoning is the point: aborting a drain mid-`record` can
/// poison the mutex, and the partially-captured output is exactly what the
/// caller needs.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How long to keep draining a reader after the process has been reaped.
const STREAM_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Wait for a reader task to finish, abandoning it after
/// [`STREAM_DRAIN_DEADLINE`].
///
/// Killing `claude` does not kill the processes *it* spawned (its own `bash`,
/// a build it started), and those inherit the pipe. An unbounded drain would therefore
/// block for as long as a grandchild lives — turning a 20-minute timeout into an
/// open-ended hang. Whatever was parsed before the cut-off is still readable
/// from the shared state the task writes into.
async fn drain_or_abort(mut task: tokio::task::JoinHandle<()>, stream: &str) {
    if tokio::time::timeout(STREAM_DRAIN_DEADLINE, &mut task)
        .await
        .is_err()
    {
        warn!(target: "cc", stream, "claude {stream} still open after the process exited — abandoning the drain");
        task.abort();
    }
}

/// Config for a retry: `--resume` plus what is left of the call's budget.
///
/// A previous attempt may have created the transcript file at `session_id`,
/// claiming the UUID. Re-running with `--session-id <uuid>` would then fail
/// with "Session ID … is already in use". Switch to `--resume`: if the
/// transcript exists we attach to it; if it doesn't, claude reports
/// `session_not_found` and the outer `dispatch_turn` recovery mints a fresh
/// UUID. No flip when the session isn't persisted (no transcript can exist) or
/// when no session_id is configured (one-shot).
fn config_for_retry(config: &ClaudeCodeConfig, budget: Duration) -> ClaudeCodeConfig {
    ClaudeCodeConfig {
        resume: config.resume || (config.persist_session && config.session_id.is_some()),
        timeout: budget,
        ..config.clone()
    }
}

/// Run with retries on failure. Never falls through to other providers.
///
/// `config.timeout` bounds the **whole call**, not each attempt: every attempt
/// draws from one [`CallBudget`], and a retry is skipped once too little is left
/// for it to reach a useful stopping point. A fresh full window per attempt is
/// how a 20-minute bound produced a 31-minute turn in production.
///
/// Retry policy depends on the outcome:
/// - **Rate limit (429)**: up to 2 retries with adaptive backoff. Anthropic's Max
///   plan caps input TPM, and a chunky resumed-session prefix can deplete the
///   bucket for several minutes; a 60s retry would 429 again immediately. The
///   backoff schedule [`RATE_LIMIT_BACKOFF_SECS`] is sized for typical refill
///   times. Each wait emits `ProgressEvent::RateLimited` so the UI can show the
///   wait state instead of appearing stuck.
/// - **Other failures**: a single 60s retry, matching the prior behaviour.
/// - **Budget exhaustion ([`Completion::TurnCap`] / [`Completion::Timeout`])**:
///   returned as-is. The round did real work; re-running it would only reach the
///   same limit again, and could duplicate a branch or PR it already pushed.
/// - **Cancellation / session-not-found**: returned immediately without retry.
pub async fn run_with_retry(
    config: &ClaudeCodeConfig,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
) -> Result<ClaudeCodeResult> {
    let budget = CallBudget::new(
        config.timeout,
        config
            .jobs
            .as_ref()
            .map_or(Duration::ZERO, RoundJobs::spent),
    );
    let mut attempt: u32 = 0;
    // A keyed round's first attempt has what is left of the round's budget;
    // a round collected at a later attempt runs it as that retry would have.
    let mut cfg_for_call = match config.jobs.as_ref() {
        Some(jobs) if jobs.pinned() && jobs.attempt > 0 => {
            config_for_retry(config, budget.remaining())
        }
        Some(_) => ClaudeCodeConfig {
            timeout: budget.remaining(),
            ..config.clone()
        },
        None => config.clone(),
    };
    loop {
        cfg_for_call.jobs = config.jobs.as_ref().map(|jobs| jobs.for_attempt(attempt));
        attempt += 1;
        let result = run(
            &cfg_for_call,
            vault,
            global_vault,
            cancel.clone(),
            progress_tx.clone(),
        )
        .await;

        // A collected round resumes at its one recorded job and decides
        // nothing past it: a retry could start work the round never chose.
        if cfg_for_call.jobs.as_ref().is_some_and(RoundJobs::pinned) {
            return result;
        }
        match result {
            Ok(r) => match after_attempt(&r, attempt) {
                AfterAttempt::Return => return Ok(r),
                AfterAttempt::UsageCapped => {
                    info!(
                        target: "cc",
                        attempt,
                        reason = %r.failure_line(),
                        "Claude Code usage window spent; not retrying"
                    );
                    return Ok(r);
                }
                AfterAttempt::Retry {
                    delay,
                    rate_limited,
                } => {
                    let Some(left) = budget.next_attempt(delay) else {
                        warn!(
                            exit_code = r.exit_code,
                            attempt,
                            completion = r.completion.as_str(),
                            usage_capped = false,
                            rate_limited,
                            reason = %r.failure_line(),
                            remaining_secs = budget.remaining().as_secs(),
                            "Claude Code failed with too little budget left to retry"
                        );
                        return Ok(r);
                    };
                    warn!(
                        exit_code = r.exit_code,
                        attempt,
                        completion = r.completion.as_str(),
                        usage_capped = false,
                        rate_limited,
                        reason = %r.failure_line(),
                        delay_secs = delay.as_secs(),
                        next_budget_secs = left.as_secs(),
                        "Claude Code failed, retrying"
                    );
                    announce_rate_limit_wait(&progress_tx, rate_limited, delay.as_secs(), attempt)
                        .await;
                    tokio::time::sleep(delay).await;
                    cfg_for_call = config_for_retry(config, left);
                }
            },
            Err(e) if is_cancelled(&e) => return Err(e),
            Err(e) => {
                let delay = Duration::from_secs(DEFAULT_RETRY_DELAY_SECS);
                let Some(left) = budget.next_attempt(delay).filter(|_| attempt == 1) else {
                    return Err(e);
                };
                warn!(error = %e, "Claude Code error, retrying once in 60s");
                tokio::time::sleep(delay).await;
                cfg_for_call = config_for_retry(config, left);
            }
        }
    }
}

/// What a finished attempt calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterAttempt {
    /// Hand the round back as it is.
    Return,
    /// Hand the failed round back: its usage window is spent, so a retry on
    /// the same model is wasted.
    UsageCapped,
    /// Run the round again once `delay` has passed.
    Retry { delay: Duration, rate_limited: bool },
}

/// Whether a finished attempt is retried, and after how long.
fn after_attempt(r: &ClaudeCodeResult, attempt: u32) -> AfterAttempt {
    if !r.completion.is_retryable() {
        return AfterAttempt::Return;
    }
    if is_usage_capped(&r.text) {
        return AfterAttempt::UsageCapped;
    }
    let rate_limited = is_rate_limited(&r.text);
    next_retry_delay(rate_limited, attempt, &r.text).map_or(AfterAttempt::Return, |secs| {
        AfterAttempt::Retry {
            delay: Duration::from_secs(secs),
            rate_limited,
        }
    })
}

/// Decide whether to retry, and after how many seconds, based on the failure shape.
/// `None` means give up — return the failed result to the caller as-is.
fn next_retry_delay(rate_limited: bool, attempt: u32, text: &str) -> Option<u64> {
    if let Some(secs) = parse_retry_after(text) {
        return Some(secs);
    }
    if rate_limited {
        // attempt is 1-indexed: attempt=1 means "first call just failed; wait
        // RATE_LIMIT_BACKOFF_SECS[0] before retry #2".
        RATE_LIMIT_BACKOFF_SECS.get((attempt - 1) as usize).copied()
    } else if attempt == 1 {
        Some(DEFAULT_RETRY_DELAY_SECS)
    } else {
        None
    }
}

/// Notify any subscribed renderer that the runner is sleeping due to a 429,
/// so the UI can swap the working spinner for a meaningful "rate-limited" state.
async fn announce_rate_limit_wait(
    progress_tx: &Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
    rate_limited: bool,
    delay_secs: u64,
    attempt: u32,
) {
    if !rate_limited {
        return;
    }
    let Some(tx) = progress_tx else {
        return;
    };
    let _ = tx
        .send(workforce_agent::runner::ProgressEvent::RateLimited {
            wait_secs: delay_secs,
            attempt,
        })
        .await;
}

fn log_stream_event(event: &serde_json::Value) {
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match event_type {
        "assistant" => {
            if let Some(tool) = event
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            b.get("name").and_then(|n| n.as_str())
                        } else {
                            None
                        }
                    })
                })
            {
                info!(tool, "Claude Code tool call");
            }
        }
        "result" => {
            let turns = event.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0);
            let cost = event
                .get("total_cost_usd")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0);
            let input_tokens = event
                .pointer("/usage/input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output_tokens = event
                .pointer("/usage/output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cache_creation_tokens = event
                .pointer("/usage/cache_creation_input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cache_read_tokens = event
                .pointer("/usage/cache_read_input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            info!(
                turns,
                cost_usd = format!("{cost:.4}"),
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                "Claude Code finished"
            );
        }
        _ => {}
    }
}

/// The progress a Claude Code event implies, counting assistant turns in
/// `round`.
fn progress_events_of(event: &serde_json::Value, round: &mut u32) -> Vec<ProgressEvent> {
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match event_type {
        "assistant" => {
            *round += 1;
            let blocks = event
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten();
            std::iter::once(ProgressEvent::LlmCall { round: *round })
                .chain(blocks.filter_map(|block| {
                    match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "text" => block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .filter(|t| !t.is_empty())
                            .map(|text| ProgressEvent::Response {
                                text: text.to_string(),
                            }),
                        "tool_use" => {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            Some(ProgressEvent::ToolStart {
                                name: format!("claude:{name}"),
                                round: *round,
                                detail: summarize_tool_input(name, block.get("input")),
                            })
                        }
                        _ => None,
                    }
                }))
                .collect()
        }
        "result" => {
            let count =
                |pointer: &str| event.pointer(pointer).and_then(|t| t.as_u64()).unwrap_or(0) as u32;
            vec![ProgressEvent::Done {
                rounds: event.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                input_tokens: count("/usage/input_tokens"),
                output_tokens: count("/usage/output_tokens"),
                cache_read_tokens: count("/usage/cache_read_input_tokens"),
                tool_count: 0,
            }]
        }
        _ => Vec::new(),
    }
}

/// A contained round's event as an agent turn's progress: each model call
/// (`last_call` is the one the previous event belonged to), then its text
/// blocks as streamed segments. The message the CLI makes of an API error is
/// no model output and is not shown.
fn turn_progress_of(
    event: &serde_json::Value,
    round: &mut u32,
    last_call: &mut Option<String>,
) -> Vec<ProgressEvent> {
    if event.get("type").and_then(|t| t.as_str()) != Some("assistant") || reports_api_error(event) {
        return Vec::new();
    }
    let call = event
        .pointer("/message/id")
        .and_then(|id| id.as_str())
        .map(str::to_string);
    let new_call = call.is_none() || call != *last_call;
    *last_call = call;
    if new_call {
        *round += 1;
    }
    let texts = event
        .pointer("/message/content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
        .filter(|text| !text.is_empty());
    new_call
        .then_some(ProgressEvent::LlmCall { round: *round })
        .into_iter()
        .chain(texts.flat_map(segment))
        .collect()
}

/// `text` as a closed stream segment, the way an agent turn reports what its
/// model said along the way.
pub(crate) fn segment(text: &str) -> [ProgressEvent; 3] {
    [
        ProgressEvent::StreamStart,
        ProgressEvent::StreamDelta {
            text: text.to_string(),
        },
        ProgressEvent::StreamEnd,
    ]
}

/// Whether an assistant event is the CLI's own report of an API error (a
/// refused login, a spent limit) rather than anything the model said.
fn reports_api_error(event: &serde_json::Value) -> bool {
    event
        .get("is_api_error_message")
        .and_then(|flag| flag.as_bool())
        .unwrap_or(false)
        || event.pointer("/message/model").and_then(|m| m.as_str()) == Some("<synthetic>")
}

/// Short human-readable summary of a Claude Code tool call for progress display.
fn summarize_tool_input(tool: &str, input: Option<&serde_json::Value>) -> String {
    const MAX: usize = 120;
    let Some(input) = input else {
        return String::new();
    };
    let raw = match tool {
        "Read" | "Edit" | "Write" | "NotebookEdit" => input
            .get("file_path")
            .and_then(|p| p.as_str())
            .unwrap_or(""),
        "Bash" => input.get("command").and_then(|c| c.as_str()).unwrap_or(""),
        "Glob" | "Grep" => input.get("pattern").and_then(|p| p.as_str()).unwrap_or(""),
        _ => "",
    };
    if raw.is_empty() {
        String::new()
    } else if raw.len() <= MAX {
        raw.to_string()
    } else {
        let mut truncated: String = raw.chars().take(MAX).collect();
        truncated.push('…');
        truncated
    }
}

fn parse_retry_after(text: &str) -> Option<u64> {
    let lower = text.to_lowercase();
    let pos = lower.find("retry-after")?;
    lower[pos..]
        .split_whitespace()
        .filter_map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_digit())
                .parse::<u64>()
                .ok()
        })
        .find(|&secs| secs > 0 && secs < 600)
}

/// One coding round, its progress bridged onto the parent turn's status line
/// unless the caller renders it from `progress` itself.
pub async fn run_coding_round(
    config: &ClaudeCodeConfig,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    cancel: Option<CancellationToken>,
    progress: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
) -> Result<ClaudeCodeResult> {
    match progress {
        Some(progress) => run_with_retry(config, vault, global_vault, cancel, Some(progress)).await,
        None => run_with_subactivity(config, vault, global_vault, cancel, "coding").await,
    }
}

/// Run Claude Code and stream its nested activity onto the current agent turn's
/// status line as `<source> ▸ …`. The convenience over [`run_with_retry`] +
/// [`bridge_subactivity`] for the "drive CC from inside a tool" case (advisor,
/// `coding.session`): wires the bridge to `current_progress_tx` and drives both
/// concurrently so the inner progress channel is always drained.
pub async fn run_with_subactivity(
    config: &ClaudeCodeConfig,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    cancel: Option<CancellationToken>,
    source: &str,
) -> Result<ClaudeCodeResult> {
    let (inner_tx, inner_rx) = tokio::sync::mpsc::channel(64);
    let outer = workforce_agent::runner::current_progress_tx();
    let (result, _) = tokio::join!(
        run_with_retry(config, vault, global_vault, cancel, Some(inner_tx)),
        bridge_subactivity(source, inner_rx, outer),
    );
    result
}

/// Translate a nested Claude Code progress stream onto the *parent* agent
/// turn's status line as [`workforce_agent::runner::ProgressEvent::SubActivity`],
/// so a human watching sees the advisor / `coding.session` actually working
/// instead of a status line that looks frozen for minutes.
///
/// `inner_rx` receives the events emitted by [`run_with_retry`]; `outer` is the
/// turn's progress sender (from `workforce_agent::runner::current_progress_tx`),
/// or `None` on surfaces with no progress rendering. This future **always drains
/// `inner_rx` to completion** — even when `outer` is `None` — because the
/// producer sends with `.await` and would otherwise stall once the channel fills.
///
/// It forwards *semantic* state only (the latest activity detail); the elapsed
/// counter is owned by each renderer (the Slack consumer and the gateway both
/// tick the in-flight line on their own heartbeat). Emitting elapsed here too
/// would double-count (`advisor ▸ thinking · 36s · 5s`). Every send is
/// `try_send` + drop-on-full: cosmetic progress must never back-pressure the
/// stream it reports on.
///
/// Run it concurrently with the work it reports on (inline, not spawned), e.g.
/// `tokio::join!(run_with_retry(..), bridge_subactivity("advisor", rx, outer))`.
pub async fn bridge_subactivity(
    source: &str,
    mut inner_rx: tokio::sync::mpsc::Receiver<workforce_agent::runner::ProgressEvent>,
    outer: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
) {
    use workforce_agent::runner::ProgressEvent;

    while let Some(event) = inner_rx.recv().await {
        if let Some(detail) = subactivity_detail(&event) {
            if let Some(tx) = &outer {
                // Drop on full/closed: never block the stream we're reporting on.
                let _ = tx.try_send(ProgressEvent::SubActivity {
                    source: source.to_string(),
                    detail,
                });
            }
        }
    }
}

/// Map a Claude Code progress event to a one-line status string, or `None` for
/// events that should not move the status line (stream deltas, tool-end,
/// completion). Reuses the Slack renderer's tool-name vocabulary so nested
/// activity reads consistently with the agent's own tool calls.
fn subactivity_detail(event: &workforce_agent::runner::ProgressEvent) -> Option<String> {
    use workforce_agent::runner::ProgressEvent;
    match event {
        ProgressEvent::LlmCall { round } => Some(format!("thinking (round {round})")),
        ProgressEvent::ToolStart { name, detail, .. } => {
            let label = crate::slack_manager::friendly_tool_name(name);
            if detail.trim().is_empty() {
                Some(label.to_string())
            } else {
                Some(format!(
                    "{label}: {}",
                    crate::slack_manager::truncate_detail(detail, 80)
                ))
            }
        }
        ProgressEvent::RateLimited { wait_secs, .. } => Some(format!(
            "rate-limited, waiting ~{}m",
            wait_secs.div_ceil(60)
        )),
        ProgressEvent::SlowModelFailover {
            model,
            elapsed_secs,
        } => Some(format!(
            "{model} silent for {elapsed_secs}s, switching model"
        )),
        _ => None,
    }
}

/// Extract the outermost JSON array from a CC response. Tolerates leading
/// prose or markdown fences by slicing from the first `[` to the last `]`.
/// Returns the array's elements; an empty vec when no well-formed array is
/// present. Shared by the toolless one-shot backends (self-learning curation,
/// memory consolidation) that ask CC to answer with a single JSON array.
pub fn extract_json_array(text: &str) -> Vec<serde_json::Value> {
    let trimmed = text.trim();
    let slice = match (trimmed.find('['), trimmed.rfind(']')) {
        (Some(a), Some(b)) if b > a => &trimmed[a..=b],
        _ => return Vec::new(),
    };
    match serde_json::from_str::<serde_json::Value>(slice) {
        Ok(serde_json::Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    tokio::task_local! {
        /// The `claude` a test's rounds run instead of the real CLI.
        pub(crate) static CLAUDE: PathBuf;
    }

    /// Scripted `claude` runs, recorded and replayed.
    pub(crate) mod scripted {
        use std::path::PathBuf;
        use std::time::Duration;

        use serde_json::{json, Value};

        /// A scripted CLI. Each run records its arguments, working directory
        /// and stdin, and what `records` keeps besides, then plays the reply
        /// scripted for its number, else the default one.
        pub(crate) struct ScriptedCli {
            dir: tempfile::TempDir,
            name: &'static str,
        }

        impl ScriptedCli {
            /// The CLI `name`, each run first running `records`, a shell
            /// fragment that may read `$dir` and the run's number `$n`, and
            /// replying `default` unless scripted otherwise.
            pub(crate) fn new(name: &'static str, records: &str, default: &str) -> Self {
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::tempdir().unwrap();
                let script = format!(
                    r#"#!/bin/sh
        dir='{}'
        n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
        echo "$n" > "$dir/count"
        printf '%s\n' "$@" > "$dir/$n.args"
        pwd > "$dir/$n.cwd"
        {records}
        cat > "$dir/$n.stdin"
        reply="$dir/reply-$n"
        [ -f "$reply" ] || reply="$dir/reply"
        . "$reply"
        "#,
                    dir.path().display()
                );
                let path = dir.path().join(name);
                std::fs::write(&path, script).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                let cli = Self { dir, name };
                cli.script(None, default);
                cli
            }

            pub(crate) fn program(&self) -> PathBuf {
                self.dir.path().join(self.name)
            }

            /// Run `round` (every run without one of its own when `None`) runs
            /// `script`.
            pub(crate) fn script(&self, round: Option<usize>, script: &str) {
                let name = round.map_or("reply".to_string(), |n| format!("reply-{n}"));
                std::fs::write(self.dir.path().join(name), script).unwrap();
            }

            /// What a run left in `name` of the script's directory.
            pub(crate) fn file(&self, name: &str) -> String {
                std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
            }

            /// What run `n` (from 1) recorded under `name`.
            pub(crate) fn recorded(&self, n: usize, name: &str) -> String {
                self.file(&format!("{n}.{name}"))
            }

            /// How many times the CLI has run.
            pub(crate) fn runs(&self) -> usize {
                self.file("count").trim().parse().unwrap_or(0)
            }

            /// Until the scripted CLI has begun its `n`th run.
            pub(crate) fn started(
                &self,
                n: usize,
            ) -> impl std::future::Future<Output = ()> + Send + 'static {
                let count = self.dir.path().join("count");
                async move {
                    let runs = || {
                        std::fs::read_to_string(&count)
                            .ok()
                            .and_then(|runs| runs.trim().parse::<usize>().ok())
                            .unwrap_or(0)
                    };
                    while runs() < n {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }

        /// A scripted `claude`, recording besides each run's system prompt,
        /// token and tool timeout.
        pub(crate) struct ScriptedClaude(ScriptedCli);

        impl std::ops::Deref for ScriptedClaude {
            type Target = ScriptedCli;

            fn deref(&self) -> &ScriptedCli {
                &self.0
            }
        }

        /// One recorded run of the scripted `claude`.
        pub(crate) struct Call {
            pub(crate) args: Vec<String>,
            pub(crate) stdin: Value,
            pub(crate) prompt: String,
            pub(crate) token: String,
            pub(crate) tool_timeout: String,
        }

        impl Call {
            /// The value following `flag`.
            pub(crate) fn flag(&self, flag: &str) -> Option<&str> {
                self.args
                    .iter()
                    .position(|arg| arg == flag)
                    .and_then(|i| self.args.get(i + 1))
                    .map(String::as_str)
            }

            pub(crate) fn has(&self, flag: &str) -> bool {
                self.args.iter().any(|arg| arg == flag)
            }

            /// The text blocks of the user message it was sent.
            pub(crate) fn text(&self) -> String {
                self.stdin["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }

        impl ScriptedClaude {
            pub(crate) fn new() -> Self {
                Self(ScriptedCli::new(
                    "claude",
                    r#"prompt=$(awk 'prev=="--system-prompt-file"{print} {prev=$0}' "$dir/$n.args")
        [ -n "$prompt" ] && cp "$prompt" "$dir/$n.prompt"
        printf '%s' "$CLAUDE_CODE_OAUTH_TOKEN" > "$dir/$n.token"
        printf '%s' "$MCP_TOOL_TIMEOUT" > "$dir/$n.timeout""#,
                    &print(&answer("default answer")),
                ))
            }

            /// `call`, its rounds run on this `claude`.
            pub(crate) async fn scoped<F: std::future::Future>(&self, call: F) -> F::Output {
                super::CLAUDE.scope(self.program(), call).await
            }

            /// Run `round` (every run without one of its own when `None`) prints
            /// `events` and exits cleanly.
            pub(crate) fn reply(&self, round: Option<usize>, events: &[Value]) {
                self.script(round, &print(events));
            }

            pub(crate) fn calls(&self) -> Vec<Call> {
                (1..=self.runs())
                    .map(|n| Call {
                        args: self
                            .recorded(n, "args")
                            .lines()
                            .map(str::to_string)
                            .collect(),
                        stdin: serde_json::from_str(self.recorded(n, "stdin").trim())
                            .unwrap_or(Value::Null),
                        prompt: self.recorded(n, "prompt"),
                        token: self.recorded(n, "token"),
                        tool_timeout: self.recorded(n, "timeout"),
                    })
                    .collect()
            }
        }

        /// A script printing each of `events` as a line.
        pub(crate) fn print(events: &[Value]) -> String {
            events
                .iter()
                .map(|event| {
                    format!(
                        "printf '%s\\n' '{}'\n",
                        event.to_string().replace('\'', "'\\''")
                    )
                })
                .collect()
        }

        pub(crate) fn assistant(id: &str, text: &str) -> Value {
            json!({
                "type": "assistant",
                "message": {
                    "id": id,
                    "model": "claude-sonnet-4-6",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }],
                },
            })
        }

        pub(crate) fn result(text: &str) -> Value {
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "num_turns": 1,
                "result": text,
                "usage": { "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100 },
                "total_cost_usd": 7.5,
            })
        }

        /// A round that answers `text`.
        pub(crate) fn answer(text: &str) -> Vec<Value> {
            vec![assistant("msg-answer", text), result(text)]
        }

        /// A round refused at its five-hour limit, as 2.1.284 reports one: the
        /// CLI's own API-error message, the rejected window, an error result.
        pub(crate) fn rejected() -> Vec<Value> {
            rejected_until(chrono::Utc::now() + chrono::TimeDelta::hours(3))
        }

        /// [`rejected`], its window resetting at `resets`.
        pub(crate) fn rejected_until(resets: chrono::DateTime<chrono::Utc>) -> Vec<Value> {
            let resets = resets.timestamp();
            vec![
                json!({
                    "type": "assistant",
                    "is_api_error_message": true,
                    "error": "rate_limit",
                    "message": {
                        "id": "msg-limit",
                        "model": "<synthetic>",
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "You've hit your session limit" }],
                    },
                }),
                json!({
                    "type": "rate_limit_event",
                    "rate_limit_info": { "status": "rejected", "resetsAt": resets, "rateLimitType": "five_hour" },
                }),
                json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": true,
                    "num_turns": 1,
                    "result": "You've hit your session limit",
                }),
            ]
        }
    }

    #[test]
    fn an_error_result_at_the_turn_cap_is_a_failure() {
        let refused = capture_of(&[serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "num_turns": 1,
            "result": "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
        })]);
        assert_eq!(
            completion_of(1, false, false, &refused, Some(1)),
            Completion::Failed,
            "a one-turn round's refusal is not a turn cap"
        );
        let capped = capture_of(&[assistant("a"), assistant("b")]);
        assert_eq!(
            completion_of(1, false, false, &capped, Some(2)),
            Completion::TurnCap
        );
    }

    #[test]
    fn an_error_result_on_the_last_of_many_turns_is_retried() {
        for (error, rate_limited, delay) in [
            (
                "API Error: 500 Internal server error",
                false,
                DEFAULT_RETRY_DELAY_SECS,
            ),
            (
                "API Error: Request rejected (429)",
                true,
                RATE_LIMIT_BACKOFF_SECS[0],
            ),
        ] {
            let failed = capture_of(&[serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "num_turns": 30,
                "result": error,
            })]);
            let completion = completion_of(1, false, false, &failed, Some(30));
            assert_eq!(completion, Completion::Failed, "{error}: not a turn cap");
            let result = ClaudeCodeResult {
                completion,
                ..cc_result(1, error, "")
            };
            assert_eq!(
                after_attempt(&result, 1),
                AfterAttempt::Retry {
                    delay: Duration::from_secs(delay),
                    rate_limited
                },
                "{error}"
            );
        }
    }

    #[test]
    fn subactivity_detail_maps_visible_events_and_skips_noise() {
        use workforce_agent::runner::ProgressEvent;

        assert_eq!(
            subactivity_detail(&ProgressEvent::LlmCall { round: 2 }).as_deref(),
            Some("thinking (round 2)")
        );
        // Tool name is stripped to its leaf and joined with the detail.
        let tool = ProgressEvent::ToolStart {
            name: "Read".into(),
            round: 1,
            detail: "src/main.rs".into(),
        };
        assert_eq!(
            subactivity_detail(&tool).as_deref(),
            Some("Read: src/main.rs")
        );
        assert_eq!(
            subactivity_detail(&ProgressEvent::RateLimited {
                wait_secs: 90,
                attempt: 1
            })
            .as_deref(),
            Some("rate-limited, waiting ~2m")
        );
        // Completion / stream noise must not move the status line.
        assert!(subactivity_detail(&ProgressEvent::Done {
            rounds: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            tool_count: 0,
        })
        .is_none());
        assert!(subactivity_detail(&ProgressEvent::StreamDelta { text: "x".into() }).is_none());
    }

    #[test]
    fn parse_retry_after_extracts_seconds() {
        assert_eq!(
            parse_retry_after("Rate limited. Retry-after: 30 seconds"),
            Some(30)
        );
        assert_eq!(parse_retry_after("no retry info here"), None);
        assert_eq!(parse_retry_after("Retry-after: 0"), None);
        assert_eq!(parse_retry_after("Retry-after: 999"), None);
    }

    #[test]
    fn a_usage_capped_failure_is_returned_without_a_retry() {
        let capped = cc_result(1, "You've hit your weekly limit · resets Sep 18, 9am", "");
        assert_eq!(after_attempt(&capped, 1), AfterAttempt::UsageCapped);
        assert_eq!(
            after_attempt(&cc_result(1, "segfault", ""), 1),
            AfterAttempt::Retry {
                delay: Duration::from_secs(DEFAULT_RETRY_DELAY_SECS),
                rate_limited: false
            }
        );
        assert_eq!(
            after_attempt(&cc_result(1, "API Error: Request rejected (429)", ""), 1),
            AfterAttempt::Retry {
                delay: Duration::from_secs(RATE_LIMIT_BACKOFF_SECS[0]),
                rate_limited: true
            }
        );
        assert_eq!(
            after_attempt(&cc_result(1, "segfault", ""), 2),
            AfterAttempt::Return
        );
        assert_eq!(
            after_attempt(&cc_result(0, "You've hit your weekly limit", ""), 1),
            AfterAttempt::Return,
            "a round that did not fail is never retried"
        );
    }

    #[test]
    fn failure_line_prefers_the_last_line_then_the_crash() {
        let salvaged = cc_result(1, "…\nworking on it\nAPI Error: overloaded\n\n", "");
        assert_eq!(salvaged.failure_line(), "API Error: overloaded");
        let killed = ClaudeCodeResult {
            text: "…\n".into(),
            ..cc_result(137, "", "")
        };
        assert_eq!(killed.failure_line(), CrashKind::KilledLikelyOom.label());
        let bare = ClaudeCodeResult {
            crash_kind: None,
            ..cc_result(1, "", "")
        };
        assert_eq!(bare.failure_line(), "exit 1");
    }

    #[test]
    fn is_cancelled_matches_only_cancellation() {
        assert!(is_cancelled(&eyre!("claude cancelled")));
        assert!(is_cancelled(&eyre!("round cancelled by the caller")));
        assert!(!is_cancelled(&eyre!(
            "secure command runner unavailable: output consumer dropped"
        )));
        assert!(!is_cancelled(&eyre!("You've hit your weekly limit")));
    }

    #[test]
    fn passes_large_output_is_true_on_exactly_one_line() {
        let crossings = |lengths: &[usize]| {
            lengths
                .iter()
                .scan(0, |before, &len| {
                    let passes = passes_large_output(*before, len);
                    *before += len;
                    Some(passes)
                })
                .filter(|&passes| passes)
                .count()
        };
        assert_eq!(crossings(&[1_000; 3_000]), 1);
        assert_eq!(crossings(&[LARGE_OUTPUT_BYTES, 1, 1]), 1);
        assert_eq!(crossings(&[LARGE_OUTPUT_BYTES - 1, 1, 0, 1]), 1);
        assert_eq!(crossings(&[LARGE_OUTPUT_BYTES]), 0);
        assert_eq!(crossings(&[5 * LARGE_OUTPUT_BYTES, 1]), 1);
        assert_eq!(crossings(&[]), 0);
    }

    #[test]
    fn stderr_capture_keeps_its_opening_and_its_end() {
        let opening = "No conversation found with session ID x\n";
        let body: Vec<u8> = (0..1024 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
        let mut capture = StderrCapture::default();
        capture.push(opening.as_bytes());
        body.chunks(4 * 1024).for_each(|chunk| capture.push(chunk));
        let bytes = capture.into_bytes();
        let text = String::from_utf8_lossy(&bytes);

        assert!(stderr_indicates_session_not_found(&text));
        assert!(bytes.ends_with(&body[body.len() - STDERR_TAIL_BYTES..]));
        let at = text.find("\n[").expect("the gap is marked");
        let omitted: usize = text[at + 2..]
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("the marker names what it omitted");
        let marker = format!("\n[{omitted} bytes of stderr omitted]\n");
        assert_eq!(
            &text[STDERR_HEAD_BYTES..STDERR_HEAD_BYTES + marker.len()],
            marker
        );
        assert_eq!(
            bytes.len() - marker.len() + omitted,
            opening.len() + body.len()
        );
        assert!(bytes.len() <= STDERR_HEAD_BYTES + 2 * STDERR_KEEP_BYTES + marker.len());

        let mut short = StderrCapture::default();
        short.push(b"boom\n");
        short.push(b"again");
        assert_eq!(short.into_bytes(), b"boom\nagain");
    }

    fn cc_result(exit_code: i32, text: &str, stderr_tail: &str) -> ClaudeCodeResult {
        ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: text.to_string(),
            exit_code,
            duration: Duration::from_secs(0),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            num_turns: 0,
            completion: if exit_code == 0 {
                Completion::Success
            } else {
                Completion::Failed
            },
            crash_kind: CrashKind::from_exit_code(exit_code),
            stderr_tail: stderr_tail.to_string(),
            handed_over: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn classify_failure_none_on_success() {
        assert!(classify_failure(&Ok(cc_result(0, "all good", ""))).is_none());
    }

    fn capture_of(events: &[serde_json::Value]) -> StreamCapture {
        let mut capture = StreamCapture::default();
        for event in events {
            capture.record(event);
        }
        capture
    }

    fn assistant(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": text }] },
        })
    }

    fn result_event(subtype: &str, num_turns: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "result",
            "subtype": subtype,
            "num_turns": num_turns,
            "result": "final answer",
        })
    }

    #[test]
    fn contained_round_keeps_tool_search_whatever_the_install_blocks() {
        assert!(blocks_tool_search(Some("1"), false));
        assert!(blocks_tool_search(Some("TRUE"), false));
        assert!(!blocks_tool_search(None, false));
        assert!(!blocks_tool_search(Some("0"), false));
        assert!(
            !blocks_tool_search(Some("1"), true),
            "a contained round loads every tool it has through ToolSearch"
        );
    }

    #[test]
    fn completion_of_maps_every_outcome() {
        let clean = capture_of(&[result_event("success", 3)]);
        assert_eq!(
            completion_of(0, false, false, &clean, Some(30)),
            Completion::Success
        );

        // The subtype alone is enough, even when the turn count is nowhere
        // near the cap.
        let capped = capture_of(&[result_event("error_max_turns", 4)]);
        assert_eq!(
            completion_of(1, false, false, &capped, Some(30)),
            Completion::TurnCap
        );

        // …and so is the count alone, if the CLI ever renames the subtype.
        // Prod reported 31 turns against `--max-turns 30`, hence `>=`.
        let renamed = capture_of(&[result_event("error_something_new", 31)]);
        assert_eq!(
            completion_of(1, false, false, &renamed, Some(30)),
            Completion::TurnCap
        );

        // A non-zero exit well short of the cap is a real failure.
        let crashed = capture_of(&[result_event("error_during_execution", 4)]);
        assert_eq!(
            completion_of(1, false, false, &crashed, Some(30)),
            Completion::Failed
        );

        // With no cap configured, only the subtype can indicate one.
        assert_eq!(
            completion_of(1, false, false, &renamed, None),
            Completion::Failed
        );

        // The CLI's reported cap outranks a later timeout and session-loss…
        assert_eq!(
            completion_of(1, true, true, &capped, Some(30)),
            Completion::TurnCap
        );
        assert_eq!(
            completion_of(1, false, true, &capped, Some(30)),
            Completion::TurnCap
        );
        // …while a timeout still outranks an unrecognised error and session-loss.
        assert_eq!(
            completion_of(1, true, true, &crashed, Some(30)),
            Completion::Timeout
        );
        assert_eq!(
            completion_of(1, false, true, &crashed, Some(30)),
            Completion::SessionNotFound
        );

        // …but not the CLI's own "success": a round that finished and then hung
        // on shutdown is done, and must not be reported as continuable. Note the
        // -1: `assemble_result` overwrites the exit code of a round we killed,
        // so this branch cannot lean on `exit_code == 0`.
        assert_eq!(
            completion_of(-1, true, false, &clean, Some(30)),
            Completion::Success
        );

        // A timeout that salvaged nothing has nothing to hand back, so it is a
        // failure — not a "success" with an empty response.
        let nothing = StreamCapture::default();
        assert_eq!(
            completion_of(-1, true, false, &nothing, Some(30)),
            Completion::Failed
        );
        let some_narration = capture_of(&[assistant("pushed a branch")]);
        assert_eq!(
            completion_of(-1, true, false, &some_narration, Some(30)),
            Completion::Timeout
        );
    }

    /// Git in a remote scope answers github.com with the round's token and
    /// leaves nothing behind in the scope's config.
    #[test]
    fn remote_git_credentials_come_from_the_rounds_token() {
        let env = remote_git_credential_env();
        assert_eq!(env[0], ("GIT_CONFIG_COUNT", "1"));
        assert_eq!(env[1], ("GIT_CONFIG_KEY_0", "credential.helper"));
        let helper = env[2].1;
        assert!(helper.starts_with('!'), "{helper}");
        assert!(helper.contains("username=x-access-token"));
        assert!(
            helper.contains("$GH_TOKEN"),
            "the token is read at use, not baked in"
        );
    }

    /// A remote round names its scope and binds MCP for the worker; a hub-local
    /// one does neither and inherits nothing new.
    #[test]
    fn dispatch_targets_shape_the_request_for_their_daemon() {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .current_dir("/work/rails")
            .env("GH_TOKEN", "t");
        let endpoint = RunnerEndpoint::Unix(PathBuf::from("/run/exec.sock"));

        let local =
            DispatchTarget::local(&endpoint).request(&cmd, "task".into(), Duration::from_secs(9));
        assert_eq!(local.scope, None);
        assert_eq!(local.mcp, None);
        assert_eq!(local.workdir.as_deref(), Some("/work/rails"));
        assert_eq!(local.timeout_secs, 9);
        assert!(DispatchTarget::local(&endpoint).responder().is_none());

        let served = McpEndpoint::new(
            crate::mcp::Surface::Fixed(Arc::new(
                workforce_agent::tools::registry::ToolRegistry::new(),
            )),
            "workforce-a".into(),
            Some("served".into()),
            Arc::new(crate::mcp::DirectExecutor),
        );
        let placement = RemotePlacement {
            endpoint: endpoint.clone(),
            scope: ScopeId::new("scope-1"),
            jobs: false,
        };
        let target = DispatchTarget::remote(&placement, Some(&served), None, None);
        let remote = target.request(&cmd, "task".into(), Duration::from_secs(9));
        assert_eq!(remote.scope, Some(ScopeId::new("scope-1")));
        assert_eq!(
            remote.mcp,
            Some(McpBinding {
                server_name: MCP_SERVER_KEY.into(),
                instructions: Some("served".into()),
                codex_config: None,
            })
        );
        assert_eq!(remote.env, vec![("GH_TOKEN".to_string(), "t".to_string())]);
        assert!(target.responder().is_some());

        assert_eq!(
            DispatchTarget::remote(&placement, None, None, None)
                .request(&cmd, String::new(), Duration::ZERO)
                .mcp,
            None
        );
    }

    /// A worker that holds every job open until it is asked to cancel,
    /// recording the jobs started and the jobs cancelled.
    #[derive(Default)]
    struct HoldingWorker {
        started: std::sync::Mutex<Vec<String>>,
        cancelled: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl workforce_agent::tools::exec_runner::JobHandler for HoldingWorker {
        async fn run(
            &self,
            request: workforce_agent::tools::exec_runner::Request,
            link: workforce_agent::tools::exec_runner::JobLink,
        ) -> Result<workforce_agent::tools::exec_runner::ServerFrame> {
            let workforce_agent::tools::exec_runner::Request::Run(run) = request else {
                eyre::bail!("only jobs are held");
            };
            if run.exec.is_some() {
                self.started.lock().unwrap().push(run.job.to_string());
            }
            link.cancel_requested().await;
            self.cancelled.lock().unwrap().push(run.job.to_string());
            Ok(workforce_agent::tools::exec_runner::ServerFrame::Done(
                ExecSummary::killed(),
            ))
        }
    }

    async fn holding_worker(dir: &std::path::Path) -> (Arc<HoldingWorker>, RemotePlacement) {
        use std::os::unix::fs::MetadataExt;
        let socket = dir.join("worker.sock");
        let worker = Arc::new(HoldingWorker::default());
        let listener = workforce_agent::tools::exec_runner::Listener::Unix {
            path: socket.clone(),
            accept_uid: std::fs::metadata(dir).unwrap().uid(),
        };
        let handler: Arc<dyn workforce_agent::tools::exec_runner::JobHandler> = worker.clone();
        tokio::spawn(workforce_agent::tools::exec_runner::serve(
            listener, handler,
        ));
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let placement = RemotePlacement {
            endpoint: RunnerEndpoint::Unix(socket),
            scope: ScopeId::new("scope-1"),
            jobs: true,
        };
        (worker, placement)
    }

    async fn until_started(worker: &HoldingWorker) {
        while worker.started.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn cancelled_within(worker: &HoldingWorker, wait: Duration) -> Vec<String> {
        let until = tokio::time::Instant::now() + wait;
        while worker.cancelled.lock().unwrap().is_empty() && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.cancelled.lock().unwrap().clone()
    }

    fn held_round(
        placement: RemotePlacement,
        draining: fn() -> bool,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<ExecSummary>> {
        tokio::spawn(async move {
            let target = DispatchTarget {
                draining,
                ..DispatchTarget::remote(&placement, None, None, None)
            };
            let cmd = Command::new("claude");
            let request = target.request(&cmd, "task".into(), Duration::from_secs(600));
            target
                .run(request, tokio::sync::mpsc::channel(64).0, cancel)
                .await
        })
    }

    #[test]
    fn a_keyed_round_names_each_attempt_and_spends_its_budget_from_its_start() {
        let started_at = chrono::Utc::now() - chrono::Duration::minutes(5);
        let jobs = RoundJobs::new("r1".into(), started_at);
        assert_eq!(jobs.for_engine(2).for_attempt(3).job().as_str(), "r1.2.3");
        let budget = CallBudget::new(Duration::from_secs(600), jobs.spent());
        let left = budget.remaining();
        assert!(
            left <= Duration::from_secs(300) && left > Duration::from_secs(290),
            "{left:?}"
        );
    }

    #[tokio::test]
    async fn a_keyed_round_runs_as_the_job_it_was_named() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = holding_worker(tmp.path()).await;
        let round = tokio::spawn(async move {
            let target = DispatchTarget::remote(&placement, None, None, Some(JobId::new("r1.0.1")));
            let request = target.request(
                &Command::new("claude"),
                "task".into(),
                Duration::from_secs(600),
            );
            target
                .run(
                    request,
                    tokio::sync::mpsc::channel(64).0,
                    CancellationToken::new(),
                )
                .await
        });
        until_started(&worker).await;
        round.abort();
        assert_eq!(worker.started.lock().unwrap().clone(), vec!["r1.0.1"]);
    }

    /// A worker that ends each job it is sent at once, as failed, keeping the
    /// ids it was asked to start.
    #[derive(Default)]
    struct FailingWorker(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl workforce_agent::tools::exec_runner::JobHandler for FailingWorker {
        async fn run(
            &self,
            request: workforce_agent::tools::exec_runner::Request,
            _: workforce_agent::tools::exec_runner::JobLink,
        ) -> Result<workforce_agent::tools::exec_runner::ServerFrame> {
            let workforce_agent::tools::exec_runner::Request::Run(run) = request else {
                return Ok(workforce_agent::tools::exec_runner::ServerFrame::Done(
                    ExecSummary::exited(0),
                ));
            };
            self.0.lock().unwrap().push(run.job.to_string());
            Ok(workforce_agent::tools::exec_runner::ServerFrame::Done(
                ExecSummary::exited(1),
            ))
        }
    }

    #[derive(Default)]
    struct Marks {
        marks: std::sync::Mutex<Vec<String>>,
        refuse: bool,
    }

    #[async_trait::async_trait]
    impl workforce_agent::turn_journal::TurnJournal for Marks {
        async fn pending(
            &self,
            _: &str,
            _: &str,
            _: &serde_json::Value,
            _: serde_json::Value,
        ) -> Result<()> {
            Ok(())
        }
        async fn checkpoint(&self, _: &str, mark: &str) -> Result<()> {
            if self.refuse {
                eyre::bail!("the journal is unreachable");
            }
            self.marks.lock().unwrap().push(mark.to_string());
            Ok(())
        }
        async fn finished(&self, _: &str, _: &workforce_agent::runner::ToolInvocation) {}
    }

    async fn failing_worker(dir: &std::path::Path) -> (Arc<FailingWorker>, RemotePlacement) {
        use std::os::unix::fs::MetadataExt;
        let socket = dir.join("failing.sock");
        let worker = Arc::new(FailingWorker::default());
        let listener = workforce_agent::tools::exec_runner::Listener::Unix {
            path: socket.clone(),
            accept_uid: std::fs::metadata(dir).unwrap().uid(),
        };
        let handler: Arc<dyn workforce_agent::tools::exec_runner::JobHandler> = worker.clone();
        tokio::spawn(workforce_agent::tools::exec_runner::serve(
            listener, handler,
        ));
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let placement = RemotePlacement {
            endpoint: RunnerEndpoint::Unix(socket),
            scope: ScopeId::new("scope-1"),
            jobs: true,
        };
        (worker, placement)
    }

    fn marks_call(marks: &Arc<Marks>) -> workforce_agent::turn_journal::ToolCall {
        let journal: Arc<dyn workforce_agent::turn_journal::TurnJournal> = marks.clone();
        workforce_agent::turn_journal::ToolCall::new(Some(journal), serde_json::json!({}))
    }

    async fn keyed_run(placement: RemotePlacement) -> Result<ExecSummary> {
        let target = DispatchTarget::remote(&placement, None, None, Some(JobId::new("r1.0.0")));
        let request = target.request(
            &Command::new("claude"),
            "task".into(),
            Duration::from_secs(60),
        );
        target
            .run(
                request,
                tokio::sync::mpsc::channel(64).0,
                CancellationToken::new(),
            )
            .await
    }

    #[tokio::test]
    async fn a_keyed_job_is_recorded_before_it_starts_and_not_started_unrecorded() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = failing_worker(tmp.path()).await;
        let marks = Arc::new(Marks::default());
        marks_call(&marks)
            .scope(keyed_run(placement.clone()))
            .await
            .unwrap();
        assert_eq!(marks.marks.lock().unwrap().clone(), vec!["r1.0.0"]);
        assert_eq!(worker.0.lock().unwrap().clone(), vec!["r1.0.0"]);

        let refusing = Arc::new(Marks {
            refuse: true,
            ..Default::default()
        });
        let err = marks_call(&refusing)
            .scope(keyed_run(placement))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unreachable"), "{err:#}");
        assert_eq!(
            worker.0.lock().unwrap().len(),
            1,
            "an unrecorded job never starts"
        );
    }

    #[tokio::test]
    async fn a_collected_round_runs_its_one_recorded_job_and_retries_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = failing_worker(tmp.path()).await;
        let config = ClaudeCodeConfig {
            remote: Some(placement),
            auth: ClaudeAuth::Bedrock(BedrockSession {
                region: "eu-west-2".into(),
                credentials: BedrockCredentials {
                    access_key_id: "ASIA".into(),
                    secret_access_key: "secret".into(),
                    session_token: None,
                },
                pins: ModelPins {
                    opus: "o".into(),
                    sonnet: "s".into(),
                    haiku: "h".into(),
                },
            }),
            jobs: RoundJobs::resuming("r1", chrono::Utc::now(), "r1.0.1"),
            contained: None,
            timeout: Duration::from_secs(3600),
            ..retry_fixture(None, false, false)
        };
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            run_with_retry(&config, None, None, None, None),
        )
        .await
        .expect("no retry wait");
        assert_eq!(result.unwrap().completion, Completion::Failed);
        assert_eq!(worker.0.lock().unwrap().clone(), vec!["r1.0.1"]);
    }

    #[test]
    fn a_recorded_job_names_where_its_round_resumes() {
        let at = chrono::Utc::now();
        let jobs = RoundJobs::resuming("r1-cold", at, "r1-cold.1.2").unwrap();
        assert!(jobs.pinned());
        assert_eq!(jobs.engine(), 1);
        assert_eq!(jobs.for_attempt(5).job().as_str(), "r1-cold.1.2");
        assert!(RoundJobs::resuming("r1", at, "r1-cold.1.2").is_none());
        assert!(RoundJobs::resuming("r1", at, "r1.x.2").is_none());
    }

    #[tokio::test]
    async fn a_round_dropped_before_its_summary_ends_its_job() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = holding_worker(tmp.path()).await;
        let round = held_round(placement, || false, CancellationToken::new());
        until_started(&worker).await;
        round.abort();
        let cancelled = cancelled_within(&worker, Duration::from_secs(10)).await;
        assert_eq!(cancelled, worker.started.lock().unwrap().clone());
    }

    #[tokio::test]
    async fn a_cancelled_round_ends_its_job() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = holding_worker(tmp.path()).await;
        let cancel = CancellationToken::new();
        let round = held_round(placement, || false, cancel.clone());
        until_started(&worker).await;
        cancel.cancel();
        let err = round.await.unwrap().unwrap_err();
        assert!(is_cancelled(&err), "{err}");
        let cancelled = cancelled_within(&worker, Duration::from_secs(10)).await;
        assert_eq!(cancelled, worker.started.lock().unwrap().clone());
    }

    #[tokio::test]
    async fn a_draining_hub_leaves_its_jobs_running_for_a_successor() {
        let tmp = tempfile::tempdir().unwrap();
        let (worker, placement) = holding_worker(tmp.path()).await;
        let round = held_round(placement, || true, CancellationToken::new());
        until_started(&worker).await;
        round.abort();
        let cancelled = cancelled_within(&worker, Duration::from_millis(500)).await;
        assert!(cancelled.is_empty(), "{cancelled:?}");
    }

    #[tokio::test]
    async fn the_remote_responder_answers_from_the_endpoint() {
        let served = McpEndpoint::new(
            crate::mcp::Surface::Fixed(Arc::new(
                workforce_agent::tools::registry::ToolRegistry::new(),
            )),
            "w".into(),
            None,
            Arc::new(crate::mcp::DirectExecutor),
        );
        let placement = RemotePlacement {
            endpoint: RunnerEndpoint::Unix(PathBuf::from("/run/exec.sock")),
            scope: ScopeId::new("scope-1"),
            jobs: false,
        };
        let responder = DispatchTarget::remote(&placement, Some(&served), None, None)
            .responder()
            .unwrap();
        let listed = responder(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into()).await;
        assert!(listed.contains("\"tools\""), "{listed}");
        let notified =
            responder(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into()).await;
        assert!(
            notified.is_empty(),
            "a notification is answered with an empty body"
        );
    }

    /// A probe served for a round bound in the turn keyed `key`, as the turn
    /// that collects a round binds it.
    async fn bound_in_turn(
        key: &str,
    ) -> (
        Arc<crate::mcp_turn::testing::Probe>,
        crate::mcp_turn::RoundTools,
    ) {
        let service = crate::mcp_turn::testing::service().await;
        let probe = crate::mcp_turn::testing::Probe::named("probe.whoami");
        service.tools.register(probe.clone());
        let tools = workforce_agent::runner::SessionContext {
            key: Some(key.to_string()),
            ..Default::default()
        }
        .scope(async {
            crate::mcp_turn::RoundTools::bind(
                &service,
                "alice",
                crate::mcp_turn::McpExposure::Patterns(vec!["probe.*".to_string()].into()),
                None,
                crate::mcp_turn::ClientTimeout::CLAUDE_CODE,
            )
        })
        .await
        .unwrap();
        (probe, tools)
    }

    const PROBE_CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"probe_whoami","arguments":{}}}"#;

    fn placement(jobs: bool) -> RemotePlacement {
        RemotePlacement {
            endpoint: RunnerEndpoint::Unix(PathBuf::from("/run/exec.sock")),
            scope: ScopeId::new("scope-1"),
            jobs,
        }
    }

    #[tokio::test]
    async fn collected_round_serves_mcp_from_collecting_turn() {
        let (_probe, tools) = bound_in_turn("slack:T1:C1:collector").await;
        let placement = placement(true);
        let responder = DispatchTarget::remote(
            &placement,
            Some(tools.endpoint()),
            None,
            Some(JobId::new("r1.0.0")),
        )
        .responder()
        .unwrap();
        // The worker's calls arrive on a task of their own, with nothing of
        // the collecting turn in scope.
        let reply = tokio::spawn(responder(PROBE_CALL.into())).await.unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        let seen: serde_json::Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(seen["session_key"], "slack:T1:C1:collector");
        assert_eq!(seen["identity"], "alice");
    }

    #[tokio::test]
    async fn durable_job_mcp_call_refused_while_draining() {
        let (probe, tools) = bound_in_turn("slack:T1:C1:1").await;
        let durable = placement(true);
        let responder = DispatchTarget {
            draining: || true,
            ..DispatchTarget::remote(&durable, Some(tools.endpoint()), None, None)
        }
        .responder()
        .unwrap();
        let refused: serde_json::Value =
            serde_json::from_str(&responder(PROBE_CALL.into()).await).unwrap();
        assert_eq!(refused["id"], 7);
        assert_eq!(refused["error"]["message"], DRAINING_REFUSAL);
        assert_eq!(probe.runs(), 0, "nothing ran whose reply could be lost");
        let listed = responder(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into()).await;
        assert!(
            listed.contains("probe_whoami"),
            "the rest still answers: {listed}"
        );

        let plain = placement(false);
        let responder = DispatchTarget {
            draining: || true,
            ..DispatchTarget::remote(&plain, Some(tools.endpoint()), None, None)
        }
        .responder()
        .unwrap();
        let ran: serde_json::Value =
            serde_json::from_str(&responder(PROBE_CALL.into()).await).unwrap();
        assert_eq!(ran["result"]["isError"], false, "{ran}");
        assert_eq!(
            probe.runs(),
            1,
            "a round that is not a job finishes through the drain"
        );
    }

    /// The whole-path version of the case above: a round that finished and then
    /// hung must come out of `assemble_result` looking clean to the callers that
    /// still gate on `exit_code`, not killed.
    #[test]
    fn assemble_result_treats_a_shutdown_hang_after_success_as_success() {
        let r = assemble_result(
            CapturedRun {
                exit_code: None,
                ended: Ended::WallClock,
                capture: capture_of(&[result_event("success", 4)]),
                stderr: Vec::new(),
            },
            &retry_fixture(None, false, false),
            Duration::from_secs(1),
        );

        assert_eq!(r.completion, Completion::Success);
        assert_eq!(r.exit_code, 0, "callers gating on exit_code must see clean");
        assert_eq!(r.crash_kind, None);
        assert!(!r.completion.is_truncated(), "must not ask for a follow-up");
        assert!(classify_failure(&Ok(r)).is_none());
    }

    #[test]
    fn assemble_result_carries_the_streamed_limit_and_the_job() {
        let jobs = RoundJobs::new("r1".into(), chrono::Utc::now()).for_engine(2);
        let config = ClaudeCodeConfig {
            jobs: Some(jobs),
            contained: None,
            ..retry_fixture(None, false, false)
        };
        let rejected = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "rejected", "resetsAt": 1_790_956_800, "rateLimitType": "seven_day"},
        });
        let limited = |events: &[serde_json::Value], exit_code| {
            assemble_result(
                CapturedRun {
                    exit_code: Some(exit_code),
                    ended: Ended::Exited,
                    capture: capture_of(events),
                    stderr: Vec::new(),
                },
                &config,
                Duration::from_secs(1),
            )
        };

        let r = limited(&[rejected.clone(), result_event("success", 1)], 0);
        assert_eq!(
            r.job,
            Some(JobId::new("r1.2.0")),
            "the job keys its cost row"
        );
        let limit = r.limit.expect("the stream's refusal");
        assert_eq!(limit.status, crate::subscription::LimitStatus::Rejected);
        assert_eq!(limit.window.as_deref(), Some("seven_day"));

        let noticed = limited(
            &[serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "result": "You've hit your session limit · resets 7pm (Europe/London)",
            })],
            1,
        );
        assert_eq!(
            noticed.limit.map(|l| l.status),
            Some(crate::subscription::LimitStatus::Rejected),
            "a failed round's notice is the backstop"
        );
        assert_eq!(limited(&[result_event("success", 1)], 0).limit, None);
    }

    #[test]
    fn a_round_narrating_a_limit_never_reports_one() {
        let config = ClaudeCodeConfig {
            max_turns: Some(30),
            ..retry_fixture(None, false, false)
        };
        let round = |events: &[serde_json::Value], stderr: &str| {
            assemble_result(
                CapturedRun {
                    exit_code: Some(1),
                    ended: Ended::Exited,
                    capture: capture_of(events),
                    stderr: stderr.as_bytes().to_vec(),
                },
                &config,
                Duration::from_secs(1),
            )
        };
        let narrated = round(
            &[
                assistant("You've hit your session limit · resets 7pm"),
                assistant("You've reached your plan limit, so the banner shows"),
                serde_json::json!({
                    "type": "result",
                    "subtype": "error_max_turns",
                    "is_error": true,
                    "num_turns": 31,
                }),
            ],
            "",
        );
        assert_eq!(narrated.completion, Completion::TurnCap);
        assert_eq!(narrated.limit, None, "a round's own words cool nothing");

        let printed = round(
            &[assistant("working")],
            "You've hit your session limit · resets 7pm (Europe/London)",
        );
        assert_eq!(
            printed.limit.map(|l| l.status),
            Some(crate::subscription::LimitStatus::Rejected),
            "the CLI's own stderr notice is read"
        );
    }

    /// A wall-clock timeout must never be reported as an OOM kill. We send the
    /// SIGKILL ourselves, so its 128+9 is self-inflicted — publishing it would
    /// make every caller that reads `exit_code` say "likely OOM" for a plain
    /// timeout, and 137 is exactly what `KilledLikelyOom` identifies.
    #[test]
    fn assemble_result_reports_a_timeout_as_a_timeout_not_an_oom_kill() {
        assert_eq!(
            CrashKind::from_exit_code(137),
            Some(CrashKind::KilledLikelyOom),
            "137 is the OOM signature a timeout must not carry"
        );

        let config = retry_fixture(None, false, false);
        let r = assemble_result(
            CapturedRun {
                exit_code: None,
                ended: Ended::WallClock,
                capture: capture_of(&[assistant("pushed branch fix/x")]),
                stderr: b"some diagnostic".to_vec(),
            },
            &config,
            Duration::from_secs(1),
        );

        assert_eq!(r.completion, Completion::Timeout);
        assert_ne!(r.exit_code, 137);
        assert_eq!(
            r.crash_kind,
            Some(CrashKind::TimedOut {
                after_secs: config.timeout.as_secs()
            })
        );
        assert_eq!(r.text, "pushed branch fix/x", "the work must survive");
        assert!(
            r.stderr_tail.contains("some diagnostic"),
            "a hang's only evidence is its stderr"
        );
    }

    /// Pinned to a real `claude` 2.1.224 turn-cap event, captured with
    /// `--max-turns 1` on a two-step prompt: `subtype: "error_max_turns"`,
    /// `is_error: true`, `num_turns` one PAST the cap, and **no `result` field
    /// at all**. The absent `result` is the load-bearing detail — it is why the
    /// accumulated narration is the only thing left to hand back, and why the
    /// cap must be read from `subtype`/`num_turns` rather than from any text.
    #[test]
    fn a_real_turn_cap_event_salvages_its_narration() {
        let capture = capture_of(&[
            assistant("I'll run both commands."),
            serde_json::json!({
                "type": "result",
                "subtype": "error_max_turns",
                "is_error": true,
                "num_turns": 2,
                "total_cost_usd": 0.03,
            }),
        ]);

        assert_eq!(capture.num_turns(), 2, "the CLI reports one past the cap");
        assert_eq!(capture.text(), "I'll run both commands.");
        assert_eq!(
            completion_of(1, false, false, &capture, Some(1)),
            Completion::TurnCap
        );
    }

    #[test]
    fn assemble_result_suppresses_the_crash_card_on_a_turn_cap() {
        let config = retry_fixture(None, false, false);
        let r = assemble_result(
            CapturedRun {
                exit_code: None,
                ended: Ended::Exited,
                capture: capture_of(&[result_event("error_max_turns", 31)]),
                stderr: Vec::new(),
            },
            &ClaudeCodeConfig {
                max_turns: Some(30),
                ..config
            },
            Duration::from_secs(1),
        );

        assert_eq!(r.completion, Completion::TurnCap);
        assert!(r.completion.is_usable());
        assert_eq!(r.crash_kind, None, "a turn cap is not a crash");
        assert_eq!(r.num_turns, 31);
    }

    #[test]
    fn completion_policy_treats_budget_exhaustion_as_usable_never_retryable() {
        for c in [
            Completion::Success,
            Completion::TurnCap,
            Completion::Timeout,
        ] {
            assert!(c.is_usable(), "{c:?} should be usable");
            assert!(!c.is_retryable(), "{c:?} must never be retried");
        }
        for c in [Completion::Failed, Completion::SessionNotFound] {
            assert!(!c.is_usable(), "{c:?} should not be usable");
        }
        assert!(Completion::Failed.is_retryable());
        assert!(!Completion::SessionNotFound.is_retryable());
        assert!(Completion::TurnCap.is_truncated());
        assert!(Completion::Timeout.is_truncated());
        assert!(!Completion::Success.is_truncated());
    }

    #[test]
    fn the_clis_401_refuses_the_login_of_a_failed_round_only() {
        let settled = |completion| {
            let login = crate::subscription::test_support::claude_login(&format!(
                "sk-ant-oat01-{}",
                uuid::Uuid::new_v4().simple()
            ));
            let round = ClaudeCodeResult {
                completion,
                notice: "API Error: 401 authentication_error".into(),
                ..cc_result(1, "working on it", "")
            };
            login.settle(&round.round_end(), "scout")
        };

        assert!(matches!(
            settled(Completion::Failed),
            Some(crate::subscription::SubscriptionError::LoginInvalid { .. })
        ));
        for spared in [Completion::Timeout, Completion::TurnCap] {
            assert!(settled(spared).is_none(), "{spared:?}");
        }
    }

    #[test]
    fn classify_failure_passes_budget_exhaustion_through_as_usable() {
        let mut capped = cc_result(1, "did the work\nPR_URL: https://x/pull/9", "");
        capped.completion = Completion::TurnCap;
        assert!(classify_failure(&Ok(capped)).is_none());

        let mut timed_out = cc_result(-1, "pushed the branch", "");
        timed_out.completion = Completion::Timeout;
        assert!(classify_failure(&Ok(timed_out)).is_none());
    }

    #[test]
    fn stream_capture_accumulates_text_and_prefers_the_cli_turn_count() {
        let capture = capture_of(&[
            assistant("first"),
            assistant("second"),
            result_event("success", 17),
        ]);
        // The CLI's own count wins over the two assistant events we saw — it is
        // the number `--max-turns` is compared against.
        assert_eq!(capture.num_turns(), 17);
        assert_eq!(capture.text(), "final answer");

        // Without a result event there is no CLI count and no final answer, so
        // the accumulated narration is all the caller gets.
        let partial = capture_of(&[assistant("first"), assistant("second")]);
        assert_eq!(partial.num_turns(), 2);
        assert_eq!(partial.text(), "first\nsecond");
    }

    #[test]
    fn stream_capture_keeps_the_end_of_oversized_narration() {
        let capture = StreamCapture {
            assistant_text: format!(
                "{}\nPR_URL: https://x/pull/9",
                "x".repeat(SALVAGE_TAIL_BYTES * 2)
            ),
            ..Default::default()
        };
        let text = capture.text();
        assert!(text.len() < SALVAGE_TAIL_BYTES + 64);
        assert!(
            text.ends_with("PR_URL: https://x/pull/9"),
            "the tail is where the PR url lives"
        );
    }

    #[test]
    fn stream_capture_keeps_a_bounded_tail_that_salvages_identically() {
        let texts: Vec<String> = (0..400)
            .map(|i| format!("{i:04}{}", "€".repeat(340)))
            .collect();
        let mut capture = StreamCapture::default();
        let longest = texts
            .iter()
            .map(|text| {
                capture.record(&assistant(text));
                capture.assistant_text.len()
            })
            .max()
            .unwrap_or(0);
        let full = texts.join("\n");
        assert!(longest <= 4 * SALVAGE_TAIL_BYTES);
        assert!(capture.assistant_text.len() < full.len());
        assert_eq!(capture.assistant_turns, 400);
        assert_eq!(capture.text(), tail_utf8(&full, SALVAGE_TAIL_BYTES));
    }

    fn user_line() -> String {
        serde_json::json!({ "type": "user", "message": { "content": "x".repeat(1_000) } })
            .to_string()
    }

    /// A shell script printing `before`, then `copies` of `filler`, then
    /// `after`, a line each.
    pub(crate) fn stream_script(
        before: &[&str],
        filler: &str,
        copies: usize,
        after: &[&str],
    ) -> String {
        let print = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| format!("printf '%s\\n' '{line}'; "))
                .collect::<String>()
        };
        format!(
            "{}yes '{filler}' | head -n {copies}; {}",
            print(before),
            print(after)
        )
    }

    #[tokio::test]
    async fn spawn_and_capture_reads_past_a_megabyte_to_the_result_event() {
        let result = result_event("success", 4).to_string();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &stream_script(&[], &user_line(), 3_000, &[&result])]);

        let started = Instant::now();
        let run = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(30),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .expect("a large stream is not an error");

        assert_eq!(run.ended, Ended::Exited);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.capture.num_turns(), 4);
        assert_eq!(run.capture.text(), "final answer");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the stream is read as it comes, not stalled to the wall clock"
        );
    }

    #[tokio::test]
    async fn spawn_and_capture_reads_past_invalid_utf8_to_an_unterminated_result() {
        let result = result_event("success", 2).to_string();
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("printf 'not \\377 utf-8\\r\\n'; printf '%s' '{result}'"),
        ]);

        let run = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(30),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .expect("a stray byte is not an error");

        assert_eq!(run.capture.num_turns(), 2);
        assert_eq!(run.capture.text(), "final answer");
    }

    #[test]
    fn call_budget_stops_retrying_once_too_little_is_left() {
        let ample = CallBudget {
            total: Duration::from_secs(1200),
            started: Instant::now(),
        };
        let next = ample
            .next_attempt(Duration::from_secs(60))
            .expect("ample budget allows a retry");
        assert!(next <= Duration::from_secs(1140) && next > Duration::from_secs(1100));

        // A backoff that would leave less than one useful attempt stops the ladder.
        let tight = CallBudget {
            total: Duration::from_secs(200),
            started: Instant::now(),
        };
        assert!(tight.next_attempt(Duration::from_secs(180)).is_none());
        assert!(tight.next_attempt(Duration::from_secs(60)).is_some());

        let spent = CallBudget {
            total: Duration::from_secs(1),
            started: Instant::now() - Duration::from_secs(10),
        };
        assert_eq!(spent.remaining(), Duration::ZERO);
        assert!(spent.next_attempt(Duration::ZERO).is_none());
    }

    /// The regression test for the defect that discarded a round's work: a
    /// subprocess that streams real output and then overruns must still hand
    /// that output back, and must be killed rather than waited out.
    #[tokio::test]
    async fn spawn_and_capture_salvages_output_from_a_timed_out_subprocess() {
        let event = serde_json::to_string(&assistant("pushed branch fix/x")).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo '{event}'; sleep 30"));

        let started = Instant::now();
        let run = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(2),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .expect("timeout is not an error");

        assert_eq!(run.ended, Ended::WallClock);
        assert_eq!(run.capture.assistant_text, "pushed branch fix/x");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the child must be killed at the deadline, not waited out"
        );

        let completion = completion_of(
            run.exit_code.unwrap_or(-1),
            run.ended.timed_out(),
            false,
            &run.capture,
            Some(30),
        );
        assert_eq!(completion, Completion::Timeout);
        assert!(completion.is_usable());
    }

    /// A cancelled round must classify as cancelled. Only a genuine spawn
    /// failure may carry the "rebuild the image" wording, or `cmd: stop` and
    /// the Slack hard-stop tell the user to rebuild the hub image.
    #[tokio::test]
    async fn spawn_and_capture_reports_cancellation_distinctly_from_a_missing_cli() {
        let token = CancellationToken::new();
        token.cancel();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30");

        let started = Instant::now();
        let err = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(30),
            Some(token),
            None,
            StreamCapture::default(),
        )
        .await
        .err()
        .expect("cancellation is an error");

        assert_eq!(err.to_string(), "sh cancelled");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(
            classify_failure(&Err(err)),
            Some(CcUnavailable::Errored { .. })
        ));
    }

    /// The defect behind the 20-minute rounds: a `claude` that has replied but
    /// lingers on a background child must not hold the round open. The round
    /// ends at the grace, and it ended well — no timeout, no crash.
    #[tokio::test]
    async fn spawn_and_capture_ends_the_round_at_the_result_when_the_leader_lingers() {
        let event = serde_json::to_string(&result_event("success", 7)).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo '{event}'; sleep 30"));

        let started = Instant::now();
        let run = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(60),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .expect("a lingering leader is not an error");

        assert_eq!(run.ended, Ended::AfterResult);
        assert_eq!(run.capture.num_turns(), 7);
        assert!(
            started.elapsed() < RESULT_EXIT_GRACE + Duration::from_secs(10),
            "the round must end at the grace, not the wall clock"
        );
        let completion = completion_of(
            run.exit_code.unwrap_or(-1),
            run.ended.timed_out(),
            false,
            &run.capture,
            Some(30),
        );
        assert_eq!(completion, Completion::Success);
    }

    #[tokio::test]
    async fn settle_dispatch_returns_the_summary_when_the_leader_exits_in_time() {
        let summary = ExecSummary {
            exit_code: Some(0),
            attempts: 1,
            truncated: false,
            timed_out: false,
        };
        let token = CancellationToken::new();
        token.cancel();
        let (settled, ended) = settle_dispatch(
            std::future::ready(Ok(summary.clone())),
            &token,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(settled, summary);
        assert_eq!(ended, Ended::Exited);
    }

    #[tokio::test(start_paused = true)]
    async fn settle_dispatch_cuts_a_lingering_leader_at_the_grace_without_a_timeout() {
        let token = CancellationToken::new();
        token.cancel();
        let end = CancellationToken::new();
        let (settled, ended) = settle_dispatch(std::future::pending(), &token, &end)
            .await
            .unwrap();
        assert_eq!(settled.exit_code, None);
        assert!(!settled.timed_out);
        assert_eq!(ended, Ended::AfterResult);
        assert!(end.is_cancelled(), "the lingering round is asked to end");
    }

    #[test]
    fn completion_of_lets_the_result_outrank_how_the_process_ended() {
        let finished = capture_of(&[result_event("success", 3)]);
        assert_eq!(
            completion_of(-1, false, false, &finished, Some(30)),
            Completion::Success,
            "a leader cut after its result is a finished round"
        );
        assert_eq!(
            completion_of(1, true, false, &finished, Some(30)),
            Completion::Success,
            "the result outranks a later timeout"
        );
        let capped = capture_of(&[result_event("error_max_turns", 30)]);
        assert_eq!(
            completion_of(-1, true, false, &capped, Some(30)),
            Completion::TurnCap
        );
        let errored = capture_of(&[result_event("error_during_execution", 2)]);
        assert_eq!(
            completion_of(0, false, false, &errored, Some(30)),
            Completion::Failed
        );
        assert_eq!(
            completion_of(1, false, true, &errored, Some(30)),
            Completion::SessionNotFound
        );
    }

    /// The CLI reports an API failure as a `success` result flagged
    /// `is_error`; that must keep reading as a failure so the rate-limit
    /// retry and its card still fire.
    #[test]
    fn completion_of_keeps_an_errored_success_result_a_failure() {
        let api_error = capture_of(&[serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "num_turns": 1,
            "result": "API Error: 429 rate limit exceeded",
        })]);
        assert!(api_error.is_error);
        assert_eq!(
            completion_of(1, false, false, &api_error, Some(30)),
            Completion::Failed
        );
        assert!(completion_of(1, false, false, &api_error, Some(30)).is_retryable());
        assert_eq!(
            completion_of(-1, true, false, &api_error, Some(30)),
            Completion::Timeout
        );
        assert!(!capture_of(&[result_event("success", 1)]).is_error);
    }

    /// A round cut after an errored result was killed by us, so its 137 must
    /// not become an OOM card, and the daemon's `None` must not become
    /// "exit -1".
    #[test]
    fn assemble_result_never_blames_the_cli_for_a_post_result_kill() {
        let config = retry_fixture(None, false, false);
        for exit_code in [Some(137), None] {
            let r = assemble_result(
                CapturedRun {
                    exit_code,
                    ended: Ended::AfterResult,
                    capture: capture_of(&[result_event("error_during_execution", 3)]),
                    stderr: b"boom".to_vec(),
                },
                &config,
                Duration::from_secs(1),
            );
            assert_eq!(r.completion, Completion::Failed);
            assert_eq!(r.exit_code, -1);
            assert_eq!(r.crash_kind, None);
            assert_eq!(r.stderr_tail, "boom");
        }
    }

    #[test]
    fn toolchain_cache_env_points_every_toolchain_under_the_cache_root() {
        let env = toolchain_cache_env(Path::new("/fast/cache"));
        let get = |var: &str| env.iter().find(|(k, _)| k == var).map(|(_, v)| v.as_str());
        assert_eq!(get("XDG_DATA_HOME"), Some("/fast/cache/share"));
        assert_eq!(get("CARGO_HOME"), Some("/fast/cache/cargo"));
        assert_eq!(get("GRADLE_USER_HOME"), Some("/fast/cache/gradle"));
        assert_eq!(get("BUNDLE_PATH"), Some("/fast/cache/bundle"));
        assert!(env
            .iter()
            .all(|(k, _)| k.chars().all(|c| c.is_ascii_uppercase() || c == '_')));
        assert_eq!(env.len(), 8);
    }

    #[tokio::test]
    async fn spawn_and_capture_returns_a_clean_exit_with_its_result_event() {
        let event = serde_json::to_string(&result_event("success", 5)).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo '{event}'"));

        let run = spawn_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(10),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .expect("clean exit");

        assert_eq!(run.ended, Ended::Exited);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.capture.num_turns(), 5);
        assert_eq!(run.capture.text(), "final answer");
    }

    #[test]
    fn classify_failure_maps_rate_limit_text() {
        let r = Ok(cc_result(1, "API Error: 429 rate limit exceeded", ""));
        assert_eq!(classify_failure(&r), Some(CcUnavailable::RateLimited));
    }

    #[test]
    fn classify_failure_maps_spawn_failure_to_cli_missing() {
        let err = Err(eyre!(
            "Failed to spawn claude CLI (is @anthropic-ai/claude-code installed?): No such file"
        ));
        assert!(matches!(
            classify_failure(&err),
            Some(CcUnavailable::CliMissing { .. })
        ));
    }

    #[test]
    fn classify_failure_maps_auth_failure_from_stderr() {
        let r = Ok(cc_result(1, "", "401 unauthorized: invalid token"));
        assert_eq!(classify_failure(&r), Some(CcUnavailable::AuthFailed));
    }

    #[test]
    fn classify_failure_falls_back_to_crashed_for_unknown_nonzero() {
        let r = Ok(cc_result(1, "something went wrong", "boom"));
        match classify_failure(&r) {
            Some(CcUnavailable::Crashed { stderr_tail, .. }) => assert_eq!(stderr_tail, "boom"),
            other => panic!("expected Crashed, got {other:?}"),
        }
    }

    #[test]
    fn next_retry_delay_uses_parsed_retry_after_when_present() {
        let text = "API Error: rate limit exceeded. Retry-after: 45 seconds.";
        assert_eq!(next_retry_delay(true, 1, text), Some(45));
    }

    #[test]
    fn next_retry_delay_uses_rate_limit_schedule_when_no_retry_after() {
        // Rate-limited: two retries on the schedule, then give up.
        assert_eq!(
            next_retry_delay(true, 1, "API Error: rate limit"),
            Some(180)
        );
        assert_eq!(
            next_retry_delay(true, 2, "API Error: rate limit"),
            Some(240)
        );
        assert_eq!(next_retry_delay(true, 3, "API Error: rate limit"), None);
    }

    #[test]
    fn next_retry_delay_falls_back_to_single_60s_for_other_failures() {
        assert_eq!(next_retry_delay(false, 1, "some other error"), Some(60));
        assert_eq!(next_retry_delay(false, 2, "some other error"), None);
    }

    async fn daemon_endpoint(dir: &std::path::Path) -> RunnerEndpoint {
        use workforce_agent::tools::exec_runner::{serve, Listener, LocalJobHandler};
        let socket = dir.join("exec.sock");
        let listener = Listener::Unix {
            path: socket.clone(),
            accept_uid: {
                let (a, _b) = tokio::net::UnixStream::pair().unwrap();
                a.peer_cred().unwrap().uid()
            },
        };
        tokio::spawn(async move {
            let _ = serve(listener, Arc::new(LocalJobHandler)).await;
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        RunnerEndpoint::Unix(socket)
    }

    /// `script`, run by `sh` on a hub-local runner daemon, its stream read
    /// with `grammar`.
    pub(crate) async fn dispatched<G: StreamGrammar>(script: &str, grammar: G) -> CapturedRun<G> {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = daemon_endpoint(dir.path()).await;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        dispatch_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(30),
            None,
            None,
            &DispatchTarget::local(&endpoint),
            grammar,
        )
        .await
        .expect("the dispatch completes")
    }

    /// The production failure: past a megabyte the runner's output consumer
    /// was dropped, which ended the connection and so the round.
    #[tokio::test]
    async fn a_dispatched_turn_reads_past_a_megabyte_to_the_result_event() {
        let result = result_event("success", 4).to_string();
        let run = dispatched(
            &stream_script(&[], &user_line(), 3_000, &[&result]),
            StreamCapture::default(),
        )
        .await;

        assert_eq!(run.ended, Ended::Exited);
        assert_eq!(run.exit_code, Some(0));
        assert!(run.capture.result.is_some());
        assert_eq!(run.capture.num_turns(), 4);
        assert_eq!(run.capture.text(), "final answer");
    }

    /// The dispatched half of the 20-minute-round defect: a `claude` behind
    /// the runner that has replied but lingers ends at the grace, and its
    /// result stands.
    #[tokio::test]
    async fn a_dispatched_turn_ends_at_the_result_when_the_leader_lingers() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = daemon_endpoint(dir.path()).await;
        let event = serde_json::to_string(&result_event("success", 6)).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo '{event}'; sleep 30"));

        let started = Instant::now();
        let run = dispatch_and_capture(
            cmd,
            String::new(),
            Duration::from_secs(60),
            None,
            None,
            &DispatchTarget::local(&endpoint),
            StreamCapture::default(),
        )
        .await
        .unwrap();

        assert_eq!(run.ended, Ended::AfterResult);
        assert_eq!(run.exit_code, None);
        assert_eq!(run.capture.num_turns(), 6);
        assert!(started.elapsed() < RESULT_EXIT_GRACE + Duration::from_secs(10));
        assert_eq!(
            completion_of(-1, false, false, &run.capture, Some(30)),
            Completion::Success
        );
    }

    /// The equivalence that makes the move safe: a turn dispatched to the
    /// runner must capture exactly what the same turn captured in-process.
    /// Runs a stand-in for `claude` that emits the same `stream-json` shape,
    /// deliberately ending WITHOUT a trailing newline so the reassembler's tail
    /// handling is exercised on the line that carries the result.
    #[tokio::test]
    async fn a_dispatched_turn_captures_what_an_in_process_turn_captures() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = daemon_endpoint(dir.path()).await;

        // Two events, the second unterminated — a real CLI can exit that way.
        let script = concat!(
            r#"printf '{"type":"system","subtype":"init"}
'; "#,
            r#"printf '{"type":"result","subtype":"success","result":"done"}'"#
        );
        let build = || {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            cmd
        };

        let local = spawn_and_capture(
            build(),
            String::new(),
            Duration::from_secs(30),
            None,
            None,
            StreamCapture::default(),
        )
        .await
        .unwrap();
        let dispatched = dispatch_and_capture(
            build(),
            String::new(),
            Duration::from_secs(30),
            None,
            None,
            &DispatchTarget::local(&endpoint),
            StreamCapture::default(),
        )
        .await
        .unwrap();

        assert_eq!(local.exit_code, Some(0));
        assert_eq!(dispatched.exit_code, local.exit_code);
        assert_eq!(dispatched.ended, local.ended);
        assert_eq!(dispatched.capture.result, local.capture.result);
        assert_eq!(dispatched.capture.subtype, local.capture.subtype);
        assert_eq!(
            dispatched.capture.assistant_text,
            local.capture.assistant_text
        );
        assert_eq!(
            dispatched.capture.assistant_turns,
            local.capture.assistant_turns
        );
        // The unterminated final line is the one carrying the turn's result.
        assert_eq!(dispatched.capture.subtype.as_deref(), Some("success"));
    }

    /// I1: the subscription token is carried and the metered-API keys the hub
    /// process holds are not. `use_subscription_oauth` removes them, and a
    /// removal must arrive as absence rather than as a forwarded empty value —
    /// the runner rebuilds the environment from an allowlist, so anything not
    /// added is simply not there.
    #[test]
    fn a_dispatched_request_carries_the_subscription_token_and_no_api_key() {
        let mut cmd = Command::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "sk-ant-must-not-travel");
        use_subscription_oauth(&mut cmd, "sub-token");

        let req = exec_request_for(&cmd, "prompt".into(), std::time::Duration::from_secs(600));
        let env: std::collections::HashMap<_, _> = req.env.iter().cloned().collect();

        assert_eq!(
            env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("sub-token")
        );
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(
            !req.env.iter().any(|(_, v)| v == "sk-ant-must-not-travel"),
            "the metered-API key must not travel under any name"
        );
    }

    #[test]
    fn a_dispatched_request_mirrors_the_command_it_was_built_from() {
        let mut cmd = Command::new("claude");
        cmd.args(["-p", "--output-format", "stream-json"]);
        cmd.current_dir("/app/workspace");
        let req = exec_request_for(
            &cmd,
            "the-prompt".into(),
            std::time::Duration::from_secs(900),
        );

        assert_eq!(req.program, "claude");
        assert_eq!(req.args, ["-p", "--output-format", "stream-json"]);
        assert_eq!(req.workdir.as_deref(), Some("/app/workspace"));
        assert_eq!(req.timeout_secs, 900);
        assert_eq!(req.stdin.as_deref(), Some("the-prompt"));
        assert!(!req.allow_retry, "a turn must never be re-executed");
    }

    /// The request uses fields a pre-v1 daemon would drop, so the client must
    /// be able to tell it is talking to one that will not.
    #[test]
    fn a_dispatched_request_demands_the_v1_protocol() {
        let mut cmd = Command::new("claude");
        use_subscription_oauth(&mut cmd, "sub-token");
        let req = exec_request_for(&cmd, String::new(), std::time::Duration::from_secs(60));
        assert_eq!(req.required_protocol(), 1);
    }

    /// The hub container injects `ANTHROPIC_API_KEY`, which outranks
    /// `CLAUDE_CODE_OAUTH_TOKEN` in Claude Code's headless auth precedence. If
    /// the child inherits it, CC bills the first-party API ("Credit balance is
    /// too low") instead of the user's subscription. `use_subscription_oauth`
    /// must set the OAuth token AND record removal directives for the two
    /// higher-precedence Anthropic env vars on the child command.
    #[test]
    fn use_subscription_oauth_scrubs_higher_precedence_anthropic_vars() {
        use std::ffi::OsString;

        let mut cmd = Command::new("claude");
        // Simulate the inherited container env an unfixed spawn would carry.
        cmd.env("ANTHROPIC_API_KEY", "sk-ant-api-must-not-win");
        cmd.env("ANTHROPIC_AUTH_TOKEN", "must-not-win");

        use_subscription_oauth(&mut cmd, "sk-ant-oat01-subscription");

        let envs: std::collections::HashMap<OsString, Option<OsString>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();

        // `None` is the removal directive applied over the inherited env.
        assert_eq!(envs.get(&OsString::from("ANTHROPIC_API_KEY")), Some(&None));
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_AUTH_TOKEN")),
            Some(&None)
        );
        assert_eq!(
            envs.get(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")),
            Some(&Some(OsString::from("sk-ant-oat01-subscription")))
        );
    }

    fn envs_of(
        cmd: &Command,
    ) -> std::collections::HashMap<std::ffi::OsString, Option<std::ffi::OsString>> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect()
    }

    #[test]
    fn a_privacy_route_points_the_child_at_the_gateway_without_replacing_the_login() {
        use std::ffi::OsString;
        let route = PrivacyRoute {
            gateway_url: "https://privacy.internal:8787".into(),
            token: "tenant-token-0123456789abcdef0123456789".into(),
        };
        let mut cmd = Command::new("claude");
        use_subscription_oauth_via(&mut cmd, "sk-ant-oat01-subscription", Some(&route));
        let envs = envs_of(&cmd);
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_BASE_URL")),
            Some(&Some(OsString::from("https://privacy.internal:8787")))
        );
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_CUSTOM_HEADERS")),
            Some(&Some(OsString::from(
                "X-Privacy-Token: tenant-token-0123456789abcdef0123456789"
            )))
        );
        assert_eq!(
            envs.get(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")),
            Some(&Some(OsString::from("sk-ant-oat01-subscription")))
        );
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_AUTH_TOKEN")),
            Some(&None)
        );

        // No route: an inherited base URL must not send the round anywhere odd.
        let mut cmd = Command::new("claude");
        cmd.env("ANTHROPIC_BASE_URL", "https://stale.example");
        use_subscription_oauth_via(&mut cmd, "sk-ant-oat01-subscription", None);
        let envs = envs_of(&cmd);
        assert_eq!(envs.get(&OsString::from("ANTHROPIC_BASE_URL")), Some(&None));
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_CUSTOM_HEADERS")),
            Some(&None)
        );
    }

    #[test]
    fn a_privacy_route_is_all_or_nothing() {
        use workforce_common::settings::PrivacyConfig;
        let off = PrivacyConfig::default();
        assert_eq!(PrivacyRoute::from_config(&off).unwrap(), None);
        let complete = PrivacyConfig {
            enabled: true,
            gateway_url: Some("https://privacy.internal:8787/".into()),
            token: Some(" tok-0123456789abcdef0123456789abcdef ".into()),
        };
        assert_eq!(
            PrivacyRoute::from_config(&complete).unwrap(),
            Some(PrivacyRoute {
                gateway_url: "https://privacy.internal:8787".into(),
                token: "tok-0123456789abcdef0123456789abcdef".into(),
            })
        );
        for broken in [
            PrivacyConfig {
                enabled: true,
                gateway_url: None,
                token: Some("t".into()),
            },
            PrivacyConfig {
                enabled: true,
                gateway_url: Some("privacy.internal".into()),
                token: Some("t".into()),
            },
            PrivacyConfig {
                enabled: true,
                gateway_url: Some("https://privacy.internal".into()),
                token: Some("   ".into()),
            },
        ] {
            assert!(PrivacyRoute::from_config(&broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn use_bedrock_displaces_every_other_way_to_authenticate() {
        use std::ffi::{OsStr, OsString};
        let mut cmd = Command::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "sk-ant-api-must-not-win");
        cmd.env("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-must-not-win");
        cmd.env(
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "/v2/credentials/task-role",
        );

        use_bedrock(
            &mut cmd,
            &BedrockSession {
                region: "eu-west-2".into(),
                credentials: BedrockCredentials {
                    access_key_id: "ASIA".into(),
                    secret_access_key: "secret".into(),
                    session_token: Some("token".into()),
                },
                pins: ModelPins {
                    opus: "eu.anthropic.claude-opus-4-8".into(),
                    sonnet: "eu.anthropic.claude-sonnet-5".into(),
                    haiku: "eu.anthropic.claude-haiku-4-5".into(),
                },
            },
        );

        let envs: std::collections::HashMap<OsString, Option<OsString>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        let set = |var: &str| envs.get(&OsString::from(var)).cloned().flatten();
        assert_eq!(
            set("CLAUDE_CODE_USE_BEDROCK").as_deref(),
            Some(OsStr::new("1"))
        );
        assert_eq!(set("AWS_REGION").as_deref(), Some(OsStr::new("eu-west-2")));
        assert_eq!(
            set("AWS_ACCESS_KEY_ID").as_deref(),
            Some(OsStr::new("ASIA"))
        );
        assert_eq!(
            set("AWS_SESSION_TOKEN").as_deref(),
            Some(OsStr::new("token"))
        );
        assert_eq!(
            set("ANTHROPIC_DEFAULT_HAIKU_MODEL").as_deref(),
            Some(OsStr::new("eu.anthropic.claude-haiku-4-5"))
        );
        for displaced in SUBPROCESS_SECRET_VARS.iter().chain(BEDROCK_DISPLACED_VARS) {
            assert_eq!(
                envs.get(&OsString::from(*displaced)),
                Some(&None),
                "{displaced} must be removed on the child"
            );
        }
    }

    #[test]
    fn stderr_indicates_session_not_found_matches_real_message() {
        // Verbatim line observed from prod logs after a redeploy wiped the
        // /app/.claude session directory.
        let real = "No conversation found with session ID: 986dfe1c-af29-4613-ab65-83016dfaf5c6\n";
        assert!(stderr_indicates_session_not_found(real));
    }

    #[test]
    fn stderr_indicates_session_not_found_ignores_unrelated_failures() {
        assert!(!stderr_indicates_session_not_found(""));
        assert!(!stderr_indicates_session_not_found(
            "Error: API key invalid"
        ));
        assert!(!stderr_indicates_session_not_found(
            "panic: index out of bounds"
        ));
    }

    fn retry_fixture(
        session_id: Option<Uuid>,
        persist_session: bool,
        resume: bool,
    ) -> ClaudeCodeConfig {
        ClaudeCodeConfig {
            task: String::new(),
            workdir: std::path::PathBuf::from("."),
            max_turns: None,
            allowed_tools: Vec::new(),
            append_system_prompt: None,
            timeout: Duration::from_secs(60),
            model: None,
            effort: None,
            mcp: None,
            session_id,
            resume,
            persist_session,
            load_claude_md: false,
            load_auto_memory: false,
            gh_token: None,
            cache_dir: None,
            remote: None,
            auth: ClaudeAuth::Resolved(crate::subscription::test_support::claude_login(
                "sk-ant-oat01-fixture",
            )),
            jobs: None,
            contained: None,
        }
    }

    /// The config holds a bearer token for the round's whole tool surface, so
    /// it never lands in the checkout, where a `git add -A` in the round
    /// could commit it.
    #[tokio::test]
    async fn local_claude_round_mcp_config_is_outside_workdir() {
        let (_service, tools) = crate::mcp_turn::testing::round_tools(
            &["probe.*"],
            crate::mcp_turn::ClientTimeout::CLAUDE_CODE,
        )
        .await;
        let workdir = tempfile::tempdir().unwrap();
        let config = ClaudeCodeConfig {
            workdir: workdir.path().to_path_buf(),
            mcp: Some(tools),
            ..retry_fixture(None, false, false)
        };
        let (local, path) = round_mcp(&config).await.unwrap();
        let path = path.expect("a local round is pointed at a config");
        assert!(path.is_file());
        assert!(
            !path.starts_with(workdir.path()),
            "{} is inside the checkout",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        drop(local);
        assert!(!path.exists(), "removed with the round");
    }

    /// Persisted session + UUID: the first attempt may have created the
    /// transcript file, so the retry must `--resume` instead of trying to
    /// claim the same UUID a second time via `--session-id`.
    #[test]
    fn config_for_retry_flips_to_resume_when_session_persisted_and_uuid_set() {
        let cfg = retry_fixture(Some(Uuid::new_v4()), true, false);
        let retry = config_for_retry(&cfg, Duration::from_secs(600));
        assert!(retry.resume);
        assert_eq!(retry.session_id, cfg.session_id);
    }

    /// Resume already on (session was an existing one): retry stays on resume.
    #[test]
    fn config_for_retry_preserves_resume_when_already_set() {
        let cfg = retry_fixture(Some(Uuid::new_v4()), true, true);
        let retry = config_for_retry(&cfg, Duration::from_secs(600));
        assert!(retry.resume);
    }

    /// One-shot / workflow-step (no session_id): nothing to claim, no flip.
    #[test]
    fn config_for_retry_leaves_resume_false_when_no_session_id() {
        let cfg = retry_fixture(None, true, false);
        let retry = config_for_retry(&cfg, Duration::from_secs(600));
        assert!(!retry.resume);
    }

    /// `--no-session-persistence` means no transcript is written, so the UUID
    /// is never claimed and the retry can use `--session-id` again safely.
    #[test]
    fn config_for_retry_leaves_resume_false_when_session_not_persisted() {
        let cfg = retry_fixture(Some(Uuid::new_v4()), false, false);
        let retry = config_for_retry(&cfg, Duration::from_secs(600));
        assert!(!retry.resume);
    }

    /// A retry inherits what is left of the call's budget, never a fresh window
    /// — the per-attempt clock is what turned a 20-minute bound into a
    /// 31-minute turn.
    #[test]
    fn config_for_retry_narrows_the_timeout_to_the_remaining_budget() {
        let cfg = retry_fixture(Some(Uuid::new_v4()), true, false);
        let retry = config_for_retry(&cfg, Duration::from_secs(40));
        assert_eq!(retry.timeout, Duration::from_secs(40));
        assert!(retry.timeout < cfg.timeout);
    }

    /// Regression: `ExitStatus::code()` returns `None` on Unix when
    /// the process was terminated by a signal — so SIGKILL (the OOM
    /// killer in containers) would silently fall into
    /// `CrashKind::Unknown { exit_code: -1 }` if we didn't translate
    /// it to the shell's `128 + signal` convention. Pipe a real kill
    /// through the full chain to pin this.
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_kill_classifies_as_killed_likely_oom() {
        let status = tokio::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .await
            .expect("spawn sh -c 'kill -9 $$'");
        let exit_code = exit_code_from_status(&status);
        assert_eq!(exit_code, 137, "SIGKILL must surface as 128+9=137");
        assert_eq!(
            CrashKind::from_exit_code(exit_code),
            Some(CrashKind::KilledLikelyOom)
        );
    }

    #[test]
    fn crash_kind_classifies_oom_segfault_and_unknown() {
        // SIGKILL = 128 + 9; SIGSEGV = 128 + 11. The OOM killer is the
        // common producer of 137 in container deployments — calling it
        // out distinctly is the whole point of this classification.
        assert_eq!(CrashKind::from_exit_code(0), None);
        assert_eq!(
            CrashKind::from_exit_code(137),
            Some(CrashKind::KilledLikelyOom)
        );
        assert_eq!(CrashKind::from_exit_code(139), Some(CrashKind::Segfault));
        assert_eq!(
            CrashKind::from_exit_code(1),
            Some(CrashKind::Unknown { exit_code: 1 })
        );
    }

    #[test]
    fn crash_kind_label_is_human_readable_for_each_variant() {
        assert!(CrashKind::KilledLikelyOom
            .label()
            .to_lowercase()
            .contains("oom"));
        assert!(CrashKind::Segfault
            .label()
            .to_lowercase()
            .contains("segfault"));
        assert_eq!(CrashKind::Unknown { exit_code: 7 }.label(), "exit 7");
    }

    #[test]
    fn cli_health_is_available_distinguishes_variants() {
        let avail = CliHealth::Available {
            version: "1.0.32 (Claude Code)".into(),
        };
        let missing = CliHealth::Missing {
            reason: "spawn failed".into(),
        };
        assert!(avail.is_available());
        assert!(!missing.is_available());
    }

    #[test]
    fn extract_json_array_plain() {
        let items = extract_json_array(r#"[{"topic":"a"},{"topic":"b"}]"#);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["topic"], "a");
    }

    #[test]
    fn extract_json_array_from_fenced_block_with_prose() {
        let text = "Here are the facts I found:\n```json\n[{\"claim\":\"x\"}]\n```\nDone.";
        let items = extract_json_array(text);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["claim"], "x");
    }

    #[test]
    fn extract_json_array_rejects_non_arrays_and_garbage() {
        assert!(extract_json_array("[]").is_empty());
        assert!(extract_json_array("no json here").is_empty());
        assert!(extract_json_array(r#"{"topic":"obj-not-array"}"#).is_empty());
        assert!(extract_json_array("").is_empty());
        assert!(extract_json_array(r#"[{"topic":"a"},"#).is_empty());
        // First-`[`-to-last-`]` over-captures on trailing bracketed prose; a
        // miss yields nothing rather than a half-parse.
        assert!(extract_json_array(r#"[{"topic":"a"}] and see [appendix]"#).is_empty());
    }

    #[test]
    fn extract_json_array_handles_unicode() {
        let items = extract_json_array(r#"déjà → [{"claim":"café ☕"}] ✓"#);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["claim"], "café ☕");
    }
}

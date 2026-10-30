//! The Codex CLI on the user's ChatGPT login, or on an OpenAI API key: the
//! second CLI engine, on the transport Claude Code runs on.
//!
//! A round gets its own `CODEX_HOME` written into the sandbox — the login
//! file, a `config.toml` naming the model, the policies and the hub's MCP
//! server — and runs `codex exec --json` with the prompt on stdin. The stream
//! is read with [`ThreadCapture`]; the CLI reports no cost, so the hub prices
//! the usage itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;
use workforce_agent::llm::types::TokenUsage;
use workforce_agent::runner::ProgressEvent;
use workforce_agent::tools::exec_runner::{CodexMcpConfig, CodexMcpServer, SCOPE_HOME};
use workforce_common::settings::{CodingCodexConfig, CODEX_EFFORTS};
use workforce_common::subscription::{SubscriptionKind, SubscriptionRoute};

use super::{
    CliRoundResult, CodingEngine, CodingRound, Continuity, ModelChoices, Refusal, RoundContext,
};
use crate::claude_code::{
    self, crash_kind_for, exit_code_for, segment, stderr_tail_for, CapturedRun, CliHealth,
    CliInvocation, Completion, Interrupted, ProgressShape, RemotePlacement, StreamGrammar,
};
use crate::coding_sandbox::{CodingSandbox, SandboxCommand};
use crate::mcp::MCP_SERVER_KEY;
use crate::mcp_turn::{ClientTimeout, LocalMcp, RoundTools};
use crate::subscription::limits::{codex_limit_reached, codex_limit_text};
use crate::subscription::{write_back, AuthJson, Login, RefreshError, TokenRefresher, VaultSlot};
use crate::vault::Vault;
use tokio::io::AsyncWriteExt;

pub const LABEL: &str = "codex";
pub const API_LABEL: &str = "codex_api";
/// Why a Codex call has nowhere to run on a hub without a remote worker.
pub const WORKER_ONLY: &str = "codex runs only on the remote coding worker, and this hub has none";

/// The vault key holding the ChatGPT login file `codex login` writes.
pub const AUTH_VAULT_KEY: &str =
    workforce_common::subscription::SubscriptionKind::ChatGpt.vault_key();
/// The vault key holding an OpenAI API key for metered rounds.
pub const API_KEY_VAULT_KEY: &str = "openai/api_key";

const AUTH_FILE: &str = "auth.json";
/// Beside `auth.json`: the fingerprint of the login its slot held for it when
/// it was last written or kept, so whichever process finds the home can tell
/// a refresh the CLI made from the login it was handed.
const HANDED_FILE: &str = "auth.handed";
/// How long past a round's timeout its login must stay valid, so the CLI
/// never refreshes it mid-round.
const ROUND_LOGIN_MARGIN: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
const CONFIG_FILE: &str = "config.toml";
const LAST_MESSAGE_FILE: &str = "last-message.md";
const THREAD_FILE: &str = "thread-id";
/// Beside the thread id: the thread, and what it had used in all when a
/// round last left it. Codex reports a resumed thread's usage from its first
/// turn on, so a round is billed what it adds to this.
const USAGE_FILE: &str = "thread-usage";
/// The instructions that replace Codex's own in a contained round.
const INSTRUCTIONS_FILE: &str = "instructions.md";
const LAST_MESSAGE_MAX_BYTES: usize = 1024 * 1024;

/// Whose account a round bills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexAuth {
    /// The payer's ChatGPT login, as the agent's subscription logins resolve.
    Subscription,
    /// An OpenAI API key, billed per token.
    ApiKey,
}

impl CodexAuth {
    fn label(self) -> &'static str {
        match self {
            Self::Subscription => LABEL,
            Self::ApiKey => API_LABEL,
        }
    }
}

/// What a round's Codex may do beyond answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexPolicy {
    /// A coding round: the shell with the full environment, writes anywhere.
    Coding,
    /// A round that only answers, in a read-only sandbox with nothing of the
    /// host's. `apply_patch` has no switch, so such a round runs only in an
    /// empty worker scope.
    Contained,
}

impl CodexPolicy {
    /// How a round's stream reads as progress: a contained round reports
    /// itself as an agent turn does.
    fn shape(self) -> ProgressShape {
        match self {
            Self::Coding => ProgressShape::Coding,
            Self::Contained => ProgressShape::Turn,
        }
    }
}

pub struct CodexEngine {
    auth: CodexAuth,
    policy: CodexPolicy,
    config: CodingCodexConfig,
    /// Where a session round on this host keeps its home between turns.
    session_home_root: PathBuf,
}

impl CodexEngine {
    /// An engine whose local session homes live under this host's temp dir
    /// until [`Self::with_data_dir`] says otherwise.
    pub fn new(auth: CodexAuth, config: CodingCodexConfig) -> Self {
        Self {
            auth,
            policy: CodexPolicy::Coding,
            config,
            session_home_root: local_home_root(),
        }
    }

    /// An engine on the payer's ChatGPT login whose rounds only answer.
    pub fn contained(config: CodingCodexConfig) -> Self {
        Self {
            policy: CodexPolicy::Contained,
            ..Self::new(CodexAuth::Subscription, config)
        }
    }

    /// Keep local session homes under `data_dir`, the hub's data directory.
    pub fn with_data_dir(mut self, data_dir: &Path) -> Self {
        self.session_home_root = session_home_root(data_dir);
        self
    }
}

/// The folder under the hub's data directory that holds local session homes.
const SESSION_HOMES_DIR: &str = "codex-homes";

/// Where local session homes live for a hub whose data directory is
/// `data_dir`. A relative `data_dir` (the default is `./data`) is anchored to
/// this process's working directory: Codex runs in the workspace, where a
/// relative `CODEX_HOME` would name somewhere else.
fn session_home_root(data_dir: &Path) -> PathBuf {
    crate::coding_sandbox::absolutise_data_dir(data_dir.to_path_buf()).join(SESSION_HOMES_DIR)
}

/// The credential a round runs with, resolved once before anything is written.
pub(crate) enum Credential {
    /// A ChatGPT login, holding the sign-in the round is handed, and whether
    /// the round's home already holds it, as the job the round resumes left
    /// it.
    Login {
        login: Box<Login>,
        held: bool,
    },
    ApiKey(String),
}

/// The sign-in a ChatGPT login holds.
fn sign_in(login: &Login) -> Result<&AuthJson> {
    login
        .auth()
        .ok_or_else(|| eyre!("the login is not a ChatGPT sign-in"))
}

impl Credential {
    /// The ChatGPT login the subscription resolver chose.
    pub(crate) fn of_login(login: &Login) -> Result<Self> {
        sign_in(login).map(|_| Self::Login {
            login: Box::new(login.clone()),
            held: false,
        })
    }

    /// The login a ChatGPT credential holds; none for an API key.
    fn into_login(self) -> Option<Login> {
        match self {
            Self::Login { login, .. } => Some(*login),
            Self::ApiKey(_) => None,
        }
    }

    /// The OpenAI API key a metered round runs on.
    fn api_key(
        vault: Option<&Arc<Vault>>,
        global_vault: Option<&Arc<Vault>>,
    ) -> Result<Self, Refusal> {
        crate::user_config::resolve_flat(
            vault.map(Arc::as_ref),
            global_vault.map(Arc::as_ref),
            API_KEY_VAULT_KEY,
        )
        .or_else(|| claude_code::non_empty_env("OPENAI_API_KEY"))
        .map(Self::ApiKey)
        .ok_or_else(|| {
            Refusal::no_token(format!(
                "No OpenAI API key for Codex: set vault {API_KEY_VAULT_KEY}."
            ))
        })
    }
}

/// Where a round's `CODEX_HOME` lives. On the worker it is inside the scope's
/// home. On this host a session's home is under the hub's data directory,
/// which every replica of the hub shares, so the next turn finds the thread it
/// resumes whichever replica takes it, and Codex, being outside the temp dir,
/// may install its helper binaries there; a one-shot round's home is under
/// this host's temp dir. A session keeps one home across its rounds, so the
/// thread it opened can be resumed; a one-shot round gets its own and it is
/// removed afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RoundHome {
    /// A home on this host, and the root it was made under: nothing outside
    /// `root` is ever removed.
    Local {
        root: PathBuf,
        dir: PathBuf,
    },
    Remote(PathBuf),
}

/// Where one-shot round homes on this host live.
fn local_home_root() -> PathBuf {
    std::env::temp_dir().join("workforce-codex")
}

impl RoundHome {
    fn new(remote: bool, continuity: Continuity, session_root: &Path) -> Self {
        let (name, root) = match continuity {
            Continuity::Session { id, .. } => (format!(".codex-{id}"), session_root.to_path_buf()),
            Continuity::OneShot => (
                format!(".codex-{}", Uuid::new_v4().simple()),
                local_home_root(),
            ),
        };
        if remote {
            Self::Remote(Path::new(SCOPE_HOME).join(name))
        } else {
            let dir = root.join(name);
            Self::Local { root, dir }
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Local { dir, .. } | Self::Remote(dir) => dir,
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    /// Whether the workspace a thread recorded here worked in may be gone: a
    /// home on this host outlives its workspace, while a scope's home is new
    /// exactly when its scope is.
    fn outlived(&self, sandbox: &dyn CodingSandbox) -> bool {
        matches!(self, Self::Local { .. }) && sandbox.fresh()
    }

    async fn write(&self, sandbox: &dyn CodingSandbox, name: &str, content: &str) -> Result<()> {
        match self {
            Self::Local { .. } => write_shared(&self.file(name), content).await,
            Self::Remote(_) => sandbox.write(&self.file(name), content).await,
        }
    }

    async fn read(&self, sandbox: &dyn CodingSandbox, name: &str) -> Result<Option<String>> {
        match self {
            Self::Local { .. } => match tokio::fs::read(self.file(name)).await {
                Ok(bytes) => Ok(Some(crate::coding_sandbox::head_utf8(
                    &bytes,
                    LAST_MESSAGE_MAX_BYTES,
                ))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).wrap_err_with(|| format!("reading {}", self.file(name).display())),
            },
            Self::Remote(_) => sandbox.read(&self.file(name), LAST_MESSAGE_MAX_BYTES).await,
        }
    }

    /// Close a session's home on this host to everyone but the hub once its
    /// round has run to its end. Every agent's commands run as the runner's
    /// user, which may enter a home only while a round runs there: the home
    /// keeps its login and thread for the next round, whose first write opens
    /// it again ([`write_shared`]). A home on the worker is left alone; there
    /// the round writes as the user every round in its scope runs as.
    async fn close(&self) {
        let Self::Local { dir, .. } = self else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await
            {
                warn!(target: "codex", dir = %dir.display(), error = %e, "codex session home not closed");
            }
        }
    }

    /// A home no later round resumes is removed with its round, and its
    /// login with it: on this host directly, else through the sandbox, which
    /// runs as the user the round wrote as — inside a scope, which outlives
    /// the round and may hold other people's rounds, and here for what that
    /// user left behind (Codex's own logs and rollouts).
    async fn discard(&self, sandbox: &dyn CodingSandbox) {
        match self {
            Self::Local { root, dir } if dir == root || !dir.starts_with(root) => {
                warn!(target: "codex", dir = %dir.display(), "codex round home outside its root; not removed");
            }
            Self::Local { dir, .. } => match tokio::fs::remove_dir_all(dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(first) => sweep(sandbox, dir, Some(&first)).await,
            },
            Self::Remote(dir) => sweep(sandbox, dir, None).await,
        }
    }
}

/// Remove `dir` through `sandbox`, as the user its runs write as; `first` is
/// why removing it here failed, when that was tried.
async fn sweep(sandbox: &dyn CodingSandbox, dir: &Path, first: Option<&std::io::Error>) {
    let swept = sandbox
        .exec(SandboxCommand {
            program: "rm".into(),
            args: vec!["-rf".into(), dir.display().to_string()],
            cwd: None,
            env: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(30),
        })
        .await;
    match swept {
        Ok(out) if out.success() => {}
        Ok(out) => warn!(
            target: "codex", dir = %dir.display(), exit = out.exit_code(), stderr = %out.stderr,
            "codex round home not removed"
        ),
        Err(e) => warn!(
            target: "codex", dir = %dir.display(), first = ?first.map(ToString::to_string), error = %e,
            "codex round home not removed"
        ),
    }
}

/// Write a file the hub-local runner's user can read and Codex can write
/// beside: the directory and file are group-accessible, nothing wider. The
/// content lands in a fresh file that is renamed over the target, so a file
/// the runner's user rewrote on an earlier turn (Codex persists its project
/// trust in `config.toml`) never blocks the next turn's write.
async fn write_shared(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| eyre!("{} has no file name", path.display()))?;
    tokio::fs::create_dir_all(dir).await?;
    let staged = dir.join(format!(".{name}.{}", Uuid::new_v4().simple()));
    let write = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o660);
        let mut file = options.open(&staged).await?;
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o770)).await?;
            tokio::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o660)).await?;
        }
        tokio::fs::rename(&staged, path).await
    };
    if let Err(e) = write.await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(e).wrap_err_with(|| format!("writing {}", path.display()));
    }
    Ok(())
}

/// Codex features a hub round never uses. Each one left on adds tools to every
/// turn and time before the first token, so the round's config turns them
/// off.
const UNUSED_FEATURES: &[&str] = &[
    "apps",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "image_generation",
    "in_app_browser",
    "multi_agent",
    "plugins",
    "remote_plugin",
    "skill_search",
];

/// `config.toml` as this engine writes it: the model and policies for the
/// round, history off, the full environment for the shells Codex spawns (so
/// `gh` sees its token), the features a round never uses turned off, and the
/// hub's MCP server when the round runs here. On the worker the MCP block is
/// appended by the daemon, which alone knows the endpoint it binds for the
/// job.
#[derive(Debug, Serialize)]
struct ConfigFile<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_reasoning_effort: Option<&'a str>,
    approval_policy: &'static str,
    sandbox_mode: &'static str,
    cli_auth_credentials_store: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    web_search: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_doc_max_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_permissions_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_apps_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_instructions_file: Option<PathBuf>,
    history: History,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<Skills>,
    shell_environment_policy: ShellEnvironmentPolicy,
    features: BTreeMap<&'static str, bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_servers: BTreeMap<&'static str, CodexMcpServer>,
}

#[derive(Debug, Serialize)]
struct History {
    persistence: &'static str,
}

#[derive(Debug, Serialize)]
struct Skills {
    include_instructions: bool,
}

#[derive(Debug, Serialize)]
struct ShellEnvironmentPolicy {
    inherit: &'static str,
    ignore_default_excludes: bool,
}

/// The tools a contained round turns off besides [`UNUSED_FEATURES`].
const CONTAINED_OFF: &[&str] = &[
    "goals",
    "shell_tool",
    "sleep_tool",
    "tool_suggest",
    "view_image",
];

fn render_config(
    policy: CodexPolicy,
    model: &str,
    effort: Option<&str>,
    mcp: Option<CodexMcpServer>,
    instructions: Option<PathBuf>,
) -> Result<String> {
    let coding = policy == CodexPolicy::Coding;
    let off: &[&str] = if coding { &[] } else { CONTAINED_OFF };
    let file = ConfigFile {
        model,
        model_reasoning_effort: effort,
        approval_policy: "never",
        sandbox_mode: if coding {
            "danger-full-access"
        } else {
            "read-only"
        },
        cli_auth_credentials_store: "file",
        web_search: (!coding).then_some("disabled"),
        project_doc_max_bytes: (!coding).then_some(0),
        include_permissions_instructions: (!coding).then_some(false),
        include_apps_instructions: (!coding).then_some(false),
        model_instructions_file: instructions,
        history: History {
            persistence: "none",
        },
        skills: (!coding).then_some(Skills {
            include_instructions: false,
        }),
        shell_environment_policy: ShellEnvironmentPolicy {
            inherit: if coding { "all" } else { "none" },
            ignore_default_excludes: coding,
        },
        features: UNUSED_FEATURES
            .iter()
            .chain(off)
            .map(|f| (*f, false))
            .collect(),
        mcp_servers: mcp.into_iter().map(|s| (MCP_SERVER_KEY, s)).collect(),
    };
    toml::to_string(&file).wrap_err("rendering codex config.toml")
}

/// The namespace Codex lists the hub's tools in, each under its MCP name.
/// Claude Code spells a tool as the two joined by `__`.
fn tool_namespace() -> String {
    format!("mcp__{MCP_SERVER_KEY}")
}

/// How the agent's tools are named to Codex, which reaches them over MCP.
pub(crate) fn tool_naming() -> String {
    format!(
        "\n\n## Tool names\nYour tools are served over MCP by the `{MCP_SERVER_KEY}` server, in \
         the `{}` namespace: a tool these instructions call `a.b` is `a_b` there. Find a tool \
         with `tool_search` before you first call it.",
        tool_namespace()
    )
}

/// Prompts name hub tools the way Claude Code shows them; Codex lists the
/// same tool by its name alone, in [`tool_namespace`].
fn render_for_codex(text: &str) -> String {
    text.replace(&format!("{}__", tool_namespace()), "")
}

/// What a round sends Codex: the prompt on stdin, and the instructions that
/// replace Codex's own. A contained round's system prompt is those
/// instructions, read afresh on every run, so a resumed thread never holds
/// it; a coding round's heads the prompt.
fn prompts_for(policy: CodexPolicy, round: &CodingRound) -> (String, Option<String>) {
    let task = render_for_codex(&round.task);
    let system = round
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(render_for_codex);
    match (policy, system) {
        (CodexPolicy::Coding, Some(system)) => {
            (format!("# Instructions\n\n{system}\n\n{task}"), None)
        }
        (_, instructions) => (task, instructions),
    }
}

/// Everything worth keeping from `codex exec --json`: the thread to resume,
/// the last message, the thread's usage, and the failure that ended the turn
/// if one did.
#[derive(Debug, Default)]
pub(crate) struct ThreadCapture {
    shape: ProgressShape,
    /// What the thread had used before this run.
    used_before: Usage,
    thread_id: Option<String>,
    message: Option<String>,
    /// What the thread has used in all, once a turn completes.
    used: Option<Usage>,
    failure: Option<String>,
    completed: bool,
    turns: u32,
    messages: u32,
}

impl ThreadCapture {
    fn shaped(shape: ProgressShape, used_before: Usage) -> Self {
        Self {
            shape,
            used_before,
            ..Self::default()
        }
    }

    /// What this run added to its thread.
    fn usage(&self) -> Usage {
        self.used
            .map(|used| used.since(self.used_before))
            .unwrap_or_default()
    }
}

/// A thread, and what it had used in all when a round last left it.
#[derive(Debug, Serialize, Deserialize)]
struct ThreadUsage {
    thread: String,
    used: Usage,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Usage {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

impl Usage {
    /// What was used since `before`, a total this one continues.
    fn since(self, before: Self) -> Self {
        Self {
            input: self.input.saturating_sub(before.input),
            cached: self.cached.saturating_sub(before.cached),
            cache_write: self.cache_write.saturating_sub(before.cache_write),
            output: self.output.saturating_sub(before.output),
        }
    }

    /// This total with `spent`, as [`Self::tokens`] counts it, added.
    fn plus(self, spent: &TokenUsage) -> Self {
        Self {
            input: self.input
                + u64::from(spent.input_tokens)
                + u64::from(spent.cache_read_input_tokens),
            cached: self.cached + u64::from(spent.cache_read_input_tokens),
            cache_write: self.cache_write + u64::from(spent.cache_creation_input_tokens),
            output: self.output + u64::from(spent.output_tokens),
        }
    }

    fn tokens(self) -> TokenUsage {
        let count = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        TokenUsage {
            input_tokens: count(self.input.saturating_sub(self.cached)),
            output_tokens: count(self.output),
            cache_creation_input_tokens: count(self.cache_write),
            cache_read_input_tokens: count(self.cached),
        }
    }

    fn from_event(event: &serde_json::Value) -> Self {
        let count = |key: &str| {
            event
                .pointer(&format!("/usage/{key}"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        Self {
            input: count("input_tokens"),
            cached: count("cached_input_tokens"),
            cache_write: count("cache_write_input_tokens"),
            output: count("output_tokens"),
        }
    }
}

impl StreamGrammar for ThreadCapture {
    fn record(&mut self, event: &serde_json::Value) -> Vec<ProgressEvent> {
        let field = |pointer: &str| event.pointer(pointer).and_then(|v| v.as_str());
        let coding = self.shape == ProgressShape::Coding;
        match field("/type") {
            Some("thread.started") => {
                self.thread_id = field("/thread_id").map(str::to_string);
                Vec::new()
            }
            Some("turn.started") => {
                self.turns += 1;
                coding
                    .then_some(ProgressEvent::LlmCall { round: self.turns })
                    .into_iter()
                    .collect()
            }
            Some("item.started") if coding => self.item_started(event),
            Some("item.completed") => self.item_completed(event),
            Some("turn.completed") => {
                self.used = Some(Usage::from_event(event));
                self.failure = None;
                self.completed = true;
                let usage = self.usage();
                info!(
                    target: "codex",
                    turns = self.turns,
                    input_tokens = usage.input,
                    cached_input_tokens = usage.cached,
                    output_tokens = usage.output,
                    "codex finished"
                );
                coding
                    .then(|| {
                        let tokens = usage.tokens();
                        ProgressEvent::Done {
                            rounds: self.turns,
                            input_tokens: tokens.input_tokens,
                            output_tokens: tokens.output_tokens,
                            cache_read_tokens: tokens.cache_read_input_tokens,
                            tool_count: 0,
                        }
                    })
                    .into_iter()
                    .collect()
            }
            Some("turn.failed") => {
                self.failure = field("/error/message").map(str::to_string);
                self.completed = true;
                Vec::new()
            }
            // An error before any turn ends the run. Once a turn is under
            // way it may be a retry the CLI is making, so the turn's own end
            // decides, and the error stands only if the process ends first.
            Some("error") => {
                self.failure = field("/message").map(str::to_string);
                self.completed |= self.turns == 0;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn finished(&self) -> bool {
        self.completed
    }

    fn spent(&self) -> TokenUsage {
        self.usage().tokens()
    }

    fn thread(&self) -> Option<String> {
        self.thread_id.clone()
    }
}

impl ThreadCapture {
    fn item_started(&self, event: &serde_json::Value) -> Vec<ProgressEvent> {
        let item = &event["item"];
        let text = |key: &str| item.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let (name, detail) = match text("type") {
            "command_execution" => ("codex:shell".to_string(), text("command").to_string()),
            "mcp_tool_call" => (
                format!("codex:{}__{}", text("server"), text("tool")),
                String::new(),
            ),
            "file_change" => (
                "codex:edit".to_string(),
                item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("path").and_then(|p| p.as_str()))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => return Vec::new(),
        };
        info!(target: "codex", tool = %name, "codex tool call");
        vec![ProgressEvent::ToolStart {
            name,
            round: self.turns,
            detail,
        }]
    }

    fn item_completed(&mut self, event: &serde_json::Value) -> Vec<ProgressEvent> {
        let item = &event["item"];
        match item.get("type").and_then(|v| v.as_str()) {
            Some("agent_message") => {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                self.message = Some(text.to_string());
                self.messages += 1;
                match self.shape {
                    ProgressShape::Coding => vec![ProgressEvent::Response {
                        text: text.to_string(),
                    }],
                    ProgressShape::Turn => std::iter::once(ProgressEvent::LlmCall {
                        round: self.messages,
                    })
                    .chain(
                        (!text.is_empty())
                            .then(|| segment(text))
                            .into_iter()
                            .flatten(),
                    )
                    .collect(),
                }
            }
            Some("error") => {
                warn!(
                    target: "codex",
                    message = item.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                    "codex reported a non-fatal error"
                );
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// Why a Codex round ended, from every signal available: the CLI's own
/// verdict outranks how the process ended, as with Claude Code.
fn completion_of(
    exit_code: i32,
    timed_out: bool,
    resumed: bool,
    capture: &ThreadCapture,
    has_text: bool,
) -> Completion {
    match (timed_out, &capture.failure, capture.completed) {
        (true, _, _) if has_text => Completion::Timeout,
        (true, _, _) => Completion::Failed,
        (_, None, true) => Completion::Success,
        (_, None, false) if exit_code == 0 && has_text => Completion::Success,
        _ if resumed && capture.thread_id.is_none() => Completion::SessionNotFound,
        _ => Completion::Failed,
    }
}

/// Turn a finished process into the round's result. `last_message` is the
/// `-o` file when it could be read: the whole final message rather than the
/// stream's last item. A resumed round that failed before its thread even
/// started has lost the thread it asked for, which the chat recovers from by
/// minting a fresh session, as it does for Claude Code.
fn result_of(
    run: CapturedRun<ThreadCapture>,
    model: &str,
    resumed: bool,
    last_message: Option<String>,
    timeout: Duration,
    duration: Duration,
) -> CliRoundResult {
    let raw_exit = run.exit_code.unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let message = last_message
        .filter(|m| !m.trim().is_empty())
        .or_else(|| run.capture.message.clone());
    let completion = completion_of(
        raw_exit,
        run.ended.timed_out(),
        resumed,
        &run.capture,
        message.is_some(),
    );
    let exit_code = exit_code_for(completion, run.ended, raw_exit);
    let text = match (&run.capture.failure, message) {
        (Some(failure), _) => failure.clone(),
        (None, Some(message)) => message,
        (None, None) => String::new(),
    };
    let spent = run.capture.spent();
    let stderr_tail = stderr_tail_for(completion, &stderr);
    let notice = run.capture.failure.clone().unwrap_or_default();
    info!(
        target: "codex",
        exit_code,
        completion = completion.as_str(),
        duration_ms = duration.as_millis() as u64,
        stderr_head = %stderr.chars().take(500).collect::<String>(),
        "codex subprocess complete"
    );
    CliRoundResult {
        text,
        engine: String::new(),
        model: Some(model.to_string()),
        exit_code,
        duration,
        input_tokens: spent.input_tokens.into(),
        output_tokens: spent.output_tokens.into(),
        cache_creation_tokens: spent.cache_creation_input_tokens.into(),
        cache_read_tokens: spent.cache_read_input_tokens.into(),
        cost_usd: 0.0,
        num_turns: run.capture.turns.max(1),
        completion,
        crash_kind: crash_kind_for(completion, run.ended, exit_code, timeout),
        stderr_tail,
        notice,
        handed_over: Vec::new(),
        limit: run
            .capture
            .failure
            .as_deref()
            .and_then(|failure| codex_limit_text(failure, chrono::Utc::now())),
        plan_usage: Vec::new(),
        job: None,
    }
}

/// Run `codex --version` once and classify the outcome, bounded like the
/// Claude Code probe.
pub async fn probe_cli() -> CliHealth {
    let probe = async {
        let mut cmd = Command::new("codex");
        cmd.arg("--version");
        cmd.kill_on_drop(true);
        match cmd.output().await {
            Ok(out) if out.status.success() => CliHealth::Available {
                version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            },
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
        Ok(health) => health,
        Err(_) => CliHealth::Missing {
            reason: "codex --version timed out after 5s".to_string(),
        },
    }
}

/// The `codex` program a round runs.
#[cfg(not(test))]
fn codex_program() -> PathBuf {
    PathBuf::from("codex")
}

/// The scripted `codex` a test scopes in; outside such a scope, the real
/// CLI's name, as the tests that only build a command expect.
#[cfg(test)]
fn codex_program() -> PathBuf {
    tests::CODEX
        .try_with(PathBuf::clone)
        .unwrap_or_else(|_| PathBuf::from("codex"))
}

/// A round ready to run: its home written, its command built.
struct Prepared {
    home: RoundHome,
    model: String,
    invocation: CliInvocation,
    /// Whether the home is a session's, kept for the next round.
    keeps_home: bool,
    /// Whether the command resumes a thread from an earlier round.
    resumed: bool,
    /// What the resumed thread had used before this round.
    used_before: Usage,
    /// The round asked to resume a thread that is gone: a contained round's
    /// thread is all it knew, while a coding round's workspace still holds
    /// its work, so only a contained round reports it rather than starting
    /// afresh.
    lost: bool,
}

impl CodexEngine {
    /// The model id a round asking for `requested` runs on.
    fn model(&self, requested: Option<&str>) -> String {
        self.config.resolve_model(requested.unwrap_or("sol"))
    }

    /// How long Codex waits for one hub tool call: as long as the tool may
    /// run and a whole approval prompt besides, so a held call is never
    /// abandoned — and then run twice — while its prompt is still open.
    fn client_timeout(&self) -> ClientTimeout {
        ClientTimeout::for_tool(self.config.effective_tool_timeout())
    }

    /// Where a round's `CODEX_HOME` is.
    fn home(&self, sandbox: &dyn CodingSandbox, round: &CodingRound) -> RoundHome {
        RoundHome::new(
            sandbox.placement().is_some(),
            round.continuity,
            &self.session_home_root,
        )
    }

    /// Write the round's `home` and build its invocation; nothing is spawned.
    /// `mcp` is the round's hub tools: a round on this host reaches them on
    /// `local`, which the caller holds for the round, and a remote one over
    /// the tunnel its invocation carries. `images` are files in the workspace
    /// shown with the prompt.
    #[allow(clippy::too_many_arguments)]
    async fn prepare(
        &self,
        sandbox: &dyn CodingSandbox,
        round: &CodingRound,
        home: RoundHome,
        credential: &Credential,
        gh_token: Option<String>,
        mcp: Option<&RoundTools>,
        local: Option<&LocalMcp>,
        images: &[PathBuf],
    ) -> Result<Prepared> {
        let placement = sandbox.placement().map(|placement| RemotePlacement {
            jobs: placement.jobs && self.policy == CodexPolicy::Coding,
            ..placement
        });
        let remote = placement.is_some();
        let keeps_home = matches!(round.continuity, Continuity::Session { .. });
        let asks_resume = matches!(round.continuity, Continuity::Session { resume: true, .. })
            && !home.outlived(sandbox);
        let resume_thread = match asks_resume {
            true => home
                .read(sandbox, THREAD_FILE)
                .await?
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            false => None,
        };
        let used_before = match &resume_thread {
            Some(thread) => home
                .read(sandbox, USAGE_FILE)
                .await?
                .and_then(|kept| serde_json::from_str::<ThreadUsage>(kept.trim()).ok())
                .filter(|kept| &kept.thread == thread)
                .map(|kept| kept.used)
                .unwrap_or_default(),
            None => Usage::default(),
        };
        let model = self.model(round.model.as_deref());
        let tool_timeout = mcp
            .map_or_else(|| self.client_timeout(), RoundTools::client_timeout)
            .client
            .as_secs()
            .max(1);
        let local_mcp =
            local.map(|local| CodexMcpServer::http(&local.url(), local.token(), tool_timeout));
        let tunnelled = mcp.filter(|_| remote);
        let codex_mcp_config = tunnelled.map(|_| CodexMcpConfig {
            path: home.file(CONFIG_FILE).display().to_string(),
            tool_timeout_secs: tool_timeout,
        });

        if let Credential::Login { login, held: false } = credential {
            let auth = sign_in(login)?;
            home.write(sandbox, AUTH_FILE, &auth.render()).await?;
            home.write(sandbox, HANDED_FILE, &auth.fingerprint())
                .await?;
        }
        let (prompt, instructions) = prompts_for(self.policy, round);
        if let Some(instructions) = &instructions {
            home.write(sandbox, INSTRUCTIONS_FILE, instructions).await?;
        }
        home.write(
            sandbox,
            CONFIG_FILE,
            &render_config(
                self.policy,
                &model,
                round.effort.as_deref(),
                local_mcp,
                instructions.map(|_| home.file(INSTRUCTIONS_FILE)),
            )?,
        )
        .await?;

        // Every option belongs to `exec` and goes before `resume`: the
        // subcommand takes only some of them after it, and not `-C`. The
        // thread follows `resume`, then the prompt on stdin, `-`, and the
        // images last: `-i` takes every value after it.
        let mut cmd = Command::new(codex_program());
        cmd.arg("exec")
            .arg("--json")
            .arg("-o")
            .arg(home.file(LAST_MESSAGE_FILE))
            .arg("-C")
            .arg(sandbox.workdir())
            .arg("--skip-git-repo-check");
        if !keeps_home {
            cmd.arg("--ephemeral");
        }
        if let Some(thread) = &resume_thread {
            cmd.arg("resume").arg(thread);
        }
        cmd.arg("-");
        for image in images {
            cmd.arg("-i").arg(image);
        }
        cmd.current_dir(sandbox.workdir());
        cmd.env("CODEX_HOME", home.path());
        cmd.env("TZ", "UTC");
        for var in claude_code::SUBPROCESS_SECRET_VARS
            .iter()
            .chain(["CLAUDE_CODE_OAUTH_TOKEN"].iter())
        {
            cmd.env_remove(var);
        }
        match credential {
            Credential::Login { .. } => {
                cmd.env_remove("OPENAI_API_KEY");
                cmd.env_remove("CODEX_API_KEY");
            }
            Credential::ApiKey(key) => {
                cmd.env("CODEX_API_KEY", key);
                cmd.env_remove("OPENAI_API_KEY");
            }
        }
        if let Some(token) = gh_token {
            cmd.env("GH_TOKEN", &token);
            cmd.env("GITHUB_TOKEN", &token);
            if remote {
                for (var, value) in claude_code::remote_git_credential_env() {
                    cmd.env(var, value);
                }
            }
        }
        if !remote {
            let cache_env = round
                .cache_dir
                .iter()
                .flat_map(|dir| claude_code::toolchain_cache_env(dir));
            for (var, val) in claude_code::inherited_toolchain_env()
                .into_iter()
                .chain(cache_env)
            {
                cmd.env(var, val);
            }
        }

        Ok(Prepared {
            home,
            model,
            resumed: resume_thread.is_some(),
            used_before,
            lost: asks_resume && resume_thread.is_none() && self.policy == CodexPolicy::Contained,
            invocation: CliInvocation {
                command: cmd,
                stdin: prompt,
                timeout: round.timeout,
                remote: placement,
                mcp: tunnelled.map(|tools| tools.endpoint().clone()),
                codex_mcp_config,
                job: round.jobs.as_ref().map(crate::claude_code::RoundJobs::job),
            },
            keeps_home,
        })
    }

    async fn run_round(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        started_at: chrono::DateTime<chrono::Utc>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let credential = match self.handed(sandbox.as_ref(), round, ctx).await {
            Some(login) => Credential::of_login(&login)?,
            None => self
                .credential(ctx)
                .map_err(|refusal| eyre!(refusal.reason))?,
        };
        let gh_token = claude_code::effective_gh_token(
            round.gh_token.clone(),
            ctx.service.vault.as_deref(),
            ctx.service.global_vault.as_deref(),
        );
        let tools = ctx
            .round_tools(sandbox.as_ref(), cancel.as_ref(), self.client_timeout())
            .map(|tools| tools.with_model(self.model(round.model.as_deref())));
        let Driven {
            result,
            model,
            login,
        } = self
            .drive(
                sandbox.as_ref(),
                round,
                credential,
                RoundAids {
                    refresher: ctx.service.subscriptions().chatgpt().refresher(),
                    gh_token,
                    tools,
                    images: Vec::new(),
                },
                cancel,
                progress,
            )
            .await?;
        let ledger_model = format!("{}:{model}", self.label());
        let cost_usd = ctx.service.cost_tracker.calculate_cost(
            &ledger_model,
            result.input_tokens,
            result.output_tokens,
            result.cache_creation_tokens,
            result.cache_read_tokens,
        );
        let result = CliRoundResult { cost_usd, ..result };
        if let Some(login) = &login {
            login.settle(&result.round_end(), ctx.service.agent_id.as_str());
        }
        let served = login
            .map(|login| login.served(SubscriptionRoute::Codex, ctx.service.agent_id.as_str()));
        ctx.record(self.label(), &ledger_model, &result, started_at, served);
        Ok(result)
    }

    /// The credential a round on this engine runs with: the payer's ChatGPT
    /// login, or an OpenAI API key.
    fn credential(&self, ctx: &RoundContext<'_>) -> Result<Credential, Refusal> {
        match self.auth {
            CodexAuth::Subscription => ctx.login(SubscriptionKind::ChatGpt).and_then(|login| {
                Credential::of_login(&login).map_err(|e| Refusal::no_token(e.to_string()))
            }),
            CodexAuth::ApiKey => Credential::api_key(
                ctx.service.vault.as_ref(),
                ctx.service.global_vault.as_ref(),
            ),
        }
    }

    /// The login a collected round's job was handed, while its slot still
    /// holds it: the job's home goes on with it whichever login resolves now.
    async fn handed(
        &self,
        sandbox: &dyn CodingSandbox,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
    ) -> Option<Login> {
        if self.auth != CodexAuth::Subscription || !round.collecting() {
            return None;
        }
        let handed = self
            .home(sandbox, round)
            .read(sandbox, HANDED_FILE)
            .await
            .ok()
            .flatten()?;
        ctx.service
            .logins
            .holding(SubscriptionKind::ChatGpt, &|login| {
                login.credential.fingerprint() == handed.trim()
            })
    }

    /// Run `round` on Codex in `sandbox` on `login`, contained to answering:
    /// a one-shot completion, or one round of an agent turn served `tools`
    /// and shown `images`. Returns the result, unpriced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn drive_contained(
        subscriptions: &crate::hub_core::subscriptions::SharedSubscriptions,
        sandbox: &dyn CodingSandbox,
        round: &CodingRound,
        login: &Login,
        tools: Option<RoundTools>,
        images: Vec<PathBuf>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        Self::contained(subscriptions.codex().clone())
            .drive(
                sandbox,
                round,
                Credential::of_login(login)?,
                RoundAids {
                    refresher: subscriptions.chatgpt().refresher(),
                    gh_token: None,
                    tools,
                    images,
                },
                cancel,
                progress,
            )
            .await
            .map(|driven| driven.result)
    }

    /// Run one round on `credential` in `sandbox`: its home written, a
    /// refresh the CLI made kept in the login's slot whatever the outcome,
    /// a one-shot home removed, and a session's home on this host closed
    /// once the CLI has exited (a cut round's job may still be running in
    /// it).
    pub(crate) async fn drive(
        &self,
        sandbox: &dyn CodingSandbox,
        round: &CodingRound,
        credential: Credential,
        aids: RoundAids<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<Driven> {
        let home = self.home(sandbox, round);
        let credential = match credential {
            Credential::Login { login, .. } => {
                let (auth, held) =
                    round_login(aids.refresher, &home, sandbox, round, &login).await?;
                Credential::Login {
                    login: Box::new(login.refreshed(auth)),
                    held,
                }
            }
            key => key,
        };
        let mcp = aids.tools;
        // Held for the round: dropping it ends the listener and every call it
        // still holds. Codex is handed the address itself and never reads the
        // config file.
        let local = match (&mcp, sandbox.placement()) {
            (Some(tools), None) => Some(tools.serve_local().await?),
            _ => None,
        };
        let prepared = self
            .prepare(
                sandbox,
                round,
                home,
                &credential,
                aids.gh_token,
                mcp.as_ref(),
                local.as_ref(),
                &aids.images,
            )
            .await?;
        let Prepared {
            home,
            model,
            invocation,
            keeps_home,
            resumed,
            used_before,
            lost,
        } = prepared;
        if lost {
            if keeps_home {
                home.close().await;
            }
            return Ok(Driven {
                result: CliRoundResult {
                    engine: self.label().to_string(),
                    model: Some(model.clone()),
                    text: "the thread this round resumes is gone".to_string(),
                    exit_code: 1,
                    completion: Completion::SessionNotFound,
                    ..CliRoundResult::default()
                },
                model,
                login: credential.into_login(),
            });
        }
        let start = Instant::now();
        let run = claude_code::run_cli(
            invocation,
            ThreadCapture::shaped(self.policy.shape(), used_before),
            cancel,
            progress,
        )
        .await;
        drop(local);
        let ran_to_end = run.is_ok();
        let outcome = async {
            let run = match run {
                Ok(run) => run,
                Err(cut) => {
                    let left = cut
                        .downcast_ref::<Interrupted>()
                        .filter(|_| keeps_home)
                        .and_then(|cut| Some((cut.thread.as_deref()?, used_before.plus(&cut.spent))));
                    if let Some((thread, used)) = left {
                        if let Err(e) = record_thread(&home, sandbox, thread, Some(used)).await {
                            warn!(target: "codex", error = %e, "the cut round's thread was not kept; the next round starts afresh");
                        }
                    }
                    return Err(cut);
                }
            };
            if let (true, Some(thread)) = (keeps_home, &run.capture.thread_id) {
                record_thread(&home, sandbox, thread, run.capture.used).await?;
            }
            let last_message = home.read(sandbox, LAST_MESSAGE_FILE).await?;
            Ok::<_, eyre::Report>(result_of(
                run,
                &model,
                resumed,
                last_message,
                round.timeout,
                start.elapsed(),
            ))
        }
        .await;
        if let Credential::Login { login, .. } = &credential {
            keep_round_refresh(&home, sandbox, &login.slot).await;
        }
        match keeps_home {
            false => home.discard(sandbox).await,
            true if ran_to_end => home.close().await,
            true => {}
        }
        let result = CliRoundResult {
            engine: self.label().to_string(),
            job: round.jobs.as_ref().map(crate::claude_code::RoundJobs::job),
            ..outcome?
        };
        Ok(Driven {
            result,
            model,
            login: credential.into_login(),
        })
    }
}

/// A round driven to its end: its result, unpriced, the model it ran on, and
/// the login it ran on as it was handed, refreshed for the round.
pub(crate) struct Driven {
    pub result: CliRoundResult,
    pub model: String,
    pub login: Option<Login>,
}

/// Keep the thread a round left in its home for the next round to resume,
/// with what the thread had used in all by then when that is known.
async fn record_thread(
    home: &RoundHome,
    sandbox: &dyn CodingSandbox,
    thread: &str,
    used: Option<Usage>,
) -> Result<()> {
    home.write(sandbox, THREAD_FILE, thread).await?;
    match used {
        Some(used) => {
            let kept = ThreadUsage {
                thread: thread.to_string(),
                used,
            };
            home.write(sandbox, USAGE_FILE, &serde_json::to_string(&kept)?)
                .await
        }
        None => Ok(()),
    }
}

/// What a round draws on besides its credential: the refresher its login
/// renews through, a GitHub token for its shells, and the hub tools it is
/// served.
pub(crate) struct RoundAids<'a> {
    pub refresher: &'a TokenRefresher,
    pub gh_token: Option<String>,
    pub tools: Option<RoundTools>,
    pub images: Vec<PathBuf>,
}

/// `auth`, refreshed first when it would expire before a round of `timeout`
/// could end. A refresh the token endpoint cannot make now leaves the login
/// as it is; one it refuses ends the round.
async fn login_for_round(
    refresher: &TokenRefresher,
    login: &Login,
    auth: &AuthJson,
    timeout: Duration,
) -> Result<AuthJson> {
    let slot = &login.slot;
    let horizon = chrono::TimeDelta::from_std(timeout)
        .ok()
        .and_then(|timeout| timeout.checked_add(&ROUND_LOGIN_MARGIN))
        .unwrap_or(chrono::TimeDelta::MAX);
    match refresher.fresh(slot, auth, horizon).await {
        Ok(fresh) => Ok(fresh),
        Err(RefreshError::Transient(reason)) => {
            warn!(target: "codex", slot = %slot.key, %reason, "Codex login not refreshed before the round");
            Ok(auth.clone())
        }
        Err(refused) => {
            login.refreshed(auth.clone()).mark_refused();
            Err(eyre!("the Codex login cannot be used: {refused}"))
        }
    }
}

/// The login a round in `home` runs on, and whether the home already holds
/// it. A refresh a CLI left in a session's home is kept in its slot first,
/// whichever round left it. A round resuming its recorded job leaves the
/// home's login as the job holds it while the slot holds the same; any other
/// round is handed the slot's login, refreshed first when needed.
async fn round_login(
    refresher: &TokenRefresher,
    home: &RoundHome,
    sandbox: &dyn CodingSandbox,
    round: &CodingRound,
    login: &Login,
) -> Result<(AuthJson, bool)> {
    let slot = &login.slot;
    let kept = match round.continuity {
        Continuity::Session { .. } => keep_round_refresh(home, sandbox, slot).await,
        Continuity::OneShot => None,
    };
    match kept {
        Some(held) if round.collecting() => Ok((held, true)),
        kept => {
            let seen = match kept {
                Some(kept) => kept,
                None => sign_in(login)?.clone(),
            };
            Ok((
                login_for_round(refresher, login, &seen, round.timeout).await?,
                false,
            ))
        }
    }
}

/// Keep a refresh the CLI made in `home` in the slot its login came from,
/// before the home is gone or handed another login. Returns the home's login
/// when the slot now holds it.
async fn keep_round_refresh(
    home: &RoundHome,
    sandbox: &dyn CodingSandbox,
    slot: &VaultSlot,
) -> Option<AuthJson> {
    let kept = async {
        let (handed, after) = tokio::try_join!(
            home.read(sandbox, HANDED_FILE),
            home.read(sandbox, AUTH_FILE)
        )?;
        let (Some(handed), Some(after)) = (handed, after) else {
            return Ok(None);
        };
        let after = AuthJson::parse(after.trim()).map_err(|e| eyre!(e))?;
        let fingerprint = after.fingerprint();
        if write_back(slot, handed.trim(), &after).await?.fingerprint() != fingerprint {
            return Ok(None);
        }
        if handed.trim() != fingerprint {
            home.write(sandbox, HANDED_FILE, &fingerprint).await?;
        }
        Ok::<_, eyre::Report>(Some(after))
    }
    .await;
    kept.unwrap_or_else(|e| {
        warn!(target: "codex", slot = %slot.key, error = %e, "Codex login in the round's home not kept");
        None
    })
}

#[async_trait]
impl CodingEngine for CodexEngine {
    fn label(&self) -> &'static str {
        self.auth.label()
    }

    fn metered(&self) -> bool {
        self.auth == CodexAuth::ApiKey
    }

    fn preflight(&self, ctx: &RoundContext<'_>) -> Result<(), Refusal> {
        self.credential(ctx).map(drop)
    }

    fn limit_reached(&self, text: &str) -> bool {
        codex_limit_reached(text)
    }

    fn model_choices(&self) -> ModelChoices {
        ModelChoices {
            models: self.config.model_aliases().into_keys().collect(),
            efforts: CODEX_EFFORTS,
            raw_id_prefixes: &["gpt-"],
        }
    }

    fn model_for(&self, requested: Option<&str>, _: &RoundContext<'_>) -> Option<String> {
        Some(self.model(requested))
    }

    async fn cli_health(&self) -> Option<CliHealth> {
        Some(probe_cli().await)
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let started_at = chrono::Utc::now();
        self.run_round(sandbox, round, ctx, started_at, cancel, progress)
            .await
            .inspect_err(|e| ctx.record_error(self, round, started_at, e))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::claude_code::tests::scripted::{print, ScriptedCli};
    use crate::subscription::CooldownStore;
    use std::sync::Mutex;

    tokio::task_local! {
        /// The `codex` a test's rounds run instead of the real CLI.
        pub(crate) static CODEX: PathBuf;
    }

    /// A scripted `codex`. Each run records besides the `config.toml`, login
    /// and instructions its home was given, leaves the login at `refreshed` in its home
    /// when one is scripted, as a CLI that refreshed would, and answers with
    /// a completed thread unless scripted otherwise.
    pub(crate) struct ScriptedCodex(ScriptedCli);

    impl std::ops::Deref for ScriptedCodex {
        type Target = ScriptedCli;

        fn deref(&self) -> &ScriptedCli {
            &self.0
        }
    }

    pub(crate) const SCRIPTED_ANSWER: &str = "the codex answer";

    impl ScriptedCodex {
        pub(crate) fn new() -> Self {
            Self(ScriptedCli::new(
                "codex",
                r#"cp "$CODEX_HOME/config.toml" "$dir/$n.config"
        cp "$CODEX_HOME/auth.json" "$dir/$n.auth"
        [ -f "$CODEX_HOME/instructions.md" ] && cp "$CODEX_HOME/instructions.md" "$dir/$n.instructions"
        [ -f "$dir/refreshed" ] && cp "$dir/refreshed" "$CODEX_HOME/auth.json"
        out=$(awk 'prev=="-o"{print} {prev=$0}' "$dir/$n.args")"#,
                &thread(
                    SCRIPTED_ANSWER,
                    &[message_event(SCRIPTED_ANSWER), completed_event(120, 20, 7)],
                ),
            ))
        }

        /// The login the next runs leave in their homes.
        pub(crate) fn refreshes_to(&self, login: &str) {
            std::fs::write(self.program().with_file_name("refreshed"), login).unwrap();
        }

        /// Run `round` (every run without one of its own when `None`) starts
        /// a thread, writes `last` as its last message and prints `events`.
        pub(crate) fn reply(&self, round: Option<usize>, last: &str, events: &[serde_json::Value]) {
            self.script(round, &thread(last, events));
        }
    }

    /// A run that starts thread `thr_1` and a turn in it, leaves `last` as its
    /// last message and prints `events`.
    pub(crate) fn thread(last: &str, events: &[serde_json::Value]) -> String {
        format!(
            "printf '%s' '{}' > \"$out\"\n{}",
            last.replace('\'', "'\\''"),
            print(
                &[
                    serde_json::json!({"type": "thread.started", "thread_id": "thr_1"}),
                    serde_json::json!({"type": "turn.started"}),
                ]
                .into_iter()
                .chain(events.iter().cloned())
                .collect::<Vec<_>>()
            )
        )
    }

    /// A run that calls hub tool `tool` with `arguments` over the MCP server
    /// its config names, keeping the reply as `mcp`, then does `then`.
    pub(crate) fn calling(tool: &str, arguments: serde_json::Value, then: &str) -> String {
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        format!(
            r#"url=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$CODEX_HOME/config.toml")
auth=$(sed -n 's/^Authorization = "\(.*\)"$/\1/p' "$CODEX_HOME/config.toml")
curl -s -X POST "$url" -H "Authorization: $auth" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data '{call}' > "$dir/$n.mcp"
{then}"#
        )
    }

    pub(crate) fn message_event(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "item.completed",
            "item": { "id": "m", "type": "agent_message", "text": text },
        })
    }

    /// The thread's usage in all, as `turn.completed` reports it.
    pub(crate) fn completed_event(input: u64, cached: u64, output: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "turn.completed",
            "usage": { "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output },
        })
    }

    pub(crate) fn failed_event(message: &str) -> serde_json::Value {
        serde_json::json!({ "type": "turn.failed", "error": { "message": message } })
    }

    use crate::claude_code::Ended;
    use crate::coding_engine::MemoryLoad;
    use crate::coding_sandbox::{DirEntry, ExecOutcome};
    use crate::projects::setup::SetupReport;
    use crate::subscription::test_support::jwt;
    use chrono::{TimeDelta, Utc};

    fn event(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    fn capture_of(lines: &[&str]) -> (ThreadCapture, Vec<ProgressEvent>) {
        let mut capture = ThreadCapture::default();
        let progress = lines
            .iter()
            .flat_map(|line| capture.record(&event(line)))
            .collect();
        (capture, progress)
    }

    const STARTED: &str = r#"{"type":"thread.started","thread_id":"thr_1"}"#;
    const TURN: &str = r#"{"type":"turn.started"}"#;
    const COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122}}"#;

    #[tokio::test]
    async fn a_dispatched_codex_thread_past_a_megabyte_still_completes() {
        use crate::claude_code::tests::{dispatched, stream_script};
        let output = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "aggregated_output": "x".repeat(1_000) },
        })
        .to_string();
        let message =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"all done"}}"#;
        let run = dispatched(
            &stream_script(&[STARTED, TURN], &output, 2_000, &[message, COMPLETED]),
            ThreadCapture::default(),
        )
        .await;

        assert_eq!(run.exit_code, Some(0));
        assert!(run.capture.finished());
        assert_eq!(run.capture.thread_id.as_deref(), Some("thr_1"));
        assert_eq!(run.capture.message.as_deref(), Some("all done"));
        assert_eq!(
            run.capture.usage(),
            Usage {
                input: 24_763,
                cached: 24_448,
                cache_write: 0,
                output: 122,
            }
        );
    }

    fn message(text: &str) -> String {
        format!(
            r#"{{"type":"item.completed","item":{{"id":"m","type":"agent_message","text":{}}}}}"#,
            serde_json::Value::String(text.into())
        )
    }

    #[test]
    fn a_successful_thread_keeps_its_id_last_message_and_usage() {
        let first = message("draft");
        let last = message("final answer");
        let (capture, progress) = capture_of(&[STARTED, TURN, &first, &last, COMPLETED]);
        assert_eq!(capture.thread_id.as_deref(), Some("thr_1"));
        assert_eq!(capture.message.as_deref(), Some("final answer"));
        assert_eq!(
            capture.usage(),
            Usage {
                input: 24763,
                cached: 24448,
                cache_write: 0,
                output: 122
            }
        );
        assert!(capture.finished() && capture.failure.is_none());
        assert_eq!(capture.turns, 1);
        assert!(matches!(progress[0], ProgressEvent::LlmCall { round: 1 }));
        assert!(matches!(
            progress.last(),
            Some(ProgressEvent::Done {
                input_tokens: 315,
                output_tokens: 122,
                ..
            })
        ));
    }

    #[test]
    fn a_failed_turn_and_a_fatal_error_both_finish_the_stream_with_the_message() {
        let failed = r#"{"type":"turn.failed","error":{"message":"You’ve hit your usage limit for gpt-6-astra."}}"#;
        let (capture, _) = capture_of(&[STARTED, TURN, failed]);
        assert!(capture.finished());
        assert!(capture
            .failure
            .as_deref()
            .unwrap()
            .contains("hit your usage limit"));

        let fatal = r#"{"type":"error","message":"stream disconnected"}"#;
        let (capture, _) = capture_of(&[STARTED, fatal]);
        assert!(capture.finished());
        assert_eq!(capture.failure.as_deref(), Some("stream disconnected"));

        let (unfinished, _) = capture_of(&[STARTED, TURN]);
        assert!(!unfinished.finished());
    }

    #[test]
    fn tool_items_become_progress_and_a_non_fatal_error_item_does_not_fail_the_round() {
        let shell = r#"{"type":"item.started","item":{"id":"c","type":"command_execution","command":"bundle exec rspec","status":"in_progress"}}"#;
        let mcp = r#"{"type":"item.started","item":{"id":"t","type":"mcp_tool_call","server":"workforce","tool":"datadog_logs_search","status":"in_progress"}}"#;
        let oops =
            r#"{"type":"item.completed","item":{"id":"e","type":"error","message":"transient"}}"#;
        let (capture, progress) = capture_of(&[STARTED, TURN, shell, mcp, oops, COMPLETED]);
        assert!(capture.failure.is_none());
        let names: Vec<_> = progress
            .iter()
            .filter_map(|p| match p {
                ProgressEvent::ToolStart { name, detail, .. } => {
                    Some((name.clone(), detail.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("codex:shell".to_string(), "bundle exec rspec".to_string()),
                (
                    "codex:workforce__datadog_logs_search".to_string(),
                    String::new()
                ),
            ]
        );
    }

    fn run_with(
        capture: ThreadCapture,
        exit_code: Option<i32>,
        ended: Ended,
    ) -> CapturedRun<ThreadCapture> {
        CapturedRun {
            exit_code,
            ended,
            capture,
            stderr: b"warn: something\n".to_vec(),
        }
    }

    #[test]
    fn a_completed_turn_is_a_success_priced_from_uncached_input() {
        let (capture, _) = capture_of(&[STARTED, TURN, &message("done"), COMPLETED]);
        let result = result_of(
            run_with(capture, Some(0), Ended::Exited),
            "gpt-5.6-sol",
            false,
            Some("the whole message\n".into()),
            Duration::from_secs(60),
            Duration::from_secs(3),
        );
        assert_eq!(result.completion, Completion::Success);
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            result.text, "the whole message\n",
            "the -o file wins over the stream's last item"
        );
        assert_eq!(result.input_tokens, 24763 - 24448);
        assert_eq!(result.cache_read_tokens, 24448);
        assert_eq!(result.output_tokens, 122);
        assert_eq!(result.num_turns, 1);
        assert!(result.crash_kind.is_none() && result.stderr_tail.is_empty());
        assert_eq!(result.model.as_deref(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn a_completed_turn_whose_process_lingered_is_still_a_success() {
        let (capture, _) = capture_of(&[STARTED, TURN, &message("done"), COMPLETED]);
        let result = result_of(
            run_with(capture, None, Ended::AfterResult),
            "gpt-6-astra",
            false,
            None,
            Duration::from_secs(60),
            Duration::from_secs(3),
        );
        assert_eq!(result.completion, Completion::Success);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.text, "done");
    }

    #[test]
    fn a_failed_turn_carries_its_message_and_a_timeout_salvages_the_last_one() {
        let failed = r#"{"type":"turn.failed","error":{"message":"Quota exceeded. Check your plan and billing details."}}"#;
        let (capture, _) = capture_of(&[STARTED, TURN, failed]);
        let result = result_of(
            run_with(capture, Some(1), Ended::Exited),
            "gpt-6-astra",
            false,
            None,
            Duration::from_secs(60),
            Duration::from_secs(3),
        );
        assert_eq!(result.completion, Completion::Failed);
        assert_eq!(result.exit_code, 1);
        assert!(result.text.starts_with("Quota exceeded"));
        assert!(codex_limit_reached(&result.text));
        assert!(!result.stderr_tail.is_empty());

        let (capture, _) = capture_of(&[STARTED, TURN, &message("half way")]);
        let result = result_of(
            run_with(capture, Some(137), Ended::WallClock),
            "gpt-6-astra",
            false,
            None,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert_eq!(result.completion, Completion::Timeout);
        assert_eq!(
            result.exit_code, -1,
            "the hub's own kill is not the round's exit"
        );
        assert_eq!(result.text, "half way");
        assert!(matches!(
            result.crash_kind,
            Some(crate::claude_code::CrashKind::TimedOut { after_secs: 60 })
        ));

        let (capture, _) = capture_of(&[STARTED]);
        let result = result_of(
            run_with(capture, Some(137), Ended::WallClock),
            "gpt-6-astra",
            false,
            None,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert_eq!(
            result.completion,
            Completion::Failed,
            "nothing salvaged is nothing usable"
        );

        let (capture, _) = capture_of(&[STARTED, TURN]);
        let result = result_of(
            run_with(capture, Some(0), Ended::Exited),
            "gpt-6-astra",
            false,
            None,
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        assert_eq!(
            result.completion,
            Completion::Failed,
            "a clean exit that said nothing and finished no turn is not a result"
        );
    }

    #[test]
    fn a_resume_that_never_started_its_thread_has_lost_the_session() {
        let gone = r#"{"type":"error","message":"no rollout found for thread thr_old"}"#;
        let (capture, _) = capture_of(&[gone]);
        let lost = result_of(
            run_with(capture, Some(1), Ended::Exited),
            "gpt-5.6-sol",
            true,
            None,
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        assert_eq!(lost.completion, Completion::SessionNotFound);
        let silent = result_of(
            run_with(ThreadCapture::default(), Some(1), Ended::Exited),
            "gpt-5.6-sol",
            true,
            None,
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        assert_eq!(silent.completion, Completion::SessionNotFound);

        let failed = r#"{"type":"turn.failed","error":{"message":"boom"}}"#;
        let (capture, _) = capture_of(&[STARTED, TURN, failed]);
        let after_start = result_of(
            run_with(capture, Some(1), Ended::Exited),
            "gpt-5.6-sol",
            true,
            None,
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        assert_eq!(
            after_start.completion,
            Completion::Failed,
            "a thread that started and then failed was not lost"
        );
    }

    #[test]
    fn limit_messages_are_recognised_and_ordinary_failures_are_not() {
        for text in [
            "You’ve hit your usage limit for gpt-6-astra. Switch to another model now.",
            "Quota exceeded. Check your plan and billing details.",
            "rate limit exceeded: retry later",
        ] {
            assert!(codex_limit_reached(text), "{text}");
        }
        assert!(!codex_limit_reached("segfault"));
        assert!(!codex_limit_reached("the spec failed"));
    }

    /// The hub's tools as Codex 0.154.0 lists them to the model once a
    /// `tool_search` finds them, trimmed to what names them.
    const RECORDED_TOOL_LISTING: &str = r#"[{"type":"namespace","name":"mcp__workforce","description":"Tools in the mcp__workforce namespace.","tools":[{"type":"function","name":"jira_get_issue","description":"Get a Jira issue","strict":false,"defer_loading":true,"parameters":{"type":"object","properties":{"key":{"type":"string"}}}}]}]"#;

    #[test]
    fn codex_tool_names_match_a_recorded_tool_listing() {
        let listing: serde_json::Value = serde_json::from_str(RECORDED_TOOL_LISTING).unwrap();
        let namespace = &listing[0];
        assert_eq!(namespace["name"].as_str(), Some(tool_namespace().as_str()));
        let listed: Vec<&str> = namespace["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        let hub_name = crate::mcp::normalise_tool_name("jira.get_issue");
        assert!(
            listed.contains(&hub_name.as_str()),
            "a.b is a_b: {listed:?}"
        );
        assert_eq!(
            render_for_codex("call mcp__workforce__jira_get_issue, leave mcp__other__x"),
            "call jira_get_issue, leave mcp__other__x"
        );
        assert!(listed.contains(&render_for_codex("mcp__workforce__jira_get_issue").as_str()));
        assert!(
            tool_naming().contains(&format!("`{}` namespace", tool_namespace())),
            "{}",
            tool_naming()
        );
    }

    #[test]
    fn prompts_are_respelled_and_the_system_prompt_placed_by_policy() {
        let round = CodingRound {
            system_prompt: Some("Be brief. Call mcp__workforce__jira_get_issue first.".into()),
            ..CodingRound::one_shot("Fix mcp__workforce__x".into(), Duration::from_secs(1))
        };
        assert_eq!(
            prompts_for(CodexPolicy::Coding, &round),
            (
                "# Instructions\n\nBe brief. Call jira_get_issue first.\n\nFix x".to_string(),
                None
            ),
            "a coding round's prompt leads its task"
        );
        assert_eq!(
            prompts_for(CodexPolicy::Contained, &round),
            (
                "Fix x".to_string(),
                Some("Be brief. Call jira_get_issue first.".to_string())
            ),
            "a contained round's replaces Codex's own instructions"
        );
        let bare = CodingRound::one_shot("just this".into(), Duration::from_secs(1));
        for policy in [CodexPolicy::Coding, CodexPolicy::Contained] {
            assert_eq!(prompts_for(policy, &bare), ("just this".to_string(), None));
        }
    }

    #[test]
    fn the_config_file_carries_the_policies_and_only_a_local_mcp_block() {
        let with_mcp = render_config(
            CodexPolicy::Coding,
            "gpt-6-astra",
            Some("xhigh"),
            Some(CodexMcpServer::http(
                "http://127.0.0.1:4321/mcp",
                "tok-1",
                300,
            )),
            None,
        )
        .unwrap();
        for line in [
            r#"model = "gpt-6-astra""#,
            r#"model_reasoning_effort = "xhigh""#,
            r#"approval_policy = "never""#,
            r#"sandbox_mode = "danger-full-access""#,
            r#"cli_auth_credentials_store = "file""#,
            r#"persistence = "none""#,
            r#"inherit = "all""#,
            "ignore_default_excludes = true",
            "[mcp_servers.workforce]",
            r#"url = "http://127.0.0.1:4321/mcp""#,
            r#"Authorization = "Bearer tok-1""#,
        ] {
            assert!(with_mcp.contains(line), "{line} missing from:\n{with_mcp}");
        }
        let parsed: toml::Table = with_mcp.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["workforce"]["tool_timeout_sec"].as_integer(),
            Some(300)
        );

        let without = render_config(CodexPolicy::Coding, "gpt-5.6-sol", None, None, None).unwrap();
        assert!(!without.contains("mcp_servers"), "{without}");
        assert!(!without.contains("model_reasoning_effort"), "{without}");
        let appended = format!(
            "{without}{}",
            CodexMcpServer::http("http://h:1/mcp", "t", 60).toml_block("workforce")
        );
        let parsed: toml::Table = appended.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["workforce"]["url"].as_str(),
            Some("http://h:1/mcp")
        );
    }

    /// Every default alias is priced under both labels a Codex round can be
    /// recorded with; the CLI reports no cost, so these rows are the whole of
    /// it.
    #[test]
    fn every_default_alias_is_priced_under_both_codex_labels() {
        let pricing =
            workforce_common::cost::load_pricing_toml(workforce_agent::llm::DEFAULT_PRICING_TOML)
                .unwrap();
        let config = CodingCodexConfig::default();
        for (_, id) in config.model_aliases() {
            for label in [LABEL, API_LABEL] {
                let key = format!("{label}:{id}");
                let row = pricing
                    .get(&key)
                    .unwrap_or_else(|| panic!("{key} has no pricing row"));
                assert!(
                    row.input_per_million > 0.0 && row.output_per_million > row.input_per_million,
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn model_choices_are_the_aliases_and_codex_efforts() {
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        let choices = engine.model_choices();
        assert_eq!(choices.models, vec!["astra", "luna", "sol", "terra"]);
        assert_eq!(choices.efforts, CODEX_EFFORTS);
        assert_eq!(engine.label(), LABEL);
        assert!(!engine.metered());
        let metered = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default());
        assert_eq!(metered.label(), API_LABEL);
        assert!(metered.metered());
    }

    /// A ChatGPT login whose access token expires at `expires`, stored at
    /// the agent's shared key of a vault of its own.
    fn signed_in(expires: chrono::DateTime<Utc>) -> (Credential, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("vault.enc"), "test-key").unwrap());
        let text = serde_json::json!({
            "tokens": {
                "id_token": jwt(serde_json::json!({"sub": "user-1"})),
                "access_token": jwt(serde_json::json!({"exp": expires.timestamp()})),
                "refresh_token": "rt-0",
                "account_id": "acct"
            }
        })
        .to_string();
        vault.set(AUTH_VAULT_KEY, &text).unwrap();
        let credential = Credential::Login {
            login: Box::new(login_at(&VaultSlot {
                vault,
                key: AUTH_VAULT_KEY.into(),
            })),
            held: false,
        };
        (credential, dir)
    }

    /// The agent's shared ChatGPT login `slot` holds.
    fn login_at(slot: &VaultSlot) -> Login {
        let auth = stored(slot);
        Login {
            kind: SubscriptionKind::ChatGpt,
            tier: workforce_common::subscription::CredentialTier::AgentShared,
            owner: workforce_common::subscription::LoginOwner::Agent(
                workforce_common::subscription::AgentOwner::Unowned,
            ),
            key: crate::subscription::LoginKey::of_chatgpt(&auth).unwrap(),
            credential: crate::subscription::Credential::ChatGpt(auth),
            slot: slot.clone(),
        }
    }

    fn login_of(credential: &Credential) -> (&Login, &AuthJson, &VaultSlot) {
        match credential {
            Credential::Login { login, .. } => (login, sign_in(login).unwrap(), &login.slot),
            Credential::ApiKey(_) => unreachable!("a login"),
        }
    }

    fn stored(slot: &VaultSlot) -> AuthJson {
        AuthJson::parse(&slot.vault.get(&slot.key).unwrap()).unwrap()
    }

    async fn token_endpoint() -> (wiremock::MockServer, TokenRefresher) {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": jwt(serde_json::json!({"exp": (Utc::now() + TimeDelta::hours(1)).timestamp()})),
                "refresh_token": "rt-1",
            })))
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        (server, refresher)
    }

    #[tokio::test]
    async fn a_login_that_would_expire_during_the_round_is_refreshed_first() {
        let (server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::minutes(10));
        let (login, auth, slot) = login_of(&credential);

        let short = login_for_round(&refresher, login, auth, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            short.refresh_token(),
            Some("rt-0"),
            "valid past a short round"
        );
        assert!(server.received_requests().await.unwrap().is_empty());

        let long = login_for_round(&refresher, login, auth, Duration::from_secs(30 * 60))
            .await
            .unwrap();
        assert_eq!(long.refresh_token(), Some("rt-1"));
        assert_eq!(stored(slot).refresh_token(), Some("rt-1"));
    }

    #[tokio::test]
    async fn a_refused_refresh_marks_the_login_dead() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::minutes(10));
        let (login, auth, slot) = login_of(&credential);

        let refused = login_for_round(&refresher, login, auth, Duration::from_secs(30 * 60))
            .await
            .unwrap_err();

        assert!(
            refused.to_string().contains("cannot be used"),
            "{refused:#}"
        );
        assert!(
            CooldownStore::global().is_dead(slot, &auth.fingerprint()),
            "every route resolving the login skips it until it is signed in again"
        );
    }

    /// `auth` as the CLI leaves it after refreshing to `refresh_token`, with
    /// an access token that expires at `expires`.
    fn refreshed(auth: &AuthJson, refresh_token: &str, expires: chrono::DateTime<Utc>) -> String {
        let mut auth = auth.clone();
        let tokens = auth.tokens.as_mut().unwrap();
        tokens.refresh_token = refresh_token.into();
        tokens.access_token = jwt(serde_json::json!({"exp": expires.timestamp()}));
        auth.render()
    }

    /// A keyed session round on this host of a 30-minute budget, and the same
    /// round collected at the job it recorded.
    fn keyed_rounds() -> (CodingRound, CodingRound) {
        let id = Uuid::new_v4();
        let started = Utc::now();
        let round = |jobs| CodingRound {
            continuity: Continuity::Session { id, resume: true },
            jobs: Some(jobs),
            ..CodingRound::one_shot("t".into(), Duration::from_secs(30 * 60))
        };
        (
            round(crate::claude_code::RoundJobs::new("r1".into(), started)),
            round(crate::claude_code::RoundJobs::resuming("r1", started, "r1.0.0").unwrap()),
        )
    }

    /// What `run_round` does before the CLI starts: settle `round`'s login
    /// from the slot and the home, and write the home. Returns the login the
    /// round runs on, whether the home already held it, and the home.
    async fn hand(
        engine: &CodexEngine,
        refresher: &TokenRefresher,
        sandbox: &RecordingSandbox,
        round: &CodingRound,
        slot: &VaultSlot,
    ) -> (AuthJson, bool, RoundHome) {
        let home = engine.home(sandbox, round);
        let login = login_at(slot);
        let (auth, held) = round_login(refresher, &home, sandbox, round, &login)
            .await
            .unwrap();
        let credential = Credential::Login {
            login: Box::new(login.refreshed(auth.clone())),
            held,
        };
        let prepared = engine
            .prepare(sandbox, round, home, &credential, None, None, None, &[])
            .await
            .unwrap();
        (auth, held, prepared.home)
    }

    fn home_login(home: &RoundHome) -> AuthJson {
        AuthJson::parse(&std::fs::read_to_string(home.file(AUTH_FILE)).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_refresh_the_cli_made_during_the_round_is_kept_in_its_slot() {
        let (server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        let sandbox = RecordingSandbox::new(false);
        let round = CodingRound::one_shot("t".into(), Duration::from_secs(60));

        let (handed, held, home) = hand(&engine, &refresher, &sandbox, &round, slot).await;
        assert!(!held);
        assert_eq!(
            keep_round_refresh(&home, &sandbox, slot).await,
            Some(handed.clone())
        );
        assert_eq!(
            stored(slot).refresh_token(),
            Some("rt-0"),
            "nothing refreshed"
        );

        let expires = Utc::now() + TimeDelta::hours(1);
        write_shared(
            &home.file(AUTH_FILE),
            &refreshed(&handed, "rt-cli", expires),
        )
        .await
        .unwrap();
        keep_round_refresh(&home, &sandbox, slot).await;
        assert_eq!(stored(slot).refresh_token(), Some("rt-cli"));
        assert!(server.received_requests().await.unwrap().is_empty());
        home.discard(&sandbox).await;
    }

    #[tokio::test]
    async fn a_collected_round_leaves_the_login_its_job_refreshed_and_keeps_its_next_refresh() {
        let (server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let (first, collected) = keyed_rounds();

        let (handed, _, home) = hand(&engine, &refresher, &sandbox, &first, slot).await;
        let soon = Utc::now() + TimeDelta::minutes(2);
        write_shared(&home.file(AUTH_FILE), &refreshed(&handed, "rt-cli", soon))
            .await
            .unwrap();

        let (auth, held, home) = hand(&engine, &refresher, &sandbox, &collected, slot).await;
        assert!(held, "the job's home keeps its login");
        assert_eq!(auth.refresh_token(), Some("rt-cli"));
        assert_eq!(home_login(&home).refresh_token(), Some("rt-cli"));
        assert_eq!(stored(slot).refresh_token(), Some("rt-cli"));
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a login a live job holds is never refreshed from under it"
        );

        let later = Utc::now() + TimeDelta::hours(1);
        write_shared(&home.file(AUTH_FILE), &refreshed(&auth, "rt-cli-2", later))
            .await
            .unwrap();
        keep_round_refresh(&home, &sandbox, slot).await;
        assert_eq!(stored(slot).refresh_token(), Some("rt-cli-2"));
    }

    #[tokio::test]
    async fn a_collected_round_on_the_worker_leaves_its_job_s_login_in_the_scope() {
        let (server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        let sandbox = RecordingSandbox::new(true);
        let (first, collected) = keyed_rounds();

        let (handed, _, home) = hand(&engine, &refresher, &sandbox, &first, slot).await;
        let soon = Utc::now() + TimeDelta::minutes(2);
        home.write(&sandbox, AUTH_FILE, &refreshed(&handed, "rt-cli", soon))
            .await
            .unwrap();
        let writes = sandbox.writes.lock().unwrap().len();

        let (auth, held, _) = hand(&engine, &refresher, &sandbox, &collected, slot).await;
        assert!(held);
        assert_eq!(auth.refresh_token(), Some("rt-cli"));
        assert_eq!(stored(slot).refresh_token(), Some("rt-cli"));
        let rewritten: Vec<_> = sandbox.writes.lock().unwrap()[writes..]
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            !rewritten.contains(&AUTH_FILE.to_string()),
            "the job's login is left in the scope: {rewritten:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_collected_round_whose_slot_moved_on_is_handed_the_slot_s_login() {
        let (_server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let (first, collected) = keyed_rounds();

        let (handed, _, home) = hand(&engine, &refresher, &sandbox, &first, slot).await;
        let expires = Utc::now() + TimeDelta::hours(1);
        write_shared(
            &home.file(AUTH_FILE),
            &refreshed(&handed, "rt-cli", expires),
        )
        .await
        .unwrap();
        let sign_in = refreshed(&handed, "rt-new-sign-in", expires);
        slot.vault.set(&slot.key, &sign_in).unwrap();

        let (auth, held, home) = hand(&engine, &refresher, &sandbox, &collected, slot).await;
        assert!(!held);
        assert_eq!(auth.refresh_token(), Some("rt-new-sign-in"));
        assert_eq!(
            slot.vault.get(&slot.key).as_deref(),
            Some(sign_in.as_str()),
            "the round's stale login never replaces a newer sign-in"
        );
        assert_eq!(home_login(&home).refresh_token(), Some("rt-new-sign-in"));
        assert_eq!(
            std::fs::read_to_string(home.file(HANDED_FILE)).unwrap(),
            auth.fingerprint()
        );
    }

    #[tokio::test]
    async fn a_collected_round_on_a_fresh_home_is_handed_its_login() {
        let (_server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let (_, collected) = keyed_rounds();

        let (auth, held, home) = hand(&engine, &refresher, &sandbox, &collected, slot).await;
        assert!(!held);
        assert_eq!(auth.refresh_token(), Some("rt-0"));
        assert_eq!(home_login(&home), auth);
        assert_eq!(
            std::fs::read_to_string(home.file(HANDED_FILE)).unwrap(),
            auth.fingerprint()
        );
    }

    #[tokio::test]
    async fn a_session_round_keeps_what_an_ended_round_left_refreshed_before_replacing_it() {
        let (server, refresher) = token_endpoint().await;
        let (credential, _dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let (_, _, slot) = login_of(&credential);
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let (first, _) = keyed_rounds();

        let (handed, _, home) = hand(&engine, &refresher, &sandbox, &first, slot).await;
        let expires = Utc::now() + TimeDelta::hours(2);
        write_shared(
            &home.file(AUTH_FILE),
            &refreshed(&handed, "rt-cli", expires),
        )
        .await
        .unwrap();

        let (auth, held, home) = hand(&engine, &refresher, &sandbox, &first, slot).await;
        assert!(!held);
        assert_eq!(auth.refresh_token(), Some("rt-cli"));
        assert_eq!(stored(slot).refresh_token(), Some("rt-cli"));
        assert_eq!(home_login(&home).refresh_token(), Some("rt-cli"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A sandbox that records what a round writes into it.
    struct RecordingSandbox {
        remote: bool,
        fresh: bool,
        /// What reading a home's thread file gives.
        thread: Option<String>,
        writes: Mutex<Vec<(PathBuf, String)>>,
        /// Each command run in it, as its program and arguments.
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl RecordingSandbox {
        fn new(remote: bool) -> Self {
            Self {
                remote,
                fresh: true,
                thread: None,
                writes: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CodingSandbox for RecordingSandbox {
        fn workdir(&self) -> &Path {
            Path::new("/work/rails")
        }
        fn setup(&self) -> &[SetupReport] {
            &[]
        }
        fn hub_tool_patterns(&self) -> Vec<String> {
            vec!["jira.*".into()]
        }
        fn fresh(&self) -> bool {
            self.fresh
        }
        async fn exec(&self, command: SandboxCommand) -> Result<ExecOutcome> {
            self.commands.lock().unwrap().push(
                std::iter::once(command.program)
                    .chain(command.args)
                    .collect(),
            );
            Ok(ExecOutcome {
                stdout: String::new(),
                stderr: String::new(),
                summary: workforce_agent::tools::exec_runner::ExecSummary::exited(0),
            })
        }
        async fn read(&self, path: &Path, _: usize) -> Result<Option<String>> {
            let written = self
                .writes
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(at, _)| at == path)
                .map(|(_, content)| content.clone());
            Ok(written.or_else(|| self.thread.clone().filter(|_| path.ends_with(THREAD_FILE))))
        }
        async fn write(&self, path: &Path, content: &str) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((path.to_path_buf(), content.to_string()));
            Ok(())
        }
        async fn list(&self, _: &Path) -> Result<Vec<DirEntry>> {
            unreachable!()
        }
        fn placement(&self) -> Option<crate::claude_code::RemotePlacement> {
            self.remote.then(|| crate::claude_code::RemotePlacement {
                endpoint: workforce_agent::tools::exec_runner::RunnerEndpoint::Unix(PathBuf::from(
                    "/run/exec.sock",
                )),
                scope: workforce_agent::tools::exec_runner::ScopeId::new("scope-1"),
                jobs: true,
            })
        }
    }

    fn env_of(cmd: &Command) -> BTreeMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_shared_write_replaces_a_file_another_user_left_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home").join(CONFIG_FILE);
        write_shared(&path, "model = \"sol\"\n").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        write_shared(&path, "model = \"astra\"\n").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"astra\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o660
            );
        }
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != CONFIG_FILE)
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging files left behind: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn a_remote_session_round_writes_its_home_and_keeps_the_login_out_of_the_environment() {
        let sandbox = RecordingSandbox::new(true);
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        let (credential, _vault_dir) = signed_in(Utc::now() + TimeDelta::hours(1));
        let secret = login_of(&credential).1.access_token().unwrap().to_string();
        let id = Uuid::new_v4();
        let round = CodingRound {
            model: Some("astra".into()),
            effort: Some("high".into()),
            system_prompt: Some("Be brief.".into()),
            continuity: Continuity::Session { id, resume: true },
            memory: MemoryLoad {
                claude_md: true,
                auto_memory: false,
            },
            ..CodingRound::one_shot("fix mcp__workforce__x".into(), Duration::from_secs(90))
        };
        let (_service, tools) =
            crate::mcp_turn::testing::round_tools(&["jira.*"], engine.client_timeout()).await;
        let prepared = engine
            .prepare(
                &sandbox,
                &round,
                engine.home(&sandbox, &round),
                &credential,
                Some("ghp_1".into()),
                Some(&tools),
                None,
                &[],
            )
            .await
            .unwrap();

        let home = PathBuf::from(format!("/home/agent/.codex-{id}"));
        assert_eq!(prepared.home, RoundHome::Remote(home.clone()));
        assert!(prepared.keeps_home);
        assert_eq!(prepared.model, "gpt-6-astra");
        let writes = sandbox.writes.lock().unwrap();
        let written: BTreeMap<_, _> = writes.iter().cloned().collect();
        assert!(written[&home.join(AUTH_FILE)].contains(&secret));
        let (_, auth, _) = login_of(&credential);
        assert_eq!(written[&home.join(HANDED_FILE)], auth.fingerprint());
        assert!(
            [auth.access_token(), auth.refresh_token()]
                .into_iter()
                .flatten()
                .all(|token| !written[&home.join(HANDED_FILE)].contains(token)),
            "the handed file holds no token"
        );
        let config = &written[&home.join(CONFIG_FILE)];
        assert!(
            config.contains(r#"model = "gpt-6-astra""#)
                && config.contains(r#"model_reasoning_effort = "high""#)
        );
        assert!(
            !config.contains("mcp_servers"),
            "the worker appends the MCP block: {config}"
        );
        assert_eq!(
            prepared.invocation.codex_mcp_config,
            Some(CodexMcpConfig {
                path: home.join(CONFIG_FILE).display().to_string(),
                tool_timeout_secs: 600,
            })
        );
        assert!(
            prepared.invocation.mcp.is_some(),
            "a remote round's calls are answered over the tunnel"
        );

        let env = env_of(&prepared.invocation.command);
        assert_eq!(env["CODEX_HOME"].as_deref(), Some(home.to_str().unwrap()));
        assert_eq!(env["TZ"].as_deref(), Some("UTC"));
        assert_eq!(env["GH_TOKEN"].as_deref(), Some("ghp_1"));
        assert_eq!(env["OPENAI_API_KEY"], None, "removed on the child");
        for var in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            assert_eq!(env[var], None, "{var} is scrubbed from a Codex round");
        }
        assert!(env.values().flatten().all(|v| !v.contains(&secret)));
        assert!(
            env.contains_key("GIT_CONFIG_KEY_0"),
            "a remote scope gets the credential helper"
        );
        assert!(
            !env.contains_key("PATH"),
            "a remote scope brings its own toolchain"
        );

        let args = args_of(&prepared.invocation.command);
        assert_eq!(
            args[..2],
            ["exec".to_string(), "--json".into()],
            "a fresh home starts a new thread: {args:?}"
        );
        assert!(
            !args.contains(&"--ephemeral".to_string()),
            "a session keeps its rollout"
        );
        assert!(
            !args.contains(&"resume".to_string()),
            "a fresh workspace has no thread to resume: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert_eq!(
            prepared.invocation.stdin,
            "# Instructions\n\nBe brief.\n\nfix x"
        );
        assert!(prepared.invocation.remote.is_some());
        assert_eq!(prepared.invocation.timeout, Duration::from_secs(90));
    }

    #[tokio::test]
    async fn a_metered_one_shot_round_sets_the_key_and_is_ephemeral() {
        let sandbox = RecordingSandbox::new(true);
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default());
        let round = CodingRound::one_shot("t".into(), Duration::from_secs(5));
        let prepared = engine
            .prepare(
                &sandbox,
                &round,
                engine.home(&sandbox, &round),
                &Credential::ApiKey("sk-1".into()),
                None,
                None,
                None,
                &[],
            )
            .await
            .unwrap();
        assert!(!prepared.keeps_home);
        assert_eq!(prepared.model, "gpt-5.6-sol", "the default alias");
        let env = env_of(&prepared.invocation.command);
        assert_eq!(env["CODEX_API_KEY"].as_deref(), Some("sk-1"));
        assert!(!env.contains_key("GH_TOKEN"));
        assert!(args_of(&prepared.invocation.command).contains(&"--ephemeral".to_string()));
        let writes = sandbox.writes.lock().unwrap();
        assert!(
            writes.iter().all(|(p, _)| !p.ends_with(AUTH_FILE)),
            "no login file for a key"
        );
    }

    /// `codex exec resume` takes only some of `exec`'s options after it, and
    /// not `-C`: every option goes before `resume`, and only the thread and
    /// then the prompt follow it.
    #[tokio::test]
    async fn a_resumed_round_puts_every_exec_option_before_resume_and_the_prompt_last() {
        let sandbox = RecordingSandbox {
            fresh: false,
            thread: Some("thr_old\n".into()),
            ..RecordingSandbox::new(true)
        };
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        let round = CodingRound {
            continuity: Continuity::Session {
                id: Uuid::new_v4(),
                resume: true,
            },
            ..CodingRound::one_shot("carry on".into(), Duration::from_secs(5))
        };
        let prepared = engine
            .prepare(
                &sandbox,
                &round,
                engine.home(&sandbox, &round),
                &signed_in(Utc::now() + TimeDelta::hours(1)).0,
                None,
                None,
                None,
                &[],
            )
            .await
            .unwrap();
        assert!(prepared.resumed && prepared.keeps_home);

        let args = args_of(&prepared.invocation.command);
        assert_eq!(args[0], "exec", "{args:?}");
        let resume = args
            .iter()
            .position(|a| a == "resume")
            .unwrap_or_else(|| panic!("a resumed round names resume: {args:?}"));
        assert_eq!(
            args[resume + 1..],
            ["thr_old".to_string(), "-".into()],
            "only the thread and then the prompt follow resume: {args:?}"
        );
        let last_option = args[..args.len() - 1]
            .iter()
            .rposition(|a| a.starts_with('-'))
            .unwrap();
        assert!(last_option < resume, "an option after resume: {args:?}");
        let cd = args
            .iter()
            .position(|a| a == "-C")
            .unwrap_or_else(|| panic!("no -C: {args:?}"));
        assert!(cd < resume, "-C belongs to exec: {args:?}");
        assert_eq!(args[cd + 1], "/work/rails");
        assert!(
            !args.contains(&"--ephemeral".to_string()),
            "a session keeps its rollout"
        );
    }

    #[tokio::test]
    async fn a_local_session_keeps_its_home_under_the_data_dir_and_a_one_shot_under_the_temp_dir() {
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let credential = Credential::ApiKey("sk-1".into());

        let id = Uuid::new_v4();
        let session = CodingRound {
            continuity: Continuity::Session { id, resume: true },
            ..CodingRound::one_shot("t".into(), Duration::from_secs(5))
        };
        let prepared = engine
            .prepare(
                &sandbox,
                &session,
                engine.home(&sandbox, &session),
                &credential,
                None,
                None,
                None,
                &[],
            )
            .await
            .unwrap();
        let root = data.path().join("codex-homes");
        let dir = root.join(format!(".codex-{id}"));
        assert_eq!(
            prepared.home,
            RoundHome::Local {
                root: root.clone(),
                dir: dir.clone()
            }
        );
        assert!(prepared.keeps_home);
        assert!(dir.join(CONFIG_FILE).is_file(), "the config is in the home");
        let env = env_of(&prepared.invocation.command);
        assert_eq!(env["CODEX_HOME"].as_deref(), Some(dir.to_str().unwrap()));
        let args = args_of(&prepared.invocation.command);
        assert!(
            args.contains(&dir.join(LAST_MESSAGE_FILE).display().to_string()),
            "{args:?}"
        );
        assert!(
            sandbox.writes.lock().unwrap().is_empty(),
            "a local home is written on this host, not through the sandbox"
        );

        let one_shot = CodingRound::one_shot("t".into(), Duration::from_secs(5));
        let prepared = engine
            .prepare(
                &sandbox,
                &one_shot,
                engine.home(&sandbox, &one_shot),
                &credential,
                None,
                None,
                None,
                &[],
            )
            .await
            .unwrap();
        let RoundHome::Local {
            root: temp_root,
            dir,
        } = &prepared.home
        else {
            panic!("a local round has a local home: {:?}", prepared.home);
        };
        assert_eq!(*temp_root, local_home_root());
        assert!(
            dir.starts_with(local_home_root()) && !dir.starts_with(&root),
            "{}",
            dir.display()
        );
        assert!(dir.join(CONFIG_FILE).is_file());
        prepared.home.discard(&sandbox).await;
        assert!(!dir.exists(), "a one-shot home goes with its round");
    }

    /// A round on this host prepared with a loopback listener, and the
    /// `config.toml` it wrote.
    async fn local_round_config(engine: &CodexEngine) -> (String, crate::mcp_turn::LocalMcp) {
        let sandbox = RecordingSandbox::new(false);
        let (_service, tools) =
            crate::mcp_turn::testing::round_tools(&["jira.*"], engine.client_timeout()).await;
        let local = tools.serve_local().await.unwrap();
        let round = CodingRound {
            continuity: Continuity::Session {
                id: Uuid::new_v4(),
                resume: true,
            },
            ..CodingRound::one_shot("t".into(), Duration::from_secs(5))
        };
        let prepared = engine
            .prepare(
                &sandbox,
                &round,
                engine.home(&sandbox, &round),
                &Credential::ApiKey("sk-1".into()),
                None,
                Some(&tools),
                Some(&local),
                &[],
            )
            .await
            .unwrap();
        let RoundHome::Local { dir, .. } = &prepared.home else {
            panic!("a local round has a local home: {:?}", prepared.home);
        };
        assert!(
            prepared.invocation.mcp.is_none() && prepared.invocation.codex_mcp_config.is_none(),
            "a local round reaches its tools on the listener, not the tunnel"
        );
        (
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(),
            local,
        )
    }

    #[tokio::test]
    async fn codex_local_mcp_is_http_without_vault_env() {
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let (config, local) = local_round_config(&engine).await;
        let parsed: toml::Table = config.parse().unwrap();
        let server = &parsed["mcp_servers"]["workforce"];
        assert_eq!(server["url"].as_str(), Some(local.url().as_str()));
        assert_eq!(
            server["http_headers"]["Authorization"].as_str(),
            Some(format!("Bearer {}", local.token()).as_str())
        );
        for spawned in ["command", "args", "env_vars"] {
            assert!(
                server.get(spawned).is_none(),
                "nothing is spawned, so no {spawned} and no vault key: {config}"
            );
        }
    }

    #[tokio::test]
    async fn local_codex_tool_timeout_exceeds_approval_patience() {
        let data = tempfile::tempdir().unwrap();
        let codex = CodingCodexConfig {
            tool_timeout_secs: 120,
            ..CodingCodexConfig::default()
        };
        let engine = CodexEngine::new(CodexAuth::ApiKey, codex.clone()).with_data_dir(data.path());
        let (config, _local) = local_round_config(&engine).await;
        let parsed: toml::Table = config.parse().unwrap();
        let waits = Duration::from_secs(
            parsed["mcp_servers"]["workforce"]["tool_timeout_sec"]
                .as_integer()
                .unwrap() as u64,
        );
        let patience = engine.client_timeout().patience();
        assert_eq!(
            patience,
            workforce_agent::runner::SELF_SERVE_APPROVAL_TIMEOUT,
            "a held prompt gets its whole time"
        );
        assert!(
            waits >= codex.effective_tool_timeout() + patience,
            "Codex must outwait a tool that ran its full time after a full prompt: {waits:?}"
        );
    }

    #[test]
    fn a_relative_data_dir_still_gives_an_absolute_session_home_root() {
        let root = session_home_root(Path::new("data"));
        assert!(root.is_absolute(), "{}", root.display());
        assert!(root.ends_with("data/codex-homes"), "{}", root.display());
        assert_eq!(
            session_home_root(Path::new("/app/data")),
            PathBuf::from("/app/data/codex-homes")
        );
    }

    #[tokio::test]
    async fn a_one_shot_home_on_the_worker_goes_with_its_round() {
        let sandbox = RecordingSandbox::new(true);
        let engine = CodexEngine::contained(CodingCodexConfig::default());
        let home = engine.home(
            &sandbox,
            &CodingRound::one_shot("answer".into(), Duration::from_secs(30)),
        );
        let RoundHome::Remote(dir) = &home else {
            panic!("a worker round's home is in its scope: {home:?}");
        };

        home.discard(&sandbox).await;

        assert_eq!(
            *sandbox.commands.lock().unwrap(),
            vec![vec![
                "rm".to_string(),
                "-rf".into(),
                dir.display().to_string()
            ]],
            "its login is not left in a scope other people's rounds share"
        );
    }

    #[tokio::test]
    async fn discard_removes_nothing_outside_the_root_the_home_was_made_under() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let sandbox = RecordingSandbox::new(false);
        for home in [
            RoundHome::Local {
                root: tmp.path().join("root"),
                dir: outside.clone(),
            },
            RoundHome::Local {
                root: outside.clone(),
                dir: outside.clone(),
            },
        ] {
            home.discard(&sandbox).await;
            assert!(outside.is_dir(), "removed by {home:?}");
        }
    }

    #[test]
    fn the_config_file_turns_off_the_features_a_round_never_uses() {
        let parsed: toml::Table =
            render_config(CodexPolicy::Coding, "gpt-5.6-sol", None, None, None)
                .unwrap()
                .parse()
                .unwrap();
        let features = parsed["features"].as_table().expect("a [features] table");
        for feature in [
            "apps",
            "browser_use",
            "browser_use_external",
            "computer_use",
            "image_generation",
            "in_app_browser",
            "plugins",
            "remote_plugin",
            "skill_search",
            "multi_agent",
        ] {
            assert_eq!(
                features.get(feature).and_then(|v| v.as_bool()),
                Some(false),
                "{feature}"
            );
        }
        assert_eq!(features.len(), 10, "{features:?}");

        let with_mcp = render_config(
            CodexPolicy::Coding,
            "gpt-5.6-sol",
            None,
            Some(CodexMcpServer::http("http://127.0.0.1:1/mcp", "t", 300)),
            None,
        )
        .unwrap();
        let parsed: toml::Table = with_mcp.parse().unwrap();
        assert_eq!(parsed["features"]["apps"].as_bool(), Some(false));
        assert!(parsed["mcp_servers"]["workforce"].is_table(), "{with_mcp}");
    }

    #[test]
    fn codex_contained_config_turns_shell_and_env_off() {
        let rendered = render_config(
            CodexPolicy::Contained,
            "gpt-6.1-sol",
            Some("low"),
            None,
            None,
        )
        .unwrap();
        let parsed: toml::Table = rendered.parse().unwrap();
        assert_eq!(parsed["sandbox_mode"].as_str(), Some("read-only"));
        assert_eq!(parsed["web_search"].as_str(), Some("disabled"));
        assert_eq!(parsed["approval_policy"].as_str(), Some("never"));
        let env = parsed["shell_environment_policy"].as_table().unwrap();
        assert_eq!(env["inherit"].as_str(), Some("none"));
        assert_eq!(env["ignore_default_excludes"].as_bool(), Some(false));
        let features = parsed["features"].as_table().unwrap();
        for feature in UNUSED_FEATURES.iter().chain(CONTAINED_OFF) {
            assert_eq!(features[*feature].as_bool(), Some(false), "{feature}");
        }
        assert!(!rendered.contains("mcp_servers"), "{rendered}");

        let coding: toml::Table =
            render_config(CodexPolicy::Coding, "gpt-6.1-sol", None, None, None)
                .unwrap()
                .parse()
                .unwrap();
        assert!(coding.get("web_search").is_none());
        assert!(coding["features"].get("shell_tool").is_none());
    }

    #[test]
    fn codex_agent_turn_config_read_only_no_shell_no_env_no_web() {
        let parsed: toml::Table =
            render_config(CodexPolicy::Contained, "gpt-6.1-sol", None, None, None)
                .unwrap()
                .parse()
                .unwrap();
        assert_eq!(parsed["sandbox_mode"].as_str(), Some("read-only"));
        assert_eq!(parsed["web_search"].as_str(), Some("disabled"));
        assert_eq!(
            parsed["shell_environment_policy"]["inherit"].as_str(),
            Some("none")
        );
        assert_eq!(parsed["project_doc_max_bytes"].as_integer(), Some(0));
        assert_eq!(
            parsed["include_permissions_instructions"].as_bool(),
            Some(false)
        );
        assert_eq!(parsed["include_apps_instructions"].as_bool(), Some(false));
        assert_eq!(
            parsed["skills"]["include_instructions"].as_bool(),
            Some(false)
        );
        let features = parsed["features"].as_table().unwrap();
        for feature in [
            "shell_tool",
            "view_image",
            "multi_agent",
            "apps",
            "image_generation",
            "tool_suggest",
            "sleep_tool",
            "goals",
        ] {
            assert_eq!(features[feature].as_bool(), Some(false), "{feature}");
        }
        let coding: toml::Table =
            render_config(CodexPolicy::Coding, "gpt-6.1-sol", None, None, None)
                .unwrap()
                .parse()
                .unwrap();
        for kept in [
            "project_doc_max_bytes",
            "include_permissions_instructions",
            "skills",
            "model_instructions_file",
        ] {
            assert!(coding.get(kept).is_none(), "{kept}");
        }
    }

    #[tokio::test]
    async fn contained_codex_round_is_never_a_durable_job() {
        let sandbox = RecordingSandbox::new(true);
        let round = CodingRound::one_shot("t".into(), Duration::from_secs(5));
        let durable = |engine: CodexEngine| {
            let sandbox = &sandbox;
            let round = &round;
            async move {
                engine
                    .prepare(
                        sandbox,
                        round,
                        engine.home(sandbox, round),
                        &Credential::ApiKey("sk-1".into()),
                        None,
                        None,
                        None,
                        &[],
                    )
                    .await
                    .unwrap()
                    .invocation
                    .remote
                    .expect("on the worker")
                    .jobs
            }
        };
        assert!(
            !durable(CodexEngine::contained(CodingCodexConfig::default())).await,
            "a contained round ends with its connection: never a durable job"
        );
        assert!(
            durable(CodexEngine::new(
                CodexAuth::ApiKey,
                CodingCodexConfig::default()
            ))
            .await,
            "a coding round on a jobs-capable scope still is one"
        );
    }

    #[tokio::test]
    async fn codex_agent_turn_images_follow_prompt_with_dash_i() {
        let engine = CodexEngine::contained(CodingCodexConfig::default());
        let images = [
            PathBuf::from("/work/rails/.workforce-images-1/1.png"),
            PathBuf::from("/work/rails/.workforce-images-1/2.jpg"),
        ];
        let tail = |sandbox: RecordingSandbox, resume: bool| {
            let engine = &engine;
            let images = &images;
            async move {
                let round = CodingRound {
                    continuity: Continuity::Session {
                        id: Uuid::new_v4(),
                        resume,
                    },
                    ..CodingRound::one_shot("what is this?".into(), Duration::from_secs(5))
                };
                let prepared = engine
                    .prepare(
                        &sandbox,
                        &round,
                        engine.home(&sandbox, &round),
                        &Credential::ApiKey("sk-1".into()),
                        None,
                        None,
                        None,
                        images,
                    )
                    .await
                    .unwrap();
                let args = args_of(&prepared.invocation.command);
                let prompt = args.iter().position(|a| a == "-").unwrap();
                args[prompt.saturating_sub(2)..].to_vec()
            }
        };
        let expected = |head: &[&str]| -> Vec<String> {
            head.iter()
                .map(|a| a.to_string())
                .chain([
                    "-".to_string(),
                    "-i".into(),
                    images[0].display().to_string(),
                    "-i".into(),
                    images[1].display().to_string(),
                ])
                .collect()
        };

        assert_eq!(
            tail(RecordingSandbox::new(true), false).await,
            expected(&["/work/rails", "--skip-git-repo-check"]),
            "a new thread: the prompt, then each image"
        );
        let resumed = RecordingSandbox {
            fresh: false,
            thread: Some("thr_9".into()),
            ..RecordingSandbox::new(true)
        };
        assert_eq!(
            tail(resumed, true).await,
            expected(&["resume", "thr_9"]),
            "after the thread, the prompt and then each image"
        );
    }

    #[test]
    fn codex_turn_capture_emits_no_response_or_done() {
        let mut capture = ThreadCapture::shaped(ProgressShape::Turn, Usage::default());
        let mcp = r#"{"type":"item.started","item":{"id":"t","type":"mcp_tool_call","server":"workforce","tool":"jira_get_issue","status":"in_progress"}}"#;
        let progress: Vec<ProgressEvent> = [
            STARTED,
            TURN,
            &message("Looking it up."),
            mcp,
            &message("It is open."),
            COMPLETED,
        ]
        .iter()
        .flat_map(|line| capture.record(&event(line)))
        .collect();

        let shape: Vec<String> = progress
            .iter()
            .map(|event| match event {
                ProgressEvent::LlmCall { round } => format!("call {round}"),
                ProgressEvent::StreamStart => "start".into(),
                ProgressEvent::StreamDelta { text } => format!("delta {text}"),
                ProgressEvent::StreamEnd => "end".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "call 1",
                "start",
                "delta Looking it up.",
                "end",
                "call 2",
                "start",
                "delta It is open.",
                "end"
            ],
            "each answer a call and a closed segment; no tool, response or end of its own"
        );
        assert!(capture.finished());
        assert_eq!(capture.message.as_deref(), Some("It is open."));
        assert_eq!(capture.spent().output_tokens, 122);

        let mut refused = ThreadCapture::shaped(ProgressShape::Turn, Usage::default());
        let failed = r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#;
        assert!(
            [STARTED, TURN, failed]
                .iter()
                .flat_map(|line| refused.record(&event(line)))
                .next()
                .is_none(),
            "a turn refused before the model answered shows nothing"
        );
    }

    #[test]
    fn a_reconnect_notice_mid_turn_neither_ends_nor_fails_it() {
        let (pending, _) = capture_of(&[STARTED, TURN, RECONNECTING]);
        assert!(!pending.finished(), "the CLI is retrying");
        assert_eq!(
            pending.failure.as_deref(),
            Some("Reconnecting... 1/5 (stream disconnected before completion)"),
            "the notice stands if the process ends before the turn does"
        );

        let (recovered, _) =
            capture_of(&[STARTED, TURN, RECONNECTING, &message("done"), COMPLETED]);
        assert!(recovered.finished());
        assert_eq!(recovered.failure, None);
    }

    const RECONNECTING: &str = r#"{"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion)"}"#;

    #[test]
    fn a_resumed_round_reports_what_it_added_to_its_thread() {
        let before = Usage {
            input: 20_000,
            cached: 20_000,
            cache_write: 0,
            output: 100,
        };
        let mut capture = ThreadCapture::shaped(ProgressShape::Coding, before);
        let done = [STARTED, TURN, COMPLETED]
            .iter()
            .flat_map(|line| capture.record(&event(line)))
            .find_map(|progress| match progress {
                ProgressEvent::Done {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    ..
                } => Some((input_tokens, output_tokens, cache_read_tokens)),
                _ => None,
            });

        assert_eq!(
            done,
            Some((315, 22, 4_448)),
            "a coding round's progress shows its own usage"
        );
        let spent = capture.spent();
        assert_eq!(
            (
                spent.input_tokens,
                spent.output_tokens,
                spent.cache_read_input_tokens
            ),
            (315, 22, 4_448),
            "a round cut short is billed its own usage"
        );
    }

    /// A session round of `engine` in `sandbox`, prepared, that asks to
    /// resume and gives `system` as its prompt.
    async fn session_round(
        engine: &CodexEngine,
        sandbox: &RecordingSandbox,
        id: Uuid,
        resume: bool,
        system: &str,
    ) -> Prepared {
        let round = CodingRound {
            continuity: Continuity::Session { id, resume },
            system_prompt: Some(system.into()),
            ..CodingRound::one_shot("and now?".into(), Duration::from_secs(5))
        };
        engine
            .prepare(
                sandbox,
                &round,
                engine.home(sandbox, &round),
                &Credential::ApiKey("sk-1".into()),
                None,
                None,
                None,
                &[],
            )
            .await
            .unwrap()
    }

    fn config_of(sandbox: &RecordingSandbox, home: &RoundHome) -> toml::Table {
        sandbox
            .writes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(path, _)| *path == home.file(CONFIG_FILE))
            .map(|(_, config)| config.parse().unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn codex_prompt_file_refreshed_each_round() {
        let engine = CodexEngine::contained(CodingCodexConfig::default());
        let sandbox = RecordingSandbox {
            fresh: false,
            thread: Some("thr_1".into()),
            ..RecordingSandbox::new(true)
        };
        let id = Uuid::new_v4();

        for (resume, system) in [(false, "PERSONA-ONE"), (true, "PERSONA-TWO")] {
            let prepared = session_round(&engine, &sandbox, id, resume, system).await;
            let instructions = prepared.home.file(INSTRUCTIONS_FILE);
            assert_eq!(
                sandbox.read(&instructions, 1024).await.unwrap().as_deref(),
                Some(system),
                "each round writes the prompt it was given"
            );
            assert_eq!(
                config_of(&sandbox, &prepared.home)["model_instructions_file"].as_str(),
                Some(instructions.to_str().unwrap()),
                "and Codex reads it in place of its own"
            );
            assert_eq!(prepared.invocation.stdin, "and now?");
            assert_eq!(prepared.resumed, resume);
        }
    }

    #[tokio::test]
    async fn a_scope_home_resumes_its_thread_though_its_scope_was_new() {
        let engine = CodexEngine::contained(CodingCodexConfig::default());
        let sandbox = RecordingSandbox::new(true);
        let id = Uuid::new_v4();
        let first = session_round(&engine, &sandbox, id, false, "p").await;
        sandbox
            .write(&first.home.file(THREAD_FILE), "thr_7")
            .await
            .unwrap();

        let next = session_round(&engine, &sandbox, id, true, "p").await;

        assert!(
            next.resumed,
            "a scope's home is new only with its scope, so the thread a round of this turn left is resumed"
        );
        let args = args_of(&next.invocation.command);
        assert!(
            args.windows(2).any(|w| w == ["resume", "thr_7"]),
            "{args:?}"
        );
    }

    #[tokio::test]
    async fn a_host_home_never_resumes_into_a_new_workspace() {
        let data = tempfile::tempdir().unwrap();
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default())
            .with_data_dir(data.path());
        let sandbox = RecordingSandbox::new(false);
        let id = Uuid::new_v4();
        let first = session_round(&engine, &sandbox, id, false, "p").await;
        first
            .home
            .write(&sandbox, THREAD_FILE, "thr_7")
            .await
            .unwrap();

        let next = session_round(&engine, &sandbox, id, true, "p").await;

        assert!(!next.resumed && !next.lost);
    }

    #[tokio::test]
    async fn a_thread_usage_record_applies_only_to_its_thread() {
        let engine = CodexEngine::contained(CodingCodexConfig::default());
        let used = Usage {
            input: 300,
            cached: 50,
            cache_write: 0,
            output: 19,
        };
        let baseline = |kept: &str| {
            let engine = &engine;
            let kept = kept.to_string();
            async move {
                let sandbox = RecordingSandbox {
                    fresh: false,
                    thread: Some("thr_1".into()),
                    ..RecordingSandbox::new(true)
                };
                let id = Uuid::new_v4();
                let home = RoundHome::new(
                    true,
                    Continuity::Session { id, resume: true },
                    Path::new("/"),
                );
                sandbox.write(&home.file(USAGE_FILE), &kept).await.unwrap();
                session_round(engine, &sandbox, id, true, "p")
                    .await
                    .used_before
            }
        };
        let record = |thread: &str| {
            serde_json::to_string(&ThreadUsage {
                thread: thread.into(),
                used,
            })
            .unwrap()
        };

        assert_eq!(baseline(&record("thr_1")).await, used);
        assert_eq!(
            baseline(&record("thr_0")).await,
            Usage::default(),
            "another thread's total is no baseline for this one"
        );
        assert_eq!(
            baseline(&serde_json::to_string(&used).unwrap()).await,
            Usage::default(),
            "nor is a total kept before records named their thread"
        );
    }

    #[tokio::test]
    async fn a_contained_round_whose_thread_is_gone_reports_it() {
        let sandbox = RecordingSandbox {
            fresh: false,
            ..RecordingSandbox::new(true)
        };
        let id = Uuid::new_v4();
        let contained = CodexEngine::contained(CodingCodexConfig::default());
        let coding = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default());

        assert!(
            session_round(&contained, &sandbox, id, true, "p")
                .await
                .lost
        );
        assert!(
            !session_round(&coding, &sandbox, id, true, "p").await.lost,
            "a coding round's workspace still holds its work: it starts a thread there"
        );
        assert!(
            !session_round(&contained, &sandbox, id, false, "p")
                .await
                .lost
        );
    }

    #[tokio::test]
    async fn a_coding_round_rides_out_a_reconnect_notice() {
        let codex = ScriptedCodex::new();
        codex.reply(
            None,
            "done",
            &[
                event(RECONNECTING),
                message_event("done"),
                completed_event(10, 0, 3),
            ],
        );
        let workdir = tempfile::tempdir().unwrap();
        let sandbox =
            crate::coding_sandbox::adopt_workdir(workdir.path().to_path_buf(), Vec::new(), None);
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            "http://127.0.0.1:9/token".to_string(),
        );
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default());

        let Driven { result, .. } = CODEX
            .scope(
                codex.program(),
                engine.drive(
                    sandbox.as_ref(),
                    &CodingRound::one_shot("fix it".into(), Duration::from_secs(30)),
                    Credential::ApiKey("sk-1".into()),
                    RoundAids {
                        refresher: &refresher,
                        gh_token: None,
                        tools: None,
                        images: Vec::new(),
                    },
                    None,
                    None,
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.completion, Completion::Success);
        assert_eq!(result.text, "done");
    }

    #[tokio::test]
    async fn a_coding_rounds_hub_tools_still_reach_the_gate() {
        use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool};
        use workforce_agent::tools::approval::Refusal;
        let service = crate::mcp_turn::testing::service().await;
        let tool = Arc::new(RecordingTool::default());
        service
            .tools
            .register(Arc::clone(&tool) as Arc<dyn workforce_agent::tools::Tool>);
        service.set_permission_gate(Arc::new(AskEveryCall));
        let engine = CodexEngine::new(CodexAuth::ApiKey, CodingCodexConfig::default());
        let tools = RoundTools::bind(
            &service,
            "alice",
            crate::mcp_turn::McpExposure::Patterns(["catalogue.*".to_string()].into()),
            None,
            engine.client_timeout(),
        )
        .unwrap();
        let codex = ScriptedCodex::new();
        codex.script(
            None,
            &calling(
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
                &thread("done", &[message_event("done"), completed_event(10, 0, 3)]),
            ),
        );
        let workdir = tempfile::tempdir().unwrap();
        let sandbox =
            crate::coding_sandbox::adopt_workdir(workdir.path().to_path_buf(), Vec::new(), None);
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            "http://127.0.0.1:9/token".to_string(),
        );

        let Driven { result, .. } = CODEX
            .scope(
                codex.program(),
                engine.drive(
                    sandbox.as_ref(),
                    &CodingRound::one_shot("tidy the catalogue".into(), Duration::from_secs(30)),
                    Credential::ApiKey("sk-1".into()),
                    RoundAids {
                        refresher: &refresher,
                        gh_token: None,
                        tools: Some(tools),
                        images: Vec::new(),
                    },
                    None,
                    None,
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.completion, Completion::Success);
        let config: toml::Table = codex.recorded(1, "config").parse().unwrap();
        assert_eq!(
            config["mcp_servers"]["workforce"]["default_tools_approval_mode"].as_str(),
            Some("approve"),
            "Codex asks no one, under any sandbox"
        );
        assert!(
            codex
                .file("1.mcp")
                .contains(Refusal::Unattended.tool_result()),
            "the hub's gate still decides the call: {}",
            codex.file("1.mcp")
        );
        assert!(tool.runs().is_empty());
    }

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";

    /// A ChatGPT login of Alice's on its own account, stored at `key` of
    /// `vault`; returns it as stored.
    fn alice_login(vault: &Vault, key: &str) -> String {
        let login = crate::subscription::test_support::member_login(
            &format!("acct-{}", Uuid::new_v4()),
            "user-1",
            key,
            &format!("rt-{}", Uuid::new_v4()),
        );
        vault.set(key, &login).unwrap();
        login
    }

    /// One round of the subscription Codex engine on `codex`, as Alice.
    async fn alice_round(
        service: &crate::serve::AgentService,
        codex: &ScriptedCodex,
        round: &CodingRound,
        workdir: &Path,
    ) -> CliRoundResult {
        let ctx = RoundContext {
            service,
            identity: Some(ALICE),
            stand_in: workforce_common::subscription::SCHEDULER,
            ticket_id: None,
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        };
        let sandbox = crate::coding_sandbox::adopt_workdir(workdir.to_path_buf(), Vec::new(), None);
        let engine = CodexEngine::new(CodexAuth::Subscription, CodingCodexConfig::default());
        crate::user_config::as_principal(
            &workforce_common::session::TurnPrincipal::run_as(Some(ALICE.to_string())),
            CODEX.scope(
                codex.program(),
                engine.run(sandbox, round, &ctx, None, None),
            ),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cx_round_uses_resolved_login_and_writes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = crate::serve::testing::chatgpt_service(tmp.path(), &server, |_| {}).await;
        let (agent, global) = (
            service.vault.clone().unwrap(),
            service.global_vault.clone().unwrap(),
        );
        let personal = crate::user_config::per_user_key(ALICE, AUTH_VAULT_KEY);
        let on_agent = AuthJson::parse(&alice_login(&agent, &personal)).unwrap();
        let everywhere = alice_login(&global, &personal);
        let mut refreshed = on_agent.clone();
        refreshed.tokens.as_mut().unwrap().refresh_token = "rt-cli".into();
        let codex = ScriptedCodex::new();
        codex.refreshes_to(&refreshed.render());
        let workdir = tempfile::tempdir().unwrap();

        let result = alice_round(
            &service,
            &codex,
            &CodingRound::one_shot("fix it".into(), Duration::from_secs(60)),
            workdir.path(),
        )
        .await;

        assert_eq!(result.completion, Completion::Success);
        assert_eq!(
            AuthJson::parse(&codex.recorded(1, "auth"))
                .unwrap()
                .account_id(),
            on_agent.account_id(),
            "her login on this agent runs before her global one"
        );
        assert_eq!(
            AuthJson::parse(&agent.get(&personal).unwrap())
                .unwrap()
                .refresh_token(),
            Some("rt-cli"),
            "the CLI's refresh is kept in the slot the login came from"
        );
        assert_eq!(global.get(&personal).as_deref(), Some(everywhere.as_str()));
        let rows = super::super::tests::cost_rows(&service);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payer.as_deref(), Some(ALICE));
        assert_eq!(
            rows[0]
                .credential_tier
                .map(workforce_common::subscription::CredentialTier::as_str),
            Some("agent_user")
        );
    }

    /// Between rounds a session's home holds her login, so it is closed to
    /// the runner's user, which runs every agent's commands; the next round
    /// opens it and resumes there.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_home_on_this_host_is_closed_between_its_rounds() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = crate::serve::testing::chatgpt_service(tmp.path(), &server, |_| {}).await;
        let personal = crate::user_config::per_user_key(ALICE, AUTH_VAULT_KEY);
        alice_login(service.vault.as_ref().unwrap(), &personal);
        let codex = ScriptedCodex::new();
        let workdir = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let round = CodingRound {
            continuity: Continuity::Session { id, resume: true },
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(60))
        };
        let home = local_home_root().join(format!(".codex-{id}"));
        let open_to_others = || std::fs::metadata(&home).unwrap().permissions().mode() & 0o077 != 0;

        for n in 1..=2 {
            let result = alice_round(&service, &codex, &round, workdir.path()).await;
            assert_eq!(result.completion, Completion::Success, "round {n}");
            assert!(!open_to_others(), "round {n} left its home open");
            assert!(
                home.join(AUTH_FILE).is_file(),
                "the login stays for the next round"
            );
        }
        assert!(
            AuthJson::parse(&codex.recorded(2, "auth")).is_ok(),
            "the second round ran in the home the first closed"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn cx_round_refused_after_a_refresh_marks_the_refreshed_login_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/oauth/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": jwt(serde_json::json!({
                        "exp": (Utc::now() + TimeDelta::hours(1)).timestamp()
                    })),
                    "refresh_token": format!("rt-{}", Uuid::new_v4()),
                })),
            )
            .mount(&server)
            .await;
        let service = crate::serve::testing::chatgpt_service(tmp.path(), &server, |_| {}).await;
        let agent = service.vault.clone().unwrap();
        let personal = crate::user_config::per_user_key(ALICE, AUTH_VAULT_KEY);
        let mut expiring = AuthJson::parse(&alice_login(&agent, &personal)).unwrap();
        expiring.tokens.as_mut().unwrap().access_token =
            jwt(serde_json::json!({ "exp": (Utc::now() + TimeDelta::minutes(2)).timestamp() }));
        agent.set(&personal, &expiring.render()).unwrap();
        let codex = ScriptedCodex::new();
        codex.reply(
            None,
            "",
            &[failed_event("unexpected status 401 Unauthorized")],
        );
        let workdir = tempfile::tempdir().unwrap();

        alice_round(
            &service,
            &codex,
            &CodingRound::one_shot("fix it".into(), Duration::from_secs(60)),
            workdir.path(),
        )
        .await;

        assert_ne!(
            AuthJson::parse(&agent.get(&personal).unwrap())
                .unwrap()
                .fingerprint(),
            expiring.fingerprint(),
            "the login was refreshed for the round"
        );
        let no_login = crate::user_config::as_principal(
            &workforce_common::session::TurnPrincipal::run_as(Some(ALICE.to_string())),
            async { service.logins.resolve(SubscriptionKind::ChatGpt) },
        )
        .await
        .expect_err("the refused login is skipped");
        assert!(
            no_login
                .misses
                .iter()
                .any(|(_, miss)| *miss == crate::subscription::Miss::Invalid),
            "{no_login}"
        );
    }

    #[tokio::test]
    async fn collected_cx_round_goes_on_with_the_login_its_job_was_handed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = crate::serve::testing::chatgpt_service(tmp.path(), &server, |_| {}).await;
        let (agent, global) = (
            service.vault.clone().unwrap(),
            service.global_vault.clone().unwrap(),
        );
        let personal = crate::user_config::per_user_key(ALICE, AUTH_VAULT_KEY);
        let handed = AuthJson::parse(&alice_login(&global, &personal)).unwrap();
        let codex = ScriptedCodex::new();
        let workdir = tempfile::tempdir().unwrap();
        let (first, collected) = keyed_rounds();

        alice_round(&service, &codex, &first, workdir.path()).await;
        alice_login(&agent, &personal);
        alice_round(&service, &codex, &collected, workdir.path()).await;

        assert_eq!(
            AuthJson::parse(&codex.recorded(2, "auth"))
                .unwrap()
                .account_id(),
            handed.account_id(),
            "the job's home goes on with its own login, not the one resolving first now"
        );
        let rows = super::super::tests::cost_rows(&service);
        assert_eq!(rows.len(), 1, "one job, one cost row");
        assert_eq!(
            rows[0]
                .credential_tier
                .map(workforce_common::subscription::CredentialTier::as_str),
            Some("global_user")
        );
    }
}

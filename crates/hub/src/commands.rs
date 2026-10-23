//! Slash commands — in-chat controls for session management.
//!
//! Commands are detected at the start of user messages (e.g. `/status`, `/new fast`)
//! and handled before the message reaches the LLM. They work across all channel
//! surfaces (CLI, Slack, Telegram, Discord, gateway).
//!

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use workforce_agent::llm::environment::SkillVariants;
use workforce_common::secret::Secret;
use workforce_common::secret_marker::{global_cache, MarkerId, SecretSource};
use workforce_common::settings::AgentEnvironmentsConfig;

use crate::def_cmd::DefNoun;
use crate::environments::{
    self, check_mcp_binding, environment_warnings, parse_env_target, skill_binding_line,
    validate_skill_binding, EnvTarget,
};
use crate::hub_core::environments::{Active, ResolvedEnvironments};
use crate::serve::AgentService;
use workforce_orchestrator::workflow_store::EditContext;

// ─── Command Types ───────────────────────────────────────────────────────────

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/new [model]` — reset session, optionally switch model.
    New { model: Option<String> },
    /// `/reset` — clear session history.
    Reset,
    /// `/status` — show session info.
    Status,
    /// `/compact` — trigger manual context compaction.
    Compact,
    /// `/context` — show context window usage.
    Context,
    /// `/help` — list available commands.
    Help,
    /// `/model [name]` — show or switch the current model.
    Model { name: Option<String> },
    /// `/health` — show model health status.
    Health,
    /// `/wake` — trigger an immediate heartbeat check.
    Wake,
    /// `/heartbeat [history]` — show heartbeat status or history.
    Heartbeat { subcommand: Option<String> },
    /// `/config save` — no longer needed (config auto-saves to workspace config.toml).
    ConfigSave { scope: Option<String> },
    /// `/select-agent local <id>` — switch session to use a connected local agent.
    /// `/select-agent gateway` — switch back to gateway execution.
    /// `/select-agent` (no args) — list connected local agents.
    SelectAgent {
        target: String,
        agent_node_id: Option<String>,
    },
    /// `/tools [prefix]` — list available tools.
    Tools { prefix: Option<String> },
    /// `/secret <name> <value>` — stash a credential under a name the
    /// agent can reference as `[[secret:<name>]]`, without the value ever
    /// reaching the LLM. Fields are `None` when the user omits them so
    /// the executor can return a usage hint.
    Secret {
        name: Option<String>,
        value: Option<String>,
    },
    /// `/endpoint <sub>` — provision the endpoints `http.request` calls.
    /// Human-only by construction: the agent cannot emit a `cmd:`, so it can
    /// describe this but never point an endpoint at a host itself, which is
    /// what keeps `http/` denied to `vault.*`. See [`crate::endpoint_cmd`].
    Endpoint { args: Option<String> },
    /// `/workflow run <id> [key=value …]` — start a workflow as the caller,
    /// reporting into this channel; `shares`/`share`/`unshare`/`visibility`
    /// change who else may reach it and `transfer` hands it over
    /// ([`crate::def_cmd`]). Human-only: the agent cannot emit a `cmd:`, so it
    /// can never widen who reaches a definition, nor give one away.
    Workflow { args: Option<String> },
    /// `/playbook shares|share|unshare|visibility|transfer <id> …` — who else may reach
    /// a playbook. Human-only, for the same reason. The interactive directives
    /// (`list`, `start <id>`, `status`) share the noun but not this command:
    /// they belong to [`crate::playbook_runtime`] and are left unparsed here.
    Playbook { args: Option<String> },
    /// `/monitor shares|share|unshare|visibility|transfer <id> …` — who else may reach
    /// a monitor. Human-only, for the same reason.
    Monitor { args: Option<String> },
    /// `/skill grant|grants|revoke …` — consent flow for WASM skill credentials.
    Skill { args: Option<String> },
    /// `/use <workspace> <env>` (alias `/mcp use ...`) — point THIS session's
    /// gated MCP workspace at `env` for the rest of the session. `env` of
    /// `default`/`reset`/`off` clears the override. Human-only: the agent
    /// cannot emit a `cmd:`, so it can never switch environments itself.
    McpUse {
        workspace: Option<String>,
        env: Option<String>,
    },
    /// `/mcp default <workspace> <env>` — set the persistent auto-active
    /// (default) env for a workspace. Human-only (the default is auto-active,
    /// so an agent able to set it could point prod work at staging).
    McpDefault {
        workspace: Option<String>,
        env: Option<String>,
    },
    /// `/mcp list` — show MCP workspaces and which env is active/dormant.
    McpList,
    /// `/mcp <unknown>` — usage hint naming the subcommand, so a typo (or
    /// `mcp token`, which belongs to `remote-mcp`) never silently lists.
    McpUsage { subcommand: String },
    /// `/remote-mcp token|tokens|token revoke …` — mint, list and revoke the
    /// caller's remote MCP tokens. Human-only like every `cmd:`, and further
    /// requires an active linked identity (see [`crate::remote_mcp::command`]).
    RemoteMcp { args: Option<String> },
    /// `/env <subcommand> …` — manage environments (`list`, `use`, `default`,
    /// `create`, `add`, `remove`, `delete`). Human-only: the agent cannot emit a
    /// `cmd:`, so it can never switch or weaken an environment itself.
    Env { args: Option<String> },
}

/// Result of executing a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// The response text to show the user.
    pub text: String,
    /// Whether this response should be ephemeral (not stored in session history).
    pub ephemeral: bool,
    /// Whether the session was reset/modified.
    pub session_reset: bool,
}

impl CommandResult {
    pub(crate) fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ephemeral: true,
            session_reset: false,
        }
    }

    fn reset(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ephemeral: true,
            session_reset: true,
        }
    }
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

/// Strip a command prefix and return the command body (everything after it).
///
/// Accepts both the slash form (`/status`) and the `cmd:` form
/// (`cmd: status`). The `cmd:` alias exists because Slack slash commands
/// don't work inside threads — typing a literal `/` there triggers Slack's
/// own autocomplete rather than reaching us — so `cmd:` is the thread-friendly
/// way to address the agent. The prefix match is case-insensitive (Slack
/// autocapitalises to `Cmd:`). Returns `None` if neither prefix is present.
pub fn command_body(message: &str) -> Option<&str> {
    let trimmed = message.trim();
    if let Some(rest) = trimmed.strip_prefix('/') {
        return Some(rest);
    }
    if trimmed
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("cmd:"))
    {
        return Some(trimmed[4..].trim_start());
    }
    None
}

/// Does this message read as a non-interrupting "what's happening?" query?
///
/// Only `cmd:`/`/`-prefixed messages qualify (a plain message like "what's
/// happening with the deploy?" is a real question for the agent, not a status
/// probe). Covers the short command forms (`status`, `s`, `progress`) and the
/// natural phrasings a human actually types mid-wait ("btw what is happening
/// right now"). Used by the Slack dispatch loop to answer from the live runner
/// snapshot *before* the hard-stop/spawn path, so asking never kills the run.
pub fn is_status_query(message: &str) -> bool {
    let Some(body) = command_body(message) else {
        return false;
    };
    let b = body.trim().to_lowercase();
    if matches!(
        b.as_str(),
        "status" | "s" | "progress" | "sitrep" | "update" | "wtf"
    ) {
        return true;
    }
    const PHRASES: [&str; 9] = [
        "what's happening",
        "whats happening",
        "what is happening",
        "what are you doing",
        "what're you doing",
        "what you doing",
        "how's it going",
        "hows it going",
        "how is it going",
    ];
    PHRASES.iter().any(|p| b.contains(p)) || b.contains("happening")
}

/// Does this message read as a request for the agent to **stop** what it's doing?
///
/// Used by the Slack dispatch loop to deterministically cancel in-flight work
/// (the live turn + any detached research) *before* the spawn path — instead of
/// letting the model reply "ok, I'll stop" and keep going (the bug this fixes).
///
/// Deliberately conservative. The bot mention is already stripped from `text`,
/// but we also drop a stray leading `<@…>` and then require a stop verb at the
/// head of a *short* message, gated by an allowed-object guard so a real
/// instruction like "stop using metabase" is NOT treated as a halt. Biased
/// toward *not* firing: a missed stop is the status quo, but a false stop
/// swallows a genuine request. Callers only act when there is in-flight work in
/// scope, so a false negative here simply falls through to a normal turn.
pub fn is_stop_intent(message: &str) -> bool {
    // Strip a leading Slack mention token ("<@U123> stop" / "<!here> stop").
    let mut head = message.trim();
    if let Some(rest) = head.strip_prefix('<') {
        if let Some((token, after)) = rest.split_once('>') {
            if token.starts_with('@') || token.starts_with('!') {
                head = after.trim_start();
            }
        }
    }
    // Lowercase and trim surrounding punctuation/space.
    let lowered = head.trim().to_lowercase();
    let b = lowered.trim_matches(|c: char| !c.is_alphanumeric());
    if b.is_empty() {
        return false;
    }

    // Unambiguous fixed phrases (matched exactly so "hold on let me check" — a
    // longer message — does not qualify).
    if matches!(
        b,
        "nevermind"
            | "never mind"
            | "hold on"
            | "hold up"
            | "knock it off"
            | "that's enough"
            | "thats enough"
    ) {
        return true;
    }

    // Tokenise, stripping per-word punctuation ("stop," → "stop").
    let words: Vec<String> = b
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();
    // A real instruction ("stop using X and switch to Y…") is long; a halt is short.
    if words.is_empty() || words.len() > 5 {
        return false;
    }

    // Skip a leading politeness / filler prefix ("please stop", "can you stop").
    const FILLER: &[&str] = &[
        "please", "pls", "plz", "can", "could", "would", "you", "hey", "ok", "okay", "just", "yo",
        "now", "yeah",
    ];
    let mut i = 0;
    while i < words.len() && FILLER.contains(&words[i].as_str()) {
        i += 1;
    }
    let Some(verb) = words.get(i) else {
        return false;
    };
    const STOP_VERBS: &[&str] = &["stop", "cancel", "halt", "abort"];
    if !STOP_VERBS.contains(&verb.as_str()) {
        return false;
    }

    // After the verb, allow only stop-objects / fillers, so "stop researching"
    // halts but "stop using metabase" / "cancel the meeting" do not.
    const OBJECTS: &[&str] = &[
        "it",
        "that",
        "this",
        "them",
        "all",
        "everything",
        "now",
        "please",
        "the",
        "research",
        "researching",
        "investigation",
        "investigating",
        "investigations",
        "search",
        "searching",
        "work",
        "task",
        "tasks",
        "right",
        "immediately",
    ];
    words[i + 1..].iter().all(|w| OBJECTS.contains(&w.as_str()))
}

/// Try to parse a message as a command (either `/name` or `cmd: name` form).
/// Returns `None` if the message is not a command.
pub fn parse_command(message: &str) -> Option<Command> {
    // Normalise both prefixes back to the canonical `/name` form so the match
    // arms (and their aliases) stay defined in one place for both syntaxes.
    let normalized = format!("/{}", command_body(message)?);

    let mut parts = normalized.splitn(2, char::is_whitespace);
    let cmd = parts.next()?.to_lowercase();
    let args = parts.next().map(|s| s.trim().to_string());

    match cmd.as_str() {
        "/new" | "/n" => Some(Command::New { model: args }),
        "/reset" | "/r" | "/clear" => Some(Command::Reset),
        "/status" | "/s" => Some(Command::Status),
        "/compact" | "/gc" => Some(Command::Compact),
        "/context" | "/ctx" => Some(Command::Context),
        "/help" | "/h" | "/?" => Some(Command::Help),
        "/model" | "/m" => Some(Command::Model { name: args }),
        "/health" => Some(Command::Health),
        "/wake" | "/w" => Some(Command::Wake),
        "/heartbeat" | "/hb" => Some(Command::Heartbeat { subcommand: args }),
        cmd if cmd.starts_with("/config") || cmd.starts_with("/save-config") => {
            // Parse /config save [scope] or /save-config [scope]
            if cmd == "/save-config" || args.as_ref().map(|a| a.starts_with("save")) == Some(true) {
                // Extract scope if present (e.g., "/config save global" or "/save-config agent")
                let scope_arg = if cmd == "/save-config" {
                    args // /save-config [scope]
                } else {
                    // /config save [scope] - args contains "save [scope]"
                    args.and_then(|a| a.strip_prefix("save").map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                };
                Some(Command::ConfigSave { scope: scope_arg })
            } else {
                None
            }
        }
        "/select-agent" | "/sa" => {
            let parts: Vec<&str> = args.as_deref().unwrap_or("").split_whitespace().collect();
            match parts.first().copied() {
                Some("local") => Some(Command::SelectAgent {
                    target: "local".to_string(),
                    agent_node_id: parts.get(1).map(|s| s.to_string()),
                }),
                Some("gateway") | Some("remote") => Some(Command::SelectAgent {
                    target: "gateway".to_string(),
                    agent_node_id: None,
                }),
                _ => Some(Command::SelectAgent {
                    target: "list".to_string(),
                    agent_node_id: None,
                }),
            }
        }
        "/tools" | "/t" => Some(Command::Tools { prefix: args }),
        "/secret" => {
            let (name, value) = split_name_value(args.as_deref().unwrap_or(""));
            Some(Command::Secret { name, value })
        }
        "/endpoint" | "/endpoints" | "/ep" => Some(Command::Endpoint { args }),
        "/skill" | "/skills" => Some(Command::Skill { args }),
        "/workflow" | "/wf" => Some(Command::Workflow { args }),
        "/playbook" | "/playbooks" => {
            (!crate::playbook_runtime::is_directive_args(args.as_deref().unwrap_or("")))
                .then_some(Command::Playbook { args })
        }
        "/monitor" | "/monitors" => Some(Command::Monitor { args }),
        "/use" => {
            let (workspace, env) = split_two_words(args.as_deref().unwrap_or(""));
            Some(Command::McpUse { workspace, env })
        }
        "/mcp" => {
            let mut parts = args.as_deref().unwrap_or("").splitn(2, char::is_whitespace);
            let sub = parts.next().unwrap_or("").to_lowercase();
            let rest = parts.next().unwrap_or("");
            match sub.as_str() {
                "use" => {
                    let (workspace, env) = split_two_words(rest);
                    Some(Command::McpUse { workspace, env })
                }
                "default" => {
                    let (workspace, env) = split_two_words(rest);
                    Some(Command::McpDefault { workspace, env })
                }
                "list" | "" => Some(Command::McpList),
                _ => Some(Command::McpUsage { subcommand: sub }),
            }
        }
        "/remote-mcp" => Some(Command::RemoteMcp { args }),
        "/env" => Some(Command::Env { args }),
        _ => None, // Unknown command — let it pass through to the LLM
    }
}

/// Split args into the first two whitespace-delimited words; extra trailing
/// words are ignored. Empties become `None`. Used by `/use` / `/mcp use`.
pub(crate) fn split_two_words(args: &str) -> (Option<String>, Option<String>) {
    let mut it = args.split_whitespace();
    let a = it.next().map(str::to_string);
    let b = it.next().map(str::to_string);
    (a, b)
}

/// Usage hint for `/secret`, shown when args are missing or the name is
/// rejected.
pub(crate) const SECRET_USAGE: &str =
    "cmd: secret <name> <value> — name is lowercase letters, digits or underscore (e.g. `datadog_api_key`). `cmd:` works everywhere, including Slack threads.";

/// Split `<name> <value>` on the first run of whitespace. The value
/// keeps any interior whitespace; both sides are trimmed and empties
/// become `None`.
pub(crate) fn split_name_value(args: &str) -> (Option<String>, Option<String>) {
    let mut parts = args.trim().splitn(2, char::is_whitespace);
    let name = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let value = parts.next().map(credential_value).filter(|s| !s.is_empty());
    (name, value)
}

/// Recover the credential the user actually typed from the text a chat surface
/// delivers.
///
/// Every step here exists because the value is never displayed back: a
/// credential mangled in transit fails as a generic auth error at some remote
/// service, with nothing on either side able to see why.
///
/// * **Slack linkifies.** `kh+99@example.com` arrives as
///   `<mailto:kh+99@example.com|kh+99@example.com>`, and a URL as `<https://…>`. Nothing
///   upstream unwraps it — the Events API `text` reaches us verbatim.
/// * **Slack escapes `&`, `<`, `>`** as HTML entities, so a password
///   containing any of them arrives altered.
/// * **Shell habit adds quotes**, which would otherwise be stored as part of
///   the value.
pub(crate) fn credential_value(raw: &str) -> String {
    let value = unwrap_link_token(raw.trim());
    // `&amp;` last: unescaping it first would turn `&amp;lt;` into a `<`.
    let value = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    unquote(&value).to_string()
}

/// `<mailto:a@b.com|a@b.com>` / `<https://x|label>` / `<https://x>` → the
/// address itself, when the whole value is one such token. A value that merely
/// contains one is left alone: only a lone token is unambiguously the
/// credential.
pub(crate) fn unwrap_link_token(value: &str) -> &str {
    value
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .filter(|v| !v.contains('<') && !v.contains('>'))
        .map(|v| v.split_once('|').map_or(v, |(head, _)| head))
        .map(|v| v.strip_prefix("mailto:").unwrap_or(v))
        .filter(|v| !v.is_empty())
        .unwrap_or(value)
}

/// Drop one matched pair of surrounding quotes. A credential that genuinely
/// both starts and ends with the same quote character is vanishingly rarer
/// than the shell habit of wrapping a pasted value.
fn unquote(value: &str) -> &str {
    let value = value.trim();
    ['"', '\'']
        .iter()
        .find_map(|q| {
            value
                .strip_prefix(*q)
                .and_then(|v| v.strip_suffix(*q))
                .filter(|v| !v.is_empty())
        })
        .unwrap_or(value)
}

// ─── Execution ───────────────────────────────────────────────────────────────

/// Execute a command against an agent service.
pub async fn execute_command(
    cmd: &Command,
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
) -> CommandResult {
    match cmd {
        Command::New { model } => execute_new(service, channel, user, model.as_deref()).await,
        Command::Reset => execute_reset(service, channel, user).await,
        Command::Status => execute_status(service, channel, user).await,
        Command::Compact => execute_compact(service, channel, user).await,
        Command::Context => execute_context(service, channel, user).await,
        Command::Help => execute_help(),
        Command::Model { name } => execute_model(service, name.as_deref()).await,
        Command::Health => execute_health(service).await,
        Command::Wake => execute_wake(service).await,
        Command::Heartbeat { subcommand } => {
            execute_heartbeat(service, subcommand.as_deref()).await
        }
        Command::ConfigSave { scope } => execute_config_save(service, scope.as_deref()).await,
        Command::SelectAgent {
            target,
            agent_node_id,
        } => execute_select_agent(service, channel, user, target, agent_node_id.as_deref()).await,
        Command::Tools { prefix } => execute_tools(service, prefix.as_deref()),
        Command::Secret { name, value } => {
            execute_secret(channel, user, name.as_deref(), value.as_deref())
        }
        Command::Endpoint { args } => {
            crate::endpoint_cmd::execute_endpoint(service, user, args.as_deref()).await
        }
        Command::Skill { args } => {
            crate::skill_cmd::execute_skill(service, channel, user, args.as_deref()).await
        }
        Command::Workflow { args } => {
            crate::workflow_cmd::execute_workflow(service, channel, user, args.as_deref()).await
        }
        Command::Playbook { args } => {
            crate::def_cmd::execute_def(service, DefNoun::Playbook, user, args.as_deref()).await
        }
        Command::Monitor { args } => {
            crate::def_cmd::execute_def(service, DefNoun::Monitor, user, args.as_deref()).await
        }
        Command::McpUse { workspace, env } => {
            execute_mcp_use(service, channel, user, workspace.as_deref(), env.as_deref()).await
        }
        Command::McpDefault { workspace, env } => {
            execute_mcp_default(service, workspace.as_deref(), env.as_deref()).await
        }
        Command::McpList => execute_mcp_list(),
        Command::McpUsage { subcommand } => execute_mcp_usage(subcommand),
        Command::RemoteMcp { args } => {
            crate::remote_mcp::command::execute(service, user, args.as_deref()).await
        }
        Command::Env { args } => execute_env(service, channel, user, args.as_deref()).await,
    }
}

/// Render the current MCP workspaces and their activation state.
fn render_mcp_workspaces() -> String {
    let views = workforce_agent::llm::mcp_workspace::workspace_summary();
    if views.is_empty() {
        return "No MCP workspaces are configured. Ungrouped MCP servers are always active."
            .to_string();
    }
    let mut text = String::from("**MCP workspaces:**\n");
    for v in &views {
        let active = v.active_env.as_deref().unwrap_or("none (all dormant)");
        text.push_str(&format!("\n• `{}` — active: **{}**\n", v.workspace, active));
        for m in &v.members {
            let marks = match (m.active, m.default) {
                (true, true) => " ← active (default)",
                (true, false) => " ← active",
                (false, true) => " (default, dormant)",
                (false, false) => " (dormant)",
            };
            text.push_str(&format!("    - `{}` → `{}`{}\n", m.env, m.server_id, marks));
        }
    }
    text
}

/// A compact one-line list of this agent's environments for usage hints (the
/// front door).
fn render_environment_options(envs: &ResolvedEnvironments) -> String {
    if !envs.enabled {
        return "Environments are off for this agent.".to_string();
    }
    if envs.defs.is_empty() {
        return "No environments configured (create one with `cmd: env create <name>`)."
            .to_string();
    }
    let names: Vec<String> = envs
        .defs
        .keys()
        .map(
            |name| match envs.default.as_deref() == Some(name.as_str()) {
                true => format!("`{name}` (default)"),
                false => format!("`{name}`"),
            },
        )
        .collect();
    format!("**Environments:** {}", names.join(", "))
}

/// Reserved words that mean "revert to the default / clear the override".
fn is_clear_keyword(token: &str) -> bool {
    workforce_common::settings::ENVIRONMENT_CLEAR_WORDS.contains(&token)
}

/// The decision `cmd: use` makes, separated from its effects so it can be unit
/// tested. `first_is_env` / `first_is_workspace` / `has_variant` are resolved by
/// the caller (they need the async runtime + the workspace registry).
#[derive(Debug, PartialEq, Eq)]
enum UseDispatch {
    /// No token typed — show the dual-door usage.
    Usage,
    /// Single clear-keyword — clear this session's environment override.
    ClearEnv,
    /// Single token naming an environment — switch the session to it.
    SwitchEnv,
    /// Single token naming a workspace — it needs a variant.
    NeedVariant,
    /// Single token matching nothing — suggest the options.
    NotFound,
    /// Two tokens, second a clear-keyword — clear the workspace override.
    ClearWorkspace,
    /// Two tokens, a real `<workspace> <variant>` — set the override.
    SetWorkspace,
    /// Two tokens but the first is an environment, not a workspace — redirect.
    EnvNotWorkspace,
    /// Two tokens, workspace exists but lacks that variant.
    NoSuchVariant,
}

/// Pure dispatch logic for the unified `cmd: use` front door. Single token →
/// prefer the environment interpretation (the common case, `cmd: use staging`);
/// two tokens → the advanced per-workspace override.
fn classify_use(
    first: Option<&str>,
    second: Option<&str>,
    first_is_env: bool,
    first_is_workspace: bool,
    has_variant: bool,
) -> UseDispatch {
    let Some(first) = first else {
        return UseDispatch::Usage;
    };
    match second {
        None => {
            if is_clear_keyword(first) {
                UseDispatch::ClearEnv
            } else if first_is_env {
                UseDispatch::SwitchEnv
            } else if first_is_workspace {
                UseDispatch::NeedVariant
            } else {
                UseDispatch::NotFound
            }
        }
        Some(second) => {
            if is_clear_keyword(second) {
                UseDispatch::ClearWorkspace
            } else if has_variant {
                UseDispatch::SetWorkspace
            } else if first_is_env {
                UseDispatch::EnvNotWorkspace
            } else {
                UseDispatch::NoSuchVariant
            }
        }
    }
}

/// `cmd: use …` — the unified front door. A single token that names an
/// ENVIRONMENT switches the whole session to it (skills + MCP together) — the
/// common case, and what most people mean. The two-token `<workspace> <variant>`
/// form is the advanced per-workspace override. On a name that's neither, or an
/// env/workspace mix-up, it suggests the right command rather than failing with a
/// bare usage line. (Reached via `cmd: use` / `cmd: mcp use`; `workspace` holds
/// the first token, `env` the optional second.)
async fn execute_mcp_use(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    workspace: Option<&str>,
    env: Option<&str>,
) -> CommandResult {
    let envs = service.resolved.snapshot().environments.clone();
    let first_is_env = workspace.is_some_and(|f| !is_clear_keyword(f) && envs.defs.contains_key(f));
    let first_is_workspace = workspace
        .map(workforce_agent::llm::mcp_workspace::workspace_exists)
        .unwrap_or(false);
    let has_variant = match (workspace, env) {
        (Some(w), Some(e)) => workforce_agent::llm::mcp_workspace::workspace_has_env(w, e),
        _ => false,
    };

    match classify_use(workspace, env, first_is_env, first_is_workspace, has_variant) {
        UseDispatch::Usage => CommandResult::info(format!(
            "Usage:\n\
             • `cmd: use <environment>` — switch this session's environment (pins every skill \
             and MCP workspace together).\n\
             • `cmd: use <workspace> <variant>` — advanced: point one MCP workspace at a variant.\n\n\
             {}\n\n{}",
            render_environment_options(&envs),
            render_mcp_workspaces()
        )),
        UseDispatch::ClearEnv => execute_env_use(service, &envs, channel, user, "default").await,
        UseDispatch::SwitchEnv => {
            execute_env_use(service, &envs, channel, user, workspace.unwrap_or_default()).await
        }
        UseDispatch::NeedVariant => CommandResult::info(format!(
            "`{}` is an MCP workspace — name which variant: `cmd: use {} <variant>`.\n\n{}",
            workspace.unwrap(),
            workspace.unwrap(),
            render_mcp_workspaces()
        )),
        UseDispatch::NotFound => CommandResult::info(format!(
            "No environment or MCP workspace named `{}`. Did you mean one of these?\n\n{}\n\n{}",
            workspace.unwrap(),
            render_environment_options(&envs),
            render_mcp_workspaces()
        )),
        UseDispatch::ClearWorkspace => {
            let ws = workspace.unwrap();
            if let Ok(mut session) =
                service
                    .sessions
                    .get_or_create(channel, user, service.agent_id.as_str())
            {
                session.clear_active_mcp_env(ws);
                let _ = service.sessions.save(&session);
            }
            CommandResult::info(format!(
                "Cleared the `{ws}` override for this session — it now follows the default \
                 variant.\n\n{}",
                render_mcp_workspaces()
            ))
        }
        UseDispatch::EnvNotWorkspace => CommandResult::info(format!(
            "`{}` is an environment, not a workspace — to switch to it, run `cmd: use {}` (no \
             second word).",
            workspace.unwrap(),
            workspace.unwrap()
        )),
        UseDispatch::NoSuchVariant => CommandResult::info(format!(
            "Workspace `{}` has no `{}` variant. Add it first (`mcp.workspace_add`), or pick an \
             existing variant below.\n\n{}",
            workspace.unwrap(),
            env.unwrap(),
            render_mcp_workspaces()
        )),
        UseDispatch::SetWorkspace => {
            let (ws, variant) = (workspace.unwrap(), env.unwrap());
            match service
                .sessions
                .get_or_create(channel, user, service.agent_id.as_str())
            {
                Ok(mut session) => {
                    session.set_active_mcp_env(ws, variant);
                    if let Err(e) = service.sessions.save(&session) {
                        return CommandResult::info(format!("Failed to save activation: {e}"));
                    }
                    CommandResult::info(format!(
                        "This session now targets the **{variant}** variant of `{ws}`. The agent's \
                         `{ws}` MCP tools will hit `{variant}` until you run `cmd: use {ws} default`.",
                    ))
                }
                Err(e) => CommandResult::info(format!("Could not load session: {e}")),
            }
        }
    }
}

/// `/mcp default <ws> <variant>` — set the persistent default (auto-active)
/// variant for a workspace.
async fn execute_mcp_default(
    service: &Arc<AgentService>,
    workspace: Option<&str>,
    env: Option<&str>,
) -> CommandResult {
    let (Some(workspace), Some(variant)) = (workspace, env) else {
        return CommandResult::info(format!(
            "Usage: `cmd: mcp default <workspace> <variant>` (e.g. `cmd: mcp default harbor prod`).\n\n{}",
            render_mcp_workspaces()
        ));
    };
    if !workforce_agent::llm::mcp_workspace::workspace_has_env(workspace, variant) {
        return CommandResult::info(format!(
            "Workspace `{workspace}` has no `{variant}` variant.\n\n{}",
            render_mcp_workspaces()
        ));
    }
    let Some(manager) = service.mcp_manager() else {
        return CommandResult::info(
            "MCP manager is not available on this gateway, so the default can't be persisted.",
        );
    };
    match manager.set_workspace_default(workspace, variant) {
        Ok(()) => CommandResult::info(format!(
            "Set the default variant for `{workspace}` to **{variant}** — it's now auto-active for \
             every session that hasn't overridden it.\n\n{}",
            render_mcp_workspaces()
        )),
        Err(e) => CommandResult::info(format!("Failed to set default: {e}")),
    }
}

/// `/mcp list` — show MCP workspaces and their activation state.
fn execute_mcp_usage(subcommand: &str) -> CommandResult {
    CommandResult::info(format!(
        "Unknown `cmd: mcp {subcommand}`. Usage: `cmd: mcp list`, `cmd: mcp use <workspace> <env>`, \
         `cmd: mcp default <workspace> <env>`. Remote MCP tokens live under `cmd: remote-mcp`."
    ))
}

fn execute_mcp_list() -> CommandResult {
    CommandResult::info(render_mcp_workspaces())
}

fn env_edit(user: &str) -> EditContext<'_> {
    EditContext::new("command", Some(user))
}

/// Render each of this agent's environments, its bindings, the default marker,
/// and the multi-instance skills it leaves unbound.
fn render_environments(
    envs: &ResolvedEnvironments,
    catalogue: &BTreeMap<String, SkillVariants>,
) -> String {
    if envs.defs.is_empty() {
        return "No environments are configured. Create one with `cmd: env create <name>`."
            .to_string();
    }
    let mut text = String::from("**Environments:**\n");
    for (name, def) in &envs.defs {
        let mark = match (envs.default.as_deref() == Some(name.as_str()), def.draft) {
            (true, _) => " (default)",
            (false, true) => " (draft — not active until a person activates it)",
            (false, false) => "",
        };
        text.push_str(&format!("\n• `{name}`{mark}\n"));
        if def.skills.is_empty() && def.mcp.is_empty() {
            text.push_str("    - (no bindings)\n");
        }
        for (skill, instances) in &def.skills {
            text.push_str(&format!("    - {}\n", skill_binding_line(skill, instances)));
        }
        for (workspace, variant) in &def.mcp {
            text.push_str(&format!("    - mcp `{workspace}` → `{variant}`\n"));
        }
        for warning in environment_warnings(def, catalogue) {
            text.push_str(&format!("    ⚠ {warning}\n"));
        }
    }
    text
}

/// The unified **state board** — one view that joins the three layers: (1)
/// which environment is active and what it binds, (2) the MCP workspaces those
/// bindings resolve to, and (3) the MCP manager's ground truth per *installed*
/// server — connected, installed-but-awaiting-auth, or absent. Each
/// unreachable binding is surfaced with its specific fix.
///
/// Pure so it can be unit-tested without a live session or manager. Shared by
/// the human `cmd: env` board and the agent-facing `env.status` tool.
pub(crate) fn render_env_board(
    active: Option<Active<'_>>,
    default_name: Option<&str>,
    catalogue: &BTreeMap<String, SkillVariants>,
    statuses: &[crate::hosts::ServerStatus],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    match active {
        Some(active) => {
            let src = if active.is_override {
                "chosen for this session"
            } else {
                "this agent's default"
            };
            let _ = write!(out, "**Active environment: `{}`** ({src}).", active.name);
        }
        None => out.push_str("**Active environment: none.**"),
    }
    match default_name {
        Some(d) => {
            let _ = writeln!(out, "  Default: `{d}`.");
        }
        None => out.push_str("  No default set.\n"),
    }

    let mut dangling: Vec<String> = Vec::new();

    if let Some(active) = active {
        let def = active.def;
        if def.skills.is_empty() && def.mcp.is_empty() {
            out.push_str(
                "\n_Binds nothing — every multi-instance skill and MCP workspace uses its own \
                 default._\n",
            );
        } else {
            out.push_str("\n**Bound by this environment:**\n");
        }
        for (skill, instances) in &def.skills {
            let missing: Vec<&str> = instances
                .iter()
                .map(String::as_str)
                .filter(|i| !catalogue.get(skill).is_some_and(|v| v.has(i)))
                .collect();
            let line = skill_binding_line(skill, instances);
            if missing.is_empty() {
                let _ = writeln!(out, "  • {line}   ✅");
            } else {
                let _ = writeln!(
                    out,
                    "  • {line}   ⚠️ {} not configured",
                    environments::ticked(&missing)
                );
                dangling.push(format!(
                    "skill `{skill}` has no {} instance — load its credentials or rebind \
                     (`cmd: env add {} skill {skill} <instance>…`)",
                    environments::ticked(&missing),
                    active.name
                ));
            }
        }
        for (workspace, variant) in &def.mcp {
            board_mcp_line(&mut out, &mut dangling, workspace, variant, statuses);
        }
    }

    // Ungrouped servers are reachable regardless of environment — list them (with
    // their own state) so "what can I actually call right now" is answered here.
    let mut ungrouped: Vec<&crate::hosts::ServerStatus> =
        statuses.iter().filter(|s| s.workspace.is_none()).collect();
    if !ungrouped.is_empty() {
        ungrouped.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        out.push_str("\n**Ungrouped MCP servers** (always active):\n");
        for s in ungrouped {
            let mark = if s.connected {
                "✅ connected"
            } else if s.needs_auth {
                "⚠️ NOT authorised"
            } else {
                "⚠️ not connected"
            };
            let _ = writeln!(out, "  • `{}`   {mark}", s.server_id);
            if s.needs_auth {
                dangling.push(format!(
                    "MCP server `{}` is installed but not authorised — run `mcp.authorize {}`",
                    s.server_id, s.server_id
                ));
            }
        }
    }

    if dangling.is_empty() {
        out.push_str("\n✅ Everything this environment binds is reachable.");
    } else {
        let _ = write!(out, "\n⚠️ {} dangling reference(s):\n", dangling.len());
        for d in &dangling {
            let _ = writeln!(out, "  - {d}");
        }
    }
    out
}

/// One MCP binding's line on the board, joined with the installed servers for
/// its workspace, recording the fix for an unreachable one in `dangling`.
fn board_mcp_line(
    out: &mut String,
    dangling: &mut Vec<String>,
    workspace: &str,
    variant: &str,
    statuses: &[crate::hosts::ServerStatus],
) {
    use std::fmt::Write as _;
    // Installed servers for this workspace (any variant), from the
    // store — so an installed-but-unconnected server still counts.
    let for_ws: Vec<&crate::hosts::ServerStatus> = statuses
        .iter()
        .filter(|s| s.workspace.as_deref() == Some(workspace))
        .collect();
    let exact = for_ws
        .iter()
        .copied()
        .find(|s| s.env.as_deref() == Some(variant));
    match exact {
        Some(s) if s.connected => {
            let _ = writeln!(
                out,
                "  • mcp   `{}` → `{}` (server `{}`)   ✅ connected",
                workspace, variant, s.server_id
            );
        }
        // Installed + OAuth + not connected: the transcript's case.
        // Name the exact fix instead of "not installed".
        Some(s) if s.needs_auth => {
            let _ = writeln!(
                out,
                "  • mcp   `{}` → `{}` (server `{}`)   ⚠️ installed, NOT authorised",
                workspace, variant, s.server_id
            );
            dangling.push(format!(
                "MCP server `{}` (workspace `{}` / `{}`) is installed but not \
                 authorised — run `mcp.authorize {}` to start the OAuth flow",
                s.server_id, workspace, variant, s.server_id
            ));
        }
        Some(s) => {
            let _ = writeln!(
                out,
                "  • mcp   `{}` → `{}` (server `{}`)   ⚠️ installed but not connected",
                workspace, variant, s.server_id
            );
            dangling.push(format!(
                "MCP server `{}` (workspace `{}` / `{}`) is installed but not \
                 connected — check it with `mcp.show {}` and reinstall if needed",
                s.server_id, workspace, variant, s.server_id
            ));
        }
        // No server for this variant: workspace absent entirely
        // (never installed) or installed only at other variants.
        None => {
            let detail = if for_ws.is_empty() {
                format!("no MCP server is installed for workspace `{}`", workspace)
            } else {
                let mut have: Vec<&str> = for_ws.iter().filter_map(|s| s.env.as_deref()).collect();
                have.sort_unstable();
                format!(
                    "workspace `{}` has no `{}` variant (installed: {})",
                    workspace,
                    variant,
                    if have.is_empty() {
                        "none".to_string()
                    } else {
                        have.join(", ")
                    }
                )
            };
            let _ = writeln!(
                out,
                "  • mcp   `{}` → `{}`   ⚠️ {detail}",
                workspace, variant
            );
            dangling.push(format!(
                "{detail} — install/authorise that MCP server so it registers into \
                 workspace `{}` with variant `{}`",
                workspace, variant
            ));
        }
    }
}

/// `cmd: env` / `cmd: env status` — the state board for this session.
fn execute_env_board(
    service: &Arc<AgentService>,
    envs: &ResolvedEnvironments,
    channel: &str,
    user: &str,
) -> CommandResult {
    let override_name = service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())
        .ok()
        .and_then(|s| s.active_environment());
    let statuses = service
        .mcp_manager()
        .map(|m| m.server_statuses())
        .unwrap_or_default();
    CommandResult::info(render_env_board(
        envs.active(override_name.as_deref()),
        envs.default.as_deref(),
        &service.tools.skill_variants(),
        &statuses,
    ))
}

/// `cmd: env …` — manage this agent's environments. Human-only.
async fn execute_env(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    args: Option<&str>,
) -> CommandResult {
    let envs = service.resolved.snapshot().environments.clone();
    if !envs.enabled {
        return CommandResult::info(if envs.allowed {
            "Environments are off for this agent. An admin turns them on with \
             `environments.enabled = true` in this agent's config."
        } else {
            "Environments are turned off for this install."
        });
    }
    let raw = args.unwrap_or("").trim();
    let mut it = raw.splitn(2, char::is_whitespace);
    let sub = it.next().unwrap_or("").to_lowercase();
    let rest = it.next().unwrap_or("").trim();
    match sub.as_str() {
        "" | "status" | "board" => execute_env_board(service, &envs, channel, user),
        "list" => CommandResult::info(render_environments(&envs, &service.tools.skill_variants())),
        "use" => execute_env_use(service, &envs, channel, user, rest).await,
        "default" => execute_env_default(service, user, rest).await,
        "create" => execute_env_create(service, user, rest).await,
        "add" => execute_env_add(service, user, rest).await,
        "remove" | "rm" => execute_env_remove(service, user, rest).await,
        "delete" | "delete-env" => execute_env_delete(service, &envs, user, rest).await,
        other => CommandResult::info(format!(
            "Unknown env subcommand `{other}`. Try: `status` (the state board), `list`, \
             `use <name>`, `default <name|none>`, `create <name>`, \
             `add <name> skill <skill> <instance>…`, `add <name> mcp <workspace> <variant>`, \
             `remove <name> skill <skill> [<instance>]`, `remove <name> mcp <workspace>`, \
             `delete <name>` (delete the whole environment)."
        )),
    }
}

/// Save a change to this agent's environments, answering with `done` or why
/// it did not land.
async fn edit_environments(
    service: &AgentService,
    user: &str,
    transform: impl Fn(AgentEnvironmentsConfig) -> Result<AgentEnvironmentsConfig, String>,
    done: String,
) -> CommandResult {
    match environments::update(service, env_edit(user), transform).await {
        Ok(_) => CommandResult::info(done),
        Err(e) => CommandResult::info(format!("Not changed: {e}")),
    }
}

async fn execute_env_use(
    service: &Arc<AgentService>,
    envs: &ResolvedEnvironments,
    channel: &str,
    user: &str,
    rest: &str,
) -> CommandResult {
    if !envs.enabled {
        return CommandResult::info("Environments are off for this agent.");
    }
    let Some(name) = rest.split_whitespace().next() else {
        return CommandResult::info(
            "Usage: `cmd: env use <name>` (or `cmd: env use default` to clear this session's \
             choice).",
        );
    };
    let mut session = match service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())
    {
        Ok(session) => session,
        Err(e) => return CommandResult::info(format!("Could not load session: {e}")),
    };
    let reply = if is_clear_keyword(name) {
        session.clear_active_environment();
        "Cleared this session's environment — it now follows this agent's default.".to_string()
    } else {
        match envs.defs.get(name) {
            None => return CommandResult::info(format!("No environment named `{name}`.")),
            Some(def) if def.draft => {
                if let Err(e) =
                    environments::update(service, env_edit(user), environments::promote(name)).await
                {
                    return CommandResult::info(format!("Not activated: {e}"));
                }
            }
            Some(_) => {}
        }
        session.set_active_environment(name);
        format!(
            "This session now uses the **{name}** environment. Each skill and MCP workspace it \
             binds is held to that binding until you run `cmd: env use default`."
        )
    };
    match service.sessions.save(&session) {
        Ok(()) => CommandResult::info(reply),
        Err(e) => CommandResult::info(format!("Failed to save the session: {e}")),
    }
}

async fn execute_env_default(service: &AgentService, user: &str, rest: &str) -> CommandResult {
    let Some(name) = rest.split_whitespace().next() else {
        return CommandResult::info(
            "Usage: `cmd: env default <name>`, or `cmd: env default none` to have none.",
        );
    };
    if is_clear_keyword(name) {
        return edit_environments(
            service,
            user,
            environments::clear_default(),
            "This agent now has no default environment; a session uses one only when it runs \
             `cmd: env use <name>`."
                .to_string(),
        )
        .await;
    }
    edit_environments(
        service,
        user,
        environments::set_default(name),
        format!(
            "Set this agent's default environment to **{name}** — it's now active for every \
             session (and background run) that hasn't chosen another."
        ),
    )
    .await
}

async fn execute_env_create(service: &AgentService, user: &str, rest: &str) -> CommandResult {
    let Some(name) = rest.split_whitespace().next() else {
        return CommandResult::info("Usage: `cmd: env create <name>`.");
    };
    edit_environments(
        service,
        user,
        environments::create(name, None, false),
        format!(
            "Created environment **{name}**. Bind skills with \
             `cmd: env add {name} skill <skill> <instance>…`."
        ),
    )
    .await
}

async fn execute_env_add(service: &AgentService, user: &str, rest: &str) -> CommandResult {
    const USAGE: &str = "Usage: `cmd: env add <name> skill <skill> <instance> [<instance>…]` \
                         (the first instance is the default), or \
                         `cmd: env add <name> mcp <workspace> <variant>`.";
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let [env, target, member, values @ ..] = parts.as_slice() else {
        return CommandResult::info(USAGE);
    };
    match parse_env_target(target) {
        Some(EnvTarget::Skill) if !values.is_empty() => {
            let instances: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            if let Err(reason) =
                validate_skill_binding(&service.tools.skill_variants(), member, &instances)
            {
                return CommandResult::info(reason);
            }
            edit_environments(
                service,
                user,
                environments::bind_skill(env, member, instances.clone()),
                format!(
                    "**{env}** now binds {}.",
                    skill_binding_line(member, &instances)
                ),
            )
            .await
        }
        Some(EnvTarget::McpWorkspace) if values.len() == 1 => {
            let variant = values[0];
            let note = match check_mcp_binding(service.mcp_manager().as_deref(), member, variant) {
                Ok(note) => note,
                Err(reason) => return CommandResult::info(reason),
            };
            let done = format!("**{env}** now binds mcp `{member}` → `{variant}`.");
            edit_environments(
                service,
                user,
                environments::bind_mcp(env, member, variant),
                note.map_or(done.clone(), |note| format!("{done}\n\n{note}")),
            )
            .await
        }
        Some(_) => CommandResult::info(USAGE),
        None => CommandResult::info(format!(
            "Unknown binding type `{target}` — use `skill` or `mcp`."
        )),
    }
}

/// `cmd: env delete <name>` — delete a WHOLE environment (vs `remove`, which
/// drops one binding). Deleting the default leaves none; a session that chose
/// the deleted name falls back to the default on its next turn.
async fn execute_env_delete(
    service: &AgentService,
    envs: &ResolvedEnvironments,
    user: &str,
    rest: &str,
) -> CommandResult {
    let Some(name) = rest.split_whitespace().next() else {
        return CommandResult::info(
            "Usage: `cmd: env delete <name>` — deletes the whole environment. To drop one \
             binding instead, use `cmd: env remove <name> <skill|mcp> <member>`.",
        );
    };
    let note = if envs.default.as_deref() == Some(name) {
        " It was the default, so this agent now has no default environment."
    } else {
        ""
    };
    edit_environments(
        service,
        user,
        environments::delete(name),
        format!("Deleted environment **{name}**.{note}"),
    )
    .await
}

async fn execute_env_remove(service: &AgentService, user: &str, rest: &str) -> CommandResult {
    const USAGE: &str = "Usage: `cmd: env remove <name> skill <skill> [<instance>]` or \
                         `cmd: env remove <name> mcp <workspace>`.";
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.as_slice() {
        [env, target, member, rest @ ..] => match (parse_env_target(target), rest) {
            (Some(EnvTarget::Skill), [] | [_]) => {
                let instance = rest.first().map(|i| i.to_string());
                let what = instance.as_deref().map_or(format!("`{member}`"), |i| {
                    format!("`{member}` instance `{i}`")
                });
                edit_environments(
                    service,
                    user,
                    environments::unbind_skill(env, member, instance),
                    format!("Removed {what} from **{env}**."),
                )
                .await
            }
            (Some(EnvTarget::McpWorkspace), []) => {
                edit_environments(
                    service,
                    user,
                    environments::unbind_mcp(env, member),
                    format!("Removed mcp `{member}` from **{env}**."),
                )
                .await
            }
            (None, _) => CommandResult::info(format!(
                "Unknown binding type `{target}` — use `skill` or `mcp`."
            )),
            _ => CommandResult::info(USAGE),
        },
        _ => CommandResult::info(USAGE),
    }
}

/// Capture a named secret into the per-session secret cache. `channel`
/// is the session key; the cache scopes the value to its conversation
/// channel so the agent can reference `[[secret:<name>]]` from any
/// thread. The cleartext is never logged, echoed, or sent to the LLM.
pub(crate) fn execute_secret(
    channel: &str,
    user: &str,
    name: Option<&str>,
    value: Option<&str>,
) -> CommandResult {
    let (Some(name), Some(value)) = (name, value) else {
        return CommandResult::info(SECRET_USAGE);
    };
    let id = match MarkerId::named(name) {
        Ok(id) => id,
        Err(e) => return CommandResult::info(format!("Invalid secret name: {e}.\n{SECRET_USAGE}")),
    };
    let Some(cache) = global_cache() else {
        warn!(
            target: "sentinel.audit",
            user = %user,
            "/secret used but no secret cache is installed — value not stored"
        );
        return CommandResult::info(
            "The secret store is unavailable right now; nothing was saved.",
        );
    };
    cache.capture_named(
        channel,
        &id,
        Secret::new(value.to_string()),
        SecretSource::SlashCommand {
            user: user.to_string(),
        },
    );
    info!(
        target: "sentinel.audit",
        session_key = %channel,
        name = %name,
        user = %user,
        len = value.len(),
        "named secret stored via /secret"
    );
    CommandResult::info(format!(
        "Stored secret `{name}` (value hidden). Reference it as `[[secret:{name}]]` when a tool needs the credential — e.g. `vault.set(\"skills/datadog/api_key\", \"[[secret:{name}]]\")`. It stays available in this channel for 60 minutes of inactivity."
    ))
}

async fn execute_new(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    model: Option<&str>,
) -> CommandResult {
    // Create a fresh session
    match service.create_new_session(channel, user) {
        Ok(session_id) => {
            let model_info = if let Some(m) = model {
                // Resolve alias if applicable
                let resolved = resolve_model_name(service, m);
                format!(" (model: {})", resolved)
            } else {
                String::new()
            };
            CommandResult::reset(format!(
                "🔄 New session started{} — session `{}`",
                model_info,
                &session_id[..8]
            ))
        }
        Err(e) => CommandResult::info(format!("❌ Failed to create session: {}", e)),
    }
}

async fn execute_reset(service: &Arc<AgentService>, channel: &str, user: &str) -> CommandResult {
    match service.create_new_session(channel, user) {
        Ok(session_id) => CommandResult::reset(format!(
            "🗑️ Session reset — new session `{}`",
            &session_id[..8]
        )),
        Err(e) => CommandResult::info(format!("❌ Failed to reset session: {}", e)),
    }
}

async fn execute_status(service: &Arc<AgentService>, channel: &str, user: &str) -> CommandResult {
    let session = match service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())
    {
        Ok(s) => s,
        Err(e) => return CommandResult::info(format!("❌ Session error: {}", e)),
    };

    let total_tokens = session.estimated_total_tokens();
    let msg_count = session.message_count();
    let model = &service.effective_llm.default_model;
    let provider = &service.effective_llm.provider;

    let failover_info = if service.effective_llm.has_failover_config() {
        let chain: Vec<&str> = service
            .config
            .llm
            .failover
            .iter()
            .map(|s| s.as_str())
            .collect();
        format!("\n  Failover: {:?}", chain)
    } else {
        String::new()
    };

    let alias_info = if !service.effective_llm.aliases.is_empty() {
        let aliases: Vec<String> = service
            .config
            .llm
            .aliases
            .iter()
            .map(|(k, v)| format!("{} → {}", k, v))
            .collect();
        format!("\n  Aliases: {}", aliases.join(", "))
    } else {
        String::new()
    };

    let started = session.created_at.format("%Y-%m-%d %H:%M").to_string();
    let last_active = session.updated_at.format("%Y-%m-%d %H:%M").to_string();

    CommandResult::info(format!(
        "📊 *Session Status*\n\
         \n  Session: `{}`\
         \n  Started: {}\
         \n  Last active: {}\
         \n  Messages: {}\
         \n  Tokens: ~{}\
         \n  Model: {}\
         \n  Provider: {}{}{}\
         \n  Channel: {}",
        &session.id[..8],
        started,
        last_active,
        msg_count,
        total_tokens,
        model,
        provider,
        failover_info,
        alias_info,
        channel,
    ))
}

async fn execute_compact(service: &Arc<AgentService>, channel: &str, user: &str) -> CommandResult {
    let mut session = match service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())
    {
        Ok(s) => s,
        Err(e) => return CommandResult::info(format!("❌ Session error: {}", e)),
    };

    // Build compaction engine with memory writer
    let compaction_config = service.current_compaction_config();
    let memory_writer = crate::notes_tool::memory_writer(service);
    let engine = workforce_agent::compaction::CompactionEngine::with_memory_writer(
        compaction_config,
        Arc::clone(&service.llm),
        Some(service.effective_llm.default_model.clone()),
        memory_writer,
    );

    match engine.compact_manual(&mut session).await {
        Ok(event) => {
            if event.messages_compacted > 0 {
                if let Err(e) = service.sessions.save(&session) {
                    return CommandResult::info(format!(
                        "❌ Compaction succeeded but save failed: {}",
                        e
                    ));
                }
                CommandResult::info(format!(
                    "🗜️ Compacted: {} → {} tokens ({} saved, {} messages compacted)",
                    event.tokens_before,
                    event.tokens_after,
                    event.tokens_before.saturating_sub(event.tokens_after),
                    event.messages_compacted,
                ))
            } else {
                CommandResult::info("✅ No compaction needed — session within limits".to_string())
            }
        }
        Err(e) => CommandResult::info(format!("❌ Compaction failed: {}", e)),
    }
}

async fn execute_context(service: &Arc<AgentService>, channel: &str, user: &str) -> CommandResult {
    let session = match service
        .sessions
        .get_or_create(channel, user, service.agent_id.as_str())
    {
        Ok(s) => s,
        Err(e) => return CommandResult::info(format!("❌ Session error: {}", e)),
    };

    let compaction_config = service.current_compaction_config();
    let total_tokens = session.estimated_total_tokens();
    let max_tokens = compaction_config.max_context_tokens;
    let threshold_pct = (compaction_config.compaction_threshold * 100.0) as u32;
    let usage_pct = (total_tokens as f64 / max_tokens as f64 * 100.0).min(100.0);

    let bar_width = 20;
    let filled = (usage_pct / 100.0 * bar_width as f64) as usize;
    let empty = bar_width - filled;
    let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(empty));

    CommandResult::info(format!(
        "📏 *Context Window*\n\
         \n  {} {:.1}%\
         \n  Tokens: {} / {}\
         \n  Messages: {}\
         \n  Compaction threshold: {}%",
        bar,
        usage_pct,
        total_tokens,
        max_tokens,
        session.message_count(),
        threshold_pct,
    ))
}

fn execute_help() -> CommandResult {
    CommandResult::info(
        "📖 *Available Commands*\n\
         \n  `cmd: new [model]` — Start a fresh session (optionally with a different model)\
         \n  `cmd: reset` — Clear session history\
         \n  `cmd: status` — Show session info (model, tokens, channel)\
         \n  `cmd: compact` — Trigger manual context compaction\
         \n  `cmd: context` — Show context window usage\
         \n  `cmd: model [name]` — Show or switch the current model\
         \n  `cmd: config save` — (deprecated) Config auto-saves to workspace config.toml\
         \n  `cmd: health` — Show model health status\
         \n  `cmd: wake` — Trigger an immediate heartbeat check\
         \n  `cmd: heartbeat` — Show heartbeat status (use `cmd: heartbeat history` for recent results)\
         \n  `cmd: select-agent` — List connected local agents or switch execution target\
         \n  `cmd: tools [prefix]` — List available tools (filter by prefix)\
         \n  `cmd: secret <name> <value>` — Store a credential the agent can use as `[[secret:<name>]]`, without the value reaching the model\
         \n  `cmd: endpoint` — Configure the HTTP endpoints the agent can call (`list`, `cred`; `add`/`auth` are admin-only)\
         \n  `cmd: skill` — Approve WASM skill credentials (`grant`, `grants`, `revoke`); re-approval is required after every skill rebuild\
         \n  `cmd: workflow run <id> [key=value …]` — Start a workflow now; its report posts in this channel\
         \n  `cmd: workflow|playbook|monitor shares|share|unshare|visibility|transfer <id> …` — Who else can reach a definition of yours, or who owns it\
         \n  `cmd: remote-mcp` — Mint, list and revoke your remote MCP tokens for Claude Code / workforce-local (`token [label]`, `tokens`, `token revoke <id|label>`)\
         \n  `cmd: help` — Show this help\
         \n\
         \n  *Aliases (same `cmd:` prefix):* `n`=new, `r`=reset, `s`=status, `gc`=compact, `ctx`=context, `m`=model, `w`=wake, `hb`=heartbeat, `sa`=select-agent, `t`=tools, `h`=help\
         \n\
         \n  _Use the `cmd:` prefix — it works everywhere, including Slack threads (where a `/` triggers autocomplete and won't reach the agent). A plain `/` prefix, e.g. `/status`, still works outside threads._"
            .to_string(),
    )
}

async fn execute_model(service: &Arc<AgentService>, name: Option<&str>) -> CommandResult {
    match name {
        None => {
            // Show current model info
            let model = &service.effective_llm.default_model;
            let aliases: Vec<String> = service
                .config
                .llm
                .aliases
                .iter()
                .map(|(k, v)| format!("`{}` → `{}`", k, v))
                .collect();

            let alias_text = if aliases.is_empty() {
                "none configured".to_string()
            } else {
                aliases.join(", ")
            };

            CommandResult::info(format!(
                "🤖 *Model Info*\n\
                 \n  Current: `{}`\
                 \n  Provider: `{}`\
                 \n  Aliases: {}",
                model, service.effective_llm.provider, alias_text,
            ))
        }
        Some(requested) => {
            let resolved = resolve_model_name(service, requested);
            CommandResult::info(format!(
                "🤖 Model `{}` resolves to `{}`\n\
                 \n  Note: Per-session model switching requires `cmd: new {}`",
                requested, resolved, requested,
            ))
        }
    }
}

async fn execute_health(service: &Arc<AgentService>) -> CommandResult {
    if let Some(ref tracker) = service.health_tracker {
        let status = tracker.status().await;
        if status.is_empty() {
            return CommandResult::info(
                "💚 *Model Health*\n\n  All models healthy — no failures recorded".to_string(),
            );
        }

        let mut lines = vec!["💚 *Model Health*\n".to_string()];
        for (model, health) in &status {
            let icon = if health.is_degraded { "🔴" } else { "🟢" };
            let degraded_info = if health.is_degraded {
                format!(
                    " (degraded {}s ago)",
                    health.degraded_at_secs_ago.unwrap_or(0)
                )
            } else {
                String::new()
            };
            lines.push(format!(
                "  {} `{}` — {} recent failures{}",
                icon, model, health.recent_failures, degraded_info
            ));
        }

        CommandResult::info(lines.join("\n"))
    } else {
        CommandResult::info(
            "ℹ️ Model health tracking not enabled (no failover configured)".to_string(),
        )
    }
}

async fn execute_wake(service: &Arc<AgentService>) -> CommandResult {
    let handle = service.heartbeat_handle.read().clone();
    if let Some(handle) = handle {
        if handle.wake().await {
            CommandResult::info("💓 Heartbeat wake triggered — check will run now")
        } else {
            CommandResult::info("❌ Failed to trigger heartbeat wake (runner not running?)")
        }
    } else {
        CommandResult::info(
            "ℹ️ Heartbeat not enabled. Enable in workforce.toml: `[heartbeat] enabled = true`",
        )
    }
}

async fn execute_heartbeat(service: &Arc<AgentService>, subcommand: Option<&str>) -> CommandResult {
    let handle = service.heartbeat_handle.read().clone();
    if let Some(handle) = handle {
        match subcommand {
            Some("history") | Some("h") => {
                let history = handle.history().await;
                if history.is_empty() {
                    return CommandResult::info(
                        "💓 *Heartbeat History*\n\n  No heartbeats recorded yet",
                    );
                }

                let mut lines = vec!["💓 *Heartbeat History*\n".to_string()];
                for (i, record) in history.iter().rev().take(10).enumerate() {
                    let icon = if record.actionable { "🔔" } else { "✅" };
                    let delivered = if record.delivered {
                        " → delivered"
                    } else {
                        ""
                    };
                    let time = record.timestamp.format("%H:%M:%S UTC");
                    let preview = if record.response.len() > 80 {
                        format!(
                            "{}...",
                            workforce_common::text::truncate_str(&record.response, 77)
                        )
                    } else {
                        record.response.clone()
                    };
                    lines.push(format!(
                        "  {}. {} [{}] {}{}\n     {}",
                        i + 1,
                        icon,
                        time,
                        if record.actionable {
                            "actionable"
                        } else {
                            "ok"
                        },
                        delivered,
                        preview.replace('\n', " "),
                    ));
                }
                CommandResult::info(lines.join("\n"))
            }
            _ => {
                // Show heartbeat status
                let last = handle.last().await;
                let history = handle.history().await;
                let config = &service.config.heartbeat;

                let last_info = if let Some(ref record) = last {
                    let ago = (chrono::Utc::now() - record.timestamp).num_minutes();
                    format!(
                        "{}m ago ({})",
                        ago,
                        if record.actionable {
                            "actionable"
                        } else {
                            "ok"
                        }
                    )
                } else {
                    "never".to_string()
                };

                let total_actionable = history.iter().filter(|r| r.actionable).count();
                let total_delivered = history.iter().filter(|r| r.delivered).count();

                CommandResult::info(format!(
                    "💓 *Heartbeat Status*\n\
                     \n  Enabled: {}\
                     \n  Interval: {}m\
                     \n  Active hours: {} – {}\
                     \n  Deliver: {:?}\
                     \n  Channels: {}\
                     \n  Last: {}\
                     \n  History: {} total, {} actionable, {} delivered\
                     \n\
                     \n  Use `cmd: heartbeat history` for recent results\
                     \n  Use `cmd: wake` to trigger immediately",
                    config.enabled,
                    config.interval_secs / 60,
                    config.active_start,
                    config.active_end,
                    config.deliver,
                    if config.channels.is_empty() {
                        "none (internal only)".to_string()
                    } else {
                        config.channels.join(", ")
                    },
                    last_info,
                    history.len(),
                    total_actionable,
                    total_delivered,
                ))
            }
        }
    } else {
        CommandResult::info(
            "ℹ️ Heartbeat not enabled. Enable in workforce.toml: `[heartbeat] enabled = true`",
        )
    }
}

async fn execute_config_save(_service: &Arc<AgentService>, _scope: Option<&str>) -> CommandResult {
    CommandResult::info(
        "Config is now automatically saved to workspace config.toml when you use \
         system.set_model, system.set_max_tokens, etc. The /config save command is \
         no longer needed. Use settings(action=\"show\") to see the current effective configuration."
            .to_string(),
    )
}

/// Execute the `/select-agent` command: list, switch to local, or switch back to gateway.
async fn execute_select_agent(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    target: &str,
    agent_node_id: Option<&str>,
) -> CommandResult {
    let Some(ref local_agents) = service.local_agents else {
        return CommandResult::info("Local agent support is not enabled on this gateway.");
    };

    match target {
        "list" => {
            let agents = local_agents.list().await;
            if agents.is_empty() {
                CommandResult::info(
                    "No local agents connected.\n\n\
                     To connect a local agent, run:\n\
                     ```\n\
                     workforce local-agent connect --gateway <url> --token <token>\n\
                     ```",
                )
            } else {
                let mut text = String::from("**Connected Local Agents:**\n\n");
                for agent in &agents {
                    text.push_str(&format!(
                        "- **{}** ({})\n  Capabilities: {}\n  Connected: {}\n",
                        agent.name,
                        agent.id,
                        agent.capabilities.join(", "),
                        agent.connected_at.format("%Y-%m-%d %H:%M UTC"),
                    ));
                }
                text.push_str("\nUse `cmd: select-agent local <id>` to switch to a local agent.");
                CommandResult::info(text)
            }
        }
        "local" => {
            let Some(node_id) = agent_node_id else {
                return CommandResult::info(
                    "Usage: `cmd: select-agent local <agent_id>`\n\n\
                     Use `cmd: select-agent` to list connected agents.",
                );
            };

            if !local_agents.is_connected(node_id).await {
                return CommandResult::info(format!("Local agent '{}' is not connected.", node_id));
            }

            // Store selected_local_agent in session metadata
            if let Ok(mut session) =
                service
                    .sessions
                    .get_or_create(channel, user, service.agent_id.as_str())
            {
                if session.metadata.is_null() {
                    session.metadata = serde_json::json!({});
                }
                if let Some(obj) = session.metadata.as_object_mut() {
                    obj.insert(
                        "selected_local_agent".to_string(),
                        serde_json::json!(node_id),
                    );
                    let _ = service.sessions.save(&session);
                }
            }

            CommandResult::info(format!(
                "Switched to local agent **{}**. Commands will now execute on your local machine.\n\n\
                 Use `cmd: select-agent gateway` to switch back.",
                node_id
            ))
        }
        "gateway" | "remote" => {
            // Remove selected_local_agent from session metadata
            if let Ok(mut session) =
                service
                    .sessions
                    .get_or_create(channel, user, service.agent_id.as_str())
            {
                if let Some(obj) = session.metadata.as_object_mut() {
                    obj.remove("selected_local_agent");
                    let _ = service.sessions.save(&session);
                }
            }

            CommandResult::info(
                "Switched back to **gateway** execution. Commands will run on the server.",
            )
        }
        _ => CommandResult::info(
            "Unknown target. Use `cmd: select-agent local <id>` or `cmd: select-agent gateway`.",
        ),
    }
}

/// Execute the `/tools` command: list available tools.
fn execute_tools(service: &Arc<AgentService>, prefix: Option<&str>) -> CommandResult {
    let mut defs = service.tools.definitions();

    if let Some(prefix) = prefix {
        let prefix_lower = prefix.to_lowercase();
        defs.retain(|d| d.name.to_lowercase().starts_with(&prefix_lower));
    }

    if defs.is_empty() {
        let msg = match prefix {
            Some(p) => format!("No tools matching prefix `{}`", p),
            None => "No tools registered".to_string(),
        };
        return CommandResult::info(msg);
    }

    let mut lines = vec![format!("🔧 *Available Tools* ({} total)\n", defs.len())];
    for def in &defs {
        let desc = workforce_common::text::truncate_str(&def.description, 80).to_string();
        lines.push(format!("  `{}` — {}", def.name, desc));
    }

    CommandResult::info(lines.join("\n"))
}

/// Resolve a model name through aliases.
fn resolve_model_name(service: &AgentService, name: &str) -> String {
    service
        .config
        .llm
        .aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use workforce_common::settings::EnvironmentDef;

    fn mcp_def(workspace: &str, variant: &str) -> EnvironmentDef {
        EnvironmentDef {
            mcp: [(workspace.to_string(), variant.to_string())].into(),
            ..Default::default()
        }
    }

    fn active<'a>(name: &'a str, def: &'a EnvironmentDef, is_override: bool) -> Option<Active<'a>> {
        Some(Active {
            name,
            def,
            is_override,
        })
    }

    fn no_catalogue() -> BTreeMap<String, SkillVariants> {
        BTreeMap::new()
    }

    fn status(
        server_id: &str,
        workspace: Option<&str>,
        env: Option<&str>,
        connected: bool,
        needs_auth: bool,
    ) -> crate::hosts::ServerStatus {
        crate::hosts::ServerStatus {
            server_id: server_id.into(),
            workspace: workspace.map(Into::into),
            env: env.map(Into::into),
            connected,
            needs_auth,
        }
    }

    #[test]
    fn board_flags_dangling_mcp_reference_with_a_fix() {
        // Env `staging` pins workspace `harbor`, but no server is installed
        // for it at all. The board must SAY so and name the fix.
        let def = mcp_def("harbor", "staging");
        let out = render_env_board(
            active("staging", &def, true),
            Some("prod"),
            &no_catalogue(),
            &[],
        );
        assert!(out.contains("Active environment: `staging`"), "{out}");
        assert!(out.contains("chosen for this session"), "{out}");
        assert!(out.contains("Default: `prod`"), "{out}");
        assert!(
            out.contains("no MCP server is installed for workspace `harbor`"),
            "{out}"
        );
        assert!(out.contains("dangling reference"), "{out}");
        assert!(out.contains("install/authorise"), "{out}");
    }

    #[test]
    fn board_names_authorise_for_installed_but_unauthorised_server() {
        // THE transcript case: `harbor-staging` IS installed (store row) but
        // never authorised, so it's not connected and not in MEMBERS. The board
        // must say "installed, NOT authorised" and name `mcp.authorize` — NOT
        // "not installed" (the bug the store-vs-registry join fixes).
        let def = mcp_def("harbor", "staging");
        let statuses = vec![status(
            "harbor-staging",
            Some("harbor"),
            Some("staging"),
            false, // not connected
            true,  // installed + OAuth + not connected → needs auth
        )];
        let out = render_env_board(
            active("staging", &def, true),
            Some("prod"),
            &no_catalogue(),
            &statuses,
        );
        assert!(out.contains("installed, NOT authorised"), "{out}");
        assert!(out.contains("mcp.authorize harbor-staging"), "{out}");
        // Must NOT misreport an installed server as missing.
        assert!(!out.contains("no MCP server is installed"), "{out}");
    }

    #[test]
    fn board_shows_connected_pins_and_ungrouped_servers() {
        let def = mcp_def("harbor", "staging");
        let statuses = vec![
            status(
                "harbor-staging",
                Some("harbor"),
                Some("staging"),
                true,
                false,
            ),
            status("hubspot", None, None, true, false),
        ];
        let out = render_env_board(
            active("staging", &def, false),
            Some("staging"),
            &no_catalogue(),
            &statuses,
        );
        assert!(out.contains("this agent's default"), "{out}");
        assert!(out.contains("harbor-staging"), "{out}");
        assert!(out.contains("✅ connected"), "{out}");
        // hubspot is ungrouped → always-active section.
        assert!(out.contains("Ungrouped MCP servers"), "{out}");
        assert!(out.contains("`hubspot`"), "{out}");
        assert!(
            out.contains("Everything this environment binds is reachable"),
            "{out}"
        );
        assert!(!out.contains("dangling"), "{out}");
    }

    #[test]
    fn classify_use_single_token_prefers_environment() {
        // `cmd: use staging` where staging is an environment → switch to it.
        // This is the exact fumble from the transcript (`cmd: use staging` used
        // to die on "Usage: cmd: use staging <env>").
        assert_eq!(
            classify_use(Some("staging"), None, true, false, false),
            UseDispatch::SwitchEnv
        );
    }

    #[test]
    fn classify_use_single_token_workspace_needs_variant() {
        // A workspace name alone can't activate — it needs a variant.
        assert_eq!(
            classify_use(Some("harbor"), None, false, true, false),
            UseDispatch::NeedVariant
        );
    }

    #[test]
    fn classify_use_single_token_unknown_suggests() {
        assert_eq!(
            classify_use(Some("nope"), None, false, false, false),
            UseDispatch::NotFound
        );
    }

    #[test]
    fn classify_use_clear_keywords() {
        assert_eq!(
            classify_use(Some("default"), None, false, false, false),
            UseDispatch::ClearEnv
        );
        // Two-token clear targets the workspace override.
        assert_eq!(
            classify_use(Some("harbor"), Some("default"), false, true, false),
            UseDispatch::ClearWorkspace
        );
    }

    #[test]
    fn classify_use_two_tokens_workspace_override() {
        assert_eq!(
            classify_use(Some("harbor"), Some("staging"), false, true, true),
            UseDispatch::SetWorkspace
        );
        // Workspace exists but lacks that variant.
        assert_eq!(
            classify_use(Some("harbor"), Some("demo"), false, true, false),
            UseDispatch::NoSuchVariant
        );
    }

    #[test]
    fn classify_use_two_tokens_env_mixup_redirects() {
        // `cmd: use staging prod` where staging is an environment, not a
        // workspace → tell them to drop the second word.
        assert_eq!(
            classify_use(Some("staging"), Some("prod"), true, false, false),
            UseDispatch::EnvNotWorkspace
        );
    }

    #[test]
    fn classify_use_no_token_is_usage() {
        assert_eq!(
            classify_use(None, None, false, false, false),
            UseDispatch::Usage
        );
    }

    #[test]
    fn board_reports_wrong_variant_with_installed_alternatives() {
        // Env pins `harbor` → `staging`, but only `prod` is installed.
        let def = mcp_def("harbor", "staging");
        let statuses = vec![status(
            "harbor-prod",
            Some("harbor"),
            Some("prod"),
            true,
            false,
        )];
        let out = render_env_board(
            active("staging", &def, false),
            None,
            &no_catalogue(),
            &statuses,
        );
        assert!(
            out.contains("workspace `harbor` has no `staging` variant (installed: prod)"),
            "{out}"
        );
        assert!(out.contains("No default set"), "{out}");
    }

    #[test]
    fn board_shows_a_skill_set_and_names_an_instance_this_agent_lacks() {
        let def = EnvironmentDef {
            skills: [(
                "linear".to_string(),
                vec!["driftwerk".to_string(), "fabrikam-crm".to_string()],
            )]
            .into(),
            ..Default::default()
        };
        let catalogue: BTreeMap<String, SkillVariants> = [(
            "linear".to_string(),
            SkillVariants::of(["fabrikam-crm".to_string(), "other".to_string()], "other").unwrap(),
        )]
        .into();
        let out = render_env_board(active("work", &def, false), Some("work"), &catalogue, &[]);
        assert!(
            out.contains("skill `linear` → `driftwerk` (default), `fabrikam-crm`"),
            "{out}"
        );
        assert!(out.contains("`driftwerk` not configured"), "{out}");
        assert!(out.contains("dangling reference"), "{out}");
    }

    async fn env_service() -> Arc<AgentService> {
        let service = crate::environments::test_service().await;
        service.tools.set_instance_source(
            "linear",
            Arc::new(
                SkillVariants::of(
                    ["driftwerk".to_string(), "fabrikam-crm".to_string()],
                    "driftwerk",
                )
                .unwrap(),
            ),
        );
        environments::update(&service, env_edit("admin"), |mut envs| {
            envs.enabled = Some(true);
            Ok(envs)
        })
        .await
        .unwrap();
        service
    }

    async fn env(service: &Arc<AgentService>, args: &str) -> String {
        execute_env(service, "C1", "U1", Some(args)).await.text
    }

    fn snapshot(service: &AgentService) -> ResolvedEnvironments {
        service.resolved.snapshot().environments.clone()
    }

    #[tokio::test]
    async fn env_add_binds_a_set_and_remove_drops_the_last_instance() {
        let service = env_service().await;
        env(&service, "create work").await;
        let added = env(&service, "add work skill linear fabrikam-crm driftwerk").await;
        assert!(
            added.contains("`fabrikam-crm` (default), `driftwerk`"),
            "{added}"
        );
        assert_eq!(
            snapshot(&service).defs["work"].skills["linear"],
            vec!["fabrikam-crm", "driftwerk"]
        );
        let refused = env(&service, "add work skill linear acme").await;
        assert!(refused.contains("`acme`"), "{refused}");
        env(&service, "remove work skill linear fabrikam-crm").await;
        env(&service, "remove work skill linear driftwerk").await;
        assert!(snapshot(&service).defs["work"].skills.is_empty());
    }

    #[tokio::test]
    async fn env_default_none_clears_the_default() {
        let service = env_service().await;
        env(&service, "create work").await;
        env(&service, "default work").await;
        assert_eq!(snapshot(&service).default.as_deref(), Some("work"));
        let cleared = env(&service, "default none").await;
        assert!(cleared.contains("no default"), "{cleared}");
        assert_eq!(snapshot(&service).default, None);
    }

    #[tokio::test]
    async fn env_use_of_a_draft_promotes_it_for_the_session() {
        let service = env_service().await;
        environments::update(
            &service,
            env_edit("agent"),
            environments::create("next", None, true),
        )
        .await
        .unwrap();
        let used = env(&service, "use next").await;
        assert!(used.contains("now uses the **next** environment"), "{used}");
        let envs = snapshot(&service);
        assert!(!envs.defs["next"].draft);
        assert_eq!(envs.active(Some("next")).unwrap().name, "next");
    }

    #[tokio::test]
    async fn env_says_how_to_turn_it_on_when_off() {
        let service = crate::environments::test_service().await;
        let off = execute_env(&service, "C1", "U1", Some("list")).await.text;
        assert!(off.contains("environments.enabled = true"), "{off}");
    }

    #[test]
    fn options_name_only_this_agents_environments() {
        let envs = ResolvedEnvironments {
            allowed: true,
            enabled: true,
            default: Some("work".into()),
            defs: [
                ("work".to_string(), EnvironmentDef::default()),
                ("scratch".to_string(), EnvironmentDef::default()),
            ]
            .into(),
        };
        assert_eq!(
            render_environment_options(&envs),
            "**Environments:** `scratch`, `work` (default)"
        );
        assert_eq!(
            render_environment_options(&ResolvedEnvironments::default()),
            "Environments are off for this agent."
        );
    }

    #[test]
    fn stop_intent_fires_on_plain_and_mentioned_stops() {
        // The headline case: "@bot stop" reaches the loop with the bot mention
        // already stripped → bare "stop".
        assert!(is_stop_intent("stop"));
        assert!(is_stop_intent("Stop"));
        assert!(is_stop_intent("stop!"));
        assert!(is_stop_intent("stop."));
        // A stray leading mention (another user / <!here>) is stripped too.
        assert!(is_stop_intent("<@U123456> stop"));
        assert!(is_stop_intent("<!here> stop please"));
        // Politeness prefixes and short objects.
        assert!(is_stop_intent("please stop"));
        assert!(is_stop_intent("stop please"));
        assert!(is_stop_intent("can you stop"));
        assert!(is_stop_intent("stop it"));
        assert!(is_stop_intent("stop that"));
        assert!(is_stop_intent("cancel"));
        assert!(is_stop_intent("cancel that"));
        assert!(is_stop_intent("halt"));
        assert!(is_stop_intent("abort"));
        // Research-specific halts.
        assert!(is_stop_intent("stop the research"));
        assert!(is_stop_intent("stop researching"));
        assert!(is_stop_intent("stop investigating that"));
        // Fixed phrases.
        assert!(is_stop_intent("nevermind"));
        assert!(is_stop_intent("never mind"));
        assert!(is_stop_intent("hold on"));
    }

    #[test]
    fn stop_intent_ignores_real_instructions_and_chatter() {
        // "stop using X" is a genuine instruction, not a halt.
        assert!(!is_stop_intent("stop using metabase"));
        assert!(!is_stop_intent("we should stop using metabase for this"));
        assert!(!is_stop_intent("cancel the meeting at 3pm"));
        assert!(!is_stop_intent("stop the deploy"));
        // Long messages are real requests.
        assert!(!is_stop_intent(
            "can you stop what you're doing and instead look at the other ticket"
        ));
        // Not stop-shaped at all.
        assert!(!is_stop_intent("what's happening?"));
        assert!(!is_stop_intent("research the pricing options"));
        assert!(!is_stop_intent(""));
        assert!(!is_stop_intent("   "));
        // "wait" is intentionally excluded (too ambiguous).
        assert!(!is_stop_intent("wait, what about the other option"));
    }

    /// Slack linkifies an email in the message text it delivers, so the value
    /// arriving here is the wire form, not what the user typed. Storing that
    /// yields a credential that fails at some remote service with a generic
    /// auth error and no way for either side to see why.
    #[test]
    fn a_slack_linkified_email_is_stored_as_the_address() {
        let (name, value) = split_name_value("email <mailto:kh+99@example.com|kh+99@example.com>");
        assert_eq!(name.as_deref(), Some("email"));
        assert_eq!(value.as_deref(), Some("kh+99@example.com"));

        // Same for a bare token and a labelled URL.
        assert_eq!(credential_value("<mailto:a@b.com>"), "a@b.com");
        assert_eq!(credential_value("<https://h/cb|click>"), "https://h/cb");
    }

    /// `& < >` reach us as HTML entities, so a password containing any of them
    /// is altered in transit.
    #[test]
    fn escaped_entities_are_restored_in_a_credential() {
        assert_eq!(credential_value("p&amp;ss&lt;w&gt;rd"), "p&ss<w>rd");
        // `&amp;` is undone last, so an escaped entity stays escaped once.
        assert_eq!(credential_value("a&amp;lt;b"), "a&lt;b");
    }

    #[test]
    fn a_quoted_credential_is_stored_without_its_quotes() {
        let (name, value) = split_name_value("email \"kings+sa2@example.com\"");
        assert_eq!(name.as_deref(), Some("email"));
        assert_eq!(value.as_deref(), Some("kings+sa2@example.com"));

        assert_eq!(credential_value("'pw'"), "pw");
        // Only a matched pair, and never at the cost of the value itself.
        assert_eq!(credential_value("\"unbalanced"), "\"unbalanced");
        assert_eq!(credential_value("say \"hi\" now"), "say \"hi\" now");
        assert_eq!(credential_value("'\""), "'\"");
        assert_eq!(credential_value("\"\""), "\"\"");
        assert_eq!(credential_value("  spaced  "), "spaced");
    }

    /// A password of punctuation must survive untouched — the recovery steps
    /// only fire on shapes a surface actually produces.
    #[test]
    fn a_punctuation_heavy_password_passes_through_intact() {
        let (name, value) = split_name_value("password $%@pass##99!");
        assert_eq!(name.as_deref(), Some("password"));
        assert_eq!(value.as_deref(), Some("$%@pass##99!"));

        // Spaces are part of the value; only the outer edges are trimmed.
        let (_, spaced) = split_name_value("password correct horse battery");
        assert_eq!(spaced.as_deref(), Some("correct horse battery"));

        // A lone `<` or `>` inside the value is not a link token.
        assert_eq!(credential_value("a<b>c"), "a<b>c");
        assert_eq!(credential_value("<not|a|token"), "<not|a|token");
    }

    #[test]
    fn parse_endpoint_keeps_the_args_unsplit() {
        // The executor splits exactly twice, so a credential containing spaces
        // must arrive whole.
        assert_eq!(
            parse_command("cmd: endpoint cred svc password correct horse battery"),
            Some(Command::Endpoint {
                args: Some("cred svc password correct horse battery".to_string())
            }),
        );
        assert_eq!(
            parse_command("/ep list"),
            Some(Command::Endpoint {
                args: Some("list".to_string())
            }),
        );
        assert_eq!(
            parse_command("cmd: endpoint"),
            Some(Command::Endpoint { args: None }),
        );
    }

    #[test]
    fn parse_skill_grant_command() {
        assert_eq!(
            parse_command("cmd: skill grant record-keeper prod_password"),
            Some(Command::Skill {
                args: Some("grant record-keeper prod_password".to_string())
            }),
        );
        assert_eq!(
            parse_command("/skill grants"),
            Some(Command::Skill {
                args: Some("grants".to_string())
            }),
        );
        assert_eq!(
            parse_command("cmd: skill"),
            Some(Command::Skill { args: None })
        );
    }

    #[test]
    fn parse_remote_mcp_forms() {
        assert_eq!(
            parse_command("cmd: remote-mcp token laptop"),
            Some(Command::RemoteMcp {
                args: Some("token laptop".to_string())
            })
        );
        assert_eq!(
            parse_command("/remote-mcp tokens"),
            Some(Command::RemoteMcp {
                args: Some("tokens".to_string())
            })
        );
        assert_eq!(
            parse_command("Cmd: remote-mcp"),
            Some(Command::RemoteMcp { args: None })
        );
    }

    #[test]
    fn parse_mcp_use_and_default_and_list() {
        // `/use` and `cmd: use` both activate a workspace env.
        assert_eq!(
            parse_command("/use harbor staging"),
            Some(Command::McpUse {
                workspace: Some("harbor".to_string()),
                env: Some("staging".to_string()),
            })
        );
        assert_eq!(
            parse_command("cmd: use harbor staging"),
            Some(Command::McpUse {
                workspace: Some("harbor".to_string()),
                env: Some("staging".to_string()),
            })
        );
        // `/mcp use` is the namespaced alias.
        assert_eq!(
            parse_command("/mcp use harbor prod"),
            Some(Command::McpUse {
                workspace: Some("harbor".to_string()),
                env: Some("prod".to_string()),
            })
        );
        // Default lever.
        assert_eq!(
            parse_command("cmd: mcp default harbor prod"),
            Some(Command::McpDefault {
                workspace: Some("harbor".to_string()),
                env: Some("prod".to_string()),
            })
        );
        // Listing.
        assert_eq!(parse_command("/mcp list"), Some(Command::McpList));
        assert_eq!(parse_command("/mcp"), Some(Command::McpList));
        // An unknown subcommand is a usage hint, never a silent listing.
        assert_eq!(
            parse_command("cmd: mcp token"),
            Some(Command::McpUsage {
                subcommand: "token".to_string()
            })
        );
        // Bare `/use` with no args → usage hint path (fields None).
        assert_eq!(
            parse_command("/use"),
            Some(Command::McpUse {
                workspace: None,
                env: None
            })
        );
    }

    #[test]
    fn parse_new() {
        assert_eq!(parse_command("/new"), Some(Command::New { model: None }));
        assert_eq!(
            parse_command("/new claude-sonnet-4"),
            Some(Command::New {
                model: Some("claude-sonnet-4".to_string())
            })
        );
        assert_eq!(
            parse_command("/n fast"),
            Some(Command::New {
                model: Some("fast".to_string())
            })
        );
    }

    #[test]
    fn status_query_matches_commands_and_phrasings() {
        // Short command forms (both prefixes).
        assert!(is_status_query("cmd: status"));
        assert!(is_status_query("/status"));
        assert!(is_status_query("cmd: progress"));
        assert!(is_status_query("cmd: s"));
        // The natural phrasing the user actually types mid-wait.
        assert!(is_status_query("cmd: btw what is happening right now"));
        assert!(is_status_query("cmd: what are you doing?"));
        assert!(is_status_query("cmd: how's it going"));
    }

    #[test]
    fn status_query_ignores_non_status() {
        // Other commands must NOT be hijacked as status probes.
        assert!(!is_status_query("cmd: continue"));
        assert!(!is_status_query("cmd: exit"));
        assert!(!is_status_query("cmd: cc"));
        assert!(!is_status_query("/new fast"));
        // A plain (unprefixed) question is for the agent, not a status probe.
        assert!(!is_status_query("what's happening with the deploy?"));
        assert!(!is_status_query("can you check the status of the build"));
    }

    #[test]
    fn parse_workflow_run() {
        assert_eq!(
            parse_command("cmd: workflow run weekly-report days=7"),
            Some(Command::Workflow {
                args: Some("run weekly-report days=7".to_string())
            })
        );
        assert_eq!(parse_command("/wf"), Some(Command::Workflow { args: None }));
    }

    /// The interactive playbook directives share the noun with the access
    /// grammar, so `parse_command` must leave them alone. Slack intercepts them
    /// before any command parsing; every other surface parses commands first
    /// and would otherwise answer a working directive with usage text that
    /// advertises it.
    #[test]
    fn the_playbook_directives_are_not_claimed_as_access_commands() {
        for text in [
            "cmd: playbook list",
            "/playbook list",
            "cmd: playbook status",
            "cmd: playbook START triage",
            "cmd: playbooks start triage",
        ] {
            assert_eq!(parse_command(text), None, "{text}");
        }
        // `start` without an id is no directive either, so usage is the right
        // answer — and it is the usage that shows the form.
        assert_eq!(
            parse_command("cmd: playbook start"),
            Some(Command::Playbook {
                args: Some("start".to_string())
            })
        );
    }

    /// The three definition kinds share one chat grammar, so each has a command
    /// of its own.
    #[test]
    fn parse_definition_access_commands() {
        assert_eq!(
            parse_command("cmd: playbook shares triage"),
            Some(Command::Playbook {
                args: Some("shares triage".to_string())
            })
        );
        assert_eq!(
            parse_command("/playbooks"),
            Some(Command::Playbook { args: None })
        );
        assert_eq!(
            parse_command("cmd: monitor share triage <@U1> manage"),
            Some(Command::Monitor {
                args: Some("share triage <@U1> manage".to_string())
            })
        );
        assert_eq!(
            parse_command("/monitors"),
            Some(Command::Monitor { args: None })
        );
    }

    #[test]
    fn parse_cmd_prefix_alias() {
        // `cmd:` is the Slack-thread-friendly equivalent of `/`.
        assert_eq!(parse_command("cmd: status"), Some(Command::Status));
        assert_eq!(parse_command("cmd:status"), Some(Command::Status)); // no space
        assert_eq!(parse_command("Cmd: status"), Some(Command::Status)); // autocapitalised
        assert_eq!(parse_command("  cmd:  reset  "), Some(Command::Reset)); // surrounding ws
        assert_eq!(
            parse_command("cmd: new fast"),
            Some(Command::New {
                model: Some("fast".to_string())
            })
        );
        // Aliases resolve through the same path.
        assert_eq!(parse_command("cmd: s"), Some(Command::Status));
        // Unknown command still falls through to the LLM.
        assert_eq!(parse_command("cmd: not-a-command"), None);
        // Bare prefixes are not commands.
        assert_eq!(parse_command("cmd:"), None);
        assert_eq!(parse_command("/"), None);
        // `cmd` without the colon is ordinary prose, not a command.
        assert_eq!(parse_command("cmd status"), None);
    }

    #[test]
    fn command_body_strips_either_prefix() {
        assert_eq!(command_body("/status"), Some("status"));
        assert_eq!(command_body("cmd: status"), Some("status"));
        assert_eq!(command_body("cmd:status"), Some("status"));
        assert_eq!(command_body("hello world"), None);
        assert_eq!(command_body("a"), None); // shorter than the `cmd:` prefix
    }

    #[test]
    fn parse_reset() {
        assert_eq!(parse_command("/reset"), Some(Command::Reset));
        assert_eq!(parse_command("/r"), Some(Command::Reset));
        assert_eq!(parse_command("/clear"), Some(Command::Reset));
    }

    #[test]
    fn parse_status() {
        assert_eq!(parse_command("/status"), Some(Command::Status));
        assert_eq!(parse_command("/s"), Some(Command::Status));
    }

    #[test]
    fn parse_compact() {
        assert_eq!(parse_command("/compact"), Some(Command::Compact));
        assert_eq!(parse_command("/gc"), Some(Command::Compact));
    }

    #[test]
    fn parse_context() {
        assert_eq!(parse_command("/context"), Some(Command::Context));
        assert_eq!(parse_command("/ctx"), Some(Command::Context));
    }

    #[test]
    fn parse_help() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(parse_command("/h"), Some(Command::Help));
        assert_eq!(parse_command("/?"), Some(Command::Help));
    }

    #[test]
    fn parse_model() {
        assert_eq!(parse_command("/model"), Some(Command::Model { name: None }));
        assert_eq!(
            parse_command("/model fast"),
            Some(Command::Model {
                name: Some("fast".to_string())
            })
        );
        assert_eq!(parse_command("/m"), Some(Command::Model { name: None }));
    }

    #[test]
    fn parse_health() {
        assert_eq!(parse_command("/health"), Some(Command::Health));
    }

    #[test]
    fn parse_wake() {
        assert_eq!(parse_command("/wake"), Some(Command::Wake));
        assert_eq!(parse_command("/w"), Some(Command::Wake));
    }

    #[test]
    fn parse_heartbeat() {
        assert_eq!(
            parse_command("/heartbeat"),
            Some(Command::Heartbeat { subcommand: None })
        );
        assert_eq!(
            parse_command("/heartbeat history"),
            Some(Command::Heartbeat {
                subcommand: Some("history".to_string())
            })
        );
        assert_eq!(
            parse_command("/hb"),
            Some(Command::Heartbeat { subcommand: None })
        );
        assert_eq!(
            parse_command("/hb h"),
            Some(Command::Heartbeat {
                subcommand: Some("h".to_string())
            })
        );
    }

    #[test]
    fn parse_select_agent_list() {
        assert_eq!(
            parse_command("/select-agent"),
            Some(Command::SelectAgent {
                target: "list".to_string(),
                agent_node_id: None,
            })
        );
        assert_eq!(
            parse_command("/sa"),
            Some(Command::SelectAgent {
                target: "list".to_string(),
                agent_node_id: None,
            })
        );
    }

    #[test]
    fn parse_select_agent_local() {
        assert_eq!(
            parse_command("/select-agent local my-laptop"),
            Some(Command::SelectAgent {
                target: "local".to_string(),
                agent_node_id: Some("my-laptop".to_string()),
            })
        );
        // local without an id
        assert_eq!(
            parse_command("/select-agent local"),
            Some(Command::SelectAgent {
                target: "local".to_string(),
                agent_node_id: None,
            })
        );
        // alias
        assert_eq!(
            parse_command("/sa local node-42"),
            Some(Command::SelectAgent {
                target: "local".to_string(),
                agent_node_id: Some("node-42".to_string()),
            })
        );
    }

    #[test]
    fn parse_select_agent_gateway() {
        assert_eq!(
            parse_command("/select-agent gateway"),
            Some(Command::SelectAgent {
                target: "gateway".to_string(),
                agent_node_id: None,
            })
        );
        assert_eq!(
            parse_command("/select-agent remote"),
            Some(Command::SelectAgent {
                target: "gateway".to_string(),
                agent_node_id: None,
            })
        );
        assert_eq!(
            parse_command("/sa gateway"),
            Some(Command::SelectAgent {
                target: "gateway".to_string(),
                agent_node_id: None,
            })
        );
    }

    #[test]
    fn parse_not_a_command() {
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("not /a command"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn parse_unknown_command() {
        // Unknown commands return None (pass through to LLM)
        assert_eq!(parse_command("/unknown"), None);
        assert_eq!(parse_command("/foo bar"), None);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_command("/NEW"), Some(Command::New { model: None }));
        assert_eq!(parse_command("/Status"), Some(Command::Status));
        assert_eq!(parse_command("/HELP"), Some(Command::Help));
    }

    #[test]
    fn parse_with_whitespace() {
        assert_eq!(
            parse_command("  /new  "),
            Some(Command::New { model: None })
        );
        assert_eq!(
            parse_command("/new   claude-sonnet-4  "),
            Some(Command::New {
                model: Some("claude-sonnet-4".to_string())
            })
        );
    }

    #[test]
    fn parse_tools() {
        assert_eq!(
            parse_command("/tools"),
            Some(Command::Tools { prefix: None })
        );
    }

    #[test]
    fn parse_tools_with_prefix() {
        assert_eq!(
            parse_command("/tools system"),
            Some(Command::Tools {
                prefix: Some("system".to_string())
            })
        );
    }

    #[test]
    fn parse_tools_alias() {
        assert_eq!(parse_command("/t"), Some(Command::Tools { prefix: None }));
        assert_eq!(
            parse_command("/t github"),
            Some(Command::Tools {
                prefix: Some("github".to_string())
            })
        );
    }

    #[test]
    fn help_output_not_empty() {
        let result = execute_help();
        assert!(result.ephemeral);
        // Commands are presented in the thread-safe `cmd:` form.
        assert!(result.text.contains("cmd: new"));
        assert!(result.text.contains("cmd: status"));
        assert!(result.text.contains("cmd: help"));
        assert!(result.text.contains("cmd: wake"));
        assert!(result.text.contains("cmd: heartbeat"));
        assert!(result.text.contains("cmd: select-agent"));
        assert!(result.text.contains("cmd: tools"));
        assert!(result.text.contains("cmd: secret"));
    }

    #[test]
    fn parse_secret_splits_name_and_value() {
        assert_eq!(
            parse_command("/secret datadog_api_key e72ab16 99e2"),
            Some(Command::Secret {
                name: Some("datadog_api_key".to_string()),
                value: Some("e72ab16 99e2".to_string()),
            })
        );
    }

    #[test]
    fn parse_secret_with_missing_parts() {
        assert_eq!(
            parse_command("/secret"),
            Some(Command::Secret {
                name: None,
                value: None
            })
        );
        assert_eq!(
            parse_command("/secret onlyname"),
            Some(Command::Secret {
                name: Some("onlyname".to_string()),
                value: None
            })
        );
    }

    fn with_global_cache<T>(f: impl FnOnce() -> T) -> T {
        let _installed = crate::secret_cache::testing::GlobalCache::install(Arc::new(
            crate::secret_cache::InMemorySecretCache::with_defaults(),
        ));
        f()
    }

    #[test]
    fn execute_secret_missing_args_returns_usage() {
        let result = execute_secret("slack:T1:C1:slash", "U1", None, None);
        assert!(result.ephemeral);
        assert_eq!(result.text, SECRET_USAGE);
    }

    #[test]
    fn execute_secret_invalid_name_is_rejected() {
        let result = execute_secret("slack:T1:C1:slash", "U1", Some("Bad-Name"), Some("v"));
        assert!(result.text.contains("Invalid secret name"));
    }

    #[test]
    fn parse_then_execute_rejects_reserved_hex_name() {
        // The named-marker regex permits a 32-hex handle, so the realistic
        // guard against shadowing an auto id is in execute_secret.
        let hex32 = "a".repeat(32);
        let Some(Command::Secret { name, value }) =
            parse_command(&format!("/secret {hex32} somesecretvalue"))
        else {
            panic!("expected Command::Secret");
        };
        let result = execute_secret("slack:T1:C1:slash", "U1", name.as_deref(), value.as_deref());
        assert!(result.text.contains("Invalid secret name"));
    }

    #[test]
    fn execute_secret_stores_and_hides_value() {
        with_global_cache(|| {
            let secret = "feedface000000000000000000000001";
            let result = execute_secret(
                "slack:T1:C1:slash",
                "U1",
                Some("datadog_api_key"),
                Some(secret),
            );
            assert!(result.ephemeral);
            assert!(result.text.contains("[[secret:datadog_api_key]]"));
            assert!(
                !result.text.contains(secret),
                "confirmation must not echo the value"
            );

            let cache = global_cache().unwrap();
            let id = MarkerId::named("datadog_api_key").unwrap();
            let resolved = cache
                .lookup("slack:T1:C1:1700000000.5", &id)
                .expect("resolves from a thread in the same channel");
            assert_eq!(resolved.0.expose_secret(), secret);
        });
    }
}

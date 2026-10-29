//! Advisor — second-opinion / review surface for agents.
//!
//! When `[advisor]` is enabled (globally or per-agent), the agent gets an
//! `advisor()` tool it may voluntarily invoke. The tool forwards the
//! in-scope session (transcript or summary, configurable) to a configured
//! reviewer and returns written advice. The reviewer is **purely
//! advisory** — it must never edit, write, or execute. Enforcement varies
//! by backend (see [`AdvisorSource`]); all three are reinforced by a
//! system-prompt block injected by [`advisory_only_directive`].
//!
//! ## Backends
//!
//! All three run a read-only tool loop. For Model and Agent, the loop
//! lives in [`advisor_loop`]; for Claude Code, the loop lives inside the
//! CC subprocess. The advisory-only contract holds across all three
//! because the tool surface is read-only by construction (no edits, no
//! execution, no skill MCPs).
//!
//! - [`AdvisorSource::Model`] — runs the tool loop on the agent's own routes
//!   for the configured model (its `LlmProvider`, or a CLI route) with a
//!   hardcoded read-only tool set (filesystem.read, filesystem.grep,
//!   filesystem.glob, web.fetch, and web.search when a Brave key is
//!   configured). The provider's existing `FailoverProvider` owns model
//!   failover and retries; this module does not wrap a second retry layer
//!   around it.
//! - [`AdvisorSource::ClaudeCode`] — a contained `claude_code` round on the
//!   payer's Claude login: none of the CLI's own tools or settings, only
//!   read-only file, web and knowledge tools (`filesystem.read`/`grep`/
//!   `glob`, `web.fetch`/`search`, `context.fuse`, the `knowledge.*` query
//!   tools) served in process, gated as the principal (see
//!   [`ADVISOR_EXPOSED_TOOLS`]). The read-only contract still holds by
//!   construction: only the listed tools are served, so the agent's
//!   destructive entries (`knowledge.delete`, vault-set, code-edit, …) are
//!   absent, not merely hidden. CC's own retry handles transient failures
//!   and 429 backoff; no extra retry is wrapped here. Claude plan OAuth is
//!   **CLI-only**: using the Messages API with a plan token violates
//!   Anthropic's TOS, so this path must never be refactored to call the API
//!   directly.
//! - [`AdvisorSource::Agent`] — runs the same tool loop as Model but
//!   against the target advisor agent's `LlmProvider`. The target's own
//!   tool registry is intentionally bypassed — only the advisor's
//!   hardcoded read-only set is exposed, so a configured advisor-agent
//!   cannot silently gain side-effecting tools (Slack, Jira, etc.). The
//!   target must have `advisor_mode = true` opted in.
//!
//! ## Cost attribution
//!
//! For [`AdvisorSource::Model`] and [`AdvisorSource::Agent`], cost
//! records carry `identity_id = current_identity_id()` — the human user
//! in scope — with `agent_id` set to the calling agent. When no human
//! user is in scope (background tasks), `identity_id` is `None` and the
//! cost attributes to the agent only. For [`AdvisorSource::ClaudeCode`],
//! cost is captured natively by the CLI (`cc.cost_usd` — $0 for Max
//! subscriptions, real cost otherwise) and tracing-logged rather than
//! recorded through the standard cost tracker, matching the pattern in
//! `claude_code_chat` and `claude_pilot`.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use workforce_agent::tools::filesystem::{Glob, Grep, ReadFile};
use workforce_agent::tools::web::{WebFetchTool, WebSearchTool};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::cost::{CostEntry, TokenCounts};
use workforce_common::secret::{current_key, ApiKeySource};
use workforce_common::session::{MessageRole, Session, SessionMessage};
use workforce_common::settings::{AdvisorConfig, AdvisorPayload, AdvisorSource};
use workforce_common::subscription::{Origin, SubscriptionKind, SubscriptionRoute};

use crate::agent_turn::connect::no_login_notice;
use crate::agent_turn::ToolLoop;
use crate::agent_turn::TOOL_SEARCH;
use crate::claude_code::{probe_cli, ClaudeCodeConfig, CliHealth, Contained};
use crate::mcp_turn::{ClientTimeout, McpExposure, RoundTools};
use crate::serve::AgentService;
use crate::subscription::completion::Workdir;
use crate::subscription::limits::is_rate_limited;
use crate::subscription::Login;

/// System-prompt block appended to advisor calls AND injected into agents
/// that have the `advisor` tool registered. Defence-in-depth: all three
/// backends expose only a hardcoded read-only tool set (file read, grep,
/// glob, web fetch and search) — and on top of that, the prompt says the
/// same thing in plain English.
pub const ADVISOR_SYSTEM_DIRECTIVE: &str = "\
You are operating as an ADVISOR. You are PURELY ADVISORY.

Hard constraints — these are non-negotiable:
- You MUST NOT edit, write, create, or modify any file.
- You MUST NOT execute commands or invoke side-effecting tools.
- You MUST NOT take any action that changes the calling agent's state, \
the repository, the environment, or any external system.
- You read what is in front of you and you respond with a written \
opinion. That is your entire job.

Frame your response as guidance the calling agent can read, evaluate, \
and choose to act on. If you propose changes, describe them in prose — \
do not produce edit/patch artefacts intended for direct application. The \
calling agent owns every action; you own only the recommendation.";

/// Premise-discipline rules. Appended after the advisory-only contract
/// for all sources (Model, Agent, and Claude Code now all have read-only
/// tool surfaces, so the directive is unified).
const ADVISOR_PREMISES_DIRECTIVE: &str = "\
Premise discipline. Before signing off, identify the load-bearing \
premises in the calling agent's plan — the claims that, if false, break \
the recommendation. Rank by reversibility and blast radius and take the \
top 1–2; do not enumerate every assumption.

What counts as evidence: Tool-role outputs in the transcript (files the \
agent actually read, command outputs) AND content you read yourself via \
read tools. Assertions in Assistant or User messages are HYPOTHESES, no \
matter how confidently stated; the agent's question may pre-frame \
conclusions as preconditions — treat those preconditions as premises too.

You have read-only tools available (file read, grep and glob, web fetch, \
and web search when configured). For each premise that is not already \
supported by Tool-role evidence in the transcript, run one targeted \
check yourself before deciding ✅ vs ⚠️. One check per premise — not a \
full investigation. If a check is out of reach (cross-repo, requires \
credentials, needs runtime data only the calling agent has), mark \
⚠️ UNVERIFIED with the precise verification step.

Begin your response with a `## Premises` section. For each load-bearing \
premise, output one line:
- `✅ Verified` — cite the evidence (file path + line, transcript \
reference, or the tool call you ran).
- `⚠️ UNVERIFIED` — state the exact verification step the calling agent \
must run before implementing.

Never sign off silently on an unverified load-bearing claim.";

/// System directive for the reconciling synthesiser in a fan-out ("panel")
/// review. Replaces [`ADVISOR_PREMISES_DIRECTIVE`] for the synth turn: the
/// synthesiser does not re-derive premises from scratch, it UNIONS the
/// panelists' premise sections and must never weaken an `⚠️ UNVERIFIED`
/// entry — that is the load-bearing safety property the single advisor has
/// today, and a lossy "dedup and rank" synth would silently drop it.
const ADVISOR_PANEL_SYNTH_DIRECTIVE: &str = "\
You are the RECONCILING ADVISOR on a review panel. Several advisors each \
reviewed the SAME work through a different lens; their write-ups are given \
to you below. You have the same read-only tools (file read, grep and \
glob, web fetch, and web search when configured) and may run a quick spot-check to \
adjudicate a disagreement — but you do not start a fresh investigation.

Produce ONE coherent second opinion for the calling agent:
- Reconcile the panel: merge what they agree on, and where they conflict, \
say which lens is right and why (cite the evidence). Drop duplicates.
- Preserve every panelist's load-bearing premise. UNION all `## Premises` \
entries into a SINGLE `## Premises` section at the very top of your reply. \
You MUST NOT downgrade any `⚠️ UNVERIFIED` premise to `✅ Verified` — only \
the calling agent running the stated check can clear it. If any lens marked \
a premise UNVERIFIED, it stays UNVERIFIED in your output even if another \
lens did not raise it. Keep the precise verification step on every \
UNVERIFIED line.
- After the unified `## Premises` section, give the reconciled \
recommendation and the top 1–2 risks.

Your job is to reconcile and hand back one clear opinion — nothing more.";

/// The default diverse lenses, surfaced to the calling agent in the tool
/// directive so the model knows good perspectives to request. These are
/// suggestions for the *agent* to pass via `lenses`; the tool does not
/// inject them automatically (single review stays the default).
const SUGGESTED_LENSES: &[&str] = &[
    "correctness",
    "premise verification",
    "security / blast-radius",
    "simpler alternative",
];

/// Build the per-lens focused question. Each panelist sees the same session
/// payload but is told to review through one lens, so the perspectives are
/// diverse rather than partitioned.
fn lens_focus(question: Option<&str>, lens: &str) -> String {
    match question {
        Some(q) if !q.trim().is_empty() => format!(
            "Review the work specifically through this lens: **{lens}**. Focus your \
             review on the {lens} dimension — other advisors on this panel cover the \
             other lenses, so do not try to cover everything.\n\nThe calling agent's \
             question: {q}"
        ),
        _ => format!(
            "Review the work specifically through this lens: **{lens}**. Focus your \
             review on the {lens} dimension — other advisors on this panel cover the \
             other lenses, so do not try to cover everything."
        ),
    }
}

/// Render the synthesiser's input: the calling agent's question (if any)
/// followed by each panelist's lens label and write-up. The synth does not
/// get the full transcript resent (the panelists already grounded their
/// claims with citations) — keeping the reconciliation cheap.
fn render_panel_for_synth(question: Option<&str>, panel: &[(String, String)]) -> String {
    let mut buf = String::with_capacity(2048 + panel.iter().map(|(_, t)| t.len()).sum::<usize>());
    buf.push_str("# Panel reconciliation\n\n");
    if let Some(q) = question.filter(|q| !q.trim().is_empty()) {
        buf.push_str("## Calling agent's question\n\n");
        buf.push_str(q);
        buf.push_str("\n\n");
    }
    buf.push_str(
        "## Panel write-ups\n\nEach section is one advisor's review through its lens. \
         Reconcile them per your instructions.\n\n",
    );
    for (lens, advice) in panel {
        buf.push_str(&format!("### Lens: {lens}\n\n{advice}\n\n"));
    }
    buf
}

/// Build the read-only tool set the advisor's in-process loop exposes
/// for `source = "model"` and `source = "agent"`. The set is built for each
/// consultation, so WebSearch is offered only when the person it runs for
/// has a Brave API key at some vault level (or the install has one).
fn build_advisor_tools(service: &AgentService) -> Vec<Arc<dyn Tool>> {
    let brave =
        crate::credentials::brave_source(service.vault.clone(), service.global_vault.clone());
    make_advisor_tool_set(
        service.workspace.clone(),
        current_key(&brave).is_some().then_some(brave),
    )
}

/// Pure helper exposed for unit testing — assembles the tool set from
/// the resolved workspace and optional Brave key source.
fn make_advisor_tool_set(
    workspace: PathBuf,
    brave_key: Option<ApiKeySource>,
) -> Vec<Arc<dyn Tool>> {
    let allowed = vec![workspace];
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile::new(allowed.clone())),
        Arc::new(Grep::new(allowed.clone())),
        Arc::new(Glob::new(allowed)),
        Arc::new(WebFetchTool::new()),
    ];
    if let Some(key) = brave_key {
        tools.push(Arc::new(WebSearchTool::new(key)));
    }
    tools
}

/// Instructions woven into the agent's own system prompt so a voluntary
/// tool actually gets called. Without this snippet, agents rarely
/// invoke advisor on their own.
pub fn advisor_tool_directive(config: &AdvisorConfig) -> String {
    let backend = match config.source {
        AdvisorSource::Model => format!(
            "an LLM reviewer ({})",
            config.model.as_deref().unwrap_or("default")
        ),
        AdvisorSource::ClaudeCode => "Claude Code (read-only)".to_string(),
        AdvisorSource::Agent => format!(
            "a designated advisor agent ({})",
            config.agent_id.as_deref().unwrap_or("unset")
        ),
    };

    // Fan-out is available on every backend (Claude Code fans out via parallel
    // CLI subprocesses) whenever it is enabled.
    let panel = if config.fanout.enabled {
        format!(
            "\n\n**For a high-stakes review, request a panel.** Pass `lenses` \
            with 2+ distinct perspectives (e.g. {}) and the advisor reviews \
            your session through each one in parallel, then reconciles them \
            into a single opinion. Use this before irreversible or expensive \
            work where one reviewer's blind spot is costly; a single review \
            (omit `lenses`) is the right default everywhere else.",
            SUGGESTED_LENSES
                .iter()
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };

    format!(
        "\n\n## Advisor\n\
        You have access to an `advisor` tool. It forwards your current \
        session to {backend} and returns a written opinion. The advisor \
        is purely advisory — it cannot act on your behalf.{panel}\n\n\
        **Call `advisor` when:**\n\
        - You are about to do substantive, hard-to-reverse work (writing \
        code, committing to a design interpretation, sending external \
        messages, running destructive commands).\n\
        - You are stuck — the same error recurring, an approach not \
        converging, results that don't fit.\n\
        - You are about to change approach mid-task.\n\
        - You are about to declare a non-trivial task done.\n\n\
        **Do NOT call `advisor` when:**\n\
        - The turn is conversational (greetings, small talk, simple \
        Q&A, status updates, acknowledgements).\n\
        - The next action is dictated by tool output you just read \
        (reactive dispatch).\n\
        - You are doing low-stakes orientation work (lookups, reads, \
        listing).\n\n\
        Skipping the advisor for trivial turns is correct, not lazy. An \
        unnecessary advisor call costs latency, context bloat, and \
        user-visible noise.\n\n\
        The advisor result is GUIDANCE, not a directive. Weigh it, \
        decide for yourself, and proceed. If the advisor reports that it \
        is UNAVAILABLE (e.g. `advisor_unavailable: ...`), do not retry \
        it for the same reason — relay the failure to the user in plain \
        language and continue the task without it. The advisor is a \
        checkpoint, never a hard dependency.\n\n\
        **Premises in the advisor's response.** The advisor returns a \
        `## Premises` section. Treat any `⚠️ UNVERIFIED` entry as a \
        blocker — run the verification step it specifies before \
        implementing. If you cannot verify, say so plainly and stop; \
        do not proceed on the assumption that the advisor's other \
        approval covers the gap."
    )
}

/// Typed failure shape — the calling agent's user-facing message ought
/// to be informative, not opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdvisorFailure {
    #[serde(alias = "no_oauth_token")]
    NoLogin {
        #[serde(default)]
        notice: String,
    },
    CliMissing {
        reason: String,
    },
    RateLimit,
    AuthFailed,
    AgentNotFound {
        agent_id: String,
    },
    Transient {
        detail: String,
    },
    Unknown {
        detail: String,
    },
    FailoverDisabled {
        detail: String,
    },
    TurnLimitExceeded {
        limit: u32,
    },
    Misconfigured {
        detail: String,
    },
    Cancelled,
}

impl AdvisorFailure {
    /// Short human-readable label for the calling agent's user-facing
    /// message. The calling agent is instructed to relay this verbatim.
    pub fn human_reason(&self) -> String {
        match self {
            Self::NoLogin { notice } => notice.clone(),
            Self::CliMissing { reason } => format!("Claude Code CLI is not installed or not on PATH: {reason}"),
            Self::RateLimit => "Advisor backend is rate-limited.".to_string(),
            Self::AuthFailed => "Advisor authentication failed (invalid or expired credential).".to_string(),
            Self::AgentNotFound { agent_id } => format!("Configured advisor agent `{agent_id}` not found or `advisor_mode = false`."),
            Self::Transient { detail } => format!("Transient failure contacting advisor backend: {detail}"),
            Self::Unknown { detail } => format!("Advisor backend failed: {detail}"),
            Self::FailoverDisabled { detail } => format!("Claude Code advisor failed and `cc_allow_failover_to_models` is false (cost guard). Exact failure: {detail}"),
            Self::TurnLimitExceeded { limit } => format!("Advisor invocation limit ({limit}) reached for this turn."),
            Self::Misconfigured { detail } => format!("Advisor is enabled but misconfigured: {detail}"),
            Self::Cancelled => "Advisor invocation was cancelled.".to_string(),
        }
    }

    fn from_error_text(error_text: &str) -> Self {
        let lower = error_text.to_ascii_lowercase();
        if is_rate_limited(error_text) {
            return Self::RateLimit;
        }
        if lower.contains("401") || lower.contains("unauthor") || lower.contains("invalid api key")
        {
            return Self::AuthFailed;
        }
        Self::Unknown {
            detail: error_text.to_string(),
        }
    }
}

/// Successful advisor response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorAdvice {
    pub text: String,
    pub source_label: String,
    pub model: Option<String>,
    pub duration_ms: u64,
}

/// Outcome of an advisor dispatch. `Unavailable` is the failure-relay
/// branch — the agent is expected to surface this to the user and
/// continue without an advisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AdvisorOutcome {
    Advice(AdvisorAdvice),
    Unavailable(AdvisorFailure),
}

/// Input forwarded to the advisor backend — a payload (transcript or
/// summary) plus optional focused question.
#[derive(Debug, Clone)]
pub struct AdvisorRequest {
    pub payload: AdvisorPayload,
    pub payload_text: String,
    pub payload_truncated: bool,
    pub question: Option<String>,
    /// Distinct review lenses. When 2+ are present (and fan-out is enabled
    /// for a `model`/`agent` backend), the advisor runs one parallel review
    /// per lens and reconciles them. Empty/one → a single review.
    pub lenses: Vec<String>,
}

/// Build the advisor input text by concatenating the directive, the
/// transcript payload, and the optional question. The advisor backends
/// receive this as a single `user`-role message; the system prompt holds
/// the advisory-only contract separately.
fn render_request(request: &AdvisorRequest) -> String {
    let mut buf = String::with_capacity(request.payload_text.len() + 1024);
    buf.push_str("# Advisor request\n\n");
    if let Some(q) = &request.question {
        buf.push_str("## Focused question\n\n");
        buf.push_str(q);
        buf.push_str("\n\n");
    } else {
        buf.push_str(
            "## Focused question\n\n\
             No specific question provided — review the conversation, \
             flag risks or missed constraints, and recommend a next step \
             or correction if appropriate.\n\n",
        );
    }
    buf.push_str(match request.payload {
        AdvisorPayload::Transcript => "## Session transcript\n\n",
        AdvisorPayload::Summary => "## Session summary\n\n",
    });
    if request.payload_truncated {
        buf.push_str("> Note: transcript exceeded the configured token budget and was truncated (oldest messages dropped).\n\n");
    }
    buf.push_str(&request.payload_text);
    buf
}

/// Materialise the payload from a session. For `Transcript`, every
/// non-tool-noise message is included, oldest-first, with role tags and
/// optional tool-name annotations. For `Summary`, the same content is
/// run through a coarse role-tagged digest (used only when explicitly
/// configured — transcript is the default).
///
/// Token estimation: 4 chars ≈ 1 token (the same heuristic used by
/// `Session::estimated_total_tokens`). When the rendered text would
/// exceed `max_tokens * 4` chars, the oldest messages are dropped first
/// and the truncated flag is set.
pub fn build_payload(
    session: &Session,
    payload_kind: AdvisorPayload,
    max_tokens: u32,
) -> (String, bool) {
    let messages: Vec<&SessionMessage> = session
        .messages
        .iter()
        .filter(|m| m.role != MessageRole::System || m.is_compaction_summary())
        .collect();

    let char_budget = (max_tokens as usize).saturating_mul(4);

    let format_message = |msg: &&SessionMessage| -> String {
        let role_label = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
            MessageRole::Tool => "Tool",
        };
        let tool_info = msg
            .tool_data
            .as_ref()
            .and_then(|td| td.get("name"))
            .and_then(|n| n.as_str())
            .map(|name| format!(" [{name}]"))
            .unwrap_or_default();
        match payload_kind {
            AdvisorPayload::Transcript => {
                format!("### {role_label}{tool_info}\n\n{}\n\n", msg.content)
            }
            AdvisorPayload::Summary => format!(
                "- {role_label}{tool_info}: {}\n",
                truncate_for_summary(&msg.content)
            ),
        }
    };

    let mut rendered: Vec<String> = messages.iter().map(format_message).collect();
    let mut truncated = false;
    let mut total: usize = rendered.iter().map(String::len).sum();

    while total > char_budget && !rendered.is_empty() {
        let head = rendered.remove(0);
        total = total.saturating_sub(head.len());
        truncated = true;
    }

    (rendered.concat(), truncated)
}

fn truncate_for_summary(text: &str) -> String {
    const MAX: usize = 280;
    if text.len() <= MAX {
        return text.replace('\n', " ");
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", text[..end].replace('\n', " "))
}

/// Count `advisor` tool invocations since the most recent user message
/// in the session. Used to enforce `per_turn_invocation_limit`. Stateless
/// — derived from the session transcript so it survives process restarts.
pub fn advisor_invocations_this_turn(session: &Session, tool_name: &str) -> u32 {
    let last_user_idx = session
        .messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.role == MessageRole::User)
        .map(|(i, _)| i);
    let start = last_user_idx.map(|i| i + 1).unwrap_or(0);
    session
        .messages
        .iter()
        .skip(start)
        .filter(|m| {
            m.role == MessageRole::Tool
                && m.tool_data
                    .as_ref()
                    .and_then(|td| td.get("name"))
                    .and_then(|n| n.as_str())
                    == Some(tool_name)
        })
        .count() as u32
}

/// Append the agent-side advisory directive to a base system prompt.
fn build_advisor_system_prompt(config: &AdvisorConfig, base: Option<&str>) -> String {
    [
        base,
        Some(ADVISOR_SYSTEM_DIRECTIVE),
        Some(ADVISOR_PREMISES_DIRECTIVE),
        config.system_prompt_suffix.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// System prompt for the fan-out synthesiser. Keeps the advisory-only
/// contract and the operator suffix, but swaps the per-lens premise
/// directive for the union-preserving panel directive.
fn build_panel_synth_system_prompt(config: &AdvisorConfig) -> String {
    [
        Some(ADVISOR_SYSTEM_DIRECTIVE),
        Some(ADVISOR_PANEL_SYNTH_DIRECTIVE),
        config.system_prompt_suffix.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// Run the advisor tool loop and map its outcome onto `AdvisorOutcome`,
/// recording cost once per round via the provided closure. Used by both
/// `Model` (caller's own LlmProvider) and `Agent` (target agent's provider).
/// The provider's existing `FailoverProvider` owns retries; the loop adds
/// no extra retry layer.
#[allow(clippy::too_many_arguments)]
async fn dispatch_via_loop(
    tool_loop: &dyn ToolLoop,
    cost_target: &Arc<AgentService>,
    system_prompt: String,
    user_message: String,
    tools: Vec<Arc<dyn workforce_agent::tools::Tool>>,
    model: Option<String>,
    max_rounds: u32,
    cancel: Option<CancellationToken>,
    identity_id: Option<&str>,
    source_label: String,
    cost_description: &'static str,
) -> AdvisorOutcome {
    let request = crate::advisor_loop::AdvisorLoopRequest {
        system_prompt,
        user_message,
        tools,
        budget: None,
        model,
        max_rounds,
        cancel,
        label: "advisor",
    };
    let on_usage = |usage: &workforce_agent::llm::types::TokenUsage,
                    model: &str,
                    served: Option<&workforce_common::subscription::Served>| {
        record_advice(
            cost_target,
            identity_id,
            cost_description,
            (model, usage.into()),
            served.cloned(),
        );
    };
    match tool_loop.run(request, &on_usage).await {
        crate::advisor_loop::AdvisorLoopOutcome::Success {
            text,
            model,
            duration_ms,
            ..
        } => AdvisorOutcome::Advice(AdvisorAdvice {
            text,
            source_label,
            model: Some(model),
            duration_ms,
        }),
        crate::advisor_loop::AdvisorLoopOutcome::Cancelled => {
            AdvisorOutcome::Unavailable(AdvisorFailure::Cancelled)
        }
        crate::advisor_loop::AdvisorLoopOutcome::ProviderError(detail) => {
            warn!(target: "advisor", source = %source_label, error = %detail, "advisor dispatch failed");
            AdvisorOutcome::Unavailable(AdvisorFailure::from_error_text(&detail))
        }
    }
}

/// Dispatch on the agent's own routes for the configured model: its
/// `FailoverProvider`, which owns chain-walking and retries, or a CLI route.
async fn dispatch_model(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    dispatch_via_loop(
        crate::agent_turn::tool_loop::tool_loop(service, config.model.as_deref())
            .await
            .as_ref(),
        service,
        build_advisor_system_prompt(config, None),
        render_request(request),
        build_advisor_tools(service),
        config.model.clone(),
        config.max_rounds,
        cancel,
        identity_id,
        "model".to_string(),
        "advisor:model",
    )
    .await
}

// ─── Fan-out (panel) ─────────────────────────────────────────────────────────

/// Wall-clock cap on a single fan-out leg (each lens, and the synthesiser).
/// The advisor is in-turn and the user is waiting, so a hung provider call
/// must not stall the whole turn. Lenses run in parallel, so the worst case is
/// roughly this × 2 (slowest lens + synth). Mirrors the 300s CC ceiling.
const FANOUT_LEG_TIMEOUT: Duration = Duration::from_secs(300);

/// True iff this request should fan out into a panel: fan-out is enabled and
/// the caller supplied 2+ lenses. Fan-out honours the configured backend —
/// `model`/`agent` run in-process loops, `claude_code` runs parallel CLI
/// subprocesses — so a strong reviewer (e.g. Claude Code on Opus) fans out on
/// the strong reviewer, never silently downgrading to a weak model.
fn should_fanout(config: &AdvisorConfig, request: &AdvisorRequest) -> bool {
    config.fanout.enabled && request.lenses.len() >= 2
}

/// Resolve the synthesiser model. An explicit `fanout.synth_model` always
/// wins (the escape hatch for reconciling on a specific/cheaper model).
/// Otherwise the synth stays on the configured backend's reviewer:
/// - `claude_code`: the same Claude Code model as the legs (Opus, etc.) — the
///   panel must NOT downgrade to a weak model just to reconcile.
/// - `agent`: the target agent's own default (`None`).
/// - `model`: the `standard` tier (a stronger reconciler than the cheap
///   per-lens default), falling back to the per-lens model.
fn fanout_synth_model(service: &Arc<AgentService>, config: &AdvisorConfig) -> Option<String> {
    // Everything except the `model`-backend tier resolution is pure and
    // testable; `None` here means "fall through to tier resolution".
    if let Some(choice) = fanout_synth_model_no_tier(config) {
        return choice;
    }
    let tier_key = service.tier_key.read().clone();
    let resolved = service.tier_registry().resolve_model("standard", &tier_key);
    if resolved != "standard" {
        Some(resolved)
    } else {
        config.model.clone()
    }
}

/// The synth-model decision that doesn't need a live provider: an explicit
/// override wins; `agent` uses the target's default (`None`); `claude_code`
/// stays on the configured CC model (no downgrade). Returns `None` only for the
/// `model` backend, signalling the caller to resolve the `standard` tier.
fn fanout_synth_model_no_tier(config: &AdvisorConfig) -> Option<Option<String>> {
    if let Some(m) = &config.fanout.synth_model {
        return Some(Some(m.clone()));
    }
    match config.source {
        AdvisorSource::Agent => Some(None),
        AdvisorSource::ClaudeCode => Some(config.model.clone()),
        AdvisorSource::Model => None,
    }
}

/// Model for each per-lens review. Mirrors the single-review paths: the
/// calling agent's configured model for `source = "model"`, but the target
/// agent's own default (`None`) for `source = "agent"` — the same choice
/// [`dispatch_agent`] and [`fanout_synth_model`] make. Handing `config.model`
/// (a model/CC-tier string) to a target agent's provider would diverge.
fn fanout_leg_model(config: &AdvisorConfig) -> Option<String> {
    match config.source {
        AdvisorSource::Agent => None,
        _ => config.model.clone(),
    }
}

/// Resolve the agent whose routes run the panel (every lens + the synth)
/// and a label base for `source_label`. `model` runs on the calling agent's
/// own routes (`None`); `agent` uses the validated target agent's (mirrors
/// [`dispatch_agent`], including the `advisor_mode` gate).
fn resolve_fanout_provider(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    registry: &crate::hosts::RegistryCell,
) -> Result<(Option<Arc<AgentService>>, String), AdvisorFailure> {
    match config.source {
        AdvisorSource::Model => Ok((None, "model".to_string())),
        AdvisorSource::Agent => {
            let target_id = validate_agent_target(config, service.agent_id.as_str())?;
            let registry = registry.get().and_then(Weak::upgrade).ok_or_else(|| {
                AdvisorFailure::Misconfigured {
                    detail: "agent registry unavailable (advisor wired before registry was set)"
                        .into(),
                }
            })?;
            let target = registry
                .get(target_id)
                .ok_or_else(|| AdvisorFailure::AgentNotFound {
                    agent_id: target_id.to_string(),
                })?;
            if !is_advisor_mode_enabled(&service.config, target_id) {
                return Err(AdvisorFailure::AgentNotFound {
                    agent_id: target_id.to_string(),
                });
            }
            Ok((Some(target), format!("agent:{target_id}")))
        }
        AdvisorSource::ClaudeCode => Err(AdvisorFailure::Misconfigured {
            detail: "fan-out is not supported for the Claude Code backend".into(),
        }),
    }
}

/// The loop a review on `model` runs on: the `target` agent's routes when
/// the panel runs on one, else the calling agent's own.
async fn review_loop(
    service: &Arc<AgentService>,
    target: Option<&Arc<AgentService>>,
    model: Option<&str>,
) -> Arc<dyn ToolLoop> {
    crate::agent_turn::tool_loop::tool_loop(target.unwrap_or(service), model).await
}

/// Build one lens's request: the same session payload, but the focused
/// question tells the panelist which lens to review through. Used both to
/// render the leg's prompt and, on the CC backend, as the failover request so
/// a CC→model fallback re-reviews that lens (not the whole panel).
fn lens_request(request: &AdvisorRequest, lens: &str) -> AdvisorRequest {
    AdvisorRequest {
        payload: request.payload,
        payload_text: request.payload_text.clone(),
        payload_truncated: request.payload_truncated,
        question: Some(lens_focus(request.question.as_deref(), lens)),
        lenses: Vec::new(),
    }
}

/// Render one lens's review input from [`lens_request`].
fn render_lens_request(request: &AdvisorRequest, lens: &str) -> String {
    render_request(&lens_request(request, lens))
}

/// Returned to the agent when synthesis fails: the raw lens reviews, unmerged,
/// with the premise-safety reminder restated so the missing union doesn't
/// silently drop a blocker.
fn degraded_panel_text(panel: &[(String, String)]) -> String {
    let mut buf = String::from(
        "_Automatic reconciliation of the review panel failed, so the individual lens \
         reviews are returned unmerged. Read each one; treat any `⚠️ UNVERIFIED` premise \
         in ANY lens as a blocker and run its verification step before proceeding._\n",
    );
    for (lens, text) in panel {
        buf.push_str(&format!("\n## Lens: {lens}\n\n{text}\n"));
    }
    buf
}

/// Fan out into a diverse-lens panel on the in-process `model`/`agent`
/// backends: one read-only advisor review per lens in parallel, then a
/// reconciling synthesiser that unions their premises. Returns the
/// synthesiser's opinion as a normal [`AdvisorOutcome::Advice`], so the calling
/// agent's contract is identical to a single review. The Claude Code backend
/// fans out via parallel CLI subprocesses in [`dispatch_fanout_cc`].
async fn dispatch_fanout_loop(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    registry: &crate::hosts::RegistryCell,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    let start = Instant::now();
    let (target, label_base) = match resolve_fanout_provider(service, config, registry) {
        Ok(v) => v,
        Err(failure) => return AdvisorOutcome::Unavailable(failure),
    };
    let system_prompt = build_advisor_system_prompt(config, None);

    let leg_model = fanout_leg_model(config);
    let leg_loop = review_loop(service, target.as_ref(), leg_model.as_deref()).await;

    // One read-only review per lens, all concurrent. Each leg is time-bounded so
    // a hung provider call can't stall the turn; a timed-out leg is just a
    // failed lens (the panel degrades, it does not hang).
    let legs = request.lenses.iter().map(|lens| {
        let user_message = render_lens_request(request, lens);
        let system_prompt = system_prompt.clone();
        let cancel = cancel.clone();
        let leg_model = leg_model.clone();
        let leg_loop = leg_loop.as_ref();
        let label_base = &label_base;
        async move {
            let outcome = tokio::time::timeout(
                FANOUT_LEG_TIMEOUT,
                dispatch_via_loop(
                    leg_loop,
                    service,
                    system_prompt,
                    user_message,
                    build_advisor_tools(service),
                    leg_model,
                    config.max_rounds,
                    cancel,
                    identity_id,
                    format!("{label_base}:lens"),
                    "advisor:fanout_lens",
                ),
            )
            .await
            .unwrap_or_else(|_| {
                AdvisorOutcome::Unavailable(AdvisorFailure::Transient {
                    detail: format!("lens timed out after {}s", FANOUT_LEG_TIMEOUT.as_secs()),
                })
            });
            (lens.clone(), outcome)
        }
    });

    let (panel, failures) = partition_panel(futures::future::join_all(legs).await);

    // 0 or 1 surviving lenses need no synthesiser — decide synchronously.
    if let Some(outcome) = degraded_outcome_without_synth(
        &panel,
        &failures,
        &label_base,
        leg_model.clone(),
        start.elapsed().as_millis() as u64,
    ) {
        return outcome;
    }

    let panel_count = panel.len();
    let synth_input = render_panel_for_synth(request.question.as_deref(), &panel);
    let synth_model = fanout_synth_model(service, config);
    let synth_loop = review_loop(service, target.as_ref(), synth_model.as_deref()).await;
    let synth_outcome = tokio::time::timeout(
        FANOUT_LEG_TIMEOUT,
        dispatch_via_loop(
            synth_loop.as_ref(),
            service,
            build_panel_synth_system_prompt(config),
            synth_input,
            build_advisor_tools(service),
            synth_model,
            config.max_rounds,
            cancel,
            identity_id,
            format!("{label_base}:panel({panel_count})"),
            "advisor:fanout_synth",
        ),
    )
    .await
    .unwrap_or_else(|_| {
        AdvisorOutcome::Unavailable(AdvisorFailure::Transient {
            detail: format!(
                "panel synthesis timed out after {}s",
                FANOUT_LEG_TIMEOUT.as_secs()
            ),
        })
    });

    finalize_panel(
        synth_outcome,
        &panel,
        &failures,
        &label_base,
        leg_model,
        start.elapsed().as_millis() as u64,
    )
}

/// Split the joined leg results into successful panel write-ups
/// `(lens, advice text)` and the labels of lenses that failed. Logs each
/// failure with its reason; the labels feed the thin-panel coverage note.
fn partition_panel(results: Vec<(String, AdvisorOutcome)>) -> (Vec<(String, String)>, Vec<String>) {
    let mut panel = Vec::new();
    let mut failures = Vec::new();
    for (lens, outcome) in results {
        match outcome {
            AdvisorOutcome::Advice(advice) => panel.push((lens, advice.text)),
            AdvisorOutcome::Unavailable(failure) => {
                warn!(target: "advisor", %lens, reason = %failure.human_reason(), "fan-out lens failed");
                failures.push(lens);
            }
        }
    }
    (panel, failures)
}

/// Resolve the outcome when 0 or 1 lenses survived (no synthesis needed).
/// Returns `None` when 2+ survived, signalling the caller to run the
/// synthesiser. Pure so every degrade branch is unit-testable without a
/// provider.
fn degraded_outcome_without_synth(
    panel: &[(String, String)],
    failures: &[String],
    label_base: &str,
    model: Option<String>,
    duration_ms: u64,
) -> Option<AdvisorOutcome> {
    match panel.len() {
        // No lens reported — surface a real reason, not a vague panel error.
        0 => Some(AdvisorOutcome::Unavailable(AdvisorFailure::Unknown {
            detail: format!(
                "all {} fan-out lenses failed to report",
                failures.len().max(1)
            ),
        })),
        // One survivor — hand its review back verbatim (its own `## Premises`
        // section is intact, so the safety contract holds).
        1 => {
            warn!(target: "advisor", surviving_lens = %panel[0].0, failed = failures.len(), "fan-out degraded to a single lens");
            Some(AdvisorOutcome::Advice(AdvisorAdvice {
                text: panel[0].1.clone(),
                source_label: format!("{label_base}:panel(1)"),
                model,
                duration_ms,
            }))
        }
        _ => None,
    }
}

/// Turn the synthesiser's outcome into the advice handed back to the agent.
/// On success, append a coverage note iff any lens failed. On synth failure,
/// fall back to the unmerged panel (each lens keeps its own `## Premises`, and
/// `degraded_panel_text` restates the UNVERIFIED-is-a-blocker rule) rather than
/// losing the panel's work. `model` is the per-lens (worker) model, used only
/// for the unsynthesised fallback's display metadata.
fn finalize_panel(
    synth_outcome: AdvisorOutcome,
    panel: &[(String, String)],
    failures: &[String],
    label_base: &str,
    model: Option<String>,
    duration_ms: u64,
) -> AdvisorOutcome {
    let panel_count = panel.len();
    match synth_outcome {
        AdvisorOutcome::Advice(mut advice) => {
            if !failures.is_empty() {
                // A thin panel must not read as full coverage: a failed lens
                // contributed no premises or risks, so the agent must treat the
                // review as non-exhaustive on those dimensions rather than as a
                // clean all-clear.
                advice.text = format!(
                    "{}\n\n---\n_Panel note: {} of {} lenses failed to report ({}); this \
                     synthesis reconciles only the {} that succeeded. The failed lens(es) \
                     contributed no premises or risks — treat this review as non-exhaustive \
                     on those dimensions, not as a clean all-clear._",
                    advice.text,
                    failures.len(),
                    panel_count + failures.len(),
                    failures.join(", "),
                    panel_count,
                );
            }
            advice.duration_ms = duration_ms;
            AdvisorOutcome::Advice(advice)
        }
        AdvisorOutcome::Unavailable(failure) => {
            warn!(target: "advisor", reason = %failure.human_reason(), "fan-out synthesis failed; returning unmerged panel");
            AdvisorOutcome::Advice(AdvisorAdvice {
                text: degraded_panel_text(panel),
                source_label: format!("{label_base}:panel({panel_count}):unsynthesised"),
                model,
                duration_ms,
            })
        }
    }
}

/// Bill `service` for a review on `model` that spent `tokens`, as the plan
/// that `served` it names, if one did.
fn record_advice(
    service: &AgentService,
    identity_id: Option<&str>,
    description: &str,
    (model, tokens): (&str, TokenCounts),
    served: Option<workforce_common::subscription::Served>,
) {
    service.cost_tracker.record(CostEntry {
        task_type: Some("advisor"),
        identity_id,
        description: Some(description),
        served,
        ..CostEntry::new(service.agent_id.as_str(), model, tokens, Origin::Advisor)
    });
}

/// Read-only file, web and knowledge tools the CC advisor is served over MCP.
/// These are DOTTED registry names — `crate::mcp::matches_any` filters the
/// agent's registry against them, so any tool not listed here (notably the
/// destructive `knowledge.index` / `knowledge.delete`) is never served to the
/// advisor. The read-only-by-construction contract holds: unlisted tools are
/// absent, not merely hidden.
const ADVISOR_EXPOSED_TOOLS: &[&str] = &[
    "filesystem.read",
    "filesystem.grep",
    "filesystem.glob",
    "web.fetch",
    "web.search",
    "context.fuse",
    "knowledge.ask",
    "knowledge.symbol",
    "knowledge.explain",
    "knowledge.impact",
    "knowledge.overview",
    "knowledge.graph",
];

/// Who a CC advisor round's tools run as: the person asking, else whoever the
/// calling turn runs as, else the system.
fn advisor_principal(identity_id: Option<&str>) -> String {
    identity_id
        .map(str::to_string)
        .or_else(crate::user_config::current_identity_id)
        .unwrap_or_else(|| "system".to_string())
}

/// Preflight the Claude Code backend once: the Claude login the payer in
/// scope runs on, and the CLI on PATH. A fan-out checks this a single time
/// before spawning N subprocesses rather than N times.
async fn cc_preflight(service: &Arc<AgentService>) -> Result<Login, AdvisorFailure> {
    let login = service
        .logins
        .resolve(SubscriptionKind::Claude)
        .map_err(|no_login| AdvisorFailure::NoLogin {
            notice: no_login_notice(service.agent_id.as_str(), &no_login),
        })?;
    match probe_cli().await {
        CliHealth::Available { .. } => Ok(login),
        CliHealth::Missing { reason } => Err(AdvisorFailure::CliMissing { reason }),
    }
}

/// Run a single Claude Code advisor invocation against a pre-rendered task and
/// system prompt. On CC failure, falls back to a paid model
/// only when `failover_request` is `Some` and the cost guard allows it;
/// `None` (used by the panel synthesiser) returns `Unavailable` so the caller
/// can degrade without re-reviewing the wrong payload through the model loop.
///
/// Claude plan OAuth tokens may only flow through the CLI per Anthropic TOS —
/// using the Messages API with a plan token is forbidden. This path stays on
/// the CLI; do not refactor it to call the API directly. See
/// `feedback_messages_api_forbidden.md`.
#[allow(clippy::too_many_arguments)]
async fn run_cc_advisor(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    login: &Login,
    task: String,
    append_system_prompt: String,
    model: Option<String>,
    status_label: &str,
    source_label: String,
    failover_request: Option<&AdvisorRequest>,
    identity_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    // Gate concurrent CC subprocesses (per-agent fairness + hub-wide ceiling),
    // abortable by `cancel`. Held only for the subprocess: dropped right after
    // it returns (so a fallback-to-model call doesn't occupy a CC slot) and on
    // future-drop when a fan-out leg's timeout fires. A fan-out leg's acquire
    // wait is bounded by its per-leg timeout (future-drop aborts it); the single
    // path queues under saturation (bounded by other slots' 300s ceilings).
    let cc_permits = match crate::cc_budget::acquire(service, cancel.as_ref()).await {
        Some(permits) => permits,
        None => return AdvisorOutcome::Unavailable(AdvisorFailure::Cancelled),
    };

    // Read-only file, web and knowledge tools, gated as the principal and
    // refused any approval: nobody watches an advisor round. The CLI's own
    // tools are off, so every read goes through the gate.
    let mcp = RoundTools::bind(
        service,
        &advisor_principal(identity_id),
        McpExposure::Patterns(
            ADVISOR_EXPOSED_TOOLS
                .iter()
                .map(|t| t.to_string())
                .collect(),
        ),
        cancel.as_ref(),
        ClientTimeout::CLAUDE_CODE,
    )
    .map(|tools| {
        tools
            .unattended()
            .with_model(model.clone().unwrap_or_else(|| "claude-code".to_string()))
    });
    let workdir = match Workdir::create(
        service.subscriptions().completions_dir(),
        &append_system_prompt,
    ) {
        Ok(workdir) => workdir,
        Err(e) => {
            return AdvisorOutcome::Unavailable(AdvisorFailure::Transient {
                detail: format!("advisor workdir: {e}"),
            })
        }
    };
    let cc_config = ClaudeCodeConfig {
        max_turns: Some(config.max_rounds),
        allowed_tools: mcp.iter().flat_map(RoundTools::allowed_tools).collect(),
        model: model.clone(),
        effort: config.cc_effort.clone(),
        mcp: mcp.clone(),
        ..ClaudeCodeConfig::contained(
            workdir.path().to_path_buf(),
            login.clone(),
            Contained {
                tools: mcp.iter().map(|_| TOOL_SEARCH.to_string()).collect(),
                system_prompt_file: workdir.system_prompt(),
                input: vec![serde_json::json!({"type": "text", "text": task})],
                tool_timeout: mcp.as_ref().map(|tools| tools.client_timeout().client),
                exclude_dynamic: false,
            },
            String::new(),
            Duration::from_secs(300),
        )
    };

    let start = Instant::now();
    // Stream the advisor's Claude Code activity onto the caller's status line
    // instead of a line that looks frozen for the minutes it runs.
    let result = crate::claude_code::run_with_subactivity(
        &cc_config,
        service.vault.as_deref(),
        service.global_vault.as_deref(),
        cancel.clone(),
        status_label,
    )
    .await;

    // Subprocess has exited — free the CC slot before any fallback-to-model
    // call (which is in-process and needs no subprocess slot).
    drop(cc_permits);
    drop(workdir);

    if let Ok(cc) = &result {
        login.settle(&cc.round_end(), service.agent_id.as_str());
        record_advice(
            service,
            identity_id,
            status_label,
            (
                &crate::coding_engine::labelled_model(
                    SubscriptionRoute::ClaudeCode.label(),
                    cc.model.as_deref().or(model.as_deref()),
                ),
                TokenCounts {
                    input: cc.input_tokens,
                    output: cc.output_tokens,
                    cache_creation: cc.cache_creation_tokens,
                    cache_read: cc.cache_read_tokens,
                },
            ),
            Some(login.served(SubscriptionRoute::ClaudeCode, service.agent_id.as_str())),
        );
    }

    match result {
        Ok(cc) if cc.exit_code == 0 => AdvisorOutcome::Advice(AdvisorAdvice {
            text: cc.text,
            source_label,
            model: Some(
                model
                    .as_deref()
                    .map(|m| format!("claude-code:{m}"))
                    .unwrap_or_else(|| "claude-code".to_string()),
            ),
            duration_ms: start.elapsed().as_millis() as u64,
        }),
        result => {
            let detail = match result {
                // Reached only for non-zero exit (exit 0 handled above).
                Ok(cc) => format!(
                    "exit {} — {}",
                    cc.exit_code,
                    if cc.stderr_tail.is_empty() {
                        cc.text.chars().take(400).collect::<String>()
                    } else {
                        cc.stderr_tail
                    }
                ),
                Err(e) => e.to_string(),
            };
            match failover_request {
                Some(req) => {
                    cc_failure_or_failover(service, config, req, identity_id, cancel, detail).await
                }
                None => AdvisorOutcome::Unavailable(AdvisorFailure::from_error_text(&detail)),
            }
        }
    }
}

/// Dispatch a single review via the Claude Code CLI subprocess.
async fn dispatch_claude_code(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    let login = match cc_preflight(service).await {
        Ok(login) => login,
        Err(failure) => return AdvisorOutcome::Unavailable(failure),
    };
    run_cc_advisor(
        service,
        config,
        &login,
        render_request(request),
        build_advisor_system_prompt(config, None),
        config.model.clone(),
        "advisor",
        "claude_code".to_string(),
        Some(request),
        identity_id,
        cancel,
    )
    .await
}

/// Fan out into a diverse-lens panel on the Claude Code backend: one read-only
/// CC subprocess per lens in parallel, then a CC synthesiser. Every leg and the
/// synth run on Claude Code (the configured strong reviewer) — the panel only
/// touches a weaker/non-Claude model when one is explicitly configured
/// (`fanout.synth_model`) or via the `cc_allow_failover_to_models` guard.
async fn dispatch_fanout_cc(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    let start = Instant::now();
    let login = match cc_preflight(service).await {
        Ok(login) => login,
        Err(failure) => return AdvisorOutcome::Unavailable(failure),
    };
    let leg_model = config.model.clone();
    // Identical for every leg — build once, not per lens.
    let system_prompt = build_advisor_system_prompt(config, None);
    let legs = request.lenses.iter().map(|lens| {
        let task = render_lens_request(request, lens);
        let system_prompt = system_prompt.clone();
        let failover_request = lens_request(request, lens);
        let leg_model = leg_model.clone();
        let cancel = cancel.clone();
        let login = &login;
        async move {
            let outcome = tokio::time::timeout(
                FANOUT_LEG_TIMEOUT,
                run_cc_advisor(
                    service,
                    config,
                    login,
                    task,
                    system_prompt,
                    leg_model,
                    "advisor:lens",
                    "claude_code:lens".to_string(),
                    Some(&failover_request),
                    identity_id,
                    cancel,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                AdvisorOutcome::Unavailable(AdvisorFailure::Transient {
                    detail: format!("lens timed out after {}s", FANOUT_LEG_TIMEOUT.as_secs()),
                })
            });
            (lens.clone(), outcome)
        }
    });

    let (panel, failures) = partition_panel(futures::future::join_all(legs).await);

    let synth_model = fanout_synth_model(service, config);
    if let Some(outcome) = degraded_outcome_without_synth(
        &panel,
        &failures,
        "claude_code",
        leg_model,
        start.elapsed().as_millis() as u64,
    ) {
        return outcome;
    }

    let panel_count = panel.len();
    let synth_outcome = tokio::time::timeout(
        FANOUT_LEG_TIMEOUT,
        run_cc_advisor(
            service,
            config,
            &login,
            render_panel_for_synth(request.question.as_deref(), &panel),
            build_panel_synth_system_prompt(config),
            synth_model.clone(),
            "advisor:synth",
            format!("claude_code:panel({panel_count})"),
            None, // synth failure → unmerged-panel degrade, not a model re-review
            identity_id,
            cancel,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        AdvisorOutcome::Unavailable(AdvisorFailure::Transient {
            detail: format!(
                "panel synthesis timed out after {}s",
                FANOUT_LEG_TIMEOUT.as_secs()
            ),
        })
    });

    finalize_panel(
        synth_outcome,
        &panel,
        &failures,
        "claude_code",
        synth_model,
        start.elapsed().as_millis() as u64,
    )
}

/// Decide whether a failed CC dispatch should fall back to a paid model.
/// Pure function — returns `Ok(fallback_config)` when failover is
/// allowed, or `Err(FailoverDisabled)` to honour the cost guard.
fn cc_failover_decision(
    config: &AdvisorConfig,
    detail: String,
) -> Result<AdvisorConfig, AdvisorFailure> {
    if !config.cc_allow_failover_to_models {
        return Err(AdvisorFailure::FailoverDisabled { detail });
    }
    Ok(AdvisorConfig {
        source: AdvisorSource::Model,
        model: config
            .cc_fallback_model
            .clone()
            .or_else(|| config.model.clone()),
        ..config.clone()
    })
}

/// When CC fails, respect the `cc_allow_failover_to_models` cost guard.
/// Default behaviour returns `FailoverDisabled` so subscription users
/// don't get billed against a paid API silently.
async fn cc_failure_or_failover(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    cancel: Option<CancellationToken>,
    detail: String,
) -> AdvisorOutcome {
    match cc_failover_decision(config, detail) {
        Err(failure) => {
            if let AdvisorFailure::FailoverDisabled { detail } = &failure {
                warn!(
                    target: "advisor",
                    detail = %detail,
                    "Claude Code advisor failed; failover disabled"
                );
            }
            AdvisorOutcome::Unavailable(failure)
        }
        Ok(fallback_config) => {
            info!(
                target: "advisor",
                model = ?fallback_config.model,
                "Claude Code advisor failed; failing over to model"
            );
            dispatch_model(service, &fallback_config, request, identity_id, cancel).await
        }
    }
}

/// Validate the `source = "agent"` config against the calling agent.
/// Pure function — returns `Ok(target_id)` if dispatch should proceed,
/// or a typed failure to surface to the calling agent.
fn validate_agent_target<'a>(
    config: &'a AdvisorConfig,
    calling_agent_id: &str,
) -> Result<&'a str, AdvisorFailure> {
    let target_id = match config.agent_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => {
            return Err(AdvisorFailure::Misconfigured {
                detail: "advisor.source = \"agent\" but advisor.agent_id is unset".to_string(),
            });
        }
    };
    if calling_agent_id == target_id {
        return Err(AdvisorFailure::Misconfigured {
            detail: format!(
                "advisor.agent_id points at this agent (`{target_id}`); pick a different agent"
            ),
        });
    }
    Ok(target_id)
}

/// Dispatch via another configured agent. The target's `LlmProvider`,
/// model, and tier are used; no tools are forwarded so the advisory-only
/// contract is enforced by construction.
async fn dispatch_agent(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    registry: &crate::hosts::RegistryCell,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    let target_id = match validate_agent_target(config, service.agent_id.as_str()) {
        Ok(id) => id,
        Err(failure) => return AdvisorOutcome::Unavailable(failure),
    };
    let Some(registry) = registry.get().and_then(Weak::upgrade) else {
        return AdvisorOutcome::Unavailable(AdvisorFailure::Misconfigured {
            detail: "agent registry unavailable (advisor wired before registry was set)".into(),
        });
    };
    let Some(target) = registry.get(target_id) else {
        return AdvisorOutcome::Unavailable(AdvisorFailure::AgentNotFound {
            agent_id: target_id.to_string(),
        });
    };
    if !is_advisor_mode_enabled(&service.config, target_id) {
        return AdvisorOutcome::Unavailable(AdvisorFailure::AgentNotFound {
            agent_id: target_id.to_string(),
        });
    }

    dispatch_via_loop(
        crate::agent_turn::tool_loop::tool_loop(&target, None)
            .await
            .as_ref(),
        service,
        build_advisor_system_prompt(config, None),
        render_request(request),
        build_advisor_tools(service),
        None,
        config.max_rounds,
        cancel,
        identity_id,
        format!("agent:{target_id}"),
        "advisor:agent",
    )
    .await
}

/// `true` iff the named agent has `advisor_mode = true` set in the
/// workforce config. Lookup is by id against the config (the registry
/// stores AgentServices but not the original AgentConfig).
fn is_advisor_mode_enabled(config: &workforce_common::settings::WorkforceConfig, id: &str) -> bool {
    config
        .agents
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.advisor_mode)
        .unwrap_or(false)
}

/// Entry point — dispatch the request to the configured backend.
pub async fn dispatch(
    service: &Arc<AgentService>,
    config: &AdvisorConfig,
    request: &AdvisorRequest,
    identity_id: Option<&str>,
    registry: &crate::hosts::RegistryCell,
    cancel: Option<CancellationToken>,
) -> AdvisorOutcome {
    if !config.enabled {
        return AdvisorOutcome::Unavailable(AdvisorFailure::Misconfigured {
            detail: "advisor is not enabled for this agent".to_string(),
        });
    }
    // Diverse-lens panel when the caller asked for it (2+ lenses). Fan-out
    // honours the configured backend: Claude Code spawns parallel CLI
    // subprocesses, model/agent run in-process loops.
    if should_fanout(config, request) {
        return match config.source {
            AdvisorSource::ClaudeCode => {
                dispatch_fanout_cc(service, config, request, identity_id, cancel).await
            }
            _ => {
                dispatch_fanout_loop(service, config, request, identity_id, registry, cancel).await
            }
        };
    }
    match config.source {
        AdvisorSource::Model => dispatch_model(service, config, request, identity_id, cancel).await,
        AdvisorSource::ClaudeCode => {
            dispatch_claude_code(service, config, request, identity_id, cancel).await
        }
        AdvisorSource::Agent => {
            dispatch_agent(service, config, request, identity_id, registry, cancel).await
        }
    }
}

// ─── Tool ──────────────────────────────────────────────────────────────────

/// The `advisor` tool exposed to the calling agent. Each call runs as the
/// agent's `[advisor]` says at that moment, so a change to its own config
/// (on or off, effort, how many lenses) takes effect without a restart.
pub struct AdvisorTool {
    service: Arc<AgentService>,
    registry: crate::hosts::RegistryCell,
}

impl AdvisorTool {
    pub const NAME: &'static str = "advisor";

    pub fn new(service: Arc<AgentService>, registry: crate::hosts::RegistryCell) -> Self {
        Self { service, registry }
    }
}

#[derive(Debug, Deserialize, Default)]
struct AdvisorInput {
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    lenses: Option<Vec<String>>,
}

/// Clean caller-supplied lenses: trim, drop blanks, de-duplicate
/// case-insensitively (preserving order), and cap to `max`. Returns the kept
/// lenses and how many were dropped by the cap (for a no-silent-truncation log).
fn sanitise_lenses(raw: Option<Vec<String>>, max: u32) -> (Vec<String>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut lenses: Vec<String> = raw
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && seen.insert(l.to_ascii_lowercase()))
        .collect();
    let cap = (max.max(1)) as usize;
    let dropped = lenses.len().saturating_sub(cap);
    lenses.truncate(cap);
    (lenses, dropped)
}

// Workflow-step advisor invocations are refused until the operator model is
// revisited; until then the agent must fall through and continue without
// advice.
use crate::serve::is_workflow_step_session as is_workflow_session;

/// Resolve the in-scope session. Prefers a caller-supplied `session_id`,
/// then `CURRENT_SESSION_ID`, then the agent's most-recently-updated
/// session.
fn resolve_session(service: &Arc<AgentService>, explicit: Option<&str>) -> Result<Option<Session>> {
    use workforce_agent::runner::CURRENT_SESSION_ID;
    if let Some(id) = explicit {
        return service.sessions.get(id);
    }
    if let Ok(id) = CURRENT_SESSION_ID.try_with(|s| s.clone()) {
        if let Some(s) = service.sessions.get(&id)? {
            return Ok(Some(s));
        }
    }
    Ok(service
        .sessions
        .list(None, 1)?
        .into_iter()
        .next()
        .map(|s| s.id)
        .and_then(|id| service.sessions.get(&id).ok().flatten()))
}

#[async_trait]
impl Tool for AdvisorTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Get a written second opinion from a stronger or independent reviewer. \
         The advisor sees your current session and returns advice — it cannot \
         act on your behalf. Call before substantive work, when stuck, or \
         before declaring done. Optional `question` focuses the review. For a \
         high-stakes review, pass `lenses` (2+ distinct perspectives, e.g. \
         correctness, security, premise-verification, simpler-alternative): \
         each is reviewed in parallel by a separate advisor and a final \
         advisor reconciles them into one opinion."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "Optional focused question for the advisor (e.g. \"am I missing a constraint?\"). Leave empty for a general review."
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional session ID. Defaults to the in-scope session."
                },
                "lenses": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional distinct review perspectives for a parallel panel (e.g. [\"correctness\", \"security / blast-radius\", \"premise verification\", \"simpler alternative\"]). Pass 2+ to fan out: one advisor reviews per lens in parallel, then a reconciling advisor merges them. Omit (or pass one) for a single review — the right default for most calls."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let config = self.service.effective_advisor();
        if !config.enabled {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "advisor_unavailable": "The advisor is turned off for this agent; answer without it.",
                }),
                start.elapsed().as_millis() as u64,
            ));
        }
        let parsed: AdvisorInput = serde_json::from_value(input).unwrap_or_default();

        let Some(session) = resolve_session(&self.service, parsed.session_id.as_deref())? else {
            return Ok(ToolResult::failure(
                "no active session found for advisor",
                start.elapsed().as_millis() as u64,
            ));
        };

        if is_workflow_session(&session) {
            let failure = AdvisorFailure::Misconfigured {
                detail: "advisor is not available in workflow steps".to_string(),
            };
            return Ok(ToolResult::success(
                serde_json::json!({
                    "advisor_unavailable": failure.human_reason(),
                    "failure_kind": failure,
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        let calls_so_far = advisor_invocations_this_turn(&session, Self::NAME);
        if calls_so_far >= config.per_turn_invocation_limit {
            let failure = AdvisorFailure::TurnLimitExceeded {
                limit: config.per_turn_invocation_limit,
            };
            info!(
                target: "advisor",
                calls_so_far,
                limit = config.per_turn_invocation_limit,
                "advisor turn-limit reached"
            );
            return Ok(ToolResult::success(
                serde_json::json!({
                    "advisor_unavailable": failure.human_reason(),
                    "failure_kind": failure,
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        let (payload_text, truncated) =
            build_payload(&session, config.payload, config.payload_max_tokens);
        let (lenses, dropped) = sanitise_lenses(parsed.lenses, config.fanout.max_lenses);
        if dropped > 0 {
            info!(
                target: "advisor",
                kept = lenses.len(),
                dropped,
                max = config.fanout.max_lenses,
                "advisor fan-out lenses capped"
            );
        }
        let request = AdvisorRequest {
            payload: config.payload,
            payload_text,
            payload_truncated: truncated,
            question: parsed.question,
            lenses,
        };

        let identity_id = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok();

        let outcome = dispatch(
            &self.service,
            &config,
            &request,
            identity_id.as_deref(),
            &self.registry,
            None,
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;
        match outcome {
            AdvisorOutcome::Advice(advice) => Ok(ToolResult::success(
                serde_json::json!({
                    "advice": advice.text,
                    "source": advice.source_label,
                    "model": advice.model,
                    "duration_ms": advice.duration_ms,
                }),
                duration_ms,
            )),
            AdvisorOutcome::Unavailable(failure) => Ok(ToolResult::success(
                serde_json::json!({
                    "advisor_unavailable": failure.human_reason(),
                    "failure_kind": failure,
                }),
                duration_ms,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use workforce_common::secret::fixed_key;
    use workforce_common::session::{Session, SessionMessage};

    fn user_msg(content: &str) -> SessionMessage {
        SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc::now(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    fn assistant_msg(content: &str) -> SessionMessage {
        SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc::now(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    fn tool_msg(tool_name: &str, content: &str) -> SessionMessage {
        SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::Tool,
            content: content.to_string(),
            tool_data: Some(serde_json::json!({"name": tool_name})),
            timestamp: Utc::now(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    fn session_with_messages(messages: Vec<SessionMessage>) -> Session {
        let mut s = Session::new("test", "u", "agent");
        s.messages = messages;
        s
    }

    #[test]
    fn build_payload_transcript_includes_user_and_assistant() {
        let s = session_with_messages(vec![user_msg("hello"), assistant_msg("hi there")]);
        let (text, truncated) = build_payload(&s, AdvisorPayload::Transcript, 10_000);
        assert!(!truncated);
        assert!(text.contains("### User"));
        assert!(text.contains("### Assistant"));
        assert!(text.contains("hello"));
        assert!(text.contains("hi there"));
    }

    #[test]
    fn build_payload_summary_is_compact() {
        let s = session_with_messages(vec![user_msg("hello"), assistant_msg("hi there")]);
        let (text, _) = build_payload(&s, AdvisorPayload::Summary, 10_000);
        assert!(text.contains("- User: hello"));
        assert!(text.contains("- Assistant: hi there"));
    }

    #[test]
    fn build_payload_truncates_oldest_first_when_over_budget() {
        let big = "x".repeat(10_000);
        let s = session_with_messages(vec![
            user_msg("oldest"),
            assistant_msg(&big),
            user_msg("newest"),
        ]);
        let (text, truncated) = build_payload(&s, AdvisorPayload::Transcript, 100);
        assert!(truncated);
        // newest message must survive
        assert!(text.contains("newest"));
        // oldest dropped first
        assert!(!text.contains("oldest"));
    }

    #[test]
    fn advisor_invocations_counter_resets_each_user_turn() {
        let s = session_with_messages(vec![
            user_msg("first ask"),
            tool_msg("advisor", "advice 1"),
            tool_msg("advisor", "advice 2"),
            assistant_msg("done"),
            user_msg("second ask"),
            tool_msg("advisor", "advice 3"),
        ]);
        // After the second user message only one advisor invocation has happened
        assert_eq!(advisor_invocations_this_turn(&s, "advisor"), 1);
    }

    #[test]
    fn advisor_invocations_zero_when_no_user_message() {
        let s = session_with_messages(vec![assistant_msg("hi")]);
        assert_eq!(advisor_invocations_this_turn(&s, "advisor"), 0);
    }

    #[test]
    fn advisor_invocations_ignores_other_tools() {
        let s = session_with_messages(vec![
            user_msg("ask"),
            tool_msg("not_advisor", "x"),
            tool_msg("advisor", "y"),
        ]);
        assert_eq!(advisor_invocations_this_turn(&s, "advisor"), 1);
    }

    #[test]
    fn advisory_only_directive_is_present_in_system_prompt() {
        let cfg = AdvisorConfig::default();
        let prompt = build_advisor_system_prompt(&cfg, Some("base prompt"));
        assert!(prompt.starts_with("base prompt"));
        assert!(prompt.contains("PURELY ADVISORY"));
        assert!(prompt.contains("MUST NOT edit"));
        assert!(prompt.contains("Premise discipline"));
        assert!(prompt.contains("UNVERIFIED"));
        assert!(prompt.contains("## Premises"));
    }

    #[test]
    fn advisory_only_directive_appends_custom_suffix() {
        let cfg = AdvisorConfig {
            system_prompt_suffix: Some("Focus on security review.".to_string()),
            ..Default::default()
        };
        let prompt = build_advisor_system_prompt(&cfg, None);
        assert!(prompt.contains("Focus on security review"));
    }

    #[test]
    fn make_advisor_tool_set_includes_baseline_without_brave_key() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = make_advisor_tool_set(dir.path().to_path_buf(), None)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        for expected in [
            "filesystem.read",
            "filesystem.grep",
            "filesystem.glob",
            "web.fetch",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert!(
            !names.iter().any(|n| n == "web.search"),
            "web.search must be absent without a Brave key"
        );
    }

    #[test]
    fn make_advisor_tool_set_adds_web_search_when_brave_key_configured() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> =
            make_advisor_tool_set(dir.path().to_path_buf(), Some(fixed_key("brave-test-key")))
                .iter()
                .map(|t| t.name().to_string())
                .collect();
        assert!(
            names.iter().any(|n| n == "web.search"),
            "web.search must be present when Brave key is configured; got {names:?}"
        );
    }

    #[test]
    fn cancelled_failure_human_reason() {
        let msg = AdvisorFailure::Cancelled.human_reason();
        assert!(msg.contains("cancelled"));
    }

    #[test]
    fn advisor_exposed_tools_are_read_only() {
        // The CC advisor must never be handed a side-effecting tool. The
        // agent also registers index/delete/remember/forget/sync and
        // code-edit/git tools; the served patterns must name none of them.
        const FORBIDDEN: &[&str] = &[
            "index", "delete", "remember", "forget", "sync", "edit", "write", "git", "build",
            "test", "cargo", "set", "move",
        ];
        for tool in ADVISOR_EXPOSED_TOOLS {
            for bad in FORBIDDEN {
                assert!(
                    !tool.contains(bad),
                    "exposed advisor tool {tool:?} looks side-effecting (matched {bad:?})"
                );
            }
        }
    }

    #[test]
    fn system_prompt_is_unified_across_sources() {
        let needle = "read-only tools available";
        for src in [
            AdvisorSource::Model,
            AdvisorSource::ClaudeCode,
            AdvisorSource::Agent,
        ] {
            let prompt = build_advisor_system_prompt(
                &AdvisorConfig {
                    source: src,
                    ..Default::default()
                },
                None,
            );
            assert!(prompt.contains(needle), "source = {src:?}");
            assert!(prompt.contains("Premise discipline"), "source = {src:?}");
            assert!(prompt.contains("## Premises"), "source = {src:?}");
        }
    }

    #[test]
    fn advisor_tool_directive_describes_backend() {
        let cc = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            ..Default::default()
        };
        let s = advisor_tool_directive(&cc);
        assert!(s.contains("Claude Code"));
        assert!(s.contains("advisor_unavailable"));
    }

    #[test]
    fn advisor_tool_directive_tells_agent_when_to_skip() {
        let cfg = AdvisorConfig::default();
        let s = advisor_tool_directive(&cfg);
        assert!(s.contains("Do NOT call"));
        assert!(s.contains("conversational"));
        assert!(s.contains("Skipping the advisor for trivial turns is correct"));
    }

    #[test]
    fn advisor_tool_directive_mentions_unverified_handling() {
        let cfg = AdvisorConfig::default();
        let s = advisor_tool_directive(&cfg);
        assert!(s.contains("## Premises"));
        assert!(s.contains("UNVERIFIED"));
        assert!(s.contains("blocker"));
    }

    #[test]
    fn a_stored_no_oauth_token_failure_still_reads() {
        let old: AdvisorFailure =
            serde_json::from_value(serde_json::json!({ "kind": "no_oauth_token" })).unwrap();
        assert_eq!(
            old,
            AdvisorFailure::NoLogin {
                notice: String::new()
            }
        );
    }

    #[test]
    fn failure_human_reason_is_actionable() {
        let f = AdvisorFailure::FailoverDisabled {
            detail: "exit 1: spawn failed".into(),
        };
        let msg = f.human_reason();
        assert!(msg.contains("cc_allow_failover_to_models"));
        assert!(msg.contains("spawn failed"));
    }

    #[test]
    fn from_error_text_classifies_rate_limit() {
        let s = "Claude API error: 429 rate limit exceeded";
        assert!(matches!(
            AdvisorFailure::from_error_text(s),
            AdvisorFailure::RateLimit
        ));
    }

    #[test]
    fn from_error_text_classifies_auth_failed() {
        let s = "Request failed: 401 unauthorized";
        assert!(matches!(
            AdvisorFailure::from_error_text(s),
            AdvisorFailure::AuthFailed
        ));
    }

    #[test]
    fn from_error_text_default_unknown() {
        let s = "weird error";
        match AdvisorFailure::from_error_text(s) {
            AdvisorFailure::Unknown { detail } => assert_eq!(detail, s),
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn validate_agent_target_rejects_missing_id() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            agent_id: None,
            ..Default::default()
        };
        match validate_agent_target(&cfg, "alice") {
            Err(AdvisorFailure::Misconfigured { detail }) => {
                assert!(detail.contains("agent_id is unset"));
            }
            other => panic!("expected Misconfigured, got {other:?}"),
        }
    }

    #[test]
    fn validate_agent_target_rejects_empty_id() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            agent_id: Some(String::new()),
            ..Default::default()
        };
        assert!(matches!(
            validate_agent_target(&cfg, "alice"),
            Err(AdvisorFailure::Misconfigured { .. })
        ));
    }

    #[test]
    fn validate_agent_target_rejects_self_reference() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            agent_id: Some("alice".into()),
            ..Default::default()
        };
        match validate_agent_target(&cfg, "alice") {
            Err(AdvisorFailure::Misconfigured { detail }) => {
                assert!(detail.contains("points at this agent"));
            }
            other => panic!("expected self-loop refusal, got {other:?}"),
        }
    }

    #[test]
    fn validate_agent_target_accepts_distinct_agent() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            agent_id: Some("reviewer".into()),
            ..Default::default()
        };
        assert_eq!(validate_agent_target(&cfg, "alice"), Ok("reviewer"));
    }

    #[test]
    fn cc_failover_decision_refuses_when_guard_is_off() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            cc_allow_failover_to_models: false,
            ..Default::default()
        };
        match cc_failover_decision(&cfg, "exit 1: spawn failed".into()) {
            Err(AdvisorFailure::FailoverDisabled { detail }) => {
                assert!(detail.contains("spawn failed"));
            }
            other => panic!("expected FailoverDisabled, got {other:?}"),
        }
    }

    #[test]
    fn cc_failover_decision_returns_model_config_when_guard_is_on() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            cc_allow_failover_to_models: true,
            cc_fallback_model: Some("expensive".into()),
            ..Default::default()
        };
        let fallback = cc_failover_decision(&cfg, "exit 137".into()).expect("failover allowed");
        assert_eq!(fallback.source, AdvisorSource::Model);
        assert_eq!(fallback.model.as_deref(), Some("expensive"));
    }

    #[test]
    fn cc_failover_decision_falls_back_to_model_field_when_no_fallback_model() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            cc_allow_failover_to_models: true,
            cc_fallback_model: None,
            model: Some("default-model".into()),
            ..Default::default()
        };
        let fallback = cc_failover_decision(&cfg, "x".into()).expect("failover allowed");
        assert_eq!(fallback.model.as_deref(), Some("default-model"));
    }

    #[test]
    fn cc_failover_decision_passes_alias_through_verbatim() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            cc_allow_failover_to_models: true,
            cc_fallback_model: None,
            model: Some("opus".into()),
            ..Default::default()
        };
        let fallback = cc_failover_decision(&cfg, "x".into()).expect("failover allowed");
        assert_eq!(fallback.model.as_deref(), Some("opus"));
    }

    #[test]
    fn is_advisor_mode_enabled_reads_agent_config() {
        use workforce_common::settings::{AgentConfig, WorkforceConfig};
        let mut cfg = WorkforceConfig::default();
        cfg.agents = vec![
            AgentConfig {
                id: "reviewer".into(),
                advisor_mode: true,
                ..Default::default()
            },
            AgentConfig {
                id: "scribe".into(),
                advisor_mode: false,
                ..Default::default()
            },
        ];
        assert!(is_advisor_mode_enabled(&cfg, "reviewer"));
        assert!(!is_advisor_mode_enabled(&cfg, "scribe"));
        assert!(!is_advisor_mode_enabled(&cfg, "nonexistent"));
    }

    #[test]
    fn render_request_with_question_includes_focused_section() {
        let r = AdvisorRequest {
            payload: AdvisorPayload::Transcript,
            payload_text: "session text".to_string(),
            payload_truncated: false,
            question: Some("am I missing a constraint?".to_string()),
            lenses: Vec::new(),
        };
        let text = render_request(&r);
        assert!(text.contains("Focused question"));
        assert!(text.contains("am I missing a constraint?"));
        assert!(text.contains("session text"));
    }

    #[test]
    fn is_workflow_session_detects_workflow_channels() {
        let mut s = Session::new("workflow:inst-1:step-a", "u", "agent");
        assert!(is_workflow_session(&s));
        s.channel = "slack-dm-kings".to_string();
        assert!(!is_workflow_session(&s));
        s.channel = "workflow".to_string();
        assert!(!is_workflow_session(&s));
    }

    #[test]
    fn render_request_marks_truncated_payloads() {
        let r = AdvisorRequest {
            payload: AdvisorPayload::Transcript,
            payload_text: "tail".to_string(),
            payload_truncated: true,
            question: None,
            lenses: Vec::new(),
        };
        let text = render_request(&r);
        assert!(text.contains("truncated"));
    }

    // ─── Fan-out (panel) ─────────────────────────────────────────────────

    fn request_with_lenses(lenses: &[&str]) -> AdvisorRequest {
        AdvisorRequest {
            payload: AdvisorPayload::Transcript,
            payload_text: "session".to_string(),
            payload_truncated: false,
            question: None,
            lenses: lenses.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn should_fanout_requires_two_lenses_on_parallelisable_backend() {
        let cfg = AdvisorConfig::default(); // source = Model, fanout enabled
        assert!(should_fanout(
            &cfg,
            &request_with_lenses(&["correctness", "security"])
        ));
        assert!(
            !should_fanout(&cfg, &request_with_lenses(&["only one"])),
            "one lens is a single review, not a panel"
        );
        assert!(!should_fanout(&cfg, &request_with_lenses(&[])));
    }

    #[test]
    fn should_fanout_true_for_claude_code() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            ..Default::default()
        };
        assert!(
            should_fanout(&cfg, &request_with_lenses(&["a", "b"])),
            "a strong CC reviewer must fan out on CC, not be excluded"
        );
    }

    #[test]
    fn should_fanout_false_when_disabled() {
        let cfg = AdvisorConfig {
            fanout: workforce_common::settings::AdvisorFanoutConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!should_fanout(&cfg, &request_with_lenses(&["a", "b"])));
    }

    #[test]
    fn should_fanout_true_for_agent_backend() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            ..Default::default()
        };
        assert!(should_fanout(&cfg, &request_with_lenses(&["a", "b"])));
    }

    #[test]
    fn sanitise_lenses_trims_dedups_and_caps() {
        let (lenses, dropped) = sanitise_lenses(
            Some(vec![
                "  Correctness ".to_string(),
                "correctness".to_string(), // case-insensitive dup
                String::new(),             // blank dropped
                "  ".to_string(),          // whitespace-only dropped
                "security".to_string(),
                "perf".to_string(),
            ]),
            2,
        );
        assert_eq!(
            lenses,
            vec!["Correctness".to_string(), "security".to_string()]
        );
        assert_eq!(dropped, 1, "one over the cap of 2");
    }

    #[test]
    fn sanitise_lenses_treats_zero_cap_as_one() {
        let (lenses, _) = sanitise_lenses(Some(vec!["a".into(), "b".into()]), 0);
        assert_eq!(
            lenses.len(),
            1,
            "max_lenses 0 must not divide by/allow zero"
        );
    }

    #[test]
    fn sanitise_lenses_empty_input() {
        let (lenses, dropped) = sanitise_lenses(None, 4);
        assert!(lenses.is_empty());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn fanout_leg_model_matches_single_path_per_source() {
        // model backend: legs use the configured model (like `dispatch_model`).
        let model_cfg = AdvisorConfig {
            source: AdvisorSource::Model,
            model: Some("standard".into()),
            ..Default::default()
        };
        assert_eq!(fanout_leg_model(&model_cfg).as_deref(), Some("standard"));
        // agent backend: legs use the target's own default (like `dispatch_agent`
        // and `fanout_synth_model`), NOT the calling agent's `model` string.
        let agent_cfg = AdvisorConfig {
            source: AdvisorSource::Agent,
            model: Some("standard".into()),
            ..Default::default()
        };
        assert_eq!(
            fanout_leg_model(&agent_cfg),
            None,
            "agent legs must not inherit a model/CC-tier string meant for another backend"
        );
    }

    #[test]
    fn fanout_synth_model_keeps_claude_code_on_its_model_not_a_downgrade() {
        // CC source: synth stays on the configured CC model (e.g. opus), so the
        // panel never reconciles on a weak model.
        let cc = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            model: Some("opus".into()),
            ..Default::default()
        };
        assert_eq!(
            fanout_synth_model_no_tier(&cc),
            Some(Some("opus".to_string()))
        );

        // Explicit fanout.synth_model is the escape hatch — it wins for any source.
        let cc_override = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            model: Some("opus".into()),
            fanout: workforce_common::settings::AdvisorFanoutConfig {
                synth_model: Some("haiku".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            fanout_synth_model_no_tier(&cc_override),
            Some(Some("haiku".to_string()))
        );

        // Agent source defers to the target's own default.
        let agent = AdvisorConfig {
            source: AdvisorSource::Agent,
            ..Default::default()
        };
        assert_eq!(fanout_synth_model_no_tier(&agent), Some(None));

        // Model source defers to tier resolution (needs a live provider).
        let model = AdvisorConfig {
            source: AdvisorSource::Model,
            ..Default::default()
        };
        assert_eq!(fanout_synth_model_no_tier(&model), None);
    }

    #[test]
    fn fanout_leg_model_keeps_claude_code_on_its_model() {
        // CC legs run on the configured CC model, same as the single CC path.
        let cc = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            model: Some("opus".into()),
            ..Default::default()
        };
        assert_eq!(fanout_leg_model(&cc).as_deref(), Some("opus"));
    }

    #[test]
    fn lens_focus_names_the_lens_and_keeps_question() {
        let f = lens_focus(Some("is the lock correct?"), "concurrency");
        assert!(f.contains("concurrency"));
        assert!(f.contains("is the lock correct?"));
        let f2 = lens_focus(None, "security");
        assert!(f2.contains("security"));
    }

    #[test]
    fn panel_synth_prompt_forbids_downgrading_unverified() {
        let prompt = build_panel_synth_system_prompt(&AdvisorConfig::default());
        assert!(prompt.contains("PURELY ADVISORY"));
        assert!(prompt.contains("UNION all `## Premises`"));
        assert!(
            prompt.contains("MUST NOT downgrade"),
            "the union-preserving safety rule is the whole point of the synth prompt"
        );
        // The per-lens premise directive must NOT leak into the synth prompt —
        // the synth unions, it does not re-derive.
        assert!(!prompt.contains("Premise discipline"));
    }

    #[test]
    fn render_panel_for_synth_includes_each_lens_and_question() {
        let panel = vec![
            ("correctness".to_string(), "looks right".to_string()),
            ("security".to_string(), "watch the token".to_string()),
        ];
        let text = render_panel_for_synth(Some("ship it?"), &panel);
        assert!(text.contains("ship it?"));
        assert!(text.contains("### Lens: correctness"));
        assert!(text.contains("looks right"));
        assert!(text.contains("### Lens: security"));
        assert!(text.contains("watch the token"));
    }

    fn advice_outcome(text: &str) -> AdvisorOutcome {
        AdvisorOutcome::Advice(AdvisorAdvice {
            text: text.to_string(),
            source_label: "model:lens".to_string(),
            model: Some("m".to_string()),
            duration_ms: 1,
        })
    }

    #[test]
    fn partition_panel_splits_advice_from_failures() {
        let results = vec![
            ("correctness".to_string(), advice_outcome("ok")),
            (
                "security".to_string(),
                AdvisorOutcome::Unavailable(AdvisorFailure::RateLimit),
            ),
            ("perf".to_string(), advice_outcome("fast")),
        ];
        let (panel, failures) = partition_panel(results);
        assert_eq!(panel.len(), 2);
        assert_eq!(panel[0], ("correctness".to_string(), "ok".to_string()));
        assert_eq!(failures, vec!["security".to_string()]);
    }

    #[test]
    fn degraded_without_synth_all_failed_is_unavailable() {
        let out = degraded_outcome_without_synth(
            &[],
            &["a".to_string(), "b".to_string()],
            "model",
            None,
            5,
        );
        match out {
            Some(AdvisorOutcome::Unavailable(AdvisorFailure::Unknown { detail })) => {
                assert!(detail.contains("all 2 fan-out lenses failed"));
            }
            other => panic!("expected Unavailable(Unknown), got {other:?}"),
        }
    }

    #[test]
    fn degraded_without_synth_single_lens_returns_it_verbatim() {
        let panel = vec![("correctness".to_string(), "the one review".to_string())];
        let out =
            degraded_outcome_without_synth(&panel, &["security".to_string()], "model", None, 9);
        match out {
            Some(AdvisorOutcome::Advice(a)) => {
                assert_eq!(a.text, "the one review", "survivor handed back unchanged");
                assert_eq!(a.source_label, "model:panel(1)");
                assert_eq!(a.duration_ms, 9);
            }
            other => panic!("expected Advice, got {other:?}"),
        }
    }

    #[test]
    fn degraded_without_synth_returns_none_for_two_plus() {
        let panel = vec![
            ("a".to_string(), "x".to_string()),
            ("b".to_string(), "y".to_string()),
        ];
        assert!(
            degraded_outcome_without_synth(&panel, &[], "model", None, 0).is_none(),
            "2+ survivors must fall through to synthesis"
        );
    }

    #[test]
    fn finalize_panel_appends_coverage_note_only_when_a_lens_failed() {
        let panel = vec![
            ("a".to_string(), "x".to_string()),
            ("b".to_string(), "y".to_string()),
        ];
        // No failures → text untouched.
        let clean = finalize_panel(advice_outcome("reconciled"), &panel, &[], "model", None, 3);
        match clean {
            AdvisorOutcome::Advice(a) => {
                assert_eq!(a.text, "reconciled");
                assert!(!a.text.contains("Panel note"));
                assert_eq!(a.duration_ms, 3);
            }
            other => panic!("expected Advice, got {other:?}"),
        }
        // A failed lens → non-exhaustive coverage note appended.
        let noted = finalize_panel(
            advice_outcome("reconciled"),
            &panel,
            &["security".to_string()],
            "model",
            None,
            3,
        );
        match noted {
            AdvisorOutcome::Advice(a) => {
                assert!(a.text.contains("Panel note"));
                assert!(a.text.contains("security"));
                assert!(a.text.contains("non-exhaustive"));
            }
            other => panic!("expected Advice, got {other:?}"),
        }
    }

    #[test]
    fn finalize_panel_synth_failure_falls_back_to_unmerged_panel() {
        let panel = vec![
            ("correctness".to_string(), "lens one body".to_string()),
            ("security".to_string(), "lens two body".to_string()),
        ];
        let out = finalize_panel(
            AdvisorOutcome::Unavailable(AdvisorFailure::RateLimit),
            &panel,
            &[],
            "model",
            Some("worker-model".to_string()),
            7,
        );
        match out {
            AdvisorOutcome::Advice(a) => {
                // No work is lost: both lens bodies survive...
                assert!(a.text.contains("lens one body"));
                assert!(a.text.contains("lens two body"));
                // ...and the blocker-preservation rule is restated.
                assert!(a.text.contains("UNVERIFIED"));
                assert_eq!(a.source_label, "model:panel(2):unsynthesised");
                assert_eq!(a.model.as_deref(), Some("worker-model"));
            }
            other => panic!("synth failure must still return the panel, got {other:?}"),
        }
    }

    #[test]
    fn degraded_panel_text_restates_unverified_blocker_rule() {
        let panel = vec![("correctness".to_string(), "body".to_string())];
        let text = degraded_panel_text(&panel);
        assert!(text.contains("UNVERIFIED"));
        assert!(text.contains("## Lens: correctness"));
        assert!(text.contains("body"));
    }

    #[test]
    fn directive_advertises_panel_for_model_backend() {
        let cfg = AdvisorConfig::default();
        let s = advisor_tool_directive(&cfg);
        assert!(s.contains("request a panel"));
        assert!(s.contains("lenses"));
    }

    #[test]
    fn directive_advertises_panel_for_claude_code() {
        let cfg = AdvisorConfig {
            source: AdvisorSource::ClaudeCode,
            ..Default::default()
        };
        let s = advisor_tool_directive(&cfg);
        assert!(
            s.contains("request a panel"),
            "CC fans out via parallel subprocesses; the panel must be advertised"
        );
    }

    #[test]
    fn directive_hides_panel_when_fanout_disabled() {
        let cfg = AdvisorConfig {
            fanout: workforce_common::settings::AdvisorFanoutConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!advisor_tool_directive(&cfg).contains("request a panel"));
    }

    #[tokio::test]
    async fn cc_advisor_round_is_contained_and_gated() {
        use crate::claude_code::tests::scripted::{answer, ScriptedClaude};
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = crate::serve::testing::chatgpt_service(tmp.path(), &server, |_| {}).await;
        let claude = ScriptedClaude::new();
        claude.reply(None, &answer("The plan holds."));
        let token = format!("sk-ant-oat01-advisor-{}", uuid::Uuid::new_v4().simple());
        let login = crate::subscription::test_support::claude_login(&token);

        let outcome = claude
            .scoped(run_cc_advisor(
                &service,
                &AdvisorConfig::default(),
                &login,
                "Review the plan.".into(),
                "Advise only.".into(),
                None,
                "advisor",
                "claude_code".into(),
                None,
                None,
                None,
            ))
            .await;

        assert!(
            matches!(&outcome, AdvisorOutcome::Advice(advice) if advice.text == "The plan holds."),
            "{outcome:?}"
        );
        let call = &claude.calls()[0];
        assert_eq!(call.token, token);
        assert_eq!(
            call.flag("--tools"),
            Some(TOOL_SEARCH),
            "no built-in tool but ToolSearch"
        );
        assert!(call.has("--strict-mcp-config"));
        assert_eq!(
            call.flag("--setting-sources"),
            Some(""),
            "no settings or hooks read"
        );
        assert_eq!(call.prompt, "Advise only.");
        assert_eq!(call.text(), "Review the plan.");
        let allowed: Vec<&str> = call
            .flag("--allowedTools")
            .unwrap_or_default()
            .split(',')
            .collect();
        assert!(
            allowed.contains(&"mcp__workforce__filesystem_read")
                && allowed
                    .iter()
                    .all(|tool| tool.starts_with("mcp__workforce__")),
            "every read goes through the gate: {allowed:?}"
        );
        let cwd = claude.recorded(1, "cwd");
        assert!(
            !std::path::Path::new(cwd.trim()).starts_with(&service.workspace),
            "the round runs outside the agent's workspace: {cwd}"
        );
        let rows = crate::coding_engine::tests::cost_rows(&service);
        let row = rows
            .iter()
            .find(|row| row.origin.as_deref() == Some("advisor"))
            .expect("the advice is costed");
        assert_eq!(
            row.credential_tier
                .map(workforce_common::subscription::CredentialTier::as_str),
            Some("agent_shared")
        );
    }
}

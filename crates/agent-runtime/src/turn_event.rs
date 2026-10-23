//! Canonical agent-turn event — one wide structured log per turn.
//!
//! Instead of scattering a start line, a line per tool, and a completion line
//! across every turn, the runner threads a [`TurnTelemetry`] through the turn
//! and emits a single `info!` with the whole picture (tokens, tools, rounds,
//! duration, outcome) at the end. Crucially it is emitted on **both** success
//! and error, so a failing turn — the one worth debugging — keeps full context
//! the `?`-unwind would otherwise discard.

use std::collections::BTreeSet;
use std::time::Instant;

use eyre::Result;
use tracing::info;
use uuid::Uuid;

use crate::llm::types::{ContentBlock, TokenUsage};
use crate::runner::{RunResult, RunnerConfig, SessionContext, ToolInvocation};

/// Cap on the task/response previews — enough to see what a turn was about
/// without bloating the log line or dumping full conversation content.
const PREVIEW_CHARS: usize = 200;

/// Per-turn telemetry accumulated during a run and emitted once at the end.
pub struct TurnTelemetry {
    trace_id: String,
    agent: String,
    role: Option<String>,
    team_id: Option<String>,
    session_id: Option<String>,
    user_id: Option<String>,
    channel_id: Option<String>,
    model: String,
    task: String,
    started: Instant,
    rounds: u32,
    usage: TokenUsage,
    tool_count: u32,
    tool_failures: u32,
    tool_names: BTreeSet<String>,
    /// Estimated tokens of tool-result content *billed* across the turn — the
    /// ToolResult footprint summed at each LLM call, so prior rounds' outputs are
    /// re-counted every call exactly as the provider re-bills them. Paired with the
    /// cumulative `tokens_in` in the emitted event it gives the share of paid
    /// context that is tool output — the lever tool-output compression pulls
    ///.
    tool_output_tokens: u32,
    /// Elapsed ms from turn start to the first stream event of the turn's
    /// first LLM call — user-perceived time-to-first-token. `None` when the
    /// turn never streamed (emitted as 0).
    ttft_ms: Option<u64>,
    cancelled: bool,
}

impl TurnTelemetry {
    /// Capture turn identity at the start of a run. The generated `trace_id`
    /// is shared with the trace writer so the canonical log line and the
    /// detailed trace join on one id.
    pub fn start(config: &RunnerConfig) -> Self {
        let ctx = SessionContext::capture();
        let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
        Self {
            trace_id: Uuid::new_v4().to_string(),
            agent: config.agent_id.to_string(),
            role: config.role.clone(),
            team_id: config.team_id.clone(),
            session_id: non_empty(ctx.id),
            user_id: non_empty(ctx.user_id),
            channel_id: non_empty(ctx.channel_id),
            model: config.model.clone().unwrap_or_default(),
            task: String::new(),
            started: Instant::now(),
            rounds: 0,
            usage: TokenUsage::default(),
            tool_count: 0,
            tool_failures: 0,
            tool_names: BTreeSet::new(),
            tool_output_tokens: 0,
            ttft_ms: None,
            cancelled: false,
        }
    }

    /// The shared turn/trace id, for the trace header.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Capture a preview of the turn's input so the event says what was asked.
    pub fn set_task(&mut self, content: &[ContentBlock]) {
        self.task = content_preview(content);
    }

    /// Record the rounds completed so far (the error path reads this).
    pub fn set_rounds(&mut self, rounds: u32) {
        self.rounds = rounds;
    }

    /// Fold one LLM call's usage and the model that served it.
    pub fn observe_llm(&mut self, usage: &TokenUsage, model: &str) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.usage.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.usage.cache_read_input_tokens += usage.cache_read_input_tokens;
        if !model.is_empty() {
            self.model = model.to_string();
        }
    }

    /// Fold one round's tool invocations into the running counts.
    pub fn observe_tools(&mut self, invocations: &[ToolInvocation]) {
        for inv in invocations {
            self.tool_count += 1;
            if !inv.success {
                self.tool_failures += 1;
            }
            self.tool_names.insert(inv.tool_name.clone());
        }
    }

    /// Fold one round's tool-output token estimate into the running total.
    /// Accumulated incrementally so the emitted event carries the turn's
    /// tool-output volume on both the success and error paths.
    pub fn observe_tool_output_tokens(&mut self, est_tokens: u32) {
        self.tool_output_tokens = self.tool_output_tokens.saturating_add(est_tokens);
    }

    /// Record the arrival of a stream event; only the first sticks. Called
    /// per event by the stream consumer — the guard keeps it one branch.
    pub fn observe_first_stream_event(&mut self) {
        if self.ttft_ms.is_none() {
            self.ttft_ms = Some(self.started.elapsed().as_millis() as u64);
        }
    }

    /// Mark the turn as user-cancelled (distinguishes it from a clean stop).
    pub fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    /// Emit the single canonical event for this turn. On success the
    /// authoritative `RunResult` is the source; on error — where the `?`-unwind
    /// has discarded the runner's locals — the telemetry accumulated up to the
    /// failure carries the context.
    pub fn emit(&self, result: &Result<RunResult>) {
        match result {
            Ok(r) => self.emit_line(
                resolve_outcome(self.cancelled, r.budget_capped),
                "",
                &text_preview(&r.response),
                r.rounds,
                &r.total_usage,
                summarize_tools(&r.tool_invocations),
                r.model_switches.len() as u32,
                &r.model,
            ),
            Err(e) => self.emit_line(
                if self.cancelled { "cancelled" } else { "error" },
                &e.to_string(),
                "",
                self.rounds,
                &self.usage,
                (self.tool_count, self.tool_failures, self.tools_joined()),
                0,
                &self.model,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_line(
        &self,
        outcome: &str,
        error: &str,
        response: &str,
        rounds: u32,
        usage: &TokenUsage,
        tools: (u32, u32, String),
        model_switches: u32,
        model: &str,
    ) {
        let (tool_count, tool_failures, tool_names) = tools;
        info!(
            event = "agent_turn",
            trace_id = %self.trace_id,
            agent = %self.agent,
            role = self.role.as_deref().unwrap_or(""),
            team_id = self.team_id.as_deref().unwrap_or(""),
            session_id = self.session_id.as_deref().unwrap_or(""),
            user_id = self.user_id.as_deref().unwrap_or(""),
            channel = self.channel_id.as_deref().unwrap_or(""),
            model = model,
            task = %self.task,
            response = response,
            rounds = rounds,
            tokens_in = usage.input_tokens,
            tokens_out = usage.output_tokens,
            cache_read = usage.cache_read_input_tokens,
            cache_write = usage.cache_creation_input_tokens,
            tool_output_tokens = self.tool_output_tokens,
            tool_count = tool_count,
            tools = %tool_names,
            tool_failures = tool_failures,
            model_switches = model_switches,
            ttft_ms = self.ttft_ms.unwrap_or(0),
            duration_ms = self.started.elapsed().as_millis() as u64,
            outcome = outcome,
            error = error,
            "agent turn"
        );
    }

    fn tools_joined(&self) -> String {
        self.tool_names
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Reduce a slice of tool invocations to `(count, failures, distinct-names)` —
/// the success path's authoritative tool summary, mirroring the counters the
/// telemetry keeps incrementally for the error path.
fn summarize_tools(invocations: &[ToolInvocation]) -> (u32, u32, String) {
    let failures = invocations.iter().filter(|i| !i.success).count() as u32;
    let names: BTreeSet<&str> = invocations.iter().map(|i| i.tool_name.as_str()).collect();
    let joined = names.into_iter().collect::<Vec<_>>().join(",");
    (invocations.len() as u32, failures, joined)
}

/// Resolve the success-path outcome label. Cancellation wins over a coincident
/// budget cap (the user stopped it regardless), then the round-budget cap, else
/// a clean completion. The error case is handled by the caller.
fn resolve_outcome(cancelled: bool, budget_capped: bool) -> &'static str {
    match (cancelled, budget_capped) {
        (true, _) => "cancelled",
        (false, true) => "max_rounds",
        (false, false) => "ok",
    }
}

/// Single-line, length-bounded preview of free text — whitespace collapsed so a
/// multi-line prompt/response stays one readable attribute, truncated with an
/// ellipsis past [`PREVIEW_CHARS`].
fn text_preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(PREVIEW_CHARS) {
        Some((byte_idx, _)) => format!("{}…", &collapsed[..byte_idx]),
        None => collapsed,
    }
}

/// Preview of the turn's input: the text content blocks, joined and bounded.
/// Non-text blocks (images) contribute a compact marker so an image-only turn
/// isn't blank; system-injected notice blocks (the per-turn context leading
/// the task message) are skipped so the preview shows the user's ask, unless
/// the notice is all there is.
fn content_preview(content: &[ContentBlock]) -> String {
    let shown: Vec<&str> = content
        .iter()
        .filter(|block| !is_notice_block(block))
        .map(block_text)
        .collect();
    let joined = if shown.is_empty() {
        content.iter().map(block_text).collect::<Vec<_>>()
    } else {
        shown
    }
    .join(" ");
    text_preview(&joined)
}

/// Every notice the runner or the hub injects into the conversation — round
/// warnings, wind-downs, the per-turn context block leading the task message
/// — opens with this, which is how the turn preview tells prose the user
/// wrote from prose the system wrote.
pub const RUNNER_NOTICE_PREFIX: &str = "[SYSTEM";

fn is_notice_block(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::Text { text } if text.trim_start().starts_with(RUNNER_NOTICE_PREFIX))
}

fn block_text(block: &ContentBlock) -> &str {
    match block {
        ContentBlock::Text { text } => text.as_str(),
        _ => "[non-text]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool_name: name.to_string(),
            input: serde_json::Value::Null,
            output: serde_json::Value::Null,
            success,
            policy_decision: "allow".to_string(),
            duration_ms: 0,
            denial_reason: None,
            error: None,
        }
    }

    fn telemetry() -> TurnTelemetry {
        TurnTelemetry::start(&RunnerConfig::default())
    }

    #[test]
    fn observe_llm_accumulates_usage_and_tracks_model() {
        let mut t = telemetry();
        t.observe_llm(
            &TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 2,
            },
            "claude-a",
        );
        t.observe_llm(
            &TokenUsage {
                input_tokens: 3,
                output_tokens: 4,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1,
            },
            "claude-b",
        );
        assert_eq!(t.usage.input_tokens, 13);
        assert_eq!(t.usage.output_tokens, 9);
        assert_eq!(t.usage.cache_read_input_tokens, 3);
        assert_eq!(t.usage.cache_creation_input_tokens, 1);
        assert_eq!(t.model, "claude-b");
    }

    #[test]
    fn observe_tools_counts_dedups_and_tracks_failures() {
        let mut t = telemetry();
        t.observe_tools(&[tool("read", true), tool("write", false)]);
        t.observe_tools(&[tool("read", true)]);
        assert_eq!(t.tool_count, 3);
        assert_eq!(t.tool_failures, 1);
        assert_eq!(t.tools_joined(), "read,write");
    }

    #[test]
    fn observe_tool_output_tokens_accumulates_across_rounds() {
        let mut t = telemetry();
        t.observe_tool_output_tokens(120);
        t.observe_tool_output_tokens(30);
        assert_eq!(t.tool_output_tokens, 150);
    }

    #[test]
    fn summarize_tools_matches_incremental_observation() {
        let invocations = [tool("read", true), tool("write", false), tool("read", true)];
        let (count, failures, names) = summarize_tools(&invocations);
        assert_eq!(count, 3);
        assert_eq!(failures, 1);
        assert_eq!(names, "read,write");

        let mut t = telemetry();
        t.observe_tools(&invocations);
        assert_eq!(
            (t.tool_count, t.tool_failures, t.tools_joined()),
            (count, failures, names)
        );
    }

    #[test]
    fn text_preview_collapses_whitespace_and_truncates() {
        assert_eq!(text_preview("  hello\n\n  world  "), "hello world");
        let long = "x".repeat(PREVIEW_CHARS + 50);
        let preview = text_preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1); // + ellipsis
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn content_preview_joins_text_and_marks_non_text() {
        let blocks = vec![
            ContentBlock::Text {
                text: "summarise this".to_string(),
            },
            ContentBlock::Thinking {
                thinking: "internal".to_string(),
                signature: None,
            },
        ];
        assert_eq!(content_preview(&blocks), "summarise this [non-text]");
    }

    #[test]
    fn content_preview_skips_runner_notice_blocks() {
        let blocks = vec![
            ContentBlock::Text {
                text: format!("{} CONTEXT] clock, notes", RUNNER_NOTICE_PREFIX),
            },
            ContentBlock::Text {
                text: "summarise this".to_string(),
            },
        ];
        assert_eq!(content_preview(&blocks), "summarise this");
    }

    #[test]
    fn content_preview_keeps_a_notice_that_is_the_whole_task() {
        let notice = format!("{}: resume the interrupted turn]", RUNNER_NOTICE_PREFIX);
        let blocks = vec![ContentBlock::Text {
            text: notice.clone(),
        }];
        assert_eq!(content_preview(&blocks), notice);
    }

    #[test]
    fn resolve_outcome_precedence() {
        assert_eq!(resolve_outcome(true, true), "cancelled");
        assert_eq!(resolve_outcome(true, false), "cancelled");
        assert_eq!(resolve_outcome(false, true), "max_rounds");
        assert_eq!(resolve_outcome(false, false), "ok");
    }
}

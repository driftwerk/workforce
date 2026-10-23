//! In-process tool loop for the advisor.
//!
//! Used by `source = "model"` and `source = "agent"`. Mirrors the main
//! runner's "call → tool_use → execute → tool_result → loop" contract but
//! drops everything the advisor doesn't need (policy, session bookkeeping,
//! skill MCPs, model routing). Allowlist is the tool set passed in;
//! cancellation is honoured between rounds; cost is recorded per round
//! via the caller-supplied `on_usage` closure.
//!
//! The CC path (`dispatch_claude_code`) does NOT use this — CC owns its
//! own loop inside the subprocess.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::{
    ContentBlock, LlmMessage, LlmToolDef, Role, StopReason, TokenUsage,
};
use workforce_agent::runner::{
    resolve_tool_call_logged, to_llm_name, tool_not_found_message, with_served_log, ToolCallOrigin,
};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::subscription::Served;

/// Input shape for the advisor tool loop.
#[derive(Clone)]
pub struct AdvisorLoopRequest {
    pub system_prompt: String,
    pub user_message: String,
    pub tools: Vec<Arc<dyn Tool>>,
    pub model: Option<String>,
    pub max_rounds: u32,
    /// Time budget split between gathering evidence and writing the answer.
    /// `None` keeps the round cap as the only bound.
    pub budget: Option<LoopBudget>,
    pub cancel: Option<CancellationToken>,
    /// Identifies the caller (e.g. "editor", "claims", "conclusion", "advisor")
    /// so each round and tool call is attributable in the `advisor_loop` logs —
    /// these passes are otherwise black boxes.
    pub label: &'static str,
}

/// How a loop's time is divided between gathering evidence and reporting it.
///
/// A single wall-clock timeout around the whole loop cannot express this: expiry
/// during the write-up discards every round of research and returns nothing, so
/// the caller is told the pass could not run when in fact it had gathered
/// everything and was mid-sentence.
///
/// Splitting it makes a long pass degrade into a narrower answer instead of
/// silence — what was reached is reported, what was not is left unresolved for
/// the caller to describe honestly.
#[derive(Debug, Clone, Copy)]
pub struct LoopBudget {
    /// Gathering must stop by here.
    pub research_deadline: Instant,
    /// Held back for the final, tool-less call that writes the answer. This is a
    /// guarantee, not a target: the loop declines to start a round it predicts
    /// would eat into it, and the write-up is bounded by it independently of
    /// when it begins.
    pub write_up_reserve: Duration,
}

/// Outcome of the advisor tool loop.
pub enum AdvisorLoopOutcome {
    Success {
        text: String,
        model: String,
        duration_ms: u64,
        /// The final call was forced by `research_deadline` rather than by the
        /// model finishing or by the round cap — so the answer covers only the
        /// evidence gathered before the deadline, and a caller that reports
        /// coverage must say so.
        truncated: bool,
        /// The final call was forced by the round cap while the model was
        /// still asking for tools.
        capped: bool,
        rounds_used: u32,
    },
    Cancelled,
    ProviderError(String),
}

/// Run the advisor tool loop.
///
/// `on_usage` is called once per round with the provider's reported
/// `TokenUsage`, the resolved model id and the subscription that served the
/// round (if one did), so the caller can attribute cost to the right
/// agent/identity exactly as it does for single-shot dispatch.
pub async fn run_advisor_tool_loop<F>(
    provider: &dyn LlmProvider,
    request: AdvisorLoopRequest,
    on_usage: F,
) -> AdvisorLoopOutcome
where
    F: Fn(&TokenUsage, &str, Option<&Served>) + Send + Sync,
{
    let start = Instant::now();
    let max_rounds = request.max_rounds.max(1);
    let tool_defs = build_tool_defs(&request.tools);
    // A registry, not a name map: the advisor resolves model-emitted names
    // through exactly the same path as the runner, including the repair for
    // action-dispatch tools flattened into `tool_action`.
    let registry = ToolRegistry::from_tools(request.tools.iter().map(Arc::clone));
    let mut history: Vec<LlmMessage> = vec![LlmMessage::user(request.user_message)];
    let mut last_model = String::new();
    // The longest research round observed. Rounds vary by an order of magnitude
    // with model and tool, so the only honest estimate of the next one's cost is
    // the worst seen so far.
    let mut longest_round = Duration::ZERO;

    for round in 0..max_rounds {
        if request
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return AdvisorLoopOutcome::Cancelled;
        }
        // Stop gathering while there is still time to gather AND report.
        //
        // Testing `now >= deadline` alone is not enough, and this is the bug it
        // replaces: the check only runs between rounds, so a round that starts
        // legally and runs long sails past the deadline and eats the write-up's
        // reserve. Observed live — round 3 began at 18s, ran 13s, and left the
        // write-up 9s of its promised 15s, so the whole review was discarded.
        //
        // Ask instead whether another round could finish and still leave the
        // reserve intact. Costed at the longest round seen, so the estimate
        // degrades toward caution exactly when rounds are slow.
        let out_of_time = request.budget.is_some_and(|b| {
            Instant::now() + longest_round + b.write_up_reserve >= b.research_deadline
        });
        let is_final = round + 1 == max_rounds || out_of_time;
        let active_tools: &[LlmToolDef] = if is_final { &[] } else { &tool_defs };
        let round_began = Instant::now();
        let call = with_served_log(provider.create_message(
            Some(&request.system_prompt),
            &history,
            active_tools,
            request.model.as_deref(),
            None,
            true,
        ));
        // The write-up gets its reserve from the moment it starts, not whatever
        // happens to remain. Without this the guarantee is only as good as the
        // prediction above, and a round slower than any seen would still starve
        // it — the failure this whole change exists to remove.
        let (response, served) = match (is_final, request.budget) {
            (true, Some(b)) => match tokio::time::timeout(b.write_up_reserve, call).await {
                Ok(called) => called,
                Err(_) => {
                    tracing::warn!(
                        target: "advisor_loop",
                        label = request.label,
                        round,
                        reserve_secs = b.write_up_reserve.as_secs(),
                        "write-up exceeded its reserve"
                    );
                    return AdvisorLoopOutcome::ProviderError(
                        "write-up exceeded its reserve".to_string(),
                    );
                }
            },
            _ => call.await,
        };
        let response = match response {
            Ok(r) => r,
            Err(e) => return AdvisorLoopOutcome::ProviderError(e.to_string()),
        };
        last_model = response.model.clone();
        on_usage(&response.usage, &response.model, served.as_ref());

        if !is_final {
            longest_round = longest_round.max(round_began.elapsed());
        }
        let n_tool_uses = response.tool_uses().len();
        tracing::info!(
            target: "advisor_loop",
            label = request.label,
            round,
            tools_offered = active_tools.len(),
            tool_calls_requested = n_tool_uses,
            stop_reason = ?response.stop_reason,
            model = %response.model,
            "round"
        );

        let stopped_for_text = matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence | StopReason::MaxTokens
        ) || !response.has_tool_use();
        if is_final || stopped_for_text {
            let text = response.all_text().unwrap_or_default();
            tracing::info!(
                target: "advisor_loop",
                label = request.label,
                rounds_used = round + 1,
                answered_without_tools = round == 0 && n_tool_uses == 0,
                answer_len = text.len(),
                // Which bound ended it. Without this a truncated pass is
                // indistinguishable from a converged one in production — the
                // blindness that let a 12-round cap look healthy for weeks.
                ended_on = if out_of_time {
                    "deadline"
                } else if round + 1 == max_rounds {
                    "rounds"
                } else {
                    "answer"
                },
                "complete"
            );
            return AdvisorLoopOutcome::Success {
                text,
                truncated: out_of_time,
                capped: !out_of_time && round + 1 == max_rounds && round > 0,
                rounds_used: round + 1,
                model: last_model,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }

        history.push(LlmMessage {
            role: Role::Assistant,
            content: response.content.clone(),
        });
        let tool_results = run_tool_uses(&response, &registry, request.label).await;
        history.push(LlmMessage {
            role: Role::User,
            content: tool_results,
        });
    }

    AdvisorLoopOutcome::Success {
        text: String::new(),
        truncated: false,
        capped: false,
        rounds_used: max_rounds,
        model: last_model,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn build_tool_defs(tools: &[Arc<dyn Tool>]) -> Vec<LlmToolDef> {
    tools
        .iter()
        .map(|t| LlmToolDef {
            name: to_llm_name(t.name()),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        })
        .collect()
}

async fn run_tool_uses(
    response: &workforce_agent::llm::types::LlmResponse,
    registry: &ToolRegistry,
    label: &'static str,
) -> Vec<ContentBlock> {
    let expected_ids: Vec<String> = response
        .tool_uses()
        .into_iter()
        .map(|(id, _, _)| id.to_string())
        .collect();
    let mut joinset: JoinSet<(String, String, bool)> = JoinSet::new();
    for (id, llm_name, input) in response.tool_uses() {
        let id = id.to_string();
        let llm_name = llm_name.to_string();
        let resolved = resolve_tool_call_logged(
            &llm_name,
            input,
            registry,
            ToolCallOrigin {
                via: "advisor",
                agent: Some(label),
                ..Default::default()
            },
        );
        let input = resolved.input.into_owned();
        let tool = registry.get(&resolved.name);
        let not_found = tool
            .is_none()
            .then(|| tool_not_found_message(&llm_name, registry));
        joinset.spawn(async move {
            let input_preview: String = serde_json::to_string(&input)
                .unwrap_or_default()
                .chars()
                .take(160)
                .collect();
            let (content, is_error) = match tool {
                Some(t) => match t.execute(input).await {
                    Ok(r) => format_tool_result(&r),
                    Err(e) => (format!("tool execution error: {e}"), true),
                },
                None => (
                    not_found.unwrap_or_else(|| format!("tool '{llm_name}' not available")),
                    true,
                ),
            };
            let result_preview: String = content.chars().take(160).collect();
            tracing::info!(
                target: "advisor_loop",
                label,
                tool = %llm_name,
                input = %input_preview,
                ok = !is_error,
                result_len = content.len(),
                result = %result_preview,
                "tool call"
            );
            (id, content, is_error)
        });
    }
    let mut by_id: HashMap<String, (String, bool)> = HashMap::with_capacity(expected_ids.len());
    while let Some(joined) = joinset.join_next().await {
        match joined {
            Ok((id, content, is_error)) => {
                by_id.insert(id, (content, is_error));
            }
            Err(e) => {
                warn!(target: "advisor", error = %e, "advisor tool task panicked");
            }
        }
    }
    expected_ids
        .into_iter()
        .map(|id| {
            let (content, is_error) = by_id
                .remove(&id)
                .unwrap_or_else(|| ("tool task panicked; no result produced".to_string(), true));
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error: if is_error { Some(true) } else { None },
                loaded: None,
            }
        })
        .collect()
}

/// Cap on a single tool result carried back into the loop's history.
///
/// Every result stays in `history` for the rest of the run, so an uncapped one is
/// paid for by every later call — and most of all by the final, tool-less call
/// that has to read the lot and write the answer. Measured: three rounds of
/// `code.read`/`code.search` over a real repo left a write-up that could not
/// finish in 18 seconds, so the whole review was discarded having done all the
/// work.
///
/// Sized to keep the substance rather than to save bytes: ~6KB is a couple of
/// hundred lines, well past what confirming a claim about a function needs, so an
/// ordinary read passes through untouched and only a whole-file dump is cut.
const TOOL_RESULT_MAX_BYTES: usize = 6_000;

fn format_tool_result(r: &ToolResult) -> (String, bool) {
    if r.success {
        let body = serde_json::to_string(&r.output).unwrap_or_else(|_| r.output.to_string());
        // Say so in-band. A model handed a silent fragment can read it as the
        // whole file and call a claim verified on evidence it never saw; told the
        // result was cut, it can read more narrowly or say it could not confirm.
        let body = if body.len() > TOOL_RESULT_MAX_BYTES {
            let cut = workforce_common::text::truncate_with_ellipsis(&body, TOOL_RESULT_MAX_BYTES);
            let omitted = body.len().saturating_sub(cut.len());
            format!(
                "{cut}\n\n[truncated — {omitted} more bytes not shown. Narrow the query \
                 (a path, a symbol, a line range) if you need what is missing.]"
            )
        } else {
            body
        };
        (body, false)
    } else {
        let msg = r
            .error
            .clone()
            .unwrap_or_else(|| "tool failed without error message".to_string());
        (msg, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use eyre::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use workforce_agent::llm::streaming::StreamEvent;
    use workforce_agent::llm::types::{ContentBlock, LlmResponse};

    type Script = Vec<LlmResponse>;

    struct MockProvider {
        script: Mutex<std::collections::VecDeque<LlmResponse>>,
        calls: AtomicUsize,
        last_tools: Mutex<Option<Vec<String>>>,
    }

    impl MockProvider {
        fn new(script: Script) -> Self {
            Self {
                script: Mutex::new(script.into_iter().collect()),
                calls: AtomicUsize::new(0),
                last_tools: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_tools.lock().unwrap() = Some(tools.iter().map(|t| t.name.clone()).collect());
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| eyre::eyre!("script exhausted"))
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            Err(eyre::eyre!("not implemented"))
        }
    }

    struct CannedTool {
        name: String,
        output: serde_json::Value,
        ran: Arc<AtomicUsize>,
        started_at: Arc<Mutex<Vec<Instant>>>,
        delay: std::time::Duration,
    }

    #[async_trait]
    impl Tool for CannedTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
            self.started_at.lock().unwrap().push(Instant::now());
            self.ran.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(ToolResult::success(self.output.clone(), 1))
        }
    }

    fn text_response(text: &str) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            model: "test-model".to_string(),
        }
    }

    fn tool_use_response(id: &str, name: &str) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::tool_use(
                id.to_string(),
                name.to_string(),
                serde_json::json!({}),
            )],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 8,
                output_tokens: 3,
                ..Default::default()
            },
            model: "test-model".to_string(),
        }
    }

    fn baseline_request(tools: Vec<Arc<dyn Tool>>) -> AdvisorLoopRequest {
        AdvisorLoopRequest {
            system_prompt: "advisor".to_string(),
            user_message: "review this".to_string(),
            tools,
            model: None,
            budget: None,
            max_rounds: 3,
            cancel: None,
            label: "test",
        }
    }

    #[tokio::test]
    async fn loop_returns_text_on_first_round_when_no_tool_use() {
        let provider = MockProvider::new(vec![text_response("looks fine")]);
        let outcome =
            run_advisor_tool_loop(&provider, baseline_request(vec![]), |_, _, _| {}).await;
        match outcome {
            AdvisorLoopOutcome::Success { text, .. } => assert_eq!(text, "looks fine"),
            _ => panic!("expected Success"),
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn loop_executes_tool_use_and_feeds_back() {
        let ran = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Mutex::new(Vec::new()));
        let tool: Arc<dyn Tool> = Arc::new(CannedTool {
            name: "filesystem.read".to_string(),
            output: serde_json::json!({ "content": "file body" }),
            ran: ran.clone(),
            started_at: started,
            delay: std::time::Duration::ZERO,
        });
        let provider = MockProvider::new(vec![
            tool_use_response("u1", "filesystem_read"),
            text_response("verified"),
        ]);
        let outcome =
            run_advisor_tool_loop(&provider, baseline_request(vec![tool]), |_, _, _| {}).await;
        match outcome {
            AdvisorLoopOutcome::Success { text, .. } => assert_eq!(text, "verified"),
            _ => panic!("expected Success"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn loop_caps_at_max_rounds_and_forces_final_call() {
        let ran = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Mutex::new(Vec::new()));
        let tool: Arc<dyn Tool> = Arc::new(CannedTool {
            name: "filesystem.read".to_string(),
            output: serde_json::json!({}),
            ran: ran.clone(),
            started_at: started,
            delay: std::time::Duration::ZERO,
        });
        let provider = MockProvider::new(vec![
            tool_use_response("u1", "filesystem_read"),
            tool_use_response("u2", "filesystem_read"),
            text_response("final answer"),
        ]);
        let mut req = baseline_request(vec![tool]);
        req.max_rounds = 3;
        let outcome = run_advisor_tool_loop(&provider, req, |_, _, _| {}).await;
        match outcome {
            AdvisorLoopOutcome::Success { text, .. } => assert_eq!(text, "final answer"),
            other => panic!("expected Success, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        let last_tools = provider.last_tools.lock().unwrap().clone().unwrap();
        assert!(
            last_tools.is_empty(),
            "final call must pass empty tool list, got {last_tools:?}"
        );
    }

    #[tokio::test]
    async fn loop_synthesises_error_for_unknown_tool() {
        let provider = MockProvider::new(vec![
            tool_use_response("u1", "filesystem_grep"),
            text_response("ack"),
        ]);
        let req = baseline_request(vec![]);
        let outcome = run_advisor_tool_loop(&provider, req, |_, _, _| {}).await;
        assert!(matches!(outcome, AdvisorLoopOutcome::Success { .. }));
    }

    #[tokio::test]
    async fn loop_cancels_promptly_when_token_triggered() {
        let provider = MockProvider::new(vec![text_response("never")]);
        let token = CancellationToken::new();
        token.cancel();
        let mut req = baseline_request(vec![]);
        req.cancel = Some(token);
        let outcome = run_advisor_tool_loop(&provider, req, |_, _, _| {}).await;
        assert!(matches!(outcome, AdvisorLoopOutcome::Cancelled));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn loop_propagates_provider_error() {
        let provider = MockProvider::new(vec![]);
        let outcome =
            run_advisor_tool_loop(&provider, baseline_request(vec![]), |_, _, _| {}).await;
        match outcome {
            AdvisorLoopOutcome::ProviderError(msg) => assert!(msg.contains("script exhausted")),
            _ => panic!("expected ProviderError"),
        }
    }

    #[tokio::test]
    async fn loop_invokes_on_usage_each_round() {
        let ran = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Mutex::new(Vec::new()));
        let tool: Arc<dyn Tool> = Arc::new(CannedTool {
            name: "filesystem.read".to_string(),
            output: serde_json::json!({}),
            ran,
            started_at: started,
            delay: std::time::Duration::ZERO,
        });
        let provider = MockProvider::new(vec![
            tool_use_response("u1", "filesystem_read"),
            text_response("done"),
        ]);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_in = counter.clone();
        let outcome =
            run_advisor_tool_loop(&provider, baseline_request(vec![tool]), move |_, _, _| {
                counter_in.fetch_add(1, Ordering::SeqCst);
            })
            .await;
        assert!(matches!(outcome, AdvisorLoopOutcome::Success { .. }));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn loop_runs_tools_in_parallel() {
        let ran_a = Arc::new(AtomicUsize::new(0));
        let started_a = Arc::new(Mutex::new(Vec::new()));
        let ran_b = Arc::new(AtomicUsize::new(0));
        let started_b = Arc::new(Mutex::new(Vec::new()));
        let delay = std::time::Duration::from_millis(60);
        let tool_a: Arc<dyn Tool> = Arc::new(CannedTool {
            name: "filesystem.read".to_string(),
            output: serde_json::json!({}),
            ran: ran_a.clone(),
            started_at: started_a.clone(),
            delay,
        });
        let tool_b: Arc<dyn Tool> = Arc::new(CannedTool {
            name: "filesystem.grep".to_string(),
            output: serde_json::json!({}),
            ran: ran_b.clone(),
            started_at: started_b.clone(),
            delay,
        });
        let provider = MockProvider::new(vec![
            LlmResponse {
                content: vec![
                    ContentBlock::tool_use(
                        "u1".into(),
                        "filesystem_read".into(),
                        serde_json::json!({}),
                    ),
                    ContentBlock::tool_use(
                        "u2".into(),
                        "filesystem_grep".into(),
                        serde_json::json!({}),
                    ),
                ],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
                model: "test-model".to_string(),
            },
            text_response("done"),
        ]);
        // On the paused clock the tools' sleeps take `delay` together or
        // twice that one after the other, however busy the machine is.
        let start = tokio::time::Instant::now();
        let outcome = run_advisor_tool_loop(
            &provider,
            baseline_request(vec![tool_a, tool_b]),
            |_, _, _| {},
        )
        .await;
        let elapsed = start.elapsed();
        assert!(matches!(outcome, AdvisorLoopOutcome::Success { .. }));
        assert_eq!(ran_a.load(Ordering::SeqCst), 1);
        assert_eq!(ran_b.load(Ordering::SeqCst), 1);
        assert!(
            elapsed < delay * 2,
            "tools must run in parallel; took {elapsed:?}"
        );
    }
}

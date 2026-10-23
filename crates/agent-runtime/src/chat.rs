use std::collections::HashMap;
use std::sync::Arc;

use eyre::Result;
use tracing::warn;
use workforce_core::policy::{PolicyContext, PolicyDecision, PolicyRequest};
use workforce_policy::engine::PolicyEngine;

use crate::llm::provider::{with_tool_deferral, LlmProvider};
use crate::llm::types::{LlmMessage, Role, TokenUsage};
use crate::runner::{
    loaded_result, session_tools, unwrapped, with_loaded, OfferedTools, RunnerConfig,
    ToolInvocation,
};
use crate::tools::access::{self, ToolAccess};
use crate::tools::arguments::ArgumentValidation;
use crate::tools::dispatch::{argument_gate, GateOutcome};
use crate::tools::loading::PlanContext;
use crate::tools::loading::CALL_TOOL;
use crate::tools::registry::ToolRegistry;

/// An interactive chat session with an agent.
///
/// Maintains conversation history across multiple turns.
pub struct ChatSession {
    config: RunnerConfig,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    tool_access: Arc<dyn ToolAccess>,
    messages: Vec<LlmMessage>,
    total_usage: TokenUsage,
}

/// Result of a single chat turn.
#[derive(Debug)]
pub struct TurnResult {
    /// The agent's text response.
    pub response: String,
    /// Token usage for this turn.
    pub usage: TokenUsage,
    /// Tool invocations in this turn.
    pub tool_invocations: Vec<ToolInvocation>,
    /// Number of tool rounds in this turn.
    pub rounds: u32,
}

impl ChatSession {
    pub fn new(
        config: RunnerConfig,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self {
            config,
            llm,
            tools,
            policy,
            tool_access: access::noop(),
            messages: Vec::new(),
            total_usage: TokenUsage::default(),
        }
    }

    /// Install a per-user tool-visibility policy. Matches
    /// [`crate::runner::AgentRunner::with_tool_access`] — see that
    /// method's docs for shape.
    pub fn with_tool_access(mut self, access: Arc<dyn ToolAccess>) -> Self {
        self.tool_access = access;
        self
    }

    /// Send a message and get a response, maintaining conversation history.
    pub async fn send(&mut self, user_message: &str) -> Result<TurnResult> {
        self.messages.push(LlmMessage::user(user_message));

        let mut turn_usage = TokenUsage::default();
        let mut tool_invocations: Vec<ToolInvocation> = Vec::new();
        let mut rounds = 0;
        let mut offered = OfferedTools::default();
        let session = session_tools();
        let model = self.config.model.clone().unwrap_or_default();

        loop {
            rounds += 1;

            offered.refresh(
                &self.tools,
                self.tool_access.as_ref(),
                &PlanContext {
                    loading: &self.config.tool_loading,
                    session: &session,
                    context_floor: self.llm.context_floor(&model).await,
                    precurated: crate::runner::tools_precurated(),
                },
            );
            if rounds == 1 {
                offered.log_plan(self.config.agent_id.as_str());
            }
            let access_snapshot = std::sync::Arc::clone(&offered.access);
            if rounds > self.config.max_rounds {
                // Log tool usage breakdown for diagnostics
                let mut tool_counts: HashMap<&str, usize> = HashMap::new();
                for inv in &tool_invocations {
                    *tool_counts.entry(&inv.tool_name).or_insert(0) += 1;
                }
                warn!(
                    tool_summary = ?tool_counts,
                    "Max rounds exceeded in chat turn — tool usage breakdown"
                );

                // Ask LLM for a summary without tools so the user gets useful output
                let response_text = match self
                    .llm
                    .create_message(
                        Some(&self.config.system_prompt),
                        &self.messages,
                        &[],
                        self.config.model.as_deref(),
                        self.config.max_tokens,
                        true, // enable cache
                    )
                    .await
                {
                    Ok(resp) => {
                        let text = resp.text().unwrap_or_default();
                        turn_usage.input_tokens += resp.usage.input_tokens;
                        turn_usage.output_tokens += resp.usage.output_tokens;
                        if text.is_empty() {
                            "(max tool rounds reached)".to_string()
                        } else {
                            text
                        }
                    }
                    Err(e) => {
                        warn!("Failed to get summary after max rounds: {}", e);
                        "(max tool rounds reached)".to_string()
                    }
                };

                self.messages.push(LlmMessage::assistant(&response_text));
                self.total_usage.input_tokens += turn_usage.input_tokens;
                self.total_usage.output_tokens += turn_usage.output_tokens;

                return Ok(TurnResult {
                    response: response_text,
                    usage: turn_usage,
                    tool_invocations,
                    rounds,
                });
            }

            let response = with_tool_deferral(
                offered.plan.deferral(),
                self.llm.create_message(
                    Some(&offered.prompt(&self.config.system_prompt)),
                    &self.messages,
                    &offered.defs,
                    self.config.model.as_deref(),
                    self.config.max_tokens,
                    true, // enable cache
                ),
            )
            .await?;

            turn_usage.input_tokens += response.usage.input_tokens;
            turn_usage.output_tokens += response.usage.output_tokens;

            if !response.has_tool_use() {
                let text = response.text().unwrap_or_default();
                // Add assistant response to history
                self.messages.push(LlmMessage::assistant(&text));
                self.total_usage.input_tokens += turn_usage.input_tokens;
                self.total_usage.output_tokens += turn_usage.output_tokens;

                return Ok(TurnResult {
                    response: text,
                    usage: turn_usage,
                    tool_invocations,
                    rounds,
                });
            }

            // Process tool uses
            let tool_uses = response.tool_uses();
            let mut tool_results = Vec::new();
            let mut round_loads: HashMap<String, crate::llm::types::Loaded> = HashMap::new();

            self.messages.push(LlmMessage {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            for (tool_use_id, llm_tool_name, input) in &tool_uses {
                let (llm_tool_name, input, via_call) = match unwrapped(llm_tool_name, input) {
                    Ok(call) => call,
                    Err(message) => {
                        tool_invocations.push(ToolInvocation {
                            tool_name: CALL_TOOL.to_string(),
                            input: (*input).clone(),
                            output: serde_json::Value::Null,
                            success: false,
                            policy_decision: "allow".to_string(),
                            duration_ms: 0,
                            denial_reason: None,
                            error: Some(message.clone()),
                        });
                        tool_results.push((tool_use_id.to_string(), message, true));
                        continue;
                    }
                };
                let resolved = crate::runner::resolve_tool_call_logged(
                    &llm_tool_name,
                    &input,
                    &self.tools,
                    crate::runner::ToolCallOrigin {
                        via: "chat",
                        agent: Some(self.config.agent_id.as_str()),
                        model: self.config.model.as_deref(),
                        round: None,
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
                    let tool = answer.tool.clone();
                    let exec = answer.execution();
                    let output_text = match exec.loads.is_empty() {
                        true => exec.output_text.clone(),
                        false => {
                            offered.queue_loads(&exec.loads);
                            let (content, loaded) =
                                loaded_result(&self.tools, exec.output_text.clone(), &exec.loads);
                            round_loads.insert(tool_use_id.to_string(), loaded);
                            content
                        }
                    };
                    tool_invocations.push(ToolInvocation {
                        tool_name: tool,
                        input: (*input).clone(),
                        output: exec.output,
                        success: exec.success,
                        policy_decision: "load".to_string(),
                        duration_ms: 0,
                        denial_reason: None,
                        error: exec.is_error.then(|| exec.output_text.clone()),
                    });
                    tool_results.push((tool_use_id.to_string(), output_text, exec.is_error));
                    continue;
                }

                if via_call && self.tools.get(&tool_name).is_none() {
                    let message = crate::runner::unknown_call_message(&tool_name);
                    tool_invocations.push(ToolInvocation {
                        tool_name: CALL_TOOL.to_string(),
                        input: (*input).clone(),
                        output: serde_json::Value::Null,
                        success: false,
                        policy_decision: "allow".to_string(),
                        duration_ms: 0,
                        denial_reason: None,
                        error: Some(message.clone()),
                    });
                    tool_results.push((tool_use_id.to_string(), message, true));
                    continue;
                }

                let policy_request = PolicyRequest {
                    agent_id: self.config.agent_id.clone(),
                    tool_name: tool_name.clone(),
                    parameters: (*input).clone(),
                    context: PolicyContext {
                        node_id: None,
                        team_id: self.config.team_id.clone(),
                        task_id: None,
                        role: self.config.role.clone(),
                    },
                };

                let policy_response = self.policy.evaluate(&policy_request);
                let policy_response = crate::runner::apply_cascade_gate(
                    &crate::runner::ToolGates::from(&self.config),
                    policy_response,
                    &tool_name,
                );
                let policy_response = crate::tools::dispatch::apply_tool_requirement(
                    &self.tools,
                    policy_response,
                    &tool_name,
                );

                if let Some(GateOutcome::Blocked {
                    message, reason, ..
                }) = (via_call && matches!(policy_response.decision, PolicyDecision::Allow))
                    .then(|| {
                        argument_gate(
                            ArgumentValidation::Enforce,
                            self.config.agent_id.as_str(),
                            &self.tools,
                            &tool_name,
                            input,
                        )
                    })
                    .flatten()
                {
                    tool_invocations.push(ToolInvocation {
                        tool_name: tool_name.clone(),
                        input: (*input).clone(),
                        output: serde_json::Value::Null,
                        success: false,
                        policy_decision: "deny".to_string(),
                        duration_ms: 0,
                        denial_reason: Some(reason.clone()),
                        error: Some(reason),
                    });
                    tool_results.push((tool_use_id.to_string(), message, true));
                    continue;
                }

                match &policy_response.decision {
                    PolicyDecision::Allow => {
                        // Substitute marker text → cleartext for
                        // allowlisted secret-sink fields. Unknown
                        // markers fail-closed; non-sink tools and
                        // metadata fields pass through unchanged.
                        let sub = crate::security::secret_sinks::substitute_secret_markers(
                            &tool_name,
                            (*input).clone(),
                            &crate::runner::CURRENT_SESSION_KEY
                                .try_with(|k| k.clone())
                                .unwrap_or_default(),
                        );
                        if !sub.unknown.is_empty() {
                            let reason = sub.unknown_reason();
                            tracing::warn!(target: "sentinel", agent = %self.config.agent_id, tool = %tool_name, %reason, "Secret marker substitution failed");
                            tool_invocations.push(ToolInvocation {
                                tool_name: tool_name.to_string(),
                                input: (*input).clone(),
                                output: serde_json::Value::Null,
                                success: false,
                                policy_decision: "deny".to_string(),
                                duration_ms: 0,
                                denial_reason: Some(reason.clone()),
                                error: Some(reason.clone()),
                            });
                            tool_results.push((
                                tool_use_id.to_string(),
                                format!("BLOCKED: {reason}"),
                                true,
                            ));
                            continue;
                        }
                        let substituted_input = sub.input;

                        // Sentinel pre-execution parameter scan
                        let sentinel_findings =
                            workforce_common::security_scanner::scan_parameters(&tool_name, input);
                        if let Some(workforce_common::security_scanner::ParameterBlock {
                            reason,
                            message,
                        }) =
                            workforce_common::security_scanner::parameter_block(&sentinel_findings)
                        {
                            tracing::warn!(target: "sentinel", agent = %self.config.agent_id, tool = %tool_name, %reason, "Parameter scan blocked tool");
                            tool_invocations.push(ToolInvocation {
                                tool_name: tool_name.to_string(),
                                input: substituted_input.clone(),
                                output: serde_json::Value::Null,
                                success: false,
                                policy_decision: "deny".to_string(),
                                duration_ms: 0,
                                denial_reason: Some(reason.clone()),
                                error: Some(reason),
                            });
                            tool_results.push((tool_use_id.to_string(), message, true));
                        } else {
                            if !sentinel_findings.is_empty() {
                                let summary: Vec<_> =
                                    sentinel_findings.iter().map(|f| f.description).collect();
                                tracing::info!(target: "sentinel", tool = %tool_name, findings = ?summary, "Non-critical parameter scan findings");
                            }
                            if let Some(reason) = access_snapshot.block_reason(&tool_name) {
                                let r = reason.to_string();
                                tool_invocations.push(ToolInvocation {
                                    tool_name: tool_name.to_string(),
                                    input: substituted_input.clone(),
                                    output: serde_json::Value::Null,
                                    success: false,
                                    policy_decision: "deny".to_string(),
                                    duration_ms: 0,
                                    denial_reason: Some(r.clone()),
                                    error: Some(r.clone()),
                                });
                                tool_results.push((tool_use_id.to_string(), r, true));
                            } else if let Some(tool) = self.tools.get(&tool_name) {
                                let result = tool.execute(substituted_input.clone()).await?;
                                let output_str = if result.success {
                                    serde_json::to_string(&result.output)
                                        .unwrap_or_else(|_| "{}".to_string())
                                } else {
                                    result
                                        .error
                                        .clone()
                                        .unwrap_or_else(|| "Tool failed".to_string())
                                };

                                let error = (!result.success).then(|| output_str.clone());
                                let output_str = match result.loads.is_empty() {
                                    true => output_str,
                                    false => {
                                        offered.queue_loads(&result.loads);
                                        let (content, loaded) =
                                            loaded_result(&self.tools, output_str, &result.loads);
                                        round_loads.insert(tool_use_id.to_string(), loaded);
                                        content
                                    }
                                };
                                tool_invocations.push(ToolInvocation {
                                    tool_name: tool_name.to_string(),
                                    input: substituted_input,
                                    output: result.output,
                                    success: result.success,
                                    policy_decision: "allow".to_string(),
                                    duration_ms: result.duration_ms,
                                    denial_reason: None,
                                    error,
                                });

                                tool_results.push((
                                    tool_use_id.to_string(),
                                    output_str,
                                    !result.success,
                                ));
                            } else {
                                tool_results.push((
                                    tool_use_id.to_string(),
                                    crate::runner::tool_not_found_message(&tool_name, &self.tools),
                                    true,
                                ));
                            }
                        }
                    }
                    PolicyDecision::Deny { reason } => {
                        tool_invocations.push(ToolInvocation {
                            tool_name: tool_name.to_string(),
                            input: (*input).clone(),
                            output: serde_json::Value::Null,
                            success: false,
                            policy_decision: "deny".to_string(),
                            duration_ms: 0,
                            denial_reason: Some(reason.clone()),
                            error: Some(reason.clone()),
                        });
                        tool_results.push((
                            tool_use_id.to_string(),
                            format!("BLOCKED by security policy: {reason} — You MUST inform the user about this block. Do NOT attempt workarounds or alternative approaches. Tell the user exactly what was blocked and why."),
                            true,
                        ));
                    }
                    PolicyDecision::RequireApproval { reason, .. } => {
                        tool_invocations.push(ToolInvocation {
                            tool_name: tool_name.to_string(),
                            input: (*input).clone(),
                            output: serde_json::Value::Null,
                            success: false,
                            policy_decision: "require_approval".to_string(),
                            duration_ms: 0,
                            denial_reason: Some(reason.clone()),
                            error: Some(reason.clone()),
                        });
                        tool_results.push((
                            tool_use_id.to_string(),
                            format!("Approval required: {reason}"),
                            true,
                        ));
                    }
                }
            }

            self.messages.push(with_loaded(
                LlmMessage::tool_results(tool_results),
                &round_loads,
            ));
        }
    }

    /// Get total token usage across all turns.
    pub fn total_usage(&self) -> &TokenUsage {
        &self.total_usage
    }

    /// Get the number of messages in the conversation.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::llm::types::{ContentBlock, LlmResponse, LlmToolDef, StopReason};
    use crate::tools::{Tool, ToolResult};

    struct GatedTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for GatedTool {
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
        async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success(serde_json::Value::Null, 0))
        }
    }

    /// Calls the gated tool once, then answers.
    struct CallsTheGatedTool(AtomicUsize);

    #[async_trait]
    impl LlmProvider for CallsTheGatedTool {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(LlmResponse {
                content: if first {
                    vec![ContentBlock::tool_use(
                        "tu_1".to_string(),
                        "record-keeper.apply_change".to_string(),
                        serde_json::json!({}),
                    )]
                } else {
                    vec![ContentBlock::Text {
                        text: "understood".to_string(),
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

    /// This loop has no in-repo consumer, so the composition exists to stop
    /// it becoming a hole if one appears: policy allows the call outright and
    /// the tool's own requirement still refuses it, unexecuted.
    #[tokio::test]
    async fn a_gated_tool_is_refused_and_never_executed() {
        let ran = Arc::new(AtomicUsize::new(0));
        let tools = ToolRegistry::new();
        tools.register(Arc::new(GatedTool(Arc::clone(&ran))));
        let mut session = ChatSession::new(
            RunnerConfig {
                max_rounds: 5,
                ..Default::default()
            },
            Arc::new(CallsTheGatedTool(AtomicUsize::new(0))),
            Arc::new(tools),
            Arc::new(
                PolicyEngine::from_yaml(
                    "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                )
                .expect("policy parse"),
            ),
        );

        let turn = session.send("apply it").await.expect("turn completes");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "the gated tool must not run");
        assert_eq!(turn.tool_invocations.len(), 1);
        assert_eq!(turn.tool_invocations[0].policy_decision, "require_approval");
        assert_eq!(turn.response, "understood");
    }

    struct Counted(String, Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for Counted {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            "counted"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success(serde_json::Value::Null, 0))
        }
    }

    /// Calls the unloaded tool, then calls it again, recording the tools of
    /// every call.
    /// Answers each call with the next scripted tool call, then ends the
    /// turn, recording the tools of every call.
    struct Scripted {
        script: Vec<(&'static str, serde_json::Value)>,
        tools: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmProvider for Scripted {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let mut seen = self.tools.lock().unwrap();
            seen.push(tools.iter().map(|t| t.name.clone()).collect());
            let round = seen.len();
            Ok(match self.script.get(round - 1) {
                Some((tool, input)) => LlmResponse {
                    content: vec![ContentBlock::tool_use(
                        format!("tu_{round}"),
                        tool.to_string(),
                        input.clone(),
                    )],
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

    async fn chat_turn(
        script: Vec<(&'static str, serde_json::Value)>,
    ) -> (TurnResult, usize, Vec<Vec<String>>, Vec<LlmMessage>) {
        let ran = Arc::new(AtomicUsize::new(0));
        let tools = ToolRegistry::new();
        tools.register(Arc::new(Counted("vault.set".into(), Arc::clone(&ran))));
        for name in ["memory".to_string()]
            .into_iter()
            .chain((0..40).map(|i| format!("zz_filler{i}")))
        {
            tools.register(Arc::new(Counted(name, Arc::new(AtomicUsize::new(0)))));
        }
        let llm = Arc::new(Scripted {
            script,
            tools: std::sync::Mutex::new(Vec::new()),
        });
        let mut session = ChatSession::new(
            RunnerConfig {
                max_rounds: 6,
                tool_loading: crate::tools::loading::ToolLoading {
                    mode: workforce_common::settings::ToolLoadingMode::On,
                    core: Some(Arc::default()),
                    always: Arc::default(),
                },
                ..Default::default()
            },
            Arc::clone(&llm) as Arc<dyn LlmProvider>,
            Arc::new(tools),
            Arc::new(
                PolicyEngine::from_yaml(
                    "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                )
                .expect("policy parse"),
            ),
        );
        let turn = session.send("set it").await.expect("turn completes");
        let offered = llm.tools.lock().unwrap().clone();
        (turn, ran.load(Ordering::SeqCst), offered, session.messages)
    }

    fn decisions(turn: &TurnResult) -> Vec<(&str, &str)> {
        turn.tool_invocations
            .iter()
            .map(|i| (i.tool_name.as_str(), i.policy_decision.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn a_chat_turn_loads_an_unloaded_tool_before_running_it() {
        let (turn, ran, offered, _) = chat_turn(vec![
            ("vault_set", serde_json::json!({})),
            ("vault_set", serde_json::json!({})),
        ])
        .await;
        assert_eq!(ran, 1, "run once, after the load");
        assert_eq!(
            decisions(&turn),
            [("vault.set", "load"), ("vault.set", "allow")]
        );
        assert_eq!(offered[0], ["memory", "tool_load", "tool_call"]);
        assert_eq!(offered[1], offered[0], "a load changes no declared tool");
    }

    #[tokio::test]
    async fn a_chat_turn_calls_loaded_tools_through_tool_call() {
        let (turn, ran, _, messages) = chat_turn(vec![
            ("tool_load", serde_json::json!({ "names": ["vault_set"] })),
            (
                "tool_call",
                serde_json::json!({ "name": "vault_set", "arguments": { "key": "k" } }),
            ),
            ("tool_call", serde_json::json!({ "name": "nope" })),
            ("tool_call", serde_json::json!({ "arguments": {} })),
        ])
        .await;
        assert_eq!(ran, 1);
        assert_eq!(
            decisions(&turn),
            [
                ("tool.load", "load"),
                ("vault.set", "allow"),
                ("tool.call", "allow"),
                ("tool.call", "allow")
            ]
        );
        let loaded = messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    content,
                    loaded: Some(loaded),
                    ..
                } => Some((content.clone(), loaded.tools.clone())),
                _ => None,
            });
        let (content, tools) = loaded.expect("the load is marked");
        assert!(content.contains("\"name\":\"vault_set\""));
        assert_eq!(tools, ["vault_set"]);
    }
}

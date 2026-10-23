use std::collections::HashMap;

use eyre::Result;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

const GEMINI_API_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
const GEMINI_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai/models";

/// Client for the Google Gemini API.
///
/// Gemini exposes an OpenAI-compatible chat completions endpoint with
/// full function-calling / tool-use support.
pub struct GeminiClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    /// Extended thinking budget. When set, Gemini models use their thinking
    /// capability to reason internally before producing output.
    thinking_budget_tokens: Option<u32>,
    /// The lowest gemini-3.x `thinking_level` each model has been seen to
    /// accept, learned from its refusals: the levels a model supports are
    /// Google's to change (gemini-3.8-flash refuses `minimal`), and a budget
    /// that maps below them would otherwise fail every call.
    level_floor: Mutex<HashMap<String, usize>>,
}

/// gemini-3.x thinking levels, least thinking first.
const THINKING_LEVELS: [&str; 4] = ["minimal", "low", "medium", "high"];

impl GeminiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            default_model: "gemini-3.1-pro-preview".to_string(),
            default_max_tokens: 8192,
            thinking_budget_tokens: None,
            level_floor: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = max_tokens;
        self
    }

    /// Enable extended thinking with the given token budget.
    ///
    /// For Gemini 2.5 models, this maps to `thinking_budget` (token count).
    /// For Gemini 3.x models, this maps to `thinking_level` (low/medium/high).
    pub fn with_thinking_budget(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget_tokens = Some(budget_tokens);
        self
    }

    /// The thinking budget for the request now being built: the per-turn
    /// override when one is in scope (see
    /// [`crate::llm::provider::effective_thinking_budget`]), otherwise this
    /// client's configured budget.
    fn effective_thinking_budget(&self) -> Option<u32> {
        super::provider::effective_thinking_budget(self.thinking_budget_tokens)
    }

    /// The `thinking_level` to send `model` for `budget`: the budget's own
    /// level, raised to the lowest the model has accepted.
    fn thinking_level(&self, model: &str, budget: u32) -> &'static str {
        let wanted = super::tiers::budget_to_thinking_level(budget);
        let wanted = THINKING_LEVELS
            .iter()
            .position(|level| *level == wanted)
            .unwrap_or(0);
        let floor = self.level_floor.lock().get(model).copied().unwrap_or(0);
        THINKING_LEVELS[wanted.max(floor)]
    }

    /// Add the thinking config for the current budget to `body`, if any.
    fn set_thinking(&self, body: &mut serde_json::Value, model: &str) {
        let Some(budget) = self.effective_thinking_budget() else {
            return;
        };
        let thinking_config = if model.contains("gemini-3") {
            // Gemini 3.x models use thinking_level instead of thinking_budget
            let level = self.thinking_level(model, budget);
            serde_json::json!({ "thinking_config": { "thinking_level": level, "include_thoughts": false } })
        } else {
            // Gemini 2.5 models use thinking_budget (token count)
            serde_json::json!({ "thinking_config": { "thinking_budget": budget, "include_thoughts": false } })
        };
        body["extra_body"] = serde_json::json!({ "google": thinking_config });
        tracing::debug!(model, thinking_budget = budget, "Gemini thinking enabled");
    }

    /// POST `body`, and when Gemini refuses its thinking level, remember the
    /// next level up as the model's floor and send again with that.
    async fn send(&self, body: &mut serde_json::Value, model: &str) -> Result<reqwest::Response> {
        loop {
            let response = self
                .client
                .post(GEMINI_API_URL)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&*body)
                .send()
                .await
                .map_err(|e| eyre::eyre!("Gemini request failed: {}", e))?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            let sent = body
                .pointer("/extra_body/google/thinking_config/thinking_level")
                .and_then(|level| level.as_str())
                .and_then(|level| THINKING_LEVELS.iter().position(|l| *l == level));
            match sent {
                Some(sent)
                    if status == reqwest::StatusCode::BAD_REQUEST
                        && refuses_thinking_level(&error_body)
                        && sent + 1 < THINKING_LEVELS.len() =>
                {
                    let next = THINKING_LEVELS[sent + 1];
                    tracing::warn!(
                        model,
                        refused = THINKING_LEVELS[sent],
                        retrying_with = next,
                        "Gemini refused the thinking level; raising it for this model"
                    );
                    self.level_floor.lock().insert(model.to_string(), sent + 1);
                    body["extra_body"]["google"]["thinking_config"]["thinking_level"] =
                        serde_json::json!(next);
                }
                _ => return Err(eyre::eyre!("Gemini API error ({}): {}", status, error_body)),
            }
        }
    }

    /// Auth-only credential probe. Hits `GET /v1beta/openai/models`
    /// (OpenAI-compatible models listing) using Bearer auth — Gemini
    /// also accepts query-param auth but the OpenAI-compat endpoint
    /// requires the header form.
    pub async fn verify_credentials(&self) -> Result<()> {
        let resp = self
            .client
            .get(GEMINI_MODELS_URL)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Gemini verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Gemini verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    /// Send a message and get a response.
    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("gemini:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "gemini",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let mut oai_messages: Vec<OaiMessage> = Vec::new();
        if let Some(system) = system_prompt {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(build_system_content(system, enable_cache)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }

        for msg in messages {
            oai_messages.extend(convert_to_oai_message(msg));
        }

        let oai_tools: Option<Vec<OaiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| OaiTool {
                        r#type: "function".to_string(),
                        function: OaiFunction {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: t.input_schema.clone(),
                        },
                    })
                    .collect(),
            )
        };

        let has_tools = oai_tools.is_some();
        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            tool_choice: if has_tools {
                Some("auto".to_string())
            } else {
                None
            },
            parallel_tool_calls: if has_tools { Some(false) } else { None },
        };

        // Serialize to JSON value so we can merge in thinking config
        let mut body = serde_json::to_value(&request)
            .map_err(|e| eyre::eyre!("Failed to serialize request: {}", e))?;

        self.set_thinking(&mut body, model);

        tracing::debug!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending request to Gemini"
        );

        let response = self.send(&mut body, model).await?;

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Gemini response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Gemini response"))?;

        // Convert back to our internal format
        let (content, stop_reason) = convert_from_oai_choice(&choice);

        let usage = api_response
            .usage
            .map(|u| {
                let details = u.prompt_tokens_details.as_ref();
                TokenUsage::from_oai(
                    u.prompt_tokens,
                    u.completion_tokens,
                    details.map(|d| d.cached_tokens).unwrap_or(0),
                    details.map(|d| d.cache_write_tokens).unwrap_or(0),
                )
            })
            .unwrap_or_default();

        tracing::debug!(
            model = %api_response.model.as_deref().unwrap_or(model),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            stop_reason = ?stop_reason,
            "Received response from Gemini"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response.model.unwrap_or_else(|| model.to_string()),
        })
    }

    /// Stream a message response using OpenAI-compatible SSE.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("gemini:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "gemini",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let mut oai_messages: Vec<OaiMessage> = Vec::new();
        if let Some(system) = system_prompt {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(build_system_content(system, enable_cache)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        for msg in messages {
            oai_messages.extend(convert_to_oai_message(msg));
        }

        let oai_tools: Option<Vec<OaiTool>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| OaiTool {
                        r#type: "function".to_string(),
                        function: OaiFunction {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: t.input_schema.clone(),
                        },
                    })
                    .collect(),
            )
        };

        let has_tools = oai_tools.is_some();
        let request = OaiStreamRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            tool_choice: if has_tools {
                Some("auto".to_string())
            } else {
                None
            },
            parallel_tool_calls: if has_tools { Some(false) } else { None },
            stream: true,
            // OpenAI-compat streaming omits the `usage` block by default;
            // without this every cost_record lands with 0 input/output tokens.
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
        };

        let mut body = serde_json::to_value(&request)
            .map_err(|e| eyre::eyre!("Failed to serialize request: {}", e))?;

        self.set_thinking(&mut body, model);

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending streaming request to Gemini"
        );

        let mut response = self.send(&mut body, model).await?;

        // Parse SSE stream (standard OpenAI format)
        let mut buffer = String::new();
        let mut state = GeminiStreamState::new(model);
        let mut total_usage = TokenUsage::default();
        let mut final_stop_reason = StopReason::EndTurn;
        let mut got_done = false;
        let mut got_finish_reason = false;

        while let Some(bytes) = response
            .chunk()
            .await
            .map_err(|e| eyre::eyre!("Stream error: {}", e))?
        {
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                let data: String = event_block
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                let data = data.trim();

                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    got_done = true;
                    continue;
                }

                let json: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(usage) = json.get("usage") {
                    let details = usage.get("prompt_tokens_details");
                    total_usage = TokenUsage::from_oai(
                        usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                        usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
                        details
                            .and_then(|d| d["cached_tokens"].as_u64())
                            .unwrap_or(0) as u32,
                        details
                            .and_then(|d| d["cache_write_tokens"].as_u64())
                            .unwrap_or(0) as u32,
                    );
                }

                let choices = match json["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };

                for choice in choices {
                    let delta = &choice["delta"];
                    let finish = choice["finish_reason"].as_str();
                    let out = state.process_choice_delta(delta, finish);
                    for ev in out.events {
                        let _ = tx.send(ev).await;
                    }
                    if let Some(stop) = out.stop_reason {
                        final_stop_reason = stop;
                    }
                    if out.saw_finish {
                        got_finish_reason = true;
                    }
                }
            }
        }

        let has_content = state.has_content;
        let has_tool_calls = state.has_tool_calls;

        if !got_done && !has_content && !has_tool_calls {
            tracing::error!(
                "Gemini stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "Gemini streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "Gemini stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "Gemini streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }
        if !got_done {
            tracing::debug!(
                "Gemini stream ended without [DONE] marker but response appears complete"
            );
        }

        final_stop_reason = promote_to_tool_use_if_pending(final_stop_reason, has_tool_calls);

        if got_done && !has_content && !has_tool_calls {
            // Gemini sometimes completes the stream cleanly ([DONE] + a
            // finish_reason) yet emits zero content and zero tool calls —
            // typically a transient safety/content filter or an unrecognised
            // finish_reason. Returning Ok here would surface an empty turn to
            // the user (in Slack: a lone ✅ reaction and no reply). No content
            // or tool-call events have been sent on `tx` yet, so it is safe to
            // fail the stream and let the failover layer retry — and then fail
            // over — exactly as the two no-content cases above do. The "(503)"
            // tag makes `classify_error` treat this as a retryable ServerError.
            tracing::warn!(
                model = model,
                stop_reason = ?final_stop_reason,
                "Gemini stream completed with [DONE] but emitted no content or tool calls — treating as a retryable empty response (possible safety/content filter or unrecognised finish_reason)"
            );
            return Err(eyre::eyre!(
                "Gemini streaming API error (503): stream completed with [DONE] but emitted no content or tool calls (stop_reason={:?}; possible safety/content filter)",
                final_stop_reason
            ));
        }

        let _ = tx
            .send(StreamEvent::MessageDone {
                stop_reason: final_stop_reason,
                usage: total_usage,
            })
            .await;

        Ok(())
    }
}

// ─── Streaming state ────────────────────────────────────────────────────────

/// Output of one OpenAI-compatible streaming chunk fed to
/// [`GeminiStreamState::process_choice_delta`].
struct ChoiceOutput {
    events: Vec<StreamEvent>,
    stop_reason: Option<StopReason>,
    saw_finish: bool,
}

/// Per-stream state for translating Gemini's OpenAI-compatible chat
/// completions streaming format into provider-neutral `StreamEvent`s.
///
/// Two Gemini-specific quirks the chunk handler must absorb:
///
/// 1. **`function.name` repeats across deltas.** The compat layer often
///    re-emits `name` (and `id`) on follow-up chunks for the same
///    `tc_index`. Treating each as a fresh `ToolUseStart` resets the
///    collector's tool slot and silently discards args + signature
///    captured from earlier chunks. We emit `ToolUseStart` exactly once
///    per `tc_index`; everything after lands as `InputJsonDelta` /
///    `ToolSignatureUpdate`.
///
/// 2. **`thought_signature` arrives on an arbitrary chunk.**
///    `extra_content.google.thought_signature` may appear on the start
///    chunk or on any later one. On the start chunk we attach it to the
///    `ToolUseStart` event; on later chunks we emit
///    `ToolSignatureUpdate`. Without this, the next request 400s with
///    "missing thought_signature in functionCall parts".
struct GeminiStreamState {
    model: String,
    content_started: bool,
    content_index: usize,
    has_content: bool,
    has_tool_calls: bool,
    started_tool_indices: std::collections::HashSet<usize>,
}

impl GeminiStreamState {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            content_started: false,
            content_index: 0,
            has_content: false,
            has_tool_calls: false,
            started_tool_indices: std::collections::HashSet::new(),
        }
    }

    /// Translate one `delta` payload (the OAI streaming `choices[*].delta`
    /// object) plus its `finish_reason` into the equivalent `StreamEvent`
    /// sequence. Stateful across calls within a single response.
    fn process_choice_delta(
        &mut self,
        delta: &serde_json::Value,
        finish: Option<&str>,
    ) -> ChoiceOutput {
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut stop_reason: Option<StopReason> = None;
        let mut saw_finish = false;

        if let Some(content) = delta["content"].as_str() {
            if !content.is_empty() {
                self.has_content = true;
                if !self.content_started {
                    self.content_started = true;
                    events.push(StreamEvent::ContentStart {
                        index: self.content_index,
                    });
                }
                events.push(StreamEvent::TextDelta {
                    index: self.content_index,
                    text: content.to_string(),
                });
            }
        }

        if let Some(reasoning) = delta["reasoning_content"].as_str() {
            if !reasoning.is_empty() {
                self.has_content = true;
                if !self.content_started {
                    self.content_started = true;
                    events.push(StreamEvent::ContentStart {
                        index: self.content_index,
                    });
                }
                events.push(StreamEvent::ThinkingDelta {
                    index: self.content_index,
                    thinking: reasoning.to_string(),
                });
            }
        }

        if let Some(tool_calls) = delta["tool_calls"].as_array() {
            for tc in tool_calls {
                self.process_tool_call_delta(tc, &mut events);
            }
        }

        if let Some(reason) = finish {
            saw_finish = true;
            stop_reason = Some(match reason {
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "stop" => StopReason::EndTurn,
                _ => StopReason::EndTurn,
            });
            events.push(StreamEvent::ContentStop {
                index: self.content_index,
            });
            self.content_index += 1;
        }

        ChoiceOutput {
            events,
            stop_reason,
            saw_finish,
        }
    }

    fn process_tool_call_delta(&mut self, tc: &serde_json::Value, events: &mut Vec<StreamEvent>) {
        let tc_index = tc["index"].as_u64().unwrap_or(0) as usize;
        let idx = tc_index + 1;

        let chunk_signature = tc["extra_content"]["google"]["thought_signature"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let func = tc.get("function");
        let name = func.and_then(|f| f["name"].as_str());
        let args = func
            .and_then(|f| f["arguments"].as_str())
            .filter(|s| !s.is_empty());

        let already_started = self.started_tool_indices.contains(&tc_index);

        tracing::debug!(
            target: "workforce_agent::llm::gemini::stream_chunk",
            model = %self.model,
            tc_index = tc_index,
            has_name = name.is_some(),
            has_args = args.is_some(),
            has_signature = chunk_signature.is_some(),
            already_started = already_started,
            "Gemini stream chunk"
        );

        if !already_started {
            // First sight of this tool slot. Need a `function.name` to open
            // the block; without it we have nowhere to attach args/signature.
            let Some(name) = name else {
                if args.is_some() || chunk_signature.is_some() {
                    tracing::warn!(
                        target: "workforce_agent::llm::gemini::stream_chunk",
                        tc_index = tc_index,
                        "Gemini delivered tool delta before function.name; dropping until name arrives"
                    );
                }
                return;
            };
            self.has_tool_calls = true;
            if self.content_started {
                self.content_started = false;
                events.push(StreamEvent::ContentStop {
                    index: self.content_index,
                });
                self.content_index += 1;
            }
            let id = tc["id"].as_str().unwrap_or("unknown").to_string();
            events.push(StreamEvent::ToolUseStart {
                index: idx,
                id,
                name: name.to_string(),
                signature: chunk_signature,
            });
            self.started_tool_indices.insert(tc_index);
            if let Some(args) = args {
                events.push(StreamEvent::InputJsonDelta {
                    index: idx,
                    partial: args.to_string(),
                });
            }
            return;
        }

        // Already-started tool slot: signature and args are deltas. Re-emitted
        // `name`/`id` on follow-up chunks is intentionally ignored — the
        // collector keeps the originals from the start chunk.
        if let Some(signature) = chunk_signature {
            events.push(StreamEvent::ToolSignatureUpdate {
                index: idx,
                signature,
            });
        }
        if let Some(args) = args {
            events.push(StreamEvent::InputJsonDelta {
                index: idx,
                partial: args.to_string(),
            });
        }
    }
}

// ─── Streaming request type ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OaiStreamRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OaiStreamOptions>,
}

#[derive(Debug, Serialize)]
struct OaiStreamOptions {
    include_usage: bool,
}

fn build_system_content(prompt: &str, enable_cache: bool) -> OaiContent {
    if enable_cache {
        OaiContent::Parts(super::cache_utils::build_cache_blocks(prompt))
    } else {
        OaiContent::String(prompt.to_string())
    }
}

/// Convert our LlmMessage to OpenAI format message(s).
fn convert_to_oai_message(msg: &LlmMessage) -> Vec<OaiMessage> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    // Check if this message contains tool results
    let has_tool_results = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    if has_tool_results {
        let mut out: Vec<OaiMessage> = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some(OaiMessage {
                    role: "tool".to_string(),
                    content: Some(OaiContent::String(content.clone())),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                    reasoning_content: None,
                }),
                _ => None,
            })
            .collect();
        // Text/media blocks alongside the tool results (e.g. a file staged by
        // `view_attachment`) can't ride on a `tool` message — emit them as a
        // following user turn so they reach the model instead of being dropped.
        let extra: Vec<serde_json::Value> = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => {
                    Some(serde_json::json!({ "type": "text", "text": text }))
                }
                ContentBlock::Image { source } | ContentBlock::Document { source } => {
                    Some(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": source.data_url()
                        }
                    }))
                }
                _ => None,
            })
            .collect();
        if !extra.is_empty() {
            out.push(OaiMessage {
                role: "user".to_string(),
                content: Some(OaiContent::Parts(extra)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        return out;
    }

    // Check if this message contains tool use (assistant requesting tools)
    let has_tool_use = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
    if has_tool_use {
        let text_content: Option<String> = msg.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        });

        let tool_calls: Vec<OaiToolCall> = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    signature,
                } => Some(OaiToolCall {
                    id: id.clone(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_default(),
                    },
                    extra_content: Some(serde_json::json!({ "google": {
                        "thought_signature": signature.as_deref().unwrap_or(UNSIGNED_CALL),
                    } })),
                }),
                _ => None,
            })
            .collect();

        return vec![OaiMessage {
            role: role.to_string(),
            content: text_content.map(OaiContent::String),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            reasoning_content: None,
        }];
    }

    // Check if message has images — if so, use multi-part content
    let has_images = msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Image { .. } | ContentBlock::Document { .. }
        )
    });

    if has_images {
        let mut parts: Vec<serde_json::Value> = Vec::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    parts.push(serde_json::json!({ "type": "text", "text": text }));
                }
                ContentBlock::Image { source } | ContentBlock::Document { source } => {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": source.data_url()
                        }
                    }));
                }
                _ => {}
            }
        }
        return vec![OaiMessage {
            role: role.to_string(),
            content: Some(OaiContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }];
    }

    // Simple text message
    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    vec![OaiMessage {
        role: role.to_string(),
        content: Some(OaiContent::String(text)),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }]
}

/// Remove `<thought>…</thought>` blocks from text content.
///
/// Gemini 3.x ignores `include_thoughts: false` in some scenarios (long
/// context, certain personas) and emits the thinking transcript inline in
/// `content` wrapped in `<thought>` tags. This belt-and-suspenders strip
/// keeps that output out of the user-visible response and out of stored
/// session messages.
/// Whether a 400 body is Gemini refusing the request's thinking level:
/// "Thinking level MINIMAL is not supported for this model."
fn refuses_thinking_level(error_body: &str) -> bool {
    let body = error_body.to_ascii_lowercase();
    body.contains("thinking level") && body.contains("not supported")
}

pub(crate) fn strip_thought_tags(text: &str) -> String {
    if !text.contains("<thought>") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<thought>") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + "<thought>".len()..];
        match after_open.find("</thought>") {
            Some(close) => rest = &after_open[close + "</thought>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Promote `EndTurn` to `ToolUse` when tool calls were emitted.
///
/// Gemini 3.x reports `finish_reason: "stop"` even when the only emitted
/// output is a tool call, and unmapped finish_reason values fall through to
/// `EndTurn`. Without this promotion the runner treats the round as terminal
/// and never dispatches the tool, producing an empty visible response.
fn promote_to_tool_use_if_pending(stop_reason: StopReason, has_tool_calls: bool) -> StopReason {
    if has_tool_calls && matches!(stop_reason, StopReason::EndTurn) {
        StopReason::ToolUse
    } else {
        stop_reason
    }
}

/// Convert OpenAI choice back to our internal format.
fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let mut content = Vec::new();

    // Thinking/reasoning content (prepend before text so it appears first)
    if let Some(ref reasoning) = choice.message.reasoning_content {
        if !reasoning.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: reasoning.clone(),
                signature: None,
            });
        }
    }

    // Text content
    if let Some(OaiContent::String(ref text)) = choice.message.content {
        let stripped = strip_thought_tags(text);
        if !stripped.is_empty() {
            content.push(ContentBlock::Text { text: stripped });
        }
    }

    // Tool calls
    let has_tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|tc| !tc.is_empty());

    if has_tool_calls {
        for tc in choice.message.tool_calls.as_ref().unwrap() {
            let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            let signature = tc
                .extra_content
                .as_ref()
                .and_then(|extra| extra.pointer("/google/thought_signature"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            content.push(ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input: super::types::sanitize_tool_input(input),
                signature,
            });
        }
    }

    let stop_reason = match choice.finish_reason.as_deref() {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("stop") => StopReason::EndTurn,
        _ => StopReason::EndTurn,
    };
    let stop_reason = promote_to_tool_use_if_pending(stop_reason, has_tool_calls);

    (content, stop_reason)
}

// ─── OpenAI-compatible types ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OaiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Reasoning/thinking content returned by Gemini when thinking is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum OaiContent {
    String(String),
    Parts(Vec<serde_json::Value>),
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiTool {
    r#type: String,
    function: OaiFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// The `thought_signature` Gemini accepts on a function call it did not
/// produce itself: one the runner made, such as an approved or collected
/// call, or one from another model's turn. Without a signature Gemini 3.x
/// rejects the request.
const UNSIGNED_CALL: &str = "skip_thought_signature_validator";

#[derive(Debug, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    r#type: String,
    function: OaiFunctionCall,
    /// Provider-specific round-trip metadata: for Gemini 3.x,
    /// `{"google": {"thought_signature": "..."}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra_content: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    model: Option<String>,
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    cache_write_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ContentBlock;

    #[test]
    fn gemini_client_default_config() {
        let client = GeminiClient::new("test-key");
        assert_eq!(client.default_model, "gemini-3.1-pro-preview");
        assert_eq!(client.default_max_tokens, 8192);
        assert_eq!(client.api_key, "test-key");
        assert!(client.thinking_budget_tokens.is_none());
    }

    #[test]
    fn gemini_client_with_model() {
        let client = GeminiClient::new("test-key").with_model("gemini-2.5-flash");
        assert_eq!(client.default_model, "gemini-2.5-flash");
    }

    #[test]
    fn gemini_client_with_max_tokens() {
        let client = GeminiClient::new("test-key").with_max_tokens(4096);
        assert_eq!(client.default_max_tokens, 4096);
    }

    #[test]
    fn a_refused_thinking_level_is_recognised_from_geminis_400() {
        // Verbatim from production, 2026-10-14, gemini-3.8-flash.
        let body = r#"[{
  "error": {
    "code": 400,
    "message": "Thinking level MINIMAL is not supported for this model. Please retry with other thinking level.",
    "status": "INVALID_ARGUMENT"
  }
}
]"#;
        assert!(refuses_thinking_level(body));
        assert!(!refuses_thinking_level(
            r#"{"error":{"message":"Request contains an invalid argument."}}"#
        ));
    }

    #[test]
    fn the_thinking_level_never_drops_below_what_the_model_accepts() {
        let client = GeminiClient::new("test-key");
        assert_eq!(client.thinking_level("gemini-3.8-flash", 0), "minimal");
        client
            .level_floor
            .lock()
            .insert("gemini-3.8-flash".to_string(), 1);
        assert_eq!(client.thinking_level("gemini-3.8-flash", 0), "low");
        assert_eq!(client.thinking_level("gemini-3.8-flash", 50_000), "high");
        assert_eq!(
            client.thinking_level("gemini-3.1-pro-preview", 0),
            "minimal"
        );
    }

    #[test]
    fn gemini_client_with_thinking_budget() {
        let client = GeminiClient::new("test-key").with_thinking_budget(10000);
        assert_eq!(client.thinking_budget_tokens, Some(10000));
    }

    #[test]
    fn convert_user_message() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "user");
        assert!(matches!(
            &oai[0].content,
            Some(OaiContent::String(s)) if s == "Hello"
        ));
    }

    #[test]
    fn convert_tool_result_message() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".to_string(),
                content: "result data".to_string(),
                is_error: Some(false),
                loaded: None,
            }],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "tool");
        assert_eq!(oai[0].tool_call_id.as_deref(), Some("call_123"));
    }

    #[test]
    fn convert_tool_result_with_image_emits_trailing_user_message() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_123".to_string(),
                    content: "ok".to_string(),
                    is_error: None,
                    loaded: None,
                },
                ContentBlock::image_base64("image/png", "abc".to_string()),
            ],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 2, "tool result + image must not drop the image");
        assert_eq!(oai[0].role, "tool");
        assert_eq!(oai[1].role, "user");
        assert!(matches!(oai[1].content, Some(OaiContent::Parts(ref p)) if p.len() == 1));
    }

    #[test]
    fn convert_tool_use_message() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call_456".to_string(),
                "my_tool".to_string(),
                serde_json::json!({"key": "value"}),
            )],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "assistant");
        let tool_calls = oai[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_456");
        assert_eq!(tool_calls[0].function.name, "my_tool");
    }

    #[test]
    fn convert_from_oai_choice_text() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("Hello world".to_string())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "Hello world"));
        assert_eq!(stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn convert_from_oai_choice_tool_use() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![OaiToolCall {
                    id: "call_1".to_string(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: "read_file".to_string(),
                        arguments: r#"{"path": "/tmp/test"}"#.to_string(),
                    },
                    extra_content: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], ContentBlock::ToolUse { name, .. } if name == "read_file"));
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn convert_from_oai_choice_with_thinking() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("The answer is 42.".to_string())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: Some("Let me think about this...".to_string()),
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, _) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 2);
        // Thinking comes first
        assert!(matches!(
            &content[0],
            ContentBlock::Thinking { thinking, .. } if thinking == "Let me think about this..."
        ));
        // Then the text response
        assert!(matches!(
            &content[1],
            ContentBlock::Text { text } if text == "The answer is 42."
        ));
    }

    #[test]
    fn build_system_content_no_cache() {
        let content = build_system_content("Hello system", false);
        assert!(matches!(content, OaiContent::String(s) if s == "Hello system"));
    }

    #[test]
    fn build_system_content_with_cache() {
        let content = build_system_content("System prompt", true);
        match content {
            OaiContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert!(parts[0]["cache_control"].is_object());
                assert_eq!(parts[0]["text"].as_str().unwrap(), "System prompt");
            }
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn build_system_content_with_cache_marker() {
        use crate::llm::anthropic::CACHE_BREAK_MARKER;
        let prompt = format!("Static part{CACHE_BREAK_MARKER}Semi-stable part");
        let content = build_system_content(&prompt, true);
        match content {
            OaiContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(parts[0]["cache_control"].is_object());
                assert!(parts[1]["cache_control"].is_object());
                assert!(parts[0]["text"].as_str().unwrap().contains("Static"));
                assert!(parts[1]["text"].as_str().unwrap().contains("Semi-stable"));
            }
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn convert_from_oai_choice_max_tokens() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("partial".to_string())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("length".to_string()),
        };
        let (_, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn promote_to_tool_use_if_pending_only_overrides_end_turn() {
        assert_eq!(
            promote_to_tool_use_if_pending(StopReason::EndTurn, true),
            StopReason::ToolUse
        );
        assert_eq!(
            promote_to_tool_use_if_pending(StopReason::EndTurn, false),
            StopReason::EndTurn
        );
        assert_eq!(
            promote_to_tool_use_if_pending(StopReason::ToolUse, true),
            StopReason::ToolUse
        );
        assert_eq!(
            promote_to_tool_use_if_pending(StopReason::MaxTokens, true),
            StopReason::MaxTokens
        );
    }

    #[test]
    fn convert_from_oai_choice_extracts_thought_signature() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![OaiToolCall {
                    id: "call_1".to_string(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: "get_time".to_string(),
                        arguments: "{}".to_string(),
                    },
                    extra_content: Some(serde_json::json!({
                        "google": { "thought_signature": "ErYFCrMF..." }
                    })),
                }]),
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, _) = convert_from_oai_choice(&choice);
        assert!(matches!(
            &content[0],
            ContentBlock::ToolUse { signature: Some(s), .. } if s == "ErYFCrMF..."
        ));
    }

    #[test]
    fn convert_to_oai_message_echoes_thought_signature() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_time".to_string(),
                input: serde_json::json!({}),
                signature: Some("ErYFCrMF...".to_string()),
            }],
        };
        let oai = convert_to_oai_message(&msg);
        let tc = &oai[0].tool_calls.as_ref().unwrap()[0];
        assert_eq!(
            tc.extra_content
                .as_ref()
                .and_then(|v| v.pointer("/google/thought_signature"))
                .and_then(|v| v.as_str()),
            Some("ErYFCrMF...")
        );
    }

    #[test]
    fn a_call_gemini_did_not_make_carries_the_signature_it_accepts_for_one() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call_1".to_string(),
                "get_time".to_string(),
                serde_json::json!({}),
            )],
        };
        let oai = convert_to_oai_message(&msg);
        let tc = &oai[0].tool_calls.as_ref().unwrap()[0];
        assert_eq!(
            tc.extra_content
                .as_ref()
                .and_then(|v| v.pointer("/google/thought_signature"))
                .and_then(|v| v.as_str()),
            Some(UNSIGNED_CALL)
        );
    }

    #[test]
    fn strip_thought_tags_removes_blocks_and_preserves_response() {
        let input =
            "<thought>**Penny's reasoning**\n\nI should reply briefly.</thought>You're welcome.";
        assert_eq!(strip_thought_tags(input), "You're welcome.");
    }

    #[test]
    fn strip_thought_tags_passes_through_when_no_tag() {
        assert_eq!(strip_thought_tags("Hello there"), "Hello there");
    }

    #[test]
    fn strip_thought_tags_handles_multiple_blocks() {
        let input = "<thought>first</thought>part one <thought>second</thought>part two";
        assert_eq!(strip_thought_tags(input), "part one part two");
    }

    #[test]
    fn strip_thought_tags_drops_unmatched_open_tag() {
        // A truncated <thought> with no closer is treated as "thinking spilled
        // through end of stream"; we drop everything from the opener onward
        // rather than show half a thought to the user.
        let input = "Real reply.\n<thought>incomplete reasoning…";
        assert_eq!(strip_thought_tags(input), "Real reply.");
    }

    #[test]
    fn convert_from_oai_choice_promotes_stop_to_tool_use_when_tool_calls_present() {
        // Gemini 3.x returns `finish_reason: "stop"` even when the only
        // emitted output is a tool call. Without this promotion the runner
        // would terminate the round before dispatching the tool, producing
        // an empty visible response.
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![OaiToolCall {
                    id: "call_1".to_string(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: "get_time".to_string(),
                        arguments: "{}".to_string(),
                    },
                    extra_content: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (_, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn stream_request_serialises_include_usage() {
        // The OpenAI-compat layer drops the streaming `usage` block
        // unless `stream_options.include_usage` is set, which is what
        // makes cost_records actually carry token counts.
        let request = OaiStreamRequest {
            model: "gemini-2.5-pro".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["stream_options"]["include_usage"], true);
    }

    // ─── GeminiStreamState ─────────────────────────────────────────────────
    //
    // These tests pin the per-chunk → StreamEvent translation. The 3.x
    // `thought_signature` plumbing is exercised here at the producer level
    // because the previous regression (chunk-1 carrying the signature, a
    // duplicate-name chunk-2 wiping it from the collector) was only visible
    // when the producer was driven with realistic multi-chunk input.

    fn tc_with(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        signature: Option<&str>,
    ) -> serde_json::Value {
        let mut tc = serde_json::json!({ "index": index });
        if let Some(id) = id {
            tc["id"] = serde_json::json!(id);
        }
        let mut func = serde_json::Map::new();
        if let Some(name) = name {
            func.insert("name".to_string(), serde_json::json!(name));
        }
        if let Some(args) = args {
            func.insert("arguments".to_string(), serde_json::json!(args));
        }
        if !func.is_empty() {
            tc["function"] = serde_json::Value::Object(func);
        }
        if let Some(sig) = signature {
            tc["extra_content"] = serde_json::json!({ "google": { "thought_signature": sig } });
        }
        tc
    }

    fn delta_with_tools(tcs: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "tool_calls": tcs })
    }

    #[test]
    fn stream_state_emits_tool_use_start_with_inline_signature() {
        // Single chunk carrying name + args + signature should produce
        // exactly one ToolUseStart with the signature attached, plus one
        // InputJsonDelta. No ToolSignatureUpdate (the start already has it).
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");
        let delta = delta_with_tools(vec![tc_with(
            0,
            Some("call_1"),
            Some("vault_set"),
            Some("{\"path\":\"x\"}"),
            Some("SigA"),
        )]);
        let out = state.process_choice_delta(&delta, None);

        let starts: Vec<_> = out
            .events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
            .collect();
        let updates: Vec<_> = out
            .events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolSignatureUpdate { .. }))
            .collect();
        let deltas: Vec<_> = out
            .events
            .iter()
            .filter(|e| matches!(e, StreamEvent::InputJsonDelta { .. }))
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(updates.len(), 0);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(
            starts[0],
            StreamEvent::ToolUseStart { signature: Some(s), name, .. }
                if s == "SigA" && name == "vault_set"
        ));
    }

    #[test]
    fn stream_state_preserves_signature_when_followup_chunk_repeats_name_without_signature() {
        // The regression case: Gemini's compat layer re-emits `function.name`
        // on follow-up chunks for the same tc_index, with no
        // `thought_signature`. Treating the second chunk as a fresh
        // ToolUseStart wipes the captured signature in the collector and
        // 400s the next request. The producer must emit ToolUseStart only
        // once per tc_index — follow-up chunks contribute args, not starts.
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");

        let out1 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("{\"path\":"),
                Some("SigA"),
            )]),
            None,
        );
        let out2 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("\"x\"}"),
                None,
            )]),
            None,
        );

        let all: Vec<_> = out1.events.iter().chain(out2.events.iter()).collect();
        let starts: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
            .collect();
        let updates: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolSignatureUpdate { .. }))
            .collect();
        let deltas: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::InputJsonDelta { .. }))
            .collect();

        assert_eq!(
            starts.len(),
            1,
            "ToolUseStart must fire exactly once per tc_index"
        );
        assert_eq!(
            updates.len(),
            0,
            "no signature update — follow-up chunk had no signature"
        );
        assert_eq!(deltas.len(), 2, "both chunks contribute args");
        assert!(matches!(
            starts[0],
            StreamEvent::ToolUseStart { signature: Some(s), .. } if s == "SigA"
        ));

        // Round-trip through the collector to confirm the signature survives.
        let mut collector = super::super::streaming::StreamCollector::new();
        collector
            .process(StreamEvent::ContentStart { index: 0 })
            .unwrap();
        for ev in out1.events.into_iter().chain(out2.events.into_iter()) {
            collector.process(ev).unwrap();
        }
        collector
            .process(StreamEvent::ContentStop { index: 1 })
            .unwrap();
        collector
            .process(StreamEvent::MessageDone {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })
            .unwrap();
        let (blocks, _, _) = collector.finish();
        let tool = blocks
            .iter()
            .find(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .expect("tool use block present");
        assert!(matches!(
            tool,
            ContentBlock::ToolUse { signature: Some(s), input, .. }
                if s == "SigA" && input["path"].as_str() == Some("x")
        ));
    }

    #[test]
    fn stream_state_routes_late_signature_through_update_event() {
        // Symmetric case: chunk 1 brings name + args (no sig), chunk 2
        // brings only the signature on a delta that has no `function.name`.
        // ToolUseStart fires with signature: None; the late chunk emits
        // ToolSignatureUpdate so the collector still attaches it.
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");

        let out1 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("{}"),
                None,
            )]),
            None,
        );
        let out2 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(0, None, None, None, Some("LateSig"))]),
            None,
        );

        let all: Vec<_> = out1.events.iter().chain(out2.events.iter()).collect();
        let starts: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
            .collect();
        let updates: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolSignatureUpdate { .. }))
            .collect();

        assert_eq!(starts.len(), 1);
        assert!(matches!(
            starts[0],
            StreamEvent::ToolUseStart {
                signature: None,
                ..
            }
        ));
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            StreamEvent::ToolSignatureUpdate { signature: s, .. } if s == "LateSig"
        ));
    }

    #[test]
    fn stream_state_handles_signature_arriving_on_followup_chunk_with_name_repeated() {
        // Belt-and-braces: chunk 1 has name + args, no sig. Chunk 2
        // *also* has name (the compat-layer quirk) AND brings the sig.
        // Must: not re-emit ToolUseStart, must route sig as
        // ToolSignatureUpdate, must keep all args.
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");

        let out1 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("{\"k\":"),
                None,
            )]),
            None,
        );
        let out2 = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("\"v\"}"),
                Some("SigLate"),
            )]),
            None,
        );

        let all: Vec<_> = out1.events.iter().chain(out2.events.iter()).collect();
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
                .count(),
            1
        );
        let updates: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolSignatureUpdate { .. }))
            .collect();
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            StreamEvent::ToolSignatureUpdate { signature: s, .. } if s == "SigLate"
        ));
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, StreamEvent::InputJsonDelta { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn stream_state_closes_text_block_before_first_tool_call() {
        // When text content has been streaming and a tool call begins, the
        // text block needs ContentStop + content_index bump so the collector
        // flushes it before opening the tool slot. The runner depends on
        // this boundary for progress emission.
        let mut state = GeminiStreamState::new("gemini-2.5-pro");

        let _ = state.process_choice_delta(&serde_json::json!({ "content": "hi" }), None);
        let out = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(
                0,
                Some("call_1"),
                Some("vault_set"),
                Some("{}"),
                None,
            )]),
            None,
        );

        let kinds: Vec<&'static str> = out
            .events
            .iter()
            .map(|e| match e {
                StreamEvent::ContentStop { .. } => "content_stop",
                StreamEvent::ToolUseStart { .. } => "tool_use_start",
                StreamEvent::InputJsonDelta { .. } => "input_json_delta",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["content_stop", "tool_use_start", "input_json_delta"]
        );
        assert_eq!(state.content_index, 1);
        assert!(!state.content_started);
    }

    #[test]
    fn stream_state_finish_reason_emits_content_stop_and_promotes_stop_reason() {
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");
        let out = state.process_choice_delta(&serde_json::json!({}), Some("tool_calls"));
        assert!(out.saw_finish);
        assert_eq!(out.stop_reason, Some(StopReason::ToolUse));
        assert!(matches!(
            out.events.as_slice(),
            [StreamEvent::ContentStop { .. }]
        ));
    }

    #[test]
    fn stream_state_drops_orphan_args_before_function_name() {
        // Defensive: if Gemini ever delivers args on a chunk before
        // function.name (not observed in practice), we drop them rather
        // than emit InputJsonDelta into a slot the collector hasn't opened.
        // A warn fires so we'd notice in production logs.
        let mut state = GeminiStreamState::new("gemini-3.1-pro-preview");
        let out = state.process_choice_delta(
            &delta_with_tools(vec![tc_with(0, None, None, Some("{}"), None)]),
            None,
        );
        assert!(out.events.is_empty());
        assert!(!state.has_tool_calls);
        assert!(state.started_tool_indices.is_empty());
    }
}

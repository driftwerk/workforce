use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use eyre::Result;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;

use super::oai_common::{
    build_messages, build_reasoning_config, build_tool_defs, convert_from_oai_choice, OaiRequest,
    OaiResponse, ReasoningConfig,
};
use super::streaming::StreamEvent;
use super::types::{LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage};

const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Client for the OpenRouter API.
///
/// OpenRouter uses the OpenAI chat completions format and supports many models
/// including DeepSeek, Qwen, GPT, Gemini, etc. Supports streaming, tool calling,
/// and essential tools filtering for large tool sets.
pub struct OpenRouterClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    thinking_budget_tokens: Option<u32>,
    /// Overrides the default OpenRouter API origin. Used by tests
    /// (point at wiremock) and self-hosted proxy deployments.
    base_url: Option<String>,
}

impl OpenRouterClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            api_key: api_key.into(),
            default_model: "deepseek/deepseek-v4-pro".to_string(),
            default_max_tokens: 8192,
            thinking_budget_tokens: None,
            base_url: None,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    fn chat_url(&self) -> String {
        match &self.base_url {
            Some(b) => format!("{}/api/v1/chat/completions", b.trim_end_matches('/')),
            None => OPENROUTER_API_URL.to_string(),
        }
    }

    fn models_url(&self) -> String {
        match &self.base_url {
            Some(b) => format!("{}/api/v1/models", b.trim_end_matches('/')),
            None => OPENROUTER_MODELS_URL.to_string(),
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

    /// Auth-only credential probe.
    ///
    /// Hits `GET /api/v1/models` which validates the API key without
    /// invoking any model — bypasses OpenRouter's upstream routing, so
    /// a flaky downstream provider (e.g. AtlasCloud returning 400 on a
    /// completion test) doesn't masquerade as an auth failure.
    pub async fn verify_credentials(&self) -> Result<()> {
        let resp = self
            .client
            .get(self.models_url())
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("OpenRouter verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "OpenRouter verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    /// Post a chat request. A request that asked for no reasoning and was
    /// refused (a 400) is sent once more asking for `minimal`; if that one
    /// succeeds the model is remembered as needing reasoning, and if it
    /// doesn't, the first refusal is what the caller sees. Every model with
    /// mandatory reasoning refuses `none`, not always in the same words.
    async fn send_chat(
        &self,
        request: &mut OaiRequest,
        model: &str,
    ) -> std::result::Result<std::result::Result<reqwest::Response, ApiFailure>, reqwest::Error>
    {
        let response = self.post_chat(request).await?;
        if response.status().is_success() {
            return Ok(Ok(response));
        }
        let failure = ApiFailure::read(response).await;
        let asked_for_none = request
            .reasoning
            .as_ref()
            .is_some_and(|r| r.effort == NO_REASONING);
        if !asked_for_none || failure.status != StatusCode::BAD_REQUEST {
            return Ok(Err(failure));
        }
        request.reasoning = Some(least_reasoning(LEAST_MANDATORY_REASONING));
        let retry = self.post_chat(request).await?;
        if !retry.status().is_success() {
            return Ok(Err(failure));
        }
        learn_reasoning_required(model);
        Ok(Ok(retry))
    }

    async fn post_chat(
        &self,
        request: &OaiRequest,
    ) -> std::result::Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/driftwerk/workforce")
            .header("X-Title", "Workforce AI Framework")
            .json(request)
            .send()
            .await
    }

    /// Send a message and get a response (non-streaming).
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
        let model = model.strip_prefix("openrouter:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "openrouter",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, enable_cache, model);

        let oai_tools = build_tool_defs(tools);

        // Background utility callers (memory/lesson extraction, summaries) scope
        // this on: reasoning models otherwise spend the whole token budget on
        // hidden reasoning and return empty/truncated `content`, silently
        // dropping the extraction. See `provider::with_reasoning_suppressed`
        // and `reasoning_for`.
        let suppress_reasoning = super::provider::reasoning_suppressed();
        let mut request = OaiRequest {
            model: Some(model.to_string()),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning: reasoning_for(suppress_reasoning, self.effective_thinking_budget(), model),
            stream: Some(false),
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            reasoning_suppressed = suppress_reasoning,
            "Sending request to OpenRouter"
        );

        let response = match self
            .send_chat(&mut request, model)
            .await
            .map_err(|e| eyre::eyre!("OpenRouter HTTP request failed: {}", e))?
        {
            Ok(response) => response,
            Err(failure) => {
                let mut err_msg = format!(
                    "OpenRouter API error ({}): {}",
                    failure.status, failure.body
                );
                if let Some(retry) = failure.retry_after {
                    err_msg.push_str(&format!(" (retry-after: {}s)", retry));
                }
                return Err(eyre::eyre!(err_msg));
            }
        };

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse OpenRouter response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in OpenRouter response"))?;

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
        let stop_reason = cut_off_if_spent(stop_reason, usage.output_tokens, max_tokens);

        tracing::info!(
            model = %api_response.model.as_deref().unwrap_or(model),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            content_blocks = content.len(),
            stop_reason = ?stop_reason,
            "OpenRouter response received"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response.model.unwrap_or_else(|| model.to_string()),
        })
    }

    /// Stream a message response using OpenAI-compatible SSE streaming.
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
        let model = model.strip_prefix("openrouter:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "openrouter",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, enable_cache, model);

        let oai_tools = build_tool_defs(tools);

        let suppress_reasoning = super::provider::reasoning_suppressed();
        let mut request = OaiRequest {
            model: Some(model.to_string()),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning: reasoning_for(suppress_reasoning, self.effective_thinking_budget(), model),
            stream: Some(true),
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            reasoning_suppressed = suppress_reasoning,
            "Sending streaming request to OpenRouter"
        );

        let response = match self
            .send_chat(&mut request, model)
            .await
            .map_err(|e| eyre::eyre!("OpenRouter streaming request failed: {}", e))?
        {
            Ok(response) => response,
            Err(failure) => {
                return Err(eyre::eyre!(
                    "OpenRouter streaming API error ({}): {}",
                    failure.status,
                    failure.body
                ));
            }
        };

        // Parse SSE stream (standard OpenAI format: data: {json}\n\n)
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut content_started = false;
        let mut content_index: usize = 0;
        let mut got_done = false;
        let mut got_finish_reason = false;
        let mut has_content = false;
        let mut has_tool_calls = false;
        let mut total_usage = TokenUsage::default();
        let mut final_stop_reason = StopReason::EndTurn;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                // Handle both single-line and multi-line data: formats
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
                    let details = &usage["prompt_tokens_details"];
                    total_usage = TokenUsage::from_oai(
                        usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                        usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
                        details["cached_tokens"].as_u64().unwrap_or(0) as u32,
                        details["cache_write_tokens"].as_u64().unwrap_or(0) as u32,
                    );
                }

                let choices = match json["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };

                for choice in choices {
                    let delta = &choice["delta"];
                    let finish = choice["finish_reason"].as_str();

                    // Text content delta
                    if let Some(content) = delta["content"].as_str() {
                        if !content.is_empty() {
                            has_content = true;
                            if !content_started {
                                content_started = true;
                                let _ = tx
                                    .send(StreamEvent::ContentStart {
                                        index: content_index,
                                    })
                                    .await;
                            }
                            let _ = tx
                                .send(StreamEvent::TextDelta {
                                    index: content_index,
                                    text: content.to_string(),
                                })
                                .await;
                        }
                    }

                    // Tool call deltas
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            let tc_index = tc["index"].as_u64().unwrap_or(0) as usize;
                            let idx = tc_index + 1;

                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func["name"].as_str() {
                                    has_tool_calls = true;
                                    if content_started {
                                        content_started = false;
                                        let _ = tx
                                            .send(StreamEvent::ContentStop {
                                                index: content_index,
                                            })
                                            .await;
                                        content_index += 1;
                                    }
                                    let id = tc["id"].as_str().unwrap_or("unknown").to_string();
                                    let _ = tx
                                        .send(StreamEvent::ToolUseStart {
                                            index: idx,
                                            id,
                                            name: name.to_string(),
                                            signature: None,
                                        })
                                        .await;
                                }
                                if let Some(args) = func["arguments"].as_str() {
                                    if !args.is_empty() {
                                        let _ = tx
                                            .send(StreamEvent::InputJsonDelta {
                                                index: idx,
                                                partial: args.to_string(),
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }

                    // Finish reason
                    if let Some(reason) = finish {
                        got_finish_reason = true;
                        match reason {
                            "tool_calls" => final_stop_reason = StopReason::ToolUse,
                            "length" => final_stop_reason = StopReason::MaxTokens,
                            "stop" => final_stop_reason = StopReason::EndTurn,
                            _ => {}
                        }
                        let _ = tx
                            .send(StreamEvent::ContentStop {
                                index: content_index,
                            })
                            .await;
                        content_index += 1;
                    }
                }
            }
        }

        if !got_done && !has_content && !has_tool_calls {
            tracing::error!(
                "OpenRouter stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "OpenRouter streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "OpenRouter stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "OpenRouter streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "OpenRouter stream ended without [DONE] marker but response appears complete"
            );
        }

        let _ = tx
            .send(StreamEvent::MessageDone {
                stop_reason: cut_off_if_spent(
                    final_stop_reason,
                    total_usage.output_tokens,
                    max_tokens,
                ),
                usage: total_usage,
            })
            .await;

        Ok(())
    }
}

/// A non-success reply from the chat endpoint.
struct ApiFailure {
    status: StatusCode,
    retry_after: Option<String>,
    body: String,
}

impl ApiFailure {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "unknown error".to_string());
        Self {
            status,
            retry_after,
            body,
        }
    }
}

const NO_REASONING: &str = "none";
/// The least reasoning a model with mandatory reasoning accepts. Not used
/// for models that allow `none`: DeepSeek v4 Pro reasons more on `minimal`
/// than with no `reasoning` field at all.
const LEAST_MANDATORY_REASONING: &str = "minimal";

/// Models that refused `reasoning: none` and then accepted `minimal`,
/// learned from the API: OpenRouter's model catalogue doesn't say which
/// models' reasoning is mandatory.
static REASONING_REQUIRED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn reasoning_required(model: &str) -> bool {
    REASONING_REQUIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(model)
}

fn learn_reasoning_required(model: &str) {
    let learned = REASONING_REQUIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(model.to_string());
    if learned {
        tracing::info!(
            model,
            "OpenRouter: model refuses reasoning `none`; asking it for `minimal` from now on"
        );
    }
}

fn least_reasoning(effort: &str) -> ReasoningConfig {
    ReasoningConfig {
        effort: effort.to_string(),
    }
}

/// A reply that used its whole token allowance was cut off, whatever the
/// route reports: OpenRouter says `tool_calls`, not `length`, for a tool call
/// cut off mid-arguments, whose arguments then don't parse.
fn cut_off_if_spent(stop_reason: StopReason, output_tokens: u32, max_tokens: u32) -> StopReason {
    if output_tokens >= max_tokens {
        StopReason::MaxTokens
    } else {
        stop_reason
    }
}

/// The request's `reasoning` field. A suppressed call, or a thinking budget
/// of zero, asks for the least reasoning the model allows: `none`, or
/// `minimal` for a model known to refuse `none` (see `send_chat`). Leaving
/// the field out isn't suppression: a model that reasons by default spends
/// the budget thinking anyway.
fn reasoning_for(
    suppressed: bool,
    thinking_budget: Option<u32>,
    model: &str,
) -> Option<ReasoningConfig> {
    if suppressed || thinking_budget == Some(0) {
        return Some(least_reasoning(if reasoning_required(model) {
            LEAST_MANDATORY_REASONING
        } else {
            NO_REASONING
        }));
    }
    build_reasoning_config(thinking_budget)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_client_default_config() {
        let client = OpenRouterClient::new("test-key");
        assert_eq!(client.default_model, "deepseek/deepseek-v4-pro");
        assert_eq!(client.default_max_tokens, 8192);
    }

    #[test]
    fn openrouter_client_with_model() {
        let client = OpenRouterClient::new("test-key").with_model("anthropic/claude-sonnet-4");
        assert_eq!(client.default_model, "anthropic/claude-sonnet-4");
    }

    // OAI conversion, cache control, and build_messages tests live in oai_common.rs.

    fn ok_body() -> serde_json::Value {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "notes"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
            "model": "m"
        })
    }

    fn effort(effort: &str) -> wiremock::matchers::BodyPartialJsonMatcher {
        body_partial_json(serde_json::json!({"reasoning": {"effort": effort}}))
    }

    async fn suppressed_call(client: &OpenRouterClient, model: &str) -> Result<LlmResponse> {
        use crate::llm::types::{ContentBlock, Role};

        let messages = [LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "summarise".into(),
            }],
        }];
        crate::llm::provider::with_reasoning_suppressed(client.create_message(
            None,
            &messages,
            &[],
            Some(model),
            Some(2_000),
            false,
        ))
        .await
    }

    #[tokio::test]
    async fn a_model_that_allows_no_reasoning_gets_none_first_time() {
        // DeepSeek v4 Pro: no `reasoning` field reasons by default, and
        // `minimal` reasons more still; only `none` stops it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(effort("none"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(effort("minimal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(0)
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("k").with_base_url(server.uri());
        let response = suppressed_call(&client, "test/allows-none").await.unwrap();
        assert_eq!(response.text().as_deref(), Some("notes"));
    }

    #[tokio::test]
    async fn a_model_that_refuses_none_is_asked_for_minimal_and_remembered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(effort("none"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(effort("minimal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(2)
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("k").with_base_url(server.uri());
        let model = "test/needs-reasoning";
        assert_eq!(
            suppressed_call(&client, model)
                .await
                .unwrap()
                .text()
                .as_deref(),
            Some("notes")
        );
        // The second call goes straight to `minimal`: `none` is asked once.
        suppressed_call(&client, model).await.unwrap();
    }

    #[tokio::test]
    async fn a_refusal_the_retry_does_not_fix_surfaces_the_first_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(effort("none"))
            .respond_with(ResponseTemplate::new(400).set_body_string("the first refusal"))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(effort("minimal"))
            .respond_with(ResponseTemplate::new(400).set_body_string("the retry's refusal"))
            .expect(2)
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("k").with_base_url(server.uri());
        let model = "test/broken-request";
        let err = suppressed_call(&client, model).await.unwrap_err();
        assert!(err.to_string().contains("the first refusal"), "{err}");
        // Not remembered: the next call asks for `none` again.
        let again = suppressed_call(&client, model).await.unwrap_err();
        assert!(again.to_string().contains("the first refusal"), "{again}");
    }

    #[tokio::test]
    async fn a_streaming_call_is_asked_again_before_anything_is_streamed() {
        use crate::llm::types::{ContentBlock, Role};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(effort("none"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Provider returned error"))
            .expect(1)
            .mount(&server)
            .await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"notes\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(effort("minimal"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("k").with_base_url(server.uri());
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        let (tx, mut rx) = mpsc::channel(64);
        crate::llm::provider::with_reasoning_suppressed(client.create_message_stream(
            None,
            &messages,
            &[],
            Some("test/streams-needs-reasoning"),
            Some(2_000),
            false,
            tx,
        ))
        .await
        .unwrap();
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::TextDelta { text: t, .. } = event {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "notes");
    }

    /// Every OpenRouter tier model, asked with reasoning suppressed, ends
    /// with text well inside its cap. Run against the live API with a
    /// realistic prompt — a toy prompt hides how much a model reasons:
    ///
    /// `OPENROUTER_API_KEY=… OPENROUTER_LIVE_PROMPT=<file> cargo test -p
    /// workforce-agent --lib live_suppressed -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "calls OpenRouter; needs OPENROUTER_API_KEY and OPENROUTER_LIVE_PROMPT"]
    async fn live_suppressed_calls_end_with_text_on_every_tier_model() {
        use crate::llm::types::{ContentBlock, Role};

        const CAP: u32 = 4_000;
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY");
        let prompt = std::fs::read_to_string(
            std::env::var("OPENROUTER_LIVE_PROMPT").expect("OPENROUTER_LIVE_PROMPT"),
        )
        .expect("read the prompt file");
        let tiers: toml::Table = include_str!("default_tiers.toml").parse().unwrap();
        let mut models: Vec<String> = tiers["openrouter"]
            .as_table()
            .unwrap()
            .values()
            .filter_map(toml::Value::as_table)
            .flat_map(|t| {
                t.values()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        models.sort();
        models.dedup();

        let client = OpenRouterClient::new(&key);
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }];
        let mut failures = Vec::new();
        for model in &models {
            for attempt in 1..=2 {
                let outcome = crate::llm::provider::with_reasoning_suppressed(
                    client.create_message(None, &messages, &[], Some(model), Some(CAP), false),
                )
                .await;
                let line = match &outcome {
                    Ok(r) => {
                        let text = r.text().unwrap_or_default();
                        let ok = r.stop_reason == StopReason::EndTurn
                            && !text.trim().is_empty()
                            && r.usage.output_tokens < CAP;
                        format!(
                            "{} {model} #{attempt}: stop={:?} output_tokens={} text_chars={}",
                            if ok { "PASS" } else { "FAIL" },
                            r.stop_reason,
                            r.usage.output_tokens,
                            text.len()
                        )
                    }
                    Err(e) => format!(
                        "FAIL {model} #{attempt}: {}",
                        e.to_string().chars().take(160).collect::<String>()
                    ),
                };
                println!("LIVE {line}");
                if line.starts_with("FAIL") {
                    failures.push(line);
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[tokio::test]
    async fn a_tool_call_cut_off_at_the_cap_is_reported_as_cut_off() {
        use crate::llm::types::{ContentBlock, Role};

        // What OpenRouter returned for a tool call cut off at 300 tokens.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "",
                    "tool_calls": [{"id": "c1", "type": "function", "function":
                        {"name": "edit", "arguments": "{\"old_text\": \"## Flags\\n- `--al"}}]},
                    "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 900, "completion_tokens": 300},
                "model": "deepseek/deepseek-v4-pro"
            })))
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("sk-or-v1-test").with_base_url(server.uri());
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "edit".into(),
            }],
        }];
        let response = client
            .create_message(None, &messages, &[], None, Some(300), false)
            .await
            .unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn a_reply_under_its_cap_keeps_its_stop_reason() {
        assert_eq!(
            cut_off_if_spent(StopReason::ToolUse, 299, 300),
            StopReason::ToolUse
        );
        assert_eq!(
            cut_off_if_spent(StopReason::EndTurn, 300, 300),
            StopReason::MaxTokens
        );
    }

    #[test]
    fn reasoning_is_left_to_the_model_unless_asked_for_or_suppressed() {
        assert!(reasoning_for(false, None, "m").is_none());
        assert_eq!(
            reasoning_for(false, Some(5_000), "m").unwrap().effort,
            "medium"
        );
        assert_eq!(
            reasoning_for(true, Some(20_000), "m").unwrap().effort,
            "none"
        );
        assert_eq!(reasoning_for(false, Some(0), "m").unwrap().effort, "none");
        learn_reasoning_required("test/known-mandatory");
        assert_eq!(
            reasoning_for(true, None, "test/known-mandatory")
                .unwrap()
                .effort,
            "minimal"
        );
    }

    use wiremock::matchers::{bearer_token, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn verify_credentials_passes_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .and(bearer_token("sk-or-v1-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("sk-or-v1-test").with_base_url(server.uri());
        client.verify_credentials().await.expect("should succeed");
    }

    #[tokio::test]
    async fn verify_credentials_fails_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"invalid"}}"#),
            )
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("bad-key").with_base_url(server.uri());
        let err = client.verify_credentials().await.expect_err("must fail");
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    #[tokio::test]
    async fn verify_credentials_does_not_call_chat_completions() {
        // The whole point: a 400 from chat/completions (e.g. AtlasCloud
        // upstream failure) must not affect verification. The `expect(0)`
        // mount makes wiremock panic on drop if verify ever touches the
        // completions endpoint.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400))
            .expect(0)
            .mount(&server)
            .await;
        let client = OpenRouterClient::new("sk-or-v1-test").with_base_url(server.uri());
        client
            .verify_credentials()
            .await
            .expect("verify must use /models only");
        server.verify().await;
    }
}

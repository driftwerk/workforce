use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;

use super::oai_common::{
    build_messages, build_reasoning_config, build_tool_defs, convert_from_oai_choice, OaiRequest,
    OaiResponse,
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
        // dropping the extraction. See `provider::with_reasoning_suppressed`.
        let suppress_reasoning = super::provider::reasoning_suppressed();
        let request = OaiRequest {
            model: Some(model.to_string()),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning: if suppress_reasoning {
                None
            } else {
                build_reasoning_config(self.effective_thinking_budget())
            },
            stream: Some(false),
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            reasoning_suppressed = suppress_reasoning,
            "Sending request to OpenRouter"
        );

        let response = self
            .client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/driftwerk/workforce")
            .header("X-Title", "Workforce AI Framework")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("OpenRouter HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            let mut err_msg = format!("OpenRouter API error ({}): {}", status, error_body);
            if let Some(retry) = retry_after {
                err_msg.push_str(&format!(" (retry-after: {}s)", retry));
            }
            return Err(eyre::eyre!(err_msg));
        }

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
        let request = OaiRequest {
            model: Some(model.to_string()),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning: if suppress_reasoning {
                None
            } else {
                build_reasoning_config(self.effective_thinking_budget())
            },
            stream: Some(true),
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            reasoning_suppressed = suppress_reasoning,
            "Sending streaming request to OpenRouter"
        );

        let response = self
            .client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/driftwerk/workforce")
            .header("X-Title", "Workforce AI Framework")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("OpenRouter streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "OpenRouter streaming API error ({}): {}",
                status,
                error_body
            ));
        }

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
                stop_reason: final_stop_reason,
                usage: total_usage,
            })
            .await;

        Ok(())
    }
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

    use wiremock::matchers::{bearer_token, method, path};
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

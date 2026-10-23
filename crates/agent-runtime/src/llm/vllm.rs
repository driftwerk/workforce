use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

/// Client for self-hosted vLLM servers with OpenAI-compatible API.
///
/// Designed for Driftwerk's RunPod infrastructure serving Qwen3-32B-AWQ (primary LLM)
/// and Qwen2.5-VL-7B (vision). Supports tool calling with two-phase selection,
/// vision routing, and API key authentication.
pub struct VllmClient {
    client: Client,
    base_url: String,
    vision_url: Option<String>,
    api_key: Option<String>,
    default_model: String,
    default_max_tokens: u32,
}

impl VllmClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let url: String = base_url.into();
        let url = url.trim_end_matches('/').to_string();

        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            base_url: url,
            vision_url: None,
            api_key: None,
            default_model: "Qwen/Qwen3-32B-AWQ".to_string(),
            default_max_tokens: 8192,
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

    pub fn with_vision_url(mut self, url: impl Into<String>) -> Self {
        let url: String = url.into();
        if !url.is_empty() {
            self.vision_url = Some(url.trim_end_matches('/').to_string());
        }
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        let key: String = key.into();
        if !key.is_empty() {
            self.api_key = Some(key);
        }
        self
    }

    /// Build an authenticated POST request to the given URL.
    fn authenticated_post(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    /// Check if the vLLM server is reachable.
    pub async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(_) => Ok(false),
            Err(_) => Ok(false),
        }
    }

    /// Send a message and get a response using the OpenAI-compatible endpoint.
    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<LlmResponse> {
        tracing::info!(
            provider = "vllm",
            model = %model.unwrap_or(&self.default_model),
            streaming = false,
            max_tokens = max_tokens.unwrap_or(self.default_max_tokens),
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );
        // Check if any message contains images and we have a vision endpoint
        let has_images = messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        });

        if has_images {
            if let Some(ref vision_url) = self.vision_url {
                return self
                    .create_vision_message(vision_url, system_prompt, messages, model, max_tokens)
                    .await;
            }
        }

        let model = model.unwrap_or(&self.default_model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);

        // Disable Qwen3 thinking mode to avoid wasting tokens on <think> chains.
        let effective_system_prompt = system_prompt.map(|s| format!("/no_think\n{}", s));
        let system_prompt = effective_system_prompt.as_deref().or(Some("/no_think"));

        let mut oai_messages: Vec<OaiMessage> = Vec::new();
        if let Some(system) = system_prompt {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(OaiContent::String(system.to_string())),
                tool_calls: None,
                tool_call_id: None,
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

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            stream: false,
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending request to vLLM"
        );

        let response = self
            .authenticated_post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("vLLM HTTP request failed: {}", e))?;

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
            let mut err_msg = format!("vLLM API error ({}): {}", status, error_body);
            if let Some(retry) = retry_after {
                err_msg.push_str(&format!(" (retry-after: {}s)", retry));
            }
            return Err(eyre::eyre!(err_msg));
        }

        let api_response: OaiResponse = parse_response(response).await?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in vLLM response"))?;

        let (content, stop_reason) = convert_from_oai_choice(&choice);

        let usage = api_response
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                ..Default::default()
            })
            .unwrap_or_default();

        tracing::info!(
            model = %api_response.model.as_deref().unwrap_or(model),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            content_blocks = content.len(),
            stop_reason = ?stop_reason,
            "vLLM non-streaming response received"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response.model.unwrap_or_else(|| model.to_string()),
        })
    }

    /// Route image-containing messages to the vision model.
    async fn create_vision_message(
        &self,
        vision_url: &str,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<LlmResponse> {
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);

        let mut oai_messages: Vec<OaiMessage> = Vec::new();

        if let Some(system) = system_prompt {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(OaiContent::String(system.to_string())),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        for msg in messages {
            let has_images = msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }));

            if has_images {
                oai_messages.extend(convert_to_oai_multimodal_message(msg));
            } else {
                oai_messages.extend(convert_to_oai_message(msg));
            }
        }

        let vision_model = model.unwrap_or("Qwen/Qwen2.5-VL-7B-Instruct");

        let request = OaiRequest {
            model: vision_model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: None,
            stream: false,
        };

        let url = format!("{}/v1/chat/completions", vision_url);

        tracing::info!(
            model = vision_model,
            messages = messages.len(),
            "Sending vision request to vLLM"
        );

        let response = self
            .authenticated_post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("vLLM vision HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(eyre::eyre!(
                "vLLM vision API error ({}): {}",
                status,
                error_body
            ));
        }

        let api_response: OaiResponse = parse_response(response).await?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in vLLM vision response"))?;

        let (content, stop_reason) = convert_from_oai_choice(&choice);

        let usage = api_response
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                ..Default::default()
            })
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response
                .model
                .unwrap_or_else(|| vision_model.to_string()),
        })
    }

    /// Stream a message response using OpenAI-compatible SSE streaming.
    ///
    /// Sends `stream: true` and parses Server-Sent Events in the OpenAI format:
    /// `data: {"choices":[{"delta":{"content":"..."}}]}`
    #[allow(clippy::too_many_arguments)]
    pub async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = model.unwrap_or(&self.default_model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "vllm",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        // Disable Qwen3 thinking mode for streaming too.
        let effective_system_prompt = system_prompt.map(|s| format!("/no_think\n{}", s));
        let system_prompt = effective_system_prompt.as_deref().or(Some("/no_think"));

        let mut oai_messages: Vec<OaiMessage> = Vec::new();
        if let Some(system) = system_prompt {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(OaiContent::String(system.to_string())),
                tool_calls: None,
                tool_call_id: None,
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

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            stream: true,
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        let response = self
            .authenticated_post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("vLLM streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "vLLM streaming API error ({}): {}",
                status,
                error_body
            ));
        }

        tracing::info!(
            status = %response.status(),
            content_type = ?response.headers().get("content-type").map(|v| v.to_str().unwrap_or("?")),
            "vLLM streaming: HTTP response received, starting SSE parse"
        );

        // Parse OpenAI-compatible SSE stream.
        // Handles both standard format (data: {json}\n\n) and RunPod's
        // pretty-printed format where each JSON line is prefixed with "data: ".
        let mut stream = response.bytes_stream();
        let mut events_parsed: usize = 0;
        let mut content_started = false; // Track if ContentStart has been sent for current block
        let mut buffer = String::new();
        let mut content_index: usize = 0;
        let mut got_done = false;
        let mut got_finish_reason = false;
        let mut has_content = false;
        let mut has_tool_calls = false;
        let mut total_usage = TokenUsage::default();
        let mut final_stop_reason = StopReason::EndTurn;
        let mut in_think_block = false; // Track <think>...</think> blocks to skip them

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                // Collect all "data: " lines in this event block and join them
                // into a single string. This handles both single-line SSE
                // (data: {"choices":...}) and RunPod's multi-line format
                // (data: {\ndata:     "choices":...\ndata: }).
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

                events_parsed += 1;

                let json: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::info!(error = %e, data_len = data.len(), data_start = &data[..data.len().min(80)], "vLLM stream: failed to parse SSE event");
                        continue;
                    }
                };

                // Extract usage if present
                if let Some(usage) = json.get("usage") {
                    total_usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
                    total_usage.output_tokens =
                        usage["completion_tokens"].as_u64().unwrap_or(0) as u32;
                }

                let choices = match json["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };

                for choice in choices {
                    let delta = &choice["delta"];
                    let finish = choice["finish_reason"].as_str();

                    // Text content delta — filter out <think>...</think> blocks
                    if let Some(content) = delta["content"].as_str() {
                        if !content.is_empty() {
                            // Track think block state
                            // Skip <think>...</think> blocks
                            if content.contains("<think>") {
                                in_think_block = true;
                                tracing::info!("vLLM stream: entering think block, skipping");
                                continue;
                            }
                            if content.contains("</think>") {
                                in_think_block = false;
                                tracing::info!("vLLM stream: exiting think block");
                                continue;
                            }
                            if in_think_block {
                                continue;
                            }

                            has_content = true;

                            tracing::info!(
                                content_len = content.len(),
                                content = content,
                                "vLLM stream: text delta"
                            );

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

        tracing::info!(
            got_done = got_done,
            got_finish_reason = got_finish_reason,
            has_content = has_content,
            has_tool_calls = has_tool_calls,
            events_parsed = events_parsed,
            content_index = content_index,
            stop_reason = ?final_stop_reason,
            in_think_block = in_think_block,
            "vLLM stream: stream ended"
        );

        if !got_done && !has_content && !has_tool_calls {
            tracing::error!(
                "vLLM stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "vLLM streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "vLLM stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "vLLM streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "vLLM stream ended without [DONE] marker but response appears complete"
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

/// Parse an HTTP response body as OaiResponse.
///
/// Handles both direct JSON (from pods/direct SGLang) and string-wrapped
/// JSON (from RunPod serverless, which wraps the response in a JSON string).
async fn parse_response(response: reqwest::Response) -> Result<OaiResponse> {
    let body = response
        .text()
        .await
        .map_err(|e| eyre::eyre!("Failed to read vLLM response body: {}", e))?;

    serde_json::from_str::<OaiResponse>(&body)
        .or_else(|_| {
            // RunPod serverless wraps responses in a JSON string:
            // "{\"id\":\"...\",\"choices\":[...]}" instead of {"id":"...","choices":[...]}
            serde_json::from_str::<String>(&body)
                .and_then(|inner| serde_json::from_str::<OaiResponse>(&inner))
        })
        .map_err(|e| {
            eyre::eyre!(
                "Failed to parse vLLM response: {} (body starts with: {})",
                e,
                &body[..body.len().min(100)]
            )
        })
}

/// Strip `<think>...</think>` wrapper from Qwen3 responses.
/// Even with `/no_think`, the model may output empty think tags.
fn strip_think_tags(text: &str) -> String {
    if let Some(end_pos) = text.find("</think>") {
        text[end_pos + "</think>".len()..].trim().to_string()
    } else if text.trim().starts_with("<think>") {
        // Unclosed think block — return empty
        String::new()
    } else {
        text.to_string()
    }
}

// ─── OpenAI message conversion ──────────────────────────────────────────────

fn convert_to_oai_message(msg: &LlmMessage) -> Vec<OaiMessage> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

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
                ContentBlock::Image { source } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": source.data_url() }
                })),
                _ => None,
            })
            .collect();
        if !extra.is_empty() {
            out.push(OaiMessage {
                role: "user".to_string(),
                content: Some(OaiContent::Array(extra)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        return out;
    }

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
                    id, name, input, ..
                } => Some(OaiToolCall {
                    id: id.clone(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_default(),
                    },
                }),
                _ => None,
            })
            .collect();

        return vec![OaiMessage {
            role: role.to_string(),
            content: text_content.map(OaiContent::String),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }];
    }

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
    }]
}

fn convert_to_oai_multimodal_message(msg: &LlmMessage) -> Vec<OaiMessage> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    let mut content_parts: Vec<serde_json::Value> = Vec::new();

    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                content_parts.push(serde_json::json!({
                    "type": "text",
                    "text": text
                }));
            }
            ContentBlock::Image { source } => {
                let data_url = source.data_url();
                content_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": data_url
                    }
                }));
            }
            _ => {}
        }
    }

    vec![OaiMessage {
        role: role.to_string(),
        content: Some(OaiContent::Array(content_parts)),
        tool_calls: None,
        tool_call_id: None,
    }]
}

fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let mut content = Vec::new();

    match &choice.message.content {
        Some(OaiContent::String(text)) if !text.is_empty() => {
            // Strip <think>...</think> wrapper from Qwen3 responses
            let cleaned = strip_think_tags(text);
            tracing::info!(
                raw_len = text.len(),
                cleaned_len = cleaned.len(),
                "vLLM response content"
            );
            if !cleaned.is_empty() {
                content.push(ContentBlock::Text { text: cleaned });
            }
        }
        Some(OaiContent::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        content.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
        }
        _ => {}
    }

    let has_tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|tc| !tc.is_empty());

    if has_tool_calls {
        for tc in choice.message.tool_calls.as_ref().unwrap() {
            let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            content.push(ContentBlock::tool_use(
                tc.id.clone(),
                tc.function.name.clone(),
                input,
            ));
        }
    }

    let stop_reason = match choice.finish_reason.as_deref() {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("stop") => StopReason::EndTurn,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };

    (content, stop_reason)
}

// ─── OpenAI-compatible types ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiTool>>,
    stream: bool,
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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum OaiContent {
    String(String),
    Array(Vec<serde_json::Value>),
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

#[derive(Debug, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    r#type: String,
    function: OaiFunctionCall,
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
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vllm_client_default_config() {
        let client = VllmClient::new("https://pod-8000.proxy.runpod.net");
        assert_eq!(client.base_url, "https://pod-8000.proxy.runpod.net");
        assert_eq!(client.default_model, "Qwen/Qwen3-32B-AWQ");
        assert_eq!(client.default_max_tokens, 8192);
        assert!(client.vision_url.is_none());
    }

    #[test]
    fn vllm_client_with_model() {
        let client =
            VllmClient::new("http://localhost:8000").with_model("Qwen/Qwen3-30B-A3B-Instruct");
        assert_eq!(client.default_model, "Qwen/Qwen3-30B-A3B-Instruct");
    }

    #[test]
    fn vllm_client_with_vision_url() {
        let client =
            VllmClient::new("http://localhost:8000").with_vision_url("http://localhost:8001");
        assert_eq!(client.vision_url.as_deref(), Some("http://localhost:8001"));
    }

    #[test]
    fn vllm_client_with_api_key() {
        let client = VllmClient::new("http://localhost:8000").with_api_key("sk-test-key");
        assert_eq!(client.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn vllm_client_empty_api_key_ignored() {
        let client = VllmClient::new("http://localhost:8000").with_api_key("");
        assert!(client.api_key.is_none());
    }

    #[test]
    fn vllm_client_strips_trailing_slash() {
        let client = VllmClient::new("http://localhost:8000/");
        assert_eq!(client.base_url, "http://localhost:8000");
    }

    // ─── Message conversion tests ───────────────────────────────────────

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
                tool_use_id: "tool_1".to_string(),
                content: "result data".to_string(),
                is_error: None,
                loaded: None,
            }],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "tool");
        assert_eq!(oai[0].tool_call_id.as_deref(), Some("tool_1"));
    }

    #[test]
    fn convert_tool_result_with_image_emits_trailing_user_message() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tool_1".to_string(),
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
        assert!(matches!(oai[1].content, Some(OaiContent::Array(ref a)) if a.len() == 1));
    }

    #[test]
    fn convert_tool_use_message() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me read that.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tc_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "/tmp/test"}),
                    signature: None,
                },
            ],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "assistant");
        assert!(oai[0].tool_calls.is_some());
        let tc = oai[0].tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].function.name, "read_file");
    }

    #[test]
    fn convert_from_oai_choice_text() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("Hello world".to_string())),
                tool_calls: None,
                tool_call_id: None,
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
                }]),
                tool_call_id: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], ContentBlock::ToolUse { name, .. } if name == "read_file"));
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn convert_from_oai_choice_max_tokens() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("partial".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("length".to_string()),
        };
        let (_, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }
}

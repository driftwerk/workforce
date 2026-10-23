use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

const CEREBRAS_API_URL: &str = "https://api.cerebras.ai/v1/chat/completions";
const CEREBRAS_MODELS_URL: &str = "https://api.cerebras.ai/v1/models";

pub struct CerebrasClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    thinking_budget_tokens: Option<u32>,
}

impl CerebrasClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            api_key: api_key.into(),
            default_model: "gpt-oss-120b".to_string(),
            default_max_tokens: 8192,
            thinking_budget_tokens: None,
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

    /// Auth-only credential probe. Hits `GET /v1/models`.
    pub async fn verify_credentials(&self) -> Result<()> {
        let resp = self
            .client
            .get(CEREBRAS_MODELS_URL)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Cerebras verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Cerebras verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    fn build_reasoning_effort(&self, model: &str) -> Option<String> {
        let is_gpt_oss = model.contains("gpt-oss");
        let is_glm = model.contains("glm");

        if !is_gpt_oss && !is_glm {
            return None;
        }

        self.effective_thinking_budget().map(|budget| {
            match budget {
                0 => "none",
                1..=2000 => "low",
                2001..=8000 => "medium",
                _ => "high",
            }
            .to_string()
        })
    }

    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<LlmResponse> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("cerebras:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "cerebras",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, model);
        let oai_tools = build_tool_defs(tools);
        let is_glm = model.contains("glm");

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning_effort: self.build_reasoning_effort(model),
            clear_thinking: is_glm.then_some(false),
            temperature: is_glm.then_some(0.8),
            top_p: is_glm.then_some(0.95),
            stream: false,
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending request to Cerebras"
        );

        let response = self
            .client
            .post(CEREBRAS_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Cerebras HTTP request failed: {}", e))?;

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
            let mut err_msg = format!("Cerebras API error ({}): {}", status, error_body);
            if let Some(retry) = retry_after {
                err_msg.push_str(&format!(" (retry-after: {}s)", retry));
            }
            return Err(eyre::eyre!(err_msg));
        }

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Cerebras response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Cerebras response"))?;

        let (content, stop_reason) = convert_from_oai_choice(&choice);

        // Cerebras prompt caching is automatic: only `cached_tokens` (reads)
        // is reported, with no creation field and no price discount (latency
        // benefit only). The from_oai split still satisfies the TokenUsage
        // fresh-only invariant; cost is unchanged because Cerebras pricing
        // leaves cache_read_per_million unset, so cached tokens fall back to
        // the regular input rate.
        let usage = api_response
            .usage
            .map(|u| {
                TokenUsage::from_oai(
                    u.prompt_tokens,
                    u.completion_tokens,
                    u.prompt_tokens_details
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0),
                    0,
                )
            })
            .unwrap_or_default();

        tracing::info!(
            model = %api_response.model.as_deref().unwrap_or(model),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            content_blocks = content.len(),
            stop_reason = ?stop_reason,
            "Cerebras response received"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response.model.unwrap_or_else(|| model.to_string()),
        })
    }

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
        let model = model.strip_prefix("cerebras:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "cerebras",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, model);
        let oai_tools = build_tool_defs(tools);
        let is_glm = model.contains("glm");

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            reasoning_effort: self.build_reasoning_effort(model),
            clear_thinking: is_glm.then_some(false),
            temperature: is_glm.then_some(0.8),
            top_p: is_glm.then_some(0.95),
            stream: true,
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending streaming request to Cerebras"
        );

        let response = self
            .client
            .post(CEREBRAS_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Cerebras streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            // Capture retry-after before consuming the body — the failover
            // layer's gated rate-limit retry and the pool cooldown both key
            // off this hint; without it a 429 means a blind 60s cooldown.
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            let error_body = response.text().await.unwrap_or_default();
            let mut err_msg = format!("Cerebras streaming API error ({status}): {error_body}");
            if let Some(ref ra) = retry_after {
                err_msg.push_str(&format!(" [retry-after: {ra}s]"));
            }
            return Err(eyre::eyre!(err_msg));
        }

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

                if let Some(usage) = usage_from_stream_chunk(&json) {
                    total_usage = usage;
                }

                let choices = match json["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };

                for choice in choices {
                    let delta = &choice["delta"];
                    let finish = choice["finish_reason"].as_str();

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
                "Cerebras stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "Cerebras streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "Cerebras stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "Cerebras streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "Cerebras stream ended without [DONE] marker but response appears complete"
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

const GLM_SYSTEM_PREFIX: &str = "\
You MUST follow these rules STRICTLY:\n\
1. You MUST always respond in English unless the user explicitly requests another language.\n\
2. You MUST use the provided tools for actions — NEVER output tool-call JSON as plain text.\n\
3. You MUST break complex tasks into small, well-defined sub-steps and execute them one at a time.\n\
4. You MUST place your final answer or action AFTER any reasoning — do NOT repeat the reasoning in the answer.\n\n";

/// Usage from a streaming chunk, with the same `from_oai` split as the
/// non-streaming path. Without reading `prompt_tokens_details.cached_tokens`,
/// streaming rounds (the agent loop's only path) reported cache_read=0
/// forever and overcounted fresh input by the cached amount — the "Cerebras
/// cache regression" was this gap, unmasked when 429 retries stopped
/// interleaving deepseek rounds that do report hits.
fn usage_from_stream_chunk(json: &serde_json::Value) -> Option<TokenUsage> {
    let usage = json.get("usage")?;
    Some(TokenUsage::from_oai(
        usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
        usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
        0,
    ))
}

fn build_messages(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    model: &str,
) -> Vec<OaiMessage> {
    let mut oai_messages: Vec<OaiMessage> = Vec::new();

    let is_glm = model.contains("glm");

    let final_system = match (is_glm, system_prompt) {
        (true, Some(sys)) => Some(format!("{GLM_SYSTEM_PREFIX}{sys}")),
        (true, None) => Some(GLM_SYSTEM_PREFIX.to_string()),
        (false, sys) => sys.map(str::to_string),
    };

    if let Some(ref system) = final_system {
        oai_messages.push(OaiMessage {
            role: "system".to_string(),
            content: Some(system.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for msg in messages {
        oai_messages.extend(convert_to_oai_message(msg));
    }

    oai_messages
}

fn build_tool_defs(tools: &[LlmToolDef]) -> Option<Vec<OaiTool>> {
    if tools.is_empty() {
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
    }
}

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
        return msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some(OaiMessage {
                    role: "tool".to_string(),
                    content: Some(content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                }),
                _ => None,
            })
            .collect();
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
            content: text_content,
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
        content: Some(text),
        tool_calls: None,
        tool_call_id: None,
    }]
}

fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let mut content = Vec::new();

    if let Some(ref text) = choice.message.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.clone() });
        }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
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
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streaming usage must apply the same fresh-only split as the
    /// non-streaming path: cached tokens land in cache_read and come OUT of
    /// input_tokens.
    #[test]
    fn stream_chunk_usage_extracts_cached_tokens() {
        let json = serde_json::json!({
            "usage": {
                "prompt_tokens": 50_000,
                "completion_tokens": 800,
                "prompt_tokens_details": { "cached_tokens": 42_000 }
            }
        });
        let usage = usage_from_stream_chunk(&json).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 42_000);
        assert_eq!(usage.input_tokens, 8_000);
        assert_eq!(usage.output_tokens, 800);

        let no_details = serde_json::json!({
            "usage": { "prompt_tokens": 100, "completion_tokens": 10 }
        });
        let usage = usage_from_stream_chunk(&no_details).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.input_tokens, 100);

        assert!(usage_from_stream_chunk(&serde_json::json!({"choices": []})).is_none());
    }

    #[test]
    fn default_config() {
        let client = CerebrasClient::new("test-key");
        assert_eq!(client.default_model, "gpt-oss-120b");
        assert_eq!(client.default_max_tokens, 8192);
    }

    #[test]
    fn with_model() {
        let client = CerebrasClient::new("test-key").with_model("llama3.1-8b");
        assert_eq!(client.default_model, "llama3.1-8b");
    }

    #[test]
    fn reasoning_effort_supported_models() {
        let client = CerebrasClient::new("key").with_thinking_budget(5000);
        assert_eq!(
            client.build_reasoning_effort("gpt-oss-120b").unwrap(),
            "medium"
        );
        assert_eq!(
            client.build_reasoning_effort("zai-glm-4.7").unwrap(),
            "medium"
        );
        assert!(client
            .build_reasoning_effort("qwen-3-235b-a22b-instruct-2507")
            .is_none());
        assert!(client.build_reasoning_effort("llama3.1-8b").is_none());
    }

    #[test]
    fn reasoning_effort_mapping() {
        let make_effort = |budget| {
            CerebrasClient::new("key")
                .with_thinking_budget(budget)
                .build_reasoning_effort("gpt-oss-120b")
                .unwrap()
        };
        assert_eq!(make_effort(0), "none");
        assert_eq!(make_effort(1000), "low");
        assert_eq!(make_effort(2000), "low");
        assert_eq!(make_effort(2001), "medium");
        assert_eq!(make_effort(8000), "medium");
        assert_eq!(make_effort(8001), "high");
        assert_eq!(make_effort(50000), "high");
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
    }

    #[test]
    fn convert_from_oai_choice_text() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some("Hello world".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
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
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn glm_system_prefix_is_prepended() {
        let msgs = build_messages(Some("You are a helpful assistant."), &[], "zai-glm-4.7");
        assert_eq!(msgs.len(), 1);
        let content = msgs[0].content.as_ref().unwrap();
        assert!(content.starts_with("You MUST follow these rules STRICTLY:"));
        assert!(content.contains("You are a helpful assistant."));
    }

    #[test]
    fn non_glm_skips_prefix() {
        let msgs = build_messages(Some("You are a helpful assistant."), &[], "gpt-oss-120b");
        let content = msgs[0].content.as_ref().unwrap();
        assert!(content.starts_with("You are a helpful assistant."));
    }
}

use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

const ZAI_API_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const ZAI_CODING_API_URL: &str = "https://api.z.ai/api/coding/paas/v4/chat/completions";
const ZAI_MODELS_URL: &str = "https://api.z.ai/api/paas/v4/models";

pub struct ZaiClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    thinking_budget_tokens: Option<u32>,
    use_coding_endpoint: bool,
}

impl ZaiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            api_key: api_key.into(),
            default_model: "glm-5.3".to_string(),
            default_max_tokens: 65536,
            thinking_budget_tokens: None,
            use_coding_endpoint: false,
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

    pub fn with_coding_endpoint(mut self, use_coding: bool) -> Self {
        self.use_coding_endpoint = use_coding;
        self
    }

    fn api_url(&self) -> &str {
        if self.use_coding_endpoint {
            ZAI_CODING_API_URL
        } else {
            ZAI_API_URL
        }
    }

    /// Auth-only credential probe. Hits `GET /api/paas/v4/models`.
    pub async fn verify_credentials(&self) -> Result<()> {
        let resp = self
            .client
            .get(ZAI_MODELS_URL)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Z.ai verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Z.ai verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    /// Build the `thinking` config object.
    ///
    /// - `None` budget → omit (Z.ai default: enabled)
    /// - Budget `0` → `{"type": "disabled"}`, or omitted on a model that
    ///   always thinks (glm-5.3, glm-5.3-flash: Z.ai documents that thinking
    ///   can only be enabled on them)
    /// - Budget `> 0` → `{"type": "enabled", "clear_thinking": false}`
    fn build_thinking(&self, model: &str) -> Option<ThinkingConfig> {
        let always_thinks = model == "glm-5.3" || model.starts_with("glm-5.3-");
        self.effective_thinking_budget()
            .filter(|&budget| budget > 0 || !always_thinks)
            .map(|budget| {
                if budget == 0 {
                    ThinkingConfig {
                        r#type: "disabled".to_string(),
                        clear_thinking: None,
                    }
                } else {
                    ThinkingConfig {
                        r#type: "enabled".to_string(),
                        clear_thinking: Some(false),
                    }
                }
            })
    }

    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool, // Z.ai caching is automatic — no API controls
    ) -> Result<LlmResponse> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("zai:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "zai",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, model);
        let oai_tools = build_tool_defs(tools);

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            thinking: self.build_thinking(model),
            temperature: Some(1.0),
            top_p: None,
            stream: false,
            tool_stream: None,
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending request to Z.ai"
        );

        let response = self
            .client
            .post(self.api_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Z.ai HTTP request failed: {}", e))?;

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
            let mut err_msg = format!("Z.ai API error ({}): {}", status, error_body);
            if let Some(retry) = retry_after {
                err_msg.push_str(&format!(" (retry-after: {}s)", retry));
            }
            return Err(eyre::eyre!(err_msg));
        }

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Z.ai response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Z.ai response"))?;

        let (content, stop_reason) = convert_from_oai_choice(&choice);

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
            cache_read_input_tokens = usage.cache_read_input_tokens,
            content_blocks = content.len(),
            stop_reason = ?stop_reason,
            "Z.ai response received"
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
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("zai:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "zai",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages, model);
        let oai_tools = build_tool_defs(tools);
        let has_tools = oai_tools.is_some();

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            thinking: self.build_thinking(model),
            temperature: Some(1.0),
            top_p: None,
            stream: true,
            tool_stream: has_tools.then_some(true),
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending streaming request to Z.ai"
        );

        let response = self
            .client
            .post(self.api_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Z.ai streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Z.ai streaming API error ({}): {}",
                status,
                error_body
            ));
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

                if let Some(usage) = json.get("usage") {
                    total_usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
                    total_usage.output_tokens =
                        usage["completion_tokens"].as_u64().unwrap_or(0) as u32;
                    if let Some(details) = usage.get("prompt_tokens_details") {
                        total_usage.cache_read_input_tokens =
                            details["cached_tokens"].as_u64().unwrap_or(0) as u32;
                    }
                }

                let choices = match json["choices"].as_array() {
                    Some(c) if !c.is_empty() => c,
                    _ => continue,
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
                "Z.ai stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "Z.ai streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "Z.ai stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "Z.ai streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "Z.ai stream ended without [DONE] marker but response appears complete"
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
            reasoning_content: None,
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
                    reasoning_content: None,
                }),
                _ => None,
            })
            .collect();
    }

    // Echo reasoning_content back for multi-turn cache hits (clear_thinking: false).
    let reasoning_content: Option<String> = msg.content.iter().find_map(|b| match b {
        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => Some(thinking.clone()),
        _ => None,
    });

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
            reasoning_content,
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
        reasoning_content,
    }]
}

fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let mut content = Vec::new();

    if let Some(ref reasoning) = choice.message.reasoning_content {
        if !reasoning.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: reasoning.clone(),
                signature: None,
            });
        }
    }

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
struct ThinkingConfig {
    r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_thinking: Option<bool>,
}

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_stream: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reasoning_content: Option<String>,
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

    #[test]
    fn default_config() {
        let client = ZaiClient::new("test-key");
        assert_eq!(client.default_model, "glm-5.3");
        assert_eq!(client.default_max_tokens, 65536);
        assert!(!client.use_coding_endpoint);
    }

    #[test]
    fn with_model() {
        let client = ZaiClient::new("test-key").with_model("glm-4.7");
        assert_eq!(client.default_model, "glm-4.7");
    }

    #[test]
    fn with_coding_endpoint() {
        let client = ZaiClient::new("test-key").with_coding_endpoint(true);
        assert_eq!(client.api_url(), ZAI_CODING_API_URL);
    }

    #[test]
    fn thinking_config_mapping() {
        // No budget → no thinking config (use Z.ai default = enabled)
        let client = ZaiClient::new("key");
        assert!(client.build_thinking("glm-5.1").is_none());

        // Budget 0 → thinking disabled
        let client = ZaiClient::new("key").with_thinking_budget(0);
        let cfg = client.build_thinking("glm-5.1").unwrap();
        assert_eq!(cfg.r#type, "disabled");
        assert!(cfg.clear_thinking.is_none());

        // ...except on a model that always thinks, which is sent none.
        assert!(client.build_thinking("glm-5.3").is_none());
        assert!(client.build_thinking("glm-5.3-flash").is_none());

        // Budget > 0 → thinking enabled with clear_thinking=false
        let client = ZaiClient::new("key").with_thinking_budget(5000);
        assert_eq!(client.build_thinking("glm-5.3").unwrap().r#type, "enabled");
        let cfg = client.build_thinking("glm-5.1").unwrap();
        assert_eq!(cfg.r#type, "enabled");
        assert_eq!(cfg.clear_thinking, Some(false));
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
    fn convert_preserves_reasoning_content() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "Let me think...".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "The answer is 42.".to_string(),
                },
            ],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].reasoning_content.as_deref(), Some("Let me think..."));
        assert_eq!(oai[0].content.as_deref(), Some("The answer is 42."));
    }

    #[test]
    fn convert_from_oai_choice_text() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some("Hello world".to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert_eq!(stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn convert_from_oai_choice_with_reasoning() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some("The answer is 42.".to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: Some("Let me think about this...".to_string()),
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 2);
        assert!(
            matches!(&content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think about this...")
        );
        assert!(matches!(&content[1], ContentBlock::Text { text } if text == "The answer is 42."));
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
                reasoning_content: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        };
        let (content, stop_reason) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn glm_system_prefix_is_prepended() {
        let msgs = build_messages(Some("You are a helpful assistant."), &[], "glm-5.1");
        assert_eq!(msgs.len(), 1);
        let content = msgs[0].content.as_ref().unwrap();
        assert!(content.starts_with("You MUST follow these rules STRICTLY:"));
        assert!(content.contains("You are a helpful assistant."));
    }

    #[test]
    fn non_glm_skips_prefix() {
        let msgs = build_messages(
            Some("You are a helpful assistant."),
            &[],
            "some-other-model",
        );
        let content = msgs[0].content.as_ref().unwrap();
        assert!(content.starts_with("You are a helpful assistant."));
    }

    #[test]
    fn request_serializes_thinking_config() {
        let thinking = ThinkingConfig {
            r#type: "enabled".to_string(),
            clear_thinking: Some(false),
        };
        let json = serde_json::to_value(&thinking).unwrap();
        assert_eq!(json["type"], "enabled");
        assert_eq!(json["clear_thinking"], false);
    }

    #[test]
    fn request_serializes_thinking_disabled() {
        let thinking = ThinkingConfig {
            r#type: "disabled".to_string(),
            clear_thinking: None,
        };
        let json = serde_json::to_value(&thinking).unwrap();
        assert_eq!(json["type"], "disabled");
        assert!(json.get("clear_thinking").is_none());
    }
}

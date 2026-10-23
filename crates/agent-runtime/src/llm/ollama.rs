use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Client for the Ollama API.
///
/// Ollama uses the OpenAI-compatible chat completions format, making it
/// suitable for local LLM inference with models like Qwen 2.5 Coder.
/// Used primarily by the Codebase Knowledge Engine for free summary generation.
pub struct OllamaClient {
    client: Client,
    base_url: String,
    default_model: String,
    default_max_tokens: u32,
    api_key: Option<String>,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let url: String = base_url.into();
        let url = if url.is_empty() {
            DEFAULT_OLLAMA_URL.to_string()
        } else {
            url.trim_end_matches('/').to_string()
        };

        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300)) // 5 min for large generations
                .build()
                .unwrap_or_else(|_| Client::new()),
            base_url: url,
            default_model: "qwen2.5-coder:32b".to_string(),
            default_max_tokens: 4096,
            api_key: None,
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

    /// Check if the Ollama server is reachable and the model is available.
    pub async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/api/tags", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let models = body["models"].as_array();
                if let Some(models) = models {
                    let available = models.iter().any(|m| {
                        m["name"]
                            .as_str()
                            .map(|n| n.starts_with(&self.default_model))
                            .unwrap_or(false)
                    });
                    Ok(available)
                } else {
                    Ok(true) // Server up but can't check model list
                }
            }
            Ok(_) => Ok(false),
            Err(_) => Ok(false),
        }
    }

    /// Pull a model if not already available.
    pub async fn ensure_model(&self, model: &str) -> Result<()> {
        let url = format!("{}/api/pull", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "name": model,
                "stream": false
            }))
            .send()
            .await
            .map_err(|e| eyre::eyre!("Failed to pull Ollama model: {}", e))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Failed to pull model '{}': {}", model, body));
        }

        Ok(())
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
        let model = model.unwrap_or(&self.default_model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "ollama",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages);
        let oai_tools = build_tool_defs(tools);

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            stream: false,
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        tracing::debug!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending request to Ollama"
        );

        let response = self
            .authenticated_post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Ollama HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(eyre::eyre!("Ollama API error ({}): {}", status, error_body));
        }

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Ollama response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Ollama response"))?;

        // Convert back to our internal format
        let (content, stop_reason) = convert_from_oai_choice(&choice);

        let usage = api_response
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                ..Default::default()
            })
            .unwrap_or_default();

        tracing::debug!(
            model = %api_response.model.as_deref().unwrap_or(model),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            stop_reason = ?stop_reason,
            "Received response from Ollama"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: api_response.model.unwrap_or_else(|| model.to_string()),
        })
    }

    /// Stream a message response, sending [`StreamEvent`]s to the provided channel.
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
            provider = "ollama",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let oai_messages = build_messages(system_prompt, messages);
        let oai_tools = build_tool_defs(tools);

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(max_tokens),
            tools: oai_tools,
            stream: true,
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = tools.len(),
            "Sending streaming request to Ollama"
        );

        let response = self
            .authenticated_post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Ollama streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Ollama streaming API error ({}): {}",
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
                "Ollama stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "Ollama streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "Ollama stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "Ollama streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "Ollama stream ended without [DONE] marker but response appears complete"
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

fn build_messages(system_prompt: Option<&str>, messages: &[LlmMessage]) -> Vec<OaiMessage> {
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

/// Convert our LlmMessage to OpenAI-compatible format message(s).
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
                    content: Some(OaiContent::String(content.clone())),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                }),
                _ => None,
            })
            .collect();
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
    }]
}

/// Convert OpenAI choice back to our internal format.
fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let mut content = Vec::new();

    // Text content
    if let Some(OaiContent::String(ref text)) = choice.message.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.clone() });
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

// ---- OpenAI-compatible types ----

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_client_default_config() {
        let client = OllamaClient::new("http://localhost:11434");
        assert_eq!(client.base_url, "http://localhost:11434");
        assert_eq!(client.default_model, "qwen2.5-coder:32b");
        assert_eq!(client.default_max_tokens, 4096);
    }

    #[test]
    fn ollama_client_empty_url_uses_default() {
        let client = OllamaClient::new("");
        assert_eq!(client.base_url, DEFAULT_OLLAMA_URL);
    }

    #[test]
    fn ollama_client_with_model() {
        let client = OllamaClient::new("http://ollama:11434").with_model("qwen2.5-coder:7b");
        assert_eq!(client.default_model, "qwen2.5-coder:7b");
    }

    #[test]
    fn ollama_client_strips_trailing_slash() {
        let client = OllamaClient::new("http://ollama:11434/");
        assert_eq!(client.base_url, "http://ollama:11434");
    }

    #[test]
    fn ollama_client_with_api_key() {
        let client = OllamaClient::new("http://ollama:11434").with_api_key("sk-test-bearer-token");
        assert_eq!(client.api_key.as_deref(), Some("sk-test-bearer-token"));
    }

    #[test]
    fn ollama_client_empty_api_key_ignored() {
        let client = OllamaClient::new("http://ollama:11434").with_api_key("");
        assert!(client.api_key.is_none());
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

use std::collections::HashSet;

use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::streaming::StreamEvent;
use super::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};

const GROK_API_URL: &str = "https://api.x.ai/v1/chat/completions";
const GROK_MODELS_URL: &str = "https://api.x.ai/v1/models";

/// Maximum number of tools to send to Grok before triggering two-phase selection.
/// Grok's constrained decoding grammar becomes too complex above this threshold.
const GROK_TOOL_THRESHOLD: usize = 40;

/// Maximum number of tools to select in phase 1.
const GROK_MAX_SELECTED_TOOLS: usize = 25;

/// Max tokens for the phase 1 tool selection call.
const TOOL_SELECTION_MAX_TOKENS: u32 = 1024;

/// Fallback: if phase 1 fails, send the first N tools alphabetically.
const GROK_FALLBACK_TOOL_COUNT: usize = 20;

/// Tool name prefixes that are always sent to Grok.
/// Everything else is discoverable via two-phase selection.
///
/// NOTE: Tool names arrive here already sanitized by `to_llm_name()`
/// which replaces dots with underscores (e.g. `filesystem.read` → `filesystem_read`).
/// Prefixes must use underscores, not dots.
const ESSENTIAL_PREFIXES: &[&str] = &[
    "filesystem_",
    "exec_",
    "code_read",
    "code_edit",
    "code_search",
    "code_git",
    "web_",
    "http_",
    "messaging_send",
    "messaging_read",
    "memory",
    "notes",
    "vault_",
    "system_",
    "settings",
    "reactions_",
    "workflow",
    "database_",
];

/// The runner's tools for loading and calling catalogued tools.
const LOADING_TOOLS: [&str; 2] = ["tool_load", "tool_call"];

/// Client for the xAI Grok API.
///
/// Grok uses the OpenAI chat completions format and provides
/// frontier-class inference with full tool calling support.
pub struct GrokClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
}

impl GrokClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            default_model: "grok-4.7".to_string(),
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

    /// Auth-only credential probe. Hits `GET /v1/models` — validates
    /// the API key without invoking a model.
    pub async fn verify_credentials(&self) -> Result<()> {
        let resp = self
            .client
            .get(GROK_MODELS_URL)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Grok verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Grok verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    /// Send a message and get a response.
    ///
    /// When the tool count exceeds [`GROK_TOOL_THRESHOLD`], a two-tier
    /// filtering strategy is applied:
    ///
    /// 1. **Essential filter** — only tools matching [`ESSENTIAL_PREFIXES`] are
    ///    kept. If this brings the count under the threshold, we're done.
    /// 2. **Two-phase selection** — if still over the threshold, a lightweight
    ///    pre-flight call (with no tool definitions) asks Grok to pick relevant
    ///    tools from a text catalogue.
    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<LlmResponse> {
        let model = model.unwrap_or(&self.default_model);
        let model = model.strip_prefix("grok:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "grok",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let effective_tools = self
            .filter_tools(system_prompt, messages, tools, model)
            .await;
        let oai_messages = build_messages(system_prompt, messages);
        let oai_tools = build_tool_defs(&effective_tools);

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
            stream: false,
        };

        tracing::debug!(
            model = model,
            messages = messages.len(),
            tools = effective_tools.len(),
            original_tools = tools.len(),
            "Sending request to Grok"
        );

        let response = self
            .client
            .post(GROK_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(eyre::eyre!("Grok API error ({}): {}", status, error_body));
        }

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Grok response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Grok response"))?;

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
            "Received response from Grok"
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
        let model = model.strip_prefix("grok:").unwrap_or(model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "grok",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let effective_tools = self
            .filter_tools(system_prompt, messages, tools, model)
            .await;
        let oai_messages = build_messages(system_prompt, messages);
        let oai_tools = build_tool_defs(&effective_tools);

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
            stream: true,
        };

        tracing::info!(
            model = model,
            messages = messages.len(),
            tools = effective_tools.len(),
            original_tools = tools.len(),
            "Sending streaming request to Grok"
        );

        let response = self
            .client
            .post(GROK_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Grok streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Grok streaming API error ({}): {}",
                status,
                error_body
            ));
        }

        // ── Parse SSE stream ─────────────────────────────────────────────
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
                "Grok stream ended without [DONE] marker and produced no content or tool calls — treating as server error"
            );
            return Err(eyre::eyre!(
                "Grok streaming API error (503): stream ended prematurely with no content (no [DONE] marker, no finish_reason)"
            ));
        }

        if !got_done && !got_finish_reason {
            tracing::warn!(
                has_content = has_content,
                has_tool_calls = has_tool_calls,
                "Grok stream ended without [DONE] marker or finish_reason — response likely truncated"
            );
            return Err(eyre::eyre!(
                "Grok streaming API error (503): stream terminated without finish_reason (likely truncated)"
            ));
        }

        if !got_done {
            tracing::debug!(
                "Grok stream ended without [DONE] marker but response appears complete"
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

    /// Phase 1: Ask Grok (with no tool definitions) which tools are relevant.
    ///
    /// Sends a lightweight request containing only the tool catalogue (names +
    /// descriptions) and the conversation context. Returns a list of tool names
    /// the model considers relevant for the current turn.
    async fn select_tools_phase1(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: &str,
    ) -> Result<Vec<String>> {
        let tool_catalogue: String = tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n");

        let selection_prompt = format!(
            "You are a tool routing assistant. Given the conversation below and the list of \
             available tools, output ONLY the names of tools that are likely needed to handle \
             the user's current request. Output one tool name per line, exactly as shown in \
             the list. Select at most {} tools. Do not add explanations.\n\n\
             ## Available tools\n{}\n\n\
             ## Instructions\n\
             - Read the conversation context (especially the last user message).\n\
             - Pick the tools the assistant would realistically call.\n\
             - Always include foundational tools like filesystem_read, filesystem_write, \
               filesystem_edit, filesystem_list, exec_run if they might be needed.\n\
             - Output ONLY tool names, one per line.",
            GROK_MAX_SELECTED_TOOLS, tool_catalogue,
        );

        let mut oai_messages: Vec<OaiMessage> = Vec::new();

        oai_messages.push(OaiMessage {
            role: "system".to_string(),
            content: Some(OaiContent::String(selection_prompt)),
            tool_calls: None,
            tool_call_id: None,
        });

        // Include truncated system prompt for role context
        if let Some(sys) = system_prompt {
            let truncated = if sys.len() > 500 {
                // Find a char boundary at or before byte 500
                let end = sys
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i <= 500)
                    .last()
                    .unwrap_or(0);
                format!("{}...", &sys[..end])
            } else {
                sys.to_string()
            };
            oai_messages.push(OaiMessage {
                role: "user".to_string(),
                content: Some(OaiContent::String(format!(
                    "[Agent system prompt summary]: {}",
                    truncated,
                ))),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Include last few conversation messages (text only) for context
        let context_messages: Vec<&LlmMessage> = messages
            .iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        for msg in &context_messages {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let text: String = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                oai_messages.push(OaiMessage {
                    role: role.to_string(),
                    content: Some(OaiContent::String(text)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }

        let request = OaiRequest {
            model: model.to_string(),
            messages: oai_messages,
            max_tokens: Some(TOOL_SELECTION_MAX_TOKENS),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            stream: false,
        };

        tracing::debug!(
            model = model,
            total_tools = tools.len(),
            "Grok phase 1: selecting relevant tools"
        );

        let response = self
            .client
            .post(GROK_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Phase 1 HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(eyre::eyre!(
                "Grok phase 1 API error ({}): {}",
                status,
                error_body
            ));
        }

        let api_response: OaiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Grok phase 1 response: {}", e))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("No choices in Grok phase 1 response"))?;

        let text = match choice.message.content {
            Some(OaiContent::String(ref t)) => t.clone(),
            _ => String::new(),
        };

        if let Some(usage) = api_response.usage {
            tracing::debug!(
                input_tokens = usage.prompt_tokens,
                output_tokens = usage.completion_tokens,
                "Grok phase 1 token usage"
            );
        }

        Ok(parse_selected_tool_names(&text))
    }

    /// Two-tier tool filtering: essential prefixes first, then phase 1 LLM selection.
    async fn filter_tools(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: &str,
    ) -> Vec<LlmToolDef> {
        if tools.len() <= GROK_TOOL_THRESHOLD {
            return tools.to_vec();
        }

        // A selection keeps the runner's loading tools, the one way to reach
        // whatever it left out.
        let with_loading = |selected: Vec<LlmToolDef>| -> Vec<LlmToolDef> {
            tools
                .iter()
                .filter(|t| {
                    LOADING_TOOLS.contains(&t.name.as_str())
                        || selected.iter().any(|s| s.name == t.name)
                })
                .cloned()
                .collect()
        };

        let essential: Vec<LlmToolDef> = tools
            .iter()
            .filter(|t| is_essential_tool(&t.name))
            .cloned()
            .collect();

        if essential.len() <= GROK_TOOL_THRESHOLD {
            tracing::info!(
                total = tools.len(),
                essential = essential.len(),
                "Grok tool filter: using essential subset"
            );
            return essential;
        }

        tracing::info!(
            total = tools.len(),
            essential = essential.len(),
            "Grok tool filter: essential set exceeds threshold, running phase 1"
        );

        let all_tool_names: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();

        match self
            .select_tools_phase1(system_prompt, messages, tools, model)
            .await
        {
            Ok(selected_names) => {
                let valid_names: Vec<&str> = selected_names
                    .iter()
                    .filter(|n| all_tool_names.contains(n.as_str()))
                    .map(|n| n.as_str())
                    .collect();

                if valid_names.is_empty() {
                    tracing::warn!("Phase 1 returned no valid tool names — using fallback");
                    with_loading(fallback_tool_selection(tools, GROK_FALLBACK_TOOL_COUNT))
                } else {
                    tracing::info!(
                        selected = valid_names.len(),
                        names = ?valid_names,
                        "Phase 1 selected tools"
                    );
                    with_loading(
                        tools
                            .iter()
                            .filter(|t| valid_names.contains(&t.name.as_str()))
                            .cloned()
                            .collect(),
                    )
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Phase 1 tool selection failed — using fallback");
                with_loading(fallback_tool_selection(tools, GROK_FALLBACK_TOOL_COUNT))
            }
        }
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

// ─── Tool selection helpers ────────────────────────────────────────────────

/// Check if a tool name matches any essential prefix.
fn is_essential_tool(name: &str) -> bool {
    LOADING_TOOLS.contains(&name)
        || ESSENTIAL_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Parse tool names from the phase 1 response.
///
/// Handles multiple formats: one per line, JSON array, comma-separated,
/// with bullets/numbers/backticks stripped.
fn parse_selected_tool_names(text: &str) -> Vec<String> {
    let trimmed = text.trim();

    // Try JSON array first: ["tool_a", "tool_b"]
    if trimmed.starts_with('[') {
        if let Ok(names) = serde_json::from_str::<Vec<String>>(trimmed) {
            return names
                .into_iter()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
        }
    }

    // Otherwise, split by newlines and/or commas
    trimmed
        .lines()
        .flat_map(|line| line.split(','))
        .map(|entry| {
            entry
                .trim()
                .trim_start_matches(|c: char| {
                    c == '-' || c == '*' || c == '.' || c.is_ascii_digit()
                })
                .trim()
                .trim_matches('`')
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty() && !name.contains(' ') && name.len() <= 100)
        .collect()
}

/// Fallback tool selection: alphabetically first N tools.
///
/// Alphabetical ordering is deterministic and tends to include foundational
/// tools (exec, filesystem) which sort early.
fn fallback_tool_selection(tools: &[LlmToolDef], count: usize) -> Vec<LlmToolDef> {
    let mut sorted: Vec<&LlmToolDef> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted.into_iter().take(count).cloned().collect()
}

// ─── OpenAI-compatible types ───

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

    // ─── is_essential_tool ────────────────────────────────────────────

    #[test]
    fn essential_tool_matches_prefix() {
        // Tool names arrive sanitized: dots → underscores (via to_llm_name)
        assert!(is_essential_tool("filesystem_read"));
        assert!(is_essential_tool("filesystem_write"));
        assert!(is_essential_tool("exec_run"));
        assert!(is_essential_tool("code_read"));
        assert!(is_essential_tool("code_edit"));
        assert!(is_essential_tool("code_search"));
        assert!(is_essential_tool("code_git"));
        assert!(is_essential_tool("web_fetch"));
        assert!(is_essential_tool("settings"));
        assert!(is_essential_tool("web_search"));
        assert!(is_essential_tool("messaging_send"));
        assert!(is_essential_tool("messaging_read"));
        assert!(is_essential_tool("messaging_read_thread"));
        assert!(is_essential_tool("memory"));
        assert!(is_essential_tool("memory_search"));
        assert!(is_essential_tool("vault_get"));
        assert!(is_essential_tool("vault_set"));
        assert!(is_essential_tool("system_enable_skill"));
        assert!(is_essential_tool("system_list_skills"));
        assert!(is_essential_tool("system_llm_status"));
        assert!(is_essential_tool("system_set_model"));
        assert!(is_essential_tool("system_list_tools"));
        assert!(is_essential_tool("reactions_add"));
        assert!(is_essential_tool("workflow"));
        assert!(is_essential_tool("tool_load"));
        assert!(is_essential_tool("tool_call"));
    }

    #[test]
    fn non_essential_tool_rejected() {
        assert!(!is_essential_tool("code_lint"));
        assert!(!is_essential_tool("code_build"));
        assert!(!is_essential_tool("code_test"));
        assert!(!is_essential_tool("messaging_pin"));
        assert!(!is_essential_tool("sentinel_scan"));
        assert!(!is_essential_tool("stats_my_usage"));
        assert!(!is_essential_tool("github_list_repos"));
        assert!(!is_essential_tool("linear_issue_create"));
    }

    // ─── parse_selected_tool_names ────────────────────────────────────

    #[test]
    fn parse_simple_lines() {
        let text = "filesystem_read\nfilesystem_write\nexec_run";
        let names = parse_selected_tool_names(text);
        assert_eq!(
            names,
            vec!["filesystem_read", "filesystem_write", "exec_run"]
        );
    }

    #[test]
    fn parse_with_bullets() {
        let text = "- filesystem_read\n- filesystem_write\n- exec_run";
        let names = parse_selected_tool_names(text);
        assert_eq!(
            names,
            vec!["filesystem_read", "filesystem_write", "exec_run"]
        );
    }

    #[test]
    fn parse_with_numbers() {
        let text = "1. filesystem_read\n2. filesystem_write\n3. exec_run";
        let names = parse_selected_tool_names(text);
        assert_eq!(
            names,
            vec!["filesystem_read", "filesystem_write", "exec_run"]
        );
    }

    #[test]
    fn parse_json_array() {
        let text = r#"["filesystem_read", "filesystem_write", "exec_run"]"#;
        let names = parse_selected_tool_names(text);
        assert_eq!(
            names,
            vec!["filesystem_read", "filesystem_write", "exec_run"]
        );
    }

    #[test]
    fn parse_comma_separated() {
        let text = "filesystem_read, filesystem_write, exec_run";
        let names = parse_selected_tool_names(text);
        assert_eq!(
            names,
            vec!["filesystem_read", "filesystem_write", "exec_run"]
        );
    }

    #[test]
    fn parse_with_backticks() {
        let text = "`filesystem_read`\n`filesystem_write`";
        let names = parse_selected_tool_names(text);
        assert_eq!(names, vec!["filesystem_read", "filesystem_write"]);
    }

    #[test]
    fn parse_empty_input() {
        let names = parse_selected_tool_names("");
        assert!(names.is_empty());
    }

    #[test]
    fn parse_filters_sentences() {
        let text = "Here are the tools you need:\nfilesystem_read\nexec_run";
        let names = parse_selected_tool_names(text);
        // Sentences contain spaces and are filtered out
        assert_eq!(names, vec!["filesystem_read", "exec_run"]);
    }

    // ─── fallback_tool_selection ──────────────────────────────────────

    #[test]
    fn fallback_returns_alphabetical_subset() {
        let tools = vec![
            LlmToolDef {
                name: "z_tool".into(),
                description: "Z".into(),
                input_schema: serde_json::json!({}),
            },
            LlmToolDef {
                name: "a_tool".into(),
                description: "A".into(),
                input_schema: serde_json::json!({}),
            },
            LlmToolDef {
                name: "m_tool".into(),
                description: "M".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let selected = fallback_tool_selection(&tools, 2);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].name, "a_tool");
        assert_eq!(selected[1].name, "m_tool");
    }

    #[test]
    fn fallback_handles_count_exceeding_tools() {
        let tools = vec![LlmToolDef {
            name: "only_tool".into(),
            description: "Only".into(),
            input_schema: serde_json::json!({}),
        }];
        let selected = fallback_tool_selection(&tools, 10);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "only_tool");
    }
}

//! Shared OpenAI Chat-Completions types and pure conversion helpers.
//!
//! Both `openrouter` and `bedrock_openai` speak the OpenAI Chat-Completions
//! body shape, so the request/response structs and the conversions between
//! our internal `LlmMessage` / `ContentBlock` / `LlmToolDef` types and OAI's
//! wire types live here once.
//!
//! Transport — HTTP+SSE for OpenRouter, SigV4 + Bedrock eventstream frames for
//! Bedrock — stays in each provider module.

use serde::{Deserialize, Serialize};

use super::tool_schema::sanitize_tool_schema;
use super::types::{ContentBlock, LlmMessage, LlmToolDef, Role, StopReason};

/// OpenAI Chat-Completions request body.
///
/// `model` and `stream` are optional because Bedrock's `/invoke` endpoint
/// carries the model id in the URL and infers streaming from the endpoint
/// suffix; OpenRouter requires both in the body.
#[derive(Debug, Serialize)]
pub struct OaiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OaiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ReasoningConfig {
    pub effort: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OaiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OaiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OaiContent {
    String(String),
    Blocks(Vec<serde_json::Value>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OaiTool {
    pub r#type: String,
    pub function: OaiFunction,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OaiFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OaiToolCall {
    pub id: String,
    pub r#type: String,
    pub function: OaiFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OaiFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct OaiResponse {
    pub choices: Vec<OaiChoice>,
    pub model: Option<String>,
    pub usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
pub struct OaiChoice {
    pub message: OaiMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OaiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<OaiPromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub struct OaiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

/// Build a `reasoning.effort` config from a thinking-budget token count.
/// None when the caller didn't ask for extended reasoning.
pub fn build_reasoning_config(thinking_budget_tokens: Option<u32>) -> Option<ReasoningConfig> {
    thinking_budget_tokens.map(|budget| ReasoningConfig {
        effort: budget_to_effort(budget).to_string(),
    })
}

/// Map a thinking-budget token count to the OpenAI `reasoning.effort` label.
pub fn budget_to_effort(budget: u32) -> &'static str {
    match budget {
        0 => "none",
        1..=2000 => "low",
        2001..=8000 => "medium",
        _ => "high",
    }
}

/// Whether the model supports OAI-style `cache_control` blocks in messages.
pub fn supports_cache_control(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("claude")
        || m.starts_with("anthropic/")
        || m.contains("gemini")
        || m.starts_with("google/")
}

/// Build the OAI `messages` array from our internal message list, optionally
/// folding a system prompt into a leading system message. A Claude model
/// (reached through a router) keeps to the Messages API's image limit.
pub fn build_messages(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    enable_cache: bool,
    model: &str,
) -> Vec<OaiMessage> {
    let messages: &[LlmMessage] = &if model.to_lowercase().contains("claude") {
        super::image_cap::cap_images(
            std::borrow::Cow::Borrowed(messages),
            super::image_cap::ANTHROPIC_MAX_IMAGES,
        )
    } else {
        std::borrow::Cow::Borrowed(messages)
    };
    let mut oai_messages: Vec<OaiMessage> = Vec::new();

    let caching = enable_cache && supports_cache_control(model);
    if let Some(system) = system_prompt {
        let content = if caching {
            OaiContent::Blocks(super::cache_utils::build_cache_blocks(system))
        } else {
            // Non-caching wire: the cache-break markers are internal plumbing
            // — strip them instead of shipping literal HTML comments.
            OaiContent::String(system.replace(super::anthropic::CACHE_BREAK_MARKER, "\n\n"))
        };
        oai_messages.push(OaiMessage {
            role: "system".to_string(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    let prefix_len =
        super::cache_utils::cache_breakpoint_message(caching, messages.len()).map_or(0, |i| i + 1);
    let (prefix, rest) = messages.split_at(prefix_len);
    oai_messages.extend(prefix.iter().flat_map(convert_to_oai_message));
    let cut = oai_messages.len();
    oai_messages.extend(rest.iter().flat_map(convert_to_oai_message));

    if caching {
        apply_message_cache_breakpoint(&mut oai_messages[..cut]);
    }

    oai_messages
}

/// Place one `cache_control: ephemeral` on the last user message of the
/// slice so the accumulated conversation prefix caches across rounds. Without
/// it the OAI wire caches only the system message and re-bills the whole
/// history — including tens of rounds of tool results — at the full input
/// rate every round. The caller bounds the slice at the round's breakpoint
/// message ([`super::cache_utils::cache_breakpoint_message`]), so on round 1
/// of a turn with history the breakpoint closes the replayed prefix rather
/// than the task message.
///
/// User-role only, deliberately: multipart user content is universally
/// accepted, whereas `tool`/`assistant` part arrays are not, and a 400 here
/// forces failover to a weaker model. The breakpoint therefore usually pins
/// at a user message; within-turn increments after it stay uncached, which
/// is the small tail of the prompt.
fn apply_message_cache_breakpoint(messages: &mut [OaiMessage]) {
    for msg in messages.iter_mut().rev() {
        if msg.role != "user" {
            continue;
        }
        match msg.content.as_mut() {
            Some(OaiContent::String(text)) if !text.trim().is_empty() => {
                let block = serde_json::json!({
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" },
                });
                msg.content = Some(OaiContent::Blocks(vec![block]));
                return;
            }
            Some(OaiContent::Blocks(blocks)) => {
                if let Some(block) = blocks
                    .iter_mut()
                    .rev()
                    .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                {
                    block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Build the OAI `tools` field from our internal tool defs. `None` when the
/// list is empty so the field is omitted from the request entirely.
pub fn build_tool_defs(tools: &[LlmToolDef]) -> Option<Vec<OaiTool>> {
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
                        // OpenRouter forwards Anthropic-target requests to the
                        // Anthropic API, which rejects top-level
                        // `oneOf`/`allOf`/`anyOf` in a tool's input schema. The
                        // native-Anthropic and Bedrock paths already sanitize;
                        // do the same here so the OAI/OpenRouter wire path can't
                        // 400 every call and force failover to a weaker model.
                        parameters: sanitize_tool_schema(t.input_schema.clone()),
                    },
                })
                .collect(),
        )
    }
}

/// Map a content block to its OAI multipart-content part, or `None` for blocks
/// that have no multipart representation (thinking, tool use/result).
fn content_part(block: &ContentBlock) -> Option<serde_json::Value> {
    match block {
        ContentBlock::Text { text } => Some(serde_json::json!({ "type": "text", "text": text })),
        ContentBlock::Image { source } | ContentBlock::Document { source } => {
            Some(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": source.data_url() },
            }))
        }
        _ => None,
    }
}

/// Convert one of our `LlmMessage`s into one or more OAI messages.
///
/// Tool-result blocks become standalone `role: "tool"` messages (OAI splits
/// each result out, while Anthropic groups them under one user message). Tool
/// uses on an assistant message ride as a `tool_calls` array. Messages carrying
/// an image or document block serialize as a multipart content array so the
/// attachment reaches vision-capable models instead of being dropped.
pub fn convert_to_oai_message(msg: &LlmMessage) -> Vec<OaiMessage> {
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
        let extra: Vec<serde_json::Value> = msg.content.iter().filter_map(content_part).collect();
        if !extra.is_empty() {
            out.push(OaiMessage {
                role: "user".to_string(),
                content: Some(OaiContent::Blocks(extra)),
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

    let has_media = msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Image { .. } | ContentBlock::Document { .. }
        )
    });
    if has_media {
        return vec![OaiMessage {
            role: role.to_string(),
            content: Some(OaiContent::Blocks(
                msg.content.iter().filter_map(content_part).collect(),
            )),
            tool_calls: None,
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

/// Pull the assistant content blocks and stop reason out of an OAI choice.
pub fn convert_from_oai_choice(choice: &OaiChoice) -> (Vec<ContentBlock>, StopReason) {
    let text_block = match choice.message.content.as_ref() {
        Some(OaiContent::String(s)) if !s.is_empty() => {
            Some(ContentBlock::Text { text: s.clone() })
        }
        _ => None,
    };

    let tool_calls = choice.message.tool_calls.as_deref().unwrap_or(&[]);
    let tool_blocks = tool_calls.iter().map(|tc| {
        let input = serde_json::from_str(&tc.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        ContentBlock::tool_use(tc.id.clone(), tc.function.name.clone(), input)
    });

    let content: Vec<ContentBlock> = text_block.into_iter().chain(tool_blocks).collect();
    let has_tool_calls = !tool_calls.is_empty();

    let stop_reason = match choice.finish_reason.as_deref() {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("stop") => StopReason::EndTurn,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };

    (content, stop_reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ContentBlock, ImageSource, LlmMessage, Role};
    use serde_json::json;

    fn text_msg(role: Role, text: &str) -> LlmMessage {
        LlmMessage {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> LlmMessage {
        LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: None,
                loaded: None,
            }],
        }
    }

    #[test]
    fn message_breakpoint_lands_on_last_user_message() {
        let messages = vec![
            text_msg(Role::User, "task"),
            text_msg(Role::Assistant, "reply"),
        ];
        let out = build_messages(Some("sys"), &messages, true, "anthropic/claude-sonnet-5");
        // [system, user, assistant] — breakpoint on the user message, which
        // converts from a plain string to a one-text-part block array.
        let user = &out[1];
        assert_eq!(user.role, "user");
        let OaiContent::Blocks(blocks) = user.content.as_ref().unwrap() else {
            panic!("user content should have become blocks");
        };
        assert_eq!(blocks[0]["cache_control"], json!({ "type": "ephemeral" }));
        let assistant = &out[2];
        assert!(matches!(
            assistant.content.as_ref().unwrap(),
            OaiContent::String(_)
        ));
    }

    #[test]
    fn message_breakpoint_skips_trailing_tool_messages() {
        let messages = vec![
            text_msg(Role::User, "task"),
            tool_result_msg("t1", "output"),
        ];
        let out = build_messages(Some("sys"), &messages, true, "anthropic/claude-sonnet-5");
        // [system, user(task), tool] — the tool message keeps its plain string
        // content; the breakpoint pins on the task message.
        let tool = out.iter().find(|m| m.role == "tool").unwrap();
        assert!(matches!(
            tool.content.as_ref().unwrap(),
            OaiContent::String(_)
        ));
        let user = out.iter().find(|m| m.role == "user").unwrap();
        let OaiContent::Blocks(blocks) = user.content.as_ref().unwrap() else {
            panic!("task message should carry the breakpoint");
        };
        assert_eq!(blocks[0]["cache_control"], json!({ "type": "ephemeral" }));
    }

    #[test]
    fn message_breakpoint_absent_for_non_caching_model() {
        let messages = vec![text_msg(Role::User, "task")];
        let out = build_messages(Some("sys"), &messages, true, "deepseek/deepseek-v3");
        assert!(matches!(
            out[1].content.as_ref().unwrap(),
            OaiContent::String(_)
        ));
    }

    #[test]
    fn message_breakpoint_absent_when_cache_disabled() {
        let messages = vec![text_msg(Role::User, "task")];
        let out = build_messages(Some("sys"), &messages, false, "anthropic/claude-sonnet-5");
        assert!(matches!(
            out[1].content.as_ref().unwrap(),
            OaiContent::String(_)
        ));
    }

    #[test]
    fn build_tool_defs_strips_top_level_constraint_keys() {
        // OpenRouter forwards Anthropic-target requests to Anthropic, which
        // 400s on top-level oneOf/allOf/anyOf. build_tool_defs must sanitize.
        let tools = vec![LlmToolDef {
            name: "gmail_search".to_string(),
            description: "search".to_string(),
            input_schema: json!({
                "type": "object",
                "anyOf": [{ "required": ["query"] }, { "required": ["thread_id"] }],
                "properties": { "query": { "type": "string" } },
            }),
        }];
        let defs = build_tool_defs(&tools).expect("tools present");
        let params = &defs[0].function.parameters;
        assert!(params.get("anyOf").is_none());
        assert!(params.get("oneOf").is_none());
        assert!(params.get("allOf").is_none());
        assert_eq!(params["properties"]["query"]["type"], "string");
    }

    #[test]
    fn build_tool_defs_keeps_nested_constraint_keys() {
        let nested = json!({
            "type": "object",
            "properties": {
                "operation": { "anyOf": [{ "type": "string" }, { "type": "integer" }] },
            },
        });
        let tools = vec![LlmToolDef {
            name: "segment_edit".to_string(),
            description: "edit".to_string(),
            input_schema: nested.clone(),
        }];
        let defs = build_tool_defs(&tools).expect("tools present");
        assert_eq!(defs[0].function.parameters, nested);
    }

    #[test]
    fn convert_user_text_message() {
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
    fn convert_tool_result_message_splits_per_result() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "ok1".to_string(),
                    is_error: None,
                    loaded: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".to_string(),
                    content: "ok2".to_string(),
                    is_error: None,
                    loaded: None,
                },
            ],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 2);
        assert!(oai.iter().all(|m| m.role == "tool"));
    }

    #[test]
    fn convert_tool_result_with_image_emits_trailing_user_message() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
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
        assert!(
            matches!(oai[1].content, Some(OaiContent::Blocks(ref b)) if b.len() == 1),
            "image rides as a multipart user message"
        );
    }

    #[test]
    fn convert_tool_use_message_carries_tool_calls() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tc_1".to_string(),
                name: "read".to_string(),
                input: json!({"p": "/tmp"}),
                signature: None,
            }],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert!(oai[0].tool_calls.is_some());
    }

    #[test]
    fn convert_from_oai_choice_text_only() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: Some(OaiContent::String("hi".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("stop".to_string()),
        };
        let (content, stop) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert_eq!(stop, StopReason::EndTurn);
    }

    #[test]
    fn convert_from_oai_choice_tool_calls_yield_tool_use_stop() {
        let choice = OaiChoice {
            message: OaiMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![OaiToolCall {
                    id: "c1".to_string(),
                    r#type: "function".to_string(),
                    function: OaiFunctionCall {
                        name: "read".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        };
        let (content, stop) = convert_from_oai_choice(&choice);
        assert_eq!(content.len(), 1);
        assert_eq!(stop, StopReason::ToolUse);
    }

    #[test]
    fn cache_control_supported_models() {
        assert!(supports_cache_control("claude-sonnet-4-6"));
        assert!(supports_cache_control("anthropic/claude-sonnet-4.6"));
        assert!(supports_cache_control("anthropic/claude-haiku-4.6"));
        assert!(supports_cache_control("claude-opus-4-7"));
        assert!(supports_cache_control("gemini-2.5-pro"));
        assert!(supports_cache_control("google/gemini-2.5-pro"));
        assert!(supports_cache_control("google/gemini-2.0-flash"));
        assert!(!supports_cache_control("deepseek/deepseek-v3.2"));
        assert!(!supports_cache_control("zai.glm-5"));
        assert!(!supports_cache_control("grok-4"));
        assert!(!supports_cache_control("meta-llama/llama-3.3-70b"));
    }

    #[test]
    fn build_tool_defs_empty_returns_none() {
        assert!(build_tool_defs(&[]).is_none());
    }

    #[test]
    fn build_messages_no_cache_for_unsupported_model() {
        let msgs = build_messages(Some("System prompt"), &[], true, "deepseek/deepseek-v3.2");
        assert_eq!(msgs.len(), 1);
        match &msgs[0].content {
            Some(OaiContent::String(s)) => assert_eq!(s, "System prompt"),
            _ => panic!("expected plain string for model without cache support"),
        }
    }

    #[test]
    fn build_messages_cache_for_claude() {
        let msgs = build_messages(
            Some("System prompt"),
            &[],
            true,
            "anthropic/claude-sonnet-4.6",
        );
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0].content, Some(OaiContent::Blocks(_))));
    }

    #[test]
    fn build_messages_keeps_a_routed_claude_within_the_image_limit() {
        let image = ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            },
        };
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![image; 24],
        }];
        let images = |model: &str| {
            serde_json::to_string(&build_messages(None, &messages, false, model))
                .unwrap()
                .matches(r#""type":"image_url""#)
                .count()
        };
        assert_eq!(images("anthropic/claude-sonnet-5"), 20);
        assert_eq!(images("openai/gpt-5.5"), 24);
    }

    #[test]
    fn build_messages_cache_for_gemini() {
        let msgs = build_messages(Some("System prompt"), &[], true, "google/gemini-2.5-pro");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0].content, Some(OaiContent::Blocks(_))));
    }

    #[test]
    fn build_messages_no_cache_when_disabled() {
        let msgs = build_messages(
            Some("System prompt"),
            &[],
            false,
            "anthropic/claude-sonnet-4.6",
        );
        assert_eq!(msgs.len(), 1);
        match &msgs[0].content {
            Some(OaiContent::String(s)) => assert_eq!(s, "System prompt"),
            _ => panic!("expected plain string when caching disabled"),
        }
    }

    #[test]
    fn build_messages_cache_blocks_split_on_marker() {
        use super::super::anthropic::CACHE_BREAK_MARKER;

        let prompt = format!("Static{CACHE_BREAK_MARKER}Semi");
        let msgs = build_messages(Some(&prompt), &[], true, "claude-sonnet-4-6");
        match &msgs[0].content {
            Some(OaiContent::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert!(blocks[0].get("cache_control").is_some());
                assert!(blocks[1].get("cache_control").is_some());
            }
            _ => panic!("expected blocks"),
        }
    }

    #[tokio::test]
    async fn message_breakpoint_stays_inside_the_declared_prefix() {
        let messages = [
            LlmMessage::user("q1"),
            LlmMessage::assistant("a1"),
            LlmMessage::user("q2"),
        ];
        let msgs = crate::llm::provider::with_cached_prefix(Some(2), async {
            build_messages(Some("sys"), &messages, true, "claude-sonnet-4-6")
        })
        .await;
        assert_eq!(msgs.len(), 4);
        let has_breakpoint = |m: &OaiMessage| match &m.content {
            Some(OaiContent::Blocks(blocks)) => {
                blocks.iter().any(|b| b.get("cache_control").is_some())
            }
            _ => false,
        };
        assert!(has_breakpoint(&msgs[1]), "q1 closes the replayed prefix");
        assert!(!has_breakpoint(&msgs[2]));
        assert!(
            matches!(&msgs[3].content, Some(OaiContent::String(s)) if s == "q2"),
            "the task message stays a plain string"
        );
    }

    #[test]
    fn build_reasoning_config_levels() {
        assert!(build_reasoning_config(None).is_none());
        assert_eq!(build_reasoning_config(Some(0)).unwrap().effort, "none");
        assert_eq!(build_reasoning_config(Some(1500)).unwrap().effort, "low");
        assert_eq!(build_reasoning_config(Some(5000)).unwrap().effort, "medium");
        assert_eq!(build_reasoning_config(Some(20000)).unwrap().effort, "high");
    }

    #[test]
    fn budget_to_effort_levels() {
        assert_eq!(budget_to_effort(0), "none");
        assert_eq!(budget_to_effort(1500), "low");
        assert_eq!(budget_to_effort(5000), "medium");
        assert_eq!(budget_to_effort(20000), "high");
    }

    #[test]
    fn oai_request_omits_optional_fields() {
        let req = OaiRequest {
            model: None,
            messages: vec![],
            max_tokens: None,
            tools: None,
            reasoning: None,
            stream: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("model").is_none());
        assert!(json.get("stream").is_none());
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("reasoning").is_none());
    }

    fn content_array(msg: &OaiMessage) -> &Vec<serde_json::Value> {
        match msg.content.as_ref() {
            Some(OaiContent::Blocks(parts)) => parts,
            other => panic!("expected multipart Blocks content, got {other:?}"),
        }
    }

    #[test]
    fn convert_image_message_emits_multipart_image_url() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::image_base64(
                "image/png",
                "abc123".to_string(),
            )],
        };
        let oai = convert_to_oai_message(&msg);
        assert_eq!(oai.len(), 1);
        assert_eq!(oai[0].role, "user");
        let parts = content_array(&oai[0]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,abc123");
    }

    #[test]
    fn convert_text_then_image_preserves_order() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "look at this".to_string(),
                },
                ContentBlock::image_base64("image/jpeg", "zzz".to_string()),
            ],
        };
        let parts = {
            let oai = convert_to_oai_message(&msg);
            content_array(&oai[0]).clone()
        };
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "look at this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,zzz");
    }

    #[test]
    fn convert_document_message_emits_image_url() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Document {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: "application/pdf".to_string(),
                    data: "pdfdata".to_string(),
                },
            }],
        };
        let oai = convert_to_oai_message(&msg);
        let parts = content_array(&oai[0]);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0]["image_url"]["url"],
            "data:application/pdf;base64,pdfdata"
        );
    }

    #[test]
    fn convert_text_only_message_stays_string() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "plain".to_string(),
            }],
        };
        let oai = convert_to_oai_message(&msg);
        assert!(matches!(
            oai[0].content.as_ref(),
            Some(OaiContent::String(s)) if s == "plain"
        ));
    }
}

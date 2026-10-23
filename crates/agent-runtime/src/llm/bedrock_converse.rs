//! Bedrock unified Converse API.
//!
//! Wire format intentionally differs from Anthropic Messages:
//! - System is an array of `{"text": "..."}` blocks plus optional `{"cachePoint": ...}`.
//! - Tools are wrapped: `{"toolSpec": {name, description, inputSchema: {json: ...}}}`.
//! - Tool results carry an explicit `status: "success" | "error"`.
//! - Reasoning surfaces as `reasoningContent` blocks both in responses and
//!   stream events.
//! - Terminal stream events are split between `messageStop` (carries
//!   `stopReason`) and `metadata` (carries `usage`). The streaming loop in
//!   [`super::bedrock`] holds a small state machine to combine the two.

use eyre::Result;
use serde_json::{json, Value};

use super::anthropic::{cache_segments, segment_is_cached};
use super::bedrock_capabilities::{self, ThinkingMode, ToggleShape};
use super::streaming::StreamEvent;
use super::tool_schema::sanitize_tool_schema;
use super::types::{
    sanitize_tool_input, ContentBlock, LlmMessage, LlmToolDef, Role, StopReason, TokenUsage,
};

/// Build the JSON body for a Bedrock Converse call.
pub fn build_body(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
    model_id: &str,
    enable_cache: bool,
    thinking_budget: Option<u32>,
) -> Value {
    let cache = enable_cache && bedrock_capabilities::caches(model_id);

    // A cachePoint closing the breakpoint message caches the conversation
    // prefix up to it — without one only [system + tools] cache and every
    // agentic round re-bills the full history at the uncached input rate.
    // Round 1 of a turn with history closes the replayed prefix instead of
    // the task message, so the next turn can read it.
    let mut converse_messages = convert_messages(&super::image_cap::anthropic_shaped(messages));
    if let Some(content) =
        super::cache_utils::cache_breakpoint_message(cache, converse_messages.len())
            .and_then(|i| converse_messages[i].get_mut("content"))
            .and_then(Value::as_array_mut)
    {
        content.push(json!({ "cachePoint": { "type": "default" } }));
    }
    let mut body = json!({
        "messages": converse_messages,
        "inferenceConfig": { "maxTokens": max_tokens },
    });

    if let Some(prompt) = system_prompt {
        body["system"] = Value::Array(build_system_blocks(prompt, cache));
    }

    if !tools.is_empty() {
        let mut tool_specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": { "json": sanitize_tool_schema(t.input_schema.clone()) },
                    }
                })
            })
            .collect();
        if cache {
            tool_specs.push(json!({ "cachePoint": { "type": "default" } }));
        }
        body["toolConfig"] = json!({ "tools": tool_specs });
    }

    if let Some(field) = thinking_request_field(model_id, thinking_budget) {
        body["additionalModelRequestFields"] = field;
    }

    body
}

fn build_system_blocks(prompt: &str, cache: bool) -> Vec<Value> {
    cache_segments(prompt)
        .into_iter()
        .enumerate()
        .flat_map(|(i, text)| {
            std::iter::once(json!({ "text": text })).chain(
                (cache && segment_is_cached(i))
                    .then(|| json!({ "cachePoint": { "type": "default" } })),
            )
        })
        .collect()
}

// Anthropic models (the only `ToggleWithBudget` family, including adaptive-only
// Opus 4.7) always use the Invoke wire, so they never reach this builder;
// adaptive thinking is handled in `super::bedrock_invoke`.
fn thinking_request_field(model_id: &str, budget: Option<u32>) -> Option<Value> {
    let mode = bedrock_capabilities::thinking(model_id);
    match (mode, budget) {
        (ThinkingMode::ToggleWithBudget, Some(b)) => Some(json!({
            "thinking": { "type": "enabled", "budget_tokens": b }
        })),
        (ThinkingMode::Toggle, Some(b)) if b > 0 => {
            match bedrock_capabilities::toggle_shape(model_id) {
                Some(ToggleShape::AnthropicLike) => Some(json!({
                    "thinking": { "type": "enabled" }
                })),
                Some(ToggleShape::EnableThinkingBool) => Some(json!({
                    "enable_thinking": true
                })),
                None => None,
            }
        }
        _ => None,
    }
}

fn convert_messages(messages: &[LlmMessage]) -> Vec<Value> {
    messages.iter().map(convert_message).collect()
}

fn convert_message(m: &LlmMessage) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = m.content.iter().filter_map(convert_block).collect();
    json!({ "role": role, "content": content })
}

fn convert_block(b: &ContentBlock) -> Option<Value> {
    match b {
        ContentBlock::Text { text } => Some(json!({ "text": text })),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            let mut inner = json!({ "text": thinking });
            if let Some(sig) = signature {
                inner["signature"] = json!(sig);
            }
            Some(json!({ "reasoningContent": { "reasoningText": inner } }))
        }
        ContentBlock::ToolUse {
            id, name, input, ..
        } => Some(json!({
            "toolUse": {
                "toolUseId": id,
                "name": name,
                "input": sanitize_tool_input(input.clone()),
            }
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let status = if is_error.unwrap_or(false) {
                "error"
            } else {
                "success"
            };
            Some(json!({
                "toolResult": {
                    "toolUseId": tool_use_id,
                    "content": [{ "text": content }],
                    "status": status,
                }
            }))
        }
        ContentBlock::Image { source } => {
            if source.source_type != "base64" {
                return None;
            }
            let format = source
                .media_type
                .strip_prefix("image/")
                .unwrap_or(&source.media_type);
            Some(json!({
                "image": {
                    "format": format,
                    "source": { "bytes": source.data },
                }
            }))
        }
        ContentBlock::Document { source } => {
            if source.source_type != "base64" {
                return None;
            }
            let format = bedrock_document_format(&source.media_type)?;
            Some(json!({
                "document": {
                    "format": format,
                    "name": bedrock_document_name(&source.data),
                    "source": { "bytes": source.data },
                }
            }))
        }
    }
}

/// Map a MIME type to the Bedrock Converse `document.format` enum. Returns
/// `None` for unsupported types so the document is dropped rather than
/// triggering a 400 from Bedrock on an unknown format.
fn bedrock_document_format(media_type: &str) -> Option<&'static str> {
    Some(match media_type {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return None,
    })
}

/// Bedrock requires each `document.name` in a message to be unique and to
/// match `^[a-zA-Z0-9](?:[a-zA-Z0-9\s\-\(\)\[\]]*[a-zA-Z0-9])?$` (no dots).
/// A content-derived suffix gives both: deterministic uniqueness across
/// distinct payloads and only the allowed character class.
fn bedrock_document_name(base64_data: &str) -> String {
    let suffix: String = base64_data
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if suffix.is_empty() {
        "attachment".to_string()
    } else {
        format!("attachment-{suffix}")
    }
}

/// Parse the JSON response body from a `/converse` call into the unified shape.
pub fn parse_response(
    value: Value,
    model: &str,
) -> Result<(Vec<ContentBlock>, StopReason, TokenUsage, String)> {
    let message = &value["output"]["message"];
    let content = parse_content_array(&message["content"]);
    let stop_reason = parse_stop_reason(value["stopReason"].as_str());
    let usage = parse_usage(&value["usage"]);
    Ok((content, stop_reason, usage, model.to_string()))
}

fn parse_content_array(array: &Value) -> Vec<ContentBlock> {
    array
        .as_array()
        .map(|blocks| blocks.iter().filter_map(parse_content_block).collect())
        .unwrap_or_default()
}

fn parse_content_block(block: &Value) -> Option<ContentBlock> {
    if let Some(text) = block["text"].as_str() {
        return Some(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    if let Some(rc) = block["reasoningContent"].as_object() {
        let rt = rc.get("reasoningText")?;
        let text = rt["text"].as_str().unwrap_or("").to_string();
        let signature = rt["signature"].as_str().map(str::to_string);
        return Some(ContentBlock::Thinking {
            thinking: text,
            signature,
        });
    }
    if let Some(tu) = block["toolUse"].as_object() {
        let id = tu.get("toolUseId")?.as_str()?.to_string();
        let name = tu.get("name")?.as_str()?.to_string();
        let input = tu.get("input").cloned().unwrap_or(Value::Null);
        return Some(ContentBlock::tool_use(id, name, input));
    }
    None
}

fn parse_stop_reason(s: Option<&str>) -> StopReason {
    match s {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

fn parse_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: value["inputTokens"].as_u64().unwrap_or(0) as u32,
        output_tokens: value["outputTokens"].as_u64().unwrap_or(0) as u32,
        cache_creation_input_tokens: value["cacheWriteInputTokens"].as_u64().unwrap_or(0) as u32,
        cache_read_input_tokens: value["cacheReadInputTokens"].as_u64().unwrap_or(0) as u32,
    }
}

/// Streaming state for Converse: holds the `stopReason` reported by the
/// `messageStop` frame so the terminal `MessageDone` event can be assembled
/// when the trailing `metadata` frame arrives with usage.
#[derive(Default)]
pub struct ConverseStreamState {
    pub pending_stop: Option<StopReason>,
}

impl ConverseStreamState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Map one Converse streaming event payload (already decoded from a Bedrock
/// event-stream frame) to zero or one [`StreamEvent`]. Returns `Err` for
/// service-side exceptions; the caller is expected to surface those as errors.
pub fn map_event(
    event_type: &str,
    payload: &Value,
    state: &mut ConverseStreamState,
) -> Result<Option<StreamEvent>> {
    match event_type {
        "messageStart" => Ok(None),
        "contentBlockStart" => {
            let index = payload["contentBlockIndex"].as_u64().unwrap_or(0) as usize;
            if let Some(tu) = payload["start"]["toolUse"].as_object() {
                let id = tu
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = tu
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Ok(Some(StreamEvent::ToolUseStart {
                    index,
                    id,
                    name,
                    signature: None,
                }))
            } else {
                Ok(Some(StreamEvent::ContentStart { index }))
            }
        }
        "contentBlockDelta" => {
            let index = payload["contentBlockIndex"].as_u64().unwrap_or(0) as usize;
            let delta = &payload["delta"];
            if let Some(text) = delta["text"].as_str() {
                return Ok(Some(StreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                }));
            }
            if let Some(partial) = delta["toolUse"]["input"].as_str() {
                return Ok(Some(StreamEvent::InputJsonDelta {
                    index,
                    partial: partial.to_string(),
                }));
            }
            if let Some(rc) = delta["reasoningContent"].as_object() {
                if let Some(text) = rc.get("text").and_then(Value::as_str) {
                    return Ok(Some(StreamEvent::ThinkingDelta {
                        index,
                        thinking: text.to_string(),
                    }));
                }
                if let Some(sig) = rc.get("signature").and_then(Value::as_str) {
                    return Ok(Some(StreamEvent::SignatureDelta {
                        index,
                        signature: sig.to_string(),
                    }));
                }
            }
            Ok(None)
        }
        "contentBlockStop" => {
            let index = payload["contentBlockIndex"].as_u64().unwrap_or(0) as usize;
            Ok(Some(StreamEvent::ContentStop { index }))
        }
        "messageStop" => {
            state.pending_stop = Some(parse_stop_reason(payload["stopReason"].as_str()));
            Ok(None)
        }
        "metadata" => {
            let usage = parse_usage(&payload["usage"]);
            Ok(Some(StreamEvent::MessageDone {
                stop_reason: state.pending_stop.take().unwrap_or(StopReason::EndTurn),
                usage,
            }))
        }
        _ => Ok(None),
    }
}

/// Build a `MessageDone` for a stream that ended after `messageStop` without
/// a trailing `metadata` frame.
pub fn flush_terminal(state: &mut ConverseStreamState) -> Option<StreamEvent> {
    state
        .pending_stop
        .take()
        .map(|stop_reason| StreamEvent::MessageDone {
            stop_reason,
            usage: TokenUsage::default(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::anthropic::CACHE_BREAK_MARKER;
    use crate::llm::types::Role;

    fn user(text: &str) -> LlmMessage {
        LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn tool(name: &str) -> LlmToolDef {
        LlmToolDef {
            name: name.to_string(),
            description: "x".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn body_omits_model_field_and_includes_inference_config() {
        let body = build_body(
            Some("sys"),
            &[user("hi")],
            &[],
            4096,
            "deepseek.v3.2",
            false,
            None,
        );
        assert!(body.get("model").is_none());
        assert_eq!(body["inferenceConfig"]["maxTokens"].as_u64(), Some(4096));
    }

    #[test]
    fn body_cache_point_emitted_for_cache_supporting_model() {
        let prompt = format!("static{CACHE_BREAK_MARKER}dynamic");
        let body = build_body(
            Some(&prompt),
            &[user("hi")],
            &[],
            1024,
            "amazon.nova-pro-v1:0",
            true,
            None,
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 4);
        assert!(system[0].get("text").is_some());
        assert!(system[1].get("cachePoint").is_some());
        assert_eq!(system[2]["text"].as_str(), Some("dynamic"));
        assert!(system[3].get("cachePoint").is_some());
    }

    #[tokio::test]
    async fn message_cache_point_lands_on_the_declared_prefix_end() {
        let messages = [user("q1"), LlmMessage::assistant("a1"), user("q2")];
        let body = crate::llm::provider::with_cached_prefix(Some(2), async {
            build_body(
                Some("sys"),
                &messages,
                &[],
                1024,
                "amazon.nova-pro-v1:0",
                true,
                None,
            )
        })
        .await;
        let content = |i: usize| body["messages"][i]["content"].as_array().unwrap().clone();
        assert!(content(0).iter().all(|b| b.get("cachePoint").is_none()));
        assert!(content(1).last().unwrap().get("cachePoint").is_some());
        assert!(content(2).iter().all(|b| b.get("cachePoint").is_none()));
    }

    #[test]
    fn body_cache_points_after_first_two_of_three_segments() {
        let prompt = format!("static{CACHE_BREAK_MARKER}semi{CACHE_BREAK_MARKER}dynamic");
        let body = build_body(
            Some(&prompt),
            &[user("hi")],
            &[],
            1024,
            "amazon.nova-pro-v1:0",
            true,
            None,
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 5);
        assert!(system[1].get("cachePoint").is_some());
        assert!(system[3].get("cachePoint").is_some());
        assert_eq!(system[4]["text"].as_str(), Some("dynamic"));
    }

    #[test]
    fn body_no_cache_point_for_non_caching_model() {
        let body = build_body(
            Some("static"),
            &[user("hi")],
            &[],
            1024,
            "meta.llama4-maverick-17b-instruct-v1:0",
            true,
            None,
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert!(system[0].get("cachePoint").is_none());
    }

    #[test]
    fn body_no_cache_point_when_caching_disabled() {
        let body = build_body(
            Some("static"),
            &[user("hi")],
            &[],
            1024,
            "amazon.nova-pro-v1:0",
            false,
            None,
        );
        let system = body["system"].as_array().unwrap();
        assert!(system.iter().all(|b| b.get("cachePoint").is_none()));
    }

    #[test]
    fn body_strips_constraint_keys_from_tool_schemas() {
        let t = LlmToolDef {
            name: "a".to_string(),
            description: "x".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "v": { "type": "string" } },
                "oneOf": [{ "required": ["v"] }],
            }),
        };
        let body = build_body(
            None,
            &[user("hi")],
            &[t],
            1024,
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            false,
            None,
        );
        let schema = &body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"];
        assert!(schema.get("oneOf").is_none());
        assert!(schema.get("allOf").is_none());
        assert!(schema.get("anyOf").is_none());
        assert_eq!(schema["properties"]["v"]["type"], json!("string"));
    }

    #[test]
    fn body_keeps_nested_constraint_keys_in_tool_schemas() {
        let nested = json!({
            "type": "object",
            "properties": {
                "operation": { "anyOf": [{ "type": "string" }, { "type": "integer" }] },
            },
        });
        let t = LlmToolDef {
            name: "a".to_string(),
            description: "x".to_string(),
            input_schema: nested.clone(),
        };
        let body = build_body(
            None,
            &[user("hi")],
            &[t],
            1024,
            "qwen.qwen3-coder-next",
            false,
            None,
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
            nested
        );
    }

    #[test]
    fn body_tools_use_tool_spec_wrapper() {
        let body = build_body(
            None,
            &[user("hi")],
            &[tool("a"), tool("b")],
            1024,
            "amazon.nova-pro-v1:0",
            true,
            None,
        );
        let tools = body["toolConfig"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["toolSpec"]["name"].as_str(), Some("a"));
        assert_eq!(
            tools[0]["toolSpec"]["inputSchema"]["json"]["type"].as_str(),
            Some("object")
        );
        assert!(tools.last().unwrap().get("cachePoint").is_some());
        // The conversation breakpoint: a cachePoint closes the last message.
        let last_content = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(last_content.last().unwrap().get("cachePoint").is_some());
    }

    #[test]
    fn body_messages_no_cache_point_for_non_caching_model() {
        let body = build_body(
            None,
            &[user("hi")],
            &[],
            1024,
            "mistral.mistral-large-2402-v1:0",
            true,
            None,
        );
        let last_content = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(last_content.iter().all(|b| b.get("cachePoint").is_none()));
    }

    #[test]
    fn body_tools_no_cache_point_for_non_caching_model() {
        let body = build_body(
            None,
            &[user("hi")],
            &[tool("a")],
            1024,
            "meta.llama4-maverick-17b-instruct-v1:0",
            true,
            None,
        );
        let tools = body["toolConfig"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].get("cachePoint").is_none());
    }

    #[test]
    fn body_thinking_toggle_with_budget_for_anthropic_like_deepseek() {
        let body = build_body(
            None,
            &[user("hi")],
            &[],
            1024,
            "deepseek.v3.2",
            false,
            Some(8000),
        );
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"].as_str(),
            Some("enabled")
        );
    }

    #[test]
    fn body_thinking_enable_bool_for_qwen_235b() {
        let body = build_body(
            None,
            &[user("hi")],
            &[],
            1024,
            "qwen.qwen3-235b-a22b-2507-v1:0",
            false,
            Some(8000),
        );
        assert_eq!(
            body["additionalModelRequestFields"]["enable_thinking"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn body_thinking_field_omitted_for_always_on_models() {
        let body = build_body(
            None,
            &[user("hi")],
            &[],
            1024,
            "deepseek.deepseek-r1-v1:0",
            false,
            Some(8000),
        );
        assert!(body.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn body_thinking_field_omitted_for_unsupported() {
        let body = build_body(
            None,
            &[user("hi")],
            &[],
            1024,
            "meta.llama4-maverick-17b-instruct-v1:0",
            false,
            Some(8000),
        );
        assert!(body.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn tool_result_carries_status_field() {
        let msg = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu1".to_string(),
                content: "oops".to_string(),
                is_error: Some(true),
                loaded: None,
            }],
        };
        let body = build_body(None, &[msg], &[], 1024, "deepseek.v3.2", false, None);
        let content = &body["messages"][0]["content"][0];
        assert_eq!(content["toolResult"]["status"].as_str(), Some("error"));
        assert_eq!(content["toolResult"]["toolUseId"].as_str(), Some("tu1"));
        assert_eq!(
            content["toolResult"]["content"][0]["text"].as_str(),
            Some("oops")
        );
    }

    #[test]
    fn parse_response_text_and_tool_use() {
        let resp = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "hi" },
                { "toolUse": { "toolUseId": "tu1", "name": "f", "input": {"k":"v"} } }
            ]}},
            "stopReason": "tool_use",
            "usage": { "inputTokens": 10, "outputTokens": 4,
                       "cacheReadInputTokens": 3, "cacheWriteInputTokens": 0 }
        });
        let (content, stop, usage, model) = parse_response(resp, "deepseek.v3.2").unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(model, "deepseek.v3.2");
    }

    #[test]
    fn parse_response_reasoning_block() {
        let resp = json!({
            "output": { "message": { "content": [
                { "reasoningContent": { "reasoningText": { "text": "thinking...", "signature": "sig" }}},
                { "text": "answer" }
            ]}},
            "stopReason": "end_turn",
            "usage": { "inputTokens": 1, "outputTokens": 1 }
        });
        let (content, _, _, _) = parse_response(resp, "x").unwrap();
        assert!(matches!(
            &content[0],
            ContentBlock::Thinking { thinking, signature }
                if thinking == "thinking..." && signature.as_deref() == Some("sig")
        ));
    }

    #[test]
    fn map_event_text_delta() {
        let mut state = ConverseStreamState::new();
        let payload = json!({"contentBlockIndex": 0, "delta": {"text": "abc"}});
        let evt = map_event("contentBlockDelta", &payload, &mut state).unwrap();
        assert!(matches!(
            evt,
            Some(StreamEvent::TextDelta { index: 0, ref text }) if text == "abc"
        ));
    }

    #[test]
    fn map_event_tool_use_start_then_input_delta() {
        let mut state = ConverseStreamState::new();
        let start = json!({
            "contentBlockIndex": 1,
            "start": { "toolUse": { "toolUseId": "tu1", "name": "f" }}
        });
        let evt = map_event("contentBlockStart", &start, &mut state).unwrap();
        assert!(matches!(
            evt,
            Some(StreamEvent::ToolUseStart { index: 1, ref id, ref name, .. })
                if id == "tu1" && name == "f"
        ));
        let delta = json!({
            "contentBlockIndex": 1,
            "delta": { "toolUse": { "input": "{\"k\":" }}
        });
        let evt = map_event("contentBlockDelta", &delta, &mut state).unwrap();
        assert!(matches!(
            evt,
            Some(StreamEvent::InputJsonDelta { index: 1, ref partial }) if partial == "{\"k\":"
        ));
    }

    #[test]
    fn map_event_reasoning_delta() {
        let mut state = ConverseStreamState::new();
        let payload = json!({
            "contentBlockIndex": 0,
            "delta": { "reasoningContent": { "text": "thinking" }}
        });
        let evt = map_event("contentBlockDelta", &payload, &mut state).unwrap();
        assert!(matches!(
            evt,
            Some(StreamEvent::ThinkingDelta { ref thinking, .. }) if thinking == "thinking"
        ));
    }

    #[test]
    fn message_stop_buffers_until_metadata() {
        let mut state = ConverseStreamState::new();
        let stop = json!({"stopReason": "tool_use"});
        let evt = map_event("messageStop", &stop, &mut state).unwrap();
        assert!(evt.is_none());
        assert!(state.pending_stop.is_some());
        let meta = json!({"usage": {"inputTokens": 1, "outputTokens": 2}});
        let evt = map_event("metadata", &meta, &mut state).unwrap();
        assert!(matches!(
            evt,
            Some(StreamEvent::MessageDone { stop_reason: StopReason::ToolUse, ref usage })
                if usage.input_tokens == 1 && usage.output_tokens == 2
        ));
    }

    #[test]
    fn flush_terminal_emits_when_metadata_missing() {
        let mut state = ConverseStreamState::new();
        let stop = json!({"stopReason": "end_turn"});
        map_event("messageStop", &stop, &mut state).unwrap();
        let evt = flush_terminal(&mut state);
        assert!(matches!(
            evt,
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));
        assert!(state.pending_stop.is_none());
    }

    #[test]
    fn flush_terminal_none_when_no_pending_stop() {
        let mut state = ConverseStreamState::new();
        assert!(flush_terminal(&mut state).is_none());
    }

    fn document_block(media_type: &str, data: &str) -> ContentBlock {
        ContentBlock::Document {
            source: crate::llm::types::ImageSource {
                source_type: "base64".to_string(),
                media_type: media_type.to_string(),
                data: data.to_string(),
            },
        }
    }

    #[test]
    fn document_pdf_emits_bedrock_document_block() {
        let block = convert_block(&document_block("application/pdf", "JVBERi0xLjQK")).unwrap();
        assert_eq!(block["document"]["format"], "pdf");
        assert_eq!(block["document"]["source"]["bytes"], "JVBERi0xLjQK");
        let name = block["document"]["name"].as_str().unwrap();
        assert!(name.starts_with("attachment-"));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn document_office_mime_types_map_to_bedrock_formats() {
        for (mime, expected) in [
            ("text/csv", "csv"),
            ("application/msword", "doc"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx",
            ),
            ("application/vnd.ms-excel", "xls"),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xlsx",
            ),
            ("text/html", "html"),
            ("text/plain", "txt"),
            ("text/markdown", "md"),
        ] {
            let block = convert_block(&document_block(mime, "QUJD")).unwrap();
            assert_eq!(block["document"]["format"], expected, "mime {mime}");
        }
    }

    #[test]
    fn document_unknown_mime_returns_none() {
        assert!(convert_block(&document_block("application/x-unknown", "QUJD")).is_none());
    }

    #[test]
    fn document_non_base64_source_returns_none() {
        let block = ContentBlock::Document {
            source: crate::llm::types::ImageSource {
                source_type: "url".to_string(),
                media_type: "application/pdf".to_string(),
                data: "https://example.com/x.pdf".to_string(),
            },
        };
        assert!(convert_block(&block).is_none());
    }

    #[test]
    fn document_names_differ_for_distinct_payloads() {
        let a = convert_block(&document_block("application/pdf", "AAAABBBBCCCCDDDD")).unwrap();
        let b = convert_block(&document_block("application/pdf", "ZZZZYYYYXXXXWWWW")).unwrap();
        assert_ne!(a["document"]["name"], b["document"]["name"]);
    }
}

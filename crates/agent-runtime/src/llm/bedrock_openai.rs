//! Bedrock OpenAI Chat-Completions wire path.
//!
//! Used by marketplace-hosted models that expose their native OpenAI API
//! over Bedrock's `/invoke` endpoint rather than Converse. Only Z.AI GLM is
//! routed here today — confirmed via direct CLI probe returning a standard
//! `{"choices":[…],"object":"chat.completion","usage":{…}}` body. Other
//! families are added as their wire format is confirmed against AWS.
//!
//! The request body is plain OAI Chat-Completions JSON with two omissions:
//! `model` is carried in the URL (not the body) and `stream` is implied by
//! the endpoint suffix (`/invoke` vs `/invoke-with-response-stream`).
//!
//! Streaming responses arrive as Bedrock eventstream frames; each frame's
//! payload is one OAI chunk in any of three shapes:
//!   - `{"bytes":"<base64>"}` envelope wrapping naked JSON
//!   - `{"bytes":"<base64>"}` envelope wrapping SSE `data:` events
//!   - the JSON or SSE bytes inline with no envelope
//!
//! `decode_oai_frame` absorbs all three.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use super::bedrock_eventstream::FrameDecoder;
use super::oai_common::{
    build_messages, build_tool_defs, convert_from_oai_choice, OaiRequest, OaiResponse,
};
use super::sse::event_data;
use super::streaming::StreamEvent;
use super::types::{ContentBlock, LlmMessage, LlmToolDef, StopReason, TokenUsage};

/// Build the JSON body for a Bedrock OAI `/invoke` call.
///
/// Three parameters are intentionally inert:
/// - `enable_cache`: Bedrock's OAI marketplace models don't support the OAI
///   `cache_control` extension, so cache blocks are never emitted. The
///   matching `bedrock_capabilities::caches()` returns false for every
///   OAI-routed family.
/// - `thinking_budget`: z.ai's native thinking control is not the OpenAI
///   `reasoning.effort` field, and no Bedrock OAI family has been
///   confirmed to honour any thinking-control field yet. Emitting an
///   unverified field risks a 400 from Bedrock, so we drop it.
/// - `_model_id`: model is carried in the URL, not the body.
///
/// Caller compatibility is preserved by keeping the parameters.
#[allow(clippy::too_many_arguments)]
pub fn build_body(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
    _model_id: &str,
    _enable_cache: bool,
    _thinking_budget: Option<u32>,
) -> Value {
    let oai_messages = build_messages(system_prompt, messages, false, "");
    let oai_tools = build_tool_defs(tools);
    let request = OaiRequest {
        model: None,
        messages: oai_messages,
        max_tokens: Some(max_tokens),
        tools: oai_tools,
        reasoning: None,
        stream: None,
    };
    serde_json::to_value(&request).unwrap_or(Value::Null)
}

/// Parse a non-streaming Bedrock OAI response into the runtime shape.
pub fn parse_response(
    value: Value,
    model: &str,
) -> Result<(Vec<ContentBlock>, StopReason, TokenUsage, String)> {
    let resp: OaiResponse = serde_json::from_value(value)
        .map_err(|e| eyre!("Bedrock OAI response parse failed: {e}"))?;

    let resolved_model = resp.model.unwrap_or_else(|| model.to_string());
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("Bedrock OAI response had empty choices array"))?;
    let (content, stop_reason) = convert_from_oai_choice(&choice);

    let usage = resp
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

    Ok((content, stop_reason, usage, resolved_model))
}

/// Run the streaming loop for a Bedrock OAI invocation, emitting events into
/// `tx` until the message is done: at its `finish_reason` once its usage is
/// known, else when the stream ends.
pub async fn run_openai_stream<S, E>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<()>
where
    S: futures::Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut driver = OaiStreamDriver::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| eyre!("Bedrock stream read failed: {e}"))?;
        decoder.feed(&chunk);
        while let Some(frame) = decoder.next_frame()? {
            if frame.message_type == "exception" {
                return Err(eyre!(
                    "Bedrock streaming exception ({}): {}",
                    frame.exception_type,
                    String::from_utf8_lossy(&frame.payload)
                ));
            }
            for chunk_json in decode_oai_frame(&frame.payload)? {
                for event in driver.consume_chunk(&chunk_json)? {
                    let done = matches!(event, StreamEvent::MessageDone { .. });
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                    if done {
                        return Ok(());
                    }
                }
            }
        }
    }

    match driver.finish() {
        Some(done) => {
            let _ = tx.send(done).await;
            Ok(())
        }
        None => Err(driver.truncation_error()),
    }
}

/// Extract zero or more OAI chunk-JSON values from a single Bedrock frame
/// payload. Each Bedrock InvokeWithResponseStream chunk is either:
///   - `{"bytes":"<base64>"}` — the documented envelope. The decoded bytes
///     are the model's native streaming chunk, which for OAI marketplace
///     models is either naked JSON or one-or-more SSE `data:` events.
///   - inline JSON — no envelope.
///   - raw SSE bytes — no envelope, model's SSE flows through verbatim.
///
/// Returns an empty vec for `[DONE]` markers or whitespace-only frames.
fn decode_oai_frame(payload: &[u8]) -> Result<Vec<Value>> {
    match serde_json::from_slice::<Value>(payload) {
        Ok(outer) => match outer.get("bytes").and_then(Value::as_str) {
            Some(b64) => {
                let decoded = BASE64
                    .decode(b64)
                    .map_err(|e| eyre!("Bedrock OAI frame base64 decode failed: {e}"))?;
                parse_json_or_sse(&decoded)
            }
            None => Ok(vec![outer]),
        },
        Err(_) => parse_json_or_sse(payload),
    }
}

/// Parse a byte slice as either naked JSON or an SSE event stream. SSE blocks
/// are split on `\n\n`, `data:` lines are joined, `[DONE]` terminators and
/// whitespace-only events are skipped.
fn parse_json_or_sse(bytes: &[u8]) -> Result<Vec<Value>> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        return Ok(vec![value]);
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| eyre!("Bedrock OAI chunk is neither JSON nor UTF-8: {e}"))?;
    text.split("\n\n")
        .filter_map(parse_sse_event)
        .collect::<Result<Vec<_>>>()
}

/// The JSON payload of one SSE event block, or `None` for empty events and
/// `[DONE]` terminators.
fn parse_sse_event(block: &str) -> Option<Result<Value>> {
    let data = event_data(block)?;
    Some(
        serde_json::from_str(&data)
            .map_err(|e| eyre!("Bedrock OAI SSE chunk JSON parse failed: {e}; chunk={data}")),
    )
}

/// State machine that turns a sequence of OAI chunk JSON values into our
/// internal `StreamEvent` sequence.
///
/// OAI streaming interleaves text deltas and tool-call deltas in a single
/// `choices[].delta` object across many chunks. We track the current "block
/// index" — alternating between the assistant's text block and each tool
/// call slot — so the downstream collector reassembles a coherent message.
struct OaiStreamDriver {
    content_started: bool,
    content_index: usize,
    has_content: bool,
    has_tool_calls: bool,
    final_stop_reason: StopReason,
    total_usage: TokenUsage,
    /// `finish_reason` came before any usage; `MessageDone` waits for it.
    done_pending: bool,
}

impl OaiStreamDriver {
    fn new() -> Self {
        Self {
            content_started: false,
            content_index: 0,
            has_content: false,
            has_tool_calls: false,
            final_stop_reason: StopReason::EndTurn,
            total_usage: TokenUsage::default(),
            done_pending: false,
        }
    }

    fn usage_known(&self) -> bool {
        let u = &self.total_usage;
        u.input_tokens + u.output_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens
            > 0
    }

    fn message_done(&mut self) -> StreamEvent {
        self.done_pending = false;
        StreamEvent::MessageDone {
            stop_reason: self.final_stop_reason.clone(),
            usage: self.total_usage.clone(),
        }
    }

    /// At the end of the stream: the `MessageDone` still waiting for usage,
    /// which then stays as it is.
    fn finish(&mut self) -> Option<StreamEvent> {
        self.done_pending.then(|| self.message_done())
    }

    /// Translate one OAI chunk into zero or more StreamEvents. The terminal
    /// `MessageDone` goes out at `finish_reason` when the usage is known.
    /// Otherwise it waits: OpenAI-style streams send usage in a chunk after
    /// `finish_reason`, and Bedrock adds its invocation metrics to the last
    /// chunk.
    fn consume_chunk(&mut self, chunk: &Value) -> Result<Vec<StreamEvent>> {
        let mut events = Vec::new();

        if let Some(metrics) = chunk.get("amazon-bedrock-invocationMetrics") {
            if !self.usage_known() {
                let count =
                    |key: &str| metrics.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
                self.total_usage = TokenUsage {
                    input_tokens: count("inputTokenCount"),
                    output_tokens: count("outputTokenCount"),
                    cache_read_input_tokens: count("cacheReadInputTokenCount"),
                    cache_creation_input_tokens: count("cacheWriteInputTokenCount"),
                };
            }
        }

        if let Some(usage) = chunk.get("usage") {
            let details = usage.get("prompt_tokens_details");
            self.total_usage = TokenUsage::from_oai(
                usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                details
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                details
                    .and_then(|d| d.get("cache_write_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
            );
        }

        if self.done_pending {
            if self.usage_known() {
                events.push(self.message_done());
            }
            return Ok(events);
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return Ok(events);
        };

        for choice in choices {
            let delta = match choice.get("delta") {
                Some(d) => d,
                None => continue,
            };

            if let Some(content) = delta.get("content").and_then(Value::as_str) {
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

            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let tc_index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let idx = tc_index + 1;
                    let Some(func) = tc.get("function") else {
                        continue;
                    };
                    if let Some(name) = func.get("name").and_then(Value::as_str) {
                        self.has_tool_calls = true;
                        if self.content_started {
                            self.content_started = false;
                            events.push(StreamEvent::ContentStop {
                                index: self.content_index,
                            });
                            self.content_index += 1;
                        }
                        let id = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        events.push(StreamEvent::ToolUseStart {
                            index: idx,
                            id,
                            name: name.to_string(),
                            signature: None,
                        });
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        if !args.is_empty() {
                            events.push(StreamEvent::InputJsonDelta {
                                index: idx,
                                partial: args.to_string(),
                            });
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.final_stop_reason = match reason {
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
                // Always emit a ContentStop on the final block so the
                // downstream StreamCollector flushes any open text or
                // tool-use state before MessageDone fires. Tool-call-only
                // responses don't have a matching ContentStart, but the
                // collector accepts that.
                events.push(StreamEvent::ContentStop {
                    index: self.content_index,
                });
                self.content_index += 1;
                self.content_started = false;
                self.done_pending = true;
                if self.usage_known() {
                    events.push(self.message_done());
                }
                return Ok(events);
            }
        }

        Ok(events)
    }

    fn truncation_error(&self) -> eyre::Report {
        if self.has_content || self.has_tool_calls {
            eyre!("Bedrock OAI streaming truncated: stream ended after content but before finish_reason")
        } else {
            eyre!("Bedrock OAI streaming ended without finish_reason or content")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ContentBlock, LlmMessage, Role};
    use serde_json::json;

    #[test]
    fn build_body_omits_model_and_stream() {
        let body = build_body(
            Some("sys"),
            &[LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }],
            &[],
            128,
            "zai.glm-5",
            true,
            None,
        );
        assert!(body.get("model").is_none());
        assert!(body.get("stream").is_none());
        assert_eq!(body["max_tokens"].as_u64(), Some(128));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"].as_str(), Some("system"));
        assert!(matches!(&msgs[0]["content"], Value::String(_)));
        assert_eq!(msgs[1]["role"].as_str(), Some("user"));
    }

    #[test]
    fn build_body_disables_cache_blocks() {
        let body = build_body(
            Some("static\n<!--CACHE_BREAK-->\ndynamic"),
            &[],
            &[],
            128,
            "zai.glm-5",
            true,
            None,
        );
        let sys = &body["messages"][0]["content"];
        assert!(matches!(sys, Value::String(_)), "expected plain string");
    }

    #[test]
    fn build_body_omits_reasoning_until_thinking_field_is_verified() {
        let body = build_body(
            None,
            &[LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "x".to_string(),
                }],
            }],
            &[],
            128,
            "zai.glm-5",
            false,
            Some(5000),
        );
        assert!(
            body.get("reasoning").is_none(),
            "reasoning is opt-in once a Bedrock OAI thinking field is confirmed; until then we omit it"
        );
    }

    #[test]
    fn parse_response_text_choice() {
        let resp = json!({
            "id": "chatcmpl-x",
            "object": "chat.completion",
            "model": "zai.glm-5",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        let (content, stop, usage, model) = parse_response(resp, "zai.glm-5").unwrap();
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "hi there"));
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(model, "zai.glm-5");
    }

    #[test]
    fn parse_response_length_finish_maps_to_max_tokens() {
        let resp = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 8}
        });
        let (_, stop, _, _) = parse_response(resp, "zai.glm-5").unwrap();
        assert_eq!(stop, StopReason::MaxTokens);
    }

    #[test]
    fn parse_response_tool_call() {
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "do_thing", "arguments": "{\"a\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let (content, stop, _, _) = parse_response(resp, "zai.glm-5").unwrap();
        assert_eq!(stop, StopReason::ToolUse);
        assert!(content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "do_thing")));
    }

    #[test]
    fn parse_response_caches_propagate_from_oai_usage() {
        let resp = json!({
            "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "prompt_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 20}
            }
        });
        let (_, _, usage, _) = parse_response(resp, "zai.glm-5").unwrap();
        assert_eq!(usage.cache_read_input_tokens, 60);
        assert_eq!(usage.cache_creation_input_tokens, 20);
    }

    #[test]
    fn driver_emits_text_then_message_done_on_stop() {
        let mut driver = OaiStreamDriver::new();
        let events_a = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {"content": "hi"}, "finish_reason": null}]
            }))
            .unwrap();
        let events_b = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            }))
            .unwrap();

        assert!(matches!(events_a[0], StreamEvent::ContentStart { .. }));
        assert!(matches!(&events_a[1], StreamEvent::TextDelta { text, .. } if text == "hi"));
        assert!(matches!(events_b[0], StreamEvent::ContentStop { .. }));
        match events_b.last().unwrap() {
            StreamEvent::MessageDone { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 1);
                assert_eq!(usage.output_tokens, 2);
            }
            _ => panic!("expected MessageDone last"),
        }
    }

    #[test]
    fn driver_waits_for_usage_sent_after_the_finish_reason() {
        let mut driver = OaiStreamDriver::new();
        let finish = driver
            .consume_chunk(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }))
            .unwrap();
        assert!(!finish
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageDone { .. })));
        let usage = driver
            .consume_chunk(&json!({
                "choices": [],
                "usage": { "prompt_tokens": 7, "completion_tokens": 3 }
            }))
            .unwrap();
        match usage.as_slice() {
            [StreamEvent::MessageDone { usage, .. }] => {
                assert_eq!((usage.input_tokens, usage.output_tokens), (7, 3));
            }
            other => panic!("expected MessageDone alone, got {other:?}"),
        }
        assert!(driver.finish().is_none(), "done once");
    }

    #[test]
    fn driver_reads_bedrock_invocation_metrics() {
        let mut driver = OaiStreamDriver::new();
        let events = driver
            .consume_chunk(&json!({
                "choices": [{ "delta": {}, "finish_reason": "tool_calls" }],
                "amazon-bedrock-invocationMetrics": { "inputTokenCount": 120, "outputTokenCount": 9 }
            }))
            .unwrap();
        match events.last() {
            Some(StreamEvent::MessageDone { stop_reason, usage }) => {
                assert_eq!(*stop_reason, StopReason::ToolUse);
                assert_eq!((usage.input_tokens, usage.output_tokens), (120, 9));
            }
            other => panic!("expected MessageDone last, got {other:?}"),
        }
    }

    #[test]
    fn a_stream_that_ends_without_usage_is_still_done() {
        let mut driver = OaiStreamDriver::new();
        driver
            .consume_chunk(
                &json!({ "choices": [{ "delta": {"content": "hi"}, "finish_reason": "stop" }] }),
            )
            .unwrap();
        assert!(matches!(
            driver.finish(),
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));
    }

    #[test]
    fn driver_emits_tool_use_chunks() {
        let mut driver = OaiStreamDriver::new();
        let evts1 = driver
            .consume_chunk(&json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "c1",
                            "function": {"name": "f", "arguments": "{\"a\""}
                        }]
                    },
                    "finish_reason": null
                }]
            }))
            .unwrap();
        let evts2 = driver
            .consume_chunk(&json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": ":1}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }))
            .unwrap();

        assert!(matches!(
            &evts1[0],
            StreamEvent::ToolUseStart { name, id, .. } if name == "f" && id == "c1"
        ));
        assert!(matches!(
            &evts1[1],
            StreamEvent::InputJsonDelta { partial, .. } if partial == "{\"a\""
        ));
        assert!(matches!(
            &evts2[0],
            StreamEvent::InputJsonDelta { partial, .. } if partial == ":1}"
        ));
        assert!(matches!(
            driver.finish(),
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn driver_truncation_error_after_partial_content() {
        let mut driver = OaiStreamDriver::new();
        let _ = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {"content": "hi"}, "finish_reason": null}]
            }))
            .unwrap();
        let err = driver.truncation_error().to_string();
        assert!(err.contains("truncated"));
    }

    #[test]
    fn driver_truncation_error_with_no_content() {
        let driver = OaiStreamDriver::new();
        let err = driver.truncation_error().to_string();
        assert!(err.contains("without finish_reason or content"));
    }

    #[test]
    fn driver_finish_reason_with_only_tool_calls_emits_content_stop() {
        let mut driver = OaiStreamDriver::new();
        let evts1 = driver
            .consume_chunk(&json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "c1",
                            "function": {"name": "f", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": null
                }]
            }))
            .unwrap();
        let evts2 = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
            }))
            .unwrap();
        assert!(matches!(evts1[0], StreamEvent::ToolUseStart { .. }));
        assert!(
            evts2
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentStop { .. })),
            "ContentStop must fire so StreamCollector flushes the open tool-use block",
        );
        assert!(matches!(
            driver.finish(),
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn build_body_includes_tool_defs() {
        let body = build_body(
            None,
            &[LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "x".to_string(),
                }],
            }],
            &[crate::llm::types::LlmToolDef {
                name: "f".to_string(),
                description: "d".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            128,
            "zai.glm-5",
            false,
            None,
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"].as_str(), Some("function"));
        assert_eq!(tools[0]["function"]["name"].as_str(), Some("f"));
    }

    #[test]
    fn driver_handles_empty_content_deltas() {
        let mut driver = OaiStreamDriver::new();
        let evts = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {"content": ""}, "finish_reason": null}]
            }))
            .unwrap();
        assert!(evts.is_empty(), "empty content delta should be silent");
    }

    #[test]
    fn driver_propagates_finish_with_no_content() {
        let mut driver = OaiStreamDriver::new();
        let evts = driver
            .consume_chunk(&json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 0}
            }))
            .unwrap();
        match evts.last().unwrap() {
            StreamEvent::MessageDone { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 3);
            }
            _ => panic!("expected MessageDone"),
        }
    }

    fn wrap_bytes(inner: &[u8]) -> Vec<u8> {
        let envelope = json!({ "bytes": BASE64.encode(inner) });
        serde_json::to_vec(&envelope).unwrap()
    }

    #[test]
    fn decode_oai_frame_inline_json_passes_through() {
        let json = br#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let out = decode_oai_frame(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"].as_str(),
            Some("hi")
        );
    }

    #[test]
    fn decode_oai_frame_bytes_envelope_with_naked_json() {
        let inner = br#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let envelope = wrap_bytes(inner);
        let out = decode_oai_frame(&envelope).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"].as_str(),
            Some("hi")
        );
    }

    #[test]
    fn decode_oai_frame_bytes_envelope_with_sse_payload() {
        let inner = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let envelope = wrap_bytes(inner);
        let out = decode_oai_frame(&envelope).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"].as_str(),
            Some("hi")
        );
    }

    #[test]
    fn decode_oai_frame_raw_sse_with_done_terminator() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n";
        let out = decode_oai_frame(sse).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["content"].as_str(), Some("x"));
    }

    #[test]
    fn decode_oai_frame_multi_event_sse_block() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n";
        let out = decode_oai_frame(sse).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"].as_str(),
            Some("hi")
        );
        assert_eq!(
            out[1]["choices"][0]["delta"]["content"].as_str(),
            Some(" there")
        );
    }

    #[test]
    fn decode_oai_frame_rejects_non_utf8_garbage() {
        let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
        let err = decode_oai_frame(bytes).unwrap_err();
        assert!(err.to_string().contains("neither JSON nor UTF-8"));
    }

    #[test]
    fn decode_oai_frame_bytes_envelope_bad_base64_errors() {
        let envelope = br#"{"bytes":"!!!not-base64!!!"}"#;
        let err = decode_oai_frame(envelope).unwrap_err();
        assert!(err.to_string().contains("base64 decode failed"));
    }
}

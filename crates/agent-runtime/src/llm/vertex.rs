//! Anthropic Claude on Google Cloud Vertex AI.
//!
//! The wire format matches [`super::bedrock_invoke`]: an Anthropic Messages
//! request body with `anthropic_version: "vertex-2023-10-16"` and the model
//! id moved into the URL path. Streaming is plain SSE (no AWS event-stream
//! frames). Authentication uses GCP OAuth2 access tokens via Application
//! Default Credentials or an explicit service-account JSON file — see
//! [`super::vertex_auth`].
//!
//! Callers must ensure `GOOGLE_CLOUD_PROJECT` and `VERTEX_LOCATION` are set
//! before constructing the provider (the factory panics if either is missing).
//! Vertex Claude quotas default to a few QPS per project; request increases
//! via the GCP console under IAM & Admin → Quotas.

use std::sync::Arc;
use std::time::Duration;

use eyre::{eyre, Result};
use futures::StreamExt;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::anthropic::{build_system_blocks, CacheControl};
use super::bedrock_capabilities::ThinkingMode;
use super::streaming::{parse_event_json, StreamEvent};
use super::types::{
    AnthropicToolDef, ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
};
use super::vertex_auth::VertexTokenSource;
use super::vertex_capabilities;

const ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
// Current-gen Vertex Claude is published unversioned (no @date snapshot) and
// served from the `global` endpoint — see default_tiers.toml [vertex.anthropic].
const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const PATH_ENCODE: &AsciiSet = &CONTROLS.add(b' ').add(b'/').add(b'?').add(b'#').add(b'%');

/// Vertex AI client for Anthropic Claude models.
pub struct VertexAnthropicClient {
    http: Client,
    token_source: Arc<dyn VertexTokenSource>,
    project: String,
    location: String,
    base_url: Option<String>,
    default_model: String,
    default_max_tokens: u32,
    thinking_budget_tokens: Option<u32>,
}

impl VertexAnthropicClient {
    pub fn new(
        token_source: Arc<dyn VertexTokenSource>,
        project: impl Into<String>,
        location: impl Into<String>,
    ) -> Self {
        Self {
            http: Client::builder()
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new()),
            token_source,
            project: project.into(),
            location: location.into(),
            base_url: None,
            default_model: DEFAULT_MODEL.to_string(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
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

    /// Override the API base URL (test-only — points to a mock server).
    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    fn endpoint(&self, model: &str, stream: bool) -> String {
        match &self.base_url {
            Some(base) => vertex_url_with_base(base, &self.project, &self.location, model, stream),
            None => vertex_url(&self.project, &self.location, model, stream),
        }
    }
}

/// Build the production Vertex AI URL for a `:rawPredict` or
/// `:streamRawPredict` request.
pub fn vertex_url(project: &str, location: &str, model: &str, stream: bool) -> String {
    let base = format!("https://{location}-aiplatform.googleapis.com");
    vertex_url_with_base(&base, project, location, model, stream)
}

fn vertex_url_with_base(
    base: &str,
    project: &str,
    location: &str,
    model: &str,
    stream: bool,
) -> String {
    let suffix = if stream {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    let model_encoded = utf8_percent_encode(model, PATH_ENCODE);
    format!(
        "{base}/v1/projects/{project}/locations/{location}/publishers/anthropic/models/{model_encoded}:{suffix}",
        base = base.trim_end_matches('/')
    )
}

fn build_body(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
    model_id: &str,
    enable_cache: bool,
    thinking_budget: Option<u32>,
) -> Value {
    let cache = enable_cache && vertex_capabilities::caches(model_id);
    let supports_thinking = matches!(
        vertex_capabilities::thinking(model_id),
        ThinkingMode::ToggleWithBudget
    );
    let sanitized: Vec<LlmMessage> = super::image_cap::anthropic_shaped(messages)
        .iter()
        .map(sanitize_message)
        .collect();
    let mut body = json!({
        "anthropic_version": ANTHROPIC_VERSION,
        "max_tokens": max_tokens,
        "messages": super::cache_utils::messages_with_cache_breakpoint(&sanitized, cache),
    });

    if let Some(prompt) = system_prompt {
        let blocks = build_system_blocks(prompt, cache);
        body["system"] = serde_json::to_value(&blocks).unwrap_or_else(|_| json!(prompt));
    }

    if !tools.is_empty() {
        let mut anthropic_tools: Vec<AnthropicToolDef> =
            tools.iter().map(AnthropicToolDef::from).collect();
        if cache {
            if let Some(last) = anthropic_tools.last_mut() {
                last.cache_control = Some(CacheControl::ephemeral());
            }
        }
        body["tools"] = serde_json::to_value(&anthropic_tools).unwrap_or(Value::Null);
    }

    if supports_thinking {
        let (thinking, output) = super::anthropic::thinking_fields(
            model_id,
            thinking_budget,
            super::provider::effective_effort(),
            max_tokens,
        );
        if let Some(thinking) = thinking {
            body["thinking"] = thinking;
        }
        if let Some(output) = output {
            body["output_config"] = output;
        }
    }

    body
}

fn parse_response(
    value: Value,
    fallback_model: &str,
) -> Result<(Vec<ContentBlock>, StopReason, TokenUsage, String)> {
    let content: Vec<ContentBlock> = serde_json::from_value(value["content"].clone())
        .map_err(|e| eyre!("failed to parse Vertex response content: {e}"))?;
    let stop_reason: StopReason =
        serde_json::from_value(value["stop_reason"].clone()).unwrap_or(StopReason::EndTurn);
    let usage = TokenUsage {
        input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
        output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
        cache_creation_input_tokens: value["usage"]["cache_creation_input_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
        cache_read_input_tokens: value["usage"]["cache_read_input_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
    };
    let resolved_model = value["model"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| fallback_model.to_string());
    Ok((content, stop_reason, usage, resolved_model))
}

fn sanitize_message(m: &LlmMessage) -> LlmMessage {
    LlmMessage {
        role: m.role.clone(),
        content: m
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    signature,
                } if !input.is_object() => ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: Value::Object(serde_json::Map::new()),
                    signature: signature.clone(),
                },
                other => other.clone(),
            })
            .collect(),
    }
}

/// Translate a non-2xx Vertex AI response into an `eyre::Error` with a
/// status-specific hint.
pub fn parse_vertex_error(status: StatusCode, body: &str) -> eyre::Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let (message, gcp_status) = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .map(|err| {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)")
                .to_string();
            let st = err
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_string();
            (msg, st)
        })
        .unwrap_or_else(|| (body.to_string(), "UNKNOWN".to_string()));

    let hint = hint_for_status(status);
    eyre!("Vertex AI {status} ({gcp_status}): {message}{hint}")
}

fn hint_for_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 | 403 => {
            " — re-run `gcloud auth application-default login` or confirm the service account has the Vertex AI User role"
        }
        404 => " — verify the model is deployed in this location (see Vertex Model Garden for supported regions)",
        429 => {
            " — Vertex Claude quota exhausted; request an increase via the GCP console (IAM & Admin → Quotas)"
        }
        _ => "",
    }
}

fn parse_sse_block(block: &str) -> Option<StreamEvent> {
    let mut event_type = String::new();
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim());
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let json: Value = serde_json::from_str(&data).ok()?;
    if event_type.is_empty() {
        if let Some(t) = json.get("type").and_then(Value::as_str) {
            event_type = t.to_string();
        }
    }
    parse_event_json(&event_type, &json)
}

impl VertexAnthropicClient {
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
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "vertex-anthropic",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );
        let body = build_body(
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            self.effective_thinking_budget(),
        );

        let token = self.token_source.fetch_token().await?;
        let url = self.endpoint(model, false);

        tracing::debug!(
            model = %model,
            location = %self.location,
            project = %self.project,
            messages = messages.len(),
            tools = tools.len(),
            "sending request to Vertex AI"
        );

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("Vertex HTTP request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(failed to read body)".to_string());
            let mut err = parse_vertex_error(status, &body);
            if let Some(ra) = retry_after {
                err = eyre!("{err} [retry-after: {ra}s]");
            }
            return Err(err);
        }

        let value: Value = response
            .json()
            .await
            .map_err(|e| eyre!("failed to parse Vertex JSON response: {e}"))?;
        let (content, stop_reason, usage, resolved_model) = parse_response(value, model)?;

        tracing::debug!(
            model = %resolved_model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            cache_read_input_tokens = usage.cache_read_input_tokens,
            stop_reason = ?stop_reason,
            "received response from Vertex AI"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: resolved_model,
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
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = model.unwrap_or(&self.default_model);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        tracing::info!(
            provider = "vertex-anthropic",
            model = %model,
            streaming = true,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );
        let body = build_body(
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            self.effective_thinking_budget(),
        );

        let token = self.token_source.fetch_token().await?;
        let url = self.endpoint(model, true);

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("Vertex streaming request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(failed to read body)".to_string());
            return Err(parse_vertex_error(status, &body));
        }

        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut got_message_done = false;

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| eyre!("Vertex stream chunk error: {e}"))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(idx) = buffer.find("\n\n") {
                let block: String = buffer.drain(..idx + 2).collect();
                let block = block.trim_end_matches('\n');

                let Some(event) = parse_sse_block(block) else {
                    continue;
                };

                if let StreamEvent::Error { message } = &event {
                    let err = eyre!("Vertex SSE error: {message}");
                    let _ = tx.send(event).await;
                    return Err(err);
                }
                let is_done = matches!(event, StreamEvent::MessageDone { .. });
                if is_done {
                    got_message_done = true;
                }
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
                if is_done {
                    return Ok(());
                }
            }
        }

        if got_message_done {
            Ok(())
        } else {
            let msg = "Vertex stream ended without message_delta (incomplete response)";
            let _ = tx
                .send(StreamEvent::Error {
                    message: msg.to_string(),
                })
                .await;
            Err(eyre!(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    const OPUS: &str = "claude-opus-4-7@20251015";
    const SONNET: &str = "claude-sonnet-4-6@20251015";
    const HAIKU3: &str = "claude-3-haiku@20240307";

    #[test]
    fn body_keeps_to_the_image_limit() {
        let image = ContentBlock::Image {
            source: crate::llm::types::ImageSource {
                source_type: "base64".into(),
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            },
        };
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![image; 21],
        }];
        let body = build_body(None, &messages, &[], 1024, SONNET, false, None).to_string();
        assert_eq!(body.matches(r#""type":"image""#).count(), 20);
    }

    #[test]
    fn vertex_url_non_streaming() {
        let url = vertex_url("my-proj", "us-east5", "claude-opus-4-7@20251015", false);
        assert_eq!(
            url,
            "https://us-east5-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-east5/publishers/anthropic/models/claude-opus-4-7@20251015:rawPredict"
        );
    }

    #[test]
    fn vertex_url_streaming() {
        let url = vertex_url(
            "my-proj",
            "europe-west1",
            "claude-sonnet-4-6@20251015",
            true,
        );
        assert_eq!(
            url,
            "https://europe-west1-aiplatform.googleapis.com/v1/projects/my-proj/locations/europe-west1/publishers/anthropic/models/claude-sonnet-4-6@20251015:streamRawPredict"
        );
    }

    #[test]
    fn vertex_url_preserves_at_sign_in_model() {
        let url = vertex_url("p", "us-east5", "claude-opus-4-7@20251015", false);
        assert!(
            url.contains("claude-opus-4-7@20251015:rawPredict"),
            "@ should not be percent-encoded: {url}"
        );
    }

    #[test]
    fn body_omits_model_and_sets_vertex_anthropic_version() {
        let body = build_body(Some("sys"), &[user("hi")], &[], 4096, OPUS, false, None);
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"].as_str(), Some(ANTHROPIC_VERSION));
        assert_eq!(body["max_tokens"].as_u64(), Some(4096));
        assert!(body["messages"].is_array());
    }

    #[test]
    fn body_with_cache_applies_cache_control_to_system_and_last_tool() {
        let body = build_body(
            Some("static"),
            &[user("hi")],
            &[tool("a"), tool("b")],
            1024,
            OPUS,
            true,
            None,
        );
        let system = body["system"].as_array().expect("system blocks");
        assert!(system[0]["cache_control"].is_object());
        let tools = body["tools"].as_array().expect("tools");
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools.last().unwrap()["cache_control"].is_object());
        // The last message's last block carries the conversation breakpoint.
        let last_content = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(last_content.last().unwrap()["cache_control"].is_object());
    }

    #[test]
    fn body_without_cache_omits_cache_control() {
        let body = build_body(
            Some("static"),
            &[user("hi")],
            &[tool("a")],
            1024,
            OPUS,
            false,
            None,
        );
        let system = body["system"].as_array().expect("system blocks");
        assert!(system[0].get("cache_control").is_none());
        let tools = body["tools"].as_array().expect("tools");
        assert!(tools[0].get("cache_control").is_none());
    }

    #[test]
    fn body_skips_cache_for_non_caching_model() {
        let body = build_body(
            Some("static"),
            &[user("hi")],
            &[tool("a")],
            1024,
            HAIKU3,
            true,
            None,
        );
        let system = body["system"].as_array().expect("system blocks");
        assert!(system[0].get("cache_control").is_none());
        let tools = body["tools"].as_array().expect("tools");
        assert!(tools[0].get("cache_control").is_none());
    }

    #[test]
    fn body_emits_thinking_when_supported_and_budget_set() {
        let body = build_body(None, &[user("hi")], &[], 16384, SONNET, false, Some(8000));
        assert_eq!(body["thinking"]["type"].as_str(), Some("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"].as_u64(), Some(8000));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn body_opus_4_7_emits_adaptive_thinking_with_effort() {
        let body = build_body(None, &[user("hi")], &[], 16384, OPUS, false, Some(10000));
        assert_eq!(body["thinking"]["type"].as_str(), Some("adaptive"));
        assert_eq!(body["thinking"]["display"].as_str(), Some("summarized"));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body["thinking"].get("effort").is_none());
        assert_eq!(body["output_config"]["effort"].as_str(), Some("high"));
    }

    #[tokio::test]
    async fn body_sends_the_turns_configured_effort_with_or_without_a_budget() {
        use crate::llm::anthropic::Effort;
        use crate::llm::provider::{with_effort_override, with_reasoning_suppressed};
        for budget in [None, Some(1_000)] {
            let body = with_effort_override(Some(Effort::Max), async {
                build_body(None, &[user("hi")], &[], 16384, OPUS, false, budget)
            })
            .await;
            assert_eq!(body["thinking"]["type"].as_str(), Some("adaptive"));
            assert_eq!(body["output_config"]["effort"].as_str(), Some("max"));
        }
        let held_back = with_effort_override(
            Some(Effort::Max),
            with_reasoning_suppressed(async {
                build_body(None, &[user("hi")], &[], 16384, OPUS, false, None)
            }),
        )
        .await;
        assert!(held_back.get("output_config").is_none());
    }

    #[test]
    fn body_omits_thinking_when_unsupported() {
        let body = build_body(None, &[user("hi")], &[], 16384, HAIKU3, false, Some(8000));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn body_omits_thinking_when_budget_absent() {
        let body = build_body(None, &[user("hi")], &[], 16384, OPUS, false, None);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn body_omits_tools_when_empty() {
        let body = build_body(None, &[user("hi")], &[], 1024, OPUS, false, None);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_omits_system_when_absent() {
        let body = build_body(None, &[user("hi")], &[], 1024, OPUS, false, None);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn sanitize_message_normalizes_non_object_tool_input() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "x".to_string(),
                input: Value::Null,
                signature: Some("sig-123".to_string()),
            }],
        };
        let sanitized = sanitize_message(&msg);
        let ContentBlock::ToolUse {
            input, signature, ..
        } = &sanitized.content[0]
        else {
            panic!("expected ToolUse");
        };
        assert!(input.is_object());
        assert_eq!(signature.as_deref(), Some("sig-123"));
    }

    #[test]
    fn sanitize_message_passes_through_object_input() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "x".to_string(),
                input: json!({"x": 1}),
                signature: None,
            }],
        };
        let sanitized = sanitize_message(&msg);
        let ContentBlock::ToolUse { input, .. } = &sanitized.content[0] else {
            panic!("expected ToolUse");
        };
        assert_eq!(input["x"], 1);
    }

    #[test]
    fn parse_response_extracts_all_fields() {
        let resp = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "model": "claude-opus-4-7@20251015",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 100,
            }
        });
        let (content, stop, usage, model) = parse_response(resp, "fallback").unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_creation_input_tokens, 4);
        assert_eq!(usage.cache_read_input_tokens, 100);
        assert_eq!(model, "claude-opus-4-7@20251015");
    }

    #[test]
    fn parse_response_falls_back_to_caller_model() {
        let resp = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let (_, _, _, model) = parse_response(resp, "fallback-model").unwrap();
        assert_eq!(model, "fallback-model");
    }

    #[test]
    fn parse_vertex_error_extracts_status_and_message() {
        let body =
            r#"{"error":{"code":429,"message":"quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = parse_vertex_error(StatusCode::TOO_MANY_REQUESTS, body);
        let s = format!("{err}");
        assert!(s.contains("RESOURCE_EXHAUSTED"), "{s}");
        assert!(s.contains("quota exceeded"), "{s}");
        assert!(s.contains("429"), "{s}");
        assert!(s.contains("Quotas"), "should include quota hint: {s}");
    }

    #[test]
    fn parse_vertex_error_401_includes_auth_hint() {
        let body = r#"{"error":{"code":401,"message":"invalid creds","status":"UNAUTHENTICATED"}}"#;
        let err = parse_vertex_error(StatusCode::UNAUTHORIZED, body);
        let s = format!("{err}");
        assert!(s.contains("application-default login"), "{s}");
    }

    #[test]
    fn parse_vertex_error_404_includes_region_hint() {
        let body = r#"{"error":{"code":404,"message":"not found","status":"NOT_FOUND"}}"#;
        let err = parse_vertex_error(StatusCode::NOT_FOUND, body);
        let s = format!("{err}");
        assert!(s.contains("Model Garden"), "{s}");
    }

    #[test]
    fn parse_vertex_error_falls_back_to_raw_body() {
        let err = parse_vertex_error(StatusCode::INTERNAL_SERVER_ERROR, "<html>oops</html>");
        let s = format!("{err}");
        assert!(s.contains("<html>oops</html>"), "{s}");
        assert!(s.contains("500"), "{s}");
    }

    #[test]
    fn parse_sse_block_handles_data_with_event_type() {
        let block = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}";
        let event = parse_sse_block(block).expect("event");
        assert!(matches!(
            event,
            StreamEvent::TextDelta { index: 0, text } if text == "Hi"
        ));
    }

    #[test]
    fn parse_sse_block_falls_back_to_type_field_when_event_missing() {
        let block = "data: {\"type\":\"content_block_stop\",\"index\":0}";
        let event = parse_sse_block(block).expect("event");
        assert!(matches!(event, StreamEvent::ContentStop { index: 0 }));
    }

    #[test]
    fn parse_sse_block_skips_ping_and_done() {
        assert!(parse_sse_block("data: [DONE]").is_none());
        assert!(parse_sse_block("event: ping\ndata: {\"type\":\"ping\"}").is_none());
    }

    mod http {
        use super::*;
        use crate::llm::vertex_auth::StaticTokenSource;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn client(server: &MockServer) -> VertexAnthropicClient {
            VertexAnthropicClient::new(
                Arc::new(StaticTokenSource::new("test-token")),
                "test-proj",
                "us-east5",
            )
            .with_base_url(server.uri())
            .with_model(OPUS)
        }

        #[tokio::test]
        async fn create_message_sends_bearer_and_parses_response() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!(
                    "/v1/projects/test-proj/locations/us-east5/publishers/anthropic/models/{OPUS}:rawPredict"
                )))
                .and(header("Authorization", "Bearer test-token"))
                .and(header("Content-Type", "application/json"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "content": [{"type": "text", "text": "Hello"}],
                    "stop_reason": "end_turn",
                    "model": OPUS,
                    "usage": {"input_tokens": 4, "output_tokens": 2},
                })))
                .mount(&server)
                .await;

            let resp = client(&server)
                .create_message(None, &[user("hi")], &[], None, None, false)
                .await
                .unwrap();
            assert_eq!(resp.text().as_deref(), Some("Hello"));
            assert_eq!(resp.usage.input_tokens, 4);
            assert_eq!(resp.usage.output_tokens, 2);
            assert_eq!(resp.model, OPUS);
        }

        #[tokio::test]
        async fn create_message_returns_parsed_error_on_4xx() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(429).set_body_string(
                    r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#,
                ))
                .mount(&server)
                .await;

            let err = client(&server)
                .create_message(None, &[user("hi")], &[], None, None, false)
                .await
                .unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("quota"), "{s}");
            assert!(s.contains("RESOURCE_EXHAUSTED"), "{s}");
        }

        #[tokio::test]
        async fn create_message_includes_retry_after_when_present() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "42")
                        .set_body_string(
                            r#"{"error":{"code":429,"message":"q","status":"RESOURCE_EXHAUSTED"}}"#,
                        ),
                )
                .mount(&server)
                .await;

            let err = client(&server)
                .create_message(None, &[user("hi")], &[], None, None, false)
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("retry-after: 42s"));
        }

        #[tokio::test]
        async fn create_message_stream_forwards_events_in_order() {
            let server = MockServer::start().await;
            let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";
            Mock::given(method("POST"))
                .and(path(format!(
                    "/v1/projects/test-proj/locations/us-east5/publishers/anthropic/models/{OPUS}:streamRawPredict"
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse),
                )
                .mount(&server)
                .await;

            let (tx, mut rx) = mpsc::channel(32);
            let cli = client(&server);
            tokio::spawn(async move {
                cli.create_message_stream(None, &[user("hi")], &[], None, None, false, tx)
                    .await
                    .expect("stream completes");
            });

            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            assert!(
                matches!(events.first(), Some(StreamEvent::ContentStart { index: 0 })),
                "got {events:?}"
            );
            assert!(events.iter().any(|e| matches!(
                e,
                StreamEvent::TextDelta { index: 0, text } if text == "Hi"
            )));
            assert!(events
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentStop { index: 0 })));
            let last = events.last().expect("last");
            assert!(
                matches!(
                    last,
                    StreamEvent::MessageDone { stop_reason, .. } if *stop_reason == StopReason::EndTurn
                ),
                "expected MessageDone, got {last:?}"
            );
        }

        #[tokio::test]
        async fn create_message_stream_surfaces_mid_stream_error() {
            let server = MockServer::start().await;
            let sse = "\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"boom\"}}\n\
\n";
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse),
                )
                .mount(&server)
                .await;

            let (tx, mut rx) = mpsc::channel(8);
            let cli = client(&server);
            let handle = tokio::spawn(async move {
                cli.create_message_stream(None, &[user("hi")], &[], None, None, false, tx)
                    .await
            });

            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            let result = handle.await.unwrap();
            assert!(result.is_err());
            assert!(format!("{}", result.unwrap_err()).contains("boom"));
            assert!(events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. })));
        }

        #[tokio::test]
        async fn create_message_stream_errors_on_truncated_stream() {
            let server = MockServer::start().await;
            let sse = "\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"oops\"}}\n\
\n";
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse),
                )
                .mount(&server)
                .await;

            let (tx, mut rx) = mpsc::channel(8);
            let cli = client(&server);
            let handle = tokio::spawn(async move {
                cli.create_message_stream(None, &[user("hi")], &[], None, None, false, tx)
                    .await
            });
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            let result = handle.await.unwrap();
            assert!(result.is_err());
            assert!(format!("{}", result.unwrap_err()).contains("incomplete response"));
            assert!(events.iter().any(|e| matches!(
                e,
                StreamEvent::Error { message } if message.contains("incomplete response")
            )));
        }

        #[tokio::test]
        async fn token_source_is_consulted_per_request() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "model": OPUS,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                })))
                .mount(&server)
                .await;

            let token_source = Arc::new(StaticTokenSource::new("rotating"));
            let cli = VertexAnthropicClient::new(token_source.clone(), "p", "us-east5")
                .with_base_url(server.uri())
                .with_model(OPUS);

            for _ in 0..3 {
                cli.create_message(None, &[user("hi")], &[], None, None, false)
                    .await
                    .unwrap();
            }
            assert_eq!(token_source.calls(), 3);
        }
    }
}

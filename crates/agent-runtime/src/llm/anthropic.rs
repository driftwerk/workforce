use eyre::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use workforce_common::settings::{is_claude_oat, oat_refusal};

use super::types::{
    AnthropicToolDef, ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// A Claude subscription token handed to the API client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", oat_refusal("The Anthropic API key"))]
pub struct OatKeyRejected;

/// Extended-thinking request schema. `Enabled` is the legacy manual-budget
/// mode; `Adaptive` is required by Opus 4.7 (and recommended on 4.6 / Sonnet
/// 4.6). The adaptive `effort` is carried separately in [`OutputConfig`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ThinkingConfig {
    Enabled { budget_tokens: u32 },
    Adaptive { display: &'static str },
}

/// Output controls sent as a top-level sibling of `thinking`. Carries the
/// adaptive `effort`; the API rejects `effort` placed inside `thinking`.
#[derive(Debug, Clone, Serialize)]
struct OutputConfig {
    effort: Effort,
}

/// Adaptive-thinking effort — soft guidance on how much Claude thinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Map a legacy thinking-token budget onto an effort tier, mirroring the
    /// budget→effort precedent in [`super::cerebras`]. `xhigh`/`max` are
    /// 4.7-specialised and intentionally not budget-derived.
    pub fn from_budget(budget: u32) -> Self {
        match budget {
            0..=2000 => Self::Low,
            2001..=8000 => Self::Medium,
            _ => Self::High,
        }
    }
}

impl Serialize for Effort {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Resolve the `(thinking, output_config)` request fields for a model and its
/// optional thinking budget. Opus 4.7/4.8, Sonnet 5, and Mythos require
/// adaptive mode; other models keep the legacy manual schema. No budget → no
/// thinking.
///
/// `max_tokens` is needed because the legacy `enabled` schema requires
/// `max_tokens > budget_tokens`: the budget is capped to fit the caller's
/// `max_tokens`, or thinking disabled when it cannot.
fn thinking_config(
    model: &str,
    budget: Option<u32>,
    max_tokens: u32,
) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
    match budget {
        Some(budget) if super::bedrock_capabilities::requires_adaptive_thinking(model) => (
            Some(ThinkingConfig::Adaptive {
                display: "summarized",
            }),
            Some(OutputConfig {
                effort: Effort::from_budget(budget),
            }),
        ),
        Some(budget) => {
            match super::bedrock_capabilities::clamp_thinking_budget(budget, max_tokens) {
                Some(b) => (Some(ThinkingConfig::Enabled { budget_tokens: b }), None),
                None => (None, None),
            }
        }
        None => (None, None),
    }
}

/// Client for the Anthropic Claude API.
pub struct AnthropicClient {
    client: Client,
    api_key: String,
    default_model: String,
    default_max_tokens: u32,
    /// Custom base URL (for proxy mode). Overrides the default API URL.
    base_url: Option<String>,
    /// Extended thinking budget. When set, the model can reason internally
    /// before producing tool calls or text output.
    thinking_budget_tokens: Option<u32>,
}

impl AnthropicClient {
    /// A client on an Anthropic API key. A Claude subscription token is
    /// refused: a plan is reached only through the `claude` CLI.
    pub fn new(api_key: impl Into<String>) -> Result<Self, OatKeyRejected> {
        let api_key = api_key.into();
        if is_claude_oat(&api_key) {
            return Err(OatKeyRejected);
        }
        Ok(Self {
            client: Client::new(),
            api_key,
            default_model: "claude-sonnet-5-5".to_string(),
            default_max_tokens: 8192,
            base_url: None,
            thinking_budget_tokens: None,
        })
    }

    /// Set a custom base URL (e.g., for the SDK proxy).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = max_tokens;
        self
    }

    /// Enable extended thinking with the given token budget.
    ///
    /// When enabled, the model can reason internally before producing tool
    /// calls or text output. This significantly improves quality for complex
    /// multi-step tasks. Requires `max_tokens >= budget_tokens`.
    pub fn with_thinking_budget(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget_tokens = Some(budget_tokens);
        self
    }

    /// Get the default model.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Get the default max tokens.
    pub fn default_max_tokens(&self) -> u32 {
        self.default_max_tokens
    }

    /// Get the HTTP client.
    pub fn http_client(&self) -> &Client {
        &self.client
    }

    /// The extended thinking budget for the request now being built: the
    /// per-turn override when one is in scope (see
    /// [`crate::llm::provider::effective_thinking_budget`]), otherwise the
    /// configured budget. The streaming request builder reads the budget
    /// through this accessor, so it must resolve the override too — streaming
    /// is the path interactive turns actually take.
    pub fn thinking_budget_tokens(&self) -> Option<u32> {
        super::provider::effective_thinking_budget(self.thinking_budget_tokens)
    }

    /// Alias of [`Self::thinking_budget_tokens`], named to match the other
    /// providers' internal accessor.
    fn effective_thinking_budget(&self) -> Option<u32> {
        self.thinking_budget_tokens()
    }

    /// Get the messages API URL.
    pub fn message_url(&self) -> String {
        if let Some(ref base) = self.base_url {
            format!("{}/v1/messages", base.trim_end_matches('/'))
        } else {
            ANTHROPIC_API_URL.to_string()
        }
    }

    /// Models endpoint, derived from the same base as `message_url`.
    fn models_url(&self) -> String {
        if let Some(ref base) = self.base_url {
            format!("{}/v1/models", base.trim_end_matches('/'))
        } else {
            "https://api.anthropic.com/v1/models".to_string()
        }
    }

    /// Auth-only credential probe: the `/v1/models` GET.
    pub async fn verify_credentials_models_probe(&self) -> Result<()> {
        let resp = self
            .client
            .get(self.models_url())
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("x-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Anthropic verify: network error: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Anthropic verify: HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ))
    }

    /// Apply auth headers to a request builder.
    pub fn apply_auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("x-api-key", &self.api_key).header(
            "anthropic-beta",
            "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14",
        )
    }

    /// Send a message to Claude and get a response.
    ///
    /// The `enable_cache` parameter controls whether prompt caching is used.
    /// When `false`, cache control annotations are skipped entirely.
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
        let messages: &[LlmMessage] = &super::image_cap::anthropic_shaped(messages);

        let anthropic_tools: Option<Vec<AnthropicToolDef>> = if tools.is_empty() {
            None
        } else {
            let mut at: Vec<AnthropicToolDef> = tools.iter().map(AnthropicToolDef::from).collect();
            if enable_cache {
                if let Some(last) = at.last_mut() {
                    last.cache_control = Some(CacheControl::ephemeral());
                }
            }
            Some(at)
        };

        // Sanitize tool_use.input values — the API requires them to be JSON objects
        // but failover/streaming can produce null or other non-object types.
        let sanitized_messages: Vec<LlmMessage> = messages
            .iter()
            .map(|m| LlmMessage {
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
                        } if !input.is_object() => {
                            tracing::warn!(
                                tool = name.as_str(),
                                "Sanitizing non-object tool_use.input before API call"
                            );
                            ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::Value::Object(serde_json::Map::new()),
                                signature: signature.clone(),
                            }
                        }
                        other => other.clone(),
                    })
                    .collect(),
            })
            .collect();

        let (thinking, output_config) =
            thinking_config(model, self.effective_thinking_budget(), max_tokens);

        let request = ApiRequest {
            model: model.to_string(),
            max_tokens,
            system: system_prompt.map(|s| build_system_blocks(s, enable_cache)),
            messages: super::cache_utils::messages_with_cache_breakpoint(
                &sanitized_messages,
                enable_cache,
            ),
            tools: anthropic_tools,
            thinking,
            output_config,
        };

        tracing::info!(
            provider = "anthropic",
            model = %model,
            streaming = false,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            "LLM invocation start"
        );

        let url = self.message_url();

        let mut req = self
            .client
            .post(url)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json");

        req = self.apply_auth_headers(req);

        let response = req
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            // Capture retry-after header before consuming the response body
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".to_string());
            let mut err_msg = format!("Anthropic API error ({}): {}", status, error_body);
            if let Some(ref ra) = retry_after {
                err_msg.push_str(&format!(" [retry-after: {}s]", ra));
            }
            return Err(eyre::eyre!("{}", err_msg));
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .map_err(|e| eyre::eyre!("Failed to parse Anthropic response: {}", e))?;

        tracing::debug!(
            model = %api_response.model,
            input_tokens = api_response.usage.input_tokens,
            output_tokens = api_response.usage.output_tokens,
            cache_creation_input_tokens = api_response.usage.cache_creation_input_tokens,
            cache_read_input_tokens = api_response.usage.cache_read_input_tokens,
            stop_reason = ?api_response.stop_reason,
            "Received response from Anthropic"
        );

        Ok(LlmResponse {
            content: api_response.content,
            stop_reason: api_response.stop_reason,
            usage: api_response.usage,
            model: api_response.model,
        })
    }
}

/// Cache control annotation for Anthropic prompt caching.
///
/// When applied to a system content block or tool definition, Anthropic caches
/// the request prefix up to and including that block. Subsequent requests with
/// the same prefix pay only ~10% of normal input token costs.
#[derive(Debug, Clone, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    control_type: String,
}

impl CacheControl {
    /// Create an ephemeral cache control (5-minute TTL, refreshed on each hit).
    pub fn ephemeral() -> Self {
        Self {
            control_type: "ephemeral".to_string(),
        }
    }
}

/// A content block within the system prompt, supporting cache control.
#[derive(Debug, Clone, Serialize)]
pub struct SystemContentBlock {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// The marker used to split a system prompt into cache segments. The chat
/// prompt carries one marker, yielding [static | semi-stable], and both
/// segments are annotated with `cache_control: {"type": "ephemeral"}`; a
/// prompt with more markers (an isolated run's joined turn context, the
/// round-cap wrap-up banner) leaves everything past the second segment
/// uncached. At most 2 system breakpoints — Anthropic allows 4 in total and
/// the tool catalogue + message breakpoint use the others.
pub const CACHE_BREAK_MARKER: &str = "\n<!-- CACHE_BREAK -->\n";

/// How many system segments may carry a cache breakpoint.
pub const MAX_CACHED_SYSTEM_SEGMENTS: usize = 2;

/// Split a prompt on [`CACHE_BREAK_MARKER`] into its non-empty segments.
/// No marker (or all-empty segments) yields the whole prompt as one segment.
pub fn cache_segments(prompt: &str) -> Vec<&str> {
    let segments: Vec<&str> = prompt
        .split(CACHE_BREAK_MARKER)
        .filter(|s| !s.trim().is_empty())
        .collect();
    if segments.is_empty() {
        vec![prompt]
    } else {
        segments
    }
}

/// Whether segment `i` gets a cache breakpoint: the first
/// [`MAX_CACHED_SYSTEM_SEGMENTS`] do. The per-turn tail no longer ends the
/// system prompt (it travels in the task message), so the last segment of the
/// two-segment chat prompt is the semi-stable block and must be cached too.
pub fn segment_is_cached(i: usize) -> bool {
    i < MAX_CACHED_SYSTEM_SEGMENTS
}

/// Build structured system content blocks from a prompt string.
///
/// Splits on [`CACHE_BREAK_MARKER`]; the first two segments are marked
/// cacheable (a single segment is cached whole). The `enable_cache` parameter
/// controls whether cache control annotations are added at all.
pub fn build_system_blocks(prompt: &str, enable_cache: bool) -> Vec<SystemContentBlock> {
    cache_segments(prompt)
        .into_iter()
        .enumerate()
        .map(|(i, text)| SystemContentBlock {
            content_type: "text".to_string(),
            text: text.to_string(),
            cache_control: (enable_cache && segment_is_cached(i)).then(CacheControl::ephemeral),
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemContentBlock>>,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
    model: String,
    stop_reason: StopReason,
    usage: TokenUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_blocks_three_segments_cache_first_two() {
        let prompt = format!("static{CACHE_BREAK_MARKER}semi{CACHE_BREAK_MARKER}dynamic");
        let blocks = build_system_blocks(&prompt, true);
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[1].cache_control.is_some());
        assert!(blocks[2].cache_control.is_none());
        assert_eq!(blocks[2].text, "dynamic");
    }

    #[test]
    fn system_blocks_two_segments_both_cached() {
        let prompt = format!("static{CACHE_BREAK_MARKER}semi");
        let blocks = build_system_blocks(&prompt, true);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[1].cache_control.is_some());
    }

    #[test]
    fn system_blocks_single_segment_cached_whole() {
        let blocks = build_system_blocks("just static", true);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_some());
    }

    #[test]
    fn system_blocks_empty_middle_segment_skipped() {
        let prompt = format!("static{CACHE_BREAK_MARKER}  {CACHE_BREAK_MARKER}dynamic");
        let blocks = build_system_blocks(&prompt, true);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[1].cache_control.is_some());
    }

    #[test]
    fn system_blocks_cap_cached_segments_at_two() {
        let prompt = format!("a{CACHE_BREAK_MARKER}b{CACHE_BREAK_MARKER}c{CACHE_BREAK_MARKER}d");
        let blocks = build_system_blocks(&prompt, true);
        assert_eq!(blocks.len(), 4);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[1].cache_control.is_some());
        assert!(blocks[2].cache_control.is_none());
        assert!(blocks[3].cache_control.is_none());
    }

    #[test]
    fn system_blocks_cache_disabled_no_annotations() {
        let prompt = format!("static{CACHE_BREAK_MARKER}semi{CACHE_BREAK_MARKER}dynamic");
        let blocks = build_system_blocks(&prompt, false);
        assert_eq!(blocks.len(), 3);
        assert!(blocks.iter().all(|b| b.cache_control.is_none()));
    }

    #[test]
    fn effort_from_budget_tiers() {
        assert_eq!(Effort::from_budget(0), Effort::Low);
        assert_eq!(Effort::from_budget(2000), Effort::Low);
        assert_eq!(Effort::from_budget(2001), Effort::Medium);
        assert_eq!(Effort::from_budget(8000), Effort::Medium);
        assert_eq!(Effort::from_budget(8001), Effort::High);
        assert_eq!(Effort::from_budget(10000), Effort::High);
    }

    #[test]
    fn effort_serializes_as_lowercase() {
        for (effort, expected) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            assert_eq!(effort.as_str(), expected);
            assert_eq!(
                serde_json::to_value(effort).unwrap(),
                serde_json::json!(expected)
            );
        }
    }

    #[test]
    fn opus_4_7_uses_adaptive_thinking_with_effort() {
        let (thinking, output) = thinking_config("claude-opus-4-7", Some(10000), 64_000);
        let thinking = serde_json::to_value(thinking.unwrap()).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(thinking["display"], "summarized");
        assert!(thinking.get("budget_tokens").is_none());
        assert!(thinking.get("effort").is_none());
        assert_eq!(
            serde_json::to_value(output.unwrap()).unwrap()["effort"],
            "high"
        );
    }

    #[test]
    fn sonnet_5_uses_adaptive_thinking_with_effort() {
        let (thinking, output) = thinking_config("claude-sonnet-5", Some(8000), 64_000);
        let thinking = serde_json::to_value(thinking.unwrap()).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert!(thinking.get("budget_tokens").is_none());
        assert!(output.is_some());
    }

    #[test]
    fn non_adaptive_model_uses_enabled_budget() {
        let (thinking, output) = thinking_config("claude-sonnet-4-6", Some(8000), 64_000);
        let thinking = serde_json::to_value(thinking.unwrap()).unwrap();
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 8000);
        assert!(output.is_none());
    }

    #[test]
    fn enabled_budget_clamped_or_disabled_by_max_tokens() {
        // max_tokens leaves room → budget capped below it (output headroom).
        let (thinking, _) = thinking_config("claude-sonnet-4-6", Some(10000), 9000);
        let thinking = serde_json::to_value(thinking.unwrap()).unwrap();
        assert_eq!(thinking["budget_tokens"], 9000 - 1024);
        // max_tokens too small for a valid budget → thinking disabled, no 400.
        let (thinking, _) = thinking_config("claude-sonnet-4-6", Some(10000), 1500);
        assert!(thinking.is_none());
    }

    #[test]
    fn no_budget_emits_no_thinking() {
        let (thinking, output) = thinking_config("claude-opus-4-7", None, 64_000);
        assert!(thinking.is_none());
        assert!(output.is_none());
    }

    #[test]
    fn api_request_places_effort_in_output_config_not_thinking() {
        let (thinking, output_config) = thinking_config("claude-opus-4-7", Some(10000), 64_000);
        let request = ApiRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 16000,
            system: None,
            messages: vec![],
            tools: None,
            thinking,
            output_config,
        };
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("effort").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
    }
}

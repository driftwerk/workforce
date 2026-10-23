use serde::{Deserialize, Serialize};
use workforce_common::cost::TokenCounts;

/// A message in the LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// The role of a message sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A block of content within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Extended thinking block — the model's internal reasoning.
    ///
    /// Returned by the API when extended thinking is enabled. Must be echoed
    /// back in assistant messages so the model can reference its own reasoning.
    Thinking {
        thinking: String,
        /// Cryptographic signature for thinking block integrity verification.
        /// Required by the API when echoing thinking blocks in message history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        source: ImageSource,
    },
    Document {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Provider-supplied integrity token returned with the tool call
        /// (e.g. Gemini 3.x `thought_signature`). Must be echoed back
        /// unchanged when this block reappears in the request history,
        /// otherwise the API rejects the next round.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// The tools this result loaded. Never serialised: a wire that loads
        /// natively renders it itself, every other wire reads `content`,
        /// which carries their definitions.
        #[serde(skip)]
        loaded: Option<Loaded>,
    },
}

/// Tools a tool result loaded (LLM names), and what that result says without
/// the definitions its `content` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub tools: Vec<String>,
    pub note: String,
}

/// Source data for an image content block (matches Anthropic API format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

impl ImageSource {
    /// The source as a `data:` URL.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

impl ContentBlock {
    /// Create a base64-encoded image content block.
    pub fn image_base64(media_type: &str, data: String) -> Self {
        Self::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type: media_type.to_string(),
                data,
            },
        }
    }

    /// Create a ToolUse block with sanitized input.
    ///
    /// The Anthropic API requires `tool_use.input` to be a JSON object.
    /// This constructor ensures that invariant holds even if the provider
    /// returned null, a string, or another non-object type.
    pub fn tool_use(id: String, name: String, input: serde_json::Value) -> Self {
        Self::ToolUse {
            id,
            name,
            input: sanitize_tool_input(input),
            signature: None,
        }
    }

    /// The signature of a Responses API reasoning block, if this is one.
    pub fn oai_reasoning_signature(&self) -> Option<&str> {
        match self {
            Self::Thinking {
                signature: Some(signature),
                ..
            } if signature.starts_with(OAI_REASONING_PREFIX) => Some(signature),
            _ => None,
        }
    }
}

/// Signature prefix of a [`ContentBlock::Thinking`] that carries a Responses
/// API reasoning item (its encrypted content) rather than Anthropic thinking.
pub const OAI_REASONING_PREFIX: &str = "oai-reasoning:";

/// `messages` without Responses API reasoning blocks, which an
/// Anthropic-shaped wire rejects as a bad thinking signature. A turn that
/// fails over from ChatGPT mid-way carries them in its history. A message
/// left empty is dropped. Borrows when there is nothing to strip.
pub fn strip_foreign_reasoning(messages: &[LlmMessage]) -> std::borrow::Cow<'_, [LlmMessage]> {
    if !has_foreign_reasoning(messages) {
        return std::borrow::Cow::Borrowed(messages);
    }
    std::borrow::Cow::Owned(
        messages
            .iter()
            .filter_map(|m| {
                let content: Vec<ContentBlock> = m
                    .content
                    .iter()
                    .filter(|b| b.oai_reasoning_signature().is_none())
                    .cloned()
                    .collect();
                (!content.is_empty()).then(|| LlmMessage {
                    role: m.role.clone(),
                    content,
                })
            })
            .collect(),
    )
}

/// Whether any of `messages` carries a Responses API reasoning block.
pub fn has_foreign_reasoning(messages: &[LlmMessage]) -> bool {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| b.oai_reasoning_signature().is_some())
}

/// Ensure a tool_use input value is a valid JSON object.
///
/// The Anthropic API requires `tool_use.input` to be a dictionary/object.
/// If the LLM or provider returns null, a string, or any other non-object
/// type, this function replaces it with an empty object `{}`.
pub fn sanitize_tool_input(input: serde_json::Value) -> serde_json::Value {
    if input.is_object() {
        input
    } else {
        tracing::warn!(
            input_type = match &input {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => "object",
            },
            "tool_use.input is not a JSON object, replacing with {{}}"
        );
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// An LLM tool definition (sent to the API).
///
/// Note: this is the *internal* representation. Each provider's client
/// converts it to the wire format the API expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Anthropic-specific tool definition with required `type` field.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicToolDef {
    pub r#type: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Cache control for prompt caching. When set on the last tool definition,
    /// Anthropic caches the entire prefix (system + tools) for subsequent requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<super::anthropic::CacheControl>,
    /// Declared without its definition until a `tool_reference` loads it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub defer_loading: bool,
}

impl From<&LlmToolDef> for AnthropicToolDef {
    fn from(def: &LlmToolDef) -> Self {
        Self {
            r#type: "custom".to_string(),
            name: def.name.clone(),
            description: def.description.clone(),
            input_schema: super::tool_schema::sanitize_tool_schema(def.input_schema.clone()),
            cache_control: None,
            defer_loading: false,
        }
    }
}

/// The reason the LLM stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// Token usage for a single LLM call.
///
/// **Invariant:** `input_tokens` is the *fresh* (non-cached) input portion
/// only. Cache reads + writes live in their own fields and must NOT be
/// folded into `input_tokens`. Anthropic reports usage this way natively;
/// OpenAI-compatible providers (OpenRouter, Gemini, Cerebras) fold cache
/// into `prompt_tokens` and the provider adapters call [`TokenUsage::from_oai`]
/// to enforce the invariant. `CostTracker::calculate_cost` relies on it to
/// avoid double-billing the cache portion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens written to the prompt cache (Anthropic / OpenRouter Claude routes).
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// Tokens read from the prompt cache (Anthropic / Gemini / OpenRouter / Cerebras).
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

impl TokenUsage {
    /// Construct from OpenAI-compatible usage fields, normalising
    /// `prompt_tokens` (which includes cached tokens) into the fresh-only
    /// `input_tokens` invariant by subtracting the cache portion.
    pub fn from_oai(
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read: u32,
        cache_write: u32,
    ) -> Self {
        Self {
            input_tokens: prompt_tokens.saturating_sub(cache_read + cache_write),
            output_tokens: completion_tokens,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
        }
    }
}

impl std::ops::Add for TokenUsage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

impl From<&TokenUsage> for TokenCounts {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input: u64::from(usage.input_tokens),
            output: u64::from(usage.output_tokens),
            cache_creation: u64::from(usage.cache_creation_input_tokens),
            cache_read: u64::from(usage.cache_read_input_tokens),
        }
    }
}

/// The response from an LLM call.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    pub model: String,
}

impl LlmResponse {
    /// Extract the text content from the response (the first text block).
    pub fn text(&self) -> Option<String> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .first()
            .cloned()
    }

    /// All text blocks joined — use when a reply may span several text blocks
    /// (e.g. a model emitting a prose preamble in one block and JSON in another;
    /// `text()` would return only the preamble). `None` when there is no text.
    pub fn all_text(&self) -> Option<String> {
        let joined = self
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!joined.is_empty()).then_some(joined)
    }

    /// Extract tool use requests from the response.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    /// Check if the response contains tool use requests.
    pub fn has_tool_use(&self) -> bool {
        self.stop_reason == StopReason::ToolUse
    }
}

impl LlmMessage {
    /// Create a user message with text content.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create an assistant message with text content.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create a user message containing tool results.
    pub fn tool_results(results: Vec<(String, String, bool)>) -> Self {
        Self {
            role: Role::User,
            content: results
                .into_iter()
                .map(
                    |(tool_use_id, content, is_error)| ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: if is_error { Some(true) } else { None },
                        loaded: None,
                    },
                )
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oai_reasoning() -> ContentBlock {
        ContentBlock::Thinking {
            thinking: "**Planning**".into(),
            signature: Some(format!("{OAI_REASONING_PREFIX}v1:{{\"enc\":\"gAAA\"}}")),
        }
    }

    #[test]
    fn strip_foreign_reasoning_keeps_anthropic_thinking_and_drops_emptied_messages() {
        let anthropic = ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: Some("EqQBCkYIAxgCIkD".into()),
        };
        let messages = vec![
            LlmMessage::user("hi"),
            LlmMessage {
                role: Role::Assistant,
                content: vec![oai_reasoning()],
            },
            LlmMessage {
                role: Role::Assistant,
                content: vec![
                    oai_reasoning(),
                    anthropic.clone(),
                    ContentBlock::tool_use("call_1".into(), "t".into(), serde_json::json!({})),
                ],
            },
        ];

        let stripped = strip_foreign_reasoning(&messages);
        assert_eq!(stripped.len(), 2);
        assert!(matches!(
            stripped[1].content.as_slice(),
            [ContentBlock::Thinking { signature: Some(s), .. }, ContentBlock::ToolUse { .. }]
                if s == "EqQBCkYIAxgCIkD"
        ));
    }

    #[test]
    fn strip_foreign_reasoning_borrows_when_nothing_to_strip() {
        let messages = vec![LlmMessage::user("hi"), LlmMessage::assistant("hello")];
        assert!(matches!(
            strip_foreign_reasoning(&messages),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn the_tools_a_result_loaded_never_reach_the_wire() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "Loaded: vault_set.".into(),
            is_error: None,
            loaded: Some(Loaded {
                tools: vec!["vault_set".into()],
                note: "Loaded: vault_set.".into(),
            }),
        };
        let wire = serde_json::to_value(&block).unwrap();
        assert!(wire.get("loaded").is_none());
        match serde_json::from_value::<ContentBlock>(wire).unwrap() {
            ContentBlock::ToolResult { loaded, .. } => assert!(loaded.is_none()),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn llm_message_user() {
        let msg = LlmMessage::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "Hello"));
    }

    #[test]
    fn llm_response_text_extraction() {
        let response = LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Here's the answer".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            model: "claude-sonnet-4".to_string(),
        };
        assert_eq!(response.text(), Some("Here's the answer".to_string()));
        assert!(!response.has_tool_use());
    }

    #[test]
    fn all_text_joins_blocks_that_text_would_drop() {
        let response = LlmResponse {
            content: vec![
                ContentBlock::Text {
                    text: "Based on my analysis:".to_string(),
                },
                ContentBlock::Text {
                    text: "[{\"claim\":\"x\",\"tier\":\"verified\"}]".to_string(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            model: "claude-sonnet-4".to_string(),
        };
        // `text()` returns only the preamble — the bug that dropped the JSON.
        assert_eq!(response.text(), Some("Based on my analysis:".to_string()));
        // `all_text()` keeps both, so the JSON survives downstream extraction.
        let all = response.all_text().unwrap();
        assert!(all.contains("Based on my analysis:"));
        assert!(all.contains("[{\"claim\":\"x\",\"tier\":\"verified\"}]"));
        assert!(LlmResponse {
            content: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            model: "m".to_string(),
        }
        .all_text()
        .is_none());
    }

    #[test]
    fn llm_response_tool_use_extraction() {
        let response = LlmResponse {
            content: vec![
                ContentBlock::Text {
                    text: "I'll read the file".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool_1".to_string(),
                    name: "filesystem.read".to_string(),
                    input: serde_json::json!({"path": "/tmp/test.txt"}),
                    signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
            model: "claude-sonnet-4".to_string(),
        };
        assert!(response.has_tool_use());
        let tools = response.tool_uses();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, "filesystem.read");
    }

    #[test]
    fn sanitize_tool_input_preserves_object() {
        let input = serde_json::json!({"path": "/tmp/test"});
        let result = sanitize_tool_input(input.clone());
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_tool_input_replaces_null() {
        let result = sanitize_tool_input(serde_json::Value::Null);
        assert!(result.is_object());
        assert_eq!(result.as_object().unwrap().len(), 0);
    }

    #[test]
    fn sanitize_tool_input_replaces_string() {
        let result = sanitize_tool_input(serde_json::Value::String("bad".to_string()));
        assert!(result.is_object());
    }

    #[test]
    fn sanitize_tool_input_replaces_array() {
        let result = sanitize_tool_input(serde_json::json!([1, 2, 3]));
        assert!(result.is_object());
    }

    #[test]
    fn content_block_tool_use_constructor_sanitizes() {
        let block = ContentBlock::tool_use(
            "id".to_string(),
            "name".to_string(),
            serde_json::Value::Null,
        );
        if let ContentBlock::ToolUse { input, .. } = block {
            assert!(input.is_object());
        } else {
            panic!("Expected ToolUse block");
        }
    }

    #[test]
    fn from_oai_subtracts_cache_from_prompt_tokens() {
        // OpenAI-style: prompt_tokens = 10000 includes cache_read = 8000.
        // Fresh-only invariant: input_tokens must be 2000.
        let usage = TokenUsage::from_oai(10_000, 500, 8_000, 0);
        assert_eq!(usage.input_tokens, 2_000);
        assert_eq!(usage.cache_read_input_tokens, 8_000);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.output_tokens, 500);
    }

    #[test]
    fn from_oai_subtracts_both_read_and_write() {
        // prompt_tokens = read + write + fresh
        let usage = TokenUsage::from_oai(10_000, 0, 6_000, 3_000);
        assert_eq!(usage.input_tokens, 1_000);
        assert_eq!(usage.cache_read_input_tokens, 6_000);
        assert_eq!(usage.cache_creation_input_tokens, 3_000);
    }

    #[test]
    fn from_oai_saturates_when_cache_exceeds_prompt() {
        // Defensive: a buggy provider that reports cache > prompt must not
        // wrap to a huge u32. saturating_sub keeps input_tokens at 0.
        let usage = TokenUsage::from_oai(100, 0, 200, 0);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 200);
    }

    #[test]
    fn from_oai_no_cache_passes_through() {
        let usage = TokenUsage::from_oai(1_500, 750, 0, 0);
        assert_eq!(usage.input_tokens, 1_500);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
    }

    #[test]
    fn anthropic_tool_def_strips_top_level_constraint_keys() {
        let def = LlmToolDef {
            name: "google.drive.upload".to_string(),
            description: "Upload a file".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "oneOf": [
                    { "required": ["content"] },
                    { "required": ["content_base64"] },
                ],
            }),
        };
        let converted = AnthropicToolDef::from(&def);
        assert_eq!(converted.r#type, "custom");
        assert_eq!(converted.name, "google.drive.upload");
        assert!(converted.input_schema.get("oneOf").is_none());
        assert!(converted.input_schema.get("allOf").is_none());
        assert!(converted.input_schema.get("anyOf").is_none());
        assert_eq!(
            converted.input_schema["properties"]["name"]["type"],
            serde_json::json!("string")
        );
    }

    #[test]
    fn anthropic_tool_def_keeps_nested_constraint_keys() {
        let def = LlmToolDef {
            name: "x".to_string(),
            description: "y".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "trigger": {
                        "type": "object",
                        "anyOf": [{ "required": ["a"] }, { "required": ["b"] }],
                    }
                }
            }),
        };
        let converted = AnthropicToolDef::from(&def);
        assert_eq!(converted.input_schema, def.input_schema);
    }
}

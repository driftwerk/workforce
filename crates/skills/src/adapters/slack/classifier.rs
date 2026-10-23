//! Pre-agent scope classifier for Slack channels.
//!
//! Decides whether an inbound message is on-topic for a channel-specific
//! persona. Pure function over an `LlmProvider` so callers can choose the
//! model; always fails open (treats as on-topic) on LLM/parse error so a
//! flaky classifier never silences the bot.

use serde::{Deserialize, Serialize};
use tracing::warn;

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::{ContentBlock, LlmMessage, LlmResponse};

/// Generous upper bound: reasoning models (Gemini 2.5/3.x, Claude with
/// extended thinking) consume thousands of hidden tokens before producing
/// any output. The JSON verdict itself is tiny (~30 tokens). Non-reasoning
/// providers ignore the headroom and use what they need.
const CLASSIFIER_MAX_TOKENS: u32 = 16384;

const GENERIC_CANNED_RESPONSE: &str =
    "I'm a specialized bot and can only answer questions related to my specific purpose.";

const PRESET_PERSONAS: &[Preset] = &[
    Preset {
        name: "triage",
        topic: "bug reports, feature requests, questions about triaged items, questions about triage itself",
        canned: "I'm the triage bot. I only handle bug reports, feature requests, and questions about triage.",
    },
    Preset {
        name: "faq",
        topic: "product questions, feature explanations, how-to questions",
        canned: "I'm the FAQ bot. I only answer questions about the product, features, and how to use them.",
    },
    Preset {
        name: "qa",
        topic: "test failures, regression reports, quality issues",
        canned: "I'm the QA bot. I only handle test failures, regression reports, and quality issues.",
    },
    Preset {
        name: "support",
        topic: "customer support tickets, troubleshooting, account issues",
        canned: "I'm the support bot. I only handle customer tickets, troubleshooting, and account issues.",
    },
    Preset {
        name: "sales",
        topic: "product questions, pricing questions, demo requests",
        canned: "I'm the sales bot. I only handle product questions, pricing inquiries, and demo requests.",
    },
];

struct Preset {
    name: &'static str,
    topic: &'static str,
    canned: &'static str,
}

/// Result of a one-shot scope classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeClassification {
    pub on_topic: bool,
    pub reason: String,
    /// The classifier gave no verdict and the message was let through:
    /// `on_topic` is then a policy, not a judgement.
    #[serde(skip)]
    pub failed_open: bool,
}

impl ScopeClassification {
    /// Fail-open verdict used whenever the LLM call or JSON parse errors.
    fn open(reason: impl Into<String>) -> Self {
        Self {
            on_topic: true,
            reason: reason.into(),
            failed_open: true,
        }
    }
}

/// Topic definition for a recognised preset persona; `None` for unknown names.
pub fn preset_definition(name: &str) -> Option<&'static str> {
    find_preset(name).map(|p| p.topic)
}

/// Iterator over the known preset persona names. Used by docs/tests.
pub fn preset_names() -> impl Iterator<Item = &'static str> {
    PRESET_PERSONAS.iter().map(|p| p.name)
}

/// Resolve the topic prompt for a scope filter — `custom_prompt` wins,
/// otherwise the preset's topic. Returns `None` when neither resolves
/// (misconfigured filter); callers should fail-open in that case.
pub fn resolve_topic_definition(
    custom_prompt: Option<&str>,
    persona: Option<&str>,
) -> Option<String> {
    if let Some(custom) = custom_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(custom.to_string());
    }
    persona
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|p| {
            let def = preset_definition(p);
            if def.is_none() {
                warn!(persona = %p, "Unknown scope persona — falling back to allow-all");
            }
            def.map(str::to_string)
        })
}

/// Canned response — explicit > preset default > generic fallback.
pub fn resolve_canned_response(explicit: Option<&str>, persona: Option<&str>) -> String {
    if let Some(s) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    persona
        .and_then(find_preset)
        .map(|p| p.canned.to_string())
        .unwrap_or_else(|| GENERIC_CANNED_RESPONSE.to_string())
}

/// Classify a single message against `topic_definition`. Always returns a
/// verdict; failures default to `on_topic = true` so a flaky classifier
/// cannot silence the bot.
pub async fn classify_scope(
    llm: &dyn LlmProvider,
    message: &str,
    topic_definition: &str,
    model: Option<&str>,
) -> ScopeClassification {
    let system = build_system_prompt(topic_definition);
    let outcome = llm
        .create_message(
            Some(&system),
            &[LlmMessage::user(message.to_string())],
            &[],
            model,
            Some(CLASSIFIER_MAX_TOKENS),
            true,
        )
        .await;

    match outcome {
        Ok(response) => parse_classification(&response).unwrap_or_else(|err| {
            warn!(
                error = %err,
                "Scope classifier returned unparseable response; failing open"
            );
            ScopeClassification::open("classifier returned malformed response")
        }),
        Err(e) => {
            warn!(error = %e, "Scope classifier LLM call failed; failing open");
            ScopeClassification::open("classifier call failed")
        }
    }
}

fn find_preset(name: &str) -> Option<&'static Preset> {
    PRESET_PERSONAS.iter().find(|p| p.name == name)
}

fn build_system_prompt(topic_definition: &str) -> String {
    format!(
        "You are a topic classifier. Decide whether the user's message is on-topic \
for a bot that handles the following:\n\n\
{topic_definition}\n\n\
Respond ONLY with a single JSON object of the exact shape \
{{\"on_topic\": true|false, \"reason\": \"<brief explanation>\"}} — no prose, no code fences. \
Keep the reason short (max 120 chars)."
    )
}

fn extract_text(response: &LlmResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn strip_fences(s: &str) -> &str {
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    body.strip_suffix("```").map(str::trim_end).unwrap_or(body)
}

fn first_braced_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end > start {
        Some(&s[start..=end])
    } else {
        None
    }
}

fn parse_classification(response: &LlmResponse) -> eyre::Result<ScopeClassification> {
    let text = extract_text(response);
    let body = strip_fences(&text);
    if let Ok(parsed) = serde_json::from_str::<ScopeClassification>(body) {
        return Ok(parsed);
    }
    if let Some(slice) = first_braced_object(body) {
        return serde_json::from_str(slice)
            .map_err(|e| eyre::eyre!("classifier JSON parse failed: {e}; body={body:?}"));
    }
    Err(eyre::eyre!(
        "classifier returned no JSON object; body={body:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_agent::llm::streaming::StreamEvent;
    use workforce_agent::llm::types::{LlmToolDef, StopReason, TokenUsage};

    struct FixedLlm {
        response: String,
        calls: AtomicUsize,
    }

    impl FixedLlm {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for FixedLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> eyre::Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.response.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> eyre::Result<()> {
            unreachable!("classifier never streams")
        }
    }

    struct FailingLlm;

    #[async_trait]
    impl LlmProvider for FailingLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> eyre::Result<LlmResponse> {
            Err(eyre::eyre!("simulated provider failure"))
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> eyre::Result<()> {
            unreachable!("classifier never streams")
        }
    }

    #[test]
    fn preset_definition_covers_all_known_names() {
        for name in preset_names() {
            assert!(
                preset_definition(name).is_some(),
                "preset_definition missing for {name}"
            );
        }
    }

    #[test]
    fn preset_definition_unknown_is_none() {
        assert!(preset_definition("nonexistent-persona").is_none());
        assert!(preset_definition("").is_none());
    }

    #[test]
    fn resolve_topic_prefers_custom_prompt() {
        let topic = resolve_topic_definition(Some("only python questions"), Some("faq"));
        assert_eq!(topic.as_deref(), Some("only python questions"));
    }

    #[test]
    fn resolve_topic_falls_back_to_persona() {
        let topic = resolve_topic_definition(None, Some("triage"));
        assert!(topic.unwrap().contains("bug reports"));
    }

    #[test]
    fn resolve_topic_blank_custom_falls_back() {
        let topic = resolve_topic_definition(Some("   "), Some("triage"));
        assert!(topic.is_some());
    }

    #[test]
    fn resolve_topic_unknown_persona_is_none() {
        assert!(resolve_topic_definition(None, Some("nope")).is_none());
        assert!(resolve_topic_definition(None, None).is_none());
    }

    #[test]
    fn canned_response_prefers_explicit() {
        assert_eq!(
            resolve_canned_response(Some("custom reply"), Some("triage")),
            "custom reply"
        );
    }

    #[test]
    fn canned_response_falls_back_to_preset() {
        let r = resolve_canned_response(None, Some("support"));
        assert!(r.contains("support bot"));
    }

    #[test]
    fn canned_response_blank_explicit_falls_back() {
        let r = resolve_canned_response(Some("  "), Some("faq"));
        assert!(r.contains("FAQ bot"));
    }

    #[test]
    fn canned_response_generic_when_no_persona() {
        let r = resolve_canned_response(None, None);
        assert_eq!(r, GENERIC_CANNED_RESPONSE);
    }

    #[test]
    fn canned_response_generic_when_persona_unknown() {
        let r = resolve_canned_response(None, Some("alien"));
        assert_eq!(r, GENERIC_CANNED_RESPONSE);
    }

    #[test]
    fn strip_fences_handles_plain() {
        assert_eq!(strip_fences("  {\"a\": 1}  "), "{\"a\": 1}");
    }

    #[test]
    fn strip_fences_handles_json_fence() {
        assert_eq!(strip_fences("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
    }

    #[test]
    fn strip_fences_handles_plain_fence() {
        assert_eq!(strip_fences("```\n{\"a\": 1}\n```"), "{\"a\": 1}");
    }

    #[test]
    fn first_braced_object_extracts_inner_json() {
        assert_eq!(
            first_braced_object("noise {\"a\": 1} trailing"),
            Some("{\"a\": 1}")
        );
    }

    #[tokio::test]
    async fn classify_on_topic_parses_clean_json() {
        let llm = FixedLlm::new(r#"{"on_topic": true, "reason": "matches"}"#);
        let v = classify_scope(&llm, "bug in login", "bug reports", None).await;
        assert!(v.on_topic);
        assert_eq!(v.reason, "matches");
        assert_eq!(llm.calls(), 1);
    }

    #[tokio::test]
    async fn classify_off_topic_parses_clean_json() {
        let llm = FixedLlm::new(r#"{"on_topic": false, "reason": "weather chat"}"#);
        let v = classify_scope(&llm, "what's the weather", "bug reports", None).await;
        assert!(!v.on_topic);
        assert_eq!(v.reason, "weather chat");
    }

    #[tokio::test]
    async fn classify_handles_code_fences() {
        let llm = FixedLlm::new("```json\n{\"on_topic\": false, \"reason\": \"off\"}\n```");
        let v = classify_scope(&llm, "hi", "bugs", None).await;
        assert!(!v.on_topic);
    }

    #[tokio::test]
    async fn classify_recovers_from_surrounding_prose() {
        let llm = FixedLlm::new(
            "Sure! Here is my verdict: {\"on_topic\": true, \"reason\": \"ok\"} done.",
        );
        let v = classify_scope(&llm, "hi", "bugs", None).await;
        assert!(v.on_topic);
        assert_eq!(v.reason, "ok");
    }

    #[tokio::test]
    async fn classify_fails_open_on_malformed_json() {
        let llm = FixedLlm::new("not json at all");
        let v = classify_scope(&llm, "hi", "bugs", None).await;
        assert!(v.on_topic, "should fail open");
    }

    #[tokio::test]
    async fn classify_fails_open_on_provider_error() {
        let llm = FailingLlm;
        let v = classify_scope(&llm, "hi", "bugs", None).await;
        assert!(v.on_topic);
    }

    #[test]
    fn classification_round_trips() {
        let v = ScopeClassification {
            on_topic: false,
            reason: "off".to_string(),
            failed_open: false,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: ScopeClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}

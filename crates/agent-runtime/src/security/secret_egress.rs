//! Defence-in-depth credential redaction at the LLM-client boundary.
//!
//! Last line of defence against shipping a cleartext credential to the
//! model. By the time text reaches here the ingress tokenizer should
//! have already replaced any detected credential with a marker — so on
//! a healthy path this is a no-op.
//!
//! Markers are intentionally **left untouched**: they're exactly what
//! the model is supposed to see so it can route them to allowlisted
//! secret-sink tools (vault.set, …) where the substitution layer
//! resolves them back to cleartext. Rendering markers here would break
//! that contract. Cleartext that slipped past tokenization is replaced
//! with `first6...last4` via [`workforce_common::redact::redact_secrets`]
//! — minting a marker is pointless because we have no way to register
//! it in the per-session cache from this depth.
//!
//! Every redaction emits a `sentinel.audit` warning, since each one
//! marks an ingress gap that should be wired into the tokenizer.
//!
//! [`SanitizingProvider`] wraps any [`LlmProvider`] to apply the
//! sanitizer transparently to every request — both streaming and
//! non-streaming. The runner wraps its provider with this once at
//! construction, so every call site is covered without per-site code.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;

use crate::llm::provider::LlmProvider;
use crate::llm::streaming::StreamEvent;
use crate::llm::types::{ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role};

pub fn sanitize_messages(messages: &mut [LlmMessage]) {
    for msg in messages.iter_mut() {
        let role = &msg.role;
        for block in msg.content.iter_mut() {
            sanitize_block(role, block);
        }
    }
}

fn sanitize_block(role: &Role, block: &mut ContentBlock) {
    match block {
        ContentBlock::Text { text } => maybe_redact(text, role, "text"),
        ContentBlock::Thinking { thinking, .. } => maybe_redact(thinking, role, "thinking"),
        ContentBlock::ToolResult { content, .. } => maybe_redact(content, role, "tool_result"),
        ContentBlock::ToolUse { input, .. } => sanitize_value(input, role),
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => {}
    }
}

fn sanitize_value(value: &mut serde_json::Value, role: &Role) {
    match value {
        serde_json::Value::String(s) => maybe_redact(s, role, "tool_use"),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| sanitize_value(v, role)),
        serde_json::Value::Array(arr) => arr.iter_mut().for_each(|v| sanitize_value(v, role)),
        _ => {}
    }
}

fn maybe_redact(text: &mut String, role: &Role, kind: &'static str) {
    redact(text, Some(role), kind);
}

/// Redact cleartext credentials in `text` bound for a model by a route that
/// bypasses [`SanitizingProvider`] — an MCP client's model, reached through a
/// tool result. `kind` names the route in the audit warning.
pub fn redact_text(text: &mut String, kind: &'static str) {
    redact(text, None, kind);
}

fn redact(text: &mut String, role: Option<&Role>, kind: &'static str) {
    if !workforce_common::redact::contains_secret(text) {
        return;
    }
    tracing::warn!(
        target: "sentinel.audit",
        ?role,
        content_kind = %kind,
        "cleartext credential reached LLM boundary — redacting (upstream tokenizer gap)"
    );
    *text = workforce_common::redact::redact_secrets(text);
}

/// Sanitize the system prompt the same way as message content. The system
/// prompt bypasses [`sanitize_messages`] entirely (it is a separate argument,
/// not a [`LlmMessage`]), so a credential baked into it — a persona/config that
/// interpolated a secret — would otherwise reach the model uncredacted. Markers
/// are token-shaped but not secret-shaped, so they pass through untouched.
fn sanitize_system_prompt(system_prompt: Option<&str>) -> Option<String> {
    system_prompt.map(|sp| {
        if workforce_common::redact::contains_secret(sp) {
            tracing::warn!(
                target: "sentinel.audit",
                content_kind = "system_prompt",
                "cleartext credential reached LLM boundary in system prompt — redacting"
            );
            workforce_common::redact::redact_secrets(sp)
        } else {
            sp.to_string()
        }
    })
}

// ───────────────────────────────────────────────────────────────────────
// Provider wrapper
// ───────────────────────────────────────────────────────────────────────

/// [`LlmProvider`] wrapper that runs [`sanitize_messages`] on the
/// request before delegating to the inner provider. Wrap once at
/// construction time and every call site (streaming or not, including
/// compaction and sub-agent paths) is covered without per-site code.
pub struct SanitizingProvider {
    inner: Arc<dyn LlmProvider>,
}

impl SanitizingProvider {
    pub fn wrap(inner: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl LlmProvider for SanitizingProvider {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let mut owned = messages.to_vec();
        sanitize_messages(&mut owned);
        let system_prompt = sanitize_system_prompt(system_prompt);
        self.inner
            .create_message(
                system_prompt.as_deref(),
                &owned,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let mut owned = messages.to_vec();
        sanitize_messages(&mut owned);
        let system_prompt = sanitize_system_prompt(system_prompt);
        self.inner
            .create_message_stream(
                system_prompt.as_deref(),
                &owned,
                tools,
                model,
                max_tokens,
                enable_cache,
                tx,
            )
            .await
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.inner.context_floor(model).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ContentBlock, LlmMessage, Role};

    fn assistant_text(text: &str) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn redacts_cleartext_credential_in_text_block() {
        let mut msgs = vec![assistant_text(concat!(
            "leaked: sk-ant",
            "-oat01-abcdefghijklmnopqrstuvwxyz1234567890"
        ))];
        sanitize_messages(&mut msgs);
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text block");
        };
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(text.contains("sk-ant...7890"));
    }

    #[test]
    fn sanitizer_keeps_reasoning_signature() {
        let signature = format!(
            "{}v1:{{\"id\":\"rs_1\",\"enc\":\"gAAAAABqu2q5gPykw7cY4RRGc5TPI23oXuKG\"}}",
            crate::llm::types::OAI_REASONING_PREFIX
        );
        let mut msgs = vec![LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: concat!("key sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz1234567890")
                    .into(),
                signature: Some(signature.clone()),
            }],
        }];
        sanitize_messages(&mut msgs);
        let ContentBlock::Thinking {
            thinking,
            signature: kept,
        } = &msgs[0].content[0]
        else {
            panic!("expected thinking block");
        };
        assert!(!thinking.contains("abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(kept.as_deref(), Some(signature.as_str()));
    }

    #[test]
    fn sanitizes_credential_in_system_prompt() {
        // The system prompt bypasses sanitize_messages, so a baked-in secret
        // must be caught by the dedicated sanitizer.
        let out = sanitize_system_prompt(Some(concat!(
            "You are an agent. Token: sk-ant",
            "-oat01-abcdefghijklmnopqrstuvwxyz1234567890"
        )))
        .unwrap();
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(out.contains("sk-ant...7890"));
    }

    #[test]
    fn system_prompt_clean_and_none_pass_through() {
        assert_eq!(sanitize_system_prompt(None), None);
        let clean = "You are a helpful agent with no secrets.";
        assert_eq!(sanitize_system_prompt(Some(clean)).as_deref(), Some(clean));
    }

    #[test]
    fn passes_clean_text_through_unchanged() {
        let original = "ordinary message with no credentials";
        let mut msgs = vec![assistant_text(original)];
        sanitize_messages(&mut msgs);
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text block");
        };
        assert_eq!(text, original);
    }

    #[test]
    fn preserves_markers_so_llm_can_pass_them_to_sink_tools() {
        // Critical contract: markers must survive the egress sanitizer
        // unchanged. The LLM uses the marker text verbatim when calling
        // vault.set / system.add_account / slack.connect; the
        // tool-boundary substitution layer then resolves it. Rendering
        // markers here would break the entire substitution pipeline.
        let original =
            "saved [[secret:00000000000000000000000000000000 type=GITHUB_PAT preview=ghp_...AbCd]] for you";
        let mut msgs = vec![assistant_text(original)];
        sanitize_messages(&mut msgs);
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text block");
        };
        assert_eq!(text, original);
    }

    #[test]
    fn redacts_credential_inside_tool_use_input_string() {
        let mut msgs = vec![LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "vault.set".into(),
                input: serde_json::json!({
                    "path": "skills.gh.token",
                    "value": concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234")
                }),
                signature: None,
            }],
        }];
        sanitize_messages(&mut msgs);
        let ContentBlock::ToolUse { input, .. } = &msgs[0].content[0] else {
            panic!("expected tool use block");
        };
        let val = input["value"].as_str().unwrap();
        assert!(!val.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(val.contains("ghp_AB...1234"));
    }

    #[test]
    fn redacts_credential_in_tool_result_content() {
        let mut msgs = vec![LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: concat!(
                    "{\"page\":\"the api key is sk-ant",
                    "-oat01-abcdefghijklmnopqrstuvwxyz1234567890 thanks\"}"
                )
                .into(),
                is_error: None,
                loaded: None,
            }],
        }];
        sanitize_messages(&mut msgs);
        let ContentBlock::ToolResult { content, .. } = &msgs[0].content[0] else {
            panic!("expected tool result block");
        };
        assert!(!content.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn leaves_non_text_blocks_alone() {
        let mut msgs = vec![LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::image_base64("image/png", "base64data".into())],
        }];
        sanitize_messages(&mut msgs);
        // No panic, no change.
        match &msgs[0].content[0] {
            ContentBlock::Image { source } => assert_eq!(source.data, "base64data"),
            _ => panic!("expected image block"),
        }
    }
}

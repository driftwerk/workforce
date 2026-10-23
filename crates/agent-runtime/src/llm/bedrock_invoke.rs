//! Bedrock `/invoke` and `/invoke-with-response-stream` for Anthropic Claude
//! models. The wire format is the Anthropic Messages API with two adjustments:
//! `model` is moved into the URL path and `anthropic_version` is set in the
//! body. No `anthropic-beta` header — Bedrock gates beta features through the
//! `anthropic_version` value.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::anthropic::{build_system_blocks, CacheControl, Effort};
use super::bedrock_capabilities::{self, ThinkingMode};
use super::provider::{tool_deferral, ToolDeferral};
use super::types::{
    AnthropicToolDef, ContentBlock, LlmMessage, LlmToolDef, Loaded, StopReason, TokenUsage,
};

const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Build the JSON body for a Bedrock Invoke call against an Anthropic model.
pub fn build_body(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
    model_id: &str,
    enable_cache: bool,
    thinking_budget: Option<u32>,
) -> Value {
    let messages: &[LlmMessage] = &super::image_cap::anthropic_shaped(messages);
    let cache = enable_cache && bedrock_capabilities::caches(model_id);
    let thinking = bedrock_capabilities::thinking(model_id);
    let deferral = tool_deferral().filter(|_| bedrock_capabilities::defers_tools(model_id));
    let anthropic_tools = declared_tools(tools, deferral.as_ref(), &referenced(messages), cache);
    let deferred: BTreeSet<&str> = anthropic_tools
        .iter()
        .filter(|t| t.defer_loading)
        .map(|t| t.name.as_str())
        .collect();
    let wire: Vec<Value> = messages
        .iter()
        .map(|m| wire_message(&sanitize_message(m), &deferred))
        .collect();
    let mut body = json!({
        "anthropic_version": ANTHROPIC_VERSION,
        "max_tokens": max_tokens,
        "messages": super::cache_utils::messages_with_cache_breakpoint(&wire, cache),
    });

    if let Some(prompt) = system_prompt {
        let blocks = build_system_blocks(prompt, cache);
        body["system"] = serde_json::to_value(&blocks).unwrap_or_else(|_| json!(prompt));
    }

    if !anthropic_tools.is_empty() {
        body["tools"] = serde_json::to_value(&anthropic_tools).unwrap_or(Value::Null);
    }

    if let (ThinkingMode::ToggleWithBudget, Some(budget)) = (thinking, thinking_budget) {
        if bedrock_capabilities::requires_adaptive_thinking(model_id) {
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            body["output_config"] = json!({ "effort": Effort::from_budget(budget).as_str() });
        } else if let Some(b) = bedrock_capabilities::clamp_thinking_budget(budget, max_tokens) {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": b });
        }
    }

    body
}

/// Parse the JSON response from a Bedrock Invoke call against an Anthropic
/// model into the unified response shape used by [`super::types::LlmResponse`].
pub fn parse_response(
    value: Value,
    model: &str,
) -> eyre::Result<(Vec<ContentBlock>, StopReason, TokenUsage, String)> {
    let content: Vec<ContentBlock> = serde_json::from_value(value["content"].clone())
        .map_err(|e| eyre::eyre!("Failed to parse Anthropic-on-Bedrock content: {e}"))?;
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
    let resolved_model = value["model"].as_str().unwrap_or(model).to_string();
    Ok((content, stop_reason, usage, resolved_model))
}

/// Every tool a surviving tool result loaded.
fn referenced(messages: &[LlmMessage]) -> BTreeSet<&str> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                loaded: Some(loaded),
                ..
            } => Some(&loaded.tools),
            _ => None,
        })
        .flatten()
        .map(String::as_str)
        .collect()
}

/// The tools a request declares: the declared ones in order, with the cache
/// breakpoint on the last of them, then — on a model that defers natively —
/// the catalogued tools, deferred and ordered by name so the array stays the
/// same for the whole turn. A loaded tool whose loading result was compacted
/// away is declared in full, since nothing would show the model its
/// definition otherwise.
fn declared_tools(
    tools: &[LlmToolDef],
    deferral: Option<&ToolDeferral>,
    referenced: &BTreeSet<&str>,
    cache: bool,
) -> Vec<AnthropicToolDef> {
    let (stranded, deferred): (Vec<&LlmToolDef>, Vec<&LlmToolDef>) = deferral
        .iter()
        .flat_map(|d| {
            d.deferred
                .iter()
                .map(move |t| (t, d.loaded.contains(&t.name)))
        })
        .fold(
            (Vec::new(), Vec::new()),
            |(mut stranded, mut deferred), (t, loaded)| {
                match loaded && !referenced.contains(t.name.as_str()) {
                    true => stranded.push(t),
                    false => deferred.push(t),
                }
                (stranded, deferred)
            },
        );
    let mut resident: Vec<AnthropicToolDef> = tools
        .iter()
        .chain(stranded)
        .map(AnthropicToolDef::from)
        .collect();
    if cache {
        if let Some(last) = resident.last_mut() {
            last.cache_control = Some(CacheControl::ephemeral());
        }
    }
    let mut deferred: Vec<AnthropicToolDef> = deferred
        .into_iter()
        .map(|t| AnthropicToolDef {
            defer_loading: true,
            ..AnthropicToolDef::from(t)
        })
        .collect();
    deferred.sort_by(|a, b| a.name.cmp(&b.name));
    resident.into_iter().chain(deferred).collect()
}

/// A message on the wire. A tool result that loaded deferred tools carries
/// their references and nothing else — the API refuses text beside a
/// reference — so its note follows the results as a text block.
fn wire_message(m: &LlmMessage, deferred: &BTreeSet<&str>) -> Value {
    let references = |loaded: &Loaded| -> Vec<Value> {
        loaded
            .tools
            .iter()
            .filter(|name| deferred.contains(name.as_str()))
            .map(|name| json!({ "type": "tool_reference", "tool_name": name }))
            .collect()
    };
    let (results, words): (Vec<Value>, Vec<Value>) = m
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                loaded: Some(loaded),
                ..
            } if !references(loaded).is_empty() => (
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": references(loaded),
                }),
                Some(json!({ "type": "text", "text": loaded.note })),
            ),
            other => (serde_json::to_value(other).unwrap_or(Value::Null), None),
        })
        .fold(
            (Vec::new(), Vec::new()),
            |(mut results, mut words), (block, text)| {
                results.push(block);
                words.extend(text);
                (results, words)
            },
        );
    json!({
        "role": m.role,
        "content": results.into_iter().chain(words).collect::<Vec<_>>(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{Role, OAI_REASONING_PREFIX};

    fn photos(n: usize) -> Vec<LlmMessage> {
        let image = ContentBlock::Image {
            source: crate::llm::types::ImageSource {
                source_type: "base64".into(),
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            },
        };
        vec![LlmMessage {
            role: Role::User,
            content: std::iter::repeat_n(image, n)
                .chain([ContentBlock::Text {
                    text: "what changed?".into(),
                }])
                .collect(),
        }]
    }

    #[test]
    fn invoke_and_converse_bodies_keep_to_the_image_limit() {
        let messages = photos(25);
        let invoke = build_body(
            None,
            &messages,
            &[],
            1024,
            "anthropic.claude-sonnet-4-6",
            false,
            None,
        )
        .to_string();
        assert_eq!(invoke.matches(r#""type":"image""#).count(), 20);
        let converse = super::super::bedrock_converse::build_body(
            None,
            &messages,
            &[],
            1024,
            "amazon.nova-pro-v1:0",
            false,
            None,
        )
        .to_string();
        assert_eq!(converse.matches(r#""image":{"#).count(), 20);
        assert_eq!(converse.matches("[image removed").count(), 5);
    }

    #[test]
    fn foreign_reasoning_stripped_on_anthropic_wire() {
        let messages = vec![
            user("weather in Porto?"),
            LlmMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "**Planning**".into(),
                        signature: Some(format!("{OAI_REASONING_PREFIX}v1:{{}}")),
                    },
                    ContentBlock::tool_use(
                        "call_1".into(),
                        "weather_get".into(),
                        serde_json::json!({"city": "Porto"}),
                    ),
                ],
            },
        ];
        let bodies = [
            build_body(
                None,
                &messages,
                &[],
                1024,
                "anthropic.claude-sonnet-4-6",
                false,
                None,
            ),
            super::super::bedrock_converse::build_body(
                None,
                &messages,
                &[],
                1024,
                "zai.glm-4.7",
                false,
                None,
            ),
        ];
        for body in bodies {
            let wire = body.to_string();
            assert!(!wire.contains(OAI_REASONING_PREFIX), "{wire}");
            assert!(!wire.contains("**Planning**"), "{wire}");
            assert!(wire.contains("call_1"), "{wire}");
        }
    }

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

    fn tool_result(id: &str, loaded: &[&str]) -> LlmMessage {
        let note = format!("Loaded: {}.", loaded.join(", "));
        LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: format!("{note}\n\n[definitions]"),
                is_error: None,
                loaded: Some(Loaded {
                    tools: loaded.iter().map(|name| name.to_string()).collect(),
                    note,
                }),
            }],
        }
    }

    fn deferral() -> ToolDeferral {
        ToolDeferral {
            deferred: std::sync::Arc::new(vec![tool("vault_set"), tool("scheduler")]),
            loaded: std::sync::Arc::new(BTreeSet::from(["vault_set".to_string()])),
        }
    }

    fn declared() -> Vec<LlmToolDef> {
        vec![tool("memory"), tool("tool_load"), tool("tool_call")]
    }

    async fn body_deferring(model: &str, messages: &[LlmMessage]) -> Value {
        crate::llm::provider::with_tool_deferral(Some(deferral()), async {
            build_body(Some("sys"), messages, &declared(), 4096, model, true, None)
        })
        .await
    }

    fn tool_shape(body: &Value) -> Vec<(String, bool, bool)> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().to_string(),
                    t["defer_loading"] == true,
                    t.get("cache_control").is_some(),
                )
            })
            .collect()
    }

    fn shape(expected: &[(&str, bool, bool)]) -> Vec<(String, bool, bool)> {
        expected
            .iter()
            .map(|(name, deferred, cached)| (name.to_string(), *deferred, *cached))
            .collect()
    }

    #[tokio::test]
    async fn a_model_that_defers_declares_every_catalogued_tool_deferred_whatever_was_loaded() {
        let messages = [
            tool_result("tu_1", &["vault_set"]),
            tool_result("tu_2", &["ghost"]),
        ];
        let body = body_deferring("eu.anthropic.claude-sonnet-5", &messages).await;
        let expected = shape(&[
            ("memory", false, false),
            ("tool_load", false, false),
            ("tool_call", false, true),
            ("scheduler", true, false),
            ("vault_set", true, false),
        ]);
        assert_eq!(tool_shape(&body), expected);
        assert_eq!(
            tool_shape(&body_deferring("eu.anthropic.claude-sonnet-5", &messages[1..]).await),
            shape(&[
                ("memory", false, false),
                ("tool_load", false, false),
                ("tool_call", false, false),
                ("vault_set", false, true),
                ("scheduler", true, false),
            ]),
            "a loaded tool whose loading result is gone is shown in full"
        );
        let first = &body["messages"][0]["content"];
        assert_eq!(
            first[0]["content"],
            json!([{ "type": "tool_reference", "tool_name": "vault_set" }])
        );
        assert_eq!(
            first[1],
            json!({ "type": "text", "text": "Loaded: vault_set." }),
            "the note follows the references, never the definitions"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["content"], "Loaded: ghost.\n\n[definitions]",
            "a reference to an undeclared tool would be refused"
        );
    }

    #[tokio::test]
    async fn a_load_leaves_the_tools_array_byte_for_byte_the_same() {
        let before = ToolDeferral {
            loaded: std::sync::Arc::default(),
            ..deferral()
        };
        let body = |deferral: ToolDeferral, messages: Vec<LlmMessage>| async move {
            crate::llm::provider::with_tool_deferral(Some(deferral), async {
                build_body(
                    Some("sys"),
                    &messages,
                    &declared(),
                    4096,
                    "eu.anthropic.claude-sonnet-5",
                    true,
                    None,
                )
            })
            .await["tools"]
                .clone()
        };
        assert_eq!(
            body(before, vec![user("hi")]).await,
            body(
                deferral(),
                vec![user("hi"), tool_result("tu_1", &["vault_set"])]
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_model_that_does_not_defer_is_sent_the_declared_tools_and_the_definitions() {
        let body = body_deferring(
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            &[tool_result("tu_1", &["vault_set"])],
        )
        .await;
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["memory", "tool_load", "tool_call"]);
        assert!(!body.to_string().contains("defer_loading"));
        assert_eq!(
            body["messages"][0]["content"][0]["content"],
            "Loaded: vault_set.\n\n[definitions]"
        );
    }

    #[test]
    fn without_a_deferral_in_scope_nothing_is_deferred() {
        let body = build_body(
            None,
            &[tool_result("tu_1", &["vault_set"])],
            &declared(),
            4096,
            "eu.anthropic.claude-sonnet-5",
            false,
            None,
        );
        assert!(!body.to_string().contains("defer_loading"));
        assert!(!body.to_string().contains("tool_reference"));
    }

    const OPUS: &str = "anthropic.claude-opus-4-7-20251015-v1:0";
    const SONNET: &str = "anthropic.claude-sonnet-4-6";
    const HAIKU3: &str = "anthropic.claude-3-haiku-20240307-v1:0";

    #[test]
    fn body_omits_model_field_and_includes_anthropic_version() {
        let body = build_body(Some("sys"), &[user("hi")], &[], 4096, OPUS, false, None);
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"].as_str(), Some(ANTHROPIC_VERSION));
        assert_eq!(body["max_tokens"].as_u64(), Some(4096));
    }

    #[test]
    fn body_with_cache_uses_cache_control_blocks() {
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
        assert!(tools.last().unwrap()["cache_control"].is_object());
        // The last message's last block carries the conversation breakpoint,
        // and the whole request stays within the API's 4-breakpoint budget.
        let last_content = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(last_content.last().unwrap()["cache_control"].is_object());
        let breakpoints = serde_json::to_string(&body)
            .unwrap()
            .matches("cache_control")
            .count();
        assert!(
            breakpoints <= 4,
            "{breakpoints} breakpoints exceed the API cap"
        );
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
    fn body_cache_skipped_for_non_caching_anthropic_model() {
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
    fn body_thinking_with_budget_emitted_when_supported() {
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

    #[test]
    fn body_opus_4_7_no_thinking_without_budget() {
        let body = build_body(None, &[user("hi")], &[], 16384, OPUS, false, None);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn body_thinking_dropped_when_unsupported() {
        let body = build_body(None, &[user("hi")], &[], 16384, HAIKU3, false, Some(8000));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn body_omits_tools_when_empty() {
        let body = build_body(None, &[user("hi")], &[], 1024, OPUS, false, None);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_sanitises_non_object_tool_input() {
        let msg = LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "x".to_string(),
                input: Value::Null,
                signature: None,
            }],
        };
        let body = build_body(None, &[msg], &[], 1024, OPUS, false, None);
        let messages = body["messages"].as_array().unwrap();
        let block = &messages[0]["content"][0];
        assert!(block["input"].is_object());
    }

    #[test]
    fn body_strips_constraint_keys_from_tool_schemas() {
        let tool = LlmToolDef {
            name: "x".to_string(),
            description: "y".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string" } },
                "oneOf": [{ "required": ["a"] }],
            }),
        };
        let body = build_body(None, &[user("hi")], &[tool], 1024, OPUS, false, None);
        let schema = &body["tools"][0]["input_schema"];
        assert!(schema.get("oneOf").is_none());
        assert!(schema.get("allOf").is_none());
        assert!(schema.get("anyOf").is_none());
        assert_eq!(schema["properties"]["a"]["type"], json!("string"));
    }

    #[test]
    fn parse_response_extracts_fields() {
        let resp = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "model": "us.anthropic.claude-sonnet-4-6-20251015-v1:0",
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
        assert_eq!(model, "us.anthropic.claude-sonnet-4-6-20251015-v1:0");
    }
}

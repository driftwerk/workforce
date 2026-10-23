//! Shared cache_control block building for OpenAI-compatible providers.

use super::anthropic::{cache_segments, segment_is_cached};

/// Build content blocks with cache_control annotations.
///
/// Splits the prompt on the cache-break marker; the first two segments get
/// `cache_control: { type: "ephemeral" }` so the provider caches them across
/// rounds and turns. A lone segment is cached whole. Works with OpenRouter
/// (Anthropic + Gemini) and direct Gemini.
pub fn build_cache_blocks(prompt: &str) -> Vec<serde_json::Value> {
    let make_block = |text: &str, cached: bool| {
        let mut block = serde_json::json!({ "type": "text", "text": text });
        if cached {
            block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
        }
        block
    };

    cache_segments(prompt)
        .into_iter()
        .enumerate()
        .map(|(i, text)| make_block(text, segment_is_cached(i)))
        .collect()
}

/// Content-block types the Anthropic API accepts `cache_control` on.
/// Thinking blocks reject it.
const CACHEABLE_BLOCK_TYPES: [&str; 5] = ["text", "tool_result", "tool_use", "image", "document"];

/// The message that carries the request's cache breakpoint: the last one
/// unless the turn declared a shorter recurring prefix; `None` for no
/// breakpoint.
fn breakpoint_index(
    enable_cache: bool,
    declared_prefix: Option<usize>,
    len: usize,
) -> Option<usize> {
    enable_cache
        .then(|| declared_prefix.map_or(len, |n| n.min(len)))
        .filter(|&n| n > 0)
        .map(|n| n - 1)
}

/// Index of the message the round's breakpoint goes on, honouring the
/// recurring prefix the runner declared through
/// [`super::provider::with_cached_prefix`]. Every wire that places a
/// message-level breakpoint resolves its target here.
pub fn cache_breakpoint_message(enable_cache: bool, len: usize) -> Option<usize> {
    breakpoint_index(enable_cache, super::provider::cached_prefix_len(), len)
}

/// Serialize a message slice, placing one `cache_control: ephemeral`
/// breakpoint on the last cacheable content block of the message
/// [`cache_breakpoint_message`] names.
///
/// Without this, the cacheable prefix ends at `[system + tools]` and the
/// whole conversation — including tens of rounds of accumulated tool
/// results in an agentic loop — is re-billed at the full input rate every
/// round. With it, everything up to the previous round is a cache read, and
/// on round 1 of a turn with history the breakpoint closes the replayed
/// prefix so the next turn reads it. Total breakpoints stay ≤ 4 (system uses
/// ≤ 2, last tool def 1, this 1).
pub fn messages_with_cache_breakpoint<T: serde::Serialize>(
    messages: &[T],
    enable_cache: bool,
) -> Vec<serde_json::Value> {
    let mut values: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
        .collect();
    if let Some(block) = cache_breakpoint_message(enable_cache, values.len())
        .and_then(|target| values[target].get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|blocks| {
            blocks.iter_mut().rev().find(|b| {
                b.get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| CACHEABLE_BLOCK_TYPES.contains(&t))
            })
        })
    {
        block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::anthropic::CACHE_BREAK_MARKER;

    #[test]
    fn single_block_without_marker() {
        let blocks = build_cache_blocks("Hello system");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"].as_str().unwrap(), "Hello system");
        assert!(blocks[0]["cache_control"].is_object());
    }

    #[test]
    fn two_segments_are_both_cached() {
        let prompt = format!("Static{CACHE_BREAK_MARKER}Semi");
        let blocks = build_cache_blocks(&prompt);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0]["cache_control"].is_object());
        assert!(blocks[1]["cache_control"].is_object());
        assert_eq!(blocks[0]["text"].as_str().unwrap(), "Static");
        assert_eq!(blocks[1]["text"].as_str().unwrap(), "Semi");
    }

    #[test]
    fn empty_static_part_caches_dynamic() {
        let prompt = format!("{CACHE_BREAK_MARKER}Dynamic only");
        let blocks = build_cache_blocks(&prompt);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0]["cache_control"].is_object());
        assert_eq!(blocks[0]["text"].as_str().unwrap(), "Dynamic only");
    }

    #[test]
    fn three_segments_cache_first_two() {
        let prompt = format!("Static{CACHE_BREAK_MARKER}Semi{CACHE_BREAK_MARKER}Dynamic");
        let blocks = build_cache_blocks(&prompt);
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0]["cache_control"].is_object());
        assert!(blocks[1]["cache_control"].is_object());
        assert!(blocks[2].get("cache_control").is_none());
        assert_eq!(blocks[2]["text"].as_str().unwrap(), "Dynamic");
    }

    fn msg(role: &str, blocks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "role": role, "content": blocks })
    }

    #[test]
    fn breakpoint_lands_on_last_cacheable_block_of_last_message() {
        let messages = vec![
            msg(
                "user",
                serde_json::json!([{ "type": "text", "text": "hi" }]),
            ),
            msg(
                "user",
                serde_json::json!([
                    { "type": "tool_result", "tool_use_id": "a", "content": "x" },
                    { "type": "tool_result", "tool_use_id": "b", "content": "y" },
                ]),
            ),
        ];
        let out = messages_with_cache_breakpoint(&messages, true);
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert!(out[1]["content"][0].get("cache_control").is_none());
        assert_eq!(
            out[1]["content"][1]["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn breakpoint_skips_thinking_blocks() {
        let messages = vec![msg(
            "assistant",
            serde_json::json!([
                { "type": "text", "text": "reasoned answer" },
                { "type": "thinking", "thinking": "...", "signature": "s" },
            ]),
        )];
        let out = messages_with_cache_breakpoint(&messages, true);
        assert!(out[0]["content"][1].get("cache_control").is_none());
        assert!(out[0]["content"][0].get("cache_control").is_some());
    }

    #[test]
    fn breakpoint_disabled_leaves_messages_untouched() {
        let messages = vec![msg(
            "user",
            serde_json::json!([{ "type": "text", "text": "hi" }]),
        )];
        let out = messages_with_cache_breakpoint(&messages, false);
        assert!(out[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn breakpoint_index_defaults_to_the_last_message() {
        assert_eq!(breakpoint_index(true, None, 3), Some(2));
        assert_eq!(breakpoint_index(false, None, 3), None);
        assert_eq!(breakpoint_index(false, Some(2), 3), None);
        assert_eq!(breakpoint_index(true, None, 0), None);
    }

    #[test]
    fn breakpoint_index_follows_the_declared_prefix() {
        assert_eq!(breakpoint_index(true, Some(2), 5), Some(1));
        assert_eq!(breakpoint_index(true, Some(0), 5), None);
        assert_eq!(breakpoint_index(true, Some(9), 5), Some(4));
    }

    #[tokio::test]
    async fn breakpoint_moves_to_the_end_of_the_declared_prefix() {
        let messages = vec![
            msg(
                "user",
                serde_json::json!([{ "type": "text", "text": "q1" }]),
            ),
            msg(
                "assistant",
                serde_json::json!([
                    { "type": "text", "text": "a1" },
                    { "type": "thinking", "thinking": "...", "signature": "s" },
                ]),
            ),
            msg(
                "user",
                serde_json::json!([{ "type": "text", "text": "q2" }]),
            ),
        ];
        let out = crate::llm::provider::with_cached_prefix(Some(2), async {
            messages_with_cache_breakpoint(&messages, true)
        })
        .await;
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert_eq!(
            out[1]["content"][0]["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
        assert!(out[1]["content"][1].get("cache_control").is_none());
        assert!(out[2]["content"][0].get("cache_control").is_none());
    }
}

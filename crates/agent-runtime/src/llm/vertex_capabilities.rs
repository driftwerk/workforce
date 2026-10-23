//! Per-model capability matrix for Anthropic Claude on GCP Vertex AI.
//!
//! Mirrors [`super::bedrock_capabilities`] in shape but keyed off the Vertex
//! model-ID format (`claude-opus-4-7@20251015`) instead of Bedrock's
//! (`anthropic.claude-opus-4-7-20251015-v1:0`). `caches` and `thinking` are
//! pure prefix-table lookups — unknown families default to `false` /
//! [`ThinkingMode::None`], which is the safe choice (the model just won't
//! receive cache-control annotations or a thinking block).

use super::bedrock_capabilities::ThinkingMode;

#[derive(Debug, Clone, Copy)]
struct FamilyEntry {
    prefix: &'static str,
    caches: bool,
    thinking: ThinkingMode,
}

const FAMILIES: &[FamilyEntry] = &[
    FamilyEntry {
        prefix: "claude-opus-4",
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
    },
    FamilyEntry {
        prefix: "claude-sonnet-4",
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
    },
    FamilyEntry {
        prefix: "claude-haiku-4",
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
    },
    FamilyEntry {
        prefix: "claude-3-5-haiku",
        caches: true,
        thinking: ThinkingMode::None,
    },
    FamilyEntry {
        prefix: "claude-3-haiku",
        caches: false,
        thinking: ThinkingMode::None,
    },
    FamilyEntry {
        prefix: "claude-",
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
    },
];

fn lookup(model_id: &str) -> Option<&'static FamilyEntry> {
    FAMILIES.iter().find(|f| model_id.starts_with(f.prefix))
}

/// Whether the given Vertex Claude model supports prompt caching.
pub fn caches(model_id: &str) -> bool {
    lookup(model_id).map(|f| f.caches).unwrap_or(false)
}

/// How the given Vertex Claude model exposes extended reasoning.
pub fn thinking(model_id: &str) -> ThinkingMode {
    lookup(model_id)
        .map(|f| f.thinking)
        .unwrap_or(ThinkingMode::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caching_matrix_matches_snapshot() {
        let cases = &[
            // Current-gen Vertex Claude ids are unversioned (no @date).
            ("claude-opus-4-8", true),
            // sonnet-5 falls through to the `claude-` catch-all family
            ("claude-sonnet-5", true),
            ("claude-opus-4-7@20251015", true),
            ("claude-opus-4-6@20250929", true),
            ("claude-sonnet-4-6@20251015", true),
            ("claude-sonnet-4-5@20250929", true),
            ("claude-haiku-4-5@20251001", true),
            ("claude-3-5-haiku@20241022", true),
            ("claude-3-haiku@20240307", false),
            ("claude-future-mystery@20991231", true),
            ("unknown-model@1", false),
            ("", false),
        ];
        for (id, expected) in cases {
            assert_eq!(caches(id), *expected, "id={id}");
        }
    }

    #[test]
    fn thinking_matrix_matches_snapshot() {
        let cases = &[
            ("claude-opus-4-7@20251015", ThinkingMode::ToggleWithBudget),
            ("claude-opus-4-6@20250929", ThinkingMode::ToggleWithBudget),
            ("claude-sonnet-4-6@20251015", ThinkingMode::ToggleWithBudget),
            ("claude-sonnet-4-5@20250929", ThinkingMode::ToggleWithBudget),
            ("claude-haiku-4-5@20251001", ThinkingMode::ToggleWithBudget),
            ("claude-3-5-haiku@20241022", ThinkingMode::None),
            ("claude-3-haiku@20240307", ThinkingMode::None),
            ("claude-future@1", ThinkingMode::ToggleWithBudget),
            ("unknown-model@1", ThinkingMode::None),
        ];
        for (id, expected) in cases {
            assert_eq!(thinking(id), *expected, "id={id}");
        }
    }

    #[test]
    fn fallback_prefix_does_not_eclipse_haiku_3() {
        assert!(!caches("claude-3-haiku@20240307"));
        assert_eq!(thinking("claude-3-haiku@20240307"), ThinkingMode::None);
    }
}

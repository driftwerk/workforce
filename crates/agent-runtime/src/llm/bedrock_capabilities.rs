//! Per-model Bedrock capability matrix.
//!
//! `wire_format_for`, `caches`, `thinking`, and `toggle_shape` are pure
//! prefix-table lookups over [`FAMILIES`]: the first row whose prefix matches
//! the (region-stripped) model id wins. Adding support for a new model means
//! adding a row to that table; nothing else dispatches.

/// Which Bedrock API a model should be invoked through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// Anthropic Messages format via `/invoke` / `/invoke-with-response-stream`.
    Invoke,
    /// Bedrock-unified Converse format via `/converse` / `/converse-stream`.
    Converse,
    /// OpenAI Chat-Completions format over Bedrock's `/invoke` endpoint.
    ///
    /// Used by every marketplace-hosted model on Bedrock — z.ai, DeepSeek,
    /// Qwen, Kimi, MiniMax, Gemma, Nemotron, gpt-oss, and 2024+ Mistral.
    /// Body is standard `{messages,max_tokens,tools,...}`; the model id is
    /// in the URL, not the body. Streaming uses
    /// `/invoke-with-response-stream` and each eventstream frame carries
    /// either a JSON chunk or an SSE `data: …` event.
    OpenAi,
}

/// How a model exposes extended reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Model has no reasoning mode. Requesting `thinking_budget_tokens` is dropped.
    None,
    /// Anthropic-style: `{thinking: {type: "enabled", budget_tokens: N}}` at body
    /// root for Invoke, or under `additionalModelRequestFields` for Converse.
    ToggleWithBudget,
    /// Toggle-only: e.g. `{thinking: {type: "enabled"}}` (GLM) or
    /// `{enable_thinking: true}` (some Qwen). Concrete shape is per-model.
    Toggle,
    /// Reasoning is always emitted; no request-side field. The Converse stream
    /// surfaces `reasoningContent` blocks automatically.
    AlwaysOn,
}

/// Toggle-thinking shape — different families spell it differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleShape {
    /// `{thinking: {type: "enabled"}}`
    AnthropicLike,
    /// `{enable_thinking: true}`
    EnableThinkingBool,
}

#[derive(Debug, Clone, Copy)]
struct FamilyEntry {
    prefix: &'static str,
    wire: WireFormat,
    caches: bool,
    thinking: ThinkingMode,
    toggle_shape: Option<ToggleShape>,
}

const FAMILIES: &[FamilyEntry] = &[
    FamilyEntry {
        prefix: "anthropic.claude-opus-4",
        wire: WireFormat::Invoke,
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "anthropic.claude-sonnet-4",
        wire: WireFormat::Invoke,
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "anthropic.claude-haiku-4",
        wire: WireFormat::Invoke,
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "anthropic.claude-3-5-haiku",
        wire: WireFormat::Invoke,
        caches: true,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "anthropic.claude-3-haiku",
        wire: WireFormat::Invoke,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "anthropic.",
        wire: WireFormat::Invoke,
        caches: true,
        thinking: ThinkingMode::ToggleWithBudget,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "zai.glm-4.7-flash",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "zai.glm-4.7",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::Toggle,
        toggle_shape: Some(ToggleShape::AnthropicLike),
    },
    FamilyEntry {
        prefix: "zai.glm-5",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::Toggle,
        toggle_shape: Some(ToggleShape::AnthropicLike),
    },
    FamilyEntry {
        prefix: "zai.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    // Marketplace OAI rows below. Wire format verified by an OAI-body probe
    // against eu-west-2's catalog: each returned `{"choices":[…]}`.
    // caches:false because cachePoint isn't part of OAI; ThinkingMode is
    // retained per-model so we can emit model-native thinking control once
    // each shape is verified — bedrock_openai::build_body ignores it today.
    FamilyEntry {
        prefix: "deepseek.r1",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::AlwaysOn,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "deepseek.v3",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::Toggle,
        toggle_shape: Some(ToggleShape::AnthropicLike),
    },
    FamilyEntry {
        prefix: "deepseek.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "qwen.qwen3-235b-a22b-2507",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::Toggle,
        toggle_shape: Some(ToggleShape::EnableThinkingBool),
    },
    FamilyEntry {
        prefix: "qwen.qwen3-next-80b",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::Toggle,
        toggle_shape: Some(ToggleShape::EnableThinkingBool),
    },
    FamilyEntry {
        prefix: "qwen.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "moonshotai.kimi-k2-thinking",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::AlwaysOn,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "moonshotai.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "minimax.minimax-m2.5",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::AlwaysOn,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "minimax.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "google.gemma",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "nvidia.nemotron",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "openai.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    // Llama and Nova reject the OAI body in the probe; both speak Converse.
    // Nova additionally supports cachePoint (AWS docs); the Converse build
    // emits it when caches:true.
    FamilyEntry {
        prefix: "meta.llama",
        wire: WireFormat::Converse,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "amazon.nova",
        wire: WireFormat::Converse,
        caches: true,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    // Mistral is bimodal: pre-2024 models (mistral-7b, mixtral-8x7b) reject
    // OAI and speak Converse; everything newer (devstral, magistral,
    // ministral, mistral-large-2402+, voxtral) is OAI.
    FamilyEntry {
        prefix: "mistral.mistral-7b",
        wire: WireFormat::Converse,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "mistral.mixtral-8x7b",
        wire: WireFormat::Converse,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "mistral.magistral",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::AlwaysOn,
        toggle_shape: None,
    },
    FamilyEntry {
        prefix: "mistral.",
        wire: WireFormat::OpenAi,
        caches: false,
        thinking: ThinkingMode::None,
        toggle_shape: None,
    },
];

/// Strip a cross-region inference-profile prefix.
///
/// AWS uses geo prefixes `us./eu./au./jp.` plus a global profile `global.`.
/// `apac.` is retained for older inference profiles that still use it.
pub fn strip_region_prefix(model_id: &str) -> &str {
    for prefix in ["us.", "eu.", "au.", "jp.", "apac.", "global."] {
        if let Some(rest) = model_id.strip_prefix(prefix) {
            return rest;
        }
    }
    model_id
}

/// Candidate Bedrock-namespaced forms for a bare model id.
///
/// AWS's catalog is inconsistent: some providers repeat their own name in
/// the family suffix (`qwen.qwen3-…`, `minimax.minimax-m2`,
/// `mistral.mistral-7b-…`) while others drop it (`deepseek.v3.2`,
/// `deepseek.r1`). The user's bare form is the same shape in both cases
/// (`deepseek-v3.2`, `qwen3-32b-…`), so we emit up to two candidates and
/// let the catalog resolver pick whichever exists:
///
///   1. **Prepend** the namespace: `claude-sonnet-4-6` →
///      `anthropic.claude-sonnet-4-6`. Always emitted when a mapping
///      matches.
///   2. **Strip + replace**: when the bare prefix's leading word equals
///      the namespace stem (e.g. `deepseek` / `deepseek.`), and the id
///      actually starts with `<stem>-`, also try `<namespace>.<rest>` so
///      `deepseek-v3.2` → `deepseek.v3.2`.
///
/// Returns an empty vec when the id is already qualified (a `.` before any
/// `-`) or when no mapping prefix matches. The catalog resolver consumes
/// this as part of [`candidate_lookup_keys`] and stops at the first match.
pub fn infer_provider_namespace(model_id: &str) -> Vec<String> {
    let already_qualified = match (model_id.find('.'), model_id.find('-')) {
        (Some(dot), Some(dash)) => dot < dash,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if already_qualified {
        return Vec::new();
    }
    let Some((bare, namespace)) = BARE_PROVIDER_MAPPINGS
        .iter()
        .find(|(bare, _)| model_id.starts_with(bare))
    else {
        return Vec::new();
    };

    let mut candidates = vec![format!("{namespace}{model_id}")];

    let namespace_stem = namespace.trim_end_matches('.');
    let bare_leading_word = bare.split('-').next().unwrap_or(bare);
    if bare_leading_word == namespace_stem {
        let stem_dash = format!("{bare_leading_word}-");
        if let Some(rest) = model_id.strip_prefix(&stem_dash) {
            candidates.push(format!("{namespace}{rest}"));
        }
    }
    candidates
}

/// `(bare prefix, Bedrock provider namespace)` pairs used by
/// [`infer_provider_namespace`]. Each `bare` is uniquely owned by one
/// provider on Bedrock — no ambiguity.
const BARE_PROVIDER_MAPPINGS: &[(&str, &str)] = &[
    ("claude-", "anthropic."),
    ("gemma-", "google."),
    ("gpt-oss-", "openai."),
    ("nemotron-", "nvidia."),
    ("nova-", "amazon."),
    ("glm-", "zai."),
    ("kimi-", "moonshotai."),
    ("llama", "meta."),
    ("qwen", "qwen."),
    ("deepseek", "deepseek."),
    ("minimax", "minimax."),
    ("mistral-", "mistral."),
    ("mixtral-", "mistral."),
    ("devstral-", "mistral."),
    ("magistral-", "mistral."),
    ("ministral-", "mistral."),
    ("voxtral-", "mistral."),
];

fn lookup(model_id: &str) -> Option<&'static FamilyEntry> {
    let stripped = strip_region_prefix(model_id);
    FAMILIES.iter().find(|f| stripped.starts_with(f.prefix))
}

/// Which wire format the given Bedrock model uses.
///
/// Unknown models — those not matching any [`FAMILIES`] prefix — default to
/// Converse since it's Bedrock's first-party unified API. Marketplace
/// providers all have a catchall row, so a new variant inside a known
/// provider routes correctly without a code change.
pub fn wire_format_for(model_id: &str) -> WireFormat {
    lookup(model_id)
        .map(|f| f.wire)
        .unwrap_or(WireFormat::Converse)
}

/// Whether the given Bedrock model has prompt caching support enabled on AWS.
pub fn caches(model_id: &str) -> bool {
    lookup(model_id).map(|f| f.caches).unwrap_or(false)
}

/// How the given Bedrock model exposes extended reasoning.
pub fn thinking(model_id: &str) -> ThinkingMode {
    lookup(model_id)
        .map(|f| f.thinking)
        .unwrap_or(ThinkingMode::None)
}

/// Toggle-thinking shape for a `ThinkingMode::Toggle` model.
pub fn toggle_shape(model_id: &str) -> Option<ToggleShape> {
    lookup(model_id).and_then(|f| f.toggle_shape)
}

/// Whether an Anthropic model accepts *only* adaptive thinking
/// (`thinking.type: "adaptive"` + `output_config.effort`) and rejects the
/// legacy `enabled` + `budget_tokens` schema with a 400.
///
/// Host-agnostic: matches the bare (`claude-opus-4-7`), Bedrock
/// (`anthropic.claude-opus-4-7-…`, optionally region-prefixed) and Vertex
/// (`claude-opus-4-7@…`) id forms. The version token is boundary-anchored so a
/// hypothetical `claude-opus-4-70` is not caught.
///
/// Opus 4.7/4.8, every Opus/Sonnet/Haiku 5.x and every Fable are
/// adaptive-only (the 5.5 generation verified on Bedrock eu-west-2,
/// 2026-10-19: `"thinking.type.enabled" is not supported for this model`).
/// The 5.x tokens also catch their point releases (`claude-sonnet-5` matches
/// `claude-sonnet-5-5` and `claude-sonnet-5.5`). Each new adaptive family must
/// be added here, or it falls through to the legacy `enabled` +
/// `budget_tokens` schema and 400s on Bedrock/Anthropic.
pub fn requires_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("mythos")
        || model_id.contains("claude-fable")
        || contains_version_token(model_id, "claude-opus-4-7")
        || contains_version_token(model_id, "claude-opus-4-8")
        || contains_version_token(model_id, "claude-opus-5")
        || contains_version_token(model_id, "claude-sonnet-5")
        || contains_version_token(model_id, "claude-haiku-5")
}

/// Whether an Anthropic model takes `defer_loading` tools and `tool_reference`
/// results. Verified on Bedrock Invoke for these versions; Sonnet 4.5 rejects
/// the field, and a model not listed is sent declared tools only.
pub fn defers_tools(model_id: &str) -> bool {
    [
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-haiku-4-5",
    ]
    .iter()
    .any(|version| contains_version_token(model_id, version))
}

/// Cap an `enabled`-mode thinking budget to a value the API will accept, or
/// `None` to disable thinking when none fits. Anthropic requires
/// `1024 <= thinking.budget_tokens < max_tokens`, and a useful request needs
/// output room beyond the budget. The runner shrinks `effective_max_tokens` as
/// context grows, so a fixed configured budget can end up >= max_tokens → a 400
/// ("max_tokens must be greater than thinking.budget_tokens"). Adaptive models
/// don't carry `budget_tokens` and never call this.
pub fn clamp_thinking_budget(budget: u32, max_tokens: u32) -> Option<u32> {
    const MIN_BUDGET: u32 = 1024; // Anthropic's minimum thinking budget
    const OUTPUT_HEADROOM: u32 = 1024; // leave room for non-thinking output
    let budget = budget.min(max_tokens.saturating_sub(OUTPUT_HEADROOM));
    (budget >= MIN_BUDGET).then_some(budget)
}

fn contains_version_token(model_id: &str, token: &str) -> bool {
    model_id
        .match_indices(token)
        .any(|(idx, _)| !model_id[idx + token.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_models_defer_tools() {
        for model in [
            "eu.anthropic.claude-sonnet-5",
            "anthropic.claude-sonnet-4-6",
            "eu.anthropic.claude-opus-4-6-v1",
            "anthropic.claude-opus-4-8",
            "eu.anthropic.claude-haiku-4-5-20251001-v1:0",
        ] {
            assert!(defers_tools(model), "{model}");
        }
        for model in [
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "anthropic.claude-3-haiku-20240307-v1:0",
            "zai.glm-5",
            "anthropic.claude-opus-4-60",
        ] {
            assert!(!defers_tools(model), "{model}");
        }
    }

    #[test]
    fn strip_region_prefix_strips_known_regions() {
        assert_eq!(strip_region_prefix("us.anthropic.x"), "anthropic.x");
        assert_eq!(strip_region_prefix("eu.zai.glm-5"), "zai.glm-5");
        assert_eq!(strip_region_prefix("au.anthropic.x"), "anthropic.x");
        assert_eq!(strip_region_prefix("jp.anthropic.x"), "anthropic.x");
        assert_eq!(strip_region_prefix("global.anthropic.x"), "anthropic.x");
        assert_eq!(strip_region_prefix("apac.qwen.foo"), "qwen.foo");
        assert_eq!(strip_region_prefix("anthropic.x"), "anthropic.x");
    }

    #[test]
    fn wire_format_anthropic_uses_invoke() {
        for id in [
            "anthropic.claude-opus-4-7",
            "anthropic.claude-sonnet-4-6",
            "us.anthropic.claude-sonnet-4-6",
            "eu.anthropic.claude-sonnet-4-6",
            "au.anthropic.claude-sonnet-4-6",
            "jp.anthropic.claude-sonnet-4-6",
            "global.anthropic.claude-sonnet-4-6",
            "anthropic.claude-haiku-4-5",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
        ] {
            assert_eq!(wire_format_for(id), WireFormat::Invoke, "id={id}");
        }
    }

    #[test]
    fn wire_format_marketplace_uses_openai() {
        // Every marketplace family confirmed against eu-west-2 catalog: the
        // bare `/invoke` endpoint returns OAI Chat-Completions shape.
        for id in [
            "zai.glm-5",
            "zai.glm-4.7",
            "zai.glm-4.7-flash",
            "zai.glm-6-future-variant",
            "eu.zai.glm-5",
            "deepseek.v3-v1:0",
            "deepseek.v3.2",
            "deepseek.r1",
            "qwen.qwen3-coder-480b-a35b-v1:0",
            "qwen.qwen3-coder-30b-a3b-v1:0",
            "qwen.qwen3-coder-next",
            "qwen.qwen3-235b-a22b-2507-v1:0",
            "qwen.qwen3-next-80b-a3b",
            "qwen.qwen3-vl-235b-a22b",
            "qwen.qwen3-32b-v1:0",
            "moonshotai.kimi-k2.5",
            "moonshotai.kimi-k2-thinking",
            "minimax.minimax-m2",
            "minimax.minimax-m2.1",
            "minimax.minimax-m2.5",
            "google.gemma-3-27b-it",
            "google.gemma-3-12b-it",
            "google.gemma-3-4b-it",
            "nvidia.nemotron-nano-9b-v2",
            "nvidia.nemotron-super-3-120b",
            "openai.gpt-oss-120b-1:0",
            "openai.gpt-oss-20b-1:0",
            "openai.gpt-oss-safeguard-120b",
            "mistral.devstral-2-123b",
            "mistral.magistral-small-2509",
            "mistral.ministral-3-8b-instruct",
            "mistral.mistral-large-2402-v1:0",
            "mistral.voxtral-small-24b-2507",
        ] {
            assert_eq!(wire_format_for(id), WireFormat::OpenAi, "id={id}");
        }
    }

    #[test]
    fn wire_format_converse_native_families() {
        for id in [
            "meta.llama3-70b-instruct-v1:0",
            "meta.llama3-8b-instruct-v1:0",
            "us.meta.llama4-maverick-17b-instruct-v1:0",
            "amazon.nova-pro-v1:0",
            "amazon.nova-lite-v1:0",
            "amazon.nova-micro-v1:0",
            "mistral.mistral-7b-instruct-v0:2",
            "mistral.mixtral-8x7b-instruct-v0:1",
        ] {
            assert_eq!(wire_format_for(id), WireFormat::Converse, "id={id}");
        }
    }

    #[test]
    fn caching_matrix_matches_snapshot() {
        let cases = &[
            ("anthropic.claude-opus-4-8", true),
            // sonnet-5 falls through to the `anthropic.` catch-all family
            ("anthropic.claude-sonnet-5", true),
            ("anthropic.claude-opus-4-7", true),
            ("anthropic.claude-sonnet-4-6", true),
            ("anthropic.claude-haiku-4-5", true),
            ("anthropic.claude-3-5-haiku-20241022-v1:0", true),
            ("anthropic.claude-3-haiku-20240307-v1:0", false),
            ("us.anthropic.claude-opus-4-7", true),
            // OAI-routed marketplace models don't accept cachePoint.
            ("zai.glm-5", false),
            ("zai.glm-4.7-flash", false),
            ("zai.glm-4.7", false),
            ("deepseek.v3.2", false),
            ("deepseek.r1", false),
            ("qwen.qwen3-coder-480b-a35b-v1:0", false),
            ("qwen.qwen3-235b-a22b-2507-v1:0", false),
            ("qwen.qwen3-32b-v1:0", false),
            ("moonshotai.kimi-k2.5", false),
            ("moonshotai.kimi-k2-thinking", false),
            ("minimax.minimax-m2.5", false),
            ("google.gemma-3-27b-it", false),
            ("nvidia.nemotron-super-3-120b", false),
            ("openai.gpt-oss-120b-1:0", false),
            ("mistral.devstral-2-123b", false),
            ("mistral.magistral-small-2509", false),
            ("mistral.mistral-large-2402-v1:0", false),
            // Converse-native families.
            ("meta.llama3-70b-instruct-v1:0", false),
            ("meta.llama3-8b-instruct-v1:0", false),
            ("mistral.mistral-7b-instruct-v0:2", false),
            ("mistral.mixtral-8x7b-instruct-v0:1", false),
            // Amazon Nova supports prompt caching on Converse.
            ("amazon.nova-pro-v1:0", true),
            ("amazon.nova-lite-v1:0", true),
        ];
        for (id, expected) in cases {
            assert_eq!(caches(id), *expected, "id={id}");
        }
    }

    #[test]
    fn thinking_matrix_matches_snapshot() {
        let cases = &[
            ("anthropic.claude-opus-4-7", ThinkingMode::ToggleWithBudget),
            (
                "anthropic.claude-sonnet-4-6",
                ThinkingMode::ToggleWithBudget,
            ),
            ("anthropic.claude-haiku-4-5", ThinkingMode::ToggleWithBudget),
            (
                "anthropic.claude-3-5-haiku-20241022-v1:0",
                ThinkingMode::None,
            ),
            ("anthropic.claude-3-haiku-20240307-v1:0", ThinkingMode::None),
            ("zai.glm-5", ThinkingMode::Toggle),
            ("zai.glm-4.7", ThinkingMode::Toggle),
            ("zai.glm-4.7-flash", ThinkingMode::None),
            ("deepseek.r1", ThinkingMode::AlwaysOn),
            ("deepseek.v3-v1:0", ThinkingMode::Toggle),
            ("deepseek.v3.2", ThinkingMode::Toggle),
            ("qwen.qwen3-coder-480b-a35b-v1:0", ThinkingMode::None),
            ("qwen.qwen3-coder-30b-a3b-v1:0", ThinkingMode::None),
            ("qwen.qwen3-235b-a22b-2507-v1:0", ThinkingMode::Toggle),
            ("qwen.qwen3-next-80b-a3b", ThinkingMode::Toggle),
            ("qwen.qwen3-32b-v1:0", ThinkingMode::None),
            ("qwen.qwen3-vl-235b-a22b", ThinkingMode::None),
            ("moonshotai.kimi-k2-thinking", ThinkingMode::AlwaysOn),
            ("moonshotai.kimi-k2.5", ThinkingMode::None),
            ("minimax.minimax-m2.5", ThinkingMode::AlwaysOn),
            ("minimax.minimax-m2.1", ThinkingMode::None),
            ("minimax.minimax-m2", ThinkingMode::None),
            ("google.gemma-3-27b-it", ThinkingMode::None),
            ("nvidia.nemotron-super-3-120b", ThinkingMode::None),
            ("openai.gpt-oss-120b-1:0", ThinkingMode::None),
            ("meta.llama4-maverick-17b-instruct-v1:0", ThinkingMode::None),
            ("meta.llama3-3-70b-instruct-v1:0", ThinkingMode::None),
            ("amazon.nova-pro-v1:0", ThinkingMode::None),
            ("mistral.magistral-small-2509", ThinkingMode::AlwaysOn),
            ("mistral.devstral-2-123b", ThinkingMode::None),
            ("mistral.mistral-large-2402-v1:0", ThinkingMode::None),
            ("mistral.mistral-7b-instruct-v0:2", ThinkingMode::None),
        ];
        for (id, expected) in cases {
            assert_eq!(thinking(id), *expected, "id={id}");
        }
    }

    #[test]
    fn toggle_shape_known() {
        assert_eq!(toggle_shape("zai.glm-5"), Some(ToggleShape::AnthropicLike));
        assert_eq!(
            toggle_shape("qwen.qwen3-235b-a22b-2507-v1:0"),
            Some(ToggleShape::EnableThinkingBool)
        );
        assert_eq!(toggle_shape("zai.glm-4.7-flash"), None);
        assert_eq!(toggle_shape("anthropic.claude-opus-4-7"), None);
    }

    #[test]
    fn clamp_thinking_budget_caps_below_max_tokens_and_disables_when_no_room() {
        // Plenty of room: budget unchanged.
        assert_eq!(clamp_thinking_budget(10_000, 16_384), Some(10_000));
        // max_tokens clamped near the budget: cap to max_tokens - output headroom.
        assert_eq!(clamp_thinking_budget(10_000, 9_000), Some(9_000 - 1024));
        // Budget already below the ceiling: unchanged.
        assert_eq!(clamp_thinking_budget(4_000, 8_192), Some(4_000));
        // Too little room for a valid (>=1024) budget → disable thinking.
        assert_eq!(clamp_thinking_budget(10_000, 2_000), None);
        assert_eq!(clamp_thinking_budget(10_000, 1_024), None);
        // Configured budget below the 1024 minimum → disable rather than 400.
        assert_eq!(clamp_thinking_budget(500, 64_000), None);
    }

    #[test]
    fn requires_adaptive_thinking_matches_opus_4_7_4_8_the_5_generation_fable_and_mythos() {
        for id in [
            "anthropic.claude-opus-4-7",
            "anthropic.claude-opus-4-7-20251015-v1:0",
            "us.anthropic.claude-opus-4-7-20251015-v1:0",
            "eu.anthropic.claude-opus-4-7",
            "claude-opus-4-7",
            "claude-opus-4-7@20251015",
            "anthropic.claude-opus-4-8",
            "anthropic.claude-opus-4-8-20260115-v1:0",
            "eu.anthropic.claude-opus-4-8",
            "claude-opus-4-8",
            "claude-opus-4-8@20260115",
            "anthropic.claude-sonnet-5",
            "global.anthropic.claude-sonnet-5",
            "claude-sonnet-5",
            "anthropic.claude-mythos-preview",
            "claude-mythos-preview",
            "anthropic.claude-opus-5",
            "eu.anthropic.claude-opus-5-5",
            "global.anthropic.claude-opus-5-5",
            "claude-opus-5-5",
            "anthropic/claude-opus-5.5",
            "eu.anthropic.claude-sonnet-5-5",
            "claude-sonnet-5-5",
            "anthropic/claude-sonnet-5.5",
            "eu.anthropic.claude-haiku-5-5",
            "global.anthropic.claude-haiku-5-5",
            "claude-haiku-5-5",
            "anthropic/claude-haiku-5.5",
            "global.anthropic.claude-fable-5-1",
            "claude-fable-5",
            "anthropic/claude-fable-5.1",
        ] {
            assert!(requires_adaptive_thinking(id), "expected adaptive: {id}");
        }
        for id in [
            "anthropic.claude-opus-4-6-v1:0",
            "claude-opus-4-6@20250929",
            "anthropic.claude-sonnet-4-6",
            "claude-sonnet-4-6@20251015",
            "anthropic.claude-opus-4-5",
            "anthropic.claude-opus-4-70",
            "anthropic.claude-opus-4-80",
            "anthropic.claude-sonnet-50",
            "anthropic.claude-opus-50",
            "anthropic.claude-haiku-50",
            "anthropic.claude-haiku-4-5",
        ] {
            assert!(
                !requires_adaptive_thinking(id),
                "expected non-adaptive: {id}"
            );
        }
    }

    #[test]
    fn infer_provider_namespace_prepends_for_different_stem() {
        // Bare prefixes owned by a single provider whose namespace differs
        // (claude/anthropic, glm/zai, kimi/moonshotai, …). Only the
        // prepend candidate makes sense.
        let cases = &[
            ("claude-sonnet-4-6", "anthropic.claude-sonnet-4-6"),
            (
                "claude-haiku-4-5-20251001-v1:0",
                "anthropic.claude-haiku-4-5-20251001-v1:0",
            ),
            ("glm-5", "zai.glm-5"),
            ("glm-4.7", "zai.glm-4.7"),
            ("glm-4.7-flash", "zai.glm-4.7-flash"),
            ("kimi-k2.5", "moonshotai.kimi-k2.5"),
            ("kimi-k2-thinking", "moonshotai.kimi-k2-thinking"),
            ("nova-pro-v1:0", "amazon.nova-pro-v1:0"),
            ("gemma-3-27b-it", "google.gemma-3-27b-it"),
            ("gpt-oss-120b-1:0", "openai.gpt-oss-120b-1:0"),
            ("nemotron-super-3-120b", "nvidia.nemotron-super-3-120b"),
            ("llama3-70b-instruct-v1:0", "meta.llama3-70b-instruct-v1:0"),
            ("devstral-2-123b", "mistral.devstral-2-123b"),
            ("magistral-small-2509", "mistral.magistral-small-2509"),
            (
                "mixtral-8x7b-instruct-v0:1",
                "mistral.mixtral-8x7b-instruct-v0:1",
            ),
        ];
        for (bare, expected) in cases {
            assert_eq!(
                infer_provider_namespace(bare),
                vec![expected.to_string()],
                "bare={bare}"
            );
        }
    }

    #[test]
    fn infer_provider_namespace_emits_strip_form_when_stem_matches() {
        // Same-stem providers where the catalog DROPS the repeated name
        // (`deepseek.v3.2`, `deepseek.r1`). The strip-and-replace
        // candidate is the one that matches.
        assert_eq!(
            infer_provider_namespace("deepseek-v3.2"),
            vec![
                "deepseek.deepseek-v3.2".to_string(),
                "deepseek.v3.2".to_string(),
            ]
        );
        assert_eq!(
            infer_provider_namespace("deepseek-r1"),
            vec![
                "deepseek.deepseek-r1".to_string(),
                "deepseek.r1".to_string(),
            ]
        );

        // Same-stem providers where the catalog REPEATS the name
        // (`minimax.minimax-m2.5`, `mistral.mistral-7b-…`). The prepend
        // candidate matches; the strip candidate is a harmless miss
        // listed alongside it.
        assert_eq!(
            infer_provider_namespace("minimax-m2.5"),
            vec![
                "minimax.minimax-m2.5".to_string(),
                "minimax.m2.5".to_string(),
            ]
        );
        assert_eq!(
            infer_provider_namespace("mistral-7b-instruct-v0:2"),
            vec![
                "mistral.mistral-7b-instruct-v0:2".to_string(),
                "mistral.7b-instruct-v0:2".to_string(),
            ]
        );

        // Same-stem provider whose bare form has NO separator after the
        // stem (`qwen3-…` rather than `qwen-…`): only prepend fires.
        assert_eq!(
            infer_provider_namespace("qwen3-32b-v1:0"),
            vec!["qwen.qwen3-32b-v1:0".to_string()]
        );
    }

    #[test]
    fn infer_provider_namespace_no_op_when_already_qualified() {
        for id in [
            "anthropic.claude-sonnet-4-6",
            "eu.anthropic.claude-sonnet-4-6",
            "zai.glm-5",
            "us.zai.glm-5",
            "qwen.qwen3-32b-v1:0",
            "deepseek.v3.2",
        ] {
            assert!(
                infer_provider_namespace(id).is_empty(),
                "expected no inference for {id}"
            );
        }
    }

    #[test]
    fn infer_provider_namespace_unknown_returns_empty() {
        assert!(infer_provider_namespace("totally-fictional-model").is_empty());
    }
}

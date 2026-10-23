//! Model-specific context window and output token limits.
//!
//! Provides a static registry of known model limits so the failover and
//! compaction systems can make context-aware decisions. Unknown models
//! fall back to the configured `max_context_tokens` — no regression for
//! single-provider setups.

use workforce_common::session::CompactionConfig;

/// Represents the token limits for a specific LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    /// The maximum number of tokens allowed in the input context.
    pub context_tokens: u32,
    /// The maximum number of tokens the model can generate in a single response.
    pub max_output_tokens: u32,
}

/// Retrieves the token limits for a given model identifier.
///
/// Uses substring matching to identify the model family and return its known
/// limits. Returns `None` for unknown models, allowing callers to fall back
/// to their configured defaults.
pub fn model_limits(model: &str) -> Option<ModelLimits> {
    // Branches with identical limits are merged to satisfy clippy::if_same_then_else.
    //
    // ORDER IS LOAD-BEARING for the Claude branches. Matching is by substring,
    // and every current-generation id CONTAINS a previous-generation one:
    // `claude-opus-4-8`.contains("claude-opus-4") and
    // `claude-sonnet-4-6`.contains("claude-sonnet-4") are both true. The 1M
    // branch must therefore come FIRST — put it after and the current models
    // silently resolve to a fifth of their real window, which is not an error
    // anywhere: `effective_context_limit` takes min(model, configured), so the
    // agent simply compacts every round, shredding tool outputs mid-turn and
    // answering from evidence it no longer has. Adding a new Claude id means
    // adding it HERE, not to the legacy branch it happens to be a superstring of.
    if model.contains("claude-fable")
        || model.contains("claude-opus-5")
        || model.contains("claude-opus-4-8")
        || model.contains("claude-opus-4-7")
        || model.contains("claude-opus-4-6")
        || model.contains("claude-sonnet-5")
        || model.contains("claude-sonnet-4-6")
        || model.contains("claude-haiku-5")
    {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 128_000,
        })
    } else if model.contains("claude-opus-4") {
        // Opus 4 / 4.1 / 4.5 — the genuinely 200K generation.
        Some(ModelLimits {
            context_tokens: 200_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("claude-sonnet-4") || model.contains("claude-haiku-4") {
        // Sonnet 4 / 4.5 and Haiku 4.5.
        Some(ModelLimits {
            context_tokens: 200_000,
            max_output_tokens: 65_536,
        })
    } else if model.contains("grok-4.7") || model.contains("grok-4.6") || model.contains("grok-4.5")
    {
        // Above every generic grok-4 match. Windows from docs.x.ai (2026-10-19).
        Some(ModelLimits {
            context_tokens: 500_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("grok-4.3") || model.contains("grok-4.20") {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("gpt-6") {
        // gpt-6-sol/-luna/-astra and gpt-6.1-sol: 1.05M on OpenRouter.
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 128_000,
        })
    } else if model.contains("deepseek-v4") || model.contains("kimi-k3") {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("qwen-3.8") {
        // Cerebras-hosted Qwen 3.8 serves a smaller window than Qwen direct.
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 32_768,
        })
    } else if model.contains("qwen3.8") {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("grok-4") || model.contains("grok-3") {
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 32_768,
        })
    } else if model.contains("gpt-5.4") {
        if model.contains("mini") || model.contains("nano") {
            Some(ModelLimits {
                context_tokens: 400_000,
                max_output_tokens: 128_000,
            })
        } else {
            Some(ModelLimits {
                context_tokens: 1_000_000,
                max_output_tokens: 128_000,
            })
        }
    } else if model.contains("gpt-5") {
        Some(ModelLimits {
            context_tokens: 400_000,
            max_output_tokens: 128_000,
        })
    } else if model.contains("gpt-4.1") {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 32_768,
        })
    } else if model.contains("gemini-3")
        || model.contains("gemini-2.5-pro")
        || model.contains("gemini-2.5-flash")
    {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 65_536,
        })
    } else if model.contains("qwen3.5-397b")
        || model.contains("qwen3.5-122b")
        || model.contains("qwen3.5-35b")
        || model.contains("qwen3.5-27b")
        || model.contains("Qwen3.5-27B")
    {
        Some(ModelLimits {
            context_tokens: 262_144,
            max_output_tokens: 65_536,
        })
    } else if model.contains("qwen3.5-flash") || model.contains("qwen3.5-plus") {
        Some(ModelLimits {
            context_tokens: 1_000_000,
            max_output_tokens: 65_536,
        })
    } else if model.contains("kimi-k2") {
        Some(ModelLimits {
            context_tokens: 262_144,
            max_output_tokens: 65_535,
        })
    } else if model.contains("Qwen3-72B")
        || model.contains("qwen3-72b")
        || model.contains("Qwen3-32B")
        || model.contains("qwen3-32b")
        || model.contains("Qwen3-30B")
        || model.contains("qwen3-30b")
    {
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 32_768,
        })
    } else if model.contains("deepseek-chat")
        || model.contains("deepseek-v3")
        || model.contains("deepseek-reasoner")
    {
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 8_192,
        })
    } else if model.contains("Qwen2.5-VL") || model.contains("qwen2.5-vl") {
        Some(ModelLimits {
            context_tokens: 32_768,
            max_output_tokens: 8_192,
        })
    } else if model.contains("qwen3-coder") && !model.contains("next") && !model.contains("30b") {
        Some(ModelLimits {
            context_tokens: 262_144,
            max_output_tokens: 65_536,
        })
    } else if model.contains("qwen3-coder-next") || model.contains("qwen3-coder-30b") {
        Some(ModelLimits {
            context_tokens: 256_000,
            max_output_tokens: 32_768,
        })
    } else if model == "glm-5" || model.starts_with("glm-5.") {
        // The direct Z.ai GLM-5 series (glm-5, glm-5.1, glm-5.2, …) — 200K
        // context. The Cerebras/OpenRouter/Bedrock variants carry their own
        // provider prefixes (`zai-`, `z-ai/`, `zai.`) and match the branches
        // below, so this only catches the direct model ids. Registering the
        // series keeps a new point release (e.g. glm-5.2) from falling through
        // to the unknown-model path, which would drop it below the tool-filter
        // large-context tier and strip its tools once past the count threshold.
        Some(ModelLimits {
            context_tokens: 200_000,
            max_output_tokens: 131_072,
        })
    } else if model.starts_with("zai-glm-5") || model.starts_with("zai-glm-4.7") {
        // Cerebras-hosted GLM has a smaller context window than Z.ai direct
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 40_000,
        })
    } else if model.starts_with("z-ai/glm-5") || model.starts_with("z-ai/glm-4.7") {
        Some(ModelLimits {
            context_tokens: 202_752,
            max_output_tokens: 40_000,
        })
    } else if model.starts_with("z-ai/glm-4.6") {
        Some(ModelLimits {
            context_tokens: 204_800,
            max_output_tokens: 32_768,
        })
    } else if model.starts_with("z-ai/glm-4.5")
        || model.contains("qwen-3-235b")
        || model.contains("gpt-oss-120b")
    {
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 32_768,
        })
    } else if model.starts_with("zai.glm-5") || model.starts_with("zai.glm-4.7") {
        // Bedrock-hosted GLM uses dotted tier ids (`zai.glm-5`, `zai.glm-4.7`,
        // `zai.glm-4.7-flash`), distinct from the hyphen/slash forms above.
        // Output cap from the Z.ai/Cerebras siblings; context a conservative floor.
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 40_000,
        })
    } else if model.starts_with("meta.llama3-") {
        // Bedrock's Llama 3 (not 3.1): an 8K window.
        Some(ModelLimits {
            context_tokens: 8_192,
            max_output_tokens: 2_048,
        })
    } else if model.starts_with("deepseek.v3")
        || model.starts_with("minimax.minimax-m2")
        || model.starts_with("mistral.devstral")
        || model.starts_with("qwen.qwen3-235b")
    {
        // Bedrock OpenAI-wire families the tier table names. Their own windows
        // run 128K-256K; a conservative floor keeps them out of the
        // unknown-model path, which strips tools past the count threshold.
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 32_768,
        })
    } else if model == "llama3.1-8b" {
        Some(ModelLimits {
            context_tokens: 131_072,
            max_output_tokens: 8_192,
        })
    } else {
        None
    }
}

/// Returns the known context window for a model, or `None` if unknown.
/// Thin accessor over [`model_limits`] for callers that only need the
/// context size (e.g. resolving an emergency-compaction fallback limit).
pub fn context_for(model: &str) -> Option<u32> {
    model_limits(model).map(|limits| limits.context_tokens)
}

/// Calculates the effective context limit for a model, considering both the
/// model's own limit and the user's configured maximum.
///
/// Returns the smaller of the two, or `configured_max` if the model is unknown.
/// An unknown model deliberately keeps whatever the operator configured: we
/// have no window to compare against, and someone running a self-hosted model
/// knows its size better than this table does.
pub fn effective_context_limit(model: &str, configured_max: u32) -> u32 {
    model_limits(model)
        .map(|limits| limits.context_tokens.min(configured_max))
        .unwrap_or(configured_max)
}

/// [`effective_context_limit`] on the window the provider serving `model`
/// reports, which can be smaller than the model's own (the ChatGPT backend
/// serves `gpt-6-sol` at 272K).
pub async fn context_limit(
    llm: &dyn super::provider::LlmProvider,
    model: &str,
    configured_max: u32,
) -> u32 {
    llm.context_floor(model)
        .await
        .map_or(configured_max, |window| window.min(configured_max))
}

// ─── CompactionConfig-aware free functions ───────────────────────────────────
//
// These live here (not on CompactionConfig) to avoid a dependency inversion
// where `workforce-common` would need to depend on `workforce-agent`.

/// Calculates the effective context limit for a model given a CompactionConfig.
pub fn effective_context_for_model(config: &CompactionConfig, model: &str) -> u32 {
    effective_context_limit(model, config.max_context_tokens)
}

/// Calculates the compaction trigger threshold for a specific model.
///
/// Uses the effective (model-aware) context limit multiplied by the
/// configured compaction threshold fraction.
pub fn trigger_threshold_for_model(config: &CompactionConfig, model: &str) -> u32 {
    let effective_max = effective_context_for_model(config, model);
    (effective_max as f32 * config.compaction_threshold) as u32
}

/// Calculates the urgent compaction trigger threshold for a specific model.
///
/// Uses the effective (model-aware) context limit multiplied by the
/// configured urgent compaction threshold fraction.
pub fn urgent_threshold_for_model(config: &CompactionConfig, model: &str) -> u32 {
    let effective_max = effective_context_for_model(config, model);
    (effective_max as f32 * config.urgent_compaction_threshold) as u32
}

// ─── Per-model max_tokens resolution ──────────────────────────────────────────

/// Resolves the effective `max_tokens` for a specific model.
///
/// Checks the per-model overrides map (exact match first, then substring match)
/// and falls back to `fallback` if no override is found.
pub fn effective_max_tokens_for_model(
    model: &str,
    model_overrides: &std::collections::HashMap<String, u32>,
    fallback: u32,
) -> u32 {
    model_overrides
        .get(model)
        .copied()
        .or_else(|| {
            model_overrides
                .iter()
                .find(|(key, _)| model.contains(key.as_str()))
                .map(|(_, v)| *v)
        })
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── model_limits() tests ────────────────────────────────────────────────

    #[test]
    fn claude_opus_limits() {
        let limits = model_limits("claude-opus-4-20250514").unwrap();
        assert_eq!(limits.context_tokens, 200_000);
        assert_eq!(limits.max_output_tokens, 32_768);
    }

    #[test]
    fn claude_sonnet_limits() {
        let limits = model_limits("claude-sonnet-4-20250514").unwrap();
        assert_eq!(limits.context_tokens, 200_000);
        assert_eq!(limits.max_output_tokens, 65_536);
    }

    #[test]
    fn claude_sonnet_4_5_limits() {
        // claude-sonnet-4-5 still contains "claude-sonnet-4"
        let limits = model_limits("claude-sonnet-4-5-20250929").unwrap();
        assert_eq!(limits.context_tokens, 200_000);
        assert_eq!(limits.max_output_tokens, 65_536);
    }

    #[test]
    fn claude_haiku_limits() {
        let limits = model_limits("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(limits.context_tokens, 200_000);
        assert_eq!(limits.max_output_tokens, 65_536);
    }

    /// Regression: the current generation was matched by the LEGACY branches —
    /// `claude-sonnet-5` was listed alongside `claude-sonnet-4`, and
    /// `claude-opus-4-8`/`4-7`/`4-6` fell into `contains("claude-opus-4")` — so
    /// every one of them resolved to 200K instead of 1M. Nothing errored: the
    /// agent just compacted every round against a fifth of its real window,
    /// evicting the tool outputs it was about to answer from.
    #[test]
    fn current_generation_claude_is_one_million_context() {
        for model in [
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            // Provider-prefixed forms must resolve identically — Bedrock and
            // Vertex are where this actually bit.
            "anthropic.claude-sonnet-5",
            "eu.anthropic.claude-sonnet-5",
            "anthropic.claude-opus-4-8",
            "anthropic/claude-sonnet-5",
            // The 5.5 generation and Fable, in each provider's id form.
            "claude-opus-5-5",
            "eu.anthropic.claude-sonnet-5-5",
            "anthropic/claude-sonnet-5.5",
            "claude-haiku-5-5",
            "eu.anthropic.claude-haiku-5-5",
            "anthropic/claude-haiku-5.5",
            "claude-fable-5-1",
            "global.anthropic.claude-fable-5-1",
        ] {
            let limits = model_limits(model).unwrap_or_else(|| panic!("{model} is known"));
            assert_eq!(limits.context_tokens, 1_000_000, "{model} context");
            assert_eq!(limits.max_output_tokens, 128_000, "{model} max output");
        }
    }

    /// Every model a default tier names must have limits: an unknown model
    /// falls below the tool filter's large-context tier and loses its tools
    /// once past the count threshold.
    #[test]
    fn every_default_tier_model_has_limits() {
        let table =
            super::super::tiers::parse_tier_table(include_str!("default_tiers.toml")).unwrap();
        let mut unknown: Vec<String> = table
            .iter()
            .filter(|(key, _)| *key != "claude-code") // the CLI's own aliases
            .flat_map(|(key, tiers)| {
                tiers
                    .iter()
                    .filter(|(_, model)| model_limits(model).is_none())
                    .map(move |(tier, model)| format!("[{key}] {tier} = {model}"))
            })
            .collect();
        unknown.sort();
        assert!(unknown.is_empty(), "no limits for {unknown:#?}");
    }

    /// The legacy generation keeps its real 200K window — the fix must not
    /// promote models that never had 1M.
    ///
    /// The output ceilings are pinned too, and that is not incidental:
    /// `max_output_tokens` becomes `ReservedContext::response_reservation_tokens`,
    /// which is *subtracted* from the usable window. Raising a legacy model's
    /// output ceiling without also raising its context window would quietly
    /// shrink the room left for conversation. Output ceilings may only rise on
    /// a model whose window rose with it.
    #[test]
    fn legacy_claude_keeps_its_window_and_its_output_reservation() {
        for (model, output) in [
            ("claude-opus-4-20250514", 32_768),
            ("claude-opus-4-1-20250805", 32_768),
            ("claude-opus-4-5-20251101", 32_768),
            ("claude-sonnet-4-20250514", 65_536),
            ("claude-sonnet-4-5-20250929", 65_536),
            ("claude-haiku-4-5-20251001", 65_536),
        ] {
            let limits = model_limits(model).unwrap_or_else(|| panic!("{model} is known"));
            assert_eq!(limits.context_tokens, 200_000, "{model} context");
            assert_eq!(limits.max_output_tokens, output, "{model} reservation");
        }
    }

    /// The bug was silent because `effective_context_limit` takes a min: a
    /// too-small table value is indistinguishable from a deliberate config cap.
    /// Pin the resolution so a regression shows up as a limit, not as symptoms.
    #[test]
    fn effective_limit_lets_current_claude_use_its_real_window() {
        assert_eq!(
            effective_context_limit("anthropic.claude-sonnet-5", 1_000_000),
            1_000_000
        );
        // A configured cap still wins when it is genuinely lower...
        assert_eq!(
            effective_context_limit("anthropic.claude-sonnet-5", 200_000),
            200_000
        );
        // ...but the model no longer imposes one of its own.
        assert_eq!(
            effective_context_limit("anthropic.claude-sonnet-5", 400_000),
            400_000
        );
    }

    #[test]
    fn grok_4_limits() {
        let limits = model_limits("grok-4").unwrap();
        assert_eq!(limits.context_tokens, 131_072);
        assert_eq!(limits.max_output_tokens, 32_768);
    }

    #[test]
    fn grok_3_limits() {
        let limits = model_limits("grok-3-latest").unwrap();
        assert_eq!(limits.context_tokens, 131_072);
        assert_eq!(limits.max_output_tokens, 32_768);
    }

    #[test]
    fn gemini_pro_limits() {
        let limits = model_limits("gemini-2.5-pro-preview-05-06").unwrap();
        assert_eq!(limits.context_tokens, 1_000_000);
        assert_eq!(limits.max_output_tokens, 65_536);
    }

    #[test]
    fn gemini_flash_limits() {
        let limits = model_limits("gemini-2.5-flash-preview-04-17").unwrap();
        assert_eq!(limits.context_tokens, 1_000_000);
        assert_eq!(limits.max_output_tokens, 65_536);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(model_limits("llama-3-70b").is_none());
        assert!(model_limits("mistral-large").is_none());
        assert!(model_limits("").is_none());
        assert!(model_limits("some-random-model").is_none());
    }

    #[test]
    fn direct_glm5_series_is_large_context() {
        // Regression: `glm-5.2` (fabrikam edge) fell through to the unknown path,
        // so the tool filter treated it as sub-large-context and stripped its
        // tools once the count passed the threshold. The whole direct GLM-5
        // series resolves to the 200K large-context tier.
        for model in ["glm-5", "glm-5.1", "glm-5.2", "glm-5.2-air"] {
            let limits = model_limits(model).unwrap_or_else(|| panic!("{model} is known"));
            assert_eq!(limits.context_tokens, 200_000, "{model}");
        }
        // Provider-prefixed variants keep their own (smaller) registrations.
        assert_eq!(model_limits("zai-glm-5").unwrap().context_tokens, 131_072);
        assert_eq!(model_limits("z-ai/glm-5").unwrap().context_tokens, 202_752);
    }

    #[test]
    fn bedrock_zai_glm_models_resolve() {
        // Regression: the dotted Bedrock ids (`zai.glm-…`) were missed by the
        // hyphen/slash registry patterns, leaving `max_tokens` unclamped (Candy).
        let glm5 = model_limits("zai.glm-5").expect("zai.glm-5 is known");
        assert_eq!(glm5.max_output_tokens, 40_000);
        let glm47 = model_limits("zai.glm-4.7").expect("zai.glm-4.7 is known");
        assert_eq!(glm47.max_output_tokens, 40_000);
        assert!(
            model_limits("zai.glm-4.7-flash").is_some(),
            "GLM-4.7-flash should resolve via the zai.glm-4.7 prefix"
        );
        assert_eq!(
            effective_context_limit("zai.glm-5", 200_000),
            131_072,
            "config max above the registry value must clamp down"
        );
    }

    // ─── effective_context_limit() tests ─────────────────────────────────────

    #[test]
    fn effective_limit_known_model_smaller_than_config() {
        // Grok has 131K, config says 200K → should use 131K
        assert_eq!(effective_context_limit("grok-4", 200_000), 131_072);
    }

    #[test]
    fn effective_limit_known_model_larger_than_config() {
        // Gemini has 1M, config says 200K → should use 200K
        assert_eq!(effective_context_limit("gemini-2.5-pro", 200_000), 200_000);
    }

    #[test]
    fn effective_limit_known_model_equal_to_config() {
        // Claude has 200K, config says 200K → should use 200K
        assert_eq!(
            effective_context_limit("claude-opus-4-20250514", 200_000),
            200_000
        );
    }

    #[test]
    fn effective_limit_unknown_model_uses_config() {
        assert_eq!(effective_context_limit("llama-3-70b", 250_000), 250_000);
    }

    // ─── CompactionConfig-aware function tests ───────────────────────────────

    fn test_config() -> CompactionConfig {
        CompactionConfig {
            max_context_tokens: 200_000,
            compaction_threshold: 0.8,
            urgent_compaction_threshold: 0.95,
            ..Default::default()
        }
    }

    #[test]
    fn effective_context_for_grok_model() {
        let config = test_config();
        // Grok limit (131K) < config (200K), so effective = 131K
        assert_eq!(effective_context_for_model(&config, "grok-4"), 131_072);
    }

    #[test]
    fn effective_context_for_gemini_model() {
        let config = test_config();
        // Gemini limit (1M) > config (200K), so effective = 200K
        assert_eq!(
            effective_context_for_model(&config, "gemini-2.5-pro"),
            200_000
        );
    }

    #[test]
    fn trigger_threshold_for_grok() {
        let config = test_config();
        // Grok effective = 131_072, threshold = 0.8 → 104_857
        let threshold = trigger_threshold_for_model(&config, "grok-4");
        assert_eq!(threshold, (131_072_f32 * 0.8) as u32);
        assert_eq!(threshold, 104_857);
    }

    #[test]
    fn trigger_threshold_for_claude() {
        let config = test_config();
        // Claude effective = 200_000, threshold = 0.8 → 160_000
        let threshold = trigger_threshold_for_model(&config, "claude-opus-4-20250514");
        assert_eq!(threshold, (200_000_f32 * 0.8) as u32);
    }

    #[test]
    fn urgent_threshold_for_grok() {
        let config = test_config();
        // Grok effective = 131_072, urgent = 0.95 → 124_518
        let threshold = urgent_threshold_for_model(&config, "grok-4");
        assert_eq!(threshold, (131_072_f32 * 0.95) as u32);
    }

    #[test]
    fn thresholds_for_unknown_model_use_config_defaults() {
        let config = test_config();
        let trigger = trigger_threshold_for_model(&config, "unknown-model");
        let urgent = urgent_threshold_for_model(&config, "unknown-model");
        assert_eq!(trigger, (200_000_f32 * 0.8) as u32);
        assert_eq!(urgent, (200_000_f32 * 0.95) as u32);
    }

    // ─── effective_max_tokens_for_model() tests ─────────────────────────────

    fn test_model_overrides() -> std::collections::HashMap<String, u32> {
        let mut map = std::collections::HashMap::new();
        map.insert("claude-opus-4-6".to_string(), 128_000);
        map.insert("claude-sonnet-4-5-20250929".to_string(), 64_000);
        map.insert("grok-4".to_string(), 32_000);
        map
    }

    #[test]
    fn exact_match_uses_override() {
        let overrides = test_model_overrides();
        assert_eq!(
            effective_max_tokens_for_model("claude-opus-4-6", &overrides, 8192),
            128_000
        );
    }

    #[test]
    fn exact_match_takes_priority() {
        let overrides = test_model_overrides();
        assert_eq!(
            effective_max_tokens_for_model("claude-sonnet-4-5-20250929", &overrides, 8192),
            64_000
        );
    }

    #[test]
    fn substring_match_works() {
        let overrides = test_model_overrides();
        // "claude-opus-4-6-20260301" contains "claude-opus-4-6"
        assert_eq!(
            effective_max_tokens_for_model("claude-opus-4-6-20260301", &overrides, 8192),
            128_000
        );
    }

    #[test]
    fn no_match_uses_fallback() {
        let overrides = test_model_overrides();
        assert_eq!(
            effective_max_tokens_for_model("llama-3-70b", &overrides, 8192),
            8192
        );
    }

    #[test]
    fn empty_overrides_uses_fallback() {
        let overrides = std::collections::HashMap::new();
        assert_eq!(
            effective_max_tokens_for_model("claude-opus-4-6", &overrides, 16_384),
            16_384
        );
    }
}

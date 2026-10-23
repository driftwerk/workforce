//! Vision-capable model registry — capability source of truth for attachment routing.
//!
//! Membership is the predicate: any model listed in `default_vision_tiers.toml`
//! (or a workspace `vision_tiers.toml` override) is considered vision-capable,
//! as is every Claude model whether listed or not. Other models omitted from
//! the registry are treated as text-only and trigger the configured
//! `[llm.vision]` fallback for image attachments.
//!
//! Deliberately separate from [`super::tiers::TierRegistry`] so the capability
//! check is `O(1)` against a flat `HashSet` and so adding a vision-capable
//! variant of a model doesn't require touching the primary tier table.

use std::collections::{HashMap, HashSet};

use eyre::Result;

use super::provider::ProviderKind;
use super::tiers::{
    parse_provider_tier_key, parse_tier_table, resolve_model_spec, split_routing_prefix,
    tier_specs, ModelSpecError, SpecRoutes,
};

/// Embedded default vision tiers TOML.
pub const DEFAULT_VISION_TIERS_TOML: &str = include_str!("default_vision_tiers.toml");

/// Registry mapping vision-capable models to tier keys per provider.
///
/// Provider keys use dots for meta-provider upstreams just like
/// [`super::tiers::TierRegistry`]: `"anthropic"`, `"openrouter.anthropic"`,
/// `"gemini"`, `"openrouter.glm"`, etc.
#[derive(Debug, Clone)]
pub struct VisionTierRegistry {
    providers: HashMap<String, HashMap<String, String>>,
    capable_models: HashSet<String>,
}

impl VisionTierRegistry {
    /// Load the registry from the embedded defaults.
    pub fn from_defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_VISION_TIERS_TOML)
    }

    /// Parse a vision_tiers.toml string into a registry.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let providers = parse_tier_table(toml_str)?;
        Ok(Self::with_providers(providers))
    }

    fn with_providers(providers: HashMap<String, HashMap<String, String>>) -> Self {
        let capable_models = providers
            .values()
            .flat_map(|tiers| tiers.values().cloned())
            .collect();
        Self {
            providers,
            capable_models,
        }
    }

    /// Merge a workspace override on top. Existing tier entries keep their
    /// values for any tier the override doesn't redefine; new providers and
    /// new tiers are added.
    pub fn merge_toml(&mut self, toml_str: &str) -> Result<()> {
        let other = parse_tier_table(toml_str)?;
        for (key, tiers) in other {
            self.providers.entry(key).or_default().extend(tiers);
        }
        self.capable_models = self
            .providers
            .values()
            .flat_map(|tiers| tiers.values().cloned())
            .collect();
        Ok(())
    }

    /// Whether `model` reads images: the registry lists it, or it is a Claude
    /// model. Named with or without the route it is called on
    /// (`"chatgpt:gpt-6.1-sol"`). Tier shorthand must already be resolved to a
    /// model. Whether that route shows it an image is
    /// [`ProviderKind::carries_images`].
    pub fn is_vision_capable(&self, model: &str) -> bool {
        let model = split_routing_prefix(model).1;
        self.capable_models.contains(model) || is_claude(model)
    }

    /// The provider and model a `[llm.vision]` spec names, read as the main
    /// registry reads a route:
    ///   - `"anthropic:cheap"` → `(Anthropic, "claude-haiku-5-5")`
    ///   - `"openrouter:anthropic:cheap"` → `(OpenRouter, "anthropic/claude-haiku-5.5")`
    ///   - `"chatgpt:gpt-6.1-sol"` → `(ChatGpt, "gpt-6.1-sol")`
    ///   - a bare tier or model is `default_provider`'s
    pub fn resolve_routed_vision_model(
        &self,
        spec: &str,
        default_provider: Option<&str>,
    ) -> Result<(ProviderKind, String), ModelSpecError> {
        let (provider, model) = resolve_model_spec(
            &self.providers,
            spec,
            default_provider,
            SpecRoutes {
                split: split_routing_prefix,
                tier_key: parse_provider_tier_key,
            },
        )?;
        ProviderKind::parse(&provider)
            .map(|kind| (kind, model))
            .map_err(|e| ModelSpecError::Provider {
                spec: spec.to_string(),
                reason: e.to_string(),
            })
    }

    /// Every `provider:tier` spec the registry answers.
    pub fn tier_specs(&self) -> impl Iterator<Item = String> + '_ {
        tier_specs(&self.providers)
    }
}

/// Every Claude model since Claude 3 reads images, on every host: Anthropic
/// (`claude-sonnet-5-5`), Bedrock (`eu.anthropic.claude-sonnet-5-5`), Vertex
/// (`claude-haiku-4-5@20251001`) and OpenRouter (`anthropic/claude-sonnet-5.5`).
/// A family rule rather than a list, so the generation an agent is moved to
/// reads images the day it ships: listing only the tier models left
/// `eu.anthropic.claude-sonnet-5-5` text-only, and its images went to a
/// describer.
fn is_claude(model: &str) -> bool {
    let id = super::bedrock_capabilities::strip_region_prefix(model);
    let id = id
        .strip_prefix("anthropic.")
        .or_else(|| id.strip_prefix("anthropic/"))
        .unwrap_or(id);
    id.starts_with("claude-") && !id.starts_with("claude-2") && !id.starts_with("claude-instant")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> VisionTierRegistry {
        VisionTierRegistry::from_defaults().unwrap()
    }

    #[test]
    fn loads_defaults() {
        let r = reg();
        assert!(r.providers.contains_key("anthropic"));
        assert!(r.providers.contains_key("openrouter.anthropic"));
        assert!(r.providers.contains_key("gemini"));
        // Deliberately absent — text-only providers.
        assert!(!r.providers.contains_key("openrouter.deepseek"));
        assert!(!r.providers.contains_key("grok"));
    }

    #[test]
    fn anthropic_models_are_vision_capable() {
        let r = reg();
        assert!(r.is_vision_capable("claude-haiku-4-5"));
        assert!(r.is_vision_capable("claude-sonnet-5"));
        assert!(r.is_vision_capable("claude-opus-4-8"));
    }

    #[test]
    fn gemini_models_are_vision_capable() {
        let r = reg();
        assert!(r.is_vision_capable("gemini-3.8-flash"));
        assert!(r.is_vision_capable("gemini-3.1-pro-preview"));
        assert!(r.is_vision_capable("gemini-3.5-flash-lite"));
    }

    #[test]
    fn openrouter_passthroughs_are_vision_capable() {
        let r = reg();
        assert!(r.is_vision_capable("anthropic/claude-haiku-4.5"));
        assert!(r.is_vision_capable("google/gemini-3.8-flash"));
        assert!(r.is_vision_capable("openai/gpt-6-sol"));
    }

    #[test]
    fn vl_specific_models_are_vision_capable() {
        let r = reg();
        assert!(r.is_vision_capable("qwen/qwen3.8-27b"));
        assert!(r.is_vision_capable("z-ai/glm-5.3-flash"));
        assert!(r.is_vision_capable("Qwen/Qwen2.5-VL-7B-Instruct"));
    }

    #[test]
    fn text_only_models_are_not_vision_capable() {
        let r = reg();
        assert!(!r.is_vision_capable("deepseek/deepseek-v3.2"));
        assert!(!r.is_vision_capable("moonshotai/kimi-k2.5"));
        assert!(!r.is_vision_capable("qwen/qwen3-coder"));
        assert!(!r.is_vision_capable("zai-glm-4.7")); // Cerebras text-only GLM
        assert!(!r.is_vision_capable("grok-4"));
        assert!(!r.is_vision_capable("glm-5.1")); // Zhipu's text variant
                                                  // Z.AI on Bedrock — confirmed live to reject image modality.
        assert!(!r.is_vision_capable("zai.glm-5"));
        assert!(!r.is_vision_capable("zai.glm-4.7"));
        assert!(!r.is_vision_capable("zai.glm-4.7-flash"));
    }

    #[test]
    fn bedrock_anthropic_ids_are_vision_capable() {
        let r = reg();
        for id in [
            // Bare foundation-model ids — what `is_vision_capable` is called with
            // before bedrock.rs rewrites to a cross-region inference profile.
            "anthropic.claude-opus-5-5",
            "anthropic.claude-sonnet-5-5",
            "anthropic.claude-haiku-5-5",
            // `eu.` cross-region inference profile prefix.
            "eu.anthropic.claude-opus-5-5",
            "eu.anthropic.claude-sonnet-5-5",
            "eu.anthropic.claude-haiku-5-5",
            // `global.` cross-region inference profile prefix.
            "global.anthropic.claude-opus-5-5",
            "global.anthropic.claude-sonnet-5-5",
            "global.anthropic.claude-haiku-5-5",
        ] {
            assert!(r.is_vision_capable(id), "{id} should be vision-capable");
        }
    }

    fn routed(spec: &str, default_provider: Option<&str>) -> (ProviderKind, String) {
        reg()
            .resolve_routed_vision_model(spec, default_provider)
            .unwrap()
    }

    #[test]
    fn resolve_provider_tier_shorthand() {
        assert_eq!(
            routed("anthropic:cheap", None),
            (ProviderKind::Anthropic, "claude-haiku-5-5".into())
        );
        assert_eq!(
            routed("gemini:standard", None),
            (ProviderKind::Gemini, "gemini-3.8-flash".into())
        );
        assert_eq!(
            routed("chatgpt:cheap", None),
            (ProviderKind::ChatGpt, "gpt-6-luna".into())
        );
    }

    #[test]
    fn resolve_openrouter_upstream_shorthand() {
        assert_eq!(
            routed("openrouter:anthropic:cheap", None),
            (
                ProviderKind::OpenRouter,
                "anthropic/claude-haiku-5.5".into()
            )
        );
        assert_eq!(
            routed("openrouter:google:powerful", None),
            (
                ProviderKind::OpenRouter,
                "google/gemini-3.1-pro-preview".into()
            )
        );
        assert_eq!(
            routed("openrouter:qwen:cheap", None),
            (ProviderKind::OpenRouter, "qwen/qwen3.8-flash".into())
        );
    }

    #[test]
    fn every_provider_route_resolves_to_its_own_provider() {
        for &kind in ProviderKind::ALL {
            let spec = format!("{}:some-model", kind.canonical_name());
            assert_eq!(
                routed(&spec, Some("anthropic")),
                (kind, "some-model".into()),
                "{spec}"
            );
        }
        assert_eq!(
            routed("chatgpt:gpt-6.1-sol", None),
            (ProviderKind::ChatGpt, "gpt-6.1-sol".into())
        );
        assert_eq!(
            routed("google:gemini-3.8-flash", None),
            (ProviderKind::Gemini, "gemini-3.8-flash".into()),
            "an alias routes as its provider"
        );
    }

    #[test]
    fn a_bare_tier_or_model_is_the_default_providers() {
        assert_eq!(
            routed("cheap", Some("anthropic")),
            (ProviderKind::Anthropic, "claude-haiku-5-5".into())
        );
        assert_eq!(
            routed("cheap", Some("openrouter:google")),
            (
                ProviderKind::OpenRouter,
                "google/gemini-3.5-flash-lite".into()
            )
        );
        assert_eq!(
            routed("qwen2.5vl:7b", Some("ollama")),
            (ProviderKind::Ollama, "qwen2.5vl:7b".into()),
            "a colon in a model id is not a route"
        );
        assert_eq!(
            routed("anthropic/claude-haiku-5.5", Some("openrouter")),
            (
                ProviderKind::OpenRouter,
                "anthropic/claude-haiku-5.5".into()
            )
        );
    }

    #[test]
    fn a_spec_naming_no_provider_is_refused_not_given_one() {
        let r = reg();
        for spec in [
            "cheap",
            "gpt-6.1-sol",
            "nonesuch:gpt-6.1-sol",
            "nonesuch:cheap",
        ] {
            assert_eq!(
                r.resolve_routed_vision_model(spec, None),
                Err(ModelSpecError::NoProvider { spec: spec.into() }),
                "{spec}"
            );
        }
        assert!(matches!(
            r.resolve_routed_vision_model("gpt-6.1-sol", Some("nonesuch")),
            Err(ModelSpecError::Provider { reason, .. }) if reason.contains("nonesuch")
        ));
    }

    #[test]
    fn a_tier_the_registry_does_not_list_is_refused() {
        let r = reg();
        assert_eq!(
            r.resolve_routed_vision_model("grok:cheap", None),
            Err(ModelSpecError::NoTier {
                spec: "grok:cheap".into(),
                tier_key: "grok".into(),
                tier: "cheap",
            })
        );
        assert!(matches!(
            r.resolve_routed_vision_model("openrouter:cheap", None),
            Err(ModelSpecError::Provider { reason, .. }) if reason.contains("upstream")
        ));
    }

    #[test]
    fn a_routed_model_is_capable_when_its_model_is() {
        let r = reg();
        assert!(r.is_vision_capable("chatgpt:gpt-6.1-sol"));
        assert!(r.is_vision_capable("gpt-6.1-sol"));
        assert!(r.is_vision_capable("gemini:gemini-3.8-flash"));
        assert!(!r.is_vision_capable("grok:grok-4"));
        assert!(!r.is_vision_capable("chatgpt:gpt-5.2"));
    }

    #[test]
    fn every_default_tier_resolves_on_a_provider_whose_requests_carry_images() {
        let r = reg();
        let specs: Vec<String> = r.tier_specs().collect();
        assert!(specs.len() >= 3 * r.providers.len(), "{specs:?}");
        for spec in specs {
            let (kind, model) = r
                .resolve_routed_vision_model(&spec, None)
                .unwrap_or_else(|e| panic!("{e}"));
            assert!(
                kind.carries_images(),
                "{spec} lists {model} as seeing, on a provider that sends it no image"
            );
            assert!(r.is_vision_capable(&model), "{spec}");
        }
        for text_only in ["ollama", "zai", "grok"] {
            assert!(!r.providers.contains_key(text_only), "{text_only}");
        }
    }

    #[test]
    fn merge_workspace_override() {
        let mut r = reg();
        r.merge_toml(
            r#"
[gemini]
cheap = "gemini-future-lite"

[openrouter.custom]
standard = "custom/vision-model"
"#,
        )
        .unwrap();
        assert!(r.is_vision_capable("gemini-future-lite"));
        assert!(r.is_vision_capable("custom/vision-model"));
        // Old default no longer in registry (overwritten at the cheap tier).
        assert!(!r.is_vision_capable("gemini-3.5-flash-lite"));
        // Other tiers preserved.
        assert!(r.is_vision_capable("gemini-3.8-flash"));
    }

    /// Regression: only the tier models were listed, so moving an agent to
    /// `eu.anthropic.claude-sonnet-5-5` made it text-only and its images went
    /// to the describer. Every Claude reads images, listed or not, in every id
    /// form — and the generation an agent moves off keeps reading them too.
    #[test]
    fn every_claude_generation_is_vision_capable_in_every_id_form() {
        let r = reg();
        for version in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001-v1:0",
        ] {
            for id in [
                version.to_string(),
                format!("anthropic.{version}"),
                format!("eu.anthropic.{version}"),
                format!("global.anthropic.{version}"),
                format!("bedrock:eu.anthropic.{version}"),
                format!("vertex:{version}"),
            ] {
                assert!(r.is_vision_capable(&id), "{id} should read images");
            }
        }
        for id in [
            "anthropic/claude-sonnet-5.5",
            "anthropic/claude-haiku-5.5",
            "claude-haiku-4-5@20251001",
        ] {
            assert!(r.is_vision_capable(id), "{id} should read images");
        }
        for id in ["claude-2.1", "anthropic.claude-instant-v1", "claudette"] {
            assert!(!r.is_vision_capable(id), "{id} is not a vision Claude");
        }
    }
}

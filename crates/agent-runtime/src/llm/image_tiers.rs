//! Image-generation model registry — resolves `[llm.image].model` specs.
//!
//! Mirrors [`super::vision_tiers::VisionTierRegistry`] but for text->image
//! generation. The same `provider:upstream:tier` shorthand grammar resolves
//! against `default_image_tiers.toml` (overridable per workspace).
//!
//! Kept deliberately separate from the vision and primary tier tables so the
//! image-model set evolves independently — adding an image provider never
//! touches chat or vision routing.

use std::collections::HashMap;

use eyre::Result;

use super::tiers::{
    parse_provider_tier_key, parse_tier_table, resolve_model_spec, split_routing_prefix,
    tier_specs, ModelSpecError, SpecRoutes,
};

/// Embedded default image tiers TOML.
pub const DEFAULT_IMAGE_TIERS_TOML: &str = include_str!("default_image_tiers.toml");

/// Registry mapping tier keys to image-generation models per provider.
///
/// Provider keys use dots for meta-provider upstreams just like
/// [`super::tiers::TierRegistry`]: `"gemini"`, `"openrouter.google"`,
/// `"bedrock.stability"`, `"bedrock.nova"`, `"vllm"`.
#[derive(Debug, Clone)]
pub struct ImageTierRegistry {
    providers: HashMap<String, HashMap<String, String>>,
}

impl ImageTierRegistry {
    /// Load the registry from the embedded defaults.
    pub fn from_defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_IMAGE_TIERS_TOML)
    }

    /// Parse an image_tiers.toml string into a registry.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        Ok(Self {
            providers: parse_tier_table(toml_str)?,
        })
    }

    /// Merge a workspace override on top. New providers and new tiers are
    /// added; redefined tiers replace the default.
    pub fn merge_toml(&mut self, toml_str: &str) -> Result<()> {
        let other = parse_tier_table(toml_str)?;
        for (key, tiers) in other {
            self.providers.entry(key).or_default().extend(tiers);
        }
        Ok(())
    }

    /// The provider name and model an `[llm.image]` spec names:
    ///   - `"gemini:standard"` → `("gemini", "gemini-3.1-flash-image")`
    ///   - `"bedrock:stability:standard"` → `("bedrock", "stability.stable-image-ultra-v1:1")`
    ///   - a bare tier or model is `default_provider`'s
    pub fn resolve_routed_image_model(
        &self,
        spec: &str,
        default_provider: Option<&str>,
    ) -> Result<(String, String), ModelSpecError> {
        resolve_model_spec(
            &self.providers,
            spec,
            default_provider,
            SpecRoutes {
                split: split_routing_prefix,
                tier_key: parse_provider_tier_key,
            },
        )
    }

    /// Whether the registry lists `model`.
    pub fn lists(&self, model: &str) -> bool {
        self.providers
            .values()
            .any(|tiers| tiers.values().any(|listed| listed == model))
    }

    /// Every `provider:tier` spec the registry answers.
    pub fn tier_specs(&self) -> impl Iterator<Item = String> + '_ {
        tier_specs(&self.providers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> ImageTierRegistry {
        ImageTierRegistry::from_defaults().unwrap()
    }

    fn routed(spec: &str, default_provider: Option<&str>) -> (String, String) {
        reg()
            .resolve_routed_image_model(spec, default_provider)
            .unwrap()
    }

    #[test]
    fn loads_defaults() {
        let r = reg();
        assert!(r.providers.contains_key("gemini"));
        assert!(r.providers.contains_key("openrouter.google"));
        assert!(r.providers.contains_key("bedrock.stability"));
        assert!(r.providers.contains_key("bedrock.nova"));
    }

    #[test]
    fn a_tier_spec_resolves_on_the_provider_it_names() {
        for (spec, provider, model) in [
            ("gemini:standard", "gemini", "gemini-3.1-flash-image"),
            ("gemini:powerful", "gemini", "gemini-3-pro-image"),
            (
                "bedrock:stability:standard",
                "bedrock",
                "stability.stable-image-ultra-v1:1",
            ),
            (
                "bedrock:nova:standard",
                "bedrock",
                "amazon.nova-canvas-v1:0",
            ),
            (
                "bedrock:titan:standard",
                "bedrock",
                "amazon.titan-image-generator-v2:0",
            ),
            (
                "openrouter:google:standard",
                "openrouter",
                "google/gemini-3.1-flash-image",
            ),
            (
                "openrouter:google:cheap",
                "openrouter",
                "google/gemini-3.1-flash-lite-image",
            ),
        ] {
            assert_eq!(
                routed(spec, None),
                (provider.to_string(), model.to_string()),
                "{spec}"
            );
        }
    }

    #[test]
    fn a_bare_tier_or_model_is_the_default_providers() {
        assert_eq!(
            routed("powerful", Some("bedrock:stability")),
            (
                "bedrock".to_string(),
                "stability.stable-image-ultra-v1:1".to_string()
            )
        );
        assert_eq!(
            routed("amazon.nova-canvas-v1:0", Some("bedrock")),
            ("bedrock".to_string(), "amazon.nova-canvas-v1:0".to_string())
        );
        assert_eq!(
            routed("gemini:gemini-2.5-flash-image", None),
            ("gemini".to_string(), "gemini-2.5-flash-image".to_string())
        );
    }

    #[test]
    fn a_spec_naming_no_provider_is_refused_not_given_one() {
        let r = reg();
        for spec in ["standard", "gemini-2.5-flash-image", "nonesuch:standard"] {
            assert_eq!(
                r.resolve_routed_image_model(spec, None),
                Err(ModelSpecError::NoProvider { spec: spec.into() }),
                "{spec}"
            );
        }
        assert!(matches!(
            r.resolve_routed_image_model("anthropic:standard", None),
            Err(ModelSpecError::NoTier { tier_key, .. }) if tier_key == "anthropic"
        ));
    }

    #[test]
    fn merge_workspace_override() {
        let mut r = reg();
        r.merge_toml(
            r#"
[gemini]
standard = "imagen-5-preview"

[fal.flux]
standard = "fal-ai/flux-pro/v2"
"#,
        )
        .unwrap();
        assert_eq!(
            r.resolve_routed_image_model("gemini:standard", None)
                .unwrap()
                .1,
            "imagen-5-preview"
        );
        assert_eq!(
            r.resolve_routed_image_model("fal:flux:standard", None)
                .unwrap(),
            ("fal".to_string(), "fal-ai/flux-pro/v2".to_string())
        );
    }
}

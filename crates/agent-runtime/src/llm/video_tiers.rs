//! Video-generation model registry — resolves `[llm.video].model` specs.
//!
//! Mirrors [`super::image_tiers::ImageTierRegistry`] but for video generation.
//! The same `provider:tier` shorthand grammar resolves against
//! `default_video_tiers.toml` (overridable per workspace via `video_tiers.toml`).

use std::collections::HashMap;

use eyre::Result;

use super::tiers::{parse_tier_table, resolve_model_spec, tier_specs, ModelSpecError, SpecRoutes};

/// Embedded default video tiers TOML.
pub const DEFAULT_VIDEO_TIERS_TOML: &str = include_str!("default_video_tiers.toml");

/// Registry mapping tier keys to video-generation models per provider.
#[derive(Debug, Clone)]
pub struct VideoTierRegistry {
    providers: HashMap<String, HashMap<String, String>>,
}

impl VideoTierRegistry {
    /// Load the registry from the embedded defaults.
    pub fn from_defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_VIDEO_TIERS_TOML)
    }

    /// Parse a video_tiers.toml string into a registry.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        Ok(Self {
            providers: parse_tier_table(toml_str)?,
        })
    }

    /// Merge a workspace override on top.
    pub fn merge_toml(&mut self, toml_str: &str) -> Result<()> {
        let other = parse_tier_table(toml_str)?;
        for (key, tiers) in other {
            self.providers.entry(key).or_default().extend(tiers);
        }
        Ok(())
    }

    /// The Veo model an `[llm.video]` spec names:
    ///   - `"veo:standard"` → `"veo-3.1-fast-generate-preview"`
    ///   - `"veo:veo-3.1-generate-preview"` → `"veo-3.1-generate-preview"`
    ///   - a bare tier or model is `default_provider`'s, which must be Veo
    pub fn resolve_video_model(
        &self,
        spec: &str,
        default_provider: Option<&str>,
    ) -> Result<String, ModelSpecError> {
        if let Some(Err(reason)) = default_provider.map(veo_tier_key) {
            return Err(ModelSpecError::Provider {
                spec: spec.to_string(),
                reason: reason.to_string(),
            });
        }
        resolve_model_spec(
            &self.providers,
            spec,
            default_provider,
            SpecRoutes {
                split: split_route,
                tier_key: veo_tier_key,
            },
        )
        .map(|(_, model)| model)
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

/// The one video backend, and the tier key its models are listed under.
pub const VEO: &str = "veo";

/// Veo answers to its own name and to Gemini's, whose API and key it is
/// called with.
fn is_veo(provider: &str) -> bool {
    ["veo", "gemini", "google"]
        .iter()
        .any(|name| provider.eq_ignore_ascii_case(name))
}

fn split_route(spec: &str) -> (Option<&str>, &str) {
    match spec.split_once(':') {
        Some((prefix, model)) if is_veo(prefix) => (Some(prefix), model),
        _ => (None, spec),
    }
}

fn veo_tier_key(provider: &str) -> Result<(String, String)> {
    if is_veo(provider) {
        Ok((VEO.to_string(), VEO.to_string()))
    } else {
        eyre::bail!("'{provider}' generates no video; the video provider is veo")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> VideoTierRegistry {
        VideoTierRegistry::from_defaults().unwrap()
    }

    fn model(spec: &str, default_provider: Option<&str>) -> String {
        reg().resolve_video_model(spec, default_provider).unwrap()
    }

    #[test]
    fn loads_defaults() {
        assert!(reg().providers.contains_key(VEO));
    }

    #[test]
    fn a_veo_spec_resolves_under_any_of_its_names() {
        assert_eq!(model("veo:standard", None), "veo-3.1-fast-generate-preview");
        assert_eq!(model("veo:powerful", None), "veo-3.1-generate-preview");
        assert_eq!(model("gemini:powerful", None), "veo-3.1-generate-preview");
        assert_eq!(
            model("veo:veo-3.1-generate-preview", None),
            "veo-3.1-generate-preview"
        );
    }

    #[test]
    fn a_bare_tier_or_model_is_the_default_providers() {
        assert_eq!(
            model("standard", Some("veo")),
            "veo-3.1-fast-generate-preview"
        );
        assert_eq!(
            model("cheap", Some("gemini")),
            "veo-3.1-lite-generate-preview"
        );
        assert_eq!(
            model("veo-3.1-generate-preview", Some("veo")),
            "veo-3.1-generate-preview"
        );
    }

    #[test]
    fn a_spec_naming_no_provider_is_refused_not_given_one() {
        let r = reg();
        for spec in ["standard", "veo-3.1-generate-preview", "nonesuch:standard"] {
            assert_eq!(
                r.resolve_video_model(spec, None),
                Err(ModelSpecError::NoProvider { spec: spec.into() }),
                "{spec}"
            );
        }
        for spec in ["standard", "veo-3.1-generate-preview"] {
            assert!(
                matches!(
                    r.resolve_video_model(spec, Some("anthropic")),
                    Err(ModelSpecError::Provider { reason, .. }) if reason.contains("generates no video")
                ),
                "{spec}"
            );
        }
    }
}

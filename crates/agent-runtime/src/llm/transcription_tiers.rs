//! Transcription model registry — capability source of truth for audio routing.
//!
//! Membership is the predicate: any model listed in
//! `default_transcription_tiers.toml` (or a workspace
//! `transcription_tiers.toml` override) is considered transcription-capable.
//! Providers omitted from the registry are unsupported and trigger a
//! placeholder text block when an audio attachment is routed through them.
//!
//! Mirrors [`super::vision_tiers::VisionTierRegistry`]: same shape, separate
//! registry so the capability check is `O(1)` and adding a transcription
//! variant doesn't need to touch the primary tier table.

use std::collections::{HashMap, HashSet};

use eyre::Result;

use super::provider::ProviderKind;
use super::tiers::{
    parse_provider_tier_key, parse_tier_table, resolve_model_spec, tier_specs, ModelSpecError,
    SpecRoutes,
};

/// The provider name a user-supplied Whisper-shaped endpoint is configured by.
pub const CUSTOM_PROVIDER: &str = "custom";

/// Embedded default transcription tiers TOML.
pub const DEFAULT_TRANSCRIPTION_TIERS_TOML: &str = include_str!("default_transcription_tiers.toml");

/// Registry mapping transcription-capable models to tier keys per provider.
///
/// Provider keys use dots for meta-provider upstreams: `"openai"`,
/// `"groq"`, `"openrouter.google"`, `"gemini"`, `"vllm"`, etc.
#[derive(Debug, Clone)]
pub struct TranscriptionTierRegistry {
    providers: HashMap<String, HashMap<String, String>>,
    capable_models: HashSet<String>,
}

impl TranscriptionTierRegistry {
    pub fn from_defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_TRANSCRIPTION_TIERS_TOML)
    }

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

    /// True iff the concrete model name is in the registry. Callers must
    /// resolve tier shorthand (`"groq:cheap"`) to a concrete model first via
    /// [`Self::resolve_routed_transcription_model`].
    pub fn is_supported(&self, concrete_model: &str) -> bool {
        self.capable_models.contains(concrete_model)
    }

    /// The provider name and model a `[llm.transcription]` spec names:
    ///   - `"groq:cheap"` → `("groq", "distil-whisper-large-v3-en")`
    ///   - `"openrouter:google:cheap"` → `("openrouter", "google/gemini-3.1-flash-lite-preview")`
    ///   - `"groq:whisper-large-v3"` → `("groq", "whisper-large-v3")`
    ///   - a bare tier or model is `default_provider`'s
    ///
    /// The name is canonical but not vetted: whether that provider can
    /// transcribe is the caller's to refuse.
    pub fn resolve_routed_transcription_model(
        &self,
        spec: &str,
        default_provider: Option<&str>,
    ) -> Result<(String, String), ModelSpecError> {
        resolve_model_spec(
            &self.providers,
            spec,
            default_provider,
            SpecRoutes {
                split: split_route,
                tier_key: parse_provider_tier_key,
            },
        )
        .map(|(provider, model)| (canonical_provider(&provider), model))
    }

    /// Every `provider:tier` spec the registry answers.
    pub fn tier_specs(&self) -> impl Iterator<Item = String> + '_ {
        tier_specs(&self.providers)
    }
}

/// Split a spec at a provider prefix: a transcription provider, or any chat
/// provider, so that naming one that cannot transcribe is refused by name
/// instead of being read as a model id.
fn split_route(spec: &str) -> (Option<&str>, &str) {
    match spec.split_once(':') {
        Some((prefix, model))
            if prefix.eq_ignore_ascii_case(CUSTOM_PROVIDER)
                || parse_provider_tier_key(prefix).is_ok()
                || ProviderKind::from_str_loose(prefix).is_some() =>
        {
            (Some(prefix), model)
        }
        _ => (None, spec),
    }
}

fn canonical_provider(name: &str) -> String {
    parse_provider_tier_key(name)
        .map(|(provider, _)| provider)
        .unwrap_or_else(|_| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> TranscriptionTierRegistry {
        TranscriptionTierRegistry::from_defaults().unwrap()
    }

    #[test]
    fn loads_defaults() {
        let r = reg();
        assert!(r.providers.contains_key("openai"));
        assert!(r.providers.contains_key("groq"));
        assert!(r.providers.contains_key("gemini"));
        assert!(r.providers.contains_key("openrouter.google"));
        assert!(r.providers.contains_key("vllm"));
        // Deliberately absent — multimodal wiring not in place.
        assert!(!r.providers.contains_key("grok"));
        assert!(!r.providers.contains_key("openrouter.openai"));
    }

    #[test]
    fn whisper_models_are_supported() {
        let r = reg();
        assert!(r.is_supported("whisper-1"));
        assert!(r.is_supported("whisper-large-v3"));
        assert!(r.is_supported("distil-whisper-large-v3-en"));
    }

    #[test]
    fn gemini_models_are_supported() {
        let r = reg();
        assert!(r.is_supported("gemini-3.8-flash"));
        assert!(r.is_supported("google/gemini-3.5-flash-lite"));
    }

    #[test]
    fn unsupported_models_return_false() {
        let r = reg();
        assert!(!r.is_supported("claude-haiku-4-5"));
        assert!(!r.is_supported("grok-4"));
        assert!(!r.is_supported("openai/gpt-5.4-mini"));
    }

    fn routed(spec: &str, default_provider: Option<&str>) -> (String, String) {
        reg()
            .resolve_routed_transcription_model(spec, default_provider)
            .unwrap()
    }

    #[test]
    fn resolve_provider_tier_shorthand() {
        assert_eq!(
            routed("groq:cheap", None),
            ("groq".into(), "distil-whisper-large-v3-en".into())
        );
        assert_eq!(
            routed("openai:standard", None),
            ("openai".into(), "whisper-1".into())
        );
    }

    #[test]
    fn resolve_openrouter_upstream_shorthand() {
        assert_eq!(
            routed("openrouter:google:cheap", None),
            ("openrouter".into(), "google/gemini-3.5-flash-lite".into())
        );
    }

    #[test]
    fn a_routed_model_keeps_its_own_provider() {
        assert_eq!(
            routed("groq:whisper-large-v3", None),
            ("groq".into(), "whisper-large-v3".into())
        );
        assert_eq!(
            routed("google:gemini-3-flash-preview", None),
            ("gemini".into(), "gemini-3-flash-preview".into())
        );
        assert_eq!(
            routed("custom:my-whisper", None),
            ("custom".into(), "my-whisper".into())
        );
        assert_eq!(
            routed("chatgpt:gpt-6.1-sol", None),
            ("chatgpt".into(), "gpt-6.1-sol".into()),
            "named for the caller to refuse, never read as another provider's model"
        );
    }

    #[test]
    fn a_bare_tier_or_model_is_the_default_providers() {
        assert_eq!(
            routed("cheap", Some("groq")),
            ("groq".into(), "distil-whisper-large-v3-en".into())
        );
        assert_eq!(
            routed("whisper-1", Some("openai")),
            ("openai".into(), "whisper-1".into())
        );
        assert_eq!(
            routed("my-whisper", Some("custom")),
            ("custom".into(), "my-whisper".into())
        );
    }

    #[test]
    fn a_spec_naming_no_provider_is_refused_not_given_one() {
        let r = reg();
        for spec in ["cheap", "whisper-1", "nonesuch:whisper-1"] {
            assert_eq!(
                r.resolve_routed_transcription_model(spec, None),
                Err(ModelSpecError::NoProvider { spec: spec.into() }),
                "{spec}"
            );
        }
        assert!(matches!(
            r.resolve_routed_transcription_model("anthropic:cheap", None),
            Err(ModelSpecError::NoTier { tier_key, .. }) if tier_key == "anthropic"
        ));
    }

    #[test]
    fn merge_workspace_override() {
        let mut r = reg();
        r.merge_toml(
            r#"
[groq]
cheap = "distil-whisper-future"

[custom-provider]
standard = "custom/whisper-xl"
"#,
        )
        .unwrap();
        assert!(r.is_supported("distil-whisper-future"));
        assert!(r.is_supported("custom/whisper-xl"));
        assert!(!r.is_supported("distil-whisper-large-v3-en"));
        // Sibling tiers preserved.
        assert!(r.is_supported("whisper-large-v3"));
    }
}

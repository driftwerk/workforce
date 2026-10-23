//! Video generation providers.
//!
//! A narrow modality abstraction, sibling to [`crate::llm::image`]. A
//! [`VideoProvider`] turns a prompt (+ an optional starting/subject image) into
//! raw video **bytes**. Unlike image generation it is long-running: the only
//! backend, Google Veo, is submit → poll-operation → download (1–6 min). The
//! provider encapsulates that round-trip so callers get bytes back, exactly
//! like the image path; delivery (Slack upload, gateway) is the shared image
//! sink, which already carries arbitrary attachments.

mod veo;

use async_trait::async_trait;
use eyre::{bail, Result};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmVideoGenerationConfig;

use super::image::InputImage;
use super::video_tiers::{VideoTierRegistry, VEO};

/// A request to generate a single video clip.
#[derive(Debug, Clone)]
pub struct VideoRequest {
    /// The positive prompt describing the desired clip / motion.
    pub prompt: String,
    /// Optional starting frame / subject image (image-to-video). When present,
    /// Veo animates from / preserves this image (e.g. "make Tom dance").
    pub input_image: Option<InputImage>,
    /// Aspect ratio, e.g. `"16:9"` or `"9:16"`.
    pub aspect_ratio: String,
    /// Requested duration in seconds (model-clamped); `None` ⇒ provider default.
    pub duration_secs: Option<u32>,
    /// People-generation safety knob (`"allow_adult"` required for real-face
    /// image-to-video on Veo).
    pub person_generation: String,
}

/// One generated video. Raw bytes + media type; nothing about delivery.
#[derive(Clone)]
pub struct GeneratedVideo {
    pub data: Vec<u8>,
    /// MIME type, e.g. `"video/mp4"`.
    pub media_type: String,
}

impl std::fmt::Debug for GeneratedVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedVideo")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Provider-agnostic video generation interface.
#[async_trait]
pub trait VideoProvider: Send + Sync {
    /// Generate one video from a prompt (+ optional input image). Blocks for the
    /// full submit→poll→download round-trip (minutes); callers run it as a
    /// background job or off the latency-critical path.
    async fn generate(&self, req: &VideoRequest) -> Result<GeneratedVideo>;

    /// Confirm the provider's credentials authenticate.
    async fn verify_credentials(&self) -> Result<()>;
}

/// Build a [`VideoProvider`] from a `[llm.video]` config block. Resolves the
/// `model` spec against the video tier registry; `key` is where Veo's key
/// (a Gemini one) is read on every generation. Only Google Veo is supported
/// today. `enabled = false` is treated as "no provider".
pub fn provider_from_config(
    cfg: &LlmVideoGenerationConfig,
    key: KeySlot,
) -> Result<Box<dyn VideoProvider>> {
    if !cfg.enabled {
        bail!("[llm.video] is present but disabled (enabled = false)");
    }
    if cfg.model.trim().is_empty() {
        bail!("[llm.video].model is empty — set a spec like 'veo:standard' or a concrete veo-* model id");
    }

    let model = VideoTierRegistry::from_defaults()?
        .resolve_video_model(&cfg.model, Some(cfg.provider.as_deref().unwrap_or(VEO)))
        .map_err(|e| eyre::eyre!("[llm.video] model {e}"))?;
    Ok(Box::new(veo::VeoVideoProvider::new(key, model)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str) -> LlmVideoGenerationConfig {
        LlmVideoGenerationConfig {
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_veo_provider_from_tier_spec() {
        assert!(provider_from_config(&cfg("veo:standard"), KeySlot::fixed("k")).is_ok());
    }

    #[test]
    fn a_bare_spec_is_veos_and_another_providers_is_refused_by_name() {
        assert!(provider_from_config(&cfg("standard"), KeySlot::fixed("k")).is_ok());
        assert!(
            provider_from_config(&cfg("veo-3.1-generate-preview"), KeySlot::fixed("k")).is_ok()
        );
        for (spec, provider, names) in [
            ("nonesuch:standard", None, "names no known provider"),
            (
                "standard",
                Some("anthropic"),
                "'anthropic' generates no video",
            ),
            (
                "veo-3.1-generate-preview",
                Some("anthropic"),
                "'anthropic' generates no video",
            ),
        ] {
            let config = LlmVideoGenerationConfig {
                provider: provider.map(str::to_string),
                ..cfg(spec)
            };
            let error = match provider_from_config(&config, KeySlot::fixed("k")) {
                Ok(_) => panic!("{spec} on {provider:?} names no Veo model"),
                Err(e) => e.to_string(),
            };
            assert!(
                error.starts_with("[llm.video] model") && error.contains(names),
                "{spec}: {error}"
            );
        }
    }

    #[test]
    fn disabled_block_is_no_provider() {
        let mut c = cfg("veo:standard");
        c.enabled = false;
        assert!(provider_from_config(&c, KeySlot::fixed("k")).is_err());
    }

    #[test]
    fn empty_model_errors() {
        assert!(provider_from_config(&cfg(""), KeySlot::fixed("k")).is_err());
    }
}

//! Video generation providers.
//!
//! A narrow modality abstraction, sibling to [`crate::llm::image`]. A
//! [`VideoProvider`] turns a prompt (+ optional input images) into raw video
//! **bytes**. Unlike image generation it is long-running: the only backend,
//! Gemini Omni, is create → poll → read the clip (15–60s). The provider
//! encapsulates that round-trip so callers get bytes back, exactly like the
//! image path; delivery (Slack upload, gateway) is the shared image sink,
//! which already carries arbitrary attachments.

mod omni;

use std::ops::RangeInclusive;
use std::str::FromStr;

use async_trait::async_trait;
use eyre::{bail, Result};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmVideoGenerationConfig;

use super::image::InputImage;
use super::video_tiers::{VideoTierRegistry, GEMINI};

/// Clip lengths the model accepts, in seconds.
pub const DURATION_SECS: RangeInclusive<u32> = 3..=10;

/// `secs` brought inside [`DURATION_SECS`].
pub fn clamp_duration(secs: u32) -> u32 {
    secs.clamp(*DURATION_SECS.start(), *DURATION_SECS.end())
}

/// The frame shapes the model renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectRatio {
    Landscape,
    Portrait,
}

impl AspectRatio {
    pub const ALL: [Self; 2] = [Self::Landscape, Self::Portrait];

    /// The value on the wire and in config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landscape => "16:9",
            Self::Portrait => "9:16",
        }
    }
}

impl FromStr for AspectRatio {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        parse_wire(&Self::ALL, Self::as_str, "aspect_ratio", raw)
    }
}

/// The output resolutions the model renders (1080p and 4k are upscaled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    P360,
    P720,
    P1080,
    K4,
}

impl Resolution {
    pub const ALL: [Self; 4] = [Self::P360, Self::P720, Self::P1080, Self::K4];

    /// The value on the wire and in config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P360 => "360p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::K4 => "4k",
        }
    }
}

impl FromStr for Resolution {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        parse_wire(&Self::ALL, Self::as_str, "resolution", raw)
    }
}

fn parse_wire<T: Copy>(
    all: &[T],
    wire: fn(T) -> &'static str,
    field: &str,
    raw: &str,
) -> Result<T, String> {
    all.iter()
        .copied()
        .find(|value| wire(*value).eq_ignore_ascii_case(raw.trim()))
        .ok_or_else(|| {
            let allowed: Vec<&str> = all.iter().copied().map(wire).collect();
            format!("{field} {raw:?} is not one of: {}", allowed.join(", "))
        })
}

/// A request to generate a single video clip.
#[derive(Debug, Clone)]
pub struct VideoRequest {
    /// The positive prompt describing the desired clip / motion.
    pub prompt: String,
    /// Images in prompt order: one is animated; two are the first and last
    /// frame; more are subject references (`<IMAGE_REF_N>` in the prompt).
    pub images: Vec<InputImage>,
    pub aspect_ratio: AspectRatio,
    /// Seconds, inside [`DURATION_SECS`]; `None` ⇒ the model's default (10s).
    pub duration_secs: Option<u32>,
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
    /// Generate one video from a prompt (+ optional input images). Blocks for
    /// the full create→poll→read round-trip.
    async fn generate(&self, req: &VideoRequest) -> Result<GeneratedVideo>;
}

/// The spec a withdrawn Veo model is pointed at.
const REPLACEMENT: &str = "gemini:standard";

/// Whether `model` is a Veo id, in any case and with or without `models/`.
fn is_veo(model: &str) -> bool {
    let id = model.trim();
    let id = id.strip_prefix("models/").unwrap_or(id);
    id.get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("veo-"))
}

/// What an `[llm.video]` block resolves to before any key is involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRoute {
    pub model: String,
    /// The clips' shape when a call names none.
    pub aspect_ratio: AspectRatio,
    pub resolution: Option<Resolution>,
}

/// Resolve and check an `[llm.video]` block without building a provider: the
/// model spec against the video tier registry, and the aspect ratio and
/// resolution the model accepts. Shared by the tool install and config
/// admission, so a block that won't install is reported when it is saved.
pub fn route(cfg: &LlmVideoGenerationConfig) -> Result<VideoRoute> {
    if !cfg.enabled {
        bail!("[llm.video] is present but disabled (enabled = false)");
    }
    if cfg.model.trim().is_empty() {
        bail!("[llm.video].model is empty — set a spec like 'gemini:standard'");
    }
    let registry = VideoTierRegistry::from_defaults()?;
    let model = registry
        .resolve_video_model(&cfg.model, Some(cfg.provider.as_deref().unwrap_or(GEMINI)))
        .map_err(|e| eyre::eyre!("[llm.video] model {e}"))?;
    if is_veo(&model) {
        let replacement = registry.resolve_video_model(REPLACEMENT, None)?;
        bail!(
            "[llm.video] model {:?}: Google withdrew Veo from the Gemini API (October 2026); \
             use {REPLACEMENT:?} ({replacement})",
            cfg.model
        );
    }
    let aspect_ratio = cfg
        .aspect_ratio
        .parse()
        .map_err(|e| eyre::eyre!("[llm.video] {e}"))?;
    let resolution = cfg
        .resolution
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|e: String| eyre::eyre!("[llm.video] {e}"))?;
    Ok(VideoRoute {
        model,
        aspect_ratio,
        resolution,
    })
}

/// The provider for a resolved `route`; `key` is where its Gemini key is read
/// on every generation.
pub fn provider_for(route: &VideoRoute, key: KeySlot) -> Box<dyn VideoProvider> {
    Box::new(omni::OmniVideoProvider::new(
        key,
        route.model.clone(),
        route.resolution,
    ))
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

    fn refusal(config: &LlmVideoGenerationConfig) -> String {
        route(config).unwrap_err().to_string()
    }

    #[test]
    fn a_tier_spec_routes_to_omni_with_the_configured_shape() {
        let routed = route(&LlmVideoGenerationConfig {
            aspect_ratio: "9:16".into(),
            resolution: Some("4K".into()),
            ..cfg("gemini:standard")
        })
        .unwrap();
        assert_eq!(
            routed,
            VideoRoute {
                model: "gemini-omni-1.1-flash".into(),
                aspect_ratio: AspectRatio::Portrait,
                resolution: Some(Resolution::K4),
            }
        );
        assert_eq!(route(&cfg("veo:standard")).unwrap().model, routed.model);
        assert_eq!(route(&cfg("standard")).unwrap().resolution, None);
    }

    #[test]
    fn a_veo_model_id_is_refused_naming_the_replacement() {
        for spec in [
            "veo-3.1-generate-preview",
            "VEO-3.1-fast-generate-preview",
            "models/veo-3.1-lite-generate-preview",
            "gemini:veo-3.1-generate-preview",
        ] {
            let error = refusal(&cfg(spec));
            assert!(
                error.contains("withdrew Veo")
                    && error.contains("\"gemini:standard\" (gemini-omni-1.1-flash)"),
                "{spec}: {error}"
            );
        }
    }

    #[test]
    fn a_concrete_gemini_model_id_passes_through() {
        assert_eq!(
            route(&cfg("gemini-omni-2-flash")).unwrap().model,
            "gemini-omni-2-flash"
        );
    }

    #[test]
    fn another_providers_spec_is_refused_by_name() {
        for (spec, provider, names) in [
            ("nonesuch:standard", None, "names no known provider"),
            (
                "standard",
                Some("anthropic"),
                "'anthropic' generates no video",
            ),
        ] {
            let error = refusal(&LlmVideoGenerationConfig {
                provider: provider.map(str::to_string),
                ..cfg(spec)
            });
            assert!(
                error.starts_with("[llm.video] model") && error.contains(names),
                "{spec}: {error}"
            );
        }
    }

    #[test]
    fn a_shape_the_model_does_not_render_is_refused_listing_the_choices() {
        let square = refusal(&LlmVideoGenerationConfig {
            aspect_ratio: "1:1".into(),
            ..cfg("gemini:standard")
        });
        assert!(square.contains("16:9, 9:16"), "{square}");
        let sd = refusal(&LlmVideoGenerationConfig {
            resolution: Some("480p".into()),
            ..cfg("gemini:standard")
        });
        assert!(sd.contains("360p, 720p, 1080p, 4k"), "{sd}");
    }

    #[test]
    fn disabled_or_modelless_blocks_are_refused() {
        let mut disabled = cfg("gemini:standard");
        disabled.enabled = false;
        assert!(route(&disabled).is_err());
        assert!(route(&cfg("")).is_err());
    }

    #[test]
    fn durations_are_brought_inside_what_the_model_accepts() {
        assert_eq!(clamp_duration(1), 3);
        assert_eq!(clamp_duration(6), 6);
        assert_eq!(clamp_duration(30), 10);
    }
}

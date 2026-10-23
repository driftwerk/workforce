//! Text-to-image generation providers.
//!
//! A deliberately narrow modality abstraction, sibling to the vision and
//! transcription paths. An [`ImageProvider`] turns a text prompt into raw image
//! **bytes** and nothing more — persistence, URL signing, transcript storage,
//! and per-surface delivery (Slack file upload, gateway blob attachment) are a
//! separate "image sink" layer ([`crate::llm::image`] does not touch them).
//!
//! Why its own trait rather than reusing [`super::provider::LlmProvider`]: the
//! signatures diverge (no streaming, no tools, no system prompt) and the return
//! type is binary, not a chat message. Keeping it separate means adding an
//! image backend never perturbs chat or vision routing.
//!
//! Convenient convergence: every backend we target — Bedrock `InvokeModel`
//! (Nova Canvas / Stability), OpenRouter image-capable chat, and the Gemini /
//! Imagen REST API — returns base64-encoded images, so one uniform
//! [`GeneratedImage`] shape covers them all without contortion.

mod bedrock;
mod gemini;
mod openrouter;
mod vllm;

use async_trait::async_trait;
use eyre::{bail, Result};
use workforce_common::settings::LlmImageGenerationConfig;

use workforce_common::secret::KeySlot;

use super::image_tiers::ImageTierRegistry;
use super::provider::ProviderKind;

/// One input/reference image for an edit or composition request. Carries raw
/// bytes + media type; the provider encodes them as inline image parts.
#[derive(Debug, Clone)]
pub struct InputImage {
    /// Raw image bytes (already decoded; not base64).
    pub data: Vec<u8>,
    /// MIME type, e.g. `"image/png"` or `"image/jpeg"`.
    pub media_type: String,
}

/// Sniff an image media type from magic bytes, defaulting to PNG. Used when a
/// reference-image input omits its media type — artifacts are stored as opaque
/// bytes, so the original mime isn't recoverable from the store.
pub fn sniff_media_type(bytes: &[u8]) -> &'static str {
    detect_image_media_type(bytes).unwrap_or("image/png")
}

/// The image media type `bytes` carry, judged by magic bytes; `None` when they
/// are not a PNG, JPEG, GIF or WebP.
pub fn detect_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// A request to generate one or more images from a text prompt, optionally
/// editing/composing over one or more reference images.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    /// The positive prompt describing the desired image.
    pub prompt: String,
    /// Optional negative prompt (subjects/qualities to avoid). Honoured by
    /// Stability and FLUX; silently ignored by providers that lack it
    /// (Gemini / Imagen).
    pub negative_prompt: Option<String>,
    /// Requested output dimensions. Providers clamp to the nearest supported
    /// size / aspect ratio.
    pub size: ImageSize,
    /// Number of images to generate. Clamped to the provider's per-call max.
    pub n: u8,
    /// Optional seed for reproducibility, where the provider supports it.
    pub seed: Option<u64>,
    /// Reference images for edit/compose mode. Empty ⇒ plain text→image. When
    /// non-empty the request is an edit ("place this person in that scene"):
    /// only the Gemini `:generateContent` and OpenRouter google image models
    /// accept image input — other providers reject a request carrying these.
    pub reference_images: Vec<InputImage>,
}

/// Output dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

impl ImageSize {
    /// Parse a `WxH` spec like `"1024x1024"`. Falls back to 1024x1024 on any
    /// malformed input so a typo never hard-fails generation.
    pub fn parse(spec: &str) -> Self {
        let parse = || {
            let (w, h) = spec.split_once(['x', 'X'])?;
            Some(Self {
                width: w.trim().parse().ok()?,
                height: h.trim().parse().ok()?,
            })
        };
        parse().unwrap_or(Self {
            width: 1024,
            height: 1024,
        })
    }

    /// Nearest standard aspect-ratio string (`"1:1"`, `"16:9"`, …) for
    /// providers like Stability that take a ratio rather than dimensions.
    pub fn nearest_aspect_ratio(self) -> &'static str {
        const RATIOS: &[(&str, f32)] = &[
            ("1:1", 1.0),
            ("16:9", 16.0 / 9.0),
            ("9:16", 9.0 / 16.0),
            ("4:3", 4.0 / 3.0),
            ("3:4", 3.0 / 4.0),
            ("3:2", 3.0 / 2.0),
            ("2:3", 2.0 / 3.0),
        ];
        let target = self.width as f32 / self.height.max(1) as f32;
        RATIOS
            .iter()
            .min_by(|a, b| {
                (a.1 - target)
                    .abs()
                    .partial_cmp(&(b.1 - target).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(label, _)| *label)
            .unwrap_or("1:1")
    }
}

/// One generated image. Carries decoded bytes + media type; nothing about how
/// or where it will be delivered.
#[derive(Clone)]
pub struct GeneratedImage {
    /// Raw decoded image bytes (already base64-decoded).
    pub data: Vec<u8>,
    /// MIME type, e.g. `"image/png"` or `"image/jpeg"`.
    pub media_type: String,
    /// Some providers rewrite/expand the prompt and report what they actually
    /// rendered; surfaced for transparency when present.
    pub revised_prompt: Option<String>,
}

impl std::fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedImage")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .field("revised_prompt", &self.revised_prompt)
            .finish()
    }
}

/// Provider-agnostic image generation interface.
#[async_trait]
pub trait ImageProvider: Send + Sync {
    /// Generate one or more images from a text prompt.
    ///
    /// Synchronous from the caller's perspective — there is no streaming in
    /// v1. Generation typically runs 5–30s, so callers should await it off the
    /// latency-critical path and surface progress via the channel surface.
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>>;

    /// Confirm the provider's credentials authenticate and the configured
    /// model is reachable. The natural home for Bedrock's two operational
    /// gotchas — per-model access opt-in and regional availability — which
    /// surface here as actionable startup errors rather than first-request
    /// 4xx in front of a user.
    async fn verify_credentials(&self) -> Result<()>;
}

/// Build an [`ImageProvider`] from resolved configuration.
///
/// `kind` is the routing provider derived from the `[llm.image].model` spec,
/// `api_key` where its key is read on every request (for Bedrock a packed AWS
/// string or the chain sentinel, read once here), `model` the concrete image
/// model id, `url` an
/// optional base-URL override for self-hosted deployments, and `edit_model` the
/// concrete model to use when a request carries reference images (the powerful
/// image tier; honoured by Gemini/OpenRouter, ignored by Bedrock/vllm which
/// reject edits).
pub fn create_image_provider(
    kind: ProviderKind,
    api_key: KeySlot,
    model: &str,
    url: Option<&str>,
    region: Option<&str>,
    edit_model: Option<&str>,
) -> Result<Box<dyn ImageProvider>> {
    match kind {
        ProviderKind::Gemini => Ok(Box::new(gemini::GeminiImageProvider::new(
            api_key, model, edit_model,
        ))),
        ProviderKind::OpenRouter => Ok(Box::new(openrouter::OpenRouterImageProvider::new(
            api_key, model, edit_model,
        ))),
        ProviderKind::Bedrock => Ok(Box::new(bedrock::BedrockImageProvider::new(
            &api_key.peek().unwrap_or_default(),
            model,
            region,
        ))),
        ProviderKind::Vllm => {
            let base = url
                .map(str::to_string)
                .or_else(|| std::env::var("VLLM_IMAGE_URL").ok())
                .or_else(|| std::env::var("VLLM_URL").ok())
                .ok_or_else(|| {
                    eyre::eyre!(
                        "self-hosted image generation needs [llm.image].url (or VLLM_IMAGE_URL) \
                         pointing at an OpenAI-images-compatible endpoint"
                    )
                })?;
            Ok(Box::new(vllm::VllmImageProvider::new(base, api_key, model)))
        }
        other => bail!(
            "provider '{}' does not support image generation; configure [llm.image].model with \
             a gemini / openrouter:google / bedrock:(stability|nova) / vllm spec",
            other.canonical_name()
        ),
    }
}

/// The provider `[llm.image]` routes to, the concrete model, and the model
/// reference-image edits run on.
fn route(cfg: &LlmImageGenerationConfig) -> Result<(ProviderKind, String, Option<String>)> {
    if !cfg.enabled {
        bail!("[llm.image] is present but disabled (enabled = false)");
    }
    if cfg.model.trim().is_empty() {
        bail!("[llm.image].model is empty — set a spec like 'gemini:standard' or 'bedrock:stability:standard'");
    }

    let registry = ImageTierRegistry::from_defaults()?;
    let resolve = |spec: &str| registry.resolve_routed_image_model(spec, cfg.provider.as_deref());
    let (provider_name, model) =
        resolve(&cfg.model).map_err(|e| eyre::eyre!("[llm.image] model {e}"))?;

    // Resolve the same provider family's *powerful* tier as the edit model.
    // Reference-image edits route here because the cheap/standard image tiers
    // (e.g. gemini-2.5-flash-image) refuse to composite a recognizable face
    // (finishReason=IMAGE_OTHER), while the powerful tier (gemini-3-pro-image)
    // performs it. `None` when the spec is a bare concrete model with no tier
    // to lift — then edits fall back to the configured model.
    let edit_model = powerful_edit_spec(&cfg.model)
        .and_then(|spec| resolve(&spec).ok())
        .map(|(_, model)| model);

    let kind = ProviderKind::from_str_loose(&provider_name)
        .ok_or_else(|| eyre::eyre!("unknown image provider '{provider_name}'"))?;
    Ok((kind, model, edit_model))
}

/// The provider `[llm.image]` routes image generation to.
pub fn routed_provider(cfg: &LlmImageGenerationConfig) -> Result<ProviderKind> {
    route(cfg).map(|(kind, ..)| kind)
}

/// Build an [`ImageProvider`] from a `[llm.image]` config block.
///
/// Resolves the `model` spec against the image tier registry to a
/// `(provider, concrete_model)` pair; `key` is where that provider's key is
/// read. Returns an error if the resolved provider can't generate images.
/// `enabled = false` is treated as "no provider".
pub fn provider_from_config(
    cfg: &LlmImageGenerationConfig,
    key: impl Fn(ProviderKind) -> KeySlot,
) -> Result<Box<dyn ImageProvider>> {
    let (kind, model, edit_model) = route(cfg)?;
    create_image_provider(
        kind,
        key(kind),
        &model,
        cfg.url.as_deref(),
        cfg.region.as_deref(),
        edit_model.as_deref(),
    )
}

/// Derive the "powerful tier" edit spec from a configured `[llm.image].model`
/// spec by lifting its tier segment to `powerful`:
///   - `"gemini:standard"` → `Some("gemini:powerful")`
///   - `"openrouter:google:cheap"` → `Some("openrouter:google:powerful")`
///   - bare tier `"standard"` → `Some("powerful")`
///   - a concrete model id with no tier → `None` (nothing to lift)
fn powerful_edit_spec(model_spec: &str) -> Option<String> {
    use super::tiers::ModelTier;
    let spec = model_spec.trim();
    if ModelTier::from_name(spec).is_some() {
        return Some("powerful".to_string());
    }
    if let Some((prefix, last)) = spec.rsplit_once(':') {
        if ModelTier::from_name(last).is_some() {
            return Some(format!("{prefix}:powerful"));
        }
    }
    None
}

/// The HTTP client image providers share — a plain reqwest client with a
/// generation-appropriate timeout (text-to-image runs ~5–30s). It sends no
/// `Referer` on a redirect: the Gemini endpoints carry the key in the URL.
pub(crate) fn http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .referer(false)
        .build()
        .unwrap_or_default()
}

/// Decode a `data:<media_type>;base64,<payload>` URL into bytes + media type.
/// Tolerates a bare base64 payload (no `data:` prefix) by assuming PNG.
pub(crate) fn decode_data_url(url: &str) -> Result<(Vec<u8>, String)> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;

    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, payload) = rest
            .split_once(',')
            .ok_or_else(|| eyre::eyre!("malformed data URL: missing comma"))?;
        let media_type = meta
            .split(';')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("image/png")
            .to_string();
        let data = BASE64
            .decode(payload.trim())
            .map_err(|e| eyre::eyre!("data URL base64 decode failed: {e}"))?;
        Ok((data, media_type))
    } else {
        let data = BASE64
            .decode(url.trim())
            .map_err(|e| eyre::eyre!("base64 decode failed: {e}"))?;
        Ok((data, "image/png".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_size_parses_and_falls_back() {
        assert_eq!(ImageSize::parse("1024x1024").width, 1024);
        assert_eq!(ImageSize::parse("1280x720").height, 720);
        assert_eq!(ImageSize::parse("768X1024").width, 768);
        // Malformed → safe default, never panics.
        assert_eq!(
            ImageSize::parse("garbage"),
            ImageSize {
                width: 1024,
                height: 1024
            }
        );
    }

    #[test]
    fn aspect_ratio_nearest() {
        assert_eq!(
            ImageSize {
                width: 1024,
                height: 1024
            }
            .nearest_aspect_ratio(),
            "1:1"
        );
        assert_eq!(
            ImageSize {
                width: 1280,
                height: 720
            }
            .nearest_aspect_ratio(),
            "16:9"
        );
        assert_eq!(
            ImageSize {
                width: 720,
                height: 1280
            }
            .nearest_aspect_ratio(),
            "9:16"
        );
    }

    #[test]
    fn decode_data_url_extracts_media_type() {
        // "hi" base64 == "aGk="
        let (bytes, mt) = decode_data_url("data:image/jpeg;base64,aGk=").unwrap();
        assert_eq!(bytes, b"hi");
        assert_eq!(mt, "image/jpeg");
    }

    #[test]
    fn decode_bare_base64_defaults_png() {
        let (bytes, mt) = decode_data_url("aGk=").unwrap();
        assert_eq!(bytes, b"hi");
        assert_eq!(mt, "image/png");
    }

    fn no_key(_: ProviderKind) -> KeySlot {
        KeySlot::fixed("")
    }

    fn image_cfg(model: &str) -> LlmImageGenerationConfig {
        LlmImageGenerationConfig {
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_spec_builds_on_the_provider_it_names_or_is_refused_by_name() {
        assert!(provider_from_config(&image_cfg("gemini:standard"), no_key).is_ok());
        let bare = LlmImageGenerationConfig {
            provider: Some("gemini".into()),
            ..image_cfg("standard")
        };
        assert!(provider_from_config(&bare, no_key).is_ok());

        for (spec, names) in [
            ("standard", "'standard' names no known provider"),
            (
                "gemini-2.5-flash-image",
                "'gemini-2.5-flash-image' names no known provider",
            ),
            (
                "nonesuch:standard",
                "'nonesuch:standard' names no known provider",
            ),
            ("anthropic:standard", "the standard tier of 'anthropic'"),
            (
                "anthropic:claude-haiku-4-5",
                "does not support image generation",
            ),
        ] {
            let error = match provider_from_config(&image_cfg(spec), no_key) {
                Ok(_) => panic!("{spec} names nothing that generates images"),
                Err(e) => e.to_string(),
            };
            assert!(error.contains(names), "{spec}: {error}");
        }
    }

    #[test]
    fn factory_rejects_text_only_provider() {
        let err = match create_image_provider(
            ProviderKind::Cerebras,
            KeySlot::fixed("k"),
            "m",
            None,
            None,
            None,
        ) {
            Ok(_) => panic!("expected text-only provider to be rejected"),
            Err(e) => e,
        };
        assert!(err
            .to_string()
            .contains("does not support image generation"));
    }

    #[test]
    fn edit_model_lifts_configured_tier_to_powerful() {
        // The load-bearing face-edit routing: reference edits must resolve to
        // the gemini-3-pro tier regardless of the configured base tier.
        let reg = ImageTierRegistry::from_defaults().unwrap();
        let edit_model = |spec: &str, provider: Option<&str>| {
            powerful_edit_spec(spec)
                .map(|s| reg.resolve_routed_image_model(&s, provider).unwrap().1)
        };
        assert_eq!(
            edit_model("gemini:standard", None).as_deref(),
            Some("gemini-3-pro-image")
        );
        assert_eq!(
            edit_model("openrouter:google:cheap", None).as_deref(),
            Some("google/gemini-3-pro-image")
        );
        assert_eq!(
            edit_model("standard", Some("gemini")).as_deref(),
            Some("gemini-3-pro-image"),
            "a bare tier lifts against the configured provider"
        );
        // A concrete model id carries no tier to lift.
        assert_eq!(powerful_edit_spec("gemini-2.5-flash-image"), None);
        assert_eq!(
            powerful_edit_spec("stability.stable-image-ultra-v1:1"),
            None
        );
    }
}

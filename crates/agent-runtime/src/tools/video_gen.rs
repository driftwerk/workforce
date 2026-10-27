//! `generate_video` — text/image-to-video generation as an agent tool.
//!
//! Sibling to [`super::image_gen`]. Calls the configured [`VideoProvider`]
//! (Gemini Omni), which creates + polls + reads the clip (15–60s, blocking),
//! stages the MP4 bytes in the artifact store, and returns the **same**
//! `generated_images` marker the image path uses — so the existing image sink
//! delivers the video to Slack (`upload_file` takes any mime) with no new
//! delivery code. Gateway/iOS keeps it staged (image-only render path), as with
//! other non-image attachments.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmVideoGenerationConfig;

use super::image_gen::{resolve_reference_images, GENERATED_IMAGES_KEY};
use super::registry::ToolRegistry;
use super::{Tool, ToolResult};
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::llm::video::{
    clamp_duration, provider_for, route, AspectRatio, VideoProvider, VideoRequest, DURATION_SECS,
};

/// Registry name of the video-generation tool.
pub const VIDEO_TOOL_NAME: &str = "generate_video";

/// The call's input images as an array: `images`, or the single `image`
/// object calls made before `images` existed still send.
fn input_images(input: &Value) -> Result<Value, String> {
    match (&input["images"], &input["image"]) {
        (Value::Array(_), _) => Ok(input["images"].clone()),
        (Value::Null, Value::Object(image)) if !image.is_empty() => Ok(json!([image])),
        (Value::Null, _) => Ok(json!([])),
        (other, _) => Err(format!(
            "'images' must be an array of image objects, not {other}"
        )),
    }
}

pub struct VideoGenerationTool {
    provider: Arc<dyn VideoProvider>,
    agent_id: String,
    aspect_ratio: AspectRatio,
    duration_secs: Option<u32>,
}

impl VideoGenerationTool {
    pub fn new(
        provider: Arc<dyn VideoProvider>,
        agent_id: impl Into<String>,
        aspect_ratio: AspectRatio,
        duration_secs: Option<u32>,
    ) -> Self {
        Self {
            provider,
            agent_id: agent_id.into(),
            aspect_ratio,
            duration_secs,
        }
    }
}

#[async_trait]
impl Tool for VideoGenerationTool {
    fn name(&self) -> &str {
        VIDEO_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Generate a short video clip (with sound) from a text `prompt`, optionally from `images`: \
         one image is animated; two become the first and last frame; more are subject \
         references, named in the prompt as <IMAGE_REF_0>, <IMAGE_REF_1>, … This takes a while \
         (~15–60s) and BLOCKS until done, then the system delivers the clip to the user \
         automatically. IMPORTANT: you will NOT see the video and the artifact is internal — that \
         is expected, not a failure. Do not re-upload, attach, verify, or claim it failed. Briefly \
         tell the user what you made (and that it's on its way)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What should happen in the clip (subject + motion + setting + sound)."
                },
                "images": {
                    "type": "array",
                    "description": "Optional input images, in order. Each { \"artifact_id\": \"art_…\", \"media_type\": \"image/png\" } from view_attachment / get_user_photo / a prior generate_image, or { \"data_base64\": \"…\" }.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "artifact_id": { "type": "string" },
                            "data_base64": { "type": "string" },
                            "media_type": { "type": "string" }
                        }
                    }
                },
                "aspect_ratio": {
                    "type": "string",
                    "enum": AspectRatio::ALL.map(AspectRatio::as_str),
                    "description": "Optional. Defaults to the configured value."
                },
                "duration_secs": {
                    "type": "integer",
                    "minimum": DURATION_SECS.start(),
                    "maximum": DURATION_SECS.end(),
                    "description": "Optional clip length in seconds, 3–10. Defaults to the configured length."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let Some(prompt) = input["prompt"].as_str().filter(|s| !s.trim().is_empty()) else {
            return Ok(ToolResult::failure(
                "generate_video requires a non-empty 'prompt'",
                elapsed(started),
            ));
        };

        let images = match input_images(&input)
            .and_then(|images| resolve_reference_images(&images, &self.agent_id))
        {
            Ok(images) => images,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed(started))),
        };

        let aspect_ratio = match input["aspect_ratio"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::parse)
            .transpose()
        {
            Ok(requested) => requested.unwrap_or(self.aspect_ratio),
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed(started))),
        };
        let duration_secs = input["duration_secs"]
            .as_u64()
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
            .or(self.duration_secs)
            .map(clamp_duration);

        let req = VideoRequest {
            prompt: prompt.to_string(),
            images,
            aspect_ratio,
            duration_secs,
        };

        let video = match self.provider.generate(&req).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("video generation failed: {e}"),
                    elapsed(started),
                ));
            }
        };

        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage generated video",
                elapsed(started),
            ));
        };
        let descriptor = ArtifactDescriptor::from_context(
            VIDEO_TOOL_NAME,
            ContentType::OctetStream,
            self.agent_id.clone(),
        )
        .with_file_meta("video.mp4", video.media_type.clone());
        let bytes_len = video.data.len();
        let handle = match store.write(&self.agent_id, descriptor, ArtifactBody::Bytes(video.data))
        {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage generated video: {e}"),
                    elapsed(started),
                ));
            }
        };

        // Reuse the image delivery marker: the sink uploads any mime to Slack.
        let mut output = serde_json::Map::new();
        output.insert(
            GENERATED_IMAGES_KEY.to_string(),
            json!([{
                "artifact_id": handle.id.as_str(),
                "media_type": video.media_type,
                "filename": "video.mp4",
                "bytes": bytes_len,
            }]),
        );
        output.insert("prompt".to_string(), json!(prompt));
        output.insert("aspect_ratio".to_string(), json!(aspect_ratio.as_str()));
        if let Some(secs) = duration_secs {
            output.insert("duration_secs".to_string(), json!(secs));
        }
        output.insert(
            "note".to_string(),
            json!("Generated. The system is delivering the video to this conversation automatically — it happens outside your context, so you will NOT see the attachment. This is expected, NOT a failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the user what you made."),
        );
        Ok(ToolResult::success(Value::Object(output), elapsed(started)))
    }
}

/// Outcome of applying a (possibly changed) `[llm.video]` config — mirrors
/// [`super::image_gen::ImageReload`].
pub enum VideoReload {
    Unchanged,
    Installed { model: String },
    Disabled,
    BuildFailed(eyre::Report),
}

impl VideoReload {
    pub fn recorded(
        &self,
        applied: Option<LlmVideoGenerationConfig>,
    ) -> Option<LlmVideoGenerationConfig> {
        match self {
            VideoReload::BuildFailed(_) => None,
            _ => applied,
        }
    }

    pub fn log(&self, agent_id: &str) {
        match self {
            VideoReload::Unchanged => {}
            VideoReload::Installed { model } => {
                tracing::info!(agent = %agent_id, %model, "Video generation tool (re)installed")
            }
            VideoReload::Disabled => {
                tracing::info!(agent = %agent_id, "Video generation disabled; tool removed")
            }
            VideoReload::BuildFailed(e) => {
                tracing::warn!(agent = %agent_id, error = %e, "Video config changed but provider build failed; tool removed")
            }
        }
    }
}

/// Reconcile the registered `generate_video` tool with `next` config, given the
/// `current` config it was last built from. Shared by startup (`current = None`)
/// and hot-reload.
pub fn install_video_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    current: Option<&LlmVideoGenerationConfig>,
    next: Option<&LlmVideoGenerationConfig>,
    key: KeySlot,
) -> VideoReload {
    if current == next {
        return VideoReload::Unchanged;
    }
    reinstall_video_tool(tools, agent_id, next, key)
}

/// Rebuild the `generate_video` tool from `cfg` unconditionally; the provider
/// reads its key from `key` on every generation.
pub fn reinstall_video_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    cfg: Option<&LlmVideoGenerationConfig>,
    key: KeySlot,
) -> VideoReload {
    match cfg.filter(|c| c.enabled) {
        Some(cfg) => match route(cfg) {
            Ok(route) => {
                tools.register(Arc::new(VideoGenerationTool::new(
                    Arc::from(provider_for(&route, key)),
                    agent_id,
                    route.aspect_ratio,
                    cfg.duration_secs,
                )));
                VideoReload::Installed {
                    model: cfg.model.clone(),
                }
            }
            Err(e) => {
                tools.unregister(VIDEO_TOOL_NAME);
                VideoReload::BuildFailed(e)
            }
        },
        None => {
            tools.unregister(VIDEO_TOOL_NAME);
            VideoReload::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_cfg(model: &str) -> LlmVideoGenerationConfig {
        LlmVideoGenerationConfig {
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn install_registers_enabled_provider() {
        let tools = ToolRegistry::new();
        let outcome = install_video_tool(
            &tools,
            "agent",
            None,
            Some(&video_cfg("gemini:standard")),
            KeySlot::fixed("k"),
        );
        assert!(matches!(outcome, VideoReload::Installed { .. }));
        assert!(tools.has(VIDEO_TOOL_NAME));
    }

    #[test]
    fn install_removes_tool_when_disabled() {
        let tools = ToolRegistry::new();
        let enabled = video_cfg("gemini:standard");
        install_video_tool(&tools, "agent", None, Some(&enabled), KeySlot::fixed("k"));
        let disabled = LlmVideoGenerationConfig {
            enabled: false,
            ..enabled.clone()
        };
        let outcome = install_video_tool(
            &tools,
            "agent",
            Some(&enabled),
            Some(&disabled),
            KeySlot::fixed("k"),
        );
        assert!(matches!(outcome, VideoReload::Disabled));
        assert!(!tools.has(VIDEO_TOOL_NAME));
    }

    #[test]
    fn install_is_noop_when_unchanged() {
        let tools = ToolRegistry::new();
        let cfg = video_cfg("gemini:standard");
        install_video_tool(&tools, "agent", None, Some(&cfg), KeySlot::fixed("k"));
        let outcome =
            install_video_tool(&tools, "agent", Some(&cfg), Some(&cfg), KeySlot::fixed("k"));
        assert!(matches!(outcome, VideoReload::Unchanged));
        assert!(tools.has(VIDEO_TOOL_NAME));
    }

    /// A provider that records the request it is asked for and declines it,
    /// so a test sees what the tool sends without staging anything.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Option<VideoRequest>>);

    #[async_trait]
    impl VideoProvider for Recording {
        async fn generate(
            &self,
            req: &VideoRequest,
        ) -> eyre::Result<crate::llm::video::GeneratedVideo> {
            *self.0.lock().unwrap() = Some(req.clone());
            eyre::bail!("recorded")
        }
    }

    fn tool(provider: Arc<Recording>) -> VideoGenerationTool {
        VideoGenerationTool::new(provider, "agent", AspectRatio::Landscape, Some(30))
    }

    #[tokio::test]
    async fn images_reach_the_provider_in_order_with_the_clip_bounded() {
        use base64::Engine;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let provider = Arc::new(Recording::default());
        tool(Arc::clone(&provider))
            .execute(json!({
                "prompt": "from dawn to dusk",
                "images": [
                    { "data_base64": b64(b"first"), "media_type": "image/png" },
                    { "data_base64": b64(b"last"), "media_type": "image/jpeg" },
                ],
                "aspect_ratio": "9:16",
            }))
            .await
            .unwrap();
        let sent = provider
            .0
            .lock()
            .unwrap()
            .take()
            .expect("provider was called");
        let images: Vec<(&[u8], &str)> = sent
            .images
            .iter()
            .map(|i| (i.data.as_slice(), i.media_type.as_str()))
            .collect();
        assert_eq!(
            images,
            [(&b"first"[..], "image/png"), (&b"last"[..], "image/jpeg")]
        );
        assert_eq!(sent.aspect_ratio, AspectRatio::Portrait);
        assert_eq!(sent.duration_secs, Some(10));
    }

    #[tokio::test]
    async fn a_lone_image_is_still_animated_and_a_malformed_list_refused() {
        use base64::Engine;
        let photo = base64::engine::general_purpose::STANDARD.encode(b"photo");
        let provider = Arc::new(Recording::default());
        tool(Arc::clone(&provider))
            .execute(json!({ "prompt": "wave", "image": { "data_base64": photo } }))
            .await
            .unwrap();
        let sent = provider
            .0
            .lock()
            .unwrap()
            .take()
            .expect("provider was called");
        assert_eq!(sent.images.len(), 1);
        assert_eq!(sent.images[0].data, b"photo");

        let result = tool(Arc::clone(&provider))
            .execute(json!({ "prompt": "wave", "images": { "data_base64": photo } }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(provider.0.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_shape_the_model_does_not_render_fails_before_the_provider() {
        let provider = Arc::new(Recording::default());
        let result = tool(Arc::clone(&provider))
            .execute(json!({ "prompt": "a cube", "aspect_ratio": "1:1" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(provider.0.lock().unwrap().is_none());
    }

    #[test]
    fn install_removes_tool_on_build_failure() {
        let tools = ToolRegistry::new();
        let outcome = install_video_tool(
            &tools,
            "agent",
            None,
            Some(&video_cfg("")),
            KeySlot::fixed("k"),
        );
        assert!(matches!(outcome, VideoReload::BuildFailed(_)));
        assert!(!tools.has(VIDEO_TOOL_NAME));
    }
}

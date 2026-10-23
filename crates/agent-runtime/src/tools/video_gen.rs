//! `generate_video` — text/image-to-video generation as an agent tool.
//!
//! Sibling to [`super::image_gen`]. Calls the configured [`VideoProvider`]
//! (Google Veo), which submits + polls + downloads the clip (1–6 min, blocking),
//! stages the MP4 bytes in the artifact store, and returns the **same**
//! `generated_images` marker the image path uses — so the existing image sink
//! delivers the video to Slack (`upload_file` takes any mime) with no new
//! delivery code. Gateway/iOS keeps it staged (image-only render path), as with
//! other non-image attachments.
//!
//! NOTE on latency: this blocks the tool call for the whole round-trip. The
//! fast Veo tier returns in ~40s; slower tiers take minutes. A future revision
//! can run it as a background job,
//! which is intentionally avoided here to stay off the in-flight job_runner.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmVideoGenerationConfig;

use super::image_gen::{resolve_image_input, GENERATED_IMAGES_KEY};
use super::registry::ToolRegistry;
use super::{Tool, ToolResult};
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::llm::image::InputImage;
use crate::llm::video::{provider_from_config, VideoProvider, VideoRequest};

/// Registry name of the video-generation tool.
pub const VIDEO_TOOL_NAME: &str = "generate_video";

/// Resolve the optional starting/subject `image`. Absent or an empty object ⇒
/// `None` (text-to-video); otherwise the shared image-input resolver.
fn resolve_input_image(entry: &Value, agent_id: &str) -> Result<Option<InputImage>, String> {
    if entry.get("artifact_id").is_none() && entry.get("data_base64").is_none() {
        return Ok(None);
    }
    resolve_image_input(entry, agent_id, "image").map(Some)
}

pub struct VideoGenerationTool {
    provider: Arc<dyn VideoProvider>,
    agent_id: String,
    aspect_ratio: String,
    duration_secs: Option<u32>,
    person_generation: String,
}

impl VideoGenerationTool {
    pub fn new(
        provider: Arc<dyn VideoProvider>,
        agent_id: impl Into<String>,
        aspect_ratio: impl Into<String>,
        duration_secs: Option<u32>,
        person_generation: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            agent_id: agent_id.into(),
            aspect_ratio: aspect_ratio.into(),
            duration_secs,
            person_generation: person_generation.into(),
        }
    }
}

#[async_trait]
impl Tool for VideoGenerationTool {
    fn name(&self) -> &str {
        VIDEO_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Generate a short video clip from a text `prompt`, optionally animating a starting `image` \
         (image-to-video — e.g. from get_user_photo to 'make a video of Tom dancing'). For an \
         expressive person clip prefer a fuller-body source over a tight headshot. This takes a \
         while (~1–2 min) and BLOCKS until done, then the system delivers the clip to the user \
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
                    "description": "What should happen in the clip (subject + motion + setting)."
                },
                "image": {
                    "type": "object",
                    "description": "Optional starting/subject image to animate. { \"artifact_id\": \"art_…\", \"media_type\": \"image/png\" } from get_user_photo / view_attachment / a prior generate_image.",
                    "properties": {
                        "artifact_id": { "type": "string" },
                        "media_type": { "type": "string" }
                    }
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Optional, e.g. '16:9' or '9:16'. Defaults to the configured value."
                },
                "duration_secs": {
                    "type": "integer",
                    "description": "Optional clip length in seconds (model-clamped)."
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

        let input_image = match resolve_input_image(&input["image"], &self.agent_id) {
            Ok(img) => img,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed(started))),
        };

        let aspect_ratio = input["aspect_ratio"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.aspect_ratio.clone());
        let duration_secs = input["duration_secs"]
            .as_u64()
            .map(|v| v as u32)
            .or(self.duration_secs);

        let req = VideoRequest {
            prompt: prompt.to_string(),
            input_image,
            aspect_ratio,
            duration_secs,
            person_generation: self.person_generation.clone(),
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

/// Rebuild the `generate_video` tool from `cfg` unconditionally; Veo reads its
/// key from `key` on every generation.
pub fn reinstall_video_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    cfg: Option<&LlmVideoGenerationConfig>,
    key: KeySlot,
) -> VideoReload {
    match cfg.filter(|c| c.enabled) {
        Some(cfg) => match provider_from_config(cfg, key) {
            Ok(provider) => {
                tools.register(Arc::new(VideoGenerationTool::new(
                    Arc::from(provider),
                    agent_id,
                    cfg.aspect_ratio.clone(),
                    cfg.duration_secs,
                    cfg.person_generation.clone(),
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
            Some(&video_cfg("veo:standard")),
            KeySlot::fixed("k"),
        );
        assert!(matches!(outcome, VideoReload::Installed { .. }));
        assert!(tools.has(VIDEO_TOOL_NAME));
    }

    #[test]
    fn install_removes_tool_when_disabled() {
        let tools = ToolRegistry::new();
        let enabled = video_cfg("veo:standard");
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
        let cfg = video_cfg("veo:standard");
        install_video_tool(&tools, "agent", None, Some(&cfg), KeySlot::fixed("k"));
        let outcome =
            install_video_tool(&tools, "agent", Some(&cfg), Some(&cfg), KeySlot::fixed("k"));
        assert!(matches!(outcome, VideoReload::Unchanged));
        assert!(tools.has(VIDEO_TOOL_NAME));
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

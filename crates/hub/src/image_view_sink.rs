//! Hub-side [`ImageViewSink`] — renders a file staged by `view_attachment` into
//! model-visible content blocks.
//!
//! The runner calls [`HubImageViewSink::render`] after a `view_attachment` tool
//! result. We resolve the staged bytes from the artifact store and route them
//! through the exact same per-type pipeline used for inbound attachments
//! ([`crate::serve::process_attachment`]): image → vision, PDF → text/document,
//! audio → transcript, video → frames + transcript, text/office → extracted text, other → workspace
//! path note. Each honours the active model's capability and the configured
//! secondary-LLM strategies; a video also shows its frames, or — opened for a
//! closer look — only the frames the view picks.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};

use workforce_agent::artifacts::store::global_store;
use workforce_agent::artifacts::types::ArtifactBody;
use workforce_agent::llm::types::ContentBlock;
use workforce_agent::llm::vision_tiers::VisionTierRegistry;
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::image_view::{ImageViewSink, ViewedImageRef};
use workforce_common::session::SessionAttachment;
use workforce_common::settings::VideoFramesConfig;

use workforce_agent::attachments::{
    process_viewed_attachment, resolve_vision_strategy_for, Describer, MediaStrategies,
    Transcriber, VisionStrategy,
};
use workforce_agent::llm::provider::ProviderKind;

pub struct HubImageViewSink {
    vision_tiers: Arc<VisionTierRegistry>,
    describer: Describer,
    transcriber: Transcriber,
    /// The provider a model named with no route of its own is called on.
    primary: ProviderKind,
    video_frames: Option<VideoFramesConfig>,
    workspace: PathBuf,
}

impl HubImageViewSink {
    pub fn new(
        vision_tiers: Arc<VisionTierRegistry>,
        describer: Describer,
        transcriber: Transcriber,
        primary: ProviderKind,
        video_frames: Option<VideoFramesConfig>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            vision_tiers,
            describer,
            transcriber,
            primary,
            video_frames,
            workspace,
        }
    }

    fn vision_strategy(&self, active_model: &str) -> VisionStrategy<'_> {
        resolve_vision_strategy_for(
            &self.vision_tiers,
            &self.describer,
            active_model,
            self.primary,
        )
    }
}

#[async_trait]
impl ImageViewSink for HubImageViewSink {
    async fn render(
        &self,
        ctx: &SessionContext,
        viewed: &ViewedImageRef,
        active_model: &str,
    ) -> Result<Vec<ContentBlock>> {
        let store = global_store().ok_or_else(|| eyre!("artifact store not configured"))?;
        let agent_id = ctx.agent_id.clone().unwrap_or_default();
        let bytes = match store.read(&agent_id, &viewed.artifact_id)? {
            ArtifactBody::Bytes(bytes) => bytes,
            other => {
                return Err(eyre!(
                    "artifact {} is {:?}, not raw bytes",
                    viewed.artifact_id.as_str(),
                    other.content_type()
                ))
            }
        };

        // `from_bytes` applies the encoding contract: text-like types arrive as
        // readable UTF-8, everything else as base64. Encoding here by hand is
        // what sent a `text/plain` YAML to the model as 100,000 chars of base64.
        let att = SessionAttachment::from_bytes(&viewed.filename, &viewed.media_type, &bytes);
        // A video is decoded again to a file; don't hold a third copy meanwhile.
        drop(bytes);
        let strategies = MediaStrategies {
            vision: self.vision_strategy(active_model),
            transcription: self.transcriber.strategy(),
            video_frames: self.video_frames.clone(),
        };
        // Runs inside a tool execution — ambient task-locals are in scope.
        let staging = workforce_agent::runner::SessionContext::capture();
        Ok(process_viewed_attachment(
            &att,
            viewed.frames.as_ref(),
            &strategies,
            &self.workspace,
            &staging,
        )
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::RwLock;
    use workforce_agent::artifacts::types::{ArtifactDescriptor, ArtifactId, ContentType};
    use workforce_agent::artifacts::{set_global_store, FsArtifactStore};
    use workforce_agent::attachments::FrameSelection;
    use workforce_agent::runner::{
        render_viewed, ToolInvocation, CURRENT_AGENT_ID, CURRENT_CHANNEL_ID,
    };
    use workforce_agent::tools::Tool;
    use workforce_common::channels::{
        ChannelCapabilities, ChannelFile, ChannelMessage, ChannelSurface, Reaction, SurfaceKey,
        SurfaceRegistry,
    };
    use workforce_skills::adapters::image_view::ViewAttachmentTool;

    fn sink(video_frames: Option<VideoFramesConfig>, workspace: PathBuf) -> HubImageViewSink {
        HubImageViewSink::new(
            Arc::new(VisionTierRegistry::from_defaults().expect("defaults")),
            Describer::Unconfigured,
            Transcriber::Unconfigured,
            ProviderKind::Anthropic,
            video_frames,
            workspace,
        )
    }

    /// The process-wide store, which the first caller installs for good:
    /// backed by a directory that outlives every test, with writes kept
    /// apart by agent id.
    fn store() -> Arc<dyn workforce_agent::artifacts::ArtifactStore> {
        static DIR: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
            let dir = tempfile::TempDir::new().expect("tempdir");
            set_global_store(FsArtifactStore::new(dir.path().to_path_buf()).expect("store"));
            dir
        });
        std::sync::LazyLock::force(&DIR);
        global_store().expect("installed")
    }

    /// `bytes` staged for `agent` as a tool would stage them.
    fn staged(agent: &str, bytes: &[u8]) -> ArtifactId {
        store()
            .write(
                agent,
                ArtifactDescriptor::from_context(
                    "view_attachment",
                    ContentType::OctetStream,
                    agent.to_string(),
                ),
                ArtifactBody::Bytes(bytes.to_vec()),
            )
            .expect("staged")
            .id
    }

    fn viewed(
        artifact_id: ArtifactId,
        media_type: &str,
        filename: &str,
        frames: Option<FrameSelection>,
    ) -> ViewedImageRef {
        ViewedImageRef {
            artifact_id,
            media_type: media_type.to_string(),
            filename: filename.to_string(),
            frames,
        }
    }

    fn agent_ctx(agent: &str) -> SessionContext {
        SessionContext {
            agent_id: Some(agent.to_string()),
            ..Default::default()
        }
    }

    /// Colour bars of `secs` seconds, 96×64, with no audio track.
    fn clip(dir: &std::path::Path, secs: u32) -> Vec<u8> {
        let path = dir.join("pan.mp4");
        let encoded = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args([
                "-i",
                &format!("testsrc=duration={secs}:size=96x64:rate=10"),
                "-c:v",
                "libx264",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .expect("spawn ffmpeg");
        assert!(encoded.success());
        std::fs::read(&path).unwrap()
    }

    fn images(blocks: &[ContentBlock]) -> usize {
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count()
    }

    fn texts(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn strip_strategy_for_text_only_model_without_vision_llm() {
        assert!(matches!(
            sink(None, PathBuf::from("/tmp")).vision_strategy("totally-unknown-text-only-model"),
            VisionStrategy::StripWithPlaceholder(_)
        ));
    }

    /// End-to-end through the sink — store round-trip included. The ENG-101
    /// regression lived precisely here: bytes went into the artifact store and
    /// came back out base64-encoded into a `text/plain` attachment, so the
    /// model was handed base64 labelled as the file's contents. Exercising
    /// `render` (not just `process_attachment`) is what covers the store
    /// round-trip and the `media_type` carried on the `viewed_images` handle.
    #[tokio::test]
    async fn render_returns_readable_text_for_a_text_attachment() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let yaml = "---\nviolations_and_warnings:\n  - id: 1\n    name: Oakridge\n";
        let agent = "sink-test-agent";
        let id = staged(agent, yaml.as_bytes());
        let ctx = SessionContext {
            id: Some("sink-session".to_string()),
            user_id: Some("u-test".to_string()),
            ..agent_ctx(agent)
        };

        let blocks = sink(None, dir.path().join("workspace"))
            .render(
                &ctx,
                &viewed(id, "text/plain", "config.ASHFORDyml", None),
                "claude",
            )
            .await
            .expect("render");

        let text = match &blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(
            text.contains("violations_and_warnings:") && text.contains("name: Oakridge"),
            "sink must yield readable text, got: {text}"
        );
    }

    /// A `view_attachment` of a video renders its frames into the next step:
    /// the staged bytes come back out of the store, through the frame sampler,
    /// as labelled image blocks the active (vision-capable) model can inspect.
    #[tokio::test]
    async fn render_shows_frames_for_a_video_attachment() {
        if !workforce_agent::ffmpeg::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let agent = "sink-video-agent";
        let id = staged(agent, &clip(dir.path(), 6));

        let blocks = sink(
            Some(VideoFramesConfig::default()),
            dir.path().join("workspace"),
        )
        .render(
            &agent_ctx(agent),
            &viewed(id, "video/mp4", "pan.mp4", None),
            "claude-sonnet-5",
        )
        .await
        .expect("render");

        assert_eq!(images(&blocks), 3, "6s at 2s spacing: {blocks:?}");
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("preface first");
        };
        assert!(
            text.contains("Video `pan.mp4` (0:06.0 long, 96×64)"),
            "{text}"
        );
    }

    /// A conversation holding one file.
    struct OneFile {
        file: ChannelFile,
        bytes: Vec<u8>,
    }

    #[async_trait]
    impl ChannelSurface for OneFile {
        fn platform(&self) -> &str {
            "slack"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }

        async fn add_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn read_messages(
            &self,
            channel: &str,
            _: usize,
            _: Option<&str>,
        ) -> Result<Vec<ChannelMessage>> {
            Ok(vec![ChannelMessage {
                channel: channel.to_string(),
                message_id: "100.0".to_string(),
                user: "U1".to_string(),
                user_name: None,
                channel_name: None,
                text: "what is on the shelf?".to_string(),
                timestamp: "100.0".to_string(),
                thread_id: None,
                files: vec![self.file.clone()],
                bot_id: None,
            }])
        }

        async fn fetch_file(&self, _: &ChannelFile) -> Result<Vec<u8>> {
            Ok(self.bytes.clone())
        }
    }

    /// What `view_attachment` called with `input` puts in the model's view of
    /// a conversation holding `file`: the tool's own result, then the blocks
    /// its staged file renders to.
    async fn view(
        agent: &str,
        file: OneFile,
        input: serde_json::Value,
    ) -> (workforce_agent::tools::ToolResult, Vec<ContentBlock>) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        store();
        let mut surfaces = SurfaceRegistry::new();
        surfaces.register(SurfaceKey::platform("slack"), Arc::new(file));
        let tool = ViewAttachmentTool::new(Arc::new(RwLock::new(surfaces)));
        let result = CURRENT_CHANNEL_ID
            .scope(
                "C1".to_string(),
                CURRENT_AGENT_ID.scope(agent.to_string(), tool.execute(input.clone())),
            )
            .await
            .expect("the tool never errors");
        let invocation = ToolInvocation {
            tool_name: tool.name().to_string(),
            input,
            output: result.output.clone(),
            success: result.success,
            policy_decision: "allow".to_string(),
            duration_ms: result.duration_ms,
            denial_reason: None,
            error: result.error.clone(),
        };
        let blocks = render_viewed(
            &sink(
                Some(VideoFramesConfig::default()),
                dir.path().join("workspace"),
            ),
            &agent_ctx(agent),
            &[invocation],
            "claude-sonnet-5",
        )
        .await;
        (result, blocks)
    }

    fn video(bytes: Vec<u8>) -> OneFile {
        OneFile {
            file: ChannelFile {
                id: "F1".to_string(),
                name: "pan.mp4".to_string(),
                mime_type: "application/octet-stream".to_string(),
                url: None,
                size: Some(bytes.len() as u64),
            },
            bytes,
        }
    }

    #[tokio::test]
    async fn view_attachment_takes_a_closer_look_at_a_range_of_a_video() {
        if !workforce_agent::ffmpeg::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (result, blocks) = view(
            "closer-look-range",
            video(clip(dir.path(), 8)),
            serde_json::json!({ "from": "0:02", "to": "0:04", "frames": 3 }),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(images(&blocks), 3, "{blocks:?}");
        let text = texts(&blocks);
        assert!(
            text.contains(
                "Video `pan.mp4` (0:08.0 long, 96×64): 3 still frame(s) sampled from 0:02.0 to \
                 0:04.0, in order"
            ),
            "{text}"
        );
        for label in [
            "Frame 1/3 of `pan.mp4` at 0:02.0:",
            "Frame 2/3 of `pan.mp4` at 0:03.0:",
            "Frame 3/3 of `pan.mp4` at 0:04.0:",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(
            !text.contains("saved to"),
            "a closer look is frames only: {text}"
        );
    }

    #[tokio::test]
    async fn view_attachment_shows_the_exact_frames_asked_for() {
        if !workforce_agent::ffmpeg::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (result, blocks) = view(
            "closer-look-exact",
            video(clip(dir.path(), 8)),
            serde_json::json!({ "at": ["0:05", "0:01.5", "0:40"], "detail": true }),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(images(&blocks), 3, "{blocks:?}");
        let text = texts(&blocks);
        assert!(
            text.contains("3 still frame(s) at the timestamps asked for"),
            "{text}"
        );
        assert!(
            text.contains(
                "Timestamps past 0:07.5, the last frame that can be shown, are shown as that \
                 frame."
            ),
            "{text}"
        );
        for label in ["1/3 of `pan.mp4` at 0:01.5:", "3/3 of `pan.mp4` at 0:07.5:"] {
            assert!(text.contains(label), "{label}: {text}");
        }
    }

    #[tokio::test]
    async fn view_attachment_without_a_selection_shows_the_whole_video_as_before() {
        if !workforce_agent::ffmpeg::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (result, blocks) = view(
            "closer-look-none",
            video(clip(dir.path(), 6)),
            serde_json::json!({ "reference": "pan.mp4" }),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        assert!(result.output["viewed_images"][0].get("frames").is_none());
        assert_eq!(images(&blocks), 3);
        let text = texts(&blocks);
        assert!(
            text.contains("3 still frame(s) sampled, in order"),
            "{text}"
        );
        assert!(text.contains("saved to"), "the untranscribed file: {text}");
    }

    #[tokio::test]
    async fn view_attachment_refuses_an_unreadable_selection_before_fetching() {
        let (result, blocks) = view(
            "closer-look-refused",
            video(Vec::new()),
            serde_json::json!({ "from": "0:16", "to": "0:10" }),
        )
        .await;

        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "`from` (0:16.0) must be before `to` (0:10.0); for a single moment, pass it as \
                 `at`."
            )
        );
        assert!(blocks.is_empty());
    }

    #[tokio::test]
    async fn a_selection_of_anything_but_a_video_is_said_to_be_ignored() {
        let notes = OneFile {
            file: ChannelFile {
                id: "F2".to_string(),
                name: "notes.txt".to_string(),
                mime_type: "text/plain".to_string(),
                url: None,
                size: Some(11),
            },
            bytes: b"shelf: tins".to_vec(),
        };
        let (result, blocks) = view(
            "closer-look-text",
            notes,
            serde_json::json!({ "at": ["0:13"] }),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        let text = texts(&blocks);
        assert!(
            text.starts_with(
                "`notes.txt` is not a video, so `from`, `to`, `at`, `frames` and `detail` do not \
                 apply and it is shown whole."
            ),
            "{text}"
        );
        assert!(text.contains("shelf: tins"), "{text}");
    }

    #[tokio::test]
    async fn a_closer_look_with_frames_turned_off_says_so() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let agent = "closer-look-off";
        let id = staged(agent, b"not decoded");
        let selection = FrameSelection {
            at: vec![13.0],
            ..Default::default()
        };
        let blocks = sink(None, dir.path().join("workspace"))
            .render(
                &agent_ctx(agent),
                &viewed(id, "video/mp4", "pan.mp4", Some(selection)),
                "claude-sonnet-5",
            )
            .await
            .expect("render");

        assert_eq!(
            texts(&blocks),
            "Video `pan.mp4`: video frames are turned off for this agent ([attachments.video]), \
             so there is no closer look to take."
        );
    }
}

//! `view_attachment` contract — the inbound counterpart to [`super::image_gen`].
//!
//! Where `generate_image` stages bytes and a sink delivers them *out* to the
//! channel, `view_attachment` stages a file already present in the conversation
//! and a sink renders it *in* as model-visible content blocks. The marker keeps
//! its `viewed_images` name for back-compat but carries any attachment (image,
//! PDF, audio/video, text/office). The tool itself (which needs channel-surface
//! access) lives in the skills crate; this module holds the shared contract: the
//! result marker the runner keys on, the staging step every producer shares
//! ([`stage_for_view`]), and the sink trait the hub implements.
//!
//! The output shape is the contract: a `viewed_images` array of
//! `{ artifact_id, media_type, filename }` entries, each with the `frames` of
//! a video to show when the view asks for a closer look.

use async_trait::async_trait;
use serde_json::Value;

use crate::artifacts::store::{global_store, ArtifactStore, ATTACHMENT_TTL};
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};
use crate::attachments::FrameSelection;
use crate::llm::types::ContentBlock;
use crate::runner::{SessionContext, CURRENT_AGENT_ID};

/// Marker key in a tool result whose presence tells the runner this turn
/// staged images that must be rendered into the conversation.
pub const VIEWED_IMAGES_KEY: &str = "viewed_images";

const FRAMES_KEY: &str = "frames";

/// A staged file referenced from a `view_attachment` tool result: the bytes
/// live in the artifact store, this is the handle the render layer resolves.
#[derive(Debug, Clone)]
pub struct ViewedImageRef {
    pub artifact_id: ArtifactId,
    pub media_type: String,
    pub filename: String,
    /// The frames to show, when the file is a video opened for a closer look.
    pub frames: Option<FrameSelection>,
}

/// Extract the staged-file handles from a `view_attachment` tool result's
/// output. Returns empty for any other tool, so the runner can call this
/// indiscriminately on every successful tool result.
pub fn parse_viewed_images(output: &Value) -> Vec<ViewedImageRef> {
    let Some(entries) = output.get(VIEWED_IMAGES_KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let artifact_id = ArtifactId::parse(e.get("artifact_id")?.as_str()?).ok()?;
            Some(ViewedImageRef {
                artifact_id,
                media_type: e
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_string(),
                filename: e
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("image")
                    .to_string(),
                frames: e
                    .get(FRAMES_KEY)
                    .and_then(|frames| serde_json::from_value(frames.clone()).ok()),
            })
        })
        .collect()
}

/// Build a single `viewed_images` entry — the marker shape every producer emits
/// and [`parse_viewed_images`] consumes. Co-locating it with the parser keeps
/// producers and the runner's render contract in lockstep.
pub fn viewed_image_entry(artifact_id: &str, media_type: &str, filename: &str) -> Value {
    serde_json::json!({
        "artifact_id": artifact_id,
        "media_type": media_type,
        "filename": filename,
    })
}

/// Per-file cap on bytes a producer pulls into memory and stages for viewing —
/// matches the Slack inbound attachment limit.
pub const MAX_VIEW_BYTES: u64 = 20 * 1024 * 1024;

/// Whether a file of `size` bytes is within [`MAX_VIEW_BYTES`].
pub fn within_view_cap(size: u64) -> bool {
    size <= MAX_VIEW_BYTES
}

/// A file staged for viewing: its artifact id and the `viewed_images` array
/// value a producer puts under [`VIEWED_IMAGES_KEY`].
#[derive(Debug, Clone)]
pub struct StagedView {
    pub artifact_id: String,
    pub marker: Value,
}

impl StagedView {
    /// This view narrowed to the frames `selection` picks, should the file
    /// be a video.
    pub fn selecting(mut self, selection: &FrameSelection) -> Self {
        for entry in self.marker.as_array_mut().into_iter().flatten() {
            entry[FRAMES_KEY] = serde_json::json!(selection);
        }
        self
    }
}

/// Stage `bytes` in the agent-scoped artifact store (with [`ATTACHMENT_TTL`])
/// and build the marker that renders them into the model's view. Fails
/// cleanly outside an agent turn or when no store is configured.
pub fn stage_for_view(
    producer: &str,
    bytes: Vec<u8>,
    media_type: &str,
    filename: &str,
) -> Result<StagedView, String> {
    let agent_id = CURRENT_AGENT_ID.try_with(Clone::clone).ok();
    stage_in(
        global_store().as_deref(),
        agent_id.as_deref(),
        producer,
        bytes,
        media_type,
        filename,
    )
}

fn stage_in(
    store: Option<&dyn ArtifactStore>,
    agent_id: Option<&str>,
    producer: &str,
    bytes: Vec<u8>,
    media_type: &str,
    filename: &str,
) -> Result<StagedView, String> {
    let store = store.ok_or("artifact store is not configured; cannot stage the file")?;
    let agent_id = agent_id
        .filter(|id| !id.is_empty())
        .ok_or("no agent context for this call; cannot stage the file")?;
    let descriptor = ArtifactDescriptor::from_context(producer, ContentType::OctetStream, agent_id)
        .with_ttl(ATTACHMENT_TTL);
    let handle = store
        .write(agent_id, descriptor, ArtifactBody::Bytes(bytes))
        .map_err(|e| format!("failed to stage `{filename}`: {e}"))?;
    let artifact_id = handle.id.as_str().to_string();
    Ok(StagedView {
        marker: Value::Array(vec![viewed_image_entry(&artifact_id, media_type, filename)]),
        artifact_id,
    })
}

/// The note a producer returns alongside its marker: the file renders on the
/// model's next step, so it must wait before describing the contents.
pub fn view_pending_note(filename: &str) -> String {
    format!(
        "Loaded `{filename}`. The system is rendering it into your view — it will appear on your \
         next step. Wait for it before describing the contents."
    )
}

/// Renders a staged attachment into the content blocks the active model can
/// consume.
///
/// The runner owns an optional `Arc<dyn ImageViewSink>` and calls
/// [`Self::render`] as soon as a `view_attachment` tool result lands, appending
/// the returned blocks to the tool-result turn so the model sees the file on its
/// next step. The implementation (in the hub) routes by media type through the
/// same pipeline as inbound attachments, honouring the active model's
/// vision/document capability (native block, secondary-LLM extraction, or a
/// placeholder).
#[async_trait]
pub trait ImageViewSink: Send + Sync {
    /// Resolve `viewed` from the artifact store (namespaced by
    /// `ctx.agent_id`) and render it for `active_model`. Returning an empty vec
    /// yields no appended content.
    async fn render(
        &self,
        ctx: &SessionContext,
        viewed: &ViewedImageRef,
        active_model: &str,
    ) -> eyre::Result<Vec<ContentBlock>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_viewed_images_reads_entries_and_defaults() {
        let id = ArtifactId::fresh();
        let output = serde_json::json!({
            VIEWED_IMAGES_KEY: [
                { "artifact_id": id.as_str(), "media_type": "image/jpeg", "filename": "a.jpg" },
                { "artifact_id": id.as_str() }
            ]
        });
        let refs = parse_viewed_images(&output);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].media_type, "image/jpeg");
        assert_eq!(refs[0].filename, "a.jpg");
        assert_eq!(
            refs[0].frames, None,
            "the whole file unless frames are picked"
        );
        assert_eq!(refs[1].media_type, "image/png", "media_type defaults");
        assert_eq!(refs[1].filename, "image", "filename defaults");
    }

    #[test]
    fn parse_viewed_images_empty_for_other_tools() {
        assert!(parse_viewed_images(&serde_json::json!({ "content": "x" })).is_empty());
    }

    #[test]
    fn viewed_image_entry_round_trips_through_parser() {
        let id = ArtifactId::fresh();
        let output = serde_json::json!({
            VIEWED_IMAGES_KEY: [viewed_image_entry(id.as_str(), "application/pdf", "spec.pdf")]
        });
        let refs = parse_viewed_images(&output);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].artifact_id.as_str(), id.as_str());
        assert_eq!(refs[0].media_type, "application/pdf");
        assert_eq!(refs[0].filename, "spec.pdf");
    }

    #[test]
    fn within_view_cap_accepts_up_to_limit_and_rejects_above() {
        assert!(within_view_cap(0));
        assert!(within_view_cap(MAX_VIEW_BYTES));
        assert!(!within_view_cap(MAX_VIEW_BYTES + 1));
    }

    #[test]
    fn stage_in_fails_without_a_store() {
        let err = stage_in(
            None,
            Some("agent"),
            "t",
            b"x".to_vec(),
            "text/plain",
            "a.txt",
        )
        .unwrap_err();
        assert!(err.contains("artifact store is not configured"), "{err}");
    }

    #[test]
    fn stage_for_view_fails_without_an_agent_id() {
        crate::artifacts::store::install_test_global_store();
        let outside = stage_for_view("t", b"x".to_vec(), "text/plain", "a.txt").unwrap_err();
        assert!(outside.contains("no agent context"), "{outside}");
        let empty = CURRENT_AGENT_ID
            .sync_scope(String::new(), || {
                stage_for_view("t", b"x".to_vec(), "text/plain", "a.txt")
            })
            .unwrap_err();
        assert!(empty.contains("no agent context"), "{empty}");
    }

    #[test]
    fn stage_for_view_stages_bytes_and_marker_round_trips() {
        let store = crate::artifacts::store::install_test_global_store();
        let agent = "image-view-stage";
        let staged = CURRENT_AGENT_ID
            .sync_scope(agent.to_string(), || {
                stage_for_view(
                    "email.get_attachment",
                    b"%PDF-1.7".to_vec(),
                    "application/pdf",
                    "spec.pdf",
                )
            })
            .expect("staged");

        let refs = parse_viewed_images(&serde_json::json!({ VIEWED_IMAGES_KEY: staged.marker }));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].artifact_id.as_str(), staged.artifact_id);
        assert_eq!(refs[0].media_type, "application/pdf");
        assert_eq!(refs[0].filename, "spec.pdf");

        let selection = FrameSelection {
            at: vec![13.0],
            detail: true,
            ..Default::default()
        };
        let narrowed = staged.clone().selecting(&selection);
        let refs = parse_viewed_images(&serde_json::json!({ VIEWED_IMAGES_KEY: narrowed.marker }));
        assert_eq!(refs[0].artifact_id.as_str(), staged.artifact_id);
        assert_eq!(refs[0].frames, Some(selection));

        let descriptor = store
            .metadata(agent, &refs[0].artifact_id)
            .unwrap()
            .expect("descriptor");
        assert_eq!(descriptor.producer, "email.get_attachment");
        assert_eq!(descriptor.ttl(), ATTACHMENT_TTL);
        let ArtifactBody::Bytes(bytes) = store.read(agent, &refs[0].artifact_id).unwrap() else {
            panic!("expected raw bytes");
        };
        assert_eq!(bytes, b"%PDF-1.7");
    }

    #[test]
    fn view_pending_note_names_the_file_and_defers_description() {
        let note = view_pending_note("a.pdf");
        assert!(note.starts_with("Loaded `a.pdf`."), "{note}");
        assert!(note.contains("next step"), "{note}");
    }
}

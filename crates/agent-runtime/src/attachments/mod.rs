//! Inbound attachment processing: what a user sent becomes model input.
//!
//! Shared by every surface that accepts a file — the hub (Slack, iOS, the web
//! UI) and the customer-facing edge widget. It lived in `hub::serve` until the
//! edge needed it; nothing here ever depended on hub-only state, so the move was
//! a relocation rather than a redesign.
//!
//! Routing per category:
//!   - images  — via [`VisionStrategy`] (native / described / placeholder)
//!   - PDFs    — via [`VisionStrategy`] ([`pdf`]: text first, page-render fallback)
//!   - audio   — via [`TranscriptionStrategy`] ([`audio`])
//!   - video   — [`frames`] for what it shows, [`video`] → [`audio`] for what it says
//!   - office  — docx and spreadsheets become text/tables (works on every model)
//!   - text    — inlined, or staged as a table past the row threshold
//!   - binary  — saved to the workspace with a path note
//!
//! Large payloads arrive BY REFERENCE (`SessionAttachment::artifact_id`) so the
//! bytes never ride a WebSocket frame, a NATS message, or a persisted session
//! row. Resolving one re-checks its owner and producer — that check is what
//! keeps one user's upload out of another user's turn.

use std::path::Path;
use std::sync::Arc;

use crate::security::external_content::wrap_for_trust;
use eyre::Result;
use workforce_common::secret::NoKey;
use workforce_common::session::{is_text_like_mime, SessionAttachment};
use workforce_common::settings::{LlmTranscriptionConfig, LlmVisionConfig, VideoFramesConfig};

use crate::llm::types::{ContentBlock, ImageSource};
use crate::runner::SessionContext;
use crate::tools::config_guard;
use crate::tools::paths::CanonicalRoots;

pub mod audio;
pub mod frames;
pub use frames::FrameSelection;
pub mod pdf;
pub mod pdf_render;
pub mod video;

/// An agent's `[llm.vision]` describer, as its startup left it.
#[derive(Clone, Default)]
pub enum Describer {
    /// No `[llm.vision]`, or it is switched off.
    #[default]
    Unconfigured,
    Ready(Arc<dyn crate::vision::VisionLlm>, LlmVisionConfig),
    /// `[llm.vision]` names nothing that can be called, and why.
    Misconfigured(String),
}

impl Describer {
    pub fn llm(&self) -> Option<&Arc<dyn crate::vision::VisionLlm>> {
        match self {
            Self::Ready(llm, _) => Some(llm),
            Self::Unconfigured | Self::Misconfigured(_) => None,
        }
    }
}

/// Why a model that cannot read an attachment has nobody to describe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDescriber<'a> {
    Unconfigured,
    Misconfigured(&'a str),
}

impl std::fmt::Display for NoDescriber<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unconfigured => f.write_str("no vision provider is configured"),
            Self::Misconfigured(why) => write!(
                f,
                "vision is misconfigured: {why}. Tell the user, so that an admin can fix it"
            ),
        }
    }
}

/// How image and PDF attachments should be routed when building task content.
///
/// Decided by [`resolve_vision_strategy_for`] from the active model and the
/// agent's [`Describer`].
pub enum VisionStrategy<'a> {
    /// Active model handles images and PDFs natively. Pass blocks through.
    Native,
    /// Active model reads images natively but documents only as text: an
    /// agent CLI, whose input carries images and no documents. Images pass
    /// through; PDFs become their extracted text.
    NativeImages,
    /// Active model is text-only and a vision LLM is configured. Describe
    /// images directly; for PDFs, use the hybrid pipeline (text extraction
    /// first, vision page-render fallback).
    DescribeWith(&'a dyn crate::vision::VisionLlm, &'a LlmVisionConfig),
    /// Active model is text-only and nothing describes for it. Substitute a
    /// placeholder text block saying why, so the agent at least knows the
    /// attachment existed.
    StripWithPlaceholder(NoDescriber<'a>),
}

/// Decide how to present an image to `active_model`: natively if it reads
/// images and the route it is called on (its prefix, else `primary`) sends
/// them, via the agent's describer otherwise, else a placeholder. Shared by
/// inbound-attachment routing and `view_attachment` rendering.
pub fn resolve_vision_strategy_for<'a>(
    tiers: &crate::llm::vision_tiers::VisionTierRegistry,
    describer: &'a Describer,
    active_model: &str,
    primary: crate::llm::provider::ProviderKind,
) -> VisionStrategy<'a> {
    use crate::llm::provider::ProviderKind;
    let route = crate::llm::tiers::split_routing_prefix(active_model)
        .0
        .and_then(ProviderKind::from_str_loose)
        .unwrap_or(primary);
    if matches!(route, ProviderKind::ClaudeCode | ProviderKind::Codex) {
        return VisionStrategy::NativeImages;
    }
    if route.carries_images() && tiers.is_vision_capable(active_model) {
        return VisionStrategy::Native;
    }
    match describer {
        Describer::Ready(llm, cfg) => VisionStrategy::DescribeWith(&**llm, cfg),
        Describer::Unconfigured => VisionStrategy::StripWithPlaceholder(NoDescriber::Unconfigured),
        Describer::Misconfigured(why) => {
            VisionStrategy::StripWithPlaceholder(NoDescriber::Misconfigured(why))
        }
    }
}

/// An agent's `[llm.transcription]` transcriber, as its startup left it.
#[derive(Clone, Default)]
pub enum Transcriber {
    /// No `[llm.transcription]`, or it is switched off.
    #[default]
    Unconfigured,
    Ready(
        Arc<dyn crate::transcription::TranscriptionProvider>,
        LlmTranscriptionConfig,
    ),
    /// `[llm.transcription]` names nothing that can be called, and why.
    Misconfigured(String),
}

impl Transcriber {
    /// How this turn's audio and video are routed.
    pub fn strategy(&self) -> TranscriptionStrategy<'_> {
        match self {
            Self::Ready(provider, cfg) => TranscriptionStrategy::TranscribeWith(&**provider, cfg),
            Self::Unconfigured => TranscriptionStrategy::Disabled,
            Self::Misconfigured(why) => TranscriptionStrategy::Misconfigured(why),
        }
    }
}

/// How audio and video attachments should be routed when building task content.
///
/// Unlike [`VisionStrategy`], there is no `Native` variant — no agent's
/// primary chat model accepts raw audio in our matrix today. Audio either
/// flows through a configured transcription provider or falls through to the
/// saved-to-workspace path.
pub enum TranscriptionStrategy<'a> {
    /// A transcription provider is configured. Audio is transcribed; video
    /// has its audio track extracted (when ffmpeg is available) and then
    /// transcribed.
    TranscribeWith(
        &'a dyn crate::transcription::TranscriptionProvider,
        &'a LlmTranscriptionConfig,
    ),
    /// No transcription provider configured. Audio/video fall through to
    /// `process_binary_attachment` — the agent sees the file path and can
    /// use file tools to read it.
    Disabled,
    /// `[llm.transcription]` names nothing that can be called. Audio/video
    /// are saved as when disabled, with a note saying why they were not
    /// transcribed.
    Misconfigured(&'a str),
}

impl TranscriptionStrategy<'_> {
    /// `saved`, the block a recording left untranscribed is saved as, and
    /// the reason when that is a misconfiguration.
    fn untranscribed(&self, filename: &str, saved: ContentBlock) -> Vec<ContentBlock> {
        let note = match self {
            Self::Misconfigured(why) => Some(ContentBlock::Text {
                text: format!(
                    "`{filename}` was not transcribed: transcription is misconfigured: {why}. \
                     Tell the user, so that an admin can fix it."
                ),
            }),
            Self::TranscribeWith(..) | Self::Disabled => None,
        };
        std::iter::once(saved).chain(note).collect()
    }
}

/// Bundle of secondary-LLM routing decisions for a single turn.
///
/// One bag instead of N independent strategy parameters keeps the
/// `build_task_content` signature stable as we add modalities.
pub struct MediaStrategies<'a> {
    pub vision: VisionStrategy<'a>,
    pub transcription: TranscriptionStrategy<'a>,
    /// This turn's `[attachments.video]`; `None` shows no video frames.
    pub video_frames: Option<VideoFramesConfig>,
}

/// Build task content blocks from the message text and optional attachments.
///
/// Routing per category:
/// - Images: via [`VisionStrategy`] (native / described / placeholder)
/// - PDFs: via [`VisionStrategy`] (native `Document` / hybrid pipeline / placeholder)
/// - Audio: via [`TranscriptionStrategy`] (transcribe / fall through)
/// - Video: frames via [`VisionStrategy`], then audio via [`TranscriptionStrategy`]
///   (extract + transcribe / saved to the workspace)
/// - DOCX/spreadsheets: text extraction (works on all LLMs)
/// - text/* and code: wrapped as text content block
/// - Other binaries: saved to `{workspace}/uploads/` with a path note
///
/// `turn_context` is the hub's per-turn context block (clock, notes, recall,
/// lessons, runtime), when the turn has one. It leads the message so the
/// system prompt stays byte-identical across turns and the replayed history
/// can be read from the cache; it is hub-produced, so it is its own content
/// block OUTSIDE the trust wrap, and the caller tokenises it.
///
/// `injected_block` is the answer-ready CURRENT SOURCE CODE block, when the
/// turn has one. It is system-produced (read from the repository at HEAD), so
/// it is composed as its own content block OUTSIDE the trust wrap — wrapping it
/// with the sender's message labelled the system's own source as untrusted user
/// content for paired senders, undercutting the header that declares it
/// authoritative. The caller tokenizes it before passing it in: repo-committed
/// credentials must get the same treatment as every other model-bound surface.
#[allow(clippy::too_many_arguments)]
pub async fn build_task_content(
    turn_context: Option<&str>,
    injected_block: Option<&str>,
    message: &str,
    attachments: &Option<Vec<SessionAttachment>>,
    workspace: &Path,
    strategies: MediaStrategies<'_>,
    trust_level: Option<workforce_common::trust::TrustLevel>,
    staging: &SessionContext,
) -> Vec<ContentBlock> {
    let mut content: Vec<ContentBlock> = [turn_context, injected_block]
        .into_iter()
        .flatten()
        .map(|text| ContentBlock::Text {
            text: text.to_string(),
        })
        .collect();
    content.push(ContentBlock::Text {
        text: wrap_for_trust(message, trust_level),
    });
    let Some(atts) = attachments.as_ref() else {
        return content;
    };
    for att in atts {
        content.extend(process_attachment(att, &strategies, workspace, staging).await);
    }
    content
}

pub async fn process_attachment(
    att: &SessionAttachment,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
    staging: &SessionContext,
) -> Vec<ContentBlock> {
    // Clients routinely send a generic mime — browsers leave `File.type`
    // empty for `.opus` (the web UI then substitutes octet-stream), mail
    // clients attach `.docx`/`.pdf` as octet-stream — which would route a
    // readable file to the saved-to-workspace path. Recover the real type from
    // the extension; the corrected mime is threaded all the way down (the
    // transcription providers pick their upload format from it).
    let retyped = infer_mime_from_extension(&att.filename, &att.mime_type)
        .and_then(|mime| retype_attachment(att, mime));
    let att = retyped.as_ref().unwrap_or(att);
    if let Some(artifact_id) = att.artifact_id.clone() {
        return process_by_ref_attachment(att, &artifact_id, strategies, workspace, staging).await;
    }
    if att.mime_type.starts_with("image/") {
        return vec![process_image_attachment(att, &strategies.vision).await];
    }
    if att.mime_type == "application/pdf" {
        return process_pdf_attachment(att, &strategies.vision).await;
    }
    if att.mime_type.starts_with("audio/") {
        return process_audio_attachment(att, &strategies.transcription, workspace).await;
    }
    if att.mime_type.starts_with("video/") {
        return process_video_attachment(att, strategies, workspace).await;
    }
    if is_spreadsheet_mime(&att.mime_type) || is_docx_mime(&att.mime_type) {
        return process_office_attachment(att, staging);
    }
    if is_text_like_mime(&att.mime_type) {
        return vec![process_text_attachment(att, workspace, staging)];
    }
    vec![process_binary_attachment(workspace, att)]
}

/// Recover a specific mime from the filename extension when the client sent a
/// generic one (empty or `application/octet-stream`); a specific mime is never
/// overridden.
///
/// Limited to formats with a dedicated handler that can read them: AV formats
/// every transcription backend handles (the whisper hosts accept these
/// extensions natively, `.opus` after the provider's `.ogg` rename, and the
/// chat path transcodes to WAV), plus PDF, images, `.docx`/`.xlsx` and plain
/// text. A failed office or text conversion yields a placeholder, so formats
/// the converters cannot open (legacy `.doc`/`.xls`, `.ods`) are deliberately
/// absent: they keep the binary saved-to-workspace fallback, where file tools
/// can still reach them, instead of being lost behind a failed conversion.
fn infer_mime_from_extension(filename: &str, mime_type: &str) -> Option<&'static str> {
    let generic =
        mime_type.is_empty() || mime_type.eq_ignore_ascii_case("application/octet-stream");
    if !generic {
        return None;
    }
    let ext = std::path::Path::new(filename)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        // WhatsApp voice notes: Opus in an Ogg container.
        "opus" | "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        // Video routes through ffmpeg audio extraction, which reads any of
        // these containers.
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "md" => "text/markdown",
        _ => return None,
    })
}

/// Relabel `att` as `mime` when its payload can be read as one; `None` keeps
/// the generic mime.
///
/// An inline payload under a generic mime is base64 (see
/// [`SessionAttachment::from_bytes`]), so a text-like target is re-encoded as
/// readable UTF-8. Images and PDFs reach the provider natively, which rejects
/// bytes that do not match the declared type, so they are relabelled only when
/// their magic bytes agree — with the subtype the bytes show, not the
/// extension's. A by-reference payload has no bytes to check yet, so those are
/// left generic here and checked once [`materialize_by_ref`] inlines them.
fn retype_attachment(att: &SessionAttachment, mime: &str) -> Option<SessionAttachment> {
    use base64::Engine;
    let relabel = |mime: &str| SessionAttachment {
        mime_type: mime.to_string(),
        ..att.clone()
    };
    let inline = att.artifact_id.is_none();
    if mime.starts_with("image/") || mime == "application/pdf" {
        let head = inline
            .then(|| {
                let prefix = att.data.get(..16).unwrap_or(&att.data);
                base64::engine::general_purpose::STANDARD
                    .decode(prefix)
                    .ok()
            })
            .flatten()?;
        let actual = crate::llm::image::detect_image_media_type(&head)
            .or_else(|| head.starts_with(b"%PDF-").then_some("application/pdf"))?;
        return (actual.starts_with("image/") == mime.starts_with("image/"))
            .then(|| relabel(actual));
    }
    let decoded = (inline && is_text_like_mime(mime))
        .then(|| decode_attachment_base64(att).ok())
        .flatten();
    Some(match decoded {
        Some(bytes) => SessionAttachment::from_bytes(&att.filename, mime, &bytes),
        None => relabel(mime),
    })
}

/// Producers whose artifacts a chat message may reference as an attachment
/// payload (`SessionAttachment::artifact_id`). Anything else — staged query
/// tables, generated images, exports — stays reachable only through its own
/// tool surface: a leaked artifact id in a transcript must not become a
/// generic read primitive.
/// Producer recorded on a file a USER uploaded to a session. Every surface that
/// accepts an upload must stamp this — it is what [`BYREF_ATTACHMENT_PRODUCERS`]
/// admits, so a surface that invents its own name silently has every attachment
/// rejected as "not an uploaded attachment".
pub const SESSION_UPLOAD_PRODUCER: &str = "session.upload";

/// Producers whose artifacts may be consumed as a by-reference attachment.
/// Deliberately closed: it is what stops a turn passing off some other tool's
/// artifact — whose id it might have learned — as a file the user sent.
const BYREF_ATTACHMENT_PRODUCERS: &[&str] = &[SESSION_UPLOAD_PRODUCER, "channel.download"];

/// Ceiling for materialising a by-reference attachment into memory when its
/// category needs bytes (image / PDF / office / text). Audio, video, and
/// anything bigger than this stay on disk end-to-end.
const MAX_BYREF_INLINE_BYTES: u64 = 64 * 1024 * 1024;

/// Process an attachment whose payload lives in the artifact store. Errors
/// become a placeholder block — the model must hear that a file arrived and
/// why it can't see it, not silence.
async fn process_by_ref_attachment(
    att: &SessionAttachment,
    artifact_id: &str,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
    staging: &SessionContext,
) -> Vec<ContentBlock> {
    match materialize_by_ref(att, artifact_id, strategies, workspace, staging).await {
        Ok(blocks) => blocks,
        Err(e) => {
            tracing::warn!(
                file = %att.filename,
                artifact_id,
                error = %e,
                "by-reference attachment failed to resolve"
            );
            vec![ContentBlock::Text {
                text: format!(
                    "File `{}` was attached by reference but could not be loaded: {e}. \
                     Uploads expire after a day — ask the user to send it again if needed.",
                    att.filename
                ),
            }]
        }
    }
}

async fn materialize_by_ref(
    att: &SessionAttachment,
    artifact_id: &str,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
    staging: &SessionContext,
) -> Result<Vec<ContentBlock>> {
    use crate::artifacts::{global_store, ArtifactId};

    let store = global_store().ok_or_else(|| eyre::eyre!("artifact store unavailable"))?;
    let id = ArtifactId::parse(artifact_id)?;
    let agent_id = staging
        .agent_id
        .clone()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| eyre::eyre!("no agent context for attachment resolution"))?;
    let descriptor = store
        .metadata(&agent_id, &id)?
        .ok_or_else(|| eyre::eyre!("upload not found or expired"))?;

    if !BYREF_ATTACHMENT_PRODUCERS.contains(&descriptor.producer.as_str()) {
        return Err(eyre::eyre!("artifact is not an uploaded attachment"));
    }
    // The uploader must be the sender of the message consuming it. A
    // producer that couldn't resolve an identity is only accepted inside
    // the exact session it was uploaded to.
    let same_user = matches!(
        (&descriptor.user_id, &staging.user_id),
        (Some(owner), Some(current)) if owner == current
    );
    let same_session_fallback = descriptor.user_id.is_none()
        && descriptor.session_id.is_some()
        && descriptor.session_id == staging.id;
    if !same_user && !same_session_fallback {
        return Err(eyre::eyre!("upload belongs to a different user"));
    }

    // Spool next to the store when it lives on disk: same filesystem, so the
    // Fs copy is cheap — and never the OS temp dir first, which is tmpfs (RAM)
    // on typical container hosts and would silently turn "on disk" into
    // "in memory". OS temp remains the fallback for byte-backed stores.
    let staging_dir = store.staging_dir(&agent_id);
    let tmpdir = match &staging_dir {
        Some(_) => None,
        None => Some(tempfile::TempDir::new()?),
    };
    let dir = staging_dir
        .or_else(|| tmpdir.as_ref().map(|t| t.path().to_path_buf()))
        .expect("staging dir or tempdir");
    let local = dir.join(format!(".byref-{}.tmp", uuid::Uuid::new_v4()));
    // Every consumer below either renames the file away (workspace save) or
    // just reads it — removing on drop covers all error paths and a
    // cancelled turn, and is a no-op after a rename.
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = RemoveOnDrop(local.clone());
    {
        // A 1 GiB copy (or S3 download) is not tokio-worker work.
        let store = std::sync::Arc::clone(&store);
        let agent = agent_id.clone();
        let artifact = id.clone();
        let dest = local.clone();
        tokio::task::spawn_blocking(move || store.read_to_file(&agent, &artifact, &dest))
            .await
            .map_err(|e| eyre::eyre!("attachment fetch task panicked: {e}"))??;
    }

    if att.mime_type.starts_with("audio/") {
        if let TranscriptionStrategy::TranscribeWith(provider, cfg) = &strategies.transcription {
            return crate::attachments::audio::process_audio_file(
                &att.filename,
                &local,
                &att.mime_type,
                *provider,
                cfg,
            )
            .await;
        }
        return Ok(strategies.transcription.untranscribed(
            &att.filename,
            save_attachment_from_path(workspace, att, &local).await,
        ));
    }
    if att.mime_type.starts_with("video/") {
        return Ok(process_video_file(att, &local, strategies, workspace).await);
    }

    // Categories whose processors need the bytes in memory — plus a generic
    // mime whose extension names an image or PDF, which is relabelled once its
    // bytes are inline and can be checked. Bounded: a file over the ceiling
    // lands in the workspace instead, where file tools can still get at it.
    let needs_bytes = att.mime_type.starts_with("image/")
        || att.mime_type == "application/pdf"
        || is_spreadsheet_mime(&att.mime_type)
        || is_docx_mime(&att.mime_type)
        || is_text_like_mime(&att.mime_type)
        || infer_mime_from_extension(&att.filename, &att.mime_type).is_some();
    let size = std::fs::metadata(&local)?.len();
    if needs_bytes && size <= MAX_BYREF_INLINE_BYTES {
        let bytes = std::fs::read(&local)?;
        let inline = SessionAttachment::from_bytes(&att.filename, &att.mime_type, &bytes);
        return Ok(Box::pin(process_attachment(&inline, strategies, workspace, staging)).await);
    }
    Ok(vec![
        save_attachment_from_path(workspace, att, &local).await,
    ])
}

/// File-based sibling of [`process_binary_attachment`]: move an
/// already-on-disk payload into `{workspace}/uploads/` and hand the agent
/// the path, without the bytes ever transiting memory.
async fn save_attachment_from_path(
    workspace: &Path,
    att: &SessionAttachment,
    source: &Path,
) -> ContentBlock {
    // The rename degrades to a full copy across filesystems — at 1 GiB
    // that's blocking-pool work, not tokio-worker work.
    let workspace = workspace.to_path_buf();
    let filename = att.filename.clone();
    let source = source.to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> Result<String> {
        let upload = Upload::at(&workspace, &filename)?;
        if config_guard::rename_into(&upload.roots, &source, &upload.real).is_err() {
            upload.write(&mut std::fs::File::open(&source)?)?;
        }
        tracing::info!(file = %upload.name, "Saved by-reference attachment to workspace");
        Ok(upload.shown())
    })
    .await
    .map_err(|e| eyre::eyre!("save task panicked: {e}"))
    .and_then(|r| r);
    match result {
        Ok(path) => ContentBlock::Text {
            text: format!(
                "Binary file `{}` ({}) saved to: {}\nUse your file tools to read or process it.",
                att.filename, att.mime_type, path
            ),
        },
        Err(e) => {
            tracing::warn!(file = %att.filename, "Failed to save by-reference attachment: {e}");
            ContentBlock::Text {
                text: format!(
                    "Binary file `{}` ({}) was attached but could not be saved: {e}",
                    att.filename, att.mime_type
                ),
            }
        }
    }
}

pub async fn process_image_attachment(
    att: &SessionAttachment,
    strategy: &VisionStrategy<'_>,
) -> ContentBlock {
    match strategy {
        VisionStrategy::Native | VisionStrategy::NativeImages => ContentBlock::Image {
            source: base64_source(&att.mime_type, &att.data),
        },
        VisionStrategy::DescribeWith(llm, _) => {
            match llm
                .describe_image(&att.mime_type, &att.data, Some(&att.filename))
                .await
            {
                Ok(desc) => ContentBlock::Text {
                    text: format!(
                        "Image `{}` (described by {}):\n{}",
                        att.filename,
                        llm.model_name(),
                        desc
                    ),
                },
                Err(e) if e.is::<NoKey>() => ContentBlock::Text {
                    text: format!(
                        "Image `{}` was attached but not described: {e}",
                        att.filename
                    ),
                },
                Err(e) => {
                    tracing::warn!(
                        file = %att.filename,
                        error = %e,
                        "Vision describe failed; substituting placeholder"
                    );
                    ContentBlock::Text {
                        text: format!(
                            "Image `{}` was attached but the vision provider failed: {e}",
                            att.filename
                        ),
                    }
                }
            }
        }
        VisionStrategy::StripWithPlaceholder(why) => ContentBlock::Text {
            text: format!(
                "Image `{}` was attached but the active model cannot process images and {why}.",
                att.filename
            ),
        },
    }
}

async fn process_pdf_attachment(
    att: &SessionAttachment,
    strategy: &VisionStrategy<'_>,
) -> Vec<ContentBlock> {
    match strategy {
        VisionStrategy::Native => vec![ContentBlock::Document {
            source: base64_source(&att.mime_type, &att.data),
        }],
        VisionStrategy::NativeImages => {
            crate::attachments::pdf::text_only(&att.filename, &att.data)
        }
        VisionStrategy::DescribeWith(llm, cfg) => {
            match crate::attachments::pdf::process_pdf(&att.filename, &att.data, *llm, cfg).await {
                Ok(blocks) => blocks,
                Err(e) => {
                    tracing::warn!(file = %att.filename, error = %e, "PDF pipeline failed; substituting placeholder");
                    vec![ContentBlock::Text {
                        text: format!(
                            "PDF `{}` was attached but processing failed: {e}",
                            att.filename
                        ),
                    }]
                }
            }
        }
        VisionStrategy::StripWithPlaceholder(why) => vec![ContentBlock::Text {
            text: format!(
                "PDF `{}` was attached but the active model cannot read PDFs and {why}.",
                att.filename
            ),
        }],
    }
}

async fn process_audio_attachment(
    att: &SessionAttachment,
    strategy: &TranscriptionStrategy<'_>,
    workspace: &Path,
) -> Vec<ContentBlock> {
    match strategy {
        TranscriptionStrategy::TranscribeWith(provider, cfg) => {
            match crate::attachments::audio::process_audio(
                &att.filename,
                &att.data,
                &att.mime_type,
                *provider,
                cfg,
            )
            .await
            {
                Ok(blocks) => blocks,
                Err(e) => {
                    tracing::warn!(file = %att.filename, error = %e, "audio pipeline failed; substituting placeholder");
                    vec![ContentBlock::Text {
                        text: format!(
                            "Audio `{}` was attached but processing failed: {e}",
                            att.filename
                        ),
                    }]
                }
            }
        }
        TranscriptionStrategy::Disabled | TranscriptionStrategy::Misconfigured(_) => {
            strategy.untranscribed(&att.filename, process_binary_attachment(workspace, att))
        }
    }
}

async fn process_video_attachment(
    att: &SessionAttachment,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
) -> Vec<ContentBlock> {
    if strategies.video_frames.is_none() && video_transcription(strategies).is_none() {
        return strategies
            .transcription
            .untranscribed(&att.filename, process_binary_attachment(workspace, att));
    }
    match spool_video(att) {
        Ok(tmp) => process_video_file(att, tmp.path(), strategies, workspace).await,
        Err(unreadable) => vec![unreadable],
    }
}

/// An inline video on disk — ffmpeg needs a seekable file, since most
/// containers keep their index at the end — or the block saying it could not
/// be put there.
fn spool_video(att: &SessionAttachment) -> Result<tempfile::NamedTempFile, ContentBlock> {
    decode_attachment_base64(att)
        .and_then(|bytes| {
            use std::io::Write;
            let mut tmp = tempfile::NamedTempFile::new()?;
            tmp.write_all(&bytes)?;
            Ok(tmp)
        })
        .map_err(|e| {
            tracing::warn!(file = %att.filename, error = %e, "video pipeline failed; substituting placeholder");
            ContentBlock::Text {
                text: format!(
                    "Video `{}` was attached but processing failed: {e}",
                    att.filename
                ),
            }
        })
}

/// An attachment the agent opened again: whole, as [`process_attachment`]
/// renders it, or — given a `selection` — the frames of a video it picks.
/// Those come without the transcript the whole video has, which is said when
/// there is one to be had. A selection of anything but a video is said to be
/// ignored.
pub async fn process_viewed_attachment(
    att: &SessionAttachment,
    selection: Option<&FrameSelection>,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
    staging: &SessionContext,
) -> Vec<ContentBlock> {
    let Some(selection) = selection else {
        return process_attachment(att, strategies, workspace, staging).await;
    };
    let note = |text: String| ContentBlock::Text { text };
    let is_video = infer_mime_from_extension(&att.filename, &att.mime_type)
        .unwrap_or(&att.mime_type)
        .starts_with("video/");
    if !is_video {
        let ignored = note(format!(
            "`{}` is not a video, so {} do not apply and it is shown whole.",
            att.filename,
            FrameSelection::PARAMETERS
        ));
        return std::iter::once(ignored)
            .chain(process_attachment(att, strategies, workspace, staging).await)
            .collect();
    }
    let said = video_transcription(strategies).map(|_| {
        note(format!(
            "A closer look shows frames only: for what is said in `{}`, view it again without \
             {}.",
            att.filename,
            FrameSelection::PARAMETERS
        ))
    });
    let Some(cfg) = &strategies.video_frames else {
        let off = note(format!(
            "Video `{}`: video frames are turned off for this agent ([attachments.video]), so \
             there is no closer look to take.",
            att.filename
        ));
        return std::iter::once(off).chain(said).collect();
    };
    let tmp = match spool_video(att) {
        Ok(tmp) => tmp,
        Err(unreadable) => return vec![unreadable],
    };
    let mut blocks = frames::render_frames(
        &att.filename,
        tmp.path(),
        cfg,
        selection,
        &strategies.vision,
    )
    .await;
    blocks.extend(said);
    blocks
}

/// A video on disk as model input: what it shows (frames), then what it says
/// (transcript) — or, without transcription, the file saved to the workspace
/// so tools can still work on it.
async fn process_video_file(
    att: &SessionAttachment,
    path: &Path,
    strategies: &MediaStrategies<'_>,
    workspace: &Path,
) -> Vec<ContentBlock> {
    let mut blocks = match &strategies.video_frames {
        Some(cfg) => {
            frames::render_frames(
                &att.filename,
                path,
                cfg,
                &FrameSelection::default(),
                &strategies.vision,
            )
            .await
        }
        None => Vec::new(),
    };
    match video_transcription(strategies) {
        Some((provider, cfg)) => {
            let transcript = video::process_video_file(&att.filename, path, provider, cfg)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(file = %att.filename, error = %e, "video transcription failed; substituting placeholder");
                    vec![ContentBlock::Text {
                        text: format!(
                            "Video `{}` was attached but transcription failed: {e}",
                            att.filename
                        ),
                    }]
                });
            blocks.extend(transcript);
        }
        None => blocks.extend(strategies.transcription.untranscribed(
            &att.filename,
            save_attachment_from_path(workspace, att, path).await,
        )),
    }
    blocks
}

/// The provider and config that transcribe this turn's videos, if any do.
fn video_transcription<'a>(
    strategies: &'a MediaStrategies<'_>,
) -> Option<(
    &'a dyn crate::transcription::TranscriptionProvider,
    &'a LlmTranscriptionConfig,
)> {
    match strategies.transcription {
        TranscriptionStrategy::TranscribeWith(provider, cfg) if cfg.transcribe_video_audio => {
            Some((provider, cfg))
        }
        _ => None,
    }
}

fn process_office_attachment(
    att: &SessionAttachment,
    staging: &SessionContext,
) -> Vec<ContentBlock> {
    let result = if is_spreadsheet_mime(&att.mime_type) {
        convert_spreadsheet_attachment(att, staging)
    } else {
        convert_docx_attachment(att).map(|text| {
            vec![ContentBlock::Text {
                text: format!("Content of `{}`:\n{}", att.filename, text),
            }]
        })
    };
    match result {
        Ok(blocks) => blocks,
        Err(e) => {
            tracing::warn!(file = %att.filename, "Failed to convert attachment: {e}");
            vec![ContentBlock::Text {
                text: format!(
                    "File `{}` was attached but could not be converted: {e}",
                    att.filename,
                ),
            }]
        }
    }
}

/// Cap on raw text inlined from one attachment. Past this the whole file is
/// written to the workspace and only a preview is inlined (see
/// [`save_text_attachment`]) — so an oversized upload cannot flood and evict
/// the conversation context. Tabular files over
/// [`ATTACHMENT_STAGE_ROW_THRESHOLD`] rows are staged queryably before this
/// (see [`try_stage_text_table`]); this bounds everything else.
///
/// 32K chars ≈ 8K tokens. The previous 100K (~25K tokens) was set as a
/// truncation backstop, but a single block that size is itself enough to push a
/// busy thread into the emergency-compaction path — which is how ENG-101's
/// 91KB YAML behaved. Spilling well below the compaction ceiling keeps the
/// COMPLETE file available on disk (immune to compaction) instead of inlining a
/// copy large enough to get itself truncated. Small configs still inline whole.
const MAX_INLINE_ATTACHMENT_CHARS: usize = 32_000;

/// How much of an oversized text attachment to inline as a preview. The full
/// file is written to the workspace instead (see [`save_text_attachment`]), so
/// this only has to be enough for the model to recognise the shape of the file
/// and decide how to read the rest. 100K chars of "preview" was itself a
/// context-flooding event.
const OVERSIZED_TEXT_PREVIEW_CHARS: usize = 8_000;

/// Pick the fence language for an attachment's code block from its filename.
///
/// Upload pipelines mangle names — the ENG-101 attachment reached Jira as
/// `violations_and_warnings.ASHFORDyml`, so the naive last-dot-segment yielded a
/// ```` ```ASHFORDyml ```` fence. Match a known language exactly, else by suffix
/// (which recovers `ASHFORDyml` → `yml`), else emit no language at all.
fn text_fence_language(filename: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "yaml", "yml", "json", "toml", "xml", "html", "css", "scss", "js", "jsx", "ts", "tsx",
        "rs", "py", "rb", "go", "java", "kt", "swift", "php", "sh", "bash", "zsh", "sql",
        "graphql", "md", "csv", "tsv", "ini", "env", "diff", "patch", "log", "txt",
    ];
    let Some(raw) = filename.rsplit('.').next() else {
        return "";
    };
    let lower = raw.to_ascii_lowercase();
    if let Some(hit) = KNOWN.iter().find(|k| **k == lower) {
        return hit;
    }
    // Suffix fallback, but only across a case/character boundary: `ASHFORDyml`
    // has an uppercase `S` before `yml` so the extension was clearly glued on,
    // whereas `changelog` and `rockstars` are single lowercase words that merely
    // happen to end in `log`/`rs`. Longest match wins (`.ASHFORDyaml` → `yaml`).
    KNOWN
        .iter()
        .filter(|k| {
            let Some(prefix) = raw
                .len()
                .checked_sub(k.len())
                .and_then(|split| raw.get(..split))
            else {
                return false;
            };
            lower.ends_with(**k)
                && !prefix.is_empty()
                && !prefix.ends_with(|c: char| c.is_ascii_lowercase())
        })
        .max_by_key(|k| k.len())
        .copied()
        .unwrap_or("")
}

/// Does this text payload look like base64 that a producer forgot to decode?
///
/// Deliberately strict — every condition must hold — because the cost of a
/// false positive (mangling a real text file) is worse than the cost of a miss.
/// Real text has line breaks, spaces or punctuation outside the base64
/// alphabet; base64 of a UTF-8 file has none of that and decodes cleanly.
fn looks_like_undecoded_base64(data: &str) -> bool {
    use base64::Engine;
    const MIN_LEN: usize = 512;
    if data.len() < MIN_LEN || !data.len().is_multiple_of(4) {
        return false;
    }
    if !data
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return false;
    }
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|decoded| {
            // Valid UTF-8 is not enough: 512 `A`s decode to NULs, which are
            // "valid UTF-8" and would turn a legitimate (if odd) text file into
            // a wall of control characters. Require the decode to look like
            // prose/markup before believing it.
            !decoded.is_empty()
                && decoded
                    .chars()
                    .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                    .count()
                    * 100
                    / decoded.chars().count()
                    < 5
        })
}

fn process_text_attachment(
    att: &SessionAttachment,
    workspace: &Path,
    staging: &SessionContext,
) -> ContentBlock {
    // Defence in depth for the `SessionAttachment::data` contract. A caller
    // that base64-encodes a text-like type lands here and would otherwise
    // inline the base64 verbatim, labelled as the file's contents — the model
    // then "reads" gibberish and cannot say why. Recover rather than poison
    // the context, and log it as the caller bug it is.
    let decoded;
    let att = if looks_like_undecoded_base64(&att.data) {
        use base64::Engine;
        tracing::warn!(
            file = %att.filename,
            mime = %att.mime_type,
            "Text attachment arrived base64-encoded — decoding; the producer should use SessionAttachment::from_bytes"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&att.data)
            .unwrap_or_default();
        decoded = SessionAttachment {
            data: String::from_utf8_lossy(&bytes).into_owned(),
            ..att.clone()
        };
        &decoded
    } else {
        att
    };

    if let Some(staged) = try_stage_text_table(att, staging) {
        return staged;
    }
    let ext = text_fence_language(&att.filename);
    if att.data.len() > MAX_INLINE_ATTACHMENT_CHARS {
        // Spill rather than flood. Inlining 100K chars costs ~25-33K tokens and
        // can single-handedly blow the context window — which then triggers an
        // emergency compaction that discards the turn's real work. Write the
        // whole file where the agent's file tools can reach it and inline only
        // a preview.
        let head: String = att
            .data
            .chars()
            .take(OVERSIZED_TEXT_PREVIEW_CHARS)
            .collect();
        let text = match save_text_attachment(workspace, att) {
            Ok(path) => format!(
                "Content of `{}` ({} bytes — too large to inline; the COMPLETE file is saved at {}, read it with your file tools). First {} chars:\n```{}\n{}\n```",
                att.filename, att.data.len(), path, OVERSIZED_TEXT_PREVIEW_CHARS, ext, head
            ),
            Err(e) => {
                tracing::warn!(file = %att.filename, error = %e, "Failed to save oversized text attachment");
                format!(
                    "Content of `{}` (TRUNCATED — first {} chars of {} bytes; the rest could not be saved to the workspace: {e}):\n```{}\n{}\n```",
                    att.filename, OVERSIZED_TEXT_PREVIEW_CHARS, att.data.len(), ext, head
                )
            }
        };
        return ContentBlock::Text { text };
    }
    ContentBlock::Text {
        text: format!(
            "Content of `{}`:\n```{}\n{}\n```",
            att.filename, ext, att.data
        ),
    }
}

/// Write an already-decoded text attachment to the workspace uploads dir and
/// return its path. Sibling of [`save_binary_attachment`], which base64-decodes
/// first; text attachments hold their content verbatim.
fn save_text_attachment(workspace: &Path, att: &SessionAttachment) -> Result<String> {
    let upload = Upload::at(workspace, &att.filename)?;
    upload.write(&mut att.data.as_bytes())?;
    tracing::info!(
        file = %upload.name,
        bytes = att.data.len(),
        "Saved oversized text attachment to workspace"
    );
    Ok(upload.shown())
}

/// Where an attachment lands: `{workspace}/uploads/{name}`. The agent uid can
/// rearrange that tree, and the hub does the writing, so it goes through
/// [`config_guard`] and never reaches the agent's configuration.
struct Upload {
    roots: CanonicalRoots,
    name: String,
    shown: std::path::PathBuf,
    real: std::path::PathBuf,
}

impl Upload {
    fn at(workspace: &Path, filename: &str) -> Result<Self> {
        let roots = CanonicalRoots::new(&[workspace.to_path_buf()]);
        let name = filename.replace(['/', '\\'], "_").replace("..", "_");
        let shown = workspace.join("uploads").join(&name);
        let real = roots
            .resolve(&shown)
            .ok_or_else(|| eyre::eyre!("{} leads outside the workspace", shown.display()))?;
        if let Some(dir) = real.parent() {
            config_guard::create_dirs(&roots, dir)?;
        }
        Ok(Self {
            roots,
            name,
            shown,
            real,
        })
    }

    fn write(&self, content: &mut dyn std::io::Read) -> Result<()> {
        Ok(config_guard::write_from(&self.roots, &self.real, content)?)
    }

    fn shown(&self) -> String {
        self.shown.to_string_lossy().to_string()
    }
}

fn process_binary_attachment(workspace: &Path, att: &SessionAttachment) -> ContentBlock {
    match save_binary_attachment(workspace, att) {
        Ok(path) => ContentBlock::Text {
            text: format!(
                "Binary file `{}` ({}) saved to: {}\nUse your file tools to read or process it.",
                att.filename, att.mime_type, path
            ),
        },
        Err(e) => {
            tracing::warn!(file = %att.filename, "Failed to save binary attachment: {e}");
            ContentBlock::Text {
                text: format!(
                    "Binary file `{}` ({}) was attached but could not be saved: {e}",
                    att.filename, att.mime_type
                ),
            }
        }
    }
}

fn base64_source(media_type: &str, data: &str) -> ImageSource {
    ImageSource {
        source_type: "base64".to_string(),
        media_type: media_type.to_string(),
        data: data.to_string(),
    }
}

/// Check if a MIME type is a spreadsheet format we can convert.
fn is_spreadsheet_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-excel"
            | "application/vnd.oasis.opendocument.spreadsheet"
    )
}

fn is_docx_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/msword"
    )
}

fn decode_attachment_base64(att: &SessionAttachment) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(&att.data)
        .map_err(|e| eyre::eyre!("base64 decode failed: {e}"))
}

/// Extract paragraph text from a docx (ZIP + XML) attachment.
fn convert_docx_attachment(att: &SessionAttachment) -> Result<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use std::io::Read;

    let bytes = decode_attachment_base64(att)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| eyre::eyre!("failed to open docx: {e}"))?;

    let mut xml_content = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|e| eyre::eyre!("no document.xml in docx: {e}"))?
        .read_to_string(&mut xml_content)
        .map_err(|e| eyre::eyre!("failed to read document.xml: {e}"))?;

    let mut reader = Reader::from_str(&xml_content);
    reader.trim_text(true);

    let mut output = String::new();
    let mut in_paragraph = false;
    let mut paragraph_text = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"p" {
                    in_paragraph = true;
                    paragraph_text.clear();
                } else if in_paragraph && local.as_ref() == b"tab" {
                    paragraph_text.push('\t');
                } else if in_paragraph && local.as_ref() == b"br" {
                    paragraph_text.push('\n');
                }
            }
            Ok(Event::Text(ref e)) if in_paragraph => {
                if let Ok(text) = e.unescape() {
                    paragraph_text.push_str(&text);
                }
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == b"p" => {
                if !paragraph_text.is_empty() {
                    output.push_str(&paragraph_text);
                }
                output.push('\n');
                in_paragraph = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::warn!("XML parse error in docx: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    let trimmed = output.trim().to_string();
    if trimmed.is_empty() {
        return Err(eyre::eyre!("document contains no text"));
    }
    Ok(trimmed)
}

/// How many data rows an uploaded table may inline before it stages as a
/// queryable artifact instead — the level where the old markdown rendering
/// used to truncate (and lose) the remainder.
const ATTACHMENT_STAGE_ROW_THRESHOLD: usize = 500;
const ATTACHMENT_PRODUCER: &str = "attachment.upload";

/// Convert a spreadsheet attachment: small sheets render as markdown tables
/// (legacy behavior); sheets past the row threshold stage as queryable
/// artifacts instead of truncating.
fn convert_spreadsheet_attachment(
    att: &SessionAttachment,
    staging: &SessionContext,
) -> Result<Vec<ContentBlock>> {
    let bytes = decode_attachment_base64(att)?;
    let mut workbook = crate::spreadsheet::Workbook::open(&bytes)?;

    let sheet_names = workbook.sheet_names();
    // Only worksheets carry cells — reading a chart/macro sheet always fails,
    // and that must not surface as a "could not be read" warning for a
    // healthy file.
    let worksheet_names: Vec<String> = workbook
        .sheets_metadata()
        .iter()
        .filter(|s| matches!(s.typ, calamine::SheetType::WorkSheet))
        .map(|s| s.name.clone())
        .collect();
    let mut inline = String::new();
    let mut staged_blocks = Vec::new();

    for name in &worksheet_names {
        let grid = match workbook.read_grid(name, sheet_cell_to_value) {
            Ok(g) => g,
            Err(e) => {
                // A visible note, not a silent skip — dropping a sheet
                // unannounced reads as "converted everything" when it didn't.
                inline.push_str(&format!("\n*Sheet \"{name}\" could not be read: {e}*\n"));
                continue;
            }
        };
        let (height, width) = (grid.height(), grid.width());
        if height == 0 || width == 0 {
            continue;
        }
        let rows = grid.rows;

        if rows.len().saturating_sub(1) > ATTACHMENT_STAGE_ROW_THRESHOLD {
            let columns = crate::artifacts::normalize_columns(rows[0].iter().map(cell_display));
            let objects = crate::artifacts::zip_table(&columns, rows[1..].to_vec());
            if let Some(block) = stage_attachment_table(
                staging,
                &att.filename,
                &format!("Sheet \"{name}\""),
                columns,
                objects,
            ) {
                staged_blocks.push(block);
                continue;
            }
        }
        render_sheet_markdown(&mut inline, &sheet_names, name, height, width, &rows);
    }

    let mut blocks = Vec::new();
    if !inline.is_empty() {
        blocks.push(ContentBlock::Text {
            text: format!("Content of `{}`:\n{inline}", att.filename),
        });
    }
    blocks.extend(staged_blocks);
    if blocks.is_empty() {
        return Err(eyre::eyre!("spreadsheet contains no data"));
    }
    Ok(blocks)
}

fn render_sheet_markdown(
    output: &mut String,
    sheet_names: &[String],
    name: &str,
    height: usize,
    width: usize,
    rows: &[Vec<serde_json::Value>],
) {
    let max_rows = ATTACHMENT_STAGE_ROW_THRESHOLD;
    if sheet_names.len() > 1 {
        output.push_str(&format!(
            "## Sheet: \"{}\" ({height} rows × {width} columns)\n\n",
            name
        ));
    }
    if let Some(header) = rows.first() {
        output.push_str("| ");
        output.push_str(
            &header
                .iter()
                .map(cell_display)
                .collect::<Vec<_>>()
                .join(" | "),
        );
        output.push_str(" |\n|");
        for _ in header {
            output.push_str("---|");
        }
        output.push('\n');
    }
    let data_rows = &rows[1..];
    let display_count = data_rows.len().min(max_rows);
    for row in &data_rows[..display_count] {
        output.push_str("| ");
        output.push_str(&row.iter().map(cell_display).collect::<Vec<_>>().join(" | "));
        output.push_str(" |\n");
    }
    let remaining = data_rows.len().saturating_sub(max_rows);
    if remaining > 0 {
        output.push_str(&format!("\n*[... {remaining} more rows]*\n"));
    }
    output.push('\n');
}

/// Typed cell conversion: numbers stay numbers (integral floats collapse to
/// ints, matching the legacy markdown rendering), so staged sheets are
/// SUM/ORDER BY-able in data.query without casts.
fn sheet_cell_to_value(cell: &calamine::DataRef<'_>) -> serde_json::Value {
    use calamine::DataRef;
    match cell {
        DataRef::Empty => serde_json::Value::Null,
        DataRef::String(s) => serde_json::Value::String(s.clone()),
        DataRef::SharedString(s) => serde_json::Value::String((*s).to_string()),
        DataRef::Int(i) => serde_json::Value::from(*i),
        DataRef::Float(f) if *f == (*f as i64) as f64 => serde_json::Value::from(*f as i64),
        DataRef::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        DataRef::Bool(b) => serde_json::Value::Bool(*b),
        DataRef::DateTime(_)
        | DataRef::DateTimeIso(_)
        | DataRef::DurationIso(_)
        | DataRef::Error(_) => serde_json::Value::String(crate::spreadsheet::display_cell(cell)),
    }
}

fn cell_display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Stage a parsed tabular attachment under the EXPLICIT session context —
/// attachment processing runs before the turn's task-local scopes are
/// entered, so ambient resolution would silently never fire. `None` means
/// "fall back to the legacy inline rendering".
fn stage_attachment_table(
    staging: &SessionContext,
    filename: &str,
    label: &str,
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
) -> Option<ContentBlock> {
    use crate::artifacts::{global_store, stage_table_with_ttl, StagedTable, ATTACHMENT_TTL};

    let agent_id = staging.agent_id.clone().filter(|a| !a.is_empty())?;
    let store = global_store()?;
    let row_count = rows.len();
    // Inline threshold 0: the row threshold already decided this table is
    // too big for context — the byte gate must not give small-but-long
    // sheets a second vote back into the truncation dead-end. ATTACHMENT_TTL
    // (not the 1h default) so a long conversation can re-read the file.
    let staged = staging.clone().sync_scope(|| {
        stage_table_with_ttl(
            store.as_ref(),
            &agent_id,
            ATTACHMENT_PRODUCER,
            Some(columns),
            rows,
            0,
            ATTACHMENT_TTL,
        )
    });
    match staged {
        Ok(StagedTable::Staged { mut envelope, .. }) => {
            crate::artifacts::tabular::wrap_envelope_preview(&mut envelope, ATTACHMENT_PRODUCER);
            Some(ContentBlock::Text {
                text: format!(
                    "{label} of `{filename}` was staged as a queryable dataset ({row_count} rows; \
                     nothing was truncated). Filter/join/aggregate it with data.query, inspect \
                     its shape with data.inspect, or export with export_xlsx — do not ask the \
                     user to re-upload or paste rows. If much later the handle reports not found, \
                     reload the file with view_attachment(reference=\"{filename}\") — it re-fetches \
                     from the conversation and re-stages it as queryable.\n{envelope}"
                ),
            })
        }
        Ok(StagedTable::Inline { .. }) => None,
        Err(e) => {
            tracing::warn!(file = %filename, error = %e, "failed to stage attachment table; inlining");
            None
        }
    }
}

/// CSV / JSON-array attachments past the row threshold stage instead of
/// inlining; everything else returns `None` for the legacy text path.
fn try_stage_text_table(att: &SessionAttachment, staging: &SessionContext) -> Option<ContentBlock> {
    let lower_name = att.filename.to_ascii_lowercase();
    if att.mime_type == "text/csv" || lower_name.ends_with(".csv") {
        let (headers, rows) = parse_csv_typed(&att.data)?;
        if rows.len() <= ATTACHMENT_STAGE_ROW_THRESHOLD {
            return None;
        }
        let columns = crate::artifacts::normalize_columns(headers);
        let objects = crate::artifacts::zip_table(&columns, rows);
        return stage_attachment_table(staging, &att.filename, "CSV", columns, objects);
    }
    if att.mime_type.contains("json") || lower_name.ends_with(".json") {
        let serde_json::Value::Array(rows) = serde_json::from_str(&att.data).ok()? else {
            return None;
        };
        if rows.len() <= ATTACHMENT_STAGE_ROW_THRESHOLD
            || !rows.iter().all(serde_json::Value::is_object)
        {
            return None;
        }
        let columns = crate::artifacts::derive_schema(&rows);
        return stage_attachment_table(staging, &att.filename, "JSON array", columns, rows);
    }
    None
}

fn parse_csv_typed(data: &str) -> Option<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(data.as_bytes());
    let headers: Vec<String> = reader.headers().ok()?.iter().map(str::to_string).collect();
    if headers.is_empty() {
        return None;
    }
    let rows = reader
        .records()
        .filter_map(|record| record.ok())
        .map(|record| record.iter().map(csv_scalar).collect())
        .collect();
    Some((headers, rows))
}

/// Conservative scalar inference: only values whose canonical rendering
/// round-trips become numbers (so `007` and `1.50` stay strings).
fn csv_scalar(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        if i.to_string() == trimmed {
            return serde_json::Value::from(i);
        }
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        if f.is_finite() && f.to_string() == trimmed {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return serde_json::Value::Number(n);
            }
        }
    }
    serde_json::Value::String(raw.to_string())
}

fn save_binary_attachment(workspace: &Path, att: &SessionAttachment) -> Result<String> {
    let bytes = decode_attachment_base64(att)?;
    let upload = Upload::at(workspace, &att.filename)?;
    upload.write(&mut bytes.as_slice())?;
    tracing::info!(
        file = %upload.name,
        bytes = bytes.len(),
        "Saved binary attachment to workspace"
    );
    Ok(upload.shown())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    struct FakeVisionLlm;

    #[async_trait::async_trait]
    impl crate::vision::VisionLlm for FakeVisionLlm {
        async fn describe_image(
            &self,
            _media_type: &str,
            _data: &str,
            _hint: Option<&str>,
        ) -> eyre::Result<String> {
            Ok("a red rectangle with white text".to_string())
        }
        fn model_name(&self) -> &str {
            "fake-vision"
        }
    }
    struct FakeTranscriber;

    #[async_trait::async_trait]
    impl crate::transcription::TranscriptionProvider for FakeTranscriber {
        async fn transcribe(
            &self,
            _input: &crate::transcription::TranscriptionInput<'_>,
        ) -> eyre::Result<crate::transcription::TranscriptionOutput> {
            Ok(crate::transcription::TranscriptionOutput {
                text: "the quick brown fox jumps over the lazy dog".to_string(),
                duration_seconds: Some(3.2),
                language: Some("en".to_string()),
            })
        }
        fn model_name(&self) -> &str {
            "fake-whisper"
        }
    }
    /// A member's upload lands where the hub saves it, never through a link
    /// the agent uid left in `uploads/` onto the agent's configuration.
    #[cfg(unix)]
    #[test]
    fn an_upload_does_not_follow_a_planted_link_into_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("uploads")).unwrap();
        std::os::unix::fs::symlink("../config.toml", workspace.join("uploads/settings.toml"))
            .unwrap();
        let upload = SessionAttachment {
            filename: "settings.toml".to_string(),
            mime_type: "text/plain".to_string(),
            data: "[access]\nmode = \"open\"\n".to_string(),
            artifact_id: None,
        };

        assert!(save_text_attachment(&workspace, &upload).is_err());
        assert!(!workspace.join("config.toml").exists());

        let report = SessionAttachment {
            filename: "report.csv".to_string(),
            ..upload
        };
        let saved = save_text_attachment(&workspace, &report).unwrap();
        assert!(std::fs::read_to_string(saved).unwrap().contains("open"));
    }

    fn image_attachment() -> SessionAttachment {
        SessionAttachment {
            filename: "screenshot.png".to_string(),
            mime_type: "image/png".to_string(),
            data: "AAAA".to_string(),
            artifact_id: None,
        }
    }
    fn audio_attachment() -> SessionAttachment {
        SessionAttachment {
            filename: "voice-memo.m4a".to_string(),
            mime_type: "audio/mp4".to_string(),
            data: "AAAA".to_string(),
            artifact_id: None,
        }
    }
    fn install_test_store() -> std::sync::Arc<dyn crate::artifacts::ArtifactStore> {
        use std::sync::LazyLock;
        static DIR: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
            let dir = tempfile::TempDir::new().expect("tempdir");
            crate::artifacts::set_global_store(
                crate::artifacts::FsArtifactStore::new(dir.path().to_path_buf()).expect("store"),
            );
            dir
        });
        LazyLock::force(&DIR);
        crate::artifacts::global_store().expect("store installed")
    }
    fn staging_ctx(agent: &str, session: &str) -> SessionContext {
        SessionContext {
            agent_id: Some(agent.to_string()),
            id: Some(session.to_string()),
            user_id: Some("u-test".to_string()),
            ..Default::default()
        }
    }
    fn xlsx_attachment(rows: usize) -> SessionAttachment {
        use crate::export::{build_xlsx, Cell, ColFormat, Column, Sheet, Table};
        use base64::Engine;
        let table = Table {
            columns: vec![
                Column::new("id", "id", ColFormat::General),
                Column::new("status", "status", ColFormat::General),
            ],
            rows: (0..rows)
                .map(|i| {
                    vec![
                        Cell::Number(i as f64),
                        Cell::Text(if i % 3 == 0 { "Unfilled" } else { "Filled" }.into()),
                    ]
                })
                .collect(),
        };
        let bytes = build_xlsx(&[Sheet::new("Data", table)]).unwrap();
        SessionAttachment {
            filename: "upload.xlsx".to_string(),
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            artifact_id: None,
        }
    }
    fn first_block_text(blocks: &[ContentBlock]) -> &str {
        match &blocks[0] {
            ContentBlock::Text { text } => text,
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn big_spreadsheet_upload_stages_as_queryable_artifact() {
        use crate::artifacts::{ArtifactBody, ArtifactId};

        let store = install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session");
        let att = xlsx_attachment(800);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("tabular_artifact"),
            "staged envelope present: {text}"
        );
        assert!(text.contains("data.query"));
        assert!(!text.contains("more rows]*"), "no truncation marker");

        let id_start = text.find("art_").expect("artifact id in envelope");
        let id = ArtifactId::parse(&text[id_start..id_start + 36]).unwrap();
        let descriptor = store.metadata("upload-agent", &id).unwrap().unwrap();
        assert_eq!(descriptor.session_id.as_deref(), Some("upload-session"));
        assert_eq!(descriptor.producer, "attachment.upload");
        assert_eq!(descriptor.row_count, Some(800));
        let ArtifactBody::Json(serde_json::Value::Array(stored)) =
            store.read("upload-agent", &id).unwrap()
        else {
            panic!("expected rows");
        };
        assert_eq!(stored.len(), 800);
        assert_eq!(stored[0]["id"], 0, "typed cells: numbers stay numbers");
        assert_eq!(stored[0]["status"], "Unfilled");
    }
    #[tokio::test]
    async fn narrow_single_column_sheet_over_threshold_still_stages() {
        use crate::export::{build_xlsx, Cell, ColFormat, Column, Sheet, Table};
        use base64::Engine;

        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-narrow");
        let table = Table {
            columns: vec![Column::new("id", "id", ColFormat::General)],
            rows: (0..600).map(|i| vec![Cell::Number(i as f64)]).collect(),
        };
        let bytes = build_xlsx(&[Sheet::new("Ids", table)]).unwrap();
        let att = SessionAttachment {
            filename: "ids.xlsx".to_string(),
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("tabular_artifact"),
            "narrow sheets past the row threshold must stage (serialized bytes are under \
             the old 8KB gate — the regression this test pins): {text}"
        );
        assert!(!text.contains("more rows]*"));
    }
    #[tokio::test]
    async fn small_spreadsheet_upload_stays_inline_markdown() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session");
        let att = xlsx_attachment(5);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("Content of `upload.xlsx`"));
        assert!(text.contains("| id | status |"));
        assert!(!text.contains("tabular_artifact"));
    }
    #[tokio::test]
    async fn stray_far_cell_spreadsheet_reports_a_note_instead_of_exploding() {
        // Two cells implying a multi-billion-cell bounding box: the dense
        // range path would OOM the process; the bounded reader must surface
        // a per-sheet note.
        use base64::Engine;
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-stray");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.write_string(0, 0, "a").unwrap();
        ws.write_string(1_000_000, 3_000, "stray").unwrap();
        let att = SessionAttachment {
            filename: "stray.xlsx".to_string(),
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(wb.save_to_buffer().unwrap()),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("could not be read"),
            "unreadable sheet is reported, not silently dropped: {text}"
        );
    }
    #[tokio::test]
    async fn chartsheet_tab_does_not_trigger_a_false_failure_note() {
        use base64::Engine;
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-chart");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Data").unwrap();
        ws.write_string(0, 0, "id").unwrap();
        ws.write_number(1, 0, 1.0).unwrap();
        let mut chart = rust_xlsxwriter::Chart::new(rust_xlsxwriter::ChartType::Column);
        chart.add_series().set_values("Data!$A$2:$A$2");
        let cs = wb.add_chartsheet();
        cs.set_name("Chart1").unwrap();
        cs.insert_chart(0, 0, &chart).unwrap();
        let att = SessionAttachment {
            filename: "report.xlsx".to_string(),
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(wb.save_to_buffer().unwrap()),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("| id |"), "data sheet converted: {text}");
        assert!(
            !text.contains("could not be read"),
            "a chart tab is not a failure: {text}"
        );
    }
    #[tokio::test]
    async fn legacy_xls_attachment_errors_with_a_resave_hint() {
        use base64::Engine;
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-legacy");
        let att = SessionAttachment {
            filename: "old.xls".to_string(),
            mime_type: "application/vnd.ms-excel".to_string(),
            // CFB magic — a legacy .xls container the bounded reader refuses.
            data: base64::engine::general_purpose::STANDARD
                .encode(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1rest"),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("could not be converted"), "{text}");
        assert!(
            text.contains("xlsx"),
            "the error tells the user to re-save as xlsx: {text}"
        );
    }
    #[tokio::test]
    async fn oversized_text_attachment_is_spilled_not_flooded() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-big-text");
        let workspace = tempfile::tempdir().expect("tempdir");
        // A large non-tabular text blob must not flood context: only a bounded
        // preview is inlined and the whole file lands in the workspace.
        let body = "x".repeat(MAX_INLINE_ATTACHMENT_CHARS + 50_000);
        let att = SessionAttachment {
            filename: "blob.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: body.clone(),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            workspace.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.len() < OVERSIZED_TEXT_PREVIEW_CHARS + 2_000,
            "inlined preview is bounded, not the full blob (was {})",
            text.len()
        );
        let saved = workspace.path().join("uploads").join("blob.txt");
        assert!(text.contains(&saved.to_string_lossy().to_string()));
        assert_eq!(
            std::fs::read_to_string(&saved).expect("saved file"),
            body,
            "the COMPLETE file is on disk, so the agent can still read the rest"
        );
    }
    /// The ENG-101 regression. A `text/plain` YAML staged through the
    /// artifact-store sink (`jira.get_attachment` / `view_attachment`) must
    /// reach the model as readable text. It previously arrived base64-encoded
    /// and truncated, so the agent "read" 100,000 chars of gibberish. The test
    /// deliberately builds the attachment the way a producer must — via
    /// `from_bytes` — because hand-writing plain text into `data` is exactly
    /// what let the old tests pass while production was broken.
    #[tokio::test]
    async fn staged_text_attachment_reaches_model_as_text_not_base64() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-yaml");
        let yaml = "---\nviolations_and_warnings:\n  - id: 1\n    name: Oakridge\n";
        let att = SessionAttachment::from_bytes(
            "violations_and_warnings (6807d122).ASHFORDyml",
            "text/plain",
            yaml.as_bytes(),
        );
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("violations_and_warnings:") && text.contains("name: Oakridge"),
            "YAML must arrive readable, got: {text}"
        );
        assert!(
            !text.contains(&base64::engine::general_purpose::STANDARD.encode(yaml)),
            "must not inline base64"
        );
        // The mangled extension still yields a sane fence language.
        assert!(text.contains("```yml"), "fence language recovered: {text}");
    }
    /// Defence in depth: even if some future producer hands us base64 for a
    /// text MIME, we decode rather than inlining gibberish.
    #[tokio::test]
    async fn base64_text_attachment_is_recovered_not_inlined() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-b64");
        let yaml = format!("---\nkey: value\n{}\n", "a: 1\n".repeat(200));
        let att = SessionAttachment {
            filename: "config.yml".to_string(),
            mime_type: "text/plain".to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&yaml),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("key: value"), "base64 decoded, got: {text}");
    }
    #[test]
    fn base64_sniffer_does_not_mangle_real_text() {
        // Real files have newlines/spaces/punctuation outside the alphabet.
        assert!(!looks_like_undecoded_base64(&"a: 1\n".repeat(500)));
        assert!(!looks_like_undecoded_base64(&"word ".repeat(500)));
        // A long alphanumeric run that is not valid base64 is left alone.
        assert!(!looks_like_undecoded_base64(&"z".repeat(1024)));
        // Decodes to valid-UTF-8 NULs — must NOT be treated as base64 text.
        assert!(!looks_like_undecoded_base64(&"A".repeat(1024)));
        // Actual base64 of UTF-8 text is caught.
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello world\n".repeat(100));
        assert!(looks_like_undecoded_base64(&encoded));
    }
    #[test]
    fn fence_language_recovers_from_mangled_extensions() {
        assert_eq!(text_fence_language("a.yml"), "yml");
        assert_eq!(text_fence_language("a.YAML"), "yaml");
        // The real ENG-101 filename: the dot before `yml` was lost.
        assert_eq!(text_fence_language("violations.ASHFORDyml"), "yml");
        assert_eq!(text_fence_language("notes.txt"), "txt");
        assert_eq!(text_fence_language("mystery.qqq"), "");
        assert_eq!(text_fence_language("noextension"), "");
        // Single lowercase words that merely end in a known token are not
        // extensions: `changelog` is not `log`, `rockstars` is not `rs`.
        assert_eq!(text_fence_language("changelog"), "");
        assert_eq!(text_fence_language("data.rockstars"), "");
    }
    #[tokio::test]
    async fn small_text_attachment_inlines_whole() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-small-text");
        let att = SessionAttachment {
            filename: "notes.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: "hello world".to_string(),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("hello world"));
        assert!(!text.contains("TRUNCATED"));
    }
    #[tokio::test]
    async fn big_spreadsheet_without_staging_context_falls_back_to_truncation() {
        install_test_store();
        let att = xlsx_attachment(800);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &SessionContext::default(),
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("more rows]*"), "legacy truncation: {text}");
        assert!(!text.contains("tabular_artifact"));
    }
    #[tokio::test]
    async fn big_csv_upload_stages_with_scalar_inference() {
        use crate::artifacts::{ArtifactBody, ArtifactId};

        let store = install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-csv");
        let mut body = String::from(
            "id,amount,code
",
        );
        for i in 0..700 {
            body.push_str(&format!("{i},{}.5,\"00{i}\"\n", i * 2));
        }
        let att = SessionAttachment {
            filename: "data.csv".to_string(),
            mime_type: "text/csv".to_string(),
            data: body,
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("tabular_artifact"), "{text}");

        let id_start = text.find("art_").unwrap();
        let id = ArtifactId::parse(&text[id_start..id_start + 36]).unwrap();
        let ArtifactBody::Json(serde_json::Value::Array(stored)) =
            store.read("upload-agent", &id).unwrap()
        else {
            panic!("expected rows");
        };
        assert_eq!(stored.len(), 700);
        assert_eq!(stored[1]["id"], 1, "ints inferred");
        assert_eq!(stored[1]["amount"], 2.5, "floats inferred");
        assert_eq!(
            stored[1]["code"], "001",
            "leading zeros preserved as strings"
        );
    }
    #[tokio::test]
    async fn big_json_array_upload_stages() {
        install_test_store();
        let staging = staging_ctx("upload-agent", "upload-session-json");
        let rows: Vec<serde_json::Value> = (0..600)
            .map(|i| serde_json::json!({"id": i, "name": format!("n{i}")}))
            .collect();
        let att = SessionAttachment {
            filename: "items.json".to_string(),
            mime_type: "application/json".to_string(),
            data: serde_json::to_string(&rows).unwrap(),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            Path::new("/tmp"),
            &staging,
        )
        .await;
        assert!(first_block_text(&blocks).contains("tabular_artifact"));
    }
    #[test]
    fn csv_scalar_inference_is_round_trip_conservative() {
        assert_eq!(csv_scalar("42"), serde_json::json!(42));
        assert_eq!(csv_scalar("2.5"), serde_json::json!(2.5));
        assert_eq!(csv_scalar("007"), serde_json::json!("007"));
        assert_eq!(csv_scalar("1.50"), serde_json::json!("1.50"));
        assert_eq!(csv_scalar(""), serde_json::Value::Null);
        assert_eq!(csv_scalar("hello"), serde_json::json!("hello"));
    }
    /// The injected CURRENT SOURCE block is SYSTEM-produced (read from the
    /// repository at HEAD). For a paired sender it must arrive as its own
    /// unwrapped content block while the sender's message is trust-wrapped —
    /// wrapping the two together labelled the system's own source as untrusted
    /// user content in the same breath its header called it authoritative.
    #[tokio::test]
    async fn injected_block_stays_outside_the_trust_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let block = "## CURRENT SOURCE CODE …\nfn enforce() {}";
        let blocks = build_task_content(
            None,
            Some(block),
            "where is it enforced?",
            &None,
            dir.path(),
            vision_only(VisionStrategy::Native),
            Some(workforce_common::trust::TrustLevel::Paired),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2, "source block + wrapped message");
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, block, "system source is verbatim, never wrapped");
            }
            other => panic!("expected text block, got {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("where is it enforced?"),
                    "message present: {text}"
                );
                assert_ne!(
                    text, "where is it enforced?",
                    "paired message must be trust-wrapped"
                );
                assert!(
                    !text.contains("CURRENT SOURCE CODE"),
                    "the wrap must not swallow the system block"
                );
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }
    /// The per-turn context block leads, then the source block, then the
    /// wrapped message; neither system block is swallowed by the trust wrap.
    #[tokio::test]
    async fn turn_context_leads_the_task_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = "[SYSTEM CONTEXT] clock and notes [END SYSTEM CONTEXT]";
        let code = "## CURRENT SOURCE CODE …\nfn enforce() {}";
        let blocks = build_task_content(
            Some(ctx),
            Some(code),
            "where is it enforced?",
            &None,
            dir.path(),
            vision_only(VisionStrategy::Native),
            Some(workforce_common::trust::TrustLevel::Paired),
            &SessionContext::default(),
        )
        .await;
        let texts: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.as_str(),
                other => panic!("expected text block, got {other:?}"),
            })
            .collect();
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0], ctx);
        assert_eq!(texts[1], code);
        assert!(texts[2].contains("where is it enforced?"));
        assert!(!texts[2].contains("SYSTEM CONTEXT"));
        assert!(!texts[2].contains("CURRENT SOURCE CODE"));
    }

    /// Without an injection the shape is unchanged: one wrapped message block.
    #[tokio::test]
    async fn no_injected_block_means_the_message_leads() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = build_task_content(
            None,
            None,
            "hello",
            &None,
            dir.path(),
            vision_only(VisionStrategy::Native),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected text block, got {other:?}"),
        }
    }
    fn vision_only(strategy: VisionStrategy<'_>) -> MediaStrategies<'_> {
        MediaStrategies {
            vision: strategy,
            transcription: TranscriptionStrategy::Disabled,
            video_frames: None,
        }
    }
    fn transcription_only<'a>(
        provider: &'a dyn crate::transcription::TranscriptionProvider,
        cfg: &'a LlmTranscriptionConfig,
    ) -> MediaStrategies<'a> {
        MediaStrategies {
            vision: VisionStrategy::Native,
            transcription: TranscriptionStrategy::TranscribeWith(provider, cfg),
            video_frames: None,
        }
    }
    /// Write an upload-shaped artifact the way `upload_session_file` /
    /// `download_file_staged` do, returning its id for a by-ref attachment.
    fn write_upload_artifact(
        store: &dyn crate::artifacts::ArtifactStore,
        agent: &str,
        session: &str,
        user: Option<&str>,
        filename: &str,
        mime: &str,
        bytes: &[u8],
        producer: &str,
    ) -> String {
        let mut descriptor = crate::artifacts::ArtifactDescriptor::from_context(
            producer,
            crate::artifacts::ContentType::OctetStream,
            agent.to_string(),
        )
        .with_file_meta(filename.to_string(), mime.to_string());
        descriptor.session_id = Some(session.to_string());
        descriptor.user_id = user.map(str::to_string);
        let handle = store
            .write(
                agent,
                descriptor,
                crate::artifacts::ArtifactBody::Bytes(bytes.to_vec()),
            )
            .unwrap();
        handle.id.as_str().to_string()
    }
    #[tokio::test]
    async fn by_ref_text_attachment_resolves_to_content() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "notes.txt",
            "text/plain",
            b"hello from the artifact store",
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("notes.txt", "text/plain", &id);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("hello from the artifact store"),
            "resolved content must reach the model: {text}"
        );
    }
    #[tokio::test]
    async fn by_ref_audio_routes_through_file_transcription() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        // Garbage bytes are fine: ffprobe can't read them so the pipeline
        // falls through to single-shot transcription, and the fake provider
        // ignores its input.
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "memo.mp3",
            "audio/mpeg",
            b"not really audio",
            "session.upload",
        );
        let provider = FakeTranscriber;
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..LlmTranscriptionConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("memo.mp3", "audio/mpeg", &id);
        let blocks = process_attachment(
            &att,
            &transcription_only(&provider, &cfg),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            text.contains("the quick brown fox"),
            "transcript must come back for by-ref audio: {text}"
        );
    }
    #[tokio::test]
    async fn by_ref_attachment_rejects_foreign_user() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("someone-else"),
            "secret.txt",
            "text/plain",
            b"cross-user payload",
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("secret.txt", "text/plain", &id);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(
            !text.contains("cross-user payload"),
            "foreign user's upload must not resolve: {text}"
        );
        assert!(text.contains("could not be loaded"), "got: {text}");
    }

    /// The producer an upload surface stamps must be one the by-reference
    /// resolver admits. These two lived apart — a bare string in the allowlist,
    /// another chosen by the caller — and the edge duly invented its own,
    /// so every customer upload came back to the model as "not an uploaded
    /// attachment" and the agent told people their file had expired.
    #[test]
    fn the_session_upload_producer_is_accepted_by_reference() {
        assert!(
            BYREF_ATTACHMENT_PRODUCERS.contains(&SESSION_UPLOAD_PRODUCER),
            "an upload stamped with the canonical producer must be readable by reference"
        );
    }

    #[tokio::test]
    async fn by_ref_attachment_rejects_non_upload_producer() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        // Same user, same session — but produced by a tool, not an upload. A
        // leaked artifact id in a transcript must not become a read primitive.
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "table.json",
            "text/plain",
            b"tool-produced rows",
            "data.query",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("table.json", "text/plain", &id);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(!text.contains("tool-produced rows"), "got: {text}");
        assert!(text.contains("could not be loaded"), "got: {text}");
    }
    #[tokio::test]
    async fn by_ref_ownerless_artifact_allowed_only_in_its_session() {
        let store = install_test_store();
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            None,
            "legacy.txt",
            "text/plain",
            b"ownerless upload",
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("legacy.txt", "text/plain", &id);

        let same_session = staging_ctx("byref-agent", "byref-session");
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &same_session,
        )
        .await;
        assert!(first_block_text(&blocks).contains("ownerless upload"));

        let other_session = staging_ctx("byref-agent", "other-session");
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &other_session,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(!text.contains("ownerless upload"), "got: {text}");
    }
    #[tokio::test]
    async fn by_ref_octet_stream_image_routes_with_the_type_its_bytes_show() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "photo.jpg",
            "application/octet-stream",
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR",
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("photo.jpg", "application/octet-stream", &id);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        match &blocks[0] {
            ContentBlock::Image { source } => assert_eq!(source.media_type, "image/png"),
            other => panic!("expected an image block, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn by_ref_binary_lands_in_workspace_uploads() {
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "bundle.zip",
            "application/zip",
            b"PK-fake-zip-bytes",
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("bundle.zip", "application/zip", &id);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("saved to"), "got: {text}");
        let saved = dir.path().join("uploads").join("bundle.zip");
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            b"PK-fake-zip-bytes",
            "workspace copy must be byte-identical"
        );
    }
    #[tokio::test]
    async fn by_ref_unknown_artifact_reports_expiry() {
        install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact(
            "gone.mp4",
            "video/mp4",
            "art_00000000000000000000000000000000",
        );
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &staging,
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("gone.mp4"));
        assert!(text.contains("could not be loaded"), "got: {text}");
    }

    /// A 4s 64×64 clip with a tone, or `None` without ffmpeg.
    fn tiny_video() -> Option<Vec<u8>> {
        if !crate::ffmpeg::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tiny.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "testsrc=duration=4:size=64x64:rate=10", "-f", "lavfi"])
            .args(["-i", "sine=duration=4", "-c:v", "libx264", "-pix_fmt"])
            .args(["yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&out)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success());
        Some(std::fs::read(out).unwrap())
    }

    fn with_frames(strategies: MediaStrategies<'_>) -> MediaStrategies<'_> {
        MediaStrategies {
            video_frames: Some(VideoFramesConfig::default()),
            ..strategies
        }
    }

    fn image_count(blocks: &[ContentBlock]) -> usize {
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count()
    }

    fn all_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn video_shows_its_frames_then_its_transcript() {
        let Some(bytes) = tiny_video() else { return };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("IMG_8026.MOV", "video/quicktime", &bytes);
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..Default::default()
        };
        let blocks = process_attachment(
            &att,
            &with_frames(transcription_only(&FakeTranscriber, &cfg)),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(image_count(&blocks), 2, "4s at 2s spacing");
        let ContentBlock::Text { text } = blocks.last().unwrap() else {
            panic!("transcript last");
        };
        assert!(text.contains("the quick brown fox"), "{text}");
        assert!(first_block_text(&blocks).contains("Video `IMG_8026.MOV`"));
    }

    #[tokio::test]
    async fn video_without_transcription_shows_frames_and_keeps_the_file() {
        let Some(bytes) = tiny_video() else { return };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "video/mp4", &bytes);
        let blocks = process_attachment(
            &att,
            &with_frames(vision_only(VisionStrategy::Native)),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(image_count(&blocks), 2);
        assert!(all_text(&blocks).contains("saved to"));
        assert_eq!(
            std::fs::read(dir.path().join("uploads").join("pan.mp4")).unwrap(),
            bytes
        );
    }

    fn closer_look() -> FrameSelection {
        FrameSelection {
            from: Some(1.0),
            to: Some(3.0),
            frames: Some(3),
            ..Default::default()
        }
    }

    fn as_json(blocks: &[ContentBlock]) -> serde_json::Value {
        serde_json::to_value(blocks).unwrap()
    }

    #[tokio::test]
    async fn a_video_viewed_without_a_selection_is_the_video_as_it_arrived() {
        let Some(bytes) = tiny_video() else { return };
        let att = SessionAttachment::from_bytes("IMG_8026.MOV", "video/quicktime", &bytes);
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..Default::default()
        };
        for strategies in [
            with_frames(transcription_only(&FakeTranscriber, &cfg)),
            with_frames(vision_only(VisionStrategy::Native)),
        ] {
            let (arrived, viewed) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let staging = SessionContext::default();
            let before = process_attachment(&att, &strategies, arrived.path(), &staging).await;
            let after =
                process_viewed_attachment(&att, None, &strategies, viewed.path(), &staging).await;
            let rooted = |blocks: &[ContentBlock], root: &Path| {
                as_json(blocks)
                    .to_string()
                    .replace(&root.display().to_string(), "<workspace>")
            };
            assert_eq!(
                rooted(&after, viewed.path()),
                rooted(&before, arrived.path())
            );
            assert_eq!(
                std::fs::read(viewed.path().join("uploads").join("IMG_8026.MOV")).ok(),
                std::fs::read(arrived.path().join("uploads").join("IMG_8026.MOV")).ok(),
                "the saved file"
            );
        }
    }

    #[tokio::test]
    async fn a_closer_look_shows_frames_and_says_where_the_transcript_is() {
        let Some(bytes) = tiny_video() else { return };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "application/octet-stream", &bytes);
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..Default::default()
        };
        let blocks = process_viewed_attachment(
            &att,
            Some(&closer_look()),
            &with_frames(transcription_only(&FakeTranscriber, &cfg)),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(image_count(&blocks), 3);
        let text = all_text(&blocks);
        assert!(text.contains("sampled from 0:01.0 to 0:03.0"), "{text}");
        assert!(!text.contains("the quick brown fox"), "{text}");
        assert!(
            text.ends_with(
                "A closer look shows frames only: for what is said in `pan.mp4`, view it again \
                 without `from`, `to`, `at`, `frames` and `detail`."
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_closer_look_with_frames_off_still_says_where_the_transcript_is() {
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "video/mp4", b"not decoded");
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..Default::default()
        };
        let blocks = process_viewed_attachment(
            &att,
            Some(&closer_look()),
            &transcription_only(&FakeTranscriber, &cfg),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        assert!(first_block_text(&blocks).contains("video frames are turned off"));
        assert!(all_text(&blocks).ends_with("without `from`, `to`, `at`, `frames` and `detail`."));
    }

    #[tokio::test]
    async fn a_closer_look_without_transcription_promises_no_transcript() {
        let Some(bytes) = tiny_video() else { return };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "video/mp4", &bytes);
        let blocks = process_viewed_attachment(
            &att,
            Some(&closer_look()),
            &with_frames(vision_only(VisionStrategy::Native)),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(image_count(&blocks), 3);
        let text = all_text(&blocks);
        assert!(!text.contains("what is said"), "{text}");
        assert!(!text.contains("saved to"), "{text}");
        assert!(
            !dir.path().join("uploads").exists(),
            "nothing is saved again"
        );
    }

    #[tokio::test]
    async fn a_closer_look_meets_the_same_vision_states_as_the_whole_video() {
        let Some(bytes) = tiny_video() else { return };
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "video/mp4", &bytes);
        let staging = SessionContext::default();
        for blind in [
            NoDescriber::Unconfigured,
            NoDescriber::Misconfigured("no key"),
        ] {
            let strategies = with_frames(vision_only(VisionStrategy::StripWithPlaceholder(blind)));
            let whole = process_attachment(&att, &strategies, dir.path(), &staging).await;
            let closer = process_viewed_attachment(
                &att,
                Some(&closer_look()),
                &strategies,
                dir.path(),
                &staging,
            )
            .await;
            assert_eq!(closer.len(), 1);
            assert_eq!(as_json(&closer)[0], as_json(&whole)[0]);
            assert!(first_block_text(&closer).contains("cannot process images"));
        }
    }

    #[tokio::test]
    async fn video_with_frames_off_and_no_transcription_is_only_saved() {
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes("pan.mp4", "video/mp4", b"opaque");
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 1);
        assert!(first_block_text(&blocks).contains("saved to"));
    }

    #[tokio::test]
    async fn undecodable_inline_video_says_processing_failed() {
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment {
            filename: "v.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            data: "not-base64!!!".to_string(),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &with_frames(vision_only(VisionStrategy::Native)),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 1);
        assert!(first_block_text(&blocks).contains("processing failed"));
    }

    #[tokio::test]
    async fn by_ref_video_renders_frames_before_the_file_moves_to_the_workspace() {
        let Some(bytes) = tiny_video() else { return };
        let store = install_test_store();
        let staging = staging_ctx("byref-agent", "byref-session");
        let id = write_upload_artifact(
            store.as_ref(),
            "byref-agent",
            "byref-session",
            Some("u-test"),
            "big.mp4",
            "video/mp4",
            &bytes,
            "session.upload",
        );
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_artifact("big.mp4", "video/mp4", &id);
        let blocks = process_attachment(
            &att,
            &with_frames(vision_only(VisionStrategy::Native)),
            dir.path(),
            &staging,
        )
        .await;
        assert_eq!(image_count(&blocks), 2, "{}", all_text(&blocks));
        assert_eq!(
            std::fs::read(dir.path().join("uploads").join("big.mp4")).unwrap(),
            bytes
        );
    }
    #[tokio::test]
    async fn build_task_content_native_passes_image_through() {
        let dir = tempfile::tempdir().unwrap();
        let atts = Some(vec![image_attachment()]);
        let blocks = build_task_content(
            None,
            None,
            "hi",
            &atts,
            dir.path(),
            vision_only(VisionStrategy::Native),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[1], ContentBlock::Image { .. }));
    }
    #[tokio::test]
    async fn build_task_content_describe_substitutes_text_block() {
        let dir = tempfile::tempdir().unwrap();
        let atts = Some(vec![image_attachment()]);
        let llm = FakeVisionLlm;
        let cfg = LlmVisionConfig {
            model: "fake".to_string(),
            ..LlmVisionConfig::default()
        };
        let blocks = build_task_content(
            None,
            None,
            "look at this",
            &atts,
            dir.path(),
            vision_only(VisionStrategy::DescribeWith(&llm, &cfg)),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("screenshot.png"));
                assert!(text.contains("a red rectangle with white text"));
                assert!(text.contains("fake-vision"));
            }
            _ => panic!("expected text block from vision describe"),
        }
    }
    #[tokio::test]
    async fn build_task_content_strip_emits_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let atts = Some(vec![image_attachment()]);
        let blocks = build_task_content(
            None,
            None,
            "hi",
            &atts,
            dir.path(),
            vision_only(VisionStrategy::StripWithPlaceholder(
                NoDescriber::Unconfigured,
            )),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("screenshot.png"));
                assert!(text.contains("cannot process images"));
            }
            _ => panic!("expected placeholder text block"),
        }
    }
    #[tokio::test]
    async fn build_task_content_transcribes_audio_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let atts = Some(vec![audio_attachment()]);
        let provider = FakeTranscriber;
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..LlmTranscriptionConfig::default()
        };
        let blocks = build_task_content(
            None,
            None,
            "what did they say",
            &atts,
            dir.path(),
            transcription_only(&provider, &cfg),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("voice-memo.m4a"));
                assert!(text.contains("the quick brown fox"));
                assert!(text.contains("fake-whisper"));
                assert!(text.contains("language en"));
            }
            _ => panic!("expected text block from transcription"),
        }
    }
    #[tokio::test]
    async fn build_task_content_audio_falls_through_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let atts = Some(vec![audio_attachment()]);
        let blocks = build_task_content(
            None,
            None,
            "hi",
            &atts,
            dir.path(),
            vision_only(VisionStrategy::Native),
            None,
            &SessionContext::default(),
        )
        .await;
        assert_eq!(blocks.len(), 2);
        // Falls through to the binary-attachment path; gets saved to workspace.
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("voice-memo.m4a"));
                assert!(text.contains("saved to:"));
            }
            _ => panic!("expected text block from binary fallback"),
        }
    }
    #[test]
    fn mime_inferred_only_for_generic_mimes() {
        assert_eq!(
            infer_mime_from_extension("PTT-20260722-WA0001.opus", "application/octet-stream"),
            Some("audio/ogg")
        );
        assert_eq!(infer_mime_from_extension("note.OGG", ""), Some("audio/ogg"));
        assert_eq!(
            infer_mime_from_extension("clip.mkv", "application/octet-stream"),
            Some("video/x-matroska")
        );
        assert_eq!(
            infer_mime_from_extension("song.mp3", "audio/mpeg"),
            None,
            "a real mime is left alone"
        );
        assert_eq!(
            infer_mime_from_extension("no-extension", "application/octet-stream"),
            None
        );
    }

    #[test]
    fn document_image_and_text_mimes_inferred_and_routable() {
        let cases = [
            (
                "BOS Integration_v1.0.DOCX",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "report.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            ("spec.pdf", "application/pdf"),
            ("photo.JPEG", "image/jpeg"),
            ("diagram.png", "image/png"),
            ("anim.gif", "image/gif"),
            ("pic.webp", "image/webp"),
            ("data.csv", "text/csv"),
            ("notes.txt", "text/plain"),
            ("README.md", "text/markdown"),
        ];
        for (filename, expected) in cases {
            let mime = infer_mime_from_extension(filename, "application/octet-stream");
            assert_eq!(mime, Some(expected), "{filename}");
            assert!(
                expected.starts_with("image/")
                    || expected == "application/pdf"
                    || is_docx_mime(expected)
                    || is_spreadsheet_mime(expected)
                    || is_text_like_mime(expected),
                "{expected} must reach a dedicated handler"
            );
        }
    }

    #[test]
    fn unconvertible_office_formats_keep_the_binary_fallback() {
        for filename in ["legacy.doc", "legacy.xls", "sheet.ods", "archive.zip"] {
            assert_eq!(
                infer_mime_from_extension(filename, "application/octet-stream"),
                None,
                "{filename}"
            );
        }
    }

    #[test]
    fn specific_mime_is_never_overridden() {
        for (filename, mime) in [
            ("spec.pdf", "application/x-custom"),
            ("data.csv", "application/vnd.ms-excel"),
            ("photo.png", "image/heic"),
        ] {
            assert_eq!(
                infer_mime_from_extension(filename, mime),
                None,
                "{filename}"
            );
        }
    }

    #[tokio::test]
    async fn octet_stream_image_routes_with_the_type_its_bytes_show() {
        let dir = tempfile::tempdir().unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let att = SessionAttachment::from_bytes("photo.jpg", "application/octet-stream", png);
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::Image { source } => assert_eq!(source.media_type, "image/png"),
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn octet_stream_files_whose_bytes_contradict_the_extension_stay_binary() {
        let dir = tempfile::tempdir().unwrap();
        for (filename, bytes) in [
            ("fake.pdf", b"PK\x03\x04 not a pdf".as_slice()),
            ("fake.png", b"%PDF-1.7 not an image".as_slice()),
        ] {
            let att = SessionAttachment::from_bytes(filename, "application/octet-stream", bytes);
            let blocks = process_attachment(
                &att,
                &vision_only(VisionStrategy::Native),
                dir.path(),
                &SessionContext::default(),
            )
            .await;
            let text = first_block_text(&blocks);
            assert!(text.contains("saved to"), "{filename}: {text}");
        }
    }

    #[test]
    fn by_reference_images_and_pdfs_keep_their_generic_mime() {
        for filename in ["spec.pdf", "photo.png"] {
            let att = SessionAttachment::from_artifact(filename, "application/octet-stream", "a1");
            let mime = infer_mime_from_extension(filename, &att.mime_type).unwrap();
            assert!(retype_attachment(&att, mime).is_none(), "{filename}");
        }
    }

    #[tokio::test]
    async fn octet_stream_text_attachment_is_inlined_as_readable_text() {
        let dir = tempfile::tempdir().unwrap();
        let att = SessionAttachment::from_bytes(
            "notes.txt",
            "application/octet-stream",
            b"quarterly figures attached",
        );
        let blocks = process_attachment(
            &att,
            &vision_only(VisionStrategy::Native),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("quarterly figures attached"), "{text}");
    }

    #[tokio::test]
    async fn octet_stream_opus_attachment_reaches_transcription_with_inferred_mime() {
        // WhatsApp voice notes: browsers report no mime for `.opus`, the UI
        // substitutes octet-stream. The file must still transcribe — and the
        // provider must see the corrected mime, not the generic one.
        struct CapturingTranscriber(std::sync::Mutex<Vec<(String, String)>>);
        #[async_trait::async_trait]
        impl crate::transcription::TranscriptionProvider for CapturingTranscriber {
            async fn transcribe(
                &self,
                input: &crate::transcription::TranscriptionInput<'_>,
            ) -> eyre::Result<crate::transcription::TranscriptionOutput> {
                self.0
                    .lock()
                    .unwrap()
                    .push((input.filename.to_string(), input.media_type.to_string()));
                Ok(crate::transcription::TranscriptionOutput {
                    text: "olá bom dia".to_string(),
                    duration_seconds: Some(2.0),
                    language: Some("pt".to_string()),
                })
            }
            fn model_name(&self) -> &str {
                "fake-whisper"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let provider = CapturingTranscriber(std::sync::Mutex::new(Vec::new()));
        let cfg = LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..LlmTranscriptionConfig::default()
        };
        let att = SessionAttachment {
            filename: "PTT-20260722-WA0001.opus".to_string(),
            mime_type: "application/octet-stream".to_string(),
            data: "AAAA".to_string(),
            artifact_id: None,
        };
        let blocks = process_attachment(
            &att,
            &transcription_only(&provider, &cfg),
            dir.path(),
            &SessionContext::default(),
        )
        .await;
        let text = first_block_text(&blocks);
        assert!(text.contains("PTT-20260722-WA0001.opus"));
        assert!(text.contains("olá bom dia"));
        let seen = provider.0.into_inner().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "PTT-20260722-WA0001.opus");
        assert_eq!(seen[0].1, "audio/ogg");
    }
    #[tokio::test]
    async fn build_task_content_wraps_live_paired_message() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = build_task_content(
            None,
            None,
            "live paired text",
            &None,
            dir.path(),
            vision_only(VisionStrategy::Native),
            Some(workforce_common::trust::TrustLevel::Paired),
            &SessionContext::default(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("PAIRED_CONTENT_"));
                assert!(text.contains("live paired text"));
            }
            _ => panic!("expected text block"),
        }
    }
    #[tokio::test]
    async fn build_task_content_leaves_owner_live_message_raw() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = build_task_content(
            None,
            None,
            "owner live text",
            &None,
            dir.path(),
            vision_only(VisionStrategy::Native),
            Some(workforce_common::trust::TrustLevel::Owner),
            &SessionContext::default(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(!text.contains("PAIRED_CONTENT_"));
                assert!(!text.contains("EXTERNAL_UNTRUSTED"));
                assert_eq!(text, "owner live text");
            }
            _ => panic!("expected text block"),
        }
    }

    fn strategy_on<'a>(
        describer: &'a Describer,
        model: &str,
        primary: crate::llm::provider::ProviderKind,
    ) -> VisionStrategy<'a> {
        let tiers = crate::llm::vision_tiers::VisionTierRegistry::from_defaults().unwrap();
        resolve_vision_strategy_for(&tiers, describer, model, primary)
    }

    #[tokio::test]
    async fn an_image_nobodys_key_could_describe_says_so_in_one_sentence() {
        struct Keyless;
        #[async_trait::async_trait]
        impl crate::vision::VisionLlm for Keyless {
            async fn describe_image(&self, _: &str, _: &str, _: Option<&str>) -> Result<String> {
                Err(NoKey("Image description is not available to you.".into()).into())
            }
            fn model_name(&self) -> &str {
                "keyless"
            }
        }
        let cfg: LlmVisionConfig =
            toml::from_str(r#"model = "openrouter:anthropic:cheap""#).unwrap();
        let block = process_image_attachment(
            &image_attachment(),
            &VisionStrategy::DescribeWith(&Keyless, &cfg),
        )
        .await;
        assert!(
            matches!(&block, ContentBlock::Text { text } if text ==
                "Image `screenshot.png` was attached but not described: Image description is \
                 not available to you."),
            "{block:?}"
        );
    }

    #[tokio::test]
    async fn claude_code_route_reads_images_natively_and_pdfs_as_text() {
        use crate::llm::provider::ProviderKind;
        let strategy = strategy_on(
            &Describer::Unconfigured,
            "claude-code:sonnet",
            ProviderKind::Anthropic,
        );
        assert!(matches!(strategy, VisionStrategy::NativeImages));
        assert!(matches!(
            strategy_on(&Describer::Unconfigured, "sonnet", ProviderKind::ClaudeCode),
            VisionStrategy::NativeImages
        ));
        assert!(matches!(
            process_image_attachment(&image_attachment(), &strategy).await,
            ContentBlock::Image { .. }
        ));
        let pdf = SessionAttachment {
            filename: "report.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            data: "AAAA".to_string(),
            artifact_id: None,
        };
        let blocks = process_pdf_attachment(&pdf, &strategy).await;
        assert!(
            matches!(blocks.as_slice(), [ContentBlock::Text { text }] if text.contains("report.pdf")),
            "a document reaches the CLI as text, never as a document block: {blocks:?}"
        );
    }

    #[tokio::test]
    async fn a_chatgpt_model_reads_images_and_pdfs_itself() {
        use crate::llm::provider::ProviderKind;
        let misconfigured = Describer::Misconfigured("no such route".into());
        for (model, primary) in [
            ("chatgpt:gpt-6.1-sol", ProviderKind::Anthropic),
            ("gpt-6.1-sol", ProviderKind::ChatGpt),
        ] {
            let strategy = strategy_on(&misconfigured, model, primary);
            assert!(
                matches!(strategy, VisionStrategy::Native),
                "{model} sees for itself, whatever the describer's state"
            );
            let pdf = SessionAttachment {
                filename: "report.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                data: "AAAA".to_string(),
                artifact_id: None,
            };
            assert!(
                matches!(
                    process_pdf_attachment(&pdf, &strategy).await.as_slice(),
                    [ContentBlock::Document { source }] if source.media_type == "application/pdf"
                ),
                "{model} is sent the PDF itself"
            );
        }
        assert!(
            matches!(
                strategy_on(
                    &Describer::Unconfigured,
                    "codex:gpt-6.1-sol",
                    ProviderKind::ChatGpt
                ),
                VisionStrategy::NativeImages
            ),
            "the same model on a CLI takes no documents"
        );

        struct NeverAsked;
        #[async_trait::async_trait]
        impl crate::vision::VisionLlm for NeverAsked {
            async fn describe_image(&self, _: &str, _: &str, _: Option<&str>) -> Result<String> {
                panic!("a model that sees for itself is never described to")
            }
            fn model_name(&self) -> &str {
                "keyed"
            }
        }
        let keyed = Describer::Ready(
            Arc::new(NeverAsked),
            toml::from_str(r#"model = "openrouter:anthropic:cheap""#).unwrap(),
        );
        let strategy = strategy_on(&keyed, "gpt-6.1-sol", ProviderKind::ChatGpt);
        assert!(
            matches!(
                process_image_attachment(&image_attachment(), &strategy).await,
                ContentBlock::Image { .. }
            ),
            "a keyed describer beside a subscription model is not asked, key or no key"
        );
    }

    #[test]
    fn a_listed_model_is_not_native_on_a_route_that_sends_no_images() {
        use crate::llm::provider::ProviderKind;
        let mut tiers = crate::llm::vision_tiers::VisionTierRegistry::from_defaults().unwrap();
        tiers
            .merge_toml("[ollama]\nstandard = \"qwen2.5vl:7b\"\n[zai]\nstandard = \"glm-4.6v\"")
            .unwrap();
        for (model, primary) in [
            ("qwen2.5vl:7b", ProviderKind::Ollama),
            ("ollama:qwen2.5vl:7b", ProviderKind::Anthropic),
            ("glm-4.6v", ProviderKind::Zai),
            ("zai:glm-4.6v", ProviderKind::Anthropic),
            ("grok:claude-haiku-4-5", ProviderKind::Anthropic),
            ("claude-haiku-4-5", ProviderKind::Cerebras),
        ] {
            assert!(
                matches!(
                    resolve_vision_strategy_for(&tiers, &Describer::Unconfigured, model, primary),
                    VisionStrategy::StripWithPlaceholder(NoDescriber::Unconfigured)
                ),
                "{model} on {primary:?} is never sent an image, so none is left for it to drop"
            );
        }
        for (model, primary) in [
            ("claude-haiku-4-5", ProviderKind::Anthropic),
            ("gemini:gemini-3.8-flash", ProviderKind::Ollama),
            ("openrouter:anthropic/claude-haiku-4.5", ProviderKind::Zai),
            ("Qwen/Qwen2.5-VL-7B-Instruct", ProviderKind::Vllm),
            (
                "eu.anthropic.claude-haiku-4-5-20251001-v1:0",
                ProviderKind::Bedrock,
            ),
        ] {
            assert!(
                matches!(
                    resolve_vision_strategy_for(&tiers, &Describer::Unconfigured, model, primary),
                    VisionStrategy::Native
                ),
                "{model} on {primary:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_misconfigured_describer_says_so_in_the_placeholder() {
        use crate::llm::provider::ProviderKind;
        let describer = Describer::Misconfigured("'nonesuch:gpt' names no known provider".into());
        let strategy = strategy_on(&describer, "grok:grok-4", ProviderKind::Anthropic);
        assert!(matches!(
            strategy,
            VisionStrategy::StripWithPlaceholder(NoDescriber::Misconfigured(_))
        ));
        let ContentBlock::Text { text } =
            process_image_attachment(&image_attachment(), &strategy).await
        else {
            panic!("a placeholder is text");
        };
        assert!(
            text.contains("vision is misconfigured: 'nonesuch:gpt' names no known provider"),
            "{text}"
        );

        assert!(matches!(
            strategy_on(&Describer::Unconfigured, "grok-4", ProviderKind::Grok),
            VisionStrategy::StripWithPlaceholder(NoDescriber::Unconfigured)
        ));
    }

    fn texts(blocks: &[ContentBlock]) -> Vec<&str> {
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.as_str(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_misconfigured_transcriber_saves_the_recording_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let transcriber = Transcriber::Misconfigured("'whisper-1' names no known provider".into());
        let video = SessionAttachment {
            filename: "clip.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            data: "AAAA".to_string(),
            artifact_id: None,
        };
        for att in [audio_attachment(), video] {
            let strategies = MediaStrategies {
                vision: VisionStrategy::Native,
                transcription: transcriber.strategy(),
                video_frames: None,
            };
            let blocks =
                process_attachment(&att, &strategies, dir.path(), &SessionContext::default()).await;
            let [saved, note] = texts(&blocks)[..] else {
                panic!("the saved file, then why it was not transcribed: {blocks:?}");
            };
            assert!(saved.contains("saved to:"), "{saved}");
            assert_eq!(
                note,
                format!(
                    "`{}` was not transcribed: transcription is misconfigured: 'whisper-1' names \
                     no known provider. Tell the user, so that an admin can fix it.",
                    att.filename
                )
            );

            let off = MediaStrategies {
                vision: VisionStrategy::Native,
                transcription: Transcriber::Unconfigured.strategy(),
                video_frames: None,
            };
            let blocks =
                process_attachment(&att, &off, dir.path(), &SessionContext::default()).await;
            assert!(
                matches!(texts(&blocks)[..], [saved] if saved.contains("saved to:")),
                "with no transcriber configured there is nothing to report: {blocks:?}"
            );
        }
    }
}

//! Files in conversations on the gateway's apps, Desk first: what agents
//! deliver into them and what people upload to them.
//!
//! - **Download**: `GET /api/agents/{id}/attachments/{artifact}` serves a
//!   file of a conversation the caller may see, by artifact id, with Range
//!   for seeking. The artifact must belong to a session; the caller must be
//!   allowed to read that session (its owner, or the hub's admin).
//! - **Upload**: `POST /api/agents/{id}/attachments?channel=desk` stages one
//!   file (multipart, or the raw body with `?filename=`) for the caller's
//!   conversation with that agent, its type sniffed from its bytes and its
//!   size capped by `[desk] attachment_max_mb`. The id it answers with goes
//!   on the person's next chat message as `attachments[].artifact_id`.
//! - **History**: in a Desk DM or room session (and with `[desk]` on), each
//!   delivered or uploaded file is kept by artifact in the desk store; when
//!   a turn ends its files go on its answer and the person's on the message
//!   that carried them (see [`bind_turn`]), and a DM's history reads them
//!   back (see [`with_attachments`]). Such a file is kept for
//!   `[desk] attachment_retention_days`.
//! - **On the message**: whether or not the desk is on, the files a live
//!   Desk turn delivered ride in its answer's Desk mark, and a file
//!   delivered outside a turn is a message of its own carrying it, so a
//!   Desk that missed them live drains them with the conversation.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{FromRequest, Multipart, Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use workforce_agent::artifacts::{
    global_store, ArtifactDescriptor, ArtifactId, ArtifactStore, ContentType, ATTACHMENT_TTL,
};
use workforce_agent::attachments::SESSION_UPLOAD_PRODUCER;
use workforce_common::channels::desk::DeskFile;
use workforce_common::channels::{is_conversational, is_desk_conversation, DESK_CHANNEL};
use workforce_common::session::{MessageRole, Session, SessionStore};
use workforce_gateway_client::protocol::Attachment;

use crate::gateway::{resolve_caller_context, GatewayState};
use crate::session_handlers::{lookup_owned_session, spool_to_disk, SessionAccess, SpoolError};

/// The producer a file an agent sends into a Desk conversation is staged as.
const DELIVERY_PRODUCER: &str = "desk.delivery";

/// What a conversation's files may be and how long they are kept.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The largest upload, in bytes.
    pub max_bytes: u64,
    /// How long a file in a Desk conversation is kept.
    pub retention: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self::from(&workforce_common::settings::DeskConfig::default())
    }
}

impl From<&workforce_common::settings::DeskConfig> for Limits {
    fn from(desk: &workforce_common::settings::DeskConfig) -> Self {
        Self {
            max_bytes: desk.attachment_max_mb.max(1).saturating_mul(1024 * 1024),
            retention: Duration::from_secs(
                desk.attachment_retention_days
                    .max(1)
                    .saturating_mul(24 * 3600),
            ),
        }
    }
}

static LIMITS: parking_lot::RwLock<Option<Limits>> = parking_lot::RwLock::new(None);

/// Takes the limits from `[desk]`; until it is called the defaults hold.
pub fn configure(desk: &workforce_common::settings::DeskConfig) {
    *LIMITS.write() = Some(Limits::from(desk));
}

pub(crate) fn limits() -> Limits {
    LIMITS.read().unwrap_or_default()
}

// ── What a file is ───────────────────────────────────────────────────────

/// Width and height of a PNG, GIF, JPEG or WebP from its first bytes.
pub(crate) fn image_size(head: &[u8]) -> Option<(u32, u32)> {
    let be16 = |b: &[u8], i: usize| -> Option<u32> {
        Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32)
    };
    let le16 = |b: &[u8], i: usize| -> Option<u32> {
        Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32)
    };
    let le24 = |b: &[u8], i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *b.get(i)?,
            *b.get(i + 1)?,
            *b.get(i + 2)?,
            0,
        ]))
    };
    let be32 = |b: &[u8], i: usize| -> Option<u32> {
        Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
    };
    let size = match head {
        [0x89, b'P', b'N', b'G', ..] if head.get(12..16) == Some(b"IHDR") => {
            (be32(head, 16)?, be32(head, 20)?)
        }
        [b'G', b'I', b'F', b'8', ..] => (le16(head, 6)?, le16(head, 8)?),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => {
            match head.get(12..16)? {
                b"VP8 " => (le16(head, 26)? & 0x3fff, le16(head, 28)? & 0x3fff),
                b"VP8L" => {
                    let bits = u32::from_le_bytes(head.get(21..25)?.try_into().ok()?);
                    ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
                }
                b"VP8X" => (le24(head, 24)? + 1, le24(head, 27)? + 1),
                _ => return None,
            }
        }
        [0xFF, 0xD8, ..] => {
            let mut i = 2;
            loop {
                while *head.get(i)? != 0xFF {
                    i += 1;
                }
                while *head.get(i)? == 0xFF {
                    i += 1;
                }
                let marker = *head.get(i)?;
                i += 1;
                if matches!(marker, 0xD8 | 0x01 | 0xD0..=0xD7) {
                    continue;
                }
                let length = be16(head, i)? as usize;
                if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    break (be16(head, i + 5)?, be16(head, i + 3)?);
                }
                if length < 2 {
                    return None;
                }
                i += length;
            }
        }
        _ => return None,
    };
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// Whether `head` reads as text: UTF-8 (a character cut off at its end
/// allowed) with no NUL.
fn looks_like_text(head: &[u8]) -> bool {
    if head.is_empty() || head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() - e.valid_up_to() < 4,
    }
}

/// The media type of an upload from its first bytes, its name only
/// refining what the bytes leave open (which zip, which text). What the
/// client said it was is never asked.
pub(crate) fn sniff_upload(head: &[u8], filename: &str) -> String {
    let named = mime_guess::from_path(filename)
        .first()
        .map(|m| m.essence_str().to_string());
    let named_as = |accept: &dyn Fn(&str) -> bool, otherwise: &str| -> String {
        named
            .clone()
            .filter(|n| accept(n))
            .unwrap_or_else(|| otherwise.to_string())
    };
    match workforce_skills::adapters::utils::sniff_mime(head) {
        Some("application/zip") => named_as(
            &|n| {
                n.starts_with("application/vnd.openxmlformats")
                    || n.starts_with("application/vnd.oasis")
                    || n == "application/epub+zip"
            },
            "application/zip",
        ),
        Some(sniffed) => sniffed.to_string(),
        None if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) => named_as(
            &|n| n.starts_with("application/vnd.ms-") || n == "application/msword",
            "application/octet-stream",
        ),
        None if looks_like_text(head) => named_as(
            &|n| {
                n.starts_with("text/")
                    || workforce_common::session::is_text_like_mime(n)
                    || n == "image/svg+xml"
            },
            "text/plain",
        ),
        None => "application/octet-stream".to_string(),
    }
}

/// What is cheap to know about a staged file.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Described {
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// How much of an image is read for its size: a JPEG's frame header may
/// sit behind a large EXIF block.
const IMAGE_HEAD: usize = 1024 * 1024;

impl Described {
    /// What `bytes` of `media_type` are: their size, and an image's pixels.
    fn of(bytes: &[u8], media_type: &str) -> Self {
        Self::sized(bytes.len() as u64, media_type, || {
            Some(bytes[..bytes.len().min(IMAGE_HEAD)].to_vec())
        })
    }

    /// `size` bytes of `media_type`, an image's pixels read from `head`.
    fn sized(size: u64, media_type: &str, head: impl FnOnce() -> Option<Vec<u8>>) -> Self {
        let (width, height) = media_type
            .starts_with("image/")
            .then(head)
            .flatten()
            .as_deref()
            .and_then(image_size)
            .map_or((None, None), |(w, h)| (Some(w), Some(h)));
        Self {
            size,
            width,
            height,
        }
    }
}

/// The size of the file `id` in `agent`'s store, and an image's pixels.
pub(crate) fn describe(
    store: &dyn ArtifactStore,
    agent: &str,
    id: &ArtifactId,
    media_type: &str,
) -> Described {
    let Some(path) = store.body_file_path(agent, id).ok().flatten() else {
        return Described::default();
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Described::sized(size, media_type, || read_head(&path, IMAGE_HEAD))
}

fn read_head(path: &Path, max: usize) -> Option<Vec<u8>> {
    let mut head = Vec::with_capacity(max.min(64 * 1024));
    std::fs::File::open(path)
        .ok()?
        .take(max as u64)
        .read_to_end(&mut head)
        .ok()?;
    Some(head)
}

/// The width of a PDF thumbnail's longer side.
const THUMBNAIL_PX: &str = "480";

/// The first page of a PDF as a PNG, by the `pdftoppm` the hub already
/// renders PDFs with; `None` when it is not installed or the PDF won't
/// render.
fn pdf_thumbnail(pdf: &Path) -> Option<Vec<u8>> {
    let dir = tempfile::TempDir::new().ok()?;
    let out = dir.path().join("thumb");
    let status = std::process::Command::new("pdftoppm")
        .args(["-png", "-singlefile", "-f", "1", "-l", "1", "-scale-to"])
        .arg(THUMBNAIL_PX)
        .arg(pdf)
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(dir.path().join("thumb.png")).ok()
}

// ── Kept in the desk store ───────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod kept {
    use super::*;
    use chrono::{DateTime, Utc};
    use workforce_desk::attachments::{Kept, Origin};
    use workforce_desk::{Desk, Page, Said, Speaker};

    /// `file`, delivered to or uploaded into `session`, a Desk DM or room
    /// session of `agent`'s: kept by artifact, on `message` when it is
    /// known, and its length and thumbnail worked out after.
    pub(crate) async fn keep(
        desk: &Desk,
        agent: &str,
        session: &Session,
        origin: Origin,
        file: &StagedFile,
        message: Option<&str>,
    ) {
        let (artifact_id, media_type) = (&file.artifact_id, file.media_type.as_str());
        let kept = Kept {
            artifact_id: artifact_id.as_str().to_string(),
            agent_id: agent.to_string(),
            channel: session.channel.clone(),
            owner: session.owner.clone(),
            session_id: session.id.clone(),
            origin,
            message_id: message.map(str::to_string),
            media_type: media_type.to_string(),
            filename: file.filename.clone(),
            size: file.described.size,
            width: file.described.width,
            height: file.described.height,
            duration: None,
            thumbnail_id: None,
            caption: file.caption.as_deref().map(crate::desk_surface::shown),
            at: Utc::now(),
        };
        if let Err(e) = desk.store().put_attachment(&kept).await {
            warn!(error = %e, artifact_id = artifact_id.as_str(), "keeping a conversation's file failed");
            return;
        }
        if media_type.starts_with("video/")
            || media_type.starts_with("audio/")
            || media_type == "application/pdf"
        {
            let (desk, agent, id, media_type, session_id) = (
                desk.clone(),
                agent.to_string(),
                artifact_id.clone(),
                media_type.to_string(),
                session.id.clone(),
            );
            tokio::spawn(async move { learn(&desk, &agent, &id, &media_type, &session_id).await });
        }
    }

    /// What takes a subprocess to know about a kept file: a recording's
    /// length and a video's size, a PDF's first page.
    async fn learn(desk: &Desk, agent: &str, id: &ArtifactId, media_type: &str, session_id: &str) {
        let Some(store) = global_store() else { return };
        let Some(path) = store.body_file_path(agent, id).ok().flatten() else {
            return;
        };
        if media_type == "application/pdf" {
            let Some(png) = tokio::task::spawn_blocking(move || pdf_thumbnail(&path))
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            let descriptor = ArtifactDescriptor {
                session_id: Some(session_id.to_string()),
                ttl_seconds: limits().retention.as_secs(),
                ..ArtifactDescriptor::from_context(
                    "desk.thumbnail",
                    ContentType::OctetStream,
                    agent,
                )
                .with_file_meta("thumbnail.png", "image/png")
            };
            let thumb = match store.write(
                agent,
                descriptor,
                workforce_agent::artifacts::ArtifactBody::Bytes(png),
            ) {
                Ok(handle) => handle.id,
                Err(e) => {
                    warn!(error = %e, "staging a PDF's thumbnail failed");
                    return;
                }
            };
            if let Err(e) = desk
                .store()
                .describe_attachment(id.as_str(), None, None, None, Some(thumb.as_str()))
                .await
            {
                warn!(error = %e, "keeping a PDF's thumbnail failed");
            }
            return;
        }
        let probe = match workforce_agent::ffmpeg::probe_media(&path).await {
            Ok(probe) => probe,
            Err(e) => {
                debug!(error = %e, "a recording's length is unknown");
                return;
            }
        };
        let (w, h) = probe.display_size();
        let shown = |v: u32| (probe.has_video && v > 0).then_some(v);
        let duration = (probe.duration > 0.0).then_some(probe.duration);
        if let Err(e) = desk
            .store()
            .describe_attachment(id.as_str(), shown(w), shown(h), duration, None)
            .await
        {
            warn!(error = %e, "keeping a recording's length failed");
        }
    }

    /// A turn of `service`'s in `session_id` that began at `since` has
    /// ended: in a Desk DM or room, the files it delivered go on its
    /// answer (the one saved under `reply_id` when the turn was promised
    /// one, else its last) and the person's on the messages that carried
    /// them. An answerless turn leaves its files as messages of their own.
    pub(crate) async fn bind_turn(
        desk: &Desk,
        service: &crate::serve::AgentService,
        session_id: &str,
        since: DateTime<Utc>,
        reply_id: Option<&str>,
    ) {
        let Ok(Some(ran)) = service.sessions.get(session_id) else {
            return;
        };
        if !is_desk_conversation(&ran.channel) {
            return;
        }
        // A turn that rolled the conversation over answered in its
        // successor.
        let mut sessions = vec![ran.id.clone()];
        let last = if ran.is_sealed() {
            match service.sessions.find(&ran.channel, &ran.owner) {
                Ok(Some(next)) => {
                    sessions.push(next.id.clone());
                    next
                }
                _ => ran,
            }
        } else {
            ran
        };
        let since_turn = |m: &&workforce_common::session::SessionMessage| m.timestamp >= since;
        let answer = last
            .messages
            .iter()
            .filter(since_turn)
            .rfind(|m| match reply_id {
                Some(id) => m.id == id,
                None => {
                    m.role == MessageRole::Assistant
                        && m.tool_data.is_none()
                        && !m.content.trim().is_empty()
                }
            })
            .map(|m| m.id.clone());
        let carried: Vec<(String, Vec<String>)> = last
            .messages
            .iter()
            .filter(since_turn)
            .filter(|m| m.role == MessageRole::User)
            .filter_map(|m| {
                let ids: Vec<String> = m
                    .attachments
                    .iter()
                    .flatten()
                    .filter_map(|a| a.artifact_id.clone())
                    .collect();
                (!ids.is_empty()).then(|| (m.id.clone(), ids))
            })
            .collect();
        if let Err(e) = desk
            .store()
            .bind_turn(
                service.agent_id.as_str(),
                &sessions,
                since,
                answer.as_deref(),
                &carried,
            )
            .await
        {
            warn!(error = %e, session_id, "putting a turn's files on its messages failed");
        }
    }

    /// The files of the messages on `page` of `owner`'s conversation on
    /// `channel` with `agent`, and the agent's files in the time it `span`s
    /// that are messages of their own, in time order.
    pub(crate) async fn with_attachments(
        desk: &Desk,
        agent: &Speaker,
        channel: &str,
        owner: &str,
        page: &mut Page,
        span: crate::desk_conversations::Span,
    ) {
        let ids: Vec<String> = page.messages.iter().map(|m| m.id.clone()).collect();
        match desk.store().attachments_on(&ids).await {
            Ok(mut on) => page.messages.iter_mut().for_each(|m| {
                if let Some(files) = on.remove(&m.id) {
                    m.attachments = files.iter().map(Kept::shown).collect();
                }
            }),
            Err(e) => warn!(error = %e, "reading the files of a DM failed"),
        }
        let loose = match desk
            .store()
            .loose_attachments(&agent.id, channel, owner, span.after, span.before)
            .await
        {
            Ok(loose) => loose,
            Err(e) => {
                warn!(error = %e, "reading a DM's own files failed");
                return;
            }
        };
        if loose.is_empty() {
            return;
        }
        page.messages.extend(loose.iter().map(|k| Said {
            id: format!("file:{}", k.artifact_id),
            from: workforce_desk::Author::Agent,
            author: agent.clone(),
            text: k.caption.clone().unwrap_or_default(),
            at: k.at,
            desk: None,
            mark: None,
            attachments: vec![k.shown()],
        }));
        // Stable: a file said at the same moment as a message follows it.
        page.messages.sort_by_key(|m| m.at);
    }
}

#[cfg(feature = "postgres")]
pub(crate) use kept::{bind_turn, keep, with_attachments};

/// The files the desk keeps for session `session_id` are `owner`'s, on
/// `channel`: its conversation moved to them.
pub(crate) async fn move_session_files(
    session_id: &str,
    owner: &str,
    channel: &str,
) -> eyre::Result<()> {
    #[cfg(feature = "postgres")]
    if let Some(desk) = crate::desk_driver::desk() {
        desk.store()
            .move_attachments(session_id, owner, channel)
            .await?;
    }
    #[cfg(not(feature = "postgres"))]
    let _ = (session_id, owner, channel);
    Ok(())
}

/// Forget the files the desk keeps for session `session_id`: it is gone.
pub(crate) async fn forget_session_files(session_id: &str) -> eyre::Result<()> {
    #[cfg(feature = "postgres")]
    if let Some(desk) = crate::desk_driver::desk() {
        desk.store().forget_attachments(session_id).await?;
    }
    #[cfg(not(feature = "postgres"))]
    let _ = session_id;
    Ok(())
}

/// A Desk conversation files go into: a DM, room or thread session of the
/// agent whose turn delivered them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskConversation {
    /// The agent whose store holds the files.
    pub agent_id: String,
    /// The conversation's session.
    pub session_id: String,
    /// The message they ride on, when it is saved already (a delivery's);
    /// a live turn's go on its answer when it ends.
    pub message: Option<String>,
}

/// A file a turn staged for delivery.
#[derive(Debug, Clone)]
pub struct StagedFile {
    pub artifact_id: ArtifactId,
    pub media_type: String,
    pub filename: String,
    /// The line its producer gave it.
    pub caption: Option<String>,
    pub described: Described,
}

impl StagedFile {
    /// The file `artifact_id` of `agent`'s store, described from the store.
    pub(crate) fn of(
        agent: &str,
        artifact_id: &ArtifactId,
        media_type: &str,
        filename: &str,
        caption: Option<&str>,
    ) -> Self {
        Self {
            artifact_id: artifact_id.clone(),
            media_type: media_type.to_string(),
            filename: filename.to_string(),
            caption: caption.map(str::to_string),
            described: global_store()
                .map(|store| describe(store.as_ref(), agent, artifact_id, media_type))
                .unwrap_or_default(),
        }
    }

    /// `bytes` staged in `agent`'s store as a file it delivers, its type
    /// read from its bytes and its name made safe.
    pub(crate) fn stage(
        agent: &str,
        filename: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> eyre::Result<Self> {
        let store = global_store().ok_or_else(|| eyre::eyre!("no artifact store"))?;
        let filename = safe_filename(filename);
        let media_type = sniff_upload(&bytes[..bytes.len().min(IMAGE_HEAD)], &filename);
        let described = Described::of(&bytes, &media_type);
        let descriptor = ArtifactDescriptor {
            ttl_seconds: limits().retention.as_secs(),
            ..ArtifactDescriptor::from_context(DELIVERY_PRODUCER, ContentType::OctetStream, agent)
                .with_file_meta(&filename, &media_type)
        };
        let handle = store.write(
            agent,
            descriptor,
            workforce_agent::artifacts::ArtifactBody::Bytes(bytes),
        )?;
        Ok(Self {
            artifact_id: handle.id,
            media_type,
            filename,
            caption: caption.map(str::to_string),
            described,
        })
    }

    /// The file as a Desk conversation's message carries it.
    pub(crate) fn delivered(&self, agent: &str) -> DeskFile {
        DeskFile {
            artifact_id: self.artifact_id.as_str().to_string(),
            agent_id: agent.to_string(),
            filename: self.filename.clone(),
            media_type: self.media_type.clone(),
            size: self.described.size,
            width: self.described.width,
            height: self.described.height,
            caption: self.caption.clone(),
        }
    }
}

/// The files a live Desk turn's `invocations` delivered, as its answer
/// carries them.
pub(crate) fn delivered_in(
    agent: &str,
    invocations: &[workforce_agent::runner::ToolInvocation],
) -> Vec<DeskFile> {
    workforce_agent::runner::generated_files(invocations)
        .map(|file| {
            StagedFile::of(
                agent,
                &file.artifact_id,
                &file.media_type,
                &crate::image_sink::delivered_name(file.filename.as_deref(), &file.media_type),
                file.caption.as_deref(),
            )
            .delivered(agent)
        })
        .collect()
}

/// `file` as a `/ws` client is sent it: its reference and size, and an
/// image's bytes inline, which a client that declared no `accept` shows.
fn sent(file: &DeskFile) -> Attachment {
    let inline = file.media_type.starts_with("image/");
    let data = global_store()
        .filter(|_| inline)
        .and_then(|store| ArtifactId::parse(&file.artifact_id).ok().map(|id| (store, id)))
        .and_then(|(store, id)| match store.read(&file.agent_id, &id) {
            Ok(body) => body.into_bytes().ok(),
            Err(e) => {
                debug!(error = %e, artifact_id = %file.artifact_id, "an image's bytes are not here");
                None
            }
        })
        .map(|bytes| BASE64.encode(bytes))
        .unwrap_or_default();
    Attachment {
        filename: file.filename.clone(),
        mime_type: file.media_type.clone(),
        data,
        artifact_id: Some(file.artifact_id.clone()),
        size: Some(file.size),
        width: file.width,
        height: file.height,
        duration: None,
    }
}

/// The `image` frame `files`, delivered into `session_id` with `caption`,
/// are shown to its connections in. Each connection is sent those of the
/// types it declared it shows (see `MediaAccept::shape`).
pub(crate) fn frame(
    session_id: &str,
    files: &[DeskFile],
    caption: Option<String>,
) -> crate::broadcast::BroadcastMessage {
    crate::broadcast::BroadcastMessage::image(
        session_id,
        files.first().map(|f| f.agent_id.clone()),
        caption,
        files.iter().map(sent).collect(),
    )
}

/// **The seam** between a turn's staged files and Desk: `files` go into the
/// conversation `to`. They are kept for its history (on the turn's answer
/// once the turn ends, see [`bind_turn`], or as messages of their own) and
/// kept in the store for the retention (filed under the conversation if
/// they name no session, so its reader may download them). History needs
/// the desk on this hub; retention does not. Whether `to` is a Desk
/// conversation; anything else is left alone. The live frames are the
/// caller's: a live Desk turn's go down its own stream, a delivery's are
/// pushed with it, and any other turn's are broadcast to the session's
/// clients, each sent what it declared it shows (see [`frame`]).
pub(crate) async fn into_desk_conversation(
    sessions: &dyn SessionStore,
    to: &DeskConversation,
    files: &[StagedFile],
) -> bool {
    let Ok(Some(session)) = sessions.get(&to.session_id) else {
        return false;
    };
    if !is_desk_conversation(&session.channel) {
        return false;
    }
    // Kept for as long as the conversation shows it, and filed under it so
    // its reader may download it, whether or not this hub keeps Desk's
    // history: a room on another hub may point at it.
    if let Some(store) = global_store() {
        let retention = limits().retention;
        for file in files {
            match store.retain(
                &to.agent_id,
                &file.artifact_id,
                retention,
                Some(&session.id),
            ) {
                Ok(true) => {}
                Ok(false) => debug!(
                    artifact_id = file.artifact_id.as_str(),
                    "the store keeps this file no longer than it was staged for"
                ),
                Err(e) => warn!(
                    error = %e,
                    artifact_id = file.artifact_id.as_str(),
                    "keeping a conversation's file longer failed"
                ),
            }
        }
    }
    #[cfg(feature = "postgres")]
    if let Some(desk) = crate::desk_driver::desk() {
        for file in files {
            keep(
                desk,
                &to.agent_id,
                &session,
                workforce_desk::attachments::Origin::Agent,
                file,
                to.message.as_deref(),
            )
            .await;
        }
    }
    true
}

// ── Routes ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

fn refused(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// `GET /api/agents/{id}/attachments/{artifact}`: the bytes of a file of a
/// conversation the caller may read.
pub async fn download(
    State(state): State<Arc<GatewayState>>,
    UrlPath((agent, artifact)): UrlPath<(String, String)>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    let caller = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    if !caller.is_authenticated() {
        return refused(StatusCode::UNAUTHORIZED, "not signed in");
    }
    if !caller.has_scope(crate::jwt::SCOPE_SESSIONS) {
        return refused(StatusCode::FORBIDDEN, "scope 'sessions' required");
    }
    if let Some(id) = caller.identity_id.as_deref() {
        if crate::standing::suspended(state.identity_store.as_ref(), id).is_some() {
            return refused(StatusCode::FORBIDDEN, "this person is suspended");
        }
    }
    let not_found = || refused(StatusCode::NOT_FOUND, "no such file");
    let Some(service) = state.registry.get(&agent) else {
        return not_found();
    };
    let Ok(id) = ArtifactId::parse(&artifact) else {
        return not_found();
    };
    let Some(store) = global_store() else {
        return refused(StatusCode::SERVICE_UNAVAILABLE, "no artifact store");
    };
    let descriptor = match store.metadata(service.agent_id.as_str(), &id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(),
        Err(e) => {
            warn!(error = %e, "reading a file's metadata failed");
            return refused(StatusCode::INTERNAL_SERVER_ERROR, "could not read the file");
        }
    };
    // Only a file of a conversation is served: whoever may read the
    // conversation may read its files.
    let Some(session_id) = descriptor.session_id.as_deref() else {
        return not_found();
    };
    if let Err(refusal) = lookup_owned_session(
        service.sessions.as_ref(),
        &caller,
        session_id,
        SessionAccess::Read,
    ) {
        return match refusal.0 {
            StatusCode::FORBIDDEN => refusal.into_response(),
            _ => not_found(),
        };
    }
    serve(
        store.as_ref(),
        service.agent_id.as_str(),
        &id,
        &descriptor,
        headers.get(header::RANGE),
    )
}

/// The part of a `len`-byte body a `Range` header asks for: `Ok(None)` for
/// all of it (no header, or one this doesn't serve in part), `Err` when it
/// asks for nothing the body has.
fn range_of(range: Option<&HeaderValue>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = range
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.trim().strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    // Several ranges are served whole.
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let (start, end) = (start.trim(), end.trim());
    let (first, last) = match (start.parse::<u64>().ok(), end.parse::<u64>().ok()) {
        (Some(s), Some(e)) if s <= e => (s, e.min(len.saturating_sub(1))),
        (Some(_), Some(_)) => return Ok(None),
        (Some(s), None) if end.is_empty() => (s, len.saturating_sub(1)),
        (None, Some(n)) if start.is_empty() => {
            if n == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len.saturating_sub(1))
        }
        _ => return Ok(None),
    };
    if len == 0 || first >= len {
        return Err(());
    }
    Ok(Some((first, last)))
}

/// Serves the file `id`: by a short-lived signed link where the store has
/// them (it serves ranges itself), else from disk or memory, a `Range` in
/// part.
fn serve(
    store: &dyn ArtifactStore,
    agent: &str,
    id: &ArtifactId,
    descriptor: &ArtifactDescriptor,
    range: Option<&HeaderValue>,
) -> Response {
    match store.presigned_get(agent, id, Duration::from_secs(300)) {
        Ok(Some(url)) => return axum::response::Redirect::temporary(&url).into_response(),
        Ok(None) => {}
        Err(e) => {
            warn!(error = %e, "signing a file's link failed");
            return refused(StatusCode::INTERNAL_SERVER_ERROR, "could not read the file");
        }
    }
    enum Body {
        File(std::path::PathBuf),
        Bytes(Vec<u8>),
    }
    let body = match store.body_file_path(agent, id) {
        Ok(Some(path)) => Body::File(path),
        _ => match store.read(agent, id).and_then(|b| b.into_bytes()) {
            Ok(bytes) => Body::Bytes(bytes),
            Err(_) => return refused(StatusCode::NOT_FOUND, "no such file"),
        },
    };
    let len = match &body {
        Body::File(path) => match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return refused(StatusCode::NOT_FOUND, "no such file"),
        },
        Body::Bytes(bytes) => bytes.len() as u64,
    };
    let part = match range_of(range, len) {
        Ok(part) => part,
        Err(()) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return r;
        }
    };
    let (first, last) = part.unwrap_or((0, len.saturating_sub(1)));
    let count = if len == 0 { 0 } else { last - first + 1 };
    let mut response = match body {
        Body::File(path) => {
            use std::io::Seek;
            use tokio::io::AsyncReadExt;
            let mut file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => return refused(StatusCode::NOT_FOUND, "no such file"),
            };
            if first > 0 && file.seek(std::io::SeekFrom::Start(first)).is_err() {
                return refused(StatusCode::INTERNAL_SERVER_ERROR, "could not read the file");
            }
            let reader = tokio::fs::File::from_std(file).take(count);
            Response::new(axum::body::Body::from_stream(
                tokio_util::io::ReaderStream::new(reader),
            ))
        }
        Body::Bytes(bytes) => {
            let end = (first + count) as usize;
            Response::new(axum::body::Body::from(
                bytes[first as usize..end.min(bytes.len())].to_vec(),
            ))
        }
    };
    let media_type = descriptor
        .media_type
        .clone()
        .unwrap_or_else(|| descriptor.mime.as_str().to_string());
    let filename = descriptor
        .filename
        .clone()
        .unwrap_or_else(|| format!("{}.{}", id.as_str(), descriptor.mime.extension()));
    let headers = response.headers_mut();
    let mut set = |name: header::HeaderName, value: String| {
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(name, v);
        }
    };
    set(header::CONTENT_TYPE, media_type);
    set(
        header::CONTENT_DISPOSITION,
        workforce_agent::artifacts::disposition::content_disposition_attachment(&filename),
    );
    set(header::CONTENT_LENGTH, count.to_string());
    set(header::ACCEPT_RANGES, "bytes".into());
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff".into());
    // Never a page of this origin, whatever the file is.
    set(header::CONTENT_SECURITY_POLICY, "sandbox".into());
    set(header::CACHE_CONTROL, "private, max-age=3600".into());
    if part.is_some() {
        set(header::CONTENT_RANGE, format!("bytes {first}-{last}/{len}"));
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    response
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub token: Option<String>,
    /// The conversation's channel: `desk` (a DM, the default) or
    /// `desk:<slug>` (a room), or another app's.
    pub channel: Option<String>,
    /// The session the upload is for, as the `/ws` `connected` frame named
    /// it; the conversation's current one when not given.
    pub session_id: Option<String>,
    /// The file's name, for a raw body.
    pub filename: Option<String>,
}

/// A name safe to keep: no directories, nothing blank, not too long.
fn safe_filename(name: &str) -> String {
    let base = Path::new(name.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let base: String = base.chars().filter(|c| !c.is_control()).take(255).collect();
    if base.is_empty() || base == "." || base == ".." {
        "upload".into()
    } else {
        base
    }
}

/// `POST /api/agents/{id}/attachments`: stages one file for the caller's
/// conversation with the agent.
pub async fn upload(
    State(state): State<Arc<GatewayState>>,
    UrlPath(agent): UrlPath<String>,
    Query(q): Query<UploadQuery>,
    request: axum::extract::Request,
) -> Response {
    let headers = request.headers().clone();
    let caller = resolve_caller_context(&state, &headers, q.token.as_deref()).await;
    let owner =
        match (caller.identity_id.as_deref(), caller.device_id.as_deref()) {
            (Some(person), _) | (None, Some(person)) => person.to_string(),
            (None, None) if caller.is_admin => return refused(
                StatusCode::BAD_REQUEST,
                "the hub's admin token is no one's: upload with the token the conversation uses",
            ),
            (None, None) => return refused(StatusCode::UNAUTHORIZED, "not signed in"),
        };
    if !caller.has_scope(crate::jwt::SCOPE_SESSIONS) {
        return refused(StatusCode::FORBIDDEN, "scope 'sessions' required");
    }
    if let Some(id) = caller.identity_id.as_deref() {
        if crate::standing::suspended(state.identity_store.as_ref(), id).is_some() {
            return refused(StatusCode::FORBIDDEN, "this person is suspended");
        }
    }
    if let Err(retry) =
        crate::gateway::check_action_rate_limit(state.action_limiter.as_ref(), &caller)
    {
        return crate::gateway::too_many_requests_err(retry).into_response();
    }
    let channel = q
        .channel
        .clone()
        .unwrap_or_else(|| DESK_CHANNEL.to_string());
    if !is_conversational(&channel) {
        return refused(
            StatusCode::BAD_REQUEST,
            "channel names no conversation: desk, desk:<slug>, ios, web or terminal",
        );
    }
    let Some(service) = state.registry.get(&agent) else {
        return refused(StatusCode::NOT_FOUND, "no such agent");
    };
    let agent_id = service.agent_id.as_str().to_string();
    let session = match q.session_id.as_deref().filter(|s| !s.is_empty()) {
        Some(sid) => match service.sessions.get(sid) {
            Ok(Some(s)) if s.owner == owner && s.channel == channel => s,
            Ok(_) => return refused(StatusCode::NOT_FOUND, "no such session"),
            Err(e) => {
                warn!(error = %e, "reading a session failed");
                return refused(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not read the session",
                );
            }
        },
        None => match crate::gateway::anchor_session(&service, &channel, &owner, &agent_id) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "opening a conversation failed");
                return refused(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not open the conversation",
                );
            }
        },
    };
    let Some(store) = global_store() else {
        return refused(StatusCode::SERVICE_UNAVAILABLE, "no artifact store");
    };
    let staging = store.staging_dir(&agent_id);
    let cap = limits().max_bytes;
    let multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    let spooled = if multipart {
        let mut form = match Multipart::from_request(request, &()).await {
            Ok(form) => form,
            Err(e) => return refused(StatusCode::BAD_REQUEST, &e.body_text()),
        };
        loop {
            match form.next_field().await {
                Ok(Some(field)) => {
                    let Some(name) = field.file_name().map(str::to_string) else {
                        continue;
                    };
                    let name = safe_filename(&name);
                    break spool_to_disk(Box::pin(field), staging.as_deref(), &name, cap)
                        .await
                        .map(|s| (name, s));
                }
                Ok(None) => return refused(StatusCode::BAD_REQUEST, "the form holds no file"),
                Err(e) => return refused(StatusCode::BAD_REQUEST, &e.body_text()),
            }
        }
    } else {
        let Some(name) = q.filename.as_deref().filter(|n| !n.trim().is_empty()) else {
            return refused(StatusCode::BAD_REQUEST, "name the file with ?filename=");
        };
        let name = safe_filename(name);
        spool_to_disk(
            request.into_body().into_data_stream(),
            staging.as_deref(),
            &name,
            cap,
        )
        .await
        .map(|s| (name, s))
    };
    let (filename, spooled) = match spooled {
        Ok(s) => s,
        Err(SpoolError::TooLarge) => {
            return refused(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("a file may be at most {} MB", cap / (1024 * 1024)),
            )
        }
        Err(SpoolError::Io(e)) => {
            warn!(error = %e, "receiving an upload failed");
            return refused(StatusCode::BAD_REQUEST, "the upload did not arrive whole");
        }
    };
    if spooled.bytes == 0 {
        spooled.cleanup();
        return refused(StatusCode::BAD_REQUEST, "the file is empty");
    }
    let head = read_head(&spooled.path, IMAGE_HEAD).unwrap_or_default();
    let media_type = sniff_upload(&head, &filename);
    let desk_conversation = is_desk_conversation(&channel);
    let mut descriptor = ArtifactDescriptor::from_context(
        SESSION_UPLOAD_PRODUCER,
        ContentType::OctetStream,
        &agent_id,
    )
    .with_file_meta(filename.clone(), media_type.clone());
    descriptor.session_id = Some(session.id.clone());
    descriptor.user_id = caller.identity_id.clone();
    descriptor.ttl_seconds = if desk_conversation {
        limits().retention.as_secs()
    } else {
        ATTACHMENT_TTL.as_secs()
    };
    let handle = match store.write_from_path(&agent_id, descriptor, &spooled.path) {
        Ok(h) => h,
        Err(e) => {
            spooled.cleanup();
            warn!(error = %e, "storing an upload failed");
            return refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the file",
            );
        }
    };
    let described = Described::sized(spooled.bytes, &media_type, || Some(head));
    #[cfg(feature = "postgres")]
    if desk_conversation {
        if let Some(desk) = crate::desk_driver::desk() {
            keep(
                desk,
                &agent_id,
                &session,
                workforce_desk::attachments::Origin::You,
                &StagedFile {
                    artifact_id: handle.id.clone(),
                    media_type: media_type.clone(),
                    filename: filename.clone(),
                    caption: None,
                    described,
                },
                None,
            )
            .await;
        }
    }
    info!(
        agent = %agent_id,
        session_id = %session.id,
        artifact_id = handle.id.as_str(),
        bytes = spooled.bytes,
        media_type = %media_type,
        "file uploaded to a conversation"
    );
    let mut body = json!({
        "artifact_id": handle.id.as_str(),
        "media_type": media_type,
        "filename": filename,
        "size": described.size,
        "thumbnail_url": null,
        "session_id": session.id,
    });
    if let (Some(w), Some(h)) = (described.width, described.height) {
        body["width"] = json!(w);
        body["height"] = json!(h);
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

// ── view_attachment on the gateway's apps ────────────────────────────────

/// The files of a conversation on the gateway's apps, for `view_attachment`:
/// what the person attached to their messages (across a rolled-over
/// conversation's sessions), and in a Desk conversation every file kept for
/// it. Bytes come from `agent`'s own store.
pub(crate) struct SessionFiles {
    agent_id: String,
    sessions: Arc<dyn SessionStore>,
}

impl SessionFiles {
    pub(crate) fn new(agent_id: impl Into<String>, sessions: Arc<dyn SessionStore>) -> Self {
        Self {
            agent_id: agent_id.into(),
            sessions,
        }
    }

    /// The sessions of the conversation `session_id` is part of, newest
    /// first.
    fn chain(&self, session_id: &str) -> eyre::Result<Vec<Session>> {
        let mut chain = Vec::new();
        let mut next = Some(session_id.to_string());
        while let Some(id) = next.take() {
            if chain.len() >= 20 {
                break;
            }
            let Some(session) = self.sessions.get(&id)? else {
                break;
            };
            next = session.prev_session_id().map(str::to_string);
            chain.push(session);
        }
        Ok(chain)
    }
}

const INLINE: &str = "inline:";

#[async_trait]
impl workforce_skills::adapters::image_view::ConversationFiles for SessionFiles {
    async fn files(
        &self,
        session_id: &str,
    ) -> eyre::Result<Vec<workforce_skills::adapters::image_view::ConversationFile>> {
        use workforce_skills::adapters::image_view::ConversationFile;
        let chain = self.chain(session_id)?;
        let Some(current) = chain.first() else {
            return Ok(Vec::new());
        };
        let mut files: Vec<ConversationFile> = Vec::new();
        for session in &chain {
            for message in session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
            {
                for (i, a) in message.attachments.iter().flatten().enumerate() {
                    let id = match &a.artifact_id {
                        Some(id) => id.clone(),
                        None if !a.data.is_empty() => {
                            format!("{INLINE}{}:{}:{i}", session.id, message.id)
                        }
                        None => continue,
                    };
                    files.push(ConversationFile {
                        id,
                        name: a.filename.clone(),
                        mime_type: a.mime_type.clone(),
                        at: message.timestamp,
                        size: None,
                    });
                }
            }
        }
        #[cfg(feature = "postgres")]
        if is_desk_conversation(&current.channel) {
            if let Some(desk) = crate::desk_driver::desk() {
                let kept = desk
                    .store()
                    .conversation_files(&self.agent_id, &current.channel, &current.owner, 200)
                    .await?;
                for k in kept {
                    if !files.iter().any(|f| f.id == k.artifact_id) {
                        files.push(ConversationFile {
                            id: k.artifact_id,
                            name: k.filename,
                            mime_type: k.media_type,
                            at: k.at,
                            size: Some(k.size),
                        });
                    }
                }
            }
        }
        #[cfg(not(feature = "postgres"))]
        let _ = current;
        Ok(files)
    }

    async fn read(
        &self,
        session_id: &str,
        file: &workforce_skills::adapters::image_view::ConversationFile,
    ) -> eyre::Result<Vec<u8>> {
        if let Some(rest) = file.id.strip_prefix(INLINE) {
            let mut parts = rest.splitn(3, ':');
            let (Some(sid), Some(mid), Some(i)) = (parts.next(), parts.next(), parts.next()) else {
                eyre::bail!("no such file");
            };
            let i: usize = i.parse()?;
            let chain = self.chain(session_id)?;
            let attachment = chain
                .iter()
                .filter(|s| s.id == sid)
                .flat_map(|s| s.messages.iter())
                .find(|m| m.id == mid)
                .and_then(|m| m.attachments.as_ref()?.get(i).cloned())
                .ok_or_else(|| eyre::eyre!("no such file"))?;
            return Ok(
                match workforce_common::session::is_text_like_mime(&attachment.mime_type) {
                    true => attachment.data.into_bytes(),
                    false => {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.decode(attachment.data)?
                    }
                },
            );
        }
        let store = global_store().ok_or_else(|| eyre::eyre!("no artifact store"))?;
        let id = ArtifactId::parse(&file.id)?;
        Ok(store.read(&self.agent_id, &id)?.into_bytes()?)
    }
}

#[cfg(test)]
#[path = "desk_attachments_tests.rs"]
mod tests;

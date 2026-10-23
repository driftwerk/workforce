//! Hub-side [`ImageSink`] — routes a generated attachment (image or document)
//! to the surface the turn originates from.
//!
//! The runner calls [`HubImageSink::deliver`] inline after a tool stages an
//! attachment. We resolve the staged bytes from the artifact store and
//! dispatch on the session context:
//!   - **Slack** (a Slack channel id is in scope): resolve the workspace's
//!     surface and perform a native `files.upload` to the channel/thread.
//!   - **Gateway / iOS** (no Slack channel): emit a `ServerMessage::image`
//!     over the in-process broadcast, filtered to the session by the WS
//!     forwarder. An image rides inline as base64, which both the web UI and
//!     iOS render; any other file rides by `artifact_id`, which the web UI
//!     offers as a download through the session file route and iOS ignores.
//!     NATS-only (distributed) deployments have no in-process broadcast
//!     wired, so there the bytes stay staged as an artifact.

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};

use workforce_agent::artifacts::store::global_store;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactId};
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::image_gen::ImageSink;
use workforce_common::channels::{ChannelHint, SurfaceRegistry};

use crate::broadcast::BroadcastMessage;
use workforce_gateway_client::protocol::Attachment;

pub struct HubImageSink {
    surface_registry: Arc<RwLock<SurfaceRegistry>>,
    /// Agent id the artifacts were written under — must match what the
    /// `generate_image` tool used, so reads resolve to the same namespace.
    agent_id: String,
    /// In-process broadcast sender for gateway/iOS delivery. `None` when the
    /// gateway channel isn't wired (e.g. NATS-only deployments).
    broadcast: Option<broadcast::Sender<BroadcastMessage>>,
}

impl HubImageSink {
    pub fn new(
        surface_registry: Arc<RwLock<SurfaceRegistry>>,
        agent_id: impl Into<String>,
        broadcast: Option<broadcast::Sender<BroadcastMessage>>,
    ) -> Self {
        Self {
            surface_registry,
            agent_id: agent_id.into(),
            broadcast,
        }
    }

    /// Read the staged attachment bytes from the artifact store.
    fn read_bytes(&self, artifact_id: &ArtifactId) -> Result<Vec<u8>> {
        let store = global_store().ok_or_else(|| eyre!("artifact store not configured"))?;
        match store.read(&self.agent_id, artifact_id)? {
            ArtifactBody::Bytes(bytes) => Ok(bytes),
            other => Err(eyre!(
                "artifact {} is {:?}, not raw bytes",
                artifact_id.as_str(),
                other.content_type()
            )),
        }
    }
}

/// File extension for a generated attachment, from its media type. Used only
/// to synthesise a filename when the producer didn't supply one.
fn extension_for(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mp4" => "m4a",
        "audio/ogg" | "audio/opus" => "ogg",
        _ => "png",
    }
}

#[async_trait]
impl ImageSink for HubImageSink {
    async fn deliver(
        &self,
        ctx: &SessionContext,
        artifact_id: &ArtifactId,
        media_type: &str,
        filename: Option<&str>,
        caption: Option<&str>,
    ) -> Result<()> {
        let filename = filename
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("generated.{}", extension_for(media_type)));

        // A Slack channel id in the session context means the turn came from
        // Slack — deliver as a native file upload. CURRENT_CHANNEL_ID is only
        // populated on Slack runs (see runner task-locals).
        if let Some(channel) = ctx.channel_id.as_deref().filter(|c| !c.is_empty()) {
            let bytes = self.read_bytes(artifact_id)?;
            let hint =
                ChannelHint::from_parts(None, ctx.workspace_id.clone(), Some(channel.to_string()));
            let surface = {
                let reg = self.surface_registry.read().await;
                reg.resolve("slack", channel, hint).await?
            };
            surface
                .upload_file(
                    channel,
                    &filename,
                    &bytes,
                    caption,
                    ctx.thread_id.as_deref().filter(|t| !t.is_empty()),
                )
                .await?;
            info!(
                channel = %channel,
                artifact_id = artifact_id.as_str(),
                bytes = bytes.len(),
                "delivered attachment to Slack"
            );
            return Ok(());
        }

        // Gateway / iOS path: emit a `ServerMessage::image` over the in-process
        // broadcast, filtered to this session by the WS forwarder. An image
        // rides inline as base64 (iOS renders `image/*` only, see
        // `ingestServerImageAttachments`); anything else rides by reference so
        // the web UI can fetch it from the session file route.
        let Some(session_id) = ctx.id.as_deref().filter(|s| !s.is_empty()) else {
            return Err(eyre!(
                "no session id in context; cannot route generated image to a gateway client"
            ));
        };
        let Some(tx) = self.broadcast.as_ref() else {
            // NATS-only / unwired deployment: bytes remain staged as an artifact.
            debug!(
                session = session_id,
                artifact_id = artifact_id.as_str(),
                "image staged; gateway broadcast not wired (NATS-only deployment?)"
            );
            return Ok(());
        };

        let inline = media_type.starts_with("image/");
        let attachment = Attachment {
            filename,
            mime_type: media_type.to_string(),
            data: if inline {
                BASE64.encode(self.read_bytes(artifact_id)?)
            } else {
                String::new()
            },
            artifact_id: (!inline).then(|| artifact_id.as_str().to_string()),
        };
        let msg = BroadcastMessage::image(
            session_id,
            ctx.agent_id.clone(),
            caption.map(str::to_string),
            vec![attachment],
        );
        // `send` errors only when there are no subscribers — not fatal.
        match tx.send(msg) {
            Ok(n) => info!(
                session = session_id,
                artifact_id = artifact_id.as_str(),
                media_type,
                inline,
                subscribers = n,
                "broadcast generated attachment to gateway client(s)"
            ),
            Err(_) => debug!(
                session = session_id,
                artifact_id = artifact_id.as_str(),
                "no gateway subscribers for generated image"
            ),
        }
        Ok(())
    }
}

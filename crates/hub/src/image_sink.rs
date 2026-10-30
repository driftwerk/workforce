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
//!     forwarder. Every file carries its `artifact_id` and size, and an image
//!     rides inline as base64 too. Each connection is sent the files of the
//!     types it declared at connect (`accept`); one that declared none (iOS)
//!     is sent images alone, inline, as before. In a Desk DM or room the
//!     file is also kept for the conversation's history.
//!     NATS-only (distributed) deployments have no in-process broadcast
//!     wired, so there the bytes stay staged as an artifact.
//!   - **Desk, live**: a turn a Desk connection attends sends the file down
//!     its own progress stream instead, which reaches that connection on
//!     any instance, ordered with the turn's text; the answer carries it.
//!   - **Desk, unattended** (a job, workflow or check made from Desk): the
//!     file is delivered into that Desk conversation as a message of its
//!     own.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};

use workforce_agent::artifacts::store::global_store;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactId};
use workforce_agent::runner::{ProgressEvent, SessionContext};
use workforce_agent::tools::image_gen::ImageSink;
use workforce_common::channels::desk::DeskTarget;
use workforce_common::channels::{ChannelHint, SurfaceRegistry};
use workforce_common::session::SessionStore;

use crate::broadcast::BroadcastMessage;
use crate::desk_attachments::{into_desk_conversation, DeskConversation, StagedFile};

pub struct HubImageSink {
    surface_registry: Arc<RwLock<SurfaceRegistry>>,
    /// Agent id the artifacts were written under — must match what the
    /// `generate_image` tool used, so reads resolve to the same namespace.
    agent_id: String,
    /// In-process broadcast sender for gateway/iOS delivery. `None` when the
    /// gateway channel isn't wired (e.g. NATS-only deployments).
    broadcast: Option<broadcast::Sender<BroadcastMessage>>,
    /// The agent's sessions, to tell a Desk conversation's files (kept for
    /// its history) from any other app's.
    sessions: Option<Arc<dyn SessionStore>>,
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
            sessions: None,
        }
    }

    /// Keeps the files delivered into a Desk conversation of `sessions`'
    /// for its history.
    pub fn with_sessions(mut self, sessions: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// `file`, delivered by a run nobody is attending that answers to the
    /// Desk conversation `target`: a message of its own there, for the
    /// person the run is for.
    async fn deliver_to_desk(
        &self,
        ctx: &SessionContext,
        target: &DeskTarget,
        file: StagedFile,
    ) -> Result<()> {
        let sessions = self
            .sessions
            .as_deref()
            .ok_or_else(|| eyre!("no sessions to deliver a Desk conversation's file into"))?;
        let owner = ctx
            .user_id
            .as_deref()
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| {
                eyre!("a Desk conversation's file is someone's: this run is for nobody")
            })?;
        let outbox = crate::desk_surface::Outbox {
            agent_id: &self.agent_id,
            sessions,
        };
        let delivered = crate::desk_surface::deliver_file(outbox, owner, target, file).await?;
        info!(
            session = %delivered.session_id,
            message_id = %delivered.message_id,
            "delivered attachment to a Desk conversation"
        );
        Ok(())
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

/// The name a staged file is delivered under: its producer's, else one
/// made from its media type.
pub(crate) fn delivered_name(filename: Option<&str>, media_type: &str) -> String {
    filename
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("generated.{}", extension_for(media_type)))
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
        let filename = delivered_name(filename, media_type);

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

        let file = StagedFile::of(&self.agent_id, artifact_id, media_type, &filename, caption);
        if let Some(scope) = ctx.desk.as_ref().filter(|scope| !scope.attended) {
            return self.deliver_to_desk(ctx, &scope.target, file).await;
        }
        let Some(session_id) = ctx.id.as_deref().filter(|s| !s.is_empty()) else {
            return Err(eyre!(
                "no session id in context; cannot route generated image to a gateway client"
            ));
        };
        if let Some(sessions) = &self.sessions {
            into_desk_conversation(
                sessions.as_ref(),
                &DeskConversation {
                    agent_id: self.agent_id.clone(),
                    session_id: session_id.to_string(),
                    message: None,
                },
                std::slice::from_ref(&file),
            )
            .await;
        }
        let delivered = file.delivered(&self.agent_id);
        // A live Desk turn shows its files in its own stream, ordered with
        // its text, on whichever instance its connection is.
        if let Some(progress) = ctx
            .desk
            .as_ref()
            .and_then(|_| workforce_agent::runner::current_progress_tx())
        {
            if progress
                .send(ProgressEvent::File(delivered.clone()))
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        let Some(tx) = self.broadcast.as_ref() else {
            // NATS-only / unwired deployment: bytes remain staged as an artifact.
            debug!(
                session = session_id,
                artifact_id = artifact_id.as_str(),
                "image staged; gateway broadcast not wired (NATS-only deployment?)"
            );
            return Ok(());
        };
        let msg = crate::desk_attachments::frame(
            session_id,
            std::slice::from_ref(&delivered),
            delivered.caption.clone(),
        );
        // `send` errors only when there are no subscribers — not fatal.
        match tx.send(msg) {
            Ok(n) => info!(
                session = session_id,
                artifact_id = artifact_id.as_str(),
                media_type,
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

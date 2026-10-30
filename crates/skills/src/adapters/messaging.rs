//! Generic messaging tool — platform-agnostic message sending.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use workforce_agent::tools::{SurfaceEffect, Tool, ToolResult};
use workforce_common::channels::{
    ChannelMessage, ChannelSurface, OutboundMessage, SurfaceRegistry,
};

use crate::adapters::messaging_routing::{
    channel_arg, channel_hint_from_input, invalidate_if_channel_not_found, platform_arg,
    schema_platform_property, schema_workspace_property, writes,
};
use crate::adapters::utils::{coerce_to_u64, decode_upload_content};

const DEDUP_WINDOW: Duration = Duration::from_secs(10);

async fn resolve_for_channel(
    surfaces: &RwLock<SurfaceRegistry>,
    input: &serde_json::Value,
    channel: &str,
) -> Result<Arc<dyn ChannelSurface>> {
    surfaces
        .read()
        .await
        .resolve(platform_arg(input), channel, channel_hint_from_input(input))
        .await
}

async fn resolve_for_workspace(
    surfaces: &RwLock<SurfaceRegistry>,
    input: &serde_json::Value,
) -> Result<Arc<dyn ChannelSurface>> {
    let platform = platform_arg(input);
    let hint = channel_hint_from_input(input);
    let registry = surfaces.read().await;
    if let Some(ws) = hint.explicit_workspace.as_deref() {
        return registry
            .get(&workforce_common::channels::SurfaceKey::workspace(
                platform, ws,
            ))
            .cloned()
            .ok_or_else(|| {
                eyre::eyre!(
                    "no {platform} surface for workspace '{ws}' (available: {:?})",
                    registry.workspaces_for(platform)
                )
            });
    }
    if let Some(ws) = hint.context_workspace.as_deref() {
        if let Some(s) = registry
            .get(&workforce_common::channels::SurfaceKey::workspace(
                platform, ws,
            ))
            .cloned()
        {
            return Ok(s);
        }
    }
    registry
        .all_for(platform)
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("no {platform} surface registered for this agent"))
}

fn channel_message_to_json(m: &ChannelMessage) -> serde_json::Value {
    serde_json::json!({
        "user": m.user,
        "user_name": m.user_name,
        "channel_name": m.channel_name,
        "text": m.text,
        "timestamp": m.timestamp,
        "message_id": m.message_id,
        "thread_id": m.thread_id,
    })
}

async fn failure_result(
    surfaces: &RwLock<SurfaceRegistry>,
    input: &serde_json::Value,
    channel: &str,
    operation: &str,
    err: eyre::Report,
    duration_ms: u64,
) -> Result<ToolResult> {
    warn!(error = %err, channel = %channel, operation = %operation, "messaging tool failed");
    invalidate_if_channel_not_found(surfaces, input, channel, &err.to_string()).await;
    Ok(ToolResult::from_report(
        &err.wrap_err(operation.to_string()),
        duration_ms,
    ))
}

/// Generic tool for sending messages across any registered platform.
///
/// Defaults to reply-only (requires channel to be specified).
/// Can send to any channel if explicitly given one.
///
/// Holds a small dedup cache: identical `(channel, thread, text)` calls
/// within [`DEDUP_WINDOW`] return success without re-posting, so a model
/// that loops on this tool (some smaller models narrate progress here
/// instead of via their streaming response) can't spam the channel.
pub struct MessagingSendTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
    dedup: Arc<Mutex<HashMap<u64, Instant>>>,
}

impl MessagingSendTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self {
            surfaces,
            dedup: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Hash a `(channel, thread, text)` triple into a single u64 — used as
    /// the dedup map key without holding cleartext or large strings.
    fn dedup_key(channel: &str, thread: Option<&str>, text: &str) -> u64 {
        let mut h = DefaultHasher::new();
        channel.hash(&mut h);
        thread.unwrap_or("").hash(&mut h);
        text.hash(&mut h);
        h.finish()
    }

    /// Record the call and tell the caller whether the same content has been
    /// sent within [`DEDUP_WINDOW`]. Also evicts any entries that have fallen
    /// out of the window so the map stays bounded.
    async fn check_and_record(&self, key: u64) -> bool {
        let mut map = self.dedup.lock().await;
        let now = Instant::now();
        map.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
        matches!(
            map.insert(key, now),
            Some(prev) if now.duration_since(prev) < DEDUP_WINDOW
        )
    }
}

#[async_trait]
impl Tool for MessagingSendTool {
    fn name(&self) -> &str {
        "messaging.send"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Send a message on Slack, Teams or Desk. Your direct text reply already posts to the \
         conversation that invoked you — use this tool ONLY when you need to message a \
         DIFFERENT channel, thread or Desk conversation, or when the user explicitly asks you to \
         post a follow-up message. In a Desk conversation, with no platform it posts to Desk: \
         no channel is this conversation, \"dm\" the person's DM, \"#<room name>\" one of \
         their rooms. Do NOT call this to narrate progress in the current conversation; that's \
         what your streaming response is for. Identical (channel, thread, text) calls within a \
         short window are deduplicated and silently capped, so retrying the same message has no \
         effect."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to send to. Required outside a Desk \
                                    conversation; in one it defaults to that conversation."
                },
                "text": {
                    "type": "string",
                    "description": "Message text to send"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread identifier for replying in a thread"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel =
            channel_arg(&input).ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let text = input["text"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'text' parameter"))?;
        let thread_id = input["thread_id"].as_str();

        let dedup_key = Self::dedup_key(channel, thread_id, text);
        if self.check_and_record(dedup_key).await {
            warn!(
                target: "messaging",
                channel = %channel,
                thread_id = ?thread_id,
                "messaging.send deduplicated — identical text sent within window"
            );
            return Ok(ToolResult::success(
                serde_json::json!({
                    "status": "deduplicated",
                    "channel": channel,
                    "note": "An identical message was sent to this channel/thread within the last 10 seconds. The duplicate was NOT posted. Stop calling messaging.send for the same content; if you intended to narrate progress to this conversation, use your direct streaming response instead.",
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        let text = if workforce_common::security_scanner::contains_credentials(text) {
            warn!(
                target: "sentinel",
                channel = %channel,
                "Credential detected in messaging.send — redacting"
            );
            workforce_common::security_scanner::redact_credentials(text)
        } else {
            text.to_string()
        };

        let message = OutboundMessage {
            channel: channel.to_string(),
            text,
            thread_id: thread_id.map(|s| s.to_string()),
            username: None,
            icon_url: None,
            blocks: None,
        };

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.send_message(&message).await {
            Ok(sent) => {
                info!(
                    channel = %channel,
                    platform = %sent.platform,
                    message_id = %sent.message_id,
                    "Message sent"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": sent.channel,
                        "message_id": sent.message_id,
                        "thread_id": sent.thread_id,
                        "platform": sent.platform,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to send message",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for reading message history from any registered platform.
pub struct MessagingReadTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingReadTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingReadTool {
    fn name(&self) -> &str {
        "messaging.read"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "Read messages from a Slack channel, newest first. Each returned message includes \
         a resolved `user_name` and `channel_name`; refer to users and channels by \
         those names in your response rather than the raw IDs. Without `oldest`/`latest` \
         this returns one page of recent messages (max 100). Set `oldest` (a platform \
         timestamp) to walk back through history: the call pages until the range is \
         exhausted or the per-call budget (20 pages of `limit`) runs out — then \
         `truncated` is true and `next_latest` is the resume cursor; call again with the \
         same `oldest` and `latest: next_latest` to continue. Thread replies are not \
         included — fetch them per thread with messaging.read_thread."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to read from"
                },
                "limit": {
                    "type": "integer",
                    "description": "Messages per page, integer (default 20, max 100). With `oldest`/`latest` set, a call returns up to 20 pages of this size.",
                    "default": 20
                },
                "oldest": {
                    "type": "string",
                    "description": "Platform timestamp (Slack: e.g. \"1747033200.000000\"); only messages strictly newer than this are returned, paging as needed."
                },
                "latest": {
                    "type": "string",
                    "description": "Platform timestamp; only messages strictly older than this are returned. Pass a previous call's `next_latest` to resume a truncated walk."
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(20).min(100) as usize;
        let oldest = input["oldest"].as_str();
        let latest = input["latest"].as_str();

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        // Cross-channel read guard: reading a channel other than the current
        // conversation requires the requesting USER (not just the bot) to be a
        // member of it.
        if let Err(msg) = crate::adapters::messaging_routing::ensure_channel_read_access(
            surface.as_ref(),
            channel,
        )
        .await
        {
            return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        if oldest.is_some() || latest.is_some() {
            return match surface.read_history(channel, limit, oldest, latest).await {
                Ok(page) => {
                    let count = page.messages.len();
                    info!(
                        channel = %channel,
                        platform = %surface.platform(),
                        count = count,
                        truncated = page.truncated,
                        "History read"
                    );
                    let next_latest = page
                        .truncated
                        .then(|| page.messages.last().map(|m| m.timestamp.clone()))
                        .flatten();
                    let messages_json: Vec<serde_json::Value> =
                        page.messages.iter().map(channel_message_to_json).collect();
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "channel": channel,
                            "count": count,
                            "platform": surface.platform(),
                            "messages": messages_json,
                            "truncated": page.truncated,
                            "next_latest": next_latest,
                        }),
                        duration_ms,
                    ))
                }
                Err(e) => {
                    failure_result(
                        &self.surfaces,
                        &input,
                        channel,
                        "Failed to read history",
                        e,
                        duration_ms,
                    )
                    .await
                }
            };
        }

        match surface.read_messages(channel, limit, None).await {
            Ok(messages) => {
                let count = messages.len();
                info!(
                    channel = %channel,
                    platform = %surface.platform(),
                    count = count,
                    "Messages read"
                );
                let messages_json: Vec<serde_json::Value> =
                    messages.iter().map(channel_message_to_json).collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "channel": channel,
                        "count": count,
                        "platform": surface.platform(),
                        "messages": messages_json,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to read messages",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for editing a previously sent message.
pub struct MessagingEditTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingEditTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingEditTool {
    fn name(&self) -> &str {
        "messaging.edit"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Edit a message that was previously sent by the bot"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the message is"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message timestamp/ID to edit"
                },
                "text": {
                    "type": "string",
                    "description": "New message text"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel", "message_id", "text"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let message_id = input["message_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'message_id' parameter"))?;
        let text = input["text"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'text' parameter"))?;

        let text = if workforce_common::security_scanner::contains_credentials(text) {
            warn!(
                target: "sentinel",
                channel = %channel,
                "Credential detected in messaging.edit — redacting"
            );
            workforce_common::security_scanner::redact_credentials(text)
        } else {
            text.to_string()
        };

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.edit_message(channel, message_id, &text).await {
            Ok(()) => {
                info!(
                    channel = %channel,
                    message_id = %message_id,
                    platform = %surface.platform(),
                    "Message edited"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": channel,
                        "message_id": message_id,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to edit message",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for deleting a previously sent message.
pub struct MessagingDeleteTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingDeleteTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingDeleteTool {
    fn name(&self) -> &str {
        "messaging.delete"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Delete a message that was previously sent by the bot"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the message is"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message timestamp/ID to delete"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel", "message_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let message_id = input["message_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'message_id' parameter"))?;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.delete_message(channel, message_id).await {
            Ok(()) => {
                info!(
                    channel = %channel,
                    message_id = %message_id,
                    platform = %surface.platform(),
                    "Message deleted"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": channel,
                        "message_id": message_id,
                        "deleted": true,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to delete message",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for reading thread replies.
pub struct MessagingReadThreadTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingReadThreadTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingReadThreadTool {
    fn name(&self) -> &str {
        "messaging.read_thread"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "Read replies in a message thread. Returns the parent message plus all \
         replies. Each message includes a resolved `user_name` and `channel_name`; \
         refer to users and channels by those names rather than raw IDs."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the thread is"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread timestamp/ID (the parent message's timestamp)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of replies to read, integer (default 50, max 200)",
                    "default": 50
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel", "thread_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let thread_id = input["thread_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'thread_id' parameter"))?;
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(200) as usize;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.read_thread(channel, thread_id, limit).await {
            Ok(messages) => {
                let count = messages.len();
                info!(
                    channel = %channel,
                    thread_id = %thread_id,
                    platform = %surface.platform(),
                    count = count,
                    "Thread replies read"
                );
                let messages_json: Vec<serde_json::Value> =
                    messages.iter().map(channel_message_to_json).collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "channel": channel,
                        "thread_id": thread_id,
                        "count": count,
                        "platform": surface.platform(),
                        "messages": messages_json,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to read thread",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for pinning a message in a channel.
pub struct MessagingPinTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingPinTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingPinTool {
    fn name(&self) -> &str {
        "messaging.pin"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Pin a message in a channel so it stays easily accessible."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the message is"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message timestamp/ID to pin"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel", "message_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let message_id = input["message_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'message_id' parameter"))?;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.pin_message(channel, message_id).await {
            Ok(()) => {
                info!(
                    channel = %channel,
                    message_id = %message_id,
                    platform = %surface.platform(),
                    "Message pinned"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": channel,
                        "message_id": message_id,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to pin message",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for unpinning a message from a channel.
pub struct MessagingUnpinTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingUnpinTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingUnpinTool {
    fn name(&self) -> &str {
        "messaging.unpin"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Unpin a previously pinned message from a channel."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID where the message is pinned"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message timestamp/ID to unpin"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel", "message_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let message_id = input["message_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'message_id' parameter"))?;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.unpin_message(channel, message_id).await {
            Ok(()) => {
                info!(
                    channel = %channel,
                    message_id = %message_id,
                    platform = %surface.platform(),
                    "Message unpinned"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": channel,
                        "message_id": message_id,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to unpin message",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for listing pinned items in a channel.
pub struct MessagingListPinsTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingListPinsTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingListPinsTool {
    fn name(&self) -> &str {
        "messaging.list_pins"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "List all pinned messages in a channel."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to list pins from"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.list_pins(channel).await {
            Ok(pins) => {
                let count = pins.len();
                info!(
                    channel = %channel,
                    platform = %surface.platform(),
                    count = count,
                    "Listed pins"
                );
                let pins_json: Vec<serde_json::Value> = pins
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "message_id": p.message_id,
                            "text": p.text,
                            "user": p.user,
                            "pinned_at": p.pinned_at,
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "channel": channel,
                        "count": count,
                        "platform": surface.platform(),
                        "pins": pins_json,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to list pins",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Generic tool for looking up member/user info.
pub struct MessagingMemberInfoTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingMemberInfoTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingMemberInfoTool {
    fn name(&self) -> &str {
        "messaging.member_info"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "Look up information about a user/member by their ID"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "User ID to look up (platform-specific, e.g. Slack user ID)"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let user_id = input["user_id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'user_id' parameter"))?;

        let surface = resolve_for_workspace(&self.surfaces, &input).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.member_info(user_id).await {
            Ok(info) => {
                info!(
                    user_id = %user_id,
                    platform = %surface.platform(),
                    "Member info retrieved"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "user_id": info.user_id,
                        "display_name": info.display_name,
                        "real_name": info.real_name,
                        "title": info.title,
                        "timezone": info.timezone,
                        "status_text": info.status_text,
                        "status_emoji": info.status_emoji,
                        "avatar_url": info.avatar_url,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, user_id = %user_id, "Failed to get member info");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get member info"),
                    duration_ms,
                ))
            }
        }
    }
}

/// Generic tool for listing custom emoji in the workspace.
pub struct MessagingEmojiListTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingEmojiListTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingEmojiListTool {
    fn name(&self) -> &str {
        "messaging.emoji_list"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "List all custom emoji available in the workspace"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let surface = resolve_for_workspace(&self.surfaces, &input).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.emoji_list().await {
            Ok(emoji) => {
                let count = emoji.len();
                info!(
                    platform = %surface.platform(),
                    count = count,
                    "Emoji list retrieved"
                );
                let emoji_json: Vec<serde_json::Value> = emoji
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "name": e.name,
                            "value": e.value,
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "count": count,
                        "platform": surface.platform(),
                        "emoji": emoji_json,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list emoji");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list emoji"),
                    duration_ms,
                ))
            }
        }
    }
}

/// Generic tool for uploading files to a channel.
/// Reject content whose bytes plainly contradict the extension the model gave
/// it. The motivating case: a context-filtered model that can't reach
/// `export_xlsx` hand-writes CSV/text and uploads it as `report.xlsx`, which
/// Excel then refuses to open as "corrupt". Only formats with an unambiguous
/// magic number are checked; everything else passes through untouched. Returns
/// an actionable error message (with a recovery path) when the bytes don't
/// match, or `None` when the upload looks fine.
fn reject_extension_mismatch(filename: &str, content: &[u8]) -> Option<String> {
    let lower = filename.to_ascii_lowercase();
    // OOXML (xlsx/docx/pptx) are ZIP containers — they always start with "PK".
    if [".xlsx", ".docx", ".pptx"]
        .iter()
        .any(|ext| lower.ends_with(ext))
        && !content.starts_with(b"PK")
    {
        let ext = lower.rsplit('.').next().unwrap_or("xlsx");
        return Some(format!(
            "Refusing to upload '{filename}': these bytes are not a real .{ext} file (a valid \
             .{ext} is a ZIP that starts with 'PK', but this content does not). You have almost \
             certainly hand-written text/CSV and named it .{ext} — Office will report it as \
             corrupt. To produce a real spreadsheet, call the `export_xlsx` tool with your rows \
             as `data`; it builds the .xlsx and delivers it to this conversation for you. If you \
             don't see `export_xlsx`, load it with `tool_load` (names:['export_xlsx']) first. Do \
             NOT upload spreadsheet/document content through messaging.upload_file."
        ));
    }
    // PDFs start with "%PDF".
    if lower.ends_with(".pdf") && !content.starts_with(b"%PDF") {
        return Some(format!(
            "Refusing to upload '{filename}': these bytes are not a real PDF (a valid PDF starts \
             with '%PDF'). Generate the PDF with the appropriate tool rather than naming \
             arbitrary text '.pdf'."
        ));
    }
    None
}

pub struct MessagingUploadFileTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingUploadFileTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingUploadFileTool {
    fn name(&self) -> &str {
        "messaging.upload_file"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Upload a file to a Slack or Teams channel, or a Desk conversation. In a Desk \
         conversation, with no platform it posts to Desk: no channel is this conversation, \
         \"dm\" the person's DM, \"#<room name>\" one of their rooms."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to upload to. Required outside a Desk \
                                    conversation; in one it defaults to that conversation."
                },
                "filename": {
                    "type": "string",
                    "description": "Name for the uploaded file (e.g. 'report.csv')"
                },
                "content_text": {
                    "type": "string",
                    "description": "UTF-8 file body. Mutually exclusive with content_base64."
                },
                "content_base64": {
                    "type": "string",
                    "description": "Base64-encoded file body. Mutually exclusive with content_text. Whitespace (e.g. line wraps from the `base64` command) is tolerated."
                },
                "message": {
                    "type": "string",
                    "description": "Optional message to accompany the file"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread ID for uploading in a thread"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["filename"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel =
            channel_arg(&input).ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;
        let filename = input["filename"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'filename' parameter"))?;
        let message = input["message"].as_str();
        let thread_id = input["thread_id"].as_str();

        let content = match decode_upload_content(
            input["content_text"].as_str(),
            input["content_base64"].as_str(),
        ) {
            Ok(bytes) => bytes,
            Err(msg) => {
                warn!(channel = %channel, filename = %filename, error = %msg, "upload_file content rejected");
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        // Guard against the corrupt-spreadsheet failure mode: bytes that don't
        // match the named extension (e.g. CSV uploaded as `.xlsx`).
        if let Some(msg) = reject_extension_mismatch(filename, &content) {
            warn!(channel = %channel, filename = %filename, "upload_file rejected: extension/content mismatch");
            return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
        }

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface
            .upload_file(channel, filename, &content, message, thread_id)
            .await
        {
            Ok(permalink) => {
                info!(
                    channel = %channel,
                    filename = %filename,
                    platform = %surface.platform(),
                    "File uploaded"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "channel": channel,
                        "filename": filename,
                        "permalink": permalink,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to upload file",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

/// Send a previously staged artifact (a generated image, a viewed/downloaded
/// file, an export) to a channel or a user's DM. Delivers a file OUT of the
/// current conversation; allow by default, gate via policy if desired.
///
/// `agent_id` is captured at construction (per-agent, like `generate_image` and
/// the image sink) because the artifact store is partitioned by agent and a
/// tool runs in a spawned task that may not carry the `CURRENT_AGENT_ID`
/// task-local.
pub struct MessagingSendFileTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
    agent_id: String,
}

impl MessagingSendFileTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>, agent_id: impl Into<String>) -> Self {
        Self {
            surfaces,
            agent_id: agent_id.into(),
        }
    }
}

#[async_trait]
impl Tool for MessagingSendFileTool {
    fn name(&self) -> &str {
        "messaging.send_file"
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(writes(input))
    }

    fn description(&self) -> &str {
        "Send a file that is already staged as an artifact — e.g. the artifact_id from a \
         generate_image, view_attachment, or export_xlsx result — to a Slack channel, a user's \
         DM, or a Desk conversation. Use this to DM a generated image to someone, or post a file \
         in another channel. Provide exactly one of `channel` (a channel id) or `user` (a Slack \
         user id or an email to DM); in a Desk conversation, with no platform it posts to Desk: \
         no destination is this conversation, \"dm\" the person's DM, \"#<room name>\" one of \
         their rooms. Sending a file outside the current conversation asks the user for approval \
         first."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "Artifact id (art_…) of the staged file to send, from a \
                                    generate_image / view_attachment / export_xlsx result."
                },
                "channel": {
                    "type": "string",
                    "description": "Destination channel id. Mutually exclusive with `user`. In \
                                    a Desk conversation it defaults to that conversation."
                },
                "user": {
                    "type": "string",
                    "description": "Destination user to DM — a Slack user id (U…) or an email \
                                    address. Mutually exclusive with `channel`."
                },
                "filename": {
                    "type": "string",
                    "description": "Optional name for the delivered file; derived from the artifact \
                                    if omitted."
                },
                "message": {
                    "type": "string",
                    "description": "Optional message to accompany the file."
                },
                "thread_id": {
                    "type": "string",
                    "description": "Optional thread to post into (only honoured with `channel`)."
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;

        let Some(artifact_id) = input["artifact_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::failure(
                "send_file requires 'artifact_id'",
                ms(),
            ));
        };
        let user_in = input["user"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let channel_in = input["channel"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| user_in.is_none().then(|| channel_arg(&input)).flatten());
        if channel_in.is_some() == user_in.is_some() {
            return Ok(ToolResult::failure(
                "send_file needs exactly one destination: 'channel' or 'user'",
                ms(),
            ));
        }

        let staged = match crate::adapters::utils::resolve_staged_file(&self.agent_id, artifact_id)
        {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("could not read artifact {artifact_id}: {e}"),
                    ms(),
                ))
            }
        };
        let filename = input["filename"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(staged.filename);
        let message = input["message"].as_str();

        // Resolve the destination surface + channel (a channel id, or a DM
        // opened against a user id / email).
        let (surface, dest_channel, thread_id, dest_label) = if let Some(channel) = channel_in {
            let surface = match resolve_for_channel(&self.surfaces, &input, channel).await {
                Ok(s) => s,
                Err(e) => return Ok(ToolResult::failure(format!("{e}"), ms())),
            };
            let thread = input["thread_id"].as_str().map(str::to_string);
            (
                surface,
                channel.to_string(),
                thread,
                format!("channel {channel}"),
            )
        } else {
            let user = user_in.expect("exactly one of channel/user is set");
            let platform = platform_arg(&input);
            let user_id = if user.contains('@') {
                let lookup = match resolve_for_workspace(&self.surfaces, &input).await {
                    Ok(s) => s,
                    Err(e) => return Ok(ToolResult::failure(format!("{e}"), ms())),
                };
                match lookup.lookup_user_id_by_email(user).await {
                    Ok(id) => id,
                    Err(e) => {
                        return Ok(ToolResult::from_report(
                            &e.wrap_err(format!("could not resolve user '{user}'")),
                            ms(),
                        ))
                    }
                }
            } else {
                user.to_string()
            };
            let resolved = {
                let registry = self.surfaces.read().await;
                registry
                    .resolve_user_dm(platform, &user_id, channel_hint_from_input(&input))
                    .await
            };
            match resolved {
                Ok((surface, dm)) => (surface, dm, None, format!("DM to {user}")),
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err(format!("could not open a DM with '{user}'")),
                        ms(),
                    ))
                }
            }
        };

        match surface
            .upload_file(
                &dest_channel,
                &filename,
                &staged.bytes,
                message,
                thread_id.as_deref(),
            )
            .await
        {
            Ok(permalink) => {
                info!(
                    artifact = %artifact_id,
                    dest = %dest_label,
                    filename = %filename,
                    platform = %surface.platform(),
                    "Artifact sent to destination"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "ok",
                        "destination": dest_label,
                        "channel": dest_channel,
                        "filename": filename,
                        "permalink": permalink,
                        "platform": surface.platform(),
                    }),
                    ms(),
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    &dest_channel,
                    "Failed to send file",
                    e,
                    ms(),
                )
                .await
            }
        }
    }
}

/// Generic tool for looking up channel information.
pub struct MessagingChannelInfoTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl MessagingChannelInfoTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for MessagingChannelInfoTool {
    fn name(&self) -> &str {
        "messaging.channel_info"
    }

    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        Some(SurfaceEffect::Reads)
    }

    fn description(&self) -> &str {
        "Look up information about a channel by its ID"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ID to look up"
                },
                "platform": schema_platform_property(),
                "workspace_id": schema_workspace_property()
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let channel = input["channel"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing 'channel' parameter"))?;

        let surface = resolve_for_channel(&self.surfaces, &input, channel).await?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match surface.channel_info(channel).await {
            Ok(info) => {
                info!(
                    channel_id = %info.channel_id,
                    display_name = %info.display_name,
                    platform = %surface.platform(),
                    "Channel info retrieved"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "channel_id": info.channel_id,
                        "display_name": info.display_name,
                        "platform": surface.platform(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                failure_result(
                    &self.surfaces,
                    &input,
                    channel,
                    "Failed to get channel info",
                    e,
                    duration_ms,
                )
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::{
        ChannelCapabilities, ChannelInfo, ChannelMessage, CustomEmoji, MemberInfo, PinnedItem,
        Reaction, SentMessage, SurfaceKey,
    };

    struct MockSurface {
        platform_name: String,
    }

    impl MockSurface {
        fn new(platform: &str) -> Arc<Self> {
            Arc::new(Self {
                platform_name: platform.to_string(),
            })
        }
    }

    #[async_trait]
    impl ChannelSurface for MockSurface {
        fn platform(&self) -> &str {
            &self.platform_name
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                reactions: false,
                reaction_removal: false,
                threads: true,
                rich_text: true,
                send_message: true,
                read_messages: true,
                edit_message: true,
                delete_message: true,
                read_thread: true,
                pins: true,
                member_info: true,
                emoji_list: true,
                file_upload: true,
                channel_info: true,
            }
        }

        async fn add_reaction(&self, _reaction: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn remove_reaction(&self, _reaction: &Reaction) -> Result<()> {
            Ok(())
        }

        async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
            Ok(SentMessage {
                channel: message.channel.clone(),
                message_id: "mock-ts".to_string(),
                thread_id: message.thread_id.clone(),
                platform: self.platform_name.clone(),
            })
        }

        async fn read_messages(
            &self,
            channel: &str,
            _limit: usize,
            _oldest: Option<&str>,
        ) -> Result<Vec<ChannelMessage>> {
            Ok(vec![ChannelMessage {
                channel: channel.to_string(),
                message_id: "1".to_string(),
                user: "u".to_string(),
                user_name: None,
                channel_name: None,
                text: "msg".to_string(),
                timestamp: "1".to_string(),
                thread_id: None,
                files: vec![],
                bot_id: None,
            }])
        }

        async fn edit_message(&self, _channel: &str, _message_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }

        async fn delete_message(&self, _channel: &str, _message_id: &str) -> Result<()> {
            Ok(())
        }

        async fn read_thread(
            &self,
            channel: &str,
            thread_ts: &str,
            _limit: usize,
        ) -> Result<Vec<ChannelMessage>> {
            Ok(vec![
                ChannelMessage {
                    channel: channel.to_string(),
                    message_id: thread_ts.to_string(),
                    user: "u1".to_string(),
                    user_name: None,
                    channel_name: None,
                    text: "parent message".to_string(),
                    timestamp: thread_ts.to_string(),
                    thread_id: Some(thread_ts.to_string()),
                    files: vec![],
                    bot_id: None,
                },
                ChannelMessage {
                    channel: channel.to_string(),
                    message_id: "200.002".to_string(),
                    user: "u2".to_string(),
                    user_name: None,
                    channel_name: None,
                    text: "reply".to_string(),
                    timestamp: "200.002".to_string(),
                    thread_id: Some(thread_ts.to_string()),
                    files: vec![],
                    bot_id: None,
                },
            ])
        }

        async fn pin_message(&self, _channel: &str, _message_id: &str) -> Result<()> {
            Ok(())
        }

        async fn unpin_message(&self, _channel: &str, _message_id: &str) -> Result<()> {
            Ok(())
        }

        async fn list_pins(&self, channel: &str) -> Result<Vec<PinnedItem>> {
            Ok(vec![PinnedItem {
                channel: channel.to_string(),
                text: "pinned message".to_string(),
                message_id: "300.001".to_string(),
                user: "u1".to_string(),
                pinned_at: Some(1700000000),
            }])
        }

        async fn member_info(&self, user_id: &str) -> Result<MemberInfo> {
            Ok(MemberInfo {
                user_id: user_id.to_string(),
                display_name: "Test User".to_string(),
                real_name: "Test R. User".to_string(),
                title: "Engineer".to_string(),
                timezone: Some("America/New_York".to_string()),
                status_text: Some("Working".to_string()),
                status_emoji: Some(":computer:".to_string()),
                avatar_url: Some("https://example.com/avatar.png".to_string()),
            })
        }

        async fn emoji_list(&self) -> Result<Vec<CustomEmoji>> {
            Ok(vec![
                CustomEmoji {
                    name: "partyparrot".to_string(),
                    value: "https://example.com/partyparrot.gif".to_string(),
                },
                CustomEmoji {
                    name: "shipit".to_string(),
                    value: "https://example.com/shipit.png".to_string(),
                },
            ])
        }

        async fn upload_file(
            &self,
            _channel: &str,
            filename: &str,
            _content: &[u8],
            _message: Option<&str>,
            _thread_id: Option<&str>,
        ) -> Result<String> {
            Ok(format!("https://example.com/files/{}", filename))
        }

        async fn channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
            Ok(ChannelInfo {
                channel_id: channel_id.to_string(),
                display_name: format!("#{}", channel_id),
            })
        }
    }

    fn make_registry_with_surface(platform: &str) -> Arc<RwLock<SurfaceRegistry>> {
        let mut registry = SurfaceRegistry::new();
        let surface = MockSurface::new(platform) as Arc<dyn ChannelSurface>;
        registry.register(SurfaceKey::workspace(platform, "test-ws"), surface);
        Arc::new(RwLock::new(registry))
    }

    #[test]
    fn messaging_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        assert_eq!(tool.name(), "messaging.send");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["text"]));
    }

    #[tokio::test]
    async fn messaging_send_auto_detects_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "hello"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["platform"], "slack");
        assert_eq!(result.output["channel"], "C123");
    }

    #[tokio::test]
    async fn messaging_send_explicit_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "hello",
            "platform": "slack"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["platform"], "slack");
    }

    #[tokio::test]
    async fn messaging_send_with_thread_id() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "reply",
            "thread_id": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["thread_id"], "100.001");
    }

    #[tokio::test]
    async fn messaging_send_unknown_platform_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "hello",
            "platform": "discord"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("discord"));
    }

    #[tokio::test]
    async fn messaging_send_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "hello"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_send_deduplicates_identical_repeat() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "text": "Navigated to example.com. Now waiting for the login form…"
        });

        let first = tool.execute(input.clone()).await.unwrap();
        assert!(first.success);
        assert_eq!(first.output["status"], "ok");

        let second = tool.execute(input).await.unwrap();
        assert!(second.success);
        assert_eq!(second.output["status"], "deduplicated");
        assert!(second.output["note"]
            .as_str()
            .unwrap()
            .contains("identical"));
    }

    #[tokio::test]
    async fn messaging_send_dedup_distinct_text_passes_through() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let first = tool
            .execute(serde_json::json!({ "channel": "C123", "text": "first" }))
            .await
            .unwrap();
        let second = tool
            .execute(serde_json::json!({ "channel": "C123", "text": "second" }))
            .await
            .unwrap();
        assert_eq!(first.output["status"], "ok");
        assert_eq!(second.output["status"], "ok");
    }

    #[tokio::test]
    async fn messaging_send_dedup_distinct_channel_passes_through() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let first = tool
            .execute(serde_json::json!({ "channel": "C123", "text": "same" }))
            .await
            .unwrap();
        let second = tool
            .execute(serde_json::json!({ "channel": "C456", "text": "same" }))
            .await
            .unwrap();
        assert_eq!(first.output["status"], "ok");
        assert_eq!(second.output["status"], "ok");
    }

    #[tokio::test]
    async fn messaging_send_dedup_distinct_thread_passes_through() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let first = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "text": "same",
                "thread_id": "100.001",
            }))
            .await
            .unwrap();
        let second = tool
            .execute(serde_json::json!({
                "channel": "C123",
                "text": "same",
                "thread_id": "100.002",
            }))
            .await
            .unwrap();
        assert_eq!(first.output["status"], "ok");
        assert_eq!(second.output["status"], "ok");
    }

    #[test]
    fn messaging_send_description_discourages_progress_narration() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let desc = tool.description();
        assert!(desc.contains("streaming response"));
        assert!(desc.contains("Do NOT"));
        assert!(desc.contains("deduplicated"));
    }

    #[tokio::test]
    async fn messaging_send_missing_channel_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({"text": "hello"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_send_missing_text_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendTool::new(registry);
        let input = serde_json::json!({"channel": "C123"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingReadTool tests ───────────────────────────────────────

    #[test]
    fn read_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        assert_eq!(tool.name(), "messaging.read");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["channel"]));
    }

    #[tokio::test]
    async fn messaging_read_auto_detects_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["platform"], "slack");
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["count"], 1);
    }

    #[tokio::test]
    async fn messaging_read_explicit_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "platform": "slack"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["platform"], "slack");
    }

    #[tokio::test]
    async fn messaging_read_with_limit() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "limit": 5
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn messaging_read_unknown_platform_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "platform": "discord"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("discord"));
    }

    #[tokio::test]
    async fn messaging_read_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_read_missing_channel_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadTool::new(registry);
        let input = serde_json::json!({ "limit": 10 });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingEditTool tests ───────────────────────────────────────

    #[test]
    fn edit_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEditTool::new(registry);
        assert_eq!(tool.name(), "messaging.edit");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "message_id", "text"])
        );
    }

    #[tokio::test]
    async fn messaging_edit_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEditTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001",
            "text": "updated text"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["message_id"], "100.001");
    }

    #[tokio::test]
    async fn messaging_edit_explicit_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEditTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001",
            "text": "updated",
            "platform": "slack"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["platform"], "slack");
    }

    #[tokio::test]
    async fn messaging_edit_missing_text_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEditTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_edit_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingEditTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001",
            "text": "updated"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingDeleteTool tests ─────────────────────────────────────

    #[test]
    fn delete_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingDeleteTool::new(registry);
        assert_eq!(tool.name(), "messaging.delete");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "message_id"])
        );
    }

    #[tokio::test]
    async fn messaging_delete_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingDeleteTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["deleted"], true);
    }

    #[tokio::test]
    async fn messaging_delete_explicit_platform() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingDeleteTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001",
            "platform": "slack"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn messaging_delete_missing_message_id_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingDeleteTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_delete_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingDeleteTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingReadThreadTool tests ─────────────────────────────────

    #[test]
    fn read_thread_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadThreadTool::new(registry);
        assert_eq!(tool.name(), "messaging.read_thread");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "thread_id"])
        );
    }

    #[tokio::test]
    async fn messaging_read_thread_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadThreadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "thread_id": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["thread_id"], "100.001");
        assert_eq!(result.output["count"], 2);
        let messages = result.output["messages"].as_array().unwrap();
        assert_eq!(messages[0]["text"], "parent message");
        assert_eq!(messages[1]["text"], "reply");
    }

    #[tokio::test]
    async fn messaging_read_thread_with_limit() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadThreadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "thread_id": "100.001",
            "limit": 10
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn messaging_read_thread_missing_thread_id_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingReadThreadTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_read_thread_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingReadThreadTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "thread_id": "100.001"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingPinTool tests ───────────────────────────────────────

    #[test]
    fn pin_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingPinTool::new(registry);
        assert_eq!(tool.name(), "messaging.pin");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "message_id"])
        );
    }

    #[tokio::test]
    async fn messaging_pin_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingPinTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["message_id"], "100.001");
    }

    #[tokio::test]
    async fn messaging_pin_missing_message_id_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingPinTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_pin_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingPinTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingUnpinTool tests ─────────────────────────────────────

    #[test]
    fn unpin_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUnpinTool::new(registry);
        assert_eq!(tool.name(), "messaging.unpin");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["channel", "message_id"])
        );
    }

    #[tokio::test]
    async fn messaging_unpin_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUnpinTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
    }

    #[tokio::test]
    async fn messaging_unpin_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingUnpinTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "message_id": "100.001"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingListPinsTool tests ──────────────────────────────────

    #[test]
    fn list_pins_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingListPinsTool::new(registry);
        assert_eq!(tool.name(), "messaging.list_pins");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["channel"]));
    }

    #[tokio::test]
    async fn messaging_list_pins_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingListPinsTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["count"], 1);
        let pins = result.output["pins"].as_array().unwrap();
        assert_eq!(pins[0]["text"], "pinned message");
        assert_eq!(pins[0]["message_id"], "300.001");
    }

    #[tokio::test]
    async fn messaging_list_pins_missing_channel_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingListPinsTool::new(registry);
        let input = serde_json::json!({});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_list_pins_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingListPinsTool::new(registry);
        let input = serde_json::json!({ "channel": "C123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingMemberInfoTool tests ────────────────────────────────

    #[test]
    fn member_info_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingMemberInfoTool::new(registry);
        assert_eq!(tool.name(), "messaging.member_info");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["user_id"]));
    }

    #[tokio::test]
    async fn messaging_member_info_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingMemberInfoTool::new(registry);
        let input = serde_json::json!({ "user_id": "U123" });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["user_id"], "U123");
        assert_eq!(result.output["display_name"], "Test User");
        assert_eq!(result.output["real_name"], "Test R. User");
        assert_eq!(result.output["title"], "Engineer");
        assert_eq!(result.output["timezone"], "America/New_York");
    }

    #[tokio::test]
    async fn messaging_member_info_missing_user_id_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingMemberInfoTool::new(registry);
        let input = serde_json::json!({});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_member_info_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingMemberInfoTool::new(registry);
        let input = serde_json::json!({ "user_id": "U123" });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingEmojiListTool tests ─────────────────────────────────

    #[test]
    fn emoji_list_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEmojiListTool::new(registry);
        assert_eq!(tool.name(), "messaging.emoji_list");
    }

    #[tokio::test]
    async fn messaging_emoji_list_success() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingEmojiListTool::new(registry);
        let input = serde_json::json!({});
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 2);
        let emoji = result.output["emoji"].as_array().unwrap();
        assert_eq!(emoji[0]["name"], "partyparrot");
        assert_eq!(emoji[1]["name"], "shipit");
    }

    #[tokio::test]
    async fn messaging_emoji_list_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingEmojiListTool::new(registry);
        let input = serde_json::json!({});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingUploadFileTool tests ────────────────────────────────

    #[test]
    fn upload_file_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        assert_eq!(tool.name(), "messaging.upload_file");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["filename"]));
    }

    #[tokio::test]
    async fn messaging_upload_file_success() {
        use base64::Engine;
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello world");
        let input = serde_json::json!({
            "channel": "C123",
            "filename": "test.txt",
            "content_base64": content_b64
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["channel"], "C123");
        assert_eq!(result.output["filename"], "test.txt");
        assert_eq!(
            result.output["permalink"],
            "https://example.com/files/test.txt"
        );
    }

    #[tokio::test]
    async fn messaging_upload_file_content_text_succeeds() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "filename": "test.txt",
            "content_text": "hello world"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.output["filename"], "test.txt");
    }

    #[tokio::test]
    async fn messaging_upload_file_rejected_content_returns_failure() {
        // Content-resolution errors (here: invalid base64) surface as a tool
        // failure, never a hard `Err`. Exhaustive input validation lives in
        // `utils::decode_upload_content` tests.
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "filename": "test.txt",
            "content_base64": "not-valid-base64!!!"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("not valid base64"));
    }

    #[test]
    fn reject_extension_mismatch_catches_csv_as_xlsx() {
        // The exact failure: CSV bytes named .xlsx are rejected and the error
        // points the model at the real tool.
        let csv = b"Ticket ID,Title\nSTA-1,Hello\n";
        let msg = reject_extension_mismatch("tickets.xlsx", csv).expect("CSV-as-xlsx must reject");
        assert!(msg.contains("export_xlsx"));
        assert!(msg.contains("tool_load"));

        // Case-insensitive on the extension; docx/pptx covered too.
        assert!(reject_extension_mismatch("Report.XLSX", csv).is_some());
        assert!(reject_extension_mismatch("notes.docx", b"plain text").is_some());
        assert!(reject_extension_mismatch("deck.pptx", b"plain text").is_some());
    }

    #[test]
    fn reject_extension_mismatch_allows_genuine_and_unknown() {
        // Real OOXML zip (PK magic) named .xlsx passes.
        assert!(reject_extension_mismatch("real.xlsx", b"PK\x03\x04rest-of-zip").is_none());
        // Real PDF passes; text-as-pdf rejected.
        assert!(reject_extension_mismatch("doc.pdf", b"%PDF-1.7\n...").is_none());
        assert!(reject_extension_mismatch("doc.pdf", b"not a pdf").is_some());
        // Formats without an enforced magic number pass straight through.
        assert!(reject_extension_mismatch("data.csv", b"a,b\n1,2\n").is_none());
        assert!(reject_extension_mismatch("notes.txt", b"hello").is_none());
    }

    #[tokio::test]
    async fn messaging_upload_file_csv_as_xlsx_rejected() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "filename": "contoso-tickets.xlsx",
            "content_text": "Ticket ID,Title\nSTA-1,Hello\n"
        });
        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("export_xlsx"));
    }

    #[tokio::test]
    async fn messaging_upload_file_missing_filename_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingUploadFileTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "content_base64": "aGVsbG8="
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn messaging_upload_file_no_surfaces_fails() {
        let registry = Arc::new(RwLock::new(SurfaceRegistry::new()));
        let tool = MessagingUploadFileTool::new(registry);
        let input = serde_json::json!({
            "channel": "C123",
            "filename": "test.txt",
            "content_base64": "aGVsbG8="
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    // ── MessagingSendFileTool tests ──────────────────────────────────

    #[test]
    fn send_file_tool_name_and_schema() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendFileTool::new(registry, "test-agent");
        assert_eq!(tool.name(), "messaging.send_file");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["artifact_id"]));
    }

    #[tokio::test]
    async fn messaging_send_file_missing_artifact_id_fails() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendFileTool::new(registry, "test-agent");
        let result = tool
            .execute(serde_json::json!({ "channel": "C123" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn messaging_send_file_requires_exactly_one_destination() {
        let registry = make_registry_with_surface("slack");
        let tool = MessagingSendFileTool::new(registry, "test-agent");
        // Both channel and user → rejected (before touching the store).
        let both = tool
            .execute(serde_json::json!({
                "artifact_id": "art_x",
                "channel": "C123",
                "user": "U999"
            }))
            .await
            .unwrap();
        assert!(!both.success);
        // Neither channel nor user → rejected.
        let neither = tool
            .execute(serde_json::json!({ "artifact_id": "art_x" }))
            .await
            .unwrap();
        assert!(!neither.success);
    }

    /// The cross-channel read guard: current conversation and system/CLI runs
    /// pass; an actual user turn that can't be verified fails closed.
    #[tokio::test]
    async fn cross_channel_read_guard_fails_closed() {
        use crate::adapters::messaging_routing::ensure_channel_read_access;
        use workforce_agent::runner::{
            CURRENT_CHANNEL_ID, CURRENT_PLATFORM_USER_ID, CURRENT_USER_ID,
        };

        let surface = MockSurface::new("slack");
        let s: &dyn ChannelSurface = surface.as_ref();

        // Current conversation → always allowed (no membership lookup).
        assert!(CURRENT_CHANNEL_ID
            .scope("C-cur".into(), ensure_channel_read_access(s, "C-cur"))
            .await
            .is_ok());

        // No requesting human in scope (CLI / scheduled / system) → allowed.
        assert!(ensure_channel_read_access(s, "C-other").await.is_ok());

        // A user turn with no resolvable platform id → fail closed.
        assert!(CURRENT_USER_ID
            .scope("uid".into(), ensure_channel_read_access(s, "C-other"))
            .await
            .is_err());

        // A user turn whose membership the surface can't verify (MockSurface
        // uses the default `is_channel_member` error) → fail closed.
        assert!(CURRENT_USER_ID
            .scope(
                "uid".into(),
                CURRENT_PLATFORM_USER_ID
                    .scope("Uxyz".into(), ensure_channel_read_access(s, "C-other")),
            )
            .await
            .is_err());
    }

    /// In a Desk conversation messaging.send needs no channel and posts to
    /// Desk; anywhere else a channel is still required.
    #[tokio::test]
    async fn a_send_with_no_channel_goes_to_the_desk_conversation_and_fails_elsewhere() {
        use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
        let mut registry = SurfaceRegistry::new();
        registry.register(SurfaceKey::platform("slack"), MockSurface::new("slack"));
        registry.register_session_surface("desk", MockSurface::new("desk"));
        let tool = MessagingSendTool::new(Arc::new(RwLock::new(registry)));
        assert_eq!(tool.input_schema()["required"], serde_json::json!(["text"]));

        let outside = tool
            .execute(serde_json::json!({"text": "hello there"}))
            .await
            .unwrap_err();
        assert!(outside.to_string().contains("Missing 'channel' parameter"));

        let scope = DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
            attended: true,
            owner: "person-1".into(),
        };
        let on_desk = workforce_agent::runner::with_desk(
            Some(scope),
            tool.execute(serde_json::json!({"text": "hello again"})),
        )
        .await
        .unwrap();
        assert!(on_desk.success, "{on_desk:?}");
        assert_eq!(on_desk.output["platform"], "desk");
        assert_eq!(
            tool.surface_effect(&serde_json::json!({"channel": "C1", "platform": "slack"})),
            Some(SurfaceEffect::Writes {
                platform: "slack".into(),
                destination: "C1".into()
            })
        );
    }

    /// A file uploaded with no channel goes to the Desk conversation the
    /// run answers to; anywhere else a channel is still required.
    #[tokio::test]
    async fn an_upload_with_no_channel_goes_to_the_desk_conversation_and_fails_elsewhere() {
        use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
        let mut registry = SurfaceRegistry::new();
        registry.register(SurfaceKey::platform("slack"), MockSurface::new("slack"));
        registry.register_session_surface("desk", MockSurface::new("desk"));
        let tool = MessagingUploadFileTool::new(Arc::new(RwLock::new(registry)));
        let upload = || serde_json::json!({"filename": "notes.txt", "content_text": "hello"});

        let outside = tool.execute(upload()).await.unwrap_err();
        assert!(outside.to_string().contains("Missing 'channel' parameter"));

        let scope = DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
            attended: true,
            owner: "person-1".into(),
        };
        let on_desk = workforce_agent::runner::with_desk(Some(scope), tool.execute(upload()))
            .await
            .unwrap();
        assert!(on_desk.success, "{on_desk:?}");
        assert_eq!(
            (&on_desk.output["platform"], &on_desk.output["channel"]),
            (&serde_json::json!("desk"), &serde_json::json!(""))
        );
    }
}

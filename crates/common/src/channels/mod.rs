//! Channel surface abstraction and identifier parsing for messaging platforms.
//!
//! Each messaging platform (Slack, Discord, Telegram) implements the
//! [`ChannelSurface`] trait, providing a uniform interface for sending
//! messages, reacting with emoji, and other platform-specific actions.
//!
//! Channels in Workforce use a "platform:id" format, e.g. "slack:C0123ABC".
//! This module provides helpers for parsing and constructing channel identifiers.

pub mod route_cache;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

pub use route_cache::ChannelRouteCache;

// ── Channel identifier parsing ──────────────────────────────────────────────

/// Parse a channel string into (platform, channel_id).
///
/// Channels use the format "platform:id" (e.g. "slack:C0123ABC", "ws:default").
/// If no colon is present, returns `("unknown", input)`.
pub fn parse_channel(channel: &str) -> (&str, &str) {
    channel.split_once(':').unwrap_or(("unknown", channel))
}

/// Check if a channel belongs to a given platform.
pub fn is_platform(channel: &str, platform: &str) -> bool {
    parse_channel(channel).0 == platform
}

/// Extract the platform-specific channel ID.
pub fn channel_id(channel: &str) -> &str {
    parse_channel(channel).1
}

pub use workforce_core::desk;
pub use workforce_core::desk::{is_desk_conversation, is_desk_room, DESK_CHANNEL};

/// The gateway's persistent one-chat-per-user channels: the iOS app, the web
/// UI, the terminal and the desktop app, each a screen the reply is read on.
pub const CONVERSATIONAL_CHANNELS: &[&str] = &["ios", "web", "terminal", DESK_CHANNEL];

/// True for channels representing a direct human-driven conversation — one of
/// [`CONVERSATIONAL_CHANNELS`] or a desktop app room, as opposed to
/// platform-scoped (`slack:…`) or internal (`workflow:…`, `scheduler:…`)
/// sessions. These are the channels where stale-resume rollover applies.
pub fn is_conversational(channel: &str) -> bool {
    CONVERSATIONAL_CHANNELS.contains(&channel) || is_desk_room(channel)
}

// ── Channel surface trait ───────────────────────────────────────────────────

/// A reaction to add or remove from a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    /// Channel identifier (platform-specific, e.g. Slack channel ID).
    pub channel: String,
    /// Message identifier (platform-specific, e.g. Slack timestamp).
    pub message_id: String,
    /// Emoji to react with. Accepts multiple formats:
    /// - Slack names: `thumbsup`, `eyes`, `white_check_mark`
    /// - Unicode: `👍`, `👀`, `✅`
    /// - Shortcodes: `:thumbsup:`, `:eyes:`
    pub emoji: String,
}

/// A message to send to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Channel identifier (platform-specific, e.g. Slack channel ID).
    pub channel: String,
    /// Message text. On platforms that support rich blocks, this serves as the
    /// notification/fallback text.
    pub text: String,
    /// Thread identifier for replies (e.g. Slack thread_ts).
    pub thread_id: Option<String>,
    /// Optional display name override (e.g., for team agent identity).
    #[serde(default)]
    pub username: Option<String>,
    /// Optional icon URL override.
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Optional Slack Block Kit payload. When present on a Slack surface,
    /// the blocks are sent alongside `text` (which becomes the notification
    /// fallback) and rendered as the primary message body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<serde_json::Value>>,
}

impl OutboundMessage {
    /// A plain top-level text message with no identity or block overrides.
    pub fn text(channel: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            text: text.into(),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: None,
        }
    }
}

/// A message that was successfully sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentMessage {
    /// Channel the message was sent to.
    pub channel: String,
    /// Platform-specific message identifier.
    pub message_id: String,
    /// Thread identifier (if sent as a threaded reply).
    pub thread_id: Option<String>,
    /// Platform name.
    pub platform: String,
}

/// A message read from a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    /// Channel identifier.
    pub channel: String,
    /// Platform-specific message identifier.
    pub message_id: String,
    /// User who sent the message.
    pub user: String,
    /// Sender's resolved display name when the surface looked it up.
    /// `None` when the surface either doesn't support lookup or the
    /// user is unreachable (deleted account, external Slack Connect
    /// user, member of a workspace this agent isn't paired with).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// Channel's resolved display name (`#engineering`, `DM with Allan`,
    /// `Group DM`). `None` under the same conditions as `user_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    /// Message text. When the surface populates `user_name` / `channel_name`
    /// the body is also rewritten in place: `<@U…>` → `@DisplayName`,
    /// `<#C…|name>` → `#name`, link/group/broadcast tokens are unwrapped to
    /// their human-readable form.
    pub text: String,
    /// Timestamp string.
    pub timestamp: String,
    /// Thread identifier (if in a thread).
    pub thread_id: Option<String>,
    /// Files attached to the message. Empty when the surface doesn't report
    /// attachments or the message has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ChannelFile>,
    /// Platform bot identifier when the message was authored by a bot/app.
    /// `None` for human messages. Retained so the monitor loop guard can tell
    /// the bot's own output apart on the catch-up read path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
}

/// One bounded walk of a channel's history, as returned by
/// [`ChannelSurface::read_history`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    /// Messages newest-first, in the platform's native read order.
    pub messages: Vec<ChannelMessage>,
    /// True when the per-call page budget ran out before the requested range
    /// was exhausted. Resume by calling again with `latest` set to the oldest
    /// returned message's timestamp.
    pub truncated: bool,
}

/// A surface-agnostic inbound message — the single normalized event that both
/// the live path (a channel adapter's event handler) and the catch-up read path
/// feed into the channel-monitor matcher. `is_self` is intentionally not carried
/// here: it is derived where the per-workspace bot identity is known, so both
/// paths compute the loop guard identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEvent {
    /// Surface that produced the event (e.g. "slack").
    pub surface: String,
    /// Per-workspace routing key / bot-identity scope.
    pub workspace: String,
    /// Channel the message landed in.
    pub channel: String,
    /// Author user id, when known.
    pub user: Option<String>,
    /// Author bot id when the message came from a bot/app; also the self-check input.
    pub bot_id: Option<String>,
    /// Message text.
    pub text: String,
    /// Message id / timestamp.
    pub ts: String,
    /// Thread id when the message is a reply.
    pub thread_ts: Option<String>,
    /// Whether the message carried any file attachments.
    pub has_files: bool,
}

impl InboundEvent {
    /// Whether the message was authored by any bot/app.
    pub fn is_bot(&self) -> bool {
        self.bot_id.is_some()
    }

    /// Whether the message is a reply *inside* a thread, as opposed to a
    /// top-level channel message. A thread's root carries `thread_ts == ts` in a
    /// read-back (catch-up) history, while a true reply carries the parent's ts
    /// (≠ its own), so this excludes only replies and keeps the root — the
    /// formulation is identical on the live and sweep paths.
    pub fn is_thread_reply(&self) -> bool {
        self.thread_ts.as_deref().is_some_and(|t| t != self.ts)
    }
}

/// Marker a surface attaches to a read error when its bot is not a member of
/// the channel (or the channel is gone), so callers can tell "invite the bot"
/// from a transport failure without parsing platform error strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAMember {
    pub channel: String,
}

impl std::fmt::Display for NotAMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the bot is not a member of channel {}", self.channel)
    }
}

impl std::error::Error for NotAMember {}

impl NotAMember {
    /// Whether `err` (anywhere in its chain) is a membership error.
    pub fn is_cause_of(err: &eyre::Report) -> bool {
        err.chain().any(|cause| cause.is::<NotAMember>())
    }
}

/// A file attached to a [`ChannelMessage`]. Carries enough metadata to display
/// the attachment and fetch its bytes via [`ChannelSurface::fetch_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelFile {
    /// Platform-specific file identifier.
    pub id: String,
    /// Original filename.
    pub name: String,
    /// MIME type (e.g. `image/png`).
    pub mime_type: String,
    /// Authenticated download URL. `None` when the surface only exposes
    /// metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// File size in bytes when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ChannelFile {
    /// Whether this file is an image based on its MIME type.
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }
}

/// A pinned item in a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedItem {
    /// Channel the item is pinned in.
    pub channel: String,
    /// Message text.
    pub text: String,
    /// Message timestamp.
    pub message_id: String,
    /// User who sent the pinned message.
    pub user: String,
    /// When the item was pinned (Unix timestamp).
    pub pinned_at: Option<i64>,
}

/// Information about a channel member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Platform-specific user ID.
    pub user_id: String,
    /// Display name.
    pub display_name: String,
    /// Real/full name.
    pub real_name: String,
    /// Job title.
    pub title: String,
    /// Timezone identifier.
    pub timezone: Option<String>,
    /// Status text.
    pub status_text: Option<String>,
    /// Status emoji.
    pub status_emoji: Option<String>,
    /// Avatar/profile image URL.
    pub avatar_url: Option<String>,
}

/// A custom emoji entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomEmoji {
    /// Emoji name (without colons).
    pub name: String,
    /// URL of the emoji image, or alias target.
    pub value: String,
}

/// Information about a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Platform-specific channel ID.
    pub channel_id: String,
    /// Human-readable display name (e.g., \"#engineering\", \"DM with Allan\", \"Group DM\").
    pub display_name: String,
}

/// Capabilities that a channel surface can support.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelCapabilities {
    /// Whether this channel supports reactions.
    pub reactions: bool,
    /// Whether reactions can be removed.
    pub reaction_removal: bool,
    /// Whether this channel supports threading.
    pub threads: bool,
    /// Whether this channel supports rich text / markdown.
    pub rich_text: bool,
    /// Whether this channel supports sending messages.
    pub send_message: bool,
    /// Whether this channel supports reading message history.
    pub read_messages: bool,
    /// Whether this channel supports editing sent messages.
    pub edit_message: bool,
    /// Whether this channel supports deleting sent messages.
    pub delete_message: bool,
    /// Whether this channel supports reading thread replies.
    pub read_thread: bool,
    /// Whether this channel supports pinning messages.
    pub pins: bool,
    /// Whether this channel supports member info lookup.
    pub member_info: bool,
    /// Whether this channel supports listing custom emoji.
    pub emoji_list: bool,
    /// Whether this channel supports file uploads.
    pub file_upload: bool,
    /// Whether this channel supports channel info lookup.
    pub channel_info: bool,
}

/// Trait for messaging platform channel surfaces.
///
/// Each platform integration implements this trait to provide
/// a consistent interface for the agent to interact with.
#[async_trait]
pub trait ChannelSurface: Send + Sync {
    /// The platform name (e.g. "slack", "discord", "telegram").
    fn platform(&self) -> &str;

    /// What this surface supports.
    fn capabilities(&self) -> ChannelCapabilities;

    /// Add an emoji reaction to a message.
    async fn add_reaction(&self, reaction: &Reaction) -> Result<()>;

    /// Remove an emoji reaction from a message.
    async fn remove_reaction(&self, reaction: &Reaction) -> Result<()>;

    /// Send a message to a channel.
    async fn send_message(&self, _message: &OutboundMessage) -> Result<SentMessage> {
        Err(eyre::eyre!(
            "send_message not supported on {}",
            self.platform()
        ))
    }

    /// Open or re-open a DM conversation with a user, returning the channel ID.
    ///
    /// Used by the scheduler to recover DM channel IDs after token rotation.
    async fn open_conversation(&self, user_id: &str) -> Result<String> {
        let _ = user_id;
        Err(eyre::eyre!(
            "open_conversation not supported on {}",
            self.platform()
        ))
    }

    /// Resolve an email address to a platform user id (for opening a DM by
    /// email). Default: unsupported. Slack implements via `users.lookupByEmail`.
    async fn lookup_user_id_by_email(&self, email: &str) -> Result<String> {
        let _ = email;
        Err(eyre::eyre!(
            "lookup_user_id_by_email not supported on {}",
            self.platform()
        ))
    }

    /// Read recent messages from a channel.
    ///
    /// `oldest`, when set, returns only messages strictly newer than that
    /// platform timestamp (exclusive), paging until the range is exhausted —
    /// the catch-up read path relies on this to fetch everything since a cursor.
    async fn read_messages(
        &self,
        channel: &str,
        limit: usize,
        oldest: Option<&str>,
    ) -> Result<Vec<ChannelMessage>> {
        let _ = (channel, limit, oldest);
        Err(eyre::eyre!(
            "read_messages not supported on {}",
            self.platform()
        ))
    }

    /// Read a bounded window of channel history, paging as needed.
    ///
    /// `oldest` / `latest` are exclusive platform timestamps bounding the
    /// window; either may be `None`. Unlike [`Self::read_messages`], this
    /// always follows pagination (up to an implementation-defined page
    /// budget) and reports via [`HistoryPage::truncated`] when it stopped
    /// early, so callers can walk arbitrarily far back by resuming with
    /// `latest` set to the oldest timestamp they received.
    async fn read_history(
        &self,
        channel: &str,
        limit: usize,
        oldest: Option<&str>,
        latest: Option<&str>,
    ) -> Result<HistoryPage> {
        let _ = (channel, limit, oldest, latest);
        Err(eyre::eyre!(
            "read_history not supported on {}",
            self.platform()
        ))
    }

    /// Edit a previously sent message.
    async fn edit_message(&self, channel: &str, message_id: &str, text: &str) -> Result<()> {
        let _ = (channel, message_id, text);
        Err(eyre::eyre!(
            "edit_message not supported on {}",
            self.platform()
        ))
    }

    /// Edit a previously sent message with a full rich payload (text + optional blocks).
    /// Default implementation falls back to the text-only `edit_message`; Slack overrides
    /// this so Block Kit payloads survive the update.
    async fn update_message(
        &self,
        channel: &str,
        message_id: &str,
        outbound: &OutboundMessage,
    ) -> Result<()> {
        self.edit_message(channel, message_id, &outbound.text).await
    }

    /// Delete a previously sent message.
    async fn delete_message(&self, channel: &str, message_id: &str) -> Result<()> {
        let _ = (channel, message_id);
        Err(eyre::eyre!(
            "delete_message not supported on {}",
            self.platform()
        ))
    }

    /// Read replies in a thread.
    async fn read_thread(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: usize,
    ) -> Result<Vec<ChannelMessage>> {
        let _ = (channel, thread_ts, limit);
        Err(eyre::eyre!(
            "read_thread not supported on {}",
            self.platform()
        ))
    }

    /// Download the raw bytes of a file attached to a message.
    async fn fetch_file(&self, file: &ChannelFile) -> Result<Vec<u8>> {
        let _ = file;
        Err(eyre::eyre!(
            "fetch_file not supported on {}",
            self.platform()
        ))
    }

    /// Pin a message in a channel.
    async fn pin_message(&self, channel: &str, message_id: &str) -> Result<()> {
        let _ = (channel, message_id);
        Err(eyre::eyre!(
            "pin_message not supported on {}",
            self.platform()
        ))
    }

    /// Unpin a message in a channel.
    async fn unpin_message(&self, channel: &str, message_id: &str) -> Result<()> {
        let _ = (channel, message_id);
        Err(eyre::eyre!(
            "unpin_message not supported on {}",
            self.platform()
        ))
    }

    /// List pinned items in a channel.
    async fn list_pins(&self, channel: &str) -> Result<Vec<PinnedItem>> {
        let _ = channel;
        Err(eyre::eyre!(
            "list_pins not supported on {}",
            self.platform()
        ))
    }

    /// Look up member info by user ID.
    async fn member_info(&self, user_id: &str) -> Result<MemberInfo> {
        let _ = user_id;
        Err(eyre::eyre!(
            "member_info not supported on {}",
            self.platform()
        ))
    }

    /// Whether `platform_user_id` (a platform-native id, e.g. a Slack `U…`) is a
    /// member of `channel`. Used to verify the REQUESTING USER's access before a
    /// cross-channel read — not just the bot's. The default errors so surfaces
    /// that can't answer cause the caller to fail closed (deny the read).
    async fn is_channel_member(&self, channel: &str, platform_user_id: &str) -> Result<bool> {
        let _ = (channel, platform_user_id);
        Err(eyre::eyre!(
            "is_channel_member not supported on {}",
            self.platform()
        ))
    }

    /// List custom emoji in the workspace.
    async fn emoji_list(&self) -> Result<Vec<CustomEmoji>> {
        Err(eyre::eyre!(
            "emoji_list not supported on {}",
            self.platform()
        ))
    }

    /// Upload a file to a channel.
    async fn upload_file(
        &self,
        channel: &str,
        filename: &str,
        content: &[u8],
        message: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<String> {
        let _ = (channel, filename, content, message, thread_id);
        Err(eyre::eyre!(
            "upload_file not supported on {}",
            self.platform()
        ))
    }

    /// Look up channel info by channel ID.
    async fn channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
        let _ = channel_id;
        Err(eyre::eyre!(
            "channel_info not supported on {}",
            self.platform()
        ))
    }
}

// ── Conversation visibility ───────────────────────────────────────────────

/// Whether a conversation is private (1:1 DM) or shared with multiple humans
/// (channels, group DMs, private channels). Drives the agent's privacy
/// posture, welcome behaviour, and engagement triggers.
///
/// Derived from the Slack channel-ID prefix: only `D` (direct message) is
/// private; everything else (`C` public channel, `G` private channel / group
/// DM) is shared. Slash-command and event payloads carry the channel ID
/// directly, so this is a zero-cost classification with no extra API call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConversationVisibility {
    Private,
    Shared,
}

impl ConversationVisibility {
    pub fn from_slack_channel_id(channel_id: &str) -> Self {
        if channel_id.starts_with('D') {
            Self::Private
        } else {
            Self::Shared
        }
    }

    pub fn is_shared(self) -> bool {
        matches!(self, Self::Shared)
    }

    pub fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }
}

// ── Surface registry ──────────────────────────────────────────────────────

/// Composite key identifying a surface within the registry.
///
/// `workspace_id == None` denotes single-tenant platforms (e.g. a local
/// CLI surface) whose identity is fully captured by the platform name.
/// `Some(ws)` denotes multi-tenant platforms where each tenant has its
/// own bot identity (e.g. one entry per Slack workspace).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SurfaceKey {
    pub platform: String,
    pub workspace_id: Option<String>,
}

impl SurfaceKey {
    pub fn new(platform: impl Into<String>, workspace_id: Option<String>) -> Self {
        Self {
            platform: platform.into(),
            workspace_id,
        }
    }

    pub fn platform(platform: impl Into<String>) -> Self {
        Self::new(platform, None)
    }

    pub fn workspace(platform: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self::new(platform, Some(workspace_id.into()))
    }
}

/// Routing hint passed into [`SurfaceRegistry::resolve`]. The fields are
/// short-circuited in order: explicit override → context fast path →
/// cache hit → probe.
#[derive(Clone, Debug, Default)]
pub struct ChannelHint {
    pub explicit_workspace: Option<String>,
    pub context_workspace: Option<String>,
    pub context_channel: Option<String>,
}

impl ChannelHint {
    pub fn from_parts(
        explicit_workspace: Option<String>,
        context_workspace: Option<String>,
        context_channel: Option<String>,
    ) -> Self {
        Self {
            explicit_workspace,
            context_workspace,
            context_channel,
        }
    }

    pub fn explicit(workspace_id: impl Into<String>) -> Self {
        Self {
            explicit_workspace: Some(workspace_id.into()),
            ..Self::default()
        }
    }
}

/// Reason a [`SurfaceRegistry::resolve`] call settled on the surface it
/// returned. Logged at debug level on `slack.routing` so operators can
/// spot when probe traffic dominates (the cache TTL may be too short).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolutionPath {
    Explicit,
    ContextFast,
    Cache,
    Probe,
    Single,
}

impl ResolutionPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::ContextFast => "context_fast",
            Self::Cache => "cache",
            Self::Probe => "probe",
            Self::Single => "single",
        }
    }
}

/// Registry of channel surfaces keyed by `(platform, workspace_id)`.
///
/// Each agent holds its own registry containing only the surfaces it has
/// bot identity in. Routing decisions therefore never cross agents. The
/// per-workspace `primaries` map records the surface to prefer for
/// outbound operations originating from a given workspace context.
#[derive(Clone, Default)]
pub struct SurfaceRegistry {
    surfaces: HashMap<SurfaceKey, Arc<dyn ChannelSurface>>,
    primaries: HashMap<SurfaceKey, Arc<dyn ChannelSurface>>,
    /// Surfaces that post into the hub's own sessions (Desk), by platform.
    /// They resolve by platform like any other, but are kept out of
    /// [`Self::all`] and [`Self::all_for`]: those are the lists callers loop
    /// over with a platform's own channel ids.
    session_surfaces: HashMap<String, Arc<dyn ChannelSurface>>,
    routes: ChannelRouteCache,
}

impl SurfaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, key: SurfaceKey, surface: Arc<dyn ChannelSurface>) {
        info!(
            platform = %key.platform,
            workspace = key.workspace_id.as_deref().unwrap_or("-"),
            "Registered channel surface",
        );
        self.surfaces.insert(key, surface);
    }

    /// Register the surface that posts into the hub's own sessions for
    /// `platform`.
    pub fn register_session_surface(
        &mut self,
        platform: impl Into<String>,
        surface: Arc<dyn ChannelSurface>,
    ) {
        self.session_surfaces.insert(platform.into(), surface);
    }

    pub fn deregister(&mut self, key: &SurfaceKey) {
        self.surfaces.remove(key);
        self.primaries.remove(key);
    }

    pub fn set_primary_for(&mut self, key: SurfaceKey, surface: Arc<dyn ChannelSurface>) {
        self.primaries.insert(key, surface);
    }

    pub fn get(&self, key: &SurfaceKey) -> Option<&Arc<dyn ChannelSurface>> {
        self.primaries
            .get(key)
            .or_else(|| self.surfaces.get(key))
            .or_else(|| match key.workspace_id {
                None => self.session_surfaces.get(&key.platform),
                Some(_) => None,
            })
    }

    /// Surfaces matching `platform`. Includes primaries for that platform
    /// that aren't already represented in `surfaces` (which is the common
    /// case — primaries are normally a subset of surfaces).
    pub fn all_for(&self, platform: &str) -> Vec<Arc<dyn ChannelSurface>> {
        let mut out: Vec<_> = self
            .surfaces
            .iter()
            .filter(|(k, _)| k.platform == platform)
            .map(|(_, s)| Arc::clone(s))
            .collect();
        for (k, p) in &self.primaries {
            if k.platform == platform && !out.iter().any(|s| Arc::ptr_eq(s, p)) {
                out.push(Arc::clone(p));
            }
        }
        out
    }

    pub fn all(&self) -> Vec<Arc<dyn ChannelSurface>> {
        let mut out: Vec<_> = self.surfaces.values().cloned().collect();
        for p in self.primaries.values() {
            if !out.iter().any(|s| Arc::ptr_eq(s, p)) {
                out.push(Arc::clone(p));
            }
        }
        out
    }

    pub fn keys(&self) -> Vec<SurfaceKey> {
        self.surfaces.keys().cloned().collect()
    }

    pub fn platforms(&self) -> Vec<String> {
        let mut p: Vec<_> = self
            .surfaces
            .keys()
            .map(|k| k.platform.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        p.sort();
        p
    }

    pub fn workspaces_for(&self, platform: &str) -> Vec<String> {
        let mut ws: Vec<_> = self
            .surfaces
            .keys()
            .filter(|k| k.platform == platform)
            .filter_map(|k| k.workspace_id.clone())
            .collect();
        ws.sort();
        ws.dedup();
        ws
    }

    pub fn routes(&self) -> &ChannelRouteCache {
        &self.routes
    }

    /// Resolve the surface that owns `channel` on `platform`. See
    /// [`ResolutionPath`] for the strategy ordering.
    pub async fn resolve(
        &self,
        platform: &str,
        channel: &str,
        hint: ChannelHint,
    ) -> Result<Arc<dyn ChannelSurface>> {
        let (surface, path) = self.resolve_with_path(platform, channel, hint).await?;
        record_resolve(platform, channel, path);
        Ok(surface)
    }

    async fn resolve_with_path(
        &self,
        platform: &str,
        channel: &str,
        hint: ChannelHint,
    ) -> Result<(Arc<dyn ChannelSurface>, ResolutionPath)> {
        if let Some(surface) = self.session_surfaces.get(platform) {
            return Ok((Arc::clone(surface), ResolutionPath::Single));
        }
        if let Some(ws) = hint.explicit_workspace.as_deref() {
            let key = SurfaceKey::workspace(platform, ws);
            return self
                .get(&key)
                .cloned()
                .map(|s| (s, ResolutionPath::Explicit))
                .ok_or_else(|| {
                    no_surface_for_workspace(platform, ws, self.workspaces_for(platform))
                });
        }

        let context_workspace = hint.context_workspace.as_deref();
        let context_channel = hint.context_channel.as_deref();

        if let (Some(ws), Some(ctx_ch)) = (context_workspace, context_channel) {
            if ctx_ch == channel {
                let key = SurfaceKey::workspace(platform, ws);
                if let Some(s) = self.get(&key).cloned() {
                    return Ok((s, ResolutionPath::ContextFast));
                }
            }
        }

        if let Some(ws) = self.routes.lookup(platform, channel).await {
            let key = SurfaceKey::workspace(platform, &ws);
            if let Some(s) = self.get(&key).cloned() {
                return Ok((s, ResolutionPath::Cache));
            }
            self.routes.invalidate(platform, channel).await;
        }

        let candidates = self.candidates(platform);
        if candidates.is_empty() {
            return self.resolve_platform_only(platform);
        }
        if candidates.len() == 1 {
            let (ws, surface) = candidates.into_iter().next().expect("len == 1");
            self.routes.insert(platform, channel, &ws).await;
            return Ok((surface, ResolutionPath::Single));
        }

        self.probe(platform, channel, candidates, context_workspace)
            .await
            .map(|s| (s, ResolutionPath::Probe))
    }

    fn candidates(&self, platform: &str) -> Vec<(String, Arc<dyn ChannelSurface>)> {
        self.surfaces
            .iter()
            .filter(|(k, _)| k.platform == platform)
            .filter_map(|(k, s)| k.workspace_id.clone().map(|ws| (ws, Arc::clone(s))))
            .collect()
    }

    fn resolve_platform_only(
        &self,
        platform: &str,
    ) -> Result<(Arc<dyn ChannelSurface>, ResolutionPath)> {
        let key = SurfaceKey::platform(platform);
        self.get(&key)
            .cloned()
            .map(|s| (s, ResolutionPath::Single))
            .ok_or_else(|| no_platform_surface(platform))
    }

    async fn probe(
        &self,
        platform: &str,
        channel: &str,
        candidates: Vec<(String, Arc<dyn ChannelSurface>)>,
        context_workspace: Option<&str>,
    ) -> Result<Arc<dyn ChannelSurface>> {
        let probes = candidates.iter().map(|(ws, s)| {
            let ws = ws.clone();
            let s = Arc::clone(s);
            async move { s.channel_info(channel).await.ok().map(|_| (ws, s)) }
        });
        let winners: Vec<_> = join_all(probes).await.into_iter().flatten().collect();

        let (workspace, surface) = pick_by_workspace(winners, |(w, _)| w, context_workspace)
            .ok_or_else(|| {
                channel_unreachable(channel, candidates.iter().map(|(w, _)| w.as_str()))
            })?;
        self.routes.insert(platform, channel, &workspace).await;
        Ok(surface)
    }

    /// Resolve a surface for delivering to `user_id` (DMs, reminders).
    /// Probes `open_conversation` across registered workspaces and returns
    /// the first surface whose Slack user directory contains the user,
    /// alongside the resolved DM channel ID. The DM channel is also
    /// cached in the route table so future calls referencing it skip
    /// the probe.
    pub async fn resolve_user_dm(
        &self,
        platform: &str,
        user_id: &str,
        hint: ChannelHint,
    ) -> Result<(Arc<dyn ChannelSurface>, String)> {
        if let Some(ws) = hint.explicit_workspace.as_deref() {
            let key = SurfaceKey::workspace(platform, ws);
            let surface = self.get(&key).cloned().ok_or_else(|| {
                no_surface_for_workspace(platform, ws, self.workspaces_for(platform))
            })?;
            let channel = surface.open_conversation(user_id).await?;
            self.routes.insert(platform, &channel, ws).await;
            return Ok((surface, channel));
        }

        let candidates = self.candidates(platform);
        if candidates.is_empty() {
            let surface = self.resolve_platform_only(platform)?.0;
            let channel = surface.open_conversation(user_id).await?;
            return Ok((surface, channel));
        }
        if candidates.len() == 1 {
            let (ws, surface) = candidates.into_iter().next().expect("len == 1");
            let channel = surface.open_conversation(user_id).await?;
            self.routes.insert(platform, &channel, &ws).await;
            return Ok((surface, channel));
        }

        let context_workspace = hint.context_workspace.as_deref();
        let probes = candidates.iter().map(|(ws, s)| {
            let ws = ws.clone();
            let s = Arc::clone(s);
            async move {
                s.open_conversation(user_id)
                    .await
                    .ok()
                    .map(|ch| (ws, s, ch))
            }
        });
        let winners: Vec<_> = join_all(probes).await.into_iter().flatten().collect();
        let (workspace, surface, channel) =
            pick_by_workspace(winners, |(w, _, _)| w, context_workspace).ok_or_else(|| {
                user_unreachable(user_id, candidates.iter().map(|(w, _)| w.as_str()))
            })?;
        self.routes.insert(platform, &channel, &workspace).await;
        Ok((surface, channel))
    }
}

/// Pick the winner from probe results: prefer the entry whose workspace
/// matches the inbound context, else sort by workspace and take the first
/// (deterministic). Returns `None` if `items` is empty.
fn pick_by_workspace<T, F>(
    mut items: Vec<T>,
    workspace_of: F,
    context_workspace: Option<&str>,
) -> Option<T>
where
    F: Fn(&T) -> &str,
{
    if items.is_empty() {
        return None;
    }
    if let Some(ctx) = context_workspace {
        if let Some(idx) = items.iter().position(|t| workspace_of(t) == ctx) {
            return Some(items.swap_remove(idx));
        }
    }
    items.sort_by(|a, b| workspace_of(a).cmp(workspace_of(b)));
    items.into_iter().next()
}

fn no_platform_surface(platform: &str) -> eyre::Report {
    eyre!("no {platform} surface registered for this agent")
}

fn no_surface_for_workspace(
    platform: &str,
    workspace: &str,
    available: Vec<String>,
) -> eyre::Report {
    eyre!("no {platform} surface for workspace '{workspace}' (this agent is in: {available:?})")
}

fn channel_unreachable<'a>(
    channel: &str,
    workspaces: impl Iterator<Item = &'a str>,
) -> eyre::Report {
    let ws: Vec<_> = workspaces.collect();
    eyre!(
        "channel '{channel}' not visible from any of this agent's workspaces ({ws:?}); \
         invite the bot, check the channel belongs to a paired workspace, or pass \
         workspace_id explicitly"
    )
}

fn user_unreachable<'a>(user_id: &str, workspaces: impl Iterator<Item = &'a str>) -> eyre::Report {
    let ws: Vec<_> = workspaces.collect();
    eyre!(
        "user '{user_id}' not reachable via any of this agent's workspaces ({ws:?}); \
         verify the user exists in a paired workspace"
    )
}

fn record_resolve(platform: &str, channel: &str, path: ResolutionPath) {
    debug!(
        target: "slack.routing",
        platform = %platform,
        channel = %channel,
        path = path.as_str(),
        "resolved channel surface",
    );
}

/// Well-known emoji used for processing indicators.
pub mod indicator {
    /// Reaction added when the agent starts processing a message.
    pub const PROCESSING: &str = "eyes";
    /// Reaction shown while the agent is calling the LLM (thinking).
    pub const THINKING: &str = "brain";
    /// Reaction shown while the agent is executing a tool.
    pub const TOOL_RUNNING: &str = "gear";
    /// Reaction added when the agent finishes processing.
    pub const DONE: &str = "white_check_mark";
    /// Reaction added when an error occurs.
    pub const ERROR: &str = "x";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slack_channel() {
        let (platform, id) = parse_channel("slack:C0123ABC");
        assert_eq!(platform, "slack");
        assert_eq!(id, "C0123ABC");
    }

    #[test]
    fn channel_message_without_files_deserializes_to_empty() {
        // Sessions persisted before `files` existed must still load.
        let json = r#"{"channel":"C1","message_id":"1","user":"U1","text":"hi","timestamp":"1"}"#;
        let msg: ChannelMessage = serde_json::from_str(json).expect("deserialize");
        assert!(msg.files.is_empty());
        // And an empty `files` is omitted on the way back out.
        let out = serde_json::to_string(&msg).unwrap();
        assert!(!out.contains("files"));
    }

    #[test]
    fn channel_file_is_image_checks_mime() {
        let img = ChannelFile {
            id: "F1".into(),
            name: "a.png".into(),
            mime_type: "image/png".into(),
            url: None,
            size: None,
        };
        assert!(img.is_image());
        assert!(!ChannelFile {
            mime_type: "application/pdf".into(),
            ..img
        }
        .is_image());
    }

    #[test]
    fn parse_ws_channel() {
        let (platform, id) = parse_channel("ws:default");
        assert_eq!(platform, "ws");
        assert_eq!(id, "default");
    }

    #[test]
    fn is_thread_reply_excludes_only_replies() {
        let base = InboundEvent {
            surface: "slack".into(),
            workspace: "T".into(),
            channel: "C1".into(),
            user: Some("U1".into()),
            bot_id: None,
            text: "hi".into(),
            ts: "100.0".into(),
            thread_ts: None,
            has_files: false,
        };
        // A top-level message (no thread) is not a reply.
        assert!(!base.is_thread_reply());
        // A reply carries the parent ts (≠ its own) → reply.
        assert!(InboundEvent {
            thread_ts: Some("99.0".into()),
            ..base.clone()
        }
        .is_thread_reply());
        // A thread root reads back as thread_ts == ts → not a reply.
        assert!(!InboundEvent {
            thread_ts: Some("100.0".into()),
            ..base.clone()
        }
        .is_thread_reply());
    }

    #[test]
    fn parse_unknown_channel() {
        let (platform, id) = parse_channel("raw-channel");
        assert_eq!(platform, "unknown");
        assert_eq!(id, "raw-channel");
    }

    #[test]
    fn is_platform_check() {
        assert!(is_platform("slack:C123", "slack"));
        assert!(!is_platform("slack:C123", "ws"));
        assert!(!is_platform("bare", "slack"));
    }

    #[test]
    fn a_desk_room_is_a_conversation_of_its_own() {
        for room in [
            "desk:release",
            "desk:a",
            "desk:q3-plan-2",
            &format!("desk:{}", "x".repeat(64)),
        ] {
            assert!(is_desk_room(room), "{room}");
            assert!(is_conversational(room), "{room}");
        }
        for not_a_room in [
            "desk",
            "desk:",
            "desk:Release",
            "desk:two words",
            "desk:a/b",
            "desk:a:b",
            "desktop:release",
            &format!("desk:{}", "x".repeat(65)),
        ] {
            assert!(!is_desk_room(not_a_room), "{not_a_room}");
        }
        assert!(is_conversational(DESK_CHANNEL));
        assert!(!is_conversational("desk:Release"));
    }

    #[test]
    fn channel_id_extraction() {
        assert_eq!(channel_id("slack:C123ABC"), "C123ABC");
        assert_eq!(channel_id("bare-id"), "bare-id");
    }

    #[test]
    fn reaction_serializes() {
        let r = Reaction {
            channel: "C123".into(),
            message_id: "1234567890.123456".into(),
            emoji: "thumbsup".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("thumbsup"));
    }

    #[test]
    fn capabilities_default_all_false() {
        let c = ChannelCapabilities::default();
        assert!(!c.reactions);
        assert!(!c.reaction_removal);
        assert!(!c.threads);
        assert!(!c.rich_text);
        assert!(!c.send_message);
        assert!(!c.read_messages);
    }

    #[test]
    fn outbound_message_serializes() {
        let m = OutboundMessage {
            channel: "C123".into(),
            text: "hello".into(),
            thread_id: Some("123.456".into()),
            username: None,
            icon_url: None,
            blocks: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("hello"));
        assert!(json.contains("123.456"));
    }

    #[test]
    fn sent_message_serializes() {
        let m = SentMessage {
            channel: "C123".into(),
            message_id: "123.456".into(),
            thread_id: None,
            platform: "slack".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("slack"));
    }

    // ─── SurfaceRegistry tests ──────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockSurface {
        platform: String,
        workspace: String,
        known_channels: std::collections::HashSet<String>,
        known_users: std::collections::HashSet<String>,
        probes: Arc<AtomicUsize>,
    }

    impl MockSurface {
        fn arc(
            platform: &str,
            workspace: &str,
            channels: &[&str],
            users: &[&str],
        ) -> (Arc<Self>, Arc<AtomicUsize>) {
            let probes = Arc::new(AtomicUsize::new(0));
            let s = Self {
                platform: platform.into(),
                workspace: workspace.into(),
                known_channels: channels.iter().map(|s| s.to_string()).collect(),
                known_users: users.iter().map(|s| s.to_string()).collect(),
                probes: Arc::clone(&probes),
            };
            (Arc::new(s), probes)
        }
    }

    #[async_trait]
    impl ChannelSurface for MockSurface {
        fn platform(&self) -> &str {
            &self.platform
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                send_message: true,
                read_messages: true,
                channel_info: true,
                ..Default::default()
            }
        }
        async fn add_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.known_channels.contains(channel_id) {
                Ok(ChannelInfo {
                    channel_id: channel_id.into(),
                    display_name: format!("#{channel_id}@{}", self.workspace),
                })
            } else {
                Err(eyre!("channel_not_found"))
            }
        }
        async fn open_conversation(&self, user_id: &str) -> Result<String> {
            if self.known_users.contains(user_id) {
                Ok(format!("D-{}-{}", self.workspace, user_id))
            } else {
                Err(eyre!("user_not_found"))
            }
        }
    }

    fn slack_key(ws: &str) -> SurfaceKey {
        SurfaceKey::workspace("slack", ws)
    }

    #[tokio::test]
    async fn resolve_explicit_workspace_short_circuits() {
        let (a, a_probes) = MockSurface::arc("slack", "a", &["C1"], &[]);
        let (b, b_probes) = MockSurface::arc("slack", "b", &["C1"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.register(slack_key("b"), b);

        let s = reg
            .resolve("slack", "C1", ChannelHint::explicit("b"))
            .await
            .unwrap();
        assert_eq!(s.platform(), "slack");
        assert_eq!(a_probes.load(Ordering::SeqCst), 0);
        assert_eq!(b_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_context_fast_path_avoids_probe() {
        let (a, a_probes) = MockSurface::arc("slack", "a", &["C1"], &[]);
        let (b, b_probes) = MockSurface::arc("slack", "b", &["C9"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.register(slack_key("b"), b);

        let hint = ChannelHint::from_parts(None, Some("a".into()), Some("C1".into()));
        reg.resolve("slack", "C1", hint).await.unwrap();
        assert_eq!(a_probes.load(Ordering::SeqCst), 0);
        assert_eq!(b_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_probes_then_caches() {
        let (a, a_probes) = MockSurface::arc("slack", "a", &[], &[]);
        let (b, b_probes) = MockSurface::arc("slack", "b", &["C1"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.register(slack_key("b"), b);

        reg.resolve("slack", "C1", ChannelHint::default())
            .await
            .unwrap();
        assert_eq!(b_probes.load(Ordering::SeqCst), 1);
        assert_eq!(a_probes.load(Ordering::SeqCst), 1);

        reg.resolve("slack", "C1", ChannelHint::default())
            .await
            .unwrap();
        assert_eq!(
            b_probes.load(Ordering::SeqCst),
            1,
            "cache hit, no second probe"
        );
    }

    #[tokio::test]
    async fn resolve_no_winner_errors_with_workspace_list() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &[]);
        let (b, _) = MockSurface::arc("slack", "b", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.register(slack_key("b"), b);

        let err = reg
            .resolve("slack", "Cmissing", ChannelHint::default())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Cmissing"));
        assert!(err.contains('a'));
        assert!(err.contains('b'));
    }

    #[tokio::test]
    async fn resolve_single_candidate_skips_probe() {
        let (a, a_probes) = MockSurface::arc("slack", "a", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);

        reg.resolve("slack", "C1", ChannelHint::default())
            .await
            .unwrap();
        assert_eq!(a_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_prefers_context_workspace_on_tie() {
        let (a, _) = MockSurface::arc("slack", "a", &["Cshared"], &[]);
        let (b, _) = MockSurface::arc("slack", "b", &["Cshared"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), Arc::clone(&a) as Arc<dyn ChannelSurface>);
        reg.register(slack_key("b"), Arc::clone(&b) as Arc<dyn ChannelSurface>);

        let hint = ChannelHint::from_parts(None, Some("b".into()), Some("Other".into()));
        reg.resolve("slack", "Cshared", hint).await.unwrap();
        let cached = reg.routes().lookup("slack", "Cshared").await;
        assert_eq!(cached.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn resolve_explicit_workspace_missing_errors() {
        let (a, _) = MockSurface::arc("slack", "a", &["C1"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);

        let err = reg
            .resolve("slack", "C1", ChannelHint::explicit("ghost"))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("ghost"));
        assert!(err.contains('a'));
    }

    #[tokio::test]
    async fn a_session_surface_resolves_by_platform_but_is_never_broadcast_to() {
        let (slack, _) = MockSurface::arc("slack", "a", &[], &[]);
        let (desk, _) = MockSurface::arc("desk", "-", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), slack);
        reg.register_session_surface("desk", Arc::clone(&desk) as Arc<dyn ChannelSurface>);
        let resolved = reg
            .resolve("desk", "desk:room", ChannelHint::default())
            .await
            .unwrap();
        assert_eq!(resolved.platform(), "desk");
        assert!(reg.get(&SurfaceKey::platform("desk")).is_some());
        assert!(reg.all().iter().all(|s| s.platform() == "slack"));
        assert!(reg.all_for("desk").is_empty());
        assert_eq!(reg.platforms(), vec!["slack".to_string()]);
    }

    #[tokio::test]
    async fn resolve_user_dm_picks_winning_workspace() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &["U_only_in_b"]);
        let (b, _) = MockSurface::arc("slack", "b", &[], &["U_only_in_b"]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.register(slack_key("b"), b);

        let (_, dm) = reg
            .resolve_user_dm("slack", "U_only_in_b", ChannelHint::default())
            .await
            .unwrap();
        assert!(dm.starts_with("D-"));
        let cached = reg.routes().lookup("slack", &dm).await;
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn resolve_user_dm_unreachable_errors() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);

        let err = reg
            .resolve_user_dm("slack", "Uphantom", ChannelHint::default())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("user_not_found") || err.contains("Uphantom"));
    }

    #[tokio::test]
    async fn primaries_override_surfaces_for_same_key() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &[]);
        let (a_primary, _) = MockSurface::arc("slack", "a", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);
        reg.set_primary_for(
            slack_key("a"),
            Arc::clone(&a_primary) as Arc<dyn ChannelSurface>,
        );

        let found = reg.get(&slack_key("a")).unwrap();
        assert!(Arc::ptr_eq(found, &(a_primary as Arc<dyn ChannelSurface>)));
    }

    #[tokio::test]
    async fn deregister_removes_from_surfaces_and_primaries() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), Arc::clone(&a) as Arc<dyn ChannelSurface>);
        reg.set_primary_for(slack_key("a"), Arc::clone(&a) as Arc<dyn ChannelSurface>);
        reg.deregister(&slack_key("a"));
        assert!(reg.get(&slack_key("a")).is_none());
    }

    #[tokio::test]
    async fn workspaces_for_returns_sorted_unique_list() {
        let (a, _) = MockSurface::arc("slack", "a", &[], &[]);
        let (b, _) = MockSurface::arc("slack", "b", &[], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("b"), b);
        reg.register(slack_key("a"), a);
        assert_eq!(reg.workspaces_for("slack"), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn cache_pointing_to_removed_workspace_is_invalidated_and_reprobed() {
        let (a, _) = MockSurface::arc("slack", "a", &["C1"], &[]);
        let mut reg = SurfaceRegistry::new();
        reg.register(slack_key("a"), a);

        reg.routes().insert("slack", "C1", "ghost").await;
        let s = reg
            .resolve("slack", "C1", ChannelHint::default())
            .await
            .unwrap();
        assert_eq!(s.platform(), "slack");
        assert_eq!(
            reg.routes().lookup("slack", "C1").await.as_deref(),
            Some("a")
        );
    }

    #[test]
    fn visibility_classifies_dm_as_private() {
        assert_eq!(
            ConversationVisibility::from_slack_channel_id("D0123ABC"),
            ConversationVisibility::Private
        );
    }

    #[test]
    fn visibility_classifies_public_channel_as_shared() {
        assert_eq!(
            ConversationVisibility::from_slack_channel_id("C0123ABC"),
            ConversationVisibility::Shared
        );
    }

    #[test]
    fn visibility_classifies_private_channel_or_group_dm_as_shared() {
        assert_eq!(
            ConversationVisibility::from_slack_channel_id("G0123ABC"),
            ConversationVisibility::Shared
        );
    }

    #[test]
    fn visibility_classifies_unknown_prefix_as_shared() {
        for s in ["", "Z123", "unknown", "M-?"] {
            assert_eq!(
                ConversationVisibility::from_slack_channel_id(s),
                ConversationVisibility::Shared,
                "input was {s:?}",
            );
        }
    }

    #[test]
    fn visibility_predicates_are_complementary() {
        let v = ConversationVisibility::Private;
        assert!(v.is_private() && !v.is_shared());
        let v = ConversationVisibility::Shared;
        assert!(v.is_shared() && !v.is_private());
    }
}
